//! Runtime configuration loaded from `.env` and environment variables.

use anyhow::Result;
use std::env;
use std::path::PathBuf;
use tracing::warn;

#[derive(Clone, Debug)]
pub struct Settings {
    pub ws_rpc: String,
    pub ipc_rpc: Option<String>,
    pub state_rpc: String,
    pub anvil_rpc: Option<String>,
    pub log_path: PathBuf,
    pub max_lag: u64,
    pub depth: u64,
    pub poll_seconds: f64,
    pub worker_concurrency: usize,
    pub sim_timeout_ms: u64,
}

impl Settings {
    pub fn from_env() -> Result<Self> {
        let _ = dotenvy::dotenv();

        let mut ws_rpc = read_trimmed("WS_RPC");
        let ipc_rpc = non_empty(read_trimmed("IPC_RPC"));
        let http_rpc = non_empty(read_trimmed("HTTP_RPC"));
        let anvil_rpc = non_empty(read_trimmed("ANVIL_RPC"));
        let prefer_anvil = matches!(
            read_trimmed("PREFER_ANVIL").to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        );

        if ws_rpc.is_empty() && ipc_rpc.is_none() {
            warn!("WS_RPC and IPC_RPC unset; defaulting to wss://ethereum-rpc.publicnode.com");
            ws_rpc = "wss://ethereum-rpc.publicnode.com".to_string();
        }

        let derived_http = ws_to_http(&ws_rpc);

        // In-process Revm queries state directly from HTTP_RPC (lag = 0 against upstream)
        // when available, falling back to WS-derived HTTPS before local ANVIL_RPC.
        let state_rpc = if prefer_anvil {
            anvil_rpc
                .clone()
                .or_else(|| http_rpc.clone())
                .or(derived_http)
                .unwrap_or_else(|| "https://ethereum-rpc.publicnode.com".to_string())
        } else {
            http_rpc
                .clone()
                .or(derived_http)
                .or_else(|| anvil_rpc.clone())
                .unwrap_or_else(|| "https://ethereum-rpc.publicnode.com".to_string())
        };

        let log_raw = non_empty(read_trimmed("LOG_PATH"))
            .or_else(|| non_empty(read_trimmed("MEMPOLSCAN_LOG_PATH")))
            .unwrap_or_else(|| "logs/hits.jsonl".to_string());

        Ok(Self {
            ws_rpc,
            ipc_rpc,
            state_rpc,
            anvil_rpc,
            log_path: PathBuf::from(log_raw),
            max_lag: read_u64("FORK_MAX_LAG", 3, 0),
            depth: read_u64("FORK_CHECK_DEPTH", 1, 0),
            poll_seconds: read_f64("FORK_POLL_SECONDS", 3.0, 0.5),
            worker_concurrency: read_u64("WORKER_CONCURRENCY", 8, 1) as usize,
            sim_timeout_ms: read_u64("SIM_TIMEOUT_MS", 6_000, 500),
        })
    }
}

fn read_trimmed(name: &str) -> String {
    let raw = env::var(name).unwrap_or_default();
    let trimmed = raw.trim();
    let unquoted = if (trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2)
        || (trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2)
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    unquoted.trim().to_string()
}

fn ws_to_http(ws: &str) -> Option<String> {
    if let Some(rest) = ws.strip_prefix("wss://") {
        Some(format!("https://{rest}"))
    } else {
        ws.strip_prefix("ws://")
            .map(|rest| format!("http://{rest}"))
    }
}

fn non_empty(val: String) -> Option<String> {
    if val.is_empty() {
        None
    } else {
        Some(val)
    }
}

fn read_u64(name: &str, default: u64, minimum: u64) -> u64 {
    let raw = read_trimmed(name);
    if raw.is_empty() {
        return default;
    }
    match raw.parse::<u64>() {
        Ok(v) => v.max(minimum),
        Err(_) => {
            warn!("Ignoring invalid {name}={raw:?}; using {default}");
            default
        }
    }
}

fn read_f64(name: &str, default: f64, minimum: f64) -> f64 {
    let raw = read_trimmed(name);
    if raw.is_empty() {
        return default;
    }
    match raw.parse::<f64>() {
        Ok(v) if v.is_finite() => v.max(minimum),
        _ => {
            warn!("Ignoring invalid {name}={raw:?}; using {default:.1}");
            default
        }
    }
}
