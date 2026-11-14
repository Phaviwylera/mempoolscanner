//! HTTP API server that serves the execution UI and bridges it to the
//! Rust execution module (`src/exec/`).
//!
//! The scanner writes JSONL/JSON snapshots to `logs/`. This server reads
//! those files and exposes them as JSON endpoints for the frontend. It also
//! exposes a `/api/execute` endpoint that runs the simulation gate, signs
//! with `EXEC_PRIVATE_KEY`, and broadcasts via Flashbots/MEV-Share.
//!
//! All safety rails from `src/exec/` apply: simulation gate is
//! non-bypassable, `--dry-run` signs without broadcasting, no private key
//! is accepted from the API, and `eth_sendRawTransaction` only appears in
//! `src/exec/broadcast.rs`.

use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use chrono::Utc;
use crate::exec::{execute_liquidation, ExecConfig, ExecOutcome, LiquidationOpportunity};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_http::services::ServeDir;
use tracing::{info, warn};

/// Shared application state for the API server.
#[derive(Clone)]
pub struct AppState {
    pub hits_path: PathBuf,
    pub liquidations_path: PathBuf,
    pub audit_path: PathBuf,
    /// Operator execution config (only set when `--execute` is passed).
    pub exec_config: Option<Arc<ExecConfig>>,
    /// Recently detected liquidation opportunities, keyed by opportunity id.
    /// Populated by the scanner pipeline so `/api/execute` can look them up.
    pub recent_opportunities: Arc<RwLock<HashMap<String, LiquidationOpportunity>>>,
    /// Last execution result for the UI to poll.
    pub last_exec: Arc<RwLock<Option<ExecResult>>>,
}

/// Result of a `/api/execute` call, surfaced to the UI.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ExecResult {
    pub timestamp: String,
    pub opportunity_id: String,
    pub success: bool,
    pub tx_hash: Option<String>,
    pub gas_spent_wei: String,
    pub bounty_wei: String,
    pub net_profit_wei: String,
    pub outcome: String,
    pub error: Option<String>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            hits_path: PathBuf::from("logs/hits.jsonl"),
            liquidations_path: PathBuf::from("logs/liquidations.json"),
            audit_path: PathBuf::from("logs/exec_audit.jsonl"),
            exec_config: None,
            recent_opportunities: Arc::new(RwLock::new(HashMap::new())),
            last_exec: Arc::new(RwLock::new(None)),
        }
    }

    pub fn with_logs_dir(dir: &str) -> Self {
        let p = PathBuf::from(dir);
        Self {
            hits_path: p.join("hits.jsonl"),
            liquidations_path: p.join("liquidations.json"),
            audit_path: p.join("exec_audit.jsonl"),
            exec_config: None,
            recent_opportunities: Arc::new(RwLock::new(HashMap::new())),
            last_exec: Arc::new(RwLock::new(None)),
        }
    }

    /// Store a detected liquidation opportunity so the API can execute it later.
    pub async fn record_opportunity(&self, id: String, opp: LiquidationOpportunity) {
        let mut guard = self
            .recent_opportunities
            .write()
            .await;
        guard.insert(id, opp);
        // Keep only the most recent 200 opportunities
        if guard.len() > 200 {
            let oldest: Vec<String> = guard.keys().cloned().take(guard.len() - 200).collect();
            for k in oldest {
                guard.remove(&k);
            }
        }
    }
}

/// Build the Axum router with static file serving and JSON API routes.
pub fn router(state: AppState) -> Router {
    let state = Arc::new(state);

    Router::new()
        // --- Static file serving (web UI) — ServeDir at / serves files from web/.
        .route_service("/", ServeDir::new("web"))
        // --- JSON API ---
        .route("/api/hits", get(get_hits))
        .route("/api/quality", get(get_quality))
        .route("/api/liquidations", get(get_liquidations))
        .route("/api/execute", post(post_execute))
        .route("/api/keepers", get(get_keepers))
        .route("/api/audit-logs", get(get_audit_logs))
        .route("/api/audit-logs/download", get(download_audit_logs))
        .route("/api/last-exec", get(get_last_exec))
        .with_state(state)
}

/// GET /api/hits — read `logs/hits.jsonl` and return recent hits as JSON.
async fn get_hits(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Json<Value> {
    let limit: usize = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);

    let mut hits: Vec<Value> = Vec::new();
    if let Ok(contents) = fs::read_to_string(&state.hits_path) {
        for line in contents.lines().rev().take(limit) {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                hits.push(v);
            }
        }
    }
    hits.reverse(); // oldest first

    Json(json!({
        "hits": hits,
        "fx": { "eth_inr": 260000.0, "source": "reference" },
    }))
}

