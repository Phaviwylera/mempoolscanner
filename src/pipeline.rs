//! Stage 1, Active Store & Settlement Reaper:
//! Alloy WebSocket/IPC pending-transaction ingestion, active open-transaction store,
//! and real-time pruning of closed / mined / user-settled transactions from `hits.jsonl`.

use crate::abi::{decode_calldata, match_calldata, REGISTRY};
use crate::config::Settings;
use crate::exec::{execute_liquidation, LiquidationOpportunity, ExecConfig};
use crate::liquidation::liquidation_watcher_loop;
use crate::sim::{evaluate_transaction, HitRecord, ObservedTx, SimulationParams};
use crate::state::{one_line_str, ForkMonitor, RpcStateBackend, StateBackend};
use alloy::consensus::{BlockHeader, Transaction as ConsensusTx};
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::{BlockResponse, TransactionResponse};
use alloy::primitives::{Address, B256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::{BlockTransactions, Transaction as RpcTransaction};
use anyhow::Result;
use chrono::{SecondsFormat, Utc};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch, Mutex, Semaphore};
use tracing::{debug, error, info, warn};

const MAX_ACTIVE_HITS: usize = 500;
const MAX_CLOSED_RING: usize = 2_048;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ScannerMeta {
    pub total_observed: u64,
    pub total_pruned: u64,
    pub pruned_closed_onchain: u64,
    pub pruned_locked_to_user: u64,
    pub active_count: usize,
    pub last_observed_hash: Option<String>,
    pub updated_ts: String,
}

struct TrackedHit {
    record: HitRecord,
    hash_lc: String,
    added_at: Instant,
    missing_checks: u8,
}

struct StoreInner {
    entries: VecDeque<TrackedHit>,
    closed_ring: VecDeque<String>,
    closed_set: HashSet<String>,
    meta: ScannerMeta,
    log_path: PathBuf,
    meta_path: PathBuf,
}

#[derive(Clone)]
pub struct ActiveHitStore {
    inner: Arc<Mutex<StoreInner>>,
}

impl ActiveHitStore {
    pub fn load(log_path: PathBuf) -> Self {
        let meta_path = sibling_path(&log_path, "scanner_meta.json");
        let mut meta: ScannerMeta = fs::read_to_string(&meta_path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();

        let mut entries = VecDeque::new();
        let mut seen = HashSet::new();
        let mut had_skipped = false;
        if let Ok(contents) = fs::read_to_string(&log_path) {
            for line in contents.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(mut rec) = serde_json::from_str::<HitRecord>(line) else {
                    had_skipped = true;
                    continue;
                };
                if rec.decision.status == "beneficiary_locked" {
                    meta.total_pruned = meta.total_pruned.saturating_add(1);
                    meta.pruned_locked_to_user = meta.pruned_locked_to_user.saturating_add(1);
                    had_skipped = true;
                    continue;
                }
                if rec.decision.status == "extractable"
                    && (!rec.decision.would_frontrun
                        || rec.decision.gross_value_wei == "0"
                        || rec.decision.net_profit_wei == "0"
                        || rec.decision.net_profit_wei.starts_with('-'))
                {
                    rec.decision.status = "not_applicable".to_string();
                    rec.decision.would_frontrun = false;
                    had_skipped = true;
                }
                let h = rec.hash.to_ascii_lowercase();
                if seen.insert(h.clone()) {
                    entries.push_back(TrackedHit {
                        record: rec,
                        hash_lc: h,
                        added_at: Instant::now()
                            .checked_sub(Duration::from_secs(60))
                            .unwrap_or_else(Instant::now),
                        missing_checks: 0,
                    });
                } else {
                    had_skipped = true;
                }
            }
        }
        while entries.len() > MAX_ACTIVE_HITS {
            entries.pop_front();
            had_skipped = true;
        }
        meta.active_count = entries.len();
        meta.updated_ts = now_iso();

        if had_skipped {
            let lines: Vec<String> = entries
                .iter()
                .filter_map(|e| serde_json::to_string(&e.record).ok())
                .collect();
            write_jsonl_atomic(&log_path, &lines);
            write_meta_atomic(&meta_path, &meta);
        }

        Self {
            inner: Arc::new(Mutex::new(StoreInner {
                entries,
                closed_ring: VecDeque::new(),
                closed_set: HashSet::new(),
                meta,
                log_path,
                meta_path,
            })),
        }
    }

