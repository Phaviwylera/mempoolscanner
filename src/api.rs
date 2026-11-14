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
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use chrono::Utc;
use crate::chainlink::ChainlinkEthUsd;
use crate::exec::{
    execute_liquidation, simulate_raw_call, ExecConfig, ExecOutcome, LiquidationOpportunity,
};
use crate::state::{RpcStateBackend, StateBackend};
use alloy::primitives::{keccak256, Address, Bytes, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::sol;
use alloy::sol_types::SolCall;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};
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
    /// HTTP RPC used for revm state forking and the Chainlink read.
    /// `None` leaves /api/simulate disabled and fx on the reference value.
    pub state_rpc: Option<String>,
    /// Cached Chainlink ETH/USD reader. `None` when no RPC is configured.
    pub eth_usd: Option<Arc<ChainlinkEthUsd>>,
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
            state_rpc: None,
            eth_usd: None,
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
            state_rpc: None,
            eth_usd: None,
        }
    }

    /// Point revm state forking and the Chainlink feed at an HTTP RPC.
    /// Failing to build the reader is non-fatal: fx falls back to reference.
    pub fn with_state_rpc(mut self, rpc_url: &str) -> Self {
        self.state_rpc = Some(rpc_url.to_string());
        match ChainlinkEthUsd::new(rpc_url) {
            Ok(feed) => self.eth_usd = Some(Arc::new(feed)),
            Err(e) => warn!("chainlink reader unavailable: {e}"),
        }
        self
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
            let oldest: Vec<String> = guard.keys().take(guard.len() - 200).cloned().collect();
            for k in oldest {
                guard.remove(&k);
            }
        }
    }
}

/// Build the Axum router with static file serving and JSON API routes.
pub fn router(state: AppState) -> Router {
    let state = Arc::new(state);

    // Permissive CORS so the Vite dev server (http://localhost:5173) can reach
    // the API on :3000. `Any` origin forbids credentialed requests by design;
    // this server accepts no cookies and no private keys, so that is correct.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Vite SPA. `route_service("/", ...)` matched ONLY the exact path "/", so a
    // built bundle's /assets/index-*.js 404'd. A fallback_service matches any
    // path no route claimed, which serves the hashed assets; the inner fallback
    // to index.html keeps client-side deep links working if a router is added.
    let spa = ServeDir::new("web-dash/dist")
        .fallback(ServeFile::new("web-dash/dist/index.html"));

    Router::new()
        // --- Legacy vanilla UI, kept reachable at /legacy while the new
        //     dashboard beds in. Serves the untouched web/ directory.
        .nest_service("/legacy", ServeDir::new("web"))
        // --- JSON API ---
        .route("/api/hits", get(get_hits))
        .route("/api/quality", get(get_quality))
        .route("/api/opportunities", get(get_opportunities))
        .route("/api/liquidations", get(get_liquidations))
        .route("/api/execute", post(post_execute))
        // axum 0.8 path params are {braces}; ":address" would panic at startup.
        .route("/api/simulate/{address}", post(post_simulate))
        .route("/api/keepers", get(get_keepers))
        .route("/api/audit-logs", get(get_audit_logs))
        .route("/api/audit-logs/download", get(download_audit_logs))
        .route("/api/last-exec", get(get_last_exec))
        // Must come after the /api routes: a fallback only runs when no route
        // matched, so it can never shadow the JSON API.
        .fallback_service(spa)
        .layer(cors)
        .with_state(state)
}

