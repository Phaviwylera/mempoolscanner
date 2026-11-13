//! Entry point for the `mempool-scanner` binary.

use anyhow::Result;
use mempool_scanner::abi::REGISTRY;
use mempool_scanner::config::Settings;
use mempool_scanner::pipeline::run_scanner;
use std::env;
use tracing_subscriber::EnvFilter;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const GIT_SHA: &str = env!("GIT_COMMIT_SHA");

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
    run_scanner(settings).await
}