    /// Admit a newly simulated pending observation, or drop it immediately if its payout
    /// is locked exclusively to the original user (`beneficiary_locked`) or already closed.
    pub async fn ingest_record(&self, record: HitRecord) -> bool {
        let mut guard = self.inner.lock().await;
        let hash_lc = record.hash.to_ascii_lowercase();

        guard.meta.total_observed = guard.meta.total_observed.saturating_add(1);
        guard.meta.last_observed_hash = Some(record.hash.clone());
        guard.meta.updated_ts = now_iso();

        // Remove immediately if payout is bound exclusively to original user.
        if record.decision.status == "beneficiary_locked" {
            guard.meta.total_pruned = guard.meta.total_pruned.saturating_add(1);
            guard.meta.pruned_locked_to_user = guard.meta.pruned_locked_to_user.saturating_add(1);
            mark_closed_inner(&mut guard, hash_lc);
            let snapshot_meta = guard.meta.clone();
            let meta_path = guard.meta_path.clone();
            drop(guard);
            let _ = tokio::task::spawn_blocking(move || write_meta_atomic(&meta_path, &snapshot_meta)).await;
            return false;
        }

        if guard.closed_set.contains(&hash_lc)
            || guard.entries.iter().any(|e| e.hash_lc == hash_lc)
        {
            let snapshot_meta = guard.meta.clone();
            let meta_path = guard.meta_path.clone();
            drop(guard);
            let _ = tokio::task::spawn_blocking(move || write_meta_atomic(&meta_path, &snapshot_meta)).await;
            return false;
        }

        guard.entries.push_back(TrackedHit {
            record,
            hash_lc,
            added_at: Instant::now(),
            missing_checks: 0,
        });
        while guard.entries.len() > MAX_ACTIVE_HITS {
            guard.entries.pop_front();
        }
        guard.meta.active_count = guard.entries.len();

        let lines = serialize_entries(&guard.entries);
        let snapshot_meta = guard.meta.clone();
        let log_path = guard.log_path.clone();
        let meta_path = guard.meta_path.clone();
        drop(guard);

        let _ = tokio::task::spawn_blocking(move || {
            write_jsonl_atomic(&log_path, &lines);
            write_meta_atomic(&meta_path, &snapshot_meta);
        })
        .await;
        true
    }

    /// Purge a set of closed/settled transaction hashes from memory and `hits.jsonl`.
    pub async fn prune_closed_hashes(&self, closed_hashes_lc: &HashSet<String>) -> usize {
        if closed_hashes_lc.is_empty() {
            return 0;
        }
        let mut guard = self.inner.lock().await;
        let before = guard.entries.len();
        let mut removed_hashes = Vec::new();

        guard.entries.retain(|entry| {
            if closed_hashes_lc.contains(&entry.hash_lc) {
                removed_hashes.push(entry.hash_lc.clone());
                false
            } else {
                true
            }
        });

        let removed = before.saturating_sub(guard.entries.len());
        if removed == 0 {
            return 0;
        }

        for h in removed_hashes {
            mark_closed_inner(&mut guard, h);
        }
        guard.meta.total_pruned = guard.meta.total_pruned.saturating_add(removed as u64);
        guard.meta.pruned_closed_onchain = guard
            .meta
            .pruned_closed_onchain
            .saturating_add(removed as u64);
        guard.meta.active_count = guard.entries.len();
        guard.meta.updated_ts = now_iso();

        let lines = serialize_entries(&guard.entries);
        let snapshot_meta = guard.meta.clone();
        let log_path = guard.log_path.clone();
        let meta_path = guard.meta_path.clone();
        drop(guard);

        let _ = tokio::task::spawn_blocking(move || {
            write_jsonl_atomic(&log_path, &lines);
            write_meta_atomic(&meta_path, &snapshot_meta);
        })
        .await;
        info!("Pruned {} closed/settled transaction(s) from scanner", removed);
        removed
    }

    pub async fn meta(&self) -> ScannerMeta {
        self.inner.lock().await.meta.clone()
    }
}

fn mark_closed_inner(guard: &mut StoreInner, hash_lc: String) {
    if guard.closed_set.insert(hash_lc.clone()) {
        guard.closed_ring.push_back(hash_lc);
        while guard.closed_ring.len() > MAX_CLOSED_RING {
            if let Some(old) = guard.closed_ring.pop_front() {
                guard.closed_set.remove(&old);
            }
        }
    }
}

