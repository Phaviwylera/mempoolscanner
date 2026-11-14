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
use tracing::{error, warn};
use tracing_subscriber::EnvFilter;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const GIT_SHA: &str = env!("GIT_COMMIT_SHA");

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
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

    let execute = flag(&args, "--execute");
    let dry_run = flag(&args, "--dry-run");

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

    // --- Oracle: Pyth price feed triggers keeper polls ---
    let eth_usd_feed_id = env::var("PYTH_ETH_USD_FEED_ID").unwrap_or_else(|_| {
        "0xff61491a931112ff13428fe0603a2e3e5b4f6c7c0c6e7c6c6c6c6c6c6c6c6c6".to_string()
    });

    let mut oracle = PythOracle::new(
        &env::var("PYTH_WS_URL")
            .unwrap_or_else(|_| "wss://hermes.pyth.network".to_string()),
        vec![eth_usd_feed_id],
    );

    let keeper_for_oracle = keeper.clone();
    oracle.on_price_update(move |_update| {
        let keeper = keeper_for_oracle.clone();
        tokio::spawn(async move {
            if let Err(e) = keeper.poll_borrowers().await {
                warn!("Keeper poll failed: {e}");
            }
        });
    });

    let oracle_handle = tokio::spawn(async move {
        if let Err(e) = oracle.run().await {
            error!(%e, "Oracle stopped");
        }
    });

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

        Some(ExecConfig {
            enabled: true,
            dry_run,
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

    oracle_handle.abort();
    let _ = oracle_handle.await;

    Ok(())
}
