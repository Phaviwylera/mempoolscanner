//! Entry point for the `mempool-scanner` binary.
//!
//! Read-only by default. Passing `--execute` opts into the feature-flagged
//! execution module (`src/exec/`), which can sign and broadcast `liquidate`
//! opportunities the operator detects. Without `--execute` the binary behaves
//! exactly as before: no keys, no signing, no broadcasting.
//!
//! An HTTP API server is also started on port 3000 (or `API_ADDR` env var)
//! to serve the execution UI (`web/`) and JSON endpoints for hits,
//! liquidations, and execution control.

use alloy::primitives::{Address, U256};
use anyhow::Result;
use mempool_scanner::abi::REGISTRY;
use mempool_scanner::api::{start_api_server, AppState};
use mempool_scanner::config::Settings;
use mempool_scanner::exec::{load_signer_from_env, ExecConfig};
use mempool_scanner::keeper::Keeper;
use mempool_scanner::oracle::PythOracle;
use mempool_scanner::pipeline::run_scanner;
use std::collections::HashMap;
use std::env;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const GIT_SHA: &str = env!("GIT_COMMIT_SHA");

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// Value following a `--flag <value>` (or `--flag=value`) argument, if present.
fn flag_value(args: &[String], name: &str) -> Option<String> {
    let eq_prefix = format!("{name}=");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix(&eq_prefix) {
            return Some(v.to_string());
        }
    }
    None
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("mempool-scanner {VERSION} ({GIT_SHA})");
        return Ok(());
    }
    if args.iter().any(|a| a == "--dump-selectors") {
        println!("{}", serde_json::to_string_pretty(&*REGISTRY)?);
        return Ok(());
    }

    // EXECUTION_MODE is the primary control; the older --execute / --dry-run
    // flags still work and can only ever ADD restriction, never remove it.
    let exec_mode_raw = env::var("EXECUTION_MODE").unwrap_or_default();
    let exec_mode = match mempool_scanner::exec::ExecutionMode::parse(&exec_mode_raw) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };

    let execute = exec_mode.is_enabled() || flag(&args, "--execute");
    // --dry-run downgrades live to dry_run; it never upgrades.
    let dry_run = exec_mode.is_dry_run() || flag(&args, "--dry-run");

    // Safety rail: `--execute` requires operator key material to be present in
    // the environment before we touch the network. Exit with a clear error
    // otherwise.
    if execute {
        let has_key = env::var("EXEC_PRIVATE_KEY").is_ok()
            && !env::var("EXEC_PRIVATE_KEY").unwrap().trim().is_empty();
        let has_keystore = env::var("EXEC_KEYSTORE_PATH").is_ok()
            && !env::var("EXEC_KEYSTORE_PATH").unwrap().trim().is_empty();
        if !has_key && !has_keystore {
            eprintln!("error: --execute requires EXEC_PRIVATE_KEY or EXEC_KEYSTORE_PATH (+ EXEC_KEYSTORE_PASSWORD) to be set");
            std::process::exit(2);
        }
    }

    let _ = dotenvy::dotenv();
    let default_level = env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_string());
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_level.to_lowercase()));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();

    // --- Manual offline transaction builder -------------------------------
    //
    // `--build-tx <address>` is a one-shot utility: it resolves the borrower's
    // liquidation opportunity, runs the revm profitability pre-flight, signs a
    // single transaction targeting the AaveV3Liquidator contract, writes the
    // raw hex to signed_tx.txt, prints it, and EXITS. It never starts the
    // server, never broadcasts, and never loops.
    if let Some(target) = flag_value(&args, "--build-tx") {
        // Optional --mock-hf <value>: fork-only HF override for testing. Present
        // without a parseable value defaults to 0.95.
        let mock_hf = if flag(&args, "--mock-hf") {
            Some(
                flag_value(&args, "--mock-hf")
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(0.95),
            )
        } else {
            None
        };
        return run_build_tx(&target, mock_hf).await;
    }

    let settings = Settings::from_env()?;

    // --- Shared state between scanner pipeline and HTTP API server ---
    let recent_opportunities: Arc<RwLock<HashMap<String, mempool_scanner::exec::LiquidationOpportunity>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Point revm state forking and the Chainlink ETH/USD feed at the same HTTP
    // RPC the keeper uses. Without this, /api/simulate is disabled and fx stays
    // on the reference price.
    let mut api_state = AppState::new().with_state_rpc(&settings.state_rpc);
    api_state.recent_opportunities = recent_opportunities.clone();

    // --- Keeper: Aave V3 health factor polling ---
    let keeper = Arc::new(
        Keeper::new(&settings.state_rpc, recent_opportunities.clone()).await?,
    );

    // Add tracked borrowers from env (comma-separated addresses).
    if let Ok(borrowers) = env::var("TRACKED_BORROWERS") {
        for addr in borrowers.split(',') {
            let addr = addr.trim();
            if !addr.is_empty() {
                if let Ok(parsed) = addr.parse::<Address>() {
                    keeper.add_borrower(parsed).await;
                }
            }
        }
    }

    // --- Keeper trigger: interval poll, with Pyth as an optional addition ---
    //
    // The keeper used to be driven ONLY by Pyth price updates, which meant that
    // when the Pyth socket could not connect the keeper never polled at all and
    // recent_opportunities stayed permanently empty. A plain timer removes that
    // single point of failure, so health factors are refreshed regardless.
    //
    // KEEPER_POLL_SECONDS=0 disables the timer.
    let keeper_poll_seconds: u64 = env::var("KEEPER_POLL_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);

    if keeper_poll_seconds > 0 {
        let keeper_for_timer = keeper.clone();
        tokio::spawn(async move {
            let mut ticker =
                tokio::time::interval(std::time::Duration::from_secs(keeper_poll_seconds));
            // Skip missed ticks rather than firing a burst after a slow poll.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if let Err(e) = keeper_for_timer.poll_borrowers().await {
                    warn!("Keeper poll failed: {e}");
                }
            }
        });
        info!("Keeper polling every {keeper_poll_seconds}s");
    }

    // Pyth is opt-in: set PYTH_WS_URL to enable it. The public Hermes socket is
    // no longer openly reachable (the bare host 404s and /ws answers 401), so
    // enabling it without credentials only produces reconnect warnings.
    let oracle_handle = match env::var("PYTH_WS_URL") {
        Ok(ws_url) if !ws_url.trim().is_empty() => {
            // Real Crypto.ETH/USD feed id from the Hermes price_feeds index.
            let eth_usd_feed_id = env::var("PYTH_ETH_USD_FEED_ID").unwrap_or_else(|_| {
                "0xff61491a931112ddf1bd8147cd1b641375f79f5825126d665480874634fd0ace".to_string()
            });

            let mut oracle = PythOracle::new(ws_url.trim(), vec![eth_usd_feed_id]);
            let keeper_for_oracle = keeper.clone();
            oracle.on_price_update(move |_update| {
                let keeper = keeper_for_oracle.clone();
                tokio::spawn(async move {
                    if let Err(e) = keeper.poll_borrowers().await {
                        warn!("Keeper poll failed: {e}");
                    }
                });
            });
            info!("Pyth oracle enabled at {}", ws_url.trim());
            Some(tokio::spawn(async move {
                if let Err(e) = oracle.run().await {
                    error!(%e, "Oracle stopped");
                }
            }))
        }
        _ => {
            info!("Pyth oracle disabled (set PYTH_WS_URL to enable); keeper runs on its timer");
            None
        }
    };

    let api_addr: SocketAddr = env::var("API_ADDR")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| "0.0.0.0:3000".parse().expect("invalid API_ADDR"));

    let api_state_clone = api_state.clone();
    tokio::spawn(async move {
        if let Err(e) = start_api_server(api_addr, api_state_clone).await {
            error!(%e, "HTTP API server stopped");
        }
    });

    let exec_config = if execute {
        let signer = load_signer_from_env()?;
        let chain_id: u64 = env::var("EXEC_CHAIN_ID")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(1);
        let rpc_url = env::var("EXEC_RPC_URL")
            .ok()
            .map(|v| url::Url::parse(&v))
            .transpose()
            .map_err(|e| anyhow::anyhow!("invalid EXEC_RPC_URL: {e}"))?
            .ok_or_else(|| anyhow::anyhow!("EXEC_RPC_URL is required when --execute is passed"))?;
        let flashbots_relay = env::var("FLASHBOTS_RELAY")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|v| url::Url::parse(&v))
            .transpose()
            .map_err(|e| anyhow::anyhow!("invalid FLASHBOTS_RELAY: {e}"))?;
        let builder_relays: Vec<url::Url> = env::var("BUILDER_RELAYS")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|v| {
                v.split(',')
                    .map(|s| {
                        let s = s.trim();
                        url::Url::parse(s).map_err(|e| anyhow::anyhow!("invalid builder relay {s}: {e}"))
                    })
                    .collect::<Result<Vec<_>, anyhow::Error>>()
            })
            .transpose()?
            .unwrap_or_default();
        let min_profit_wei: U256 = env::var("EXEC_MIN_PROFIT_WEI")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(U256::ZERO);
        let confirmations: u64 = env::var("EXEC_CONFIRMATIONS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(1);

        // The deployed AaveV3Liquidator. Live mode refuses to start without
        // it: broadcasting with no target contract would sign a transaction
        // that cannot do anything useful.
        let liquidator: Option<Address> = match env::var("EXEC_LIQUIDATOR_ADDRESS") {
            Ok(v) if !v.trim().is_empty() => Some(
                v.trim()
                    .parse()
                    .map_err(|e| anyhow::anyhow!("invalid EXEC_LIQUIDATOR_ADDRESS: {e}"))?,
            ),
            _ => None,
        };

        if !dry_run && liquidator.is_none() {
            anyhow::bail!(
                "EXECUTION_MODE=live requires EXEC_LIQUIDATOR_ADDRESS (the deployed \
                 AaveV3Liquidator contract). Set it, or use EXECUTION_MODE=dry_run."
            );
        }

        info!(
            "execution mode: {} (broadcast {})",
            exec_mode.as_str(),
            if dry_run { "DISABLED" } else { "ENABLED" }
        );

        Some(ExecConfig {
            enabled: true,
            dry_run,
            liquidator,
            mode: exec_mode,
            chain_id,
            rpc_url,
            signer: Some(signer),
            flashbots_relay,
            builder_relays,
            min_profit_wei,
            confirmations,
        })
    } else {
        None
    };

    // Share exec_config with the API server so /api/execute can use it.
    api_state.exec_config = exec_config.clone().map(Arc::new);

    run_scanner(settings, exec_config.map(Arc::new), recent_opportunities).await?;

    if let Some(handle) = oracle_handle {
        handle.abort();
        let _ = handle.await;
    }

    Ok(())
}