/// GET /api/quality — return scanner quality/telemetry summary.
async fn get_quality(
    State(_state): State<Arc<AppState>>,
    Query(_params): Query<HashMap<String, String>>,
) -> Json<Value> {
    Json(json!({
        "banner": {
            "state": "online",
            "text": "Mempool scanner API reachable",
            "lag": 0,
        },
        "rollup": {
            "available": true,
            "with_fork": 0,
            "total": 0,
            "counts": {},
            "percent": {},
        },
        "window": {
            "filter": "all",
            "filtered": 0,
            "returned": 0,
        },
        "pipeline": {
            "with_decision": 0,
            "latency_us": {},
            "frontrun_counts": {},
        },
        "fx": { "eth_inr": 260000.0, "source": "reference" },
    }))
}

/// GET /api/liquidations — read `logs/liquidations.json` and return positions.
async fn get_liquidations(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Json<Value> {
    let _filter = params.get("filter").map(|s| s.as_str()).unwrap_or("all");
    let _inspect = params.get("inspect").map(|s| s.as_str());

    let mut positions: Vec<Value> = Vec::new();
    let mut summary = json!({
        "liquidatable_count": 0,
        "at_risk_count": 0,
        "tracked_count": 0,
        "total_liquidatable_bounty_inr": "₹0.00",
        "total_liquidatable_net_inr": "₹0.00",
        "at_risk_debt_inr": "₹0.00",
        "total_tracked_debt_inr": "₹0.00",
        "total_tracked_collateral_inr": "₹0.00",
        "lowest_hf": null,
    });

    if let Ok(contents) = fs::read_to_string(&state.liquidations_path) {
        if let Ok(data) = serde_json::from_str::<Value>(&contents) {
            if let Some(poss) = data.get("positions").and_then(|p| p.as_array()) {
                for pos in poss {
                    positions.push(pos.clone());
                }
            }
            if let Some(s) = data.get("summary") {
                summary = s.clone();
            }
        }
    }

    // If inspect address was provided, return inspected result
    if let Some(addr) = _inspect {
        let inspected = json!({
            "user": addr,
            "ok": false,
            "error": "inspect not available without live liquidation watcher",
        });
        return Json(json!({ "inspected": inspected }));
    }

    Json(json!({
        "positions": positions,
        "summary": summary,
        "fx": { "eth_inr": 260000.0, "eth_usd": 2650.0, "usd_inr": 86.0, "source": "reference" },
        "base_fee_gwei": 1.0,
        "status": "live",
        "discovered_borrowers_total": positions.len(),
        "scanned_blocks_total": 0,
        "head_block": null,
        "updated_ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "recent_liquidations": [],
    }))
}

/// POST /api/execute — run the simulation gate, sign, and broadcast (or dry-run).
///
/// Request body: `{ "opportunity_id": "0x..." }`
///
/// The signer is loaded from `EXEC_PRIVATE_KEY` env var only — never from
/// the request body. This endpoint is only active when the server was
/// started with `--execute` (i.e. `EXEC_PRIVATE_KEY` and `EXEC_RPC_URL` are
/// set in the environment).
async fn post_execute(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<Value>,
) -> Json<Value> {
    let opportunity_id = payload
        .get("opportunity_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let timestamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    // Safety rail: execution requires `--execute` mode with EXEC_PRIVATE_KEY set.
    let Some(cfg) = &state.exec_config else {
        let result = ExecResult {
            timestamp,
            opportunity_id: opportunity_id.clone(),
            success: false,
            tx_hash: None,
            gas_spent_wei: "0".to_string(),
            bounty_wei: "0".to_string(),
            net_profit_wei: "0".to_string(),
            outcome: "skipped".to_string(),
            error: Some("execution disabled: --execute not passed".to_string()),
        };
        *state.last_exec.write().await = Some(result.clone());
        return Json(json!(result));
    };

    // Look up the opportunity from recently detected liquidations.
    let Some(opportunity) = state
        .recent_opportunities
        .read()
        .await
        .get(&opportunity_id)
        .cloned()
    else {
        let result = ExecResult {
            timestamp,
            opportunity_id: opportunity_id.clone(),
            success: false,
            tx_hash: None,
            gas_spent_wei: "0".to_string(),
            bounty_wei: "0".to_string(),
            net_profit_wei: "0".to_string(),
            outcome: "skipped".to_string(),
            error: Some(format!("opportunity not found: {opportunity_id}")),
        };
        *state.last_exec.write().await = Some(result.clone());
        return Json(json!(result));
    };

    // Run the simulation gate + sign + broadcast (non-bypassable simulation gate).
    match execute_liquidation(cfg, &opportunity).await {
        Ok(outcome) => {
            let (tx_hash, success, gas_spent, bounty, net_profit, outcome_str) =
                match &outcome {
                    ExecOutcome::Confirmed { tx_hash, block: _ } => {
                        (Some(format!("{tx_hash:#x}")), true, "0".to_string(), opportunity.gross_profit_wei.to_string(), opportunity.gross_profit_wei.to_string(), "confirmed".to_string())
                    }
                    ExecOutcome::Reverted { tx_hash, block: _ } => {
                        (Some(format!("{tx_hash:#x}")), false, "0".to_string(), "0".to_string(), "0".to_string(), "reverted".to_string())
                    }
                    ExecOutcome::Broadcast { tx_hash } => {
                        (Some(format!("{tx_hash:#x}")), true, "0".to_string(), opportunity.gross_profit_wei.to_string(), opportunity.gross_profit_wei.to_string(), "broadcast".to_string())
                    }
                    ExecOutcome::Simulated { profit } => {
                        (None, true, "0".to_string(), profit.to_string(), profit.to_string(), "simulated".to_string())
                    }
                    ExecOutcome::Skipped { reason } => {
                        (None, false, "0".to_string(), "0".to_string(), "0".to_string(), format!("skipped: {reason}"))
                    }
                };

            let result = ExecResult {
                timestamp,
                opportunity_id: opportunity_id.clone(),
                success,
                tx_hash,
                gas_spent_wei: gas_spent,
                bounty_wei: bounty,
                net_profit_wei: net_profit,
                outcome: outcome_str.clone(),
                error: if success { None } else { Some(outcome_str) },
            };
            *state.last_exec.write().await = Some(result.clone());
            Json(json!(result))
        }
        Err(e) => {
            warn!("execute_liquidation failed: {e}");
            let result = ExecResult {
                timestamp,
                opportunity_id: opportunity_id.clone(),
                success: false,
                tx_hash: None,
                gas_spent_wei: "0".to_string(),
                bounty_wei: "0".to_string(),
                net_profit_wei: "0".to_string(),
                outcome: "error".to_string(),
                error: Some(e.to_string()),
            };
            *state.last_exec.write().await = Some(result.clone());
            Json(json!(result))
        }
    }
}

/// GET /api/audit-logs — return the contents of `logs/exec_audit.jsonl`.
async fn get_audit_logs(
    State(state): State<Arc<AppState>>,
) -> Response {
    match fs::read_to_string(&state.audit_path) {
        Ok(contents) => {
            let entries: Vec<Value> = contents
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .collect();
            Json(json!({ "entries": entries, "count": entries.len() })).into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// GET /api/audit-logs/download — serve `exec_audit.jsonl` as a file download.
async fn download_audit_logs(
    State(state): State<Arc<AppState>>,
) -> Response {
    match fs::read(&state.audit_path) {
        Ok(contents) => {
            let mut headers = header::HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/jsonl"),
            );
            headers.insert(
                header::CONTENT_DISPOSITION,
                header::HeaderValue::from_str(
                    "attachment; filename=\"exec_audit.jsonl\"",
                )
                .unwrap(),
            );
            (headers, contents).into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// GET /api/last-exec — return the last execution result for UI polling.
async fn get_last_exec(
    State(state): State<Arc<AppState>>,
) -> Json<Value> {
    let guard = state.last_exec.read().await;
    match guard.as_ref() {
        Some(r) => Json(json!(r)),
        None => Json(json!({
            "success": false,
            "outcome": "none",
            "error": "no execution has been attempted",
        })),
    }
}

/// GET /api/keepers — return liquidatable borrower data from the keeper.
async fn get_keepers(
    State(state): State<Arc<AppState>>,
) -> Json<Value> {
    let opportunities = state
        .recent_opportunities
        .read()
        .await;

    let keepers: Vec<Value> = opportunities
        .values()
        .map(|opp| {
            json!({
                "id": opp.id,
                "borrower": format!("{:#x}", opp.borrower.unwrap_or_default()),
                "category": opp.category,
                "gross_profit_wei": opp.gross_profit_wei.to_string(),
                "repay_amount_wei": opp.repay_amount.map(|a| a.to_string()).unwrap_or_default(),
                "summary": opp.summary,
            })
        })
        .collect();

    Json(json!({
        "keepers": keepers,
        "count": keepers.len(),
    }))
}

/// Start the HTTP API server on the given address.
pub async fn start_api_server(
    addr: SocketAddr,
    state: AppState,
) -> anyhow::Result<()> {
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("HTTP API server listening on http://{}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}
