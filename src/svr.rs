//! Phase 6 — Chainlink SVR (Smart Value Recapture) back-running via Flashbots
//! MEV-Share.
//!
//! On EC2 with ~60s polling we cannot win the public-mempool latency race. SVR
//! is the one realistic edge: Chainlink routes the SVR feed's price update as
//! private order-flow through Flashbots, and a searcher can **back-run** it in
//! the *same block* with a liquidation bundle. This module subscribes to the
//! MEV-Share hint stream, recognises an SVR oracle-update hint, and (in live
//! mode) submits a `[{hash: svr_update}, {tx: our_liquidation}]` bundle.
//!
//! ## What is verified vs. what needs live validation
//! * The [`decode_svr_forward`] decoder is unit-tested by a round-trip against a
//!   synthetically-encoded report. The **real** Chainlink `transmitSecondary`
//!   report layout must be confirmed against a live SVR transaction before this
//!   is trusted in live mode — there is no real fixture in the repo.
//! * The SVR feed allowlist is **operator-supplied** via `SVR_FEEDS`. Nothing is
//!   hardcoded: an unset/empty allowlist processes no feeds (safe default).
//!
//! ## Safety
//! The public mempool is never used. Submission is gated on `EXECUTION_MODE=live`
//! exactly like the rest of `exec`; a subscription failure degrades to a warning
//! and the normal dry-run keeper keeps running (never crashes the process).

use alloy::primitives::{Address, Bytes, I256, B256};
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use anyhow::{anyhow, bail, Result};
use futures_util::StreamExt;
use serde::Deserialize;
use std::collections::HashSet;
use std::time::Duration;
use tracing::{info, warn};

/// Selector of `forward(address,bytes)` — the SVR forwarder entrypoint. The
/// ticket specifies `0x6fadcf72`; [`forward_selector_matches_abi`] asserts this
/// equals the ABI-derived selector so a wrong constant can never pass silently.
pub const FORWARD_SELECTOR: [u8; 4] = [0x6f, 0xad, 0xcf, 0x72];

sol! {
    // Chainlink AuthorizedForwarder entrypoint.
    function forward(address to, bytes data);

    // OCR2 secondary transmit (the SVR path). We only need `report`.
    function transmitSecondary(
        bytes32[3] reportContext,
        bytes report,
        bytes32[] rs,
        bytes32[] ss,
        bytes32 rawVs
    );
}

/// A decoded SVR oracle update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SvrUpdate {
    /// The `to` target of `forward(address,bytes)` — the SVR feed/forwarder.
    pub feed: Address,
    /// Median observation from the OCR2 report (feed-native decimals). Price
    /// feeds are non-negative; a negative median is rejected upstream.
    pub median_price: I256,
    /// OCR2 report observations timestamp (unix seconds).
    pub observations_timestamp: u32,
    /// Number of observations in the report.
    pub observation_count: usize,
}

/// Decode an SVR `forward(address, transmitSecondary(...))` calldata blob into
/// the target feed and the median price.
///
/// Fails (rather than panics) on any shape mismatch, so a non-SVR `forward`
/// call, or a `forward` wrapping something other than `transmitSecondary`, is a
/// clean rejection.
pub fn decode_svr_forward(calldata: &[u8]) -> Result<SvrUpdate> {
    if calldata.len() < 4 || calldata[0..4] != FORWARD_SELECTOR {
        bail!("not a forward(address,bytes) call");
    }
    let fwd = forwardCall::abi_decode(calldata)
        .map_err(|e| anyhow!("decode forward(): {e}"))?;

    let inner = fwd.data.as_ref();
    if inner.len() < 4 || inner[0..4] != transmitSecondaryCall::SELECTOR {
        bail!("forward() does not wrap transmitSecondary()");
    }
    let ts = transmitSecondaryCall::abi_decode(inner)
        .map_err(|e| anyhow!("decode transmitSecondary(): {e}"))?;

    // OCR2 price-feed report:
    //   abi.encode(uint32 observationsTimestamp, bytes32 rawObservers,
    //              int192[] observations, int192 juelsPerFeeCoin)
    // Decoded with wider int256 lanes; the on-wire width is the 32-byte slot.
    let (observations_timestamp, _observers, observations, _juels): (
        u32,
        B256,
        Vec<I256>,
        I256,
    ) = SolValue::abi_decode_params(ts.report.as_ref())
        .map_err(|e| anyhow!("decode OCR2 report: {e}"))?;

    if observations.is_empty() {
        bail!("OCR2 report has no observations");
    }
    // OCR observations arrive sorted ascending; the median is the middle element.
    let median_price = observations[observations.len() / 2];

    Ok(SvrUpdate {
        feed: fwd.to,
        median_price,
        observations_timestamp,
        observation_count: observations.len(),
    })
}