/// Handle `--build-tx <address>`: resolve the opportunity, simulate, sign, and
/// write the signed raw transaction hex. Never broadcasts. `mock_hf`, when set,
/// overrides the borrower's health factor in the pre-flight only (testing).
async fn run_build_tx(target: &str, mock_hf: Option<f64>) -> Result<()> {
    use mempool_scanner::exec::{build_signed_liquidation_tx, BuildTxInputs, DEFAULT_SWAP_FEE};
    use mempool_scanner::exec::close_factor_debt;
    use mempool_scanner::keeper::Keeper;
    use std::collections::HashMap;

    let user: Address = target
        .trim()
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid --build-tx address {target:?}: {e}"))?;

    // Signer is env-only (EXEC_PRIVATE_KEY / keystore), exactly as the execution
    // path. This utility signs offline; it does not need EXECUTION_MODE enabled.
    let signer = load_signer_from_env()?;

    let liquidator: Address = env::var("EXEC_LIQUIDATOR_ADDRESS")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "--build-tx requires EXEC_LIQUIDATOR_ADDRESS (the deployed AaveV3Liquidator contract)"
            )
        })?
        .trim()
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid EXEC_LIQUIDATOR_ADDRESS: {e}"))?;

    let chain_id: u64 = env::var("EXEC_CHAIN_ID")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(1);
    let swap_fee: u32 = env::var("BUILD_TX_SWAP_FEE")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_SWAP_FEE);
    let min_profit: U256 = env::var("BUILD_TX_MIN_PROFIT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(U256::ZERO);
    let gas_limit: u64 = env::var("BUILD_TX_GAS_LIMIT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(380_000);

    let settings = Settings::from_env()?;
    let rpc_url = settings.state_rpc.clone();

    info!("Resolving opportunity for {user:#x} via Aave data provider…");
    let recent: Arc<RwLock<HashMap<String, mempool_scanner::exec::LiquidationOpportunity>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let keeper = Keeper::new(&rpc_url, recent).await?;

    let selection = keeper.resolve_reserves(user).await;
    let (Some(collateral), Some(debt_asset)) = (selection.collateral, selection.debt_asset) else {
        anyhow::bail!(
            "could not resolve a collateral+debt pair for {user:#x}. The borrower may have no \
             open Aave V3 position, or the data provider was unreachable."
        );
    };
    if selection.debt_amount.is_zero() {
        anyhow::bail!("resolved zero debt for {user:#x}; nothing to liquidate");
    }

    // Convert an optional --mock-hf float to a 1e18-scaled target.
    let mock_hf_wad: Option<U256> =
        mock_hf.map(|v| U256::from((v * 1e18).round().max(1.0) as u128));

    // Apply the Aave close factor. When mocking, size against the mock HF so the
    // close factor matches the position the pre-flight will simulate.
    let hf = keeper.user_health_factor(user).await;
    let effective_hf = mock_hf_wad.or(hf);
    let debt_to_cover = match effective_hf {
        Some(h) => close_factor_debt(selection.debt_amount, h),
        None => selection.debt_amount / U256::from(2u8),
    };

    println!("\n── Opportunity ──────────────────────────────────────────────");
    println!("borrower        {user:#x}");
    println!(
        "health factor   {}",
        hf.map(|h| h.to_string()).unwrap_or_else(|| "unknown".into())
    );
    println!(
        "collateral      {collateral:#x}  ({})",
        selection.collateral_symbol.as_deref().unwrap_or("?")
    );
    println!(
        "debt asset      {debt_asset:#x}  ({})",
        selection.debt_symbol.as_deref().unwrap_or("?")
    );
    println!("total debt      {} (asset units)", selection.debt_amount);
    println!("debtToCover     {debt_to_cover} (after close factor)");
    println!("liquidator      {liquidator:#x}");
    println!("swapFee         {swap_fee}");
    println!("minProfit       {min_profit}");
    if let Some(v) = mock_hf {
        println!("mock HF         {v} (pre-flight override, TESTING — signed tx unaffected)");
    }

    let inputs = BuildTxInputs {
        rpc_url,
        chain_id,
        liquidator,
        user,
        collateral,
        debt_asset,
        debt_to_cover,
        swap_fee,
        min_profit,
        gas_limit,
        mock_hf_wad,
    };

    info!("Running revm pre-flight and signing (offline, no broadcast)…");
    let report = build_signed_liquidation_tx(&signer, &inputs).await?;

    println!("\n── Simulation ───────────────────────────────────────────────");
    if let Some(note) = &report.mock_hf_note {
        println!("mock HF         {note}");
    }
    println!("revm success    {}", report.simulated_ok);
    println!("gas used        {}", report.sim_gas_used);
    if let Some(reason) = &report.sim_revert_reason {
        println!("revert reason   {reason}");
    }

    println!("\n── Signed transaction (NOT broadcast) ───────────────────────");
    println!("from            {}", report.from);
    println!("to              {}", report.to);
    println!("nonce           {}", report.nonce);
    println!("chainId         {}", report.chain_id);
    println!("gasLimit        {}", report.gas_limit);
    println!("maxFeePerGas    {}", report.max_fee_per_gas);
    println!("maxPriorityFee  {}", report.max_priority_fee_per_gas);
    println!("txHash          {}", report.tx_hash);
    println!("\nraw signed tx hex:\n{}", report.raw_tx_hex);

    if !report.simulated_ok {
        eprintln!(
            "\nWARNING: the revm pre-flight did NOT succeed, so signed_tx.txt was NOT written. \
             The transaction above would likely revert as built (wrong executor key, below the \
             close factor, or an unprofitable swap). Inspect the revert reason before broadcasting."
        );
        std::process::exit(1);
    }

    std::fs::write("signed_tx.txt", format!("{}\n", report.raw_tx_hex))?;
    println!("\nWrote signed_tx.txt. Inspect it, then broadcast it yourself (e.g. via Flashbots).");
    println!("This tool never broadcasts.");

    Ok(())
}
