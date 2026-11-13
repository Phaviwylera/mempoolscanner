//! Entry point for the `mempool-scanner` binary.
//!
//! Read-only by default. Passing `--execute` opts into the feature-flagged
//! execution module (`src/exec/`), which can sign and broadcast `liquidate`
//! opportunities the operator detects. Without `--execute` the binary behaves
//! exactly as before: no keys, no signing, no broadcasting.

use alloy::primitives::U256;
use anyhow::Result;
use mempool_scanner::abi::REGISTRY;
use mempool_scanner::config::Settings;
use mempool_scanner::exec::{load_signer_from_env, ExecConfig};
use mempool_scanner::pipeline::run_scanner;
use std::env;
use std::sync::Arc;
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
        let has_key = env::var("EXEC_PRIVATE_KEY").is_ok() && !env::var("EXEC_PRIVATE_KEY").unwrap().trim().is_empty();
        let has_keystore = env::var("EXEC_KEYSTORE_PATH").is_ok() && !env::var("EXEC_KEYSTORE_PATH").unwrap().trim().is_empty();
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

    let exec_config = if execute {
        let signer = load_signer_from_env()?;
        let chain_id: u64 = env::var("EXEC_CHAIN_ID")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(1);
        let rpc_url = env::var("EXEC_RPC_URL")
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

    run_scanner(settings, exec_config.map(Arc::new)).await
}