fn serialize_entries(entries: &VecDeque<TrackedHit>) -> Vec<String> {
    entries
        .iter()
        .filter_map(|e| serde_json::to_string(&e.record).ok())
        .collect()
}

fn sibling_path(path: &Path, filename: &str) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(filename),
        _ => PathBuf::from(filename),
    }
}

fn write_jsonl_atomic(path: &Path, lines: &[String]) {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }
    let tmp_name = format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("hits.jsonl")
    );
    let tmp_path = sibling_path(path, &tmp_name);
    if let Ok(mut file) = File::create(&tmp_path) {
        for line in lines {
            let _ = writeln!(file, "{line}");
        }
        let _ = file.flush();
        let _ = fs::rename(&tmp_path, path);
    }
}

fn write_meta_atomic(path: &Path, meta: &ScannerMeta) {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }
    let tmp_path = sibling_path(path, ".scanner_meta.json.tmp");
    if let Ok(payload) = serde_json::to_vec(meta) {
        if let Ok(mut file) = File::create(&tmp_path) {
            let _ = file.write_all(&payload);
            let _ = file.flush();
            let _ = fs::rename(&tmp_path, path);
        }
    }
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub async fn run_scanner(settings: Settings, exec_config: Option<ExecConfig>) -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let state_provider = connect_http_with_retry(&settings.state_rpc).await?;

    let pubsub_endpoint = settings
        .ipc_rpc
        .clone()
        .unwrap_or_else(|| settings.ws_rpc.clone());

    let initial_ws = match ProviderBuilder::new().connect(&pubsub_endpoint).await {
        Ok(p) => Some(p.erased()),
        Err(err) => {
            warn!(
                "Initial pubsub connection to {} deferred ({}); supervisor will retry",
                pubsub_endpoint,
                one_line_str(&err.to_string())
            );
            None
        }
    };

    let monitor = Arc::new(ForkMonitor::new(
        state_provider.clone(),
        None,
        settings.poll_seconds,
    ));
    monitor.refresh().await;

    let backend: Arc<dyn StateBackend> = Arc::new(RpcStateBackend::new(state_provider.clone()));
    let hit_store = ActiveHitStore::load(settings.log_path.clone());

    let (record_tx, mut record_rx) = mpsc::channel::<HitRecord>(4_096);
    let writer_store = hit_store.clone();
    let writer_handle = tokio::spawn(async move {
        while let Some(record) = record_rx.recv().await {
            writer_store.ingest_record(record).await;
        }
    });

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let monitor_poller = {
        let m = monitor.clone();
        let mut srx = shutdown_rx.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = m.poll_forever() => {}
                _ = srx.changed() => {}
            }
        })
    };

    let reaper_handle = {
        let store = hit_store.clone();
        let provider = state_provider.clone();
        let mut srx = shutdown_rx.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = settlement_reaper_loop(store, provider) => {}
                _ = srx.changed() => {}
            }
        })
    };

    let liq_handle = {
        let log_path = settings.log_path.clone();
        let provider = state_provider.clone();
        let mon = monitor.clone();
        let back = backend.clone();
        let mut srx = shutdown_rx.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = liquidation_watcher_loop(log_path, provider, mon, back) => {}
                _ = srx.changed() => {}
            }
        })
    };

    info!(
        "Rust Revm scanner ready: watching {} selectors (max_lag={}, depth={}, poll={:.1}s, workers={})",
        REGISTRY.len(),
        settings.max_lag,
        settings.depth,
        settings.poll_seconds,
        settings.worker_concurrency,
    );

    let ingest_fut = ingestion_supervisor(
        pubsub_endpoint,
        initial_ws,
        state_provider,
        monitor,
        backend,
        record_tx.clone(),
        settings,
        shutdown_rx,
        exec_config,
    );

    tokio::select! {
        res = ingest_fut => {
            if let Err(err) = res {
                error!("Ingestion supervisor exited with error: {err}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Shutdown signal received; draining active workers...");
        }
    }

    let _ = shutdown_tx.send(true);
    drop(record_tx);
    let _ = monitor_poller.await;
    let _ = reaper_handle.await;
    let _ = liq_handle.await;
    let _ = writer_handle.await;
    info!("Shutdown complete");
    Ok(())
}