/// Parse the `SVR_FEEDS` env var (comma-separated hex addresses) into an
/// allowlist. Empty/unset → empty set, which [`feed_is_allowed`] treats as
/// "process nothing" (safe default, never a wildcard).
pub fn svr_feeds_from_env() -> HashSet<Address> {
    std::env::var("SVR_FEEDS")
        .ok()
        .map(|v| {
            v.split(',')
                .filter_map(|s| s.trim().parse::<Address>().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Whether an SVR update targets a feed the operator opted into. An empty
/// allowlist allows nothing.
pub fn feed_is_allowed(feeds: &HashSet<Address>, feed: Address) -> bool {
    feeds.contains(&feed)
}

/// Compile-time-ish guard used by tests: the ticket's constant must equal the
/// ABI-derived selector of `forward(address,bytes)`.
pub fn forward_selector_matches_abi() -> bool {
    forwardCall::SELECTOR == FORWARD_SELECTOR
}

// ---------------------------------------------------------------------------
// MEV-Share subscription (#1) + fallback (#5)
// ---------------------------------------------------------------------------

/// Operator config for the SVR back-runner.
#[derive(Clone, Debug)]
pub struct SvrConfig {
    /// MEV-Share SSE endpoint (`MEV_SHARE_SSE_URL`).
    pub sse_url: String,
    /// Allowlist of SVR feed addresses (`SVR_FEEDS`).
    pub feeds: HashSet<Address>,
    /// `SVR_DEBUG_HINTS=1`: log a bounded sample of RAW hint frames so the real
    /// MEV-Share schema can be captured and validated. Observability only.
    pub debug_hints: bool,
}

/// How many raw hint frames to log when `debug_hints` is on, before going quiet
/// (prevents flooding the journal while still capturing a representative sample).
pub const DEBUG_HINT_SAMPLE: usize = 50;

impl SvrConfig {
    /// Build from env. Returns `None` (SVR disabled) when no feeds are
    /// configured, so the caller simply skips the subscription and keeps
    /// running the normal dry-run keeper — part of the fallback contract (#5).
    pub fn from_env() -> Option<Self> {
        let feeds = svr_feeds_from_env();
        if feeds.is_empty() {
            return None;
        }
        let sse_url = std::env::var("MEV_SHARE_SSE_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "https://mev-share.flashbots.net".to_string());
        let debug_hints = std::env::var("SVR_DEBUG_HINTS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        Some(Self { sse_url, feeds, debug_hints })
    }
}

/// A detected, feed-allowlisted SVR update ready for back-run evaluation.
#[derive(Clone, Debug)]
pub struct SvrCandidate {
    /// Hash of the SVR update transaction — referenced by hash as the first
    /// item of the back-run bundle (the relay fills in the real tx).
    pub hint_tx: B256,
    /// The decoded update (feed + new median price).
    pub update: SvrUpdate,
}

/// MEV-Share hint (partial by design; fields the originator chose to reveal).
#[derive(Debug, Deserialize)]
struct MevShareHint {
    hash: B256,
    #[serde(default)]
    txs: Vec<MevShareHintTx>,
}

#[derive(Debug, Deserialize)]
struct MevShareHintTx {
    #[serde(default, rename = "callData")]
    call_data: Option<Bytes>,
}

/// Run the MEV-Share SVR back-runner loop.
///
/// Fallback contract (#5): on any error or stream end it logs a warning and
/// reconnects after a short delay. It never returns an error to the caller and
/// never panics, so a failed/unavailable subscription degrades to "keep running
/// the normal dry-run keeper" rather than crashing the process. `on_candidate`
/// is invoked for each allowed SVR update — the caller performs the health-
/// factor check and (in live mode) the bundle submission.
pub async fn run_svr_backrunner<F, Fut>(cfg: SvrConfig, on_candidate: F)
where
    F: Fn(SvrCandidate) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    info!(
        "SVR back-runner starting (url={}, {} feed(s) allowlisted)",
        cfg.sse_url,
        cfg.feeds.len()
    );
    loop {
        match subscribe_once(&cfg, &on_candidate).await {
            Ok(()) => warn!("SVR MEV-Share stream ended; reconnecting in 5s"),
            Err(e) => warn!("SVR MEV-Share stream error: {e}; reconnecting in 5s"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn subscribe_once<F, Fut>(cfg: &SvrConfig, on_candidate: &F) -> Result<()>
where
    F: Fn(SvrCandidate) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| anyhow!("http client: {e}"))?;
    let resp = client
        .get(&cfg.sse_url)
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .send()
        .await
        .map_err(|e| anyhow!("connect: {e}"))?;
    if !resp.status().is_success() {
        bail!("mev-share returned HTTP {}", resp.status());
    }
    info!("SVR connected to MEV-Share at {}", cfg.sse_url);

    // SSE frames are separated by a blank line; each carries one `data:` JSON.
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut debug_logged = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| anyhow!("stream read: {e}"))?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(pos) = buf.find("\n\n") {
            let frame: String = buf.drain(..pos + 2).collect();
            // Discovery aid: log a bounded sample of raw hint JSON so the real
            // MEV-Share schema can be inspected. Pure observability.
            if cfg.debug_hints && debug_logged < DEBUG_HINT_SAMPLE {
                if let Some(data) = frame.lines().find_map(|l| l.strip_prefix("data:")) {
                    let data = data.trim();
                    if !data.is_empty() {
                        let shown: String = data.chars().take(2000).collect();
                        debug_logged += 1;
                        info!("SVR raw hint [{debug_logged}/{DEBUG_HINT_SAMPLE}]: {shown}");
                        if debug_logged == DEBUG_HINT_SAMPLE {
                            info!("SVR raw-hint sample complete; set SVR_DEBUG_HINTS=0 and restart to stop");
                        }
                    }
                }
            }
            if let Some(cand) = parse_hint_frame(&frame, &cfg.feeds) {
                on_candidate(cand).await;
            }
        }
        // Guard against an unbounded buffer if a peer never sends a frame break.
        if buf.len() > 1 << 20 {
            buf.clear();
        }
    }
    Ok(())
}

/// Parse one SSE frame into a candidate, if it is an allowlisted SVR update.
/// Returns `None` for anything else (keepalives, non-SVR hints, hints that do
/// not reveal calldata). Pure given the frame + allowlist, so it is unit-tested.
fn parse_hint_frame(frame: &str, feeds: &HashSet<Address>) -> Option<SvrCandidate> {
    let data = frame
        .lines()
        .find_map(|l| l.strip_prefix("data:"))
        .map(str::trim)?;
    let hint: MevShareHint = serde_json::from_str(data).ok()?;
    for tx in &hint.txs {
        // A hint must reveal calldata for us to decode the SVR update; skip the
        // ones that do not (do NOT early-return — a later tx may be the SVR one).
        let Some(cd) = tx.call_data.as_ref() else {
            continue;
        };
        if cd.len() < 4 || cd[0..4] != FORWARD_SELECTOR {
            continue;
        }
        if let Ok(update) = decode_svr_forward(cd) {
            if feed_is_allowed(feeds, update.feed) {
                return Some(SvrCandidate {
                    hint_tx: hint.hash,
                    update,
                });
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    /// Build a synthetic `forward(to, transmitSecondary(report))` blob with the
    /// given observations, exactly as the real forwarder would ABI-encode it.
    fn synth_forward(to: Address, ts: u32, observations: &[i64]) -> Vec<u8> {
        let obs: Vec<I256> = observations.iter().map(|v| I256::try_from(*v).unwrap()).collect();
        let report = (ts, B256::ZERO, obs, I256::ZERO).abi_encode_params();
        let inner = transmitSecondaryCall {
            reportContext: [B256::ZERO; 3],
            report: Bytes::from(report),
            rs: vec![],
            ss: vec![],
            rawVs: B256::ZERO,
        }
        .abi_encode();
        forwardCall { to, data: Bytes::from(inner) }.abi_encode()
    }

    #[test]
    fn ticket_selector_matches_abi() {
        // If this fails, 0x6fadcf72 is wrong for forward(address,bytes).
        assert!(
            forward_selector_matches_abi(),
            "forward() selector is {:02x?}, not 0x6fadcf72",
            forwardCall::SELECTOR
        );
    }

    #[test]
    fn decodes_feed_and_median() {
        let feed = address!("00000000000000000000000000000000DeaDBeef");
        // 5 observations, sorted; median is the middle (300).
        let blob = synth_forward(feed, 1_700_000_000, &[100, 200, 300, 400, 500]);
        let u = decode_svr_forward(&blob).expect("decode");
        assert_eq!(u.feed, feed);
        assert_eq!(u.median_price, I256::try_from(300).unwrap());
        assert_eq!(u.observations_timestamp, 1_700_000_000);
        assert_eq!(u.observation_count, 5);
    }

    #[test]
    fn rejects_non_forward() {
        assert!(decode_svr_forward(&[0u8; 4]).is_err());
        assert!(decode_svr_forward(&[]).is_err());
    }

    #[test]
    fn feed_allowlist_empty_allows_nothing() {
        let feeds = HashSet::new();
        assert!(!feed_is_allowed(&feeds, Address::ZERO));
    }

    #[test]
    fn feed_allowlist_matches() {
        let feed = address!("00000000000000000000000000000000DeaDBeef");
        let mut feeds = HashSet::new();
        feeds.insert(feed);
        assert!(feed_is_allowed(&feeds, feed));
        assert!(!feed_is_allowed(&feeds, Address::ZERO));
    }

    #[test]
    fn parse_hint_frame_detects_allowlisted_svr() {
        let feed = address!("00000000000000000000000000000000DeaDBeef");
        let blob = synth_forward(feed, 1_700_000_000, &[100, 200, 300]);
        let hexcd = alloy::hex::encode_prefixed(&blob);
        let frame = format!(
            "data: {{\"hash\":\"0x{:064x}\",\"txs\":[{{\"callData\":\"{hexcd}\"}}]}}\n\n",
            1u64
        );
        let mut feeds = HashSet::new();
        feeds.insert(feed);
        let cand = parse_hint_frame(&frame, &feeds).expect("candidate");
        assert_eq!(cand.update.feed, feed);
        assert_eq!(cand.update.median_price, I256::try_from(200).unwrap());
        // A feed NOT on the allowlist yields nothing.
        assert!(parse_hint_frame(&frame, &HashSet::new()).is_none());
    }

    #[test]
    fn parse_hint_frame_ignores_keepalive_and_junk() {
        let feeds = HashSet::new();
        assert!(parse_hint_frame(":keepalive\n\n", &feeds).is_none());
        assert!(parse_hint_frame("data: not-json\n\n", &feeds).is_none());
    }
}