/// GET /api/opportunities — live snapshot of the in-memory
/// `recent_opportunities` map that the Aave V3 keeper writes to.
///
/// This is deliberately distinct from `/api/liquidations`, which reads the
/// on-disk `logs/` snapshot: this endpoint reflects keeper state as of the
/// current poll, with no filesystem round-trip. Supports `?limit=N`
/// (default 100); richest bounty first, id as a stable tie-break.
async fn get_opportunities(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Json<Value> {
    let limit: usize = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);

    let guard = state.recent_opportunities.read().await;

    let mut sorted: Vec<&LiquidationOpportunity> = guard.values().collect();
    sorted.sort_by(|a, b| {
        b.gross_profit_wei
            .cmp(&a.gross_profit_wei)
            .then_with(|| a.id.cmp(&b.id))
    });

    let total = sorted.len();
    let liquidatable = sorted
        .iter()
        .filter(|o| o.borrower.is_some())
        .count();

    let opportunities: Vec<Value> = sorted
        .into_iter()
        .take(limit)
        .map(|o| serde_json::to_value(o).unwrap_or(Value::Null))
        .collect();

    Json(json!({
        "opportunities": opportunities,
        "count": opportunities.len(),
        "total": total,
        "liquidatable_count": liquidatable,
        "status": "live",
    }))
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

    // Live Chainlink ETH/USD when a feed is configured and reachable; the old
    // hardcoded reference value only as a last resort, and labelled as such so
    // the UI never presents a made-up price as an oracle reading.
    const USD_INR: f64 = 86.0;
    const REFERENCE_ETH_USD: f64 = 2650.0;

    let quote = match &state.eth_usd {
        Some(feed) => feed.quote().await,
        None => None,
    };
    let (eth_usd, fx_source, feed_updated_at, fx_stale) = match &quote {
        Some(q) => (
            q.price_usd,
            if q.stale { "chainlink (stale)" } else { "chainlink" },
            Some(q.feed_updated_at),
            q.stale,
        ),
        None => (REFERENCE_ETH_USD, "reference", None, false),
    };

    Json(json!({
        "positions": positions,
        "summary": summary,
        "fx": {
            "eth_inr": eth_usd * USD_INR,
            "eth_usd": eth_usd,
            "usd_inr": USD_INR,
            "source": fx_source,
            "stale": fx_stale,
            "feed_updated_at": feed_updated_at,
            "feed": crate::chainlink::CHAINLINK_ETH_USD.to_string(),
        },
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

// ---------------------------------------------------------------------------
// POST /api/simulate/{address} — revm liquidation simulation
// ---------------------------------------------------------------------------

sol! {
    interface IAaveV3Pool {
        function liquidationCall(
            address collateralAsset,
            address debtAsset,
            address user,
            uint256 debtToCover,
            bool receiveAToken
        ) external;
    }
}

/// `LiquidationCall(address,address,address,uint256,uint256,address,bool)` —
/// the Aave V3 Pool event reporting the exact amounts actually moved:
/// (collateralAsset, debtAsset, user) indexed, then debtToCover,
/// liquidatedCollateralAmount, liquidator, receiveAToken in the data words.
const LIQUIDATION_CALL_SIG: &[u8] =
    b"LiquidationCall(address,address,address,uint256,uint256,address,bool)";

/// Optional overrides for a simulation. Needed because a keeper-built
/// opportunity currently carries `collateral: None` / `debt_asset: None`
/// (src/keeper.rs), and `liquidationCall` cannot be encoded without both.
#[derive(Debug, Default, serde::Deserialize)]
pub struct SimulateRequest {
    pub collateral_asset: Option<String>,
    pub debt_asset: Option<String>,
    /// Decimal or 0x-hex U256.
    pub debt_to_cover: Option<String>,
    /// Caller to simulate from; defaults to the opportunity's `from`.
    pub from: Option<String>,
    pub receive_a_token: Option<bool>,
}

fn sim_error(msg: impl Into<String>) -> Json<Value> {
    Json(json!({ "success": false, "error": msg.into() }))
}

/// POST /api/simulate/{address}
///
/// Forks the latest block with `revm` and executes
/// `Pool.liquidationCall(collateral, debt, user, debtToCover, receiveAToken)`
/// in-process. Read-only: nothing is signed, nothing is broadcast.
async fn post_simulate(
    State(state): State<Arc<AppState>>,
    Path(address): Path<String>,
    body: String,
) -> Json<Value> {
    let req: SimulateRequest = if body.trim().is_empty() {
        SimulateRequest::default()
    } else {
        match serde_json::from_str(&body) {
            Ok(r) => r,
            Err(e) => return sim_error(format!("invalid JSON body: {e}")),
        }
    };

    let user: Address = match address.trim().parse() {
        Ok(a) => a,
        Err(e) => return sim_error(format!("invalid address {address:?}: {e}")),
    };

    // --- locate the opportunity (by borrower first, then by id) ---
    let opportunity = {
        let guard = state.recent_opportunities.read().await;
        guard
            .values()
            .find(|o| o.borrower == Some(user))
            .or_else(|| {
                guard
                    .values()
                    .find(|o| o.id.eq_ignore_ascii_case(address.trim()))
            })
            .cloned()
    };
    let Some(opportunity) = opportunity else {
        return sim_error("No opportunity found for address");
    };

    // --- resolve the liquidationCall arguments ---
    let collateral_asset = match (&req.collateral_asset, opportunity.collateral) {
        (Some(s), _) => match s.trim().parse::<Address>() {
            Ok(a) => Some(a),
            Err(e) => return sim_error(format!("invalid collateral_asset: {e}")),
        },
        (None, c) => c,
    };
    let debt_asset = match (&req.debt_asset, opportunity.debt_asset) {
        (Some(s), _) => match s.trim().parse::<Address>() {
            Ok(a) => Some(a),
            Err(e) => return sim_error(format!("invalid debt_asset: {e}")),
        },
        (None, d) => d,
    };
    let debt_to_cover = match (&req.debt_to_cover, opportunity.repay_amount) {
        (Some(s), _) => match s.trim().parse::<U256>() {
            Ok(v) => Some(v),
            Err(e) => return sim_error(format!("invalid debt_to_cover: {e}")),
        },
        (None, r) => r,
    };

    let mut missing: Vec<&str> = Vec::new();
    if collateral_asset.is_none() {
        missing.push("collateral_asset");
    }
    if debt_asset.is_none() {
        missing.push("debt_asset");
    }
    if debt_to_cover.is_none() {
        missing.push("debt_to_cover");
    }
    if !missing.is_empty() {
        return Json(json!({
            "success": false,
            "error": format!(
                "cannot encode liquidationCall: missing {}. The keeper records collateral/debt_asset as null, so supply them in the POST body.",
                missing.join(", ")
            ),
            "missing": missing,
            "opportunity_id": opportunity.id,
        }));
    }
    let (collateral_asset, debt_asset, debt_to_cover) = (
        collateral_asset.expect("checked above"),
        debt_asset.expect("checked above"),
        debt_to_cover.expect("checked above"),
    );

    let Some(rpc_url) = state.state_rpc.as_deref() else {
        return sim_error(
            "no state RPC configured; simulation needs HTTP_RPC or WS_RPC so revm can fork state",
        );
    };

    let from = match &req.from {
        Some(s) => match s.trim().parse::<Address>() {
            Ok(a) => a,
            Err(e) => return sim_error(format!("invalid from: {e}")),
        },
        None => opportunity.from,
    };
    let receive_a_token = req.receive_a_token.unwrap_or(false);

    // --- encode the call ---
    let calldata: Bytes = IAaveV3Pool::liquidationCallCall {
        collateralAsset: collateral_asset,
        debtAsset: debt_asset,
        user,
        debtToCover: debt_to_cover,
        receiveAToken: receive_a_token,
    }
    .abi_encode()
    .into();

    // --- fork latest state ---
    let provider = match ProviderBuilder::new().connect(rpc_url).await {
        Ok(p) => p.erased(),
        Err(e) => return sim_error(format!("rpc connect failed: {e}")),
    };
    let block = match provider.get_block_number().await {
        Ok(b) => b,
        Err(e) => return sim_error(format!("eth_blockNumber failed: {e}")),
    };
    let gas_price_wei: u128 = provider.get_gas_price().await.unwrap_or(1_000_000_000);

    let backend: Arc<dyn StateBackend> = Arc::new(RpcStateBackend::new(provider));

    let sim = match simulate_raw_call(
        backend,
        block,
        from,
        opportunity.to,
        calldata,
        U256::ZERO,
        opportunity.gas_limit.max(1_000_000),
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return sim_error(format!("revm simulation failed: {e}")),
    };

    let gas_cost_wei = U256::from(sim.gas_used).saturating_mul(U256::from(gas_price_wei));

    if !sim.success {
        warn!(
            "simulate {user}: reverted at block {block}: {}",
            sim.failure.as_deref().unwrap_or("unknown")
        );
        return Json(json!({
            "success": false,
            "error": sim.failure.unwrap_or_else(|| "reverted".to_string()),
            "address": format!("{user:#x}"),
            "opportunity_id": opportunity.id,
            "block_simulated": block,
            "gas_used": sim.gas_used,
            "gas_cost_wei": format!("{gas_cost_wei:#x}"),
        }));
    }

    // --- read the exact amounts out of the LiquidationCall log ---
    let topic0 = keccak256(LIQUIDATION_CALL_SIG);
    let mut collateral_seized: Option<U256> = None;
    let mut debt_repaid: Option<U256> = None;
    for (addr, topics, data) in &sim.logs {
        if *addr == opportunity.to && topics.first() == Some(&topic0) && data.len() >= 64 {
            debt_repaid = Some(U256::from_be_slice(&data[0..32]));
            collateral_seized = Some(U256::from_be_slice(&data[32..64]));
            break;
        }
    }

    // Net profit is the keeper's gross payout estimate minus the gas this
    // simulation actually burned. The seized/repaid figures are raw token units
    // from the event and are NOT converted to wei: that needs a price per
    // asset, which this endpoint deliberately does not fetch.
    let estimated_net_profit_wei = opportunity.gross_profit_wei.saturating_sub(gas_cost_wei);

    Json(json!({
        "success": true,
        "address": format!("{user:#x}"),
        "opportunity_id": opportunity.id,
        "block_simulated": block,
        "gas_used": sim.gas_used,
        "gas_price_wei": format!("{gas_price_wei:#x}"),
        "gas_cost_wei": format!("{gas_cost_wei:#x}"),
        "collateral_seized": collateral_seized.map(|v| format!("{v:#x}")),
        "debt_repaid": debt_repaid.map(|v| format!("{v:#x}")),
        "liquidation_call_event_found": collateral_seized.is_some(),
        "estimated_net_profit_wei": format!("{estimated_net_profit_wei:#x}"),
        "gross_profit_wei": format!("{:#x}", opportunity.gross_profit_wei),
        "profit_basis": "opportunity gross_profit_wei minus simulated gas cost",
        "collateral_asset": format!("{collateral_asset:#x}"),
        "debt_asset": format!("{debt_asset:#x}"),
        "receive_a_token": receive_a_token,
        "simulated_from": format!("{from:#x}"),
        "broadcast": false,
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