async fn connect_http_with_retry(endpoint: &str) -> Result<DynProvider> {
    let mut attempt: u32 = 0;
    loop {
        match ProviderBuilder::new().connect(endpoint).await {
            Ok(p) => return Ok(p.erased()),
            Err(err) if attempt < 5 => {
                attempt += 1;
                warn!(
                    "State RPC connect attempt {} failed ({}); retrying...",
                    attempt,
                    one_line_str(&err.to_string())
                );
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(err) => return Err(err.into()),
        }
    }
}

/// Continuously monitors newly mined blocks and sender nonces to purge any transaction
/// that has closed on-chain (settled to the original user, replaced, or dropped).
async fn settlement_reaper_loop(store: ActiveHitStore, provider: DynProvider) {
    let mut last_block_swept: Option<u64> = None;

    loop {
        tokio::time::sleep(Duration::from_secs(4)).await;

        let active_len = { store.inner.lock().await.entries.len() };
        if active_len == 0 {
            continue;
        }

        // 1. Block-level sweep: collect mined tx hashes from newly sealed blocks.
        let mut mined_hashes_lc = HashSet::new();
        if let Ok(Some(latest_block)) = provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await
        {
            let latest_num = latest_block.header().number();
            collect_block_tx_hashes(&latest_block.transactions, &mut mined_hashes_lc);

            if let Some(prev) = last_block_swept {
                if latest_num > prev + 1 {
                    let start = (prev + 1).max(latest_num.saturating_sub(3));
                    for bn in start..latest_num {
                        if let Ok(Some(blk)) = provider
                            .get_block_by_number(BlockNumberOrTag::Number(bn))
                            .await
                        {
                            collect_block_tx_hashes(&blk.transactions, &mut mined_hashes_lc);
                        }
                    }
                }
            }
            last_block_swept = Some(latest_num);
        }

        if !mined_hashes_lc.is_empty() {
            store.prune_closed_hashes(&mined_hashes_lc).await;
        }

        // 2. Per-transaction & sender-nonce sweep (prioritizing extractable open txs).
        #[derive(Clone)]
        struct Candidate {
            hash_lc: String,
            from: String,
            nonce: Option<u64>,
            age_secs: u64,
            is_extractable: bool,
        }

        let candidates: Vec<Candidate> = {
            let guard = store.inner.lock().await;
            let mut list = Vec::new();
            for e in guard.entries.iter() {
                let age_secs = e.added_at.elapsed().as_secs();
                let is_ext = e.record.decision.status == "extractable";
                if (is_ext && age_secs >= 2) || age_secs >= 8 {
                    list.push(Candidate {
                        hash_lc: e.hash_lc.clone(),
                        from: e.record.from.clone(),
                        nonce: e.record.nonce,
                        age_secs,
                        is_extractable: is_ext,
                    });
                }
            }
            list.sort_by_key(|c| (!c.is_extractable, std::cmp::Reverse(c.age_secs)));
            list.truncate(16);
            list
        };

        if candidates.is_empty() {
            continue;
        }

        let mut closed_now = HashSet::new();
        let mut missing_delta: HashMap<String, bool> = HashMap::new();
        let mut sender_nonce_cache: HashMap<Address, u64> = HashMap::new();

        for cand in candidates {
            if cand.age_secs > 900 || (!cand.is_extractable && cand.age_secs > 600) {
                closed_now.insert(cand.hash_lc);
                continue;
            }

            let Ok(tx_hash) = B256::from_str(&cand.hash_lc) else {
                closed_now.insert(cand.hash_lc);
                continue;
            };

            let tx_lookup = tokio::time::timeout(
                Duration::from_millis(2_500),
                provider.get_transaction_by_hash(tx_hash),
            )
            .await;

            match tx_lookup {
                Ok(Ok(Some(rpc_tx))) => {
                    if TransactionResponse::block_number(&rpc_tx).is_some() {
                        // Transaction mined & closed on-chain.
                        closed_now.insert(cand.hash_lc);
                        continue;
                    }
                    missing_delta.insert(cand.hash_lc.clone(), false);

                    // Even if mempool cache still shows pending, check if sender's nonce settled.
                    if let (Some(tx_nonce), Ok(from_addr)) =
                        (cand.nonce, Address::from_str(&cand.from))
                    {
                        if cand.is_extractable || cand.age_secs >= 15 {
                            if let Some(cur_nonce) =
                                query_latest_nonce(&provider, from_addr, &mut sender_nonce_cache)
                                    .await
                            {
                                if cur_nonce > tx_nonce {
                                    closed_now.insert(cand.hash_lc);
                                }
                            }
                        }
                    }
                }
                Ok(Ok(None)) => {
                    // Absent from node mempool: check if sender's nonce settled, else count missing check.
                    let mut settled = false;
                    if let (Some(tx_nonce), Ok(from_addr)) =
                        (cand.nonce, Address::from_str(&cand.from))
                    {
                        if let Some(cur_nonce) =
                            query_latest_nonce(&provider, from_addr, &mut sender_nonce_cache).await
                        {
                            if cur_nonce > tx_nonce {
                                settled = true;
                            }
                        }
                    }
                    if settled {
                        closed_now.insert(cand.hash_lc);
                    } else {
                        missing_delta.insert(cand.hash_lc, true);
                    }
                }
                _ => {}
            }
        }

        if !missing_delta.is_empty() {
            let mut guard = store.inner.lock().await;
            for entry in guard.entries.iter_mut() {
                if let Some(&was_missing) = missing_delta.get(&entry.hash_lc) {
                    if was_missing {
                        entry.missing_checks = entry.missing_checks.saturating_add(1);
                        if entry.missing_checks >= 2 {
                            closed_now.insert(entry.hash_lc.clone());
                        }
                    } else {
                        entry.missing_checks = 0;
                    }
                }
            }
        }

        if !closed_now.is_empty() {
            store.prune_closed_hashes(&closed_now).await;
        }
    }
}

fn collect_block_tx_hashes<T: TransactionResponse>(
    txs: &BlockTransactions<T>,
    out: &mut HashSet<String>,
) {
    match txs {
        BlockTransactions::Hashes(hashes) => {
            for h in hashes {
                out.insert(format!("{h:#x}"));
            }
        }
        BlockTransactions::Full(items) => {
            for tx in items {
                let h = TransactionResponse::tx_hash(tx);
                out.insert(format!("{h:#x}"));
            }
        }
        BlockTransactions::Uncle => {}
    }
}

async fn query_latest_nonce(
    provider: &DynProvider,
    addr: Address,
    cache: &mut HashMap<Address, u64>,
) -> Option<u64> {
    if let Some(&n) = cache.get(&addr) {
        return Some(n);
    }
    let res = tokio::time::timeout(
        Duration::from_millis(2_000),
        provider.get_transaction_count(addr).block_id(BlockId::latest()),
    )
    .await;
    if let Ok(Ok(n)) = res {
        cache.insert(addr, n);
        return Some(n);
    }
    None
}

#[allow(clippy::too_many_arguments)]
async fn ingestion_supervisor(
    endpoint: String,
    mut current_ws: Option<DynProvider>,
    http_fallback: DynProvider,
    monitor: Arc<ForkMonitor>,
    backend: Arc<dyn StateBackend>,
    record_tx: mpsc::Sender<HitRecord>,
    settings: Settings,
    mut shutdown_rx: watch::Receiver<bool>,
    exec_config: Option<Arc<ExecConfig>>,
) -> Result<()> {
    let sim_sem = Arc::new(Semaphore::new(settings.worker_concurrency));
    let fetch_sem = Arc::new(Semaphore::new((settings.worker_concurrency * 2).clamp(8, 24)));
    let sim_params = Arc::new(SimulationParams {
        max_lag: settings.max_lag,
        depth: settings.depth,
        timeout: Duration::from_millis(settings.sim_timeout_ms),
    });

    let mut attempt: u32 = if current_ws.is_some() { 0 } else { 1 };
    let mut prefer_full_tx = true;
    loop {
        if *shutdown_rx.borrow() {
            return Ok(());
        }

        if current_ws.is_none() || attempt > 0 {
            if attempt > 0 {
                let delay = Duration::from_secs((2u64.pow(attempt.min(4))).min(15));
                warn!("Reconnecting to pending stream in {:?}...", delay);
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = shutdown_rx.changed() => return Ok(()),
                }
            }
            match ProviderBuilder::new().connect(&endpoint).await {
                Ok(p) => {
                    current_ws = Some(p.erased());
                }
                Err(err) => {
                    warn!("WebSocket connect failed: {}", one_line_str(&err.to_string()));
                    attempt = attempt.saturating_add(1);
                    continue;
                }
            }
        }

        let ws_conn = current_ws.as_ref().expect("ws_conn populated").clone();

        if prefer_full_tx {
            match ws_conn.subscribe_full_pending_transactions().await {
                Ok(sub) => {
                    info!("Subscribed to full pending transaction stream (0-hop mode)");
                    let mut stream = sub.into_stream();
                    let first = tokio::select! {
                        res = tokio::time::timeout(Duration::from_secs(12), stream.next()) => Some(res),
                        _ = shutdown_rx.changed() => None,
                    };
                    let Some(first_res) = first else {
                        return Ok(());
                    };
                    match first_res {
                        Ok(Some(first_tx)) => {
                            attempt = 1;
                            dispatch_full_tx(
                                first_tx,
                                0,
                                "ws_full",
                                &sim_sem,
                                &monitor,
                                &backend,
                                &sim_params,
                                &record_tx,
                                exec_config.clone(),
                            );
                            loop {
                                tokio::select! {
                                    maybe_tx = stream.next() => {
                                        let Some(tx) = maybe_tx else {
                                            warn!("Full pending transaction stream closed");
                                            current_ws = None;
                                            break;
                                        };
                                        dispatch_full_tx(
                                            tx,
                                            0,
                                            "ws_full",
                                            &sim_sem,
                                            &monitor,
                                            &backend,
                                            &sim_params,
                                            &record_tx,
                                            exec_config.clone(),
                                        );
                                    }
                                    _ = shutdown_rx.changed() => return Ok(()),
                                }
                            }
                            continue;
                        }
                        Ok(None) | Err(_) => {
                            info!("Full pending stream yielded no items within 12s; falling back to hash subscription");
                            prefer_full_tx = false;
                        }
                    }
                }
                Err(full_err) => {
                    debug!(
                        "Full pending subscription unsupported ({}); using hash subscription",
                        one_line_str(&full_err.to_string())
                    );
                    prefer_full_tx = false;
                }
            }
        }

        match ws_conn.subscribe_pending_transactions().await {
            Ok(sub) => {
                attempt = 1;
                info!("Subscribed to pending transaction hashes (concurrent fetch mode)");
                let mut stream = sub.into_stream();
                loop {
                    tokio::select! {
                        maybe_hash = stream.next() => {
                            let Some(tx_hash) = maybe_hash else {
                                warn!("Pending hash stream closed");
                                current_ws = None;
                                break;
                            };
                            dispatch_hash_tx(
                                tx_hash,
                                ws_conn.clone(),
                                http_fallback.clone(),
                                &fetch_sem,
                                &sim_sem,
                                &monitor,
                                &backend,
                                &sim_params,
                                &record_tx,
                            );
                        }
                        _ = shutdown_rx.changed() => return Ok(()),
                    }
                }
            }
            Err(hash_err) => {
                warn!(
                    "Pending hash subscription failed: {}",
                    one_line_str(&hash_err.to_string())
                );
                current_ws = None;
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch_hash_tx(
    tx_hash: B256,
    ws_provider: DynProvider,
    http_provider: DynProvider,
    fetch_sem: &Arc<Semaphore>,
    sim_sem: &Arc<Semaphore>,
    monitor: &Arc<ForkMonitor>,
    backend: &Arc<dyn StateBackend>,
    sim_params: &Arc<SimulationParams>,
    record_tx: &mpsc::Sender<HitRecord>,
) {
    let Ok(fetch_permit) = fetch_sem.clone().try_acquire_owned() else {
        return;
    };
    let sim_sem = sim_sem.clone();
    let monitor = monitor.clone();
    let backend = backend.clone();
    let sim_params = sim_params.clone();
    let record_tx = record_tx.clone();

    tokio::spawn(async move {
        let t0 = Instant::now();
        let fetched = match tokio::time::timeout(
            Duration::from_millis(2_500),
            ws_provider.get_transaction_by_hash(tx_hash),
        )
        .await
        {
            Ok(Ok(Some(tx))) => Some(tx),
            _ => match tokio::time::timeout(
                Duration::from_millis(2_500),
                http_provider.get_transaction_by_hash(tx_hash),
            )
            .await
            {
                Ok(Ok(Some(tx))) => Some(tx),
                _ => None,
            },
        };
        let ingest_us = t0.elapsed().as_micros() as u64;
        drop(fetch_permit);

        if let Some(tx) = fetched {
            dispatch_full_tx(
                tx,
                ingest_us,
                "ws_hash",
                &sim_sem,
                &monitor,
                &backend,
                &sim_params,
                &record_tx,
                exec_config.clone(),
            );
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn dispatch_full_tx(
    tx: RpcTransaction,
    ingest_us: u64,
    source: &'static str,
    sim_sem: &Arc<Semaphore>,
    monitor: &Arc<ForkMonitor>,
    backend: &Arc<dyn StateBackend>,
    sim_params: &Arc<SimulationParams>,
    record_tx: &mpsc::Sender<HitRecord>,
    exec_config: Option<Arc<ExecConfig>>,
) {
    // Ignore transactions that are already mined/closed on-chain.
    if TransactionResponse::block_number(&tx).is_some() {
        return;
    }
    let Some(to_addr) = ConsensusTx::to(&tx) else {
        return;
    };
    let input = ConsensusTx::input(&tx);
    if input.len() < 4 {
        return;
    }

    let decode_t0 = Instant::now();
    let Some(spec) = match_calldata(input) else {
        return;
    };
    let value = ConsensusTx::value(&tx);
    let decoded = decode_calldata(spec, input, value);
    let decoded_clone = decoded.clone();
    let decode_us = decode_t0.elapsed().as_micros() as u64;

    let Ok(sim_permit) = sim_sem.clone().try_acquire_owned() else {
        return;
    };

    let from_addr = TransactionResponse::from(&tx);
    let tx_hash = TransactionResponse::tx_hash(&tx);
    let nonce = Some(ConsensusTx::nonce(&tx));
    let gas_limit = ConsensusTx::gas_limit(&tx);
    let tip_wei = ConsensusTx::max_priority_fee_per_gas(&tx);
    let max_fee_wei = Some(ConsensusTx::max_fee_per_gas(&tx));
    let calldata = input.clone();
    let ts_iso = now_iso();

    let observed = ObservedTx {
        hash: format!("{tx_hash:#x}"),
        from: from_addr,
        to: to_addr,
        nonce,
        value,
        gas_limit,
        tip_wei,
        max_fee_wei,
        calldata,
        selector_hex: spec.selector_hex.clone(),
        timestamp_iso: ts_iso,
        ingest_us,
        source,
    };
    let observed_clone = observed.clone();

    let monitor = monitor.clone();
    let backend = backend.clone();
    let sim_params = sim_params.clone();
    let record_tx = record_tx.clone();

    tokio::spawn(async move {
        let _permit = sim_permit;
        let fork_status = monitor.current_status().await;
        let ref_snap = match fork_status.block {
            Some(b) if sim_params.depth > 0 && b >= sim_params.depth => {
                Some(monitor.snapshot_for(b - sim_params.depth).await)
            }
            _ => None,
        };

        let record = evaluate_transaction(
            spec,
            decoded,
            decode_us,
            observed,
            fork_status,
            ref_snap,
            backend,
            &sim_params,
        )
        .await;

        let should_exec = exec_config.is_some()
            && record.decoded.category == "liquidate"
            && record.decision.status == "extractable";
        let record_for_exec = should_exec.then(|| record.clone());

        if let Ok(line) = serde_json::to_string(&record) {
            info!("hit {line}");
        }
        let _ = record_tx.send(record).await;

        // Opt-in execution: sign and (unless dry-run) broadcast liquidate hits
        // that passed the simulation gate. Runs outside the hot store path.
        if let Some(record) = record_for_exec {
            let cfg = exec_config.expect("exec_config present when should_exec");
            let decoded = decoded_clone;
            let observed = observed_clone;
            tokio::spawn(async move {
                let mut opportunity =
                    LiquidationOpportunity::from_hit_record(&record, &decoded, &observed);
                opportunity.chain_id = cfg.chain_id;
                match execute_liquidation(&cfg, &opportunity).await {
                    Ok(outcome) => {
                        info!("execution {} -> {:?}", opportunity.id, outcome)
                    }
                    Err(err) => {
                        warn!("execution for {} failed: {}", opportunity.id, err)
                    }
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::DecodedCall;
    use crate::sim::{DecisionReport, ForkEvidence, SimOutcome, StageTelemetry};

    fn dummy_record(hash: &str, status: &str, gross_wei: &str) -> HitRecord {
        HitRecord {
            name: "claim".to_string(),
            hash: hash.to_string(),
            from: "0x1111111111111111111111111111111111111111".to_string(),
            to: "0x2222222222222222222222222222222222222222".to_string(),
            nonce: Some(7),
            value: "0".to_string(),
            selector: "0x4e71d92d".to_string(),
            gas: 120_000,
            tip: Some(1_000_000_000),
            ts: "2026-11-13T12:00:00.000Z".to_string(),
            fork: ForkEvidence {
                block: Some(100),
                head: Some(100),
                lag: Some(0),
                depth: 1,
                code_present: Some(true),
                stale: false,
                stable: true,
                checked: vec![100, 99],
                reference: None,
            },
            sim: SimOutcome {
                ok: true,
                engine: "revm".to_string(),
                gas_used: 45_000,
                result_or_error: "0x".to_string(),
                revert_data: None,
                revert_kind: None,
                revert_reason: None,
                transfers: Vec::new(),
            },
            decoded: DecodedCall {
                signature: "claim()".to_string(),
                category: "claim".to_string(),
                decode_ok: true,
                recipient: None,
                token_in: None,
                token_out: None,
                amount_in: None,
                amount_out_min: None,
                deadline: None,
                summary: "claim()".to_string(),
            },
            decision: DecisionReport {
                status: status.to_string(),
                probe_mode: "caller_substitution".to_string(),
                probe_ok: status == "extractable",
                probe_gas_used: 45_000,
                would_frontrun: status == "extractable",
                gross_value_wei: gross_wei.to_string(),
                gas_cost_wei: "50000000000000".to_string(),
                priority_fee_wei: "2000000000".to_string(),
                builder_bribe_wei: "0".to_string(),
                net_profit_wei: gross_wei.to_string(),
                probe_token_gains: 0,
                verdict: status.to_string(),
            },
            telemetry: StageTelemetry {
                source: "ws_full".to_string(),
                ingest_us: 100,
                decode_us: 5,
                sim_us: 350,
                probe_us: 180,
                total_us: 635,
            },
            label: "candidate:claim".to_string(),
        }
    }

    #[tokio::test]
    async fn tls_providers_initialize_without_crypto_provider_panic() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = ProviderBuilder::new().connect("https://127.0.0.1:1").await;
        assert!(http.is_ok());
        let ws = ProviderBuilder::new().connect("wss://127.0.0.1:1").await;
        assert!(ws.is_err());
    }

    #[tokio::test]
    async fn active_hit_store_removes_closed_and_beneficiary_locked_transactions() {
        let dir = std::env::temp_dir().join(format!(
            "mempoolscanner_store_test_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let log_path = dir.join("hits.jsonl");

        let store = ActiveHitStore::load(log_path.clone());

        // 1. Beneficiary-locked tx (moves only to original user) is rejected immediately.
        let locked = dummy_record("0xaaaa", "beneficiary_locked", "0");
        assert!(!store.ingest_record(locked).await);

        // 2. Open extractable tx + open sender-protected tx are persisted.
        let ext = dummy_record("0xbbbb", "extractable", "500000000000000000");
        let prot = dummy_record("0xcccc", "sender_protected", "0");
        assert!(store.ingest_record(ext).await);
        assert!(store.ingest_record(prot).await);

        let on_disk = fs::read_to_string(&log_path).unwrap();
        assert!(on_disk.contains("0xbbbb"));
        assert!(on_disk.contains("0xcccc"));
        assert!(!on_disk.contains("0xaaaa"));

        // 3. Once 0xbbbb closes on-chain, it is completely removed from hits.jsonl and blocked from re-entry.
        let mut closed = HashSet::new();
        closed.insert("0xbbbb".to_string());
        let removed = store.prune_closed_hashes(&closed).await;
        assert_eq!(removed, 1);

        let after_prune = fs::read_to_string(&log_path).unwrap();
        assert!(!after_prune.contains("0xbbbb"));
        assert!(after_prune.contains("0xcccc"));

        // Re-gossip of already-closed 0xbbbb is ignored.
        assert!(!store.ingest_record(dummy_record("0xbbbb", "extractable", "500000000000000000")).await);

        let meta = store.meta().await;
        assert_eq!(meta.total_observed, 4);
        assert_eq!(meta.pruned_locked_to_user, 1);
        assert_eq!(meta.pruned_closed_onchain, 1);
        assert_eq!(meta.active_count, 1);

        let _ = fs::remove_dir_all(&dir);
    }
}
