//! Stage 1 & Orchestration: Alloy WebSocket/IPC pending-transaction ingestion
//! (full-tx stream with automatic hash-stream fallback & reconnection),
//! bounded worker pool, and rotating JSONL persistence.

use crate::abi::{decode_calldata, match_calldata, REGISTRY};
use crate::config::Settings;
use crate::sim::{evaluate_transaction, HitRecord, ObservedTx, SimulationParams};
use crate::state::{one_line_str, ForkMonitor, RpcStateBackend, StateBackend};
use alloy::consensus::Transaction as ConsensusTx;
use alloy::network::TransactionResponse;
use alloy::primitives::B256;
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::Transaction as RpcTransaction;
use anyhow::Result;
use chrono::{SecondsFormat, Utc};
use futures_util::StreamExt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch, Semaphore};
use tracing::{debug, error, info, warn};

const MAX_LOG_BYTES: u64 = 50 * 1024 * 1024; // 50 MB rotation threshold

pub async fn run_scanner(settings: Settings) -> Result<()> {
    let state_provider = ProviderBuilder::new()
        .connect(&settings.state_rpc)
        .await?
        .erased();

    let pubsub_endpoint = settings
        .ipc_rpc
        .clone()
        .unwrap_or_else(|| settings.ws_rpc.clone());

    let initial_ws = ProviderBuilder::new()
        .connect(&pubsub_endpoint)
        .await?
        .erased();

    let monitor = Arc::new(ForkMonitor::new(
        state_provider.clone(),
        Some(initial_ws.clone()),
        settings.poll_seconds,
    ));
    monitor.refresh().await;

    let backend: Arc<dyn StateBackend> = Arc::new(RpcStateBackend::new(state_provider.clone()));

    let (record_tx, record_rx) = mpsc::channel::<HitRecord>(4_096);
    let log_path = settings.log_path.clone();
    let writer_handle = tokio::spawn(async move {
        jsonl_writer_loop(log_path, record_rx).await;
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
    let _ = writer_handle.await;
    info!("Shutdown complete");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn ingestion_supervisor(
    endpoint: String,
    mut current_ws: DynProvider,
    http_fallback: DynProvider,
    monitor: Arc<ForkMonitor>,
    backend: Arc<dyn StateBackend>,
    record_tx: mpsc::Sender<HitRecord>,
    settings: Settings,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let sim_sem = Arc::new(Semaphore::new(settings.worker_concurrency));
    let fetch_sem = Arc::new(Semaphore::new(settings.worker_concurrency * 2));
    let sim_params = Arc::new(SimulationParams {
        max_lag: settings.max_lag,
        depth: settings.depth,
        timeout: Duration::from_millis(settings.sim_timeout_ms),
    });

    let mut attempt: u32 = 0;
    loop {
        if *shutdown_rx.borrow() {
            return Ok(());
        }

        if attempt > 0 {
            let delay = Duration::from_secs((2u64.pow(attempt.min(4))).min(15));
            warn!("Reconnecting to pending stream in {:?}...", delay);
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = shutdown_rx.changed() => return Ok(()),
            }
            match ProviderBuilder::new().connect(&endpoint).await {
                Ok(p) => {
                    current_ws = p.erased();
                }
                Err(err) => {
                    warn!("WebSocket reconnect failed: {}", one_line_str(&err.to_string()));
                    attempt = attempt.saturating_add(1);
                    continue;
                }
            }
        }

        // Prefer full pending transactions (`eth_subscribe("newPendingTransactions", true)`)
        // for 0-hop ingestion; fall back automatically to hash stream + concurrent fetch.
        match current_ws.subscribe_full_pending_transactions().await {
            Ok(sub) => {
                attempt = 1;
                info!("Subscribed to full pending transaction stream (0-hop mode)");
                let mut stream = sub.into_stream();
                loop {
                    tokio::select! {
                        maybe_tx = stream.next() => {
                            let Some(tx) = maybe_tx else {
                                warn!("Full pending transaction stream closed");
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
                            );
                        }
                        _ = shutdown_rx.changed() => return Ok(()),
                    }
                }
            }
            Err(full_err) => {
                debug!(
                    "Full pending subscription unsupported ({}); using hash subscription",
                    one_line_str(&full_err.to_string())
                );
                match current_ws.subscribe_pending_transactions().await {
                    Ok(sub) => {
                        attempt = 1;
                        info!("Subscribed to pending transaction hashes (concurrent fetch mode)");
                        let mut stream = sub.into_stream();
                        loop {
                            tokio::select! {
                                maybe_hash = stream.next() => {
                                    let Some(tx_hash) = maybe_hash else {
                                        warn!("Pending hash stream closed");
                                        break;
                                    };
                                    dispatch_hash_tx(
                                        tx_hash,
                                        current_ws.clone(),
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
                        attempt = attempt.saturating_add(1);
                    }
                }
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
    // Tail-drop if all fetch slots are saturated so workers always inspect fresh txs.
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
) {
    let Some(to_addr) = ConsensusTx::to(&tx.inner) else {
        return;
    };
    let input = ConsensusTx::input(&tx.inner);
    if input.len() < 4 {
        return;
    }

    let decode_t0 = Instant::now();
    let Some(spec) = match_calldata(input) else {
        return;
    };
    let value = ConsensusTx::value(&tx.inner);
    let decoded = decode_calldata(spec, input, value);
    let decode_us = decode_t0.elapsed().as_micros() as u64;

    let Ok(sim_permit) = sim_sem.clone().try_acquire_owned() else {
        return;
    };

    let from_addr = TransactionResponse::from(&tx);
    let tx_hash = TransactionResponse::tx_hash(&tx);
    let gas_limit = ConsensusTx::gas_limit(&tx.inner);
    let tip_wei = ConsensusTx::max_priority_fee_per_gas(&tx.inner);
    let max_fee_wei = Some(ConsensusTx::max_fee_per_gas(&tx.inner));
    let calldata = input.clone();
    let ts_iso = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);

    let observed = ObservedTx {
        hash: format!("{tx_hash:#x}"),
        from: from_addr,
        to: to_addr,
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

        if let Ok(line) = serde_json::to_string(&record) {
            info!("hit {line}");
        }
        let _ = record_tx.send(record).await;
    });
}

async fn jsonl_writer_loop(path: PathBuf, mut rx: mpsc::Receiver<HitRecord>) {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }

    let mut writes_since_check: u32 = 0;
    while let Some(record) = rx.recv().await {
        let Ok(serialized) = serde_json::to_string(&record) else {
            continue;
        };
        let path_clone = path.clone();
        let check_rotate = writes_since_check >= 250;
        if check_rotate {
            writes_since_check = 0;
        } else {
            writes_since_check += 1;
        }

        let _ = tokio::task::spawn_blocking(move || {
            if check_rotate {
                rotate_if_oversized(&path_clone);
            }
            if let Some(parent) = path_clone.parent() {
                if !parent.as_os_str().is_empty() {
                    let _ = fs::create_dir_all(parent);
                }
            }
            if let Ok(mut file) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path_clone)
            {
                let _ = writeln!(file, "{serialized}");
            }
        })
        .await;
    }
}

fn rotate_if_oversized(path: &Path) {
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    if meta.len() < MAX_LOG_BYTES {
        return;
    }
    if let Ok(contents) = fs::read_to_string(path) {
        let lines: Vec<&str> = contents.lines().collect();
        let keep_from = lines.len().saturating_sub(2_000);
        let tail = lines[keep_from..].join("\n") + "\n";
        let backup = path.with_extension("jsonl.1");
        let _ = fs::rename(path, backup);
        let _ = fs::write(path, tail);
    }
}
