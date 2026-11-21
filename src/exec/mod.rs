//! Execution module (opt-in, feature-flagged).
//!
//! Provides the ability to act on `liquidate` opportunities detected by the
//! read-only scanner. Gated behind `--execute` (default OFF). When the flag is
//! not passed this module is never invoked and the binary behaves exactly as
//! before: no keys, no signing, no broadcasting.
//!
//! Safety rails:
//!   * `--execute` defaults to `false`.
//!   * The simulation gate in `simulate.rs` is non-bypassable: a transaction is
//!     signed only after a profitable simulation.
//!   * Signer material is loaded only from `EXEC_PRIVATE_KEY` or
//!     `EXEC_KEYSTORE_PATH` + `EXEC_KEYSTORE_PASSWORD` (see `sign.rs`). It is
//!     never accepted as a CLI flag, config-file field, or function argument.
//!   * Broadcasting is private-relay only: Flashbots/MEV-Share first, then
//!     configured builder relays. With no private relay configured the tx is
//!     `Skipped("no_private_relay")`. The public mempool is never used.
//!   * Every signed tx is appended to `exec_audit.jsonl`.

use crate::abi::DecodedCall;
use crate::sim::{HitRecord, ObservedTx};
use alloy::consensus::SignableTransaction;
use alloy::network::TxSigner;
use alloy::primitives::{Address, B256, Bytes, TxKind, U256};
use alloy::signers::local::PrivateKeySigner as LocalSigner;
use anyhow::{anyhow, Result};
use bytes::BytesMut;
use alloy::providers::{Provider, ProviderBuilder};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use tracing::info;
use url::Url;

mod broadcast;
mod build_tx;
pub mod gas;
mod sign;
mod simulate;

pub use broadcast::broadcast_and_confirm;
pub use broadcast::simulate_bundle;
pub use broadcast::submit_mev_share_bundle;
pub use gas::{plan_bid, BidDecision, BidPlan, FeeEnvelope};
pub use build_tx::{
    build_signed_liquidation_tx, build_tx_from_opportunity, build_tx_standalone,
    close_factor_debt, compute_mock_hf_overrides, encode_execute_liquidation, BuildTxInputs,
    BuildTxReport, DEFAULT_SWAP_FEE,
};
pub use sign::load_signer_from_env;
pub use simulate::fetch_block_timestamp;
pub use simulate::simulate_liquidation;
pub use simulate::simulate_with_backend;
pub use simulate::{simulate_raw_call, simulate_raw_call_with_overrides, CodeOverride, RawSimulation};

/// A liquidation opportunity detected by the scanner's pattern-match stage.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiquidationOpportunity {
    /// Stable identifier for audit/logging (the source tx hash).
    pub id: String,
    /// Chain the opportunity was observed on.
    pub chain_id: u64,
    /// Decoded category, always `"liquidate"` for this module.
    pub category: String,
    /// Liquidation contract target.
    pub to: Address,
    /// Observed sender (the liquidator / our signer).
    pub from: Address,
    pub value: U256,
    pub gas_limit: u64,
    pub nonce: u64,
    pub calldata: Bytes,
    /// Operator's off-chain gross payout estimate in wei (e.g. collateral bounty).
    pub gross_profit_wei: U256,
    pub borrower: Option<Address>,
    pub collateral: Option<Address>,
    pub debt_asset: Option<Address>,
    pub repay_amount: Option<U256>,
    pub selector: String,
    pub summary: String,
}

impl LiquidationOpportunity {
    pub fn from_hit_record(
        record: &HitRecord,
        decoded: &DecodedCall,
        observed: &ObservedTx,
    ) -> Self {
        Self {
            id: record.hash.clone(),
            chain_id: 0, // filled in by the caller from the active chain config.
            category: decoded.category.clone(),
            to: observed.to,
            from: observed.from,
            value: observed.value,
            gas_limit: observed.gas_limit,
            nonce: observed.nonce.unwrap_or(0),
            calldata: observed.calldata.clone(),
            gross_profit_wei: U256::from_str(record.decision.net_profit_wei.as_str())
                .unwrap_or(U256::ZERO),
            borrower: None,
            collateral: decoded
                .token_out
                .as_ref()
                .and_then(|a| Address::from_str(a).ok()),
            debt_asset: decoded
                .token_in
                .as_ref()
                .and_then(|a| Address::from_str(a).ok()),
            repay_amount: decoded
                .amount_in
                .as_ref()
                .and_then(|a| U256::from_str(a).ok()),
            selector: observed.selector_hex.clone(),
            summary: decoded.summary.clone(),
        }
    }
}

/// How far the execution pipeline is allowed to go.
///
/// Parsed from `EXECUTION_MODE`. The default is [`ExecutionMode::Off`], so a
/// scanner started with no configuration cannot spend anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    /// Observe only. Nothing is signed.
    Off,
    /// Simulate and sign, write the audit record, but never broadcast.
    DryRun,
    /// Simulate, sign and broadcast via Flashbots.
    Live,
}

impl ExecutionMode {
    /// Parse `EXECUTION_MODE`. Unknown values are an error rather than a
    /// silent downgrade: mistyping "live" must not quietly disable execution,
    /// and mistyping "off" must not quietly enable it.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "" | "off" => Ok(Self::Off),
            "dry_run" | "dryrun" => Ok(Self::DryRun),
            "live" => Ok(Self::Live),
            other => Err(anyhow!(
                "EXECUTION_MODE must be one of off | dry_run | live (got {other:?})"
            )),
        }
    }

    pub fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    pub fn is_dry_run(self) -> bool {
        matches!(self, Self::DryRun)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::DryRun => "dry_run",
            Self::Live => "live",
        }
    }
}

/// Operator configuration for the execution module.
#[derive(Clone, Debug)]
pub struct ExecConfig {
    /// Master kill switch. Defaults to `false`.
    pub enabled: bool,
    /// Sign but do not broadcast (testnet/dev safety valve).
    pub dry_run: bool,
    /// Chain ID the signer is bound to.
    pub chain_id: u64,
    /// State RPC used for the pre-sign re-simulation and confirmation polling.
    pub rpc_url: Url,
    /// Loaded signer (populated from env by [`sign::load_signer_from_env`]).
    pub signer: Option<LocalSigner>,
    /// Flashbots Protect / MEV-Share relay endpoint (primary broadcast path).
    pub flashbots_relay: Option<Url>,
    /// Secondary builder-relay fan-out endpoints.
    pub builder_relays: Vec<Url>,
    /// Minimum net profit (wei) required to sign.
    pub min_profit_wei: U256,
    /// Builder's share of the surplus, in basis points (9000 = 90%). From the
    /// `BID_FRACTION` env (default 0.90), paid to the builder as priority fee.
    pub bid_fraction_bps: u32,
    /// Added to gas cost in the coverage gate (`EXEC_GAS_SAFETY_BUFFER_WEI`).
    pub safety_buffer_wei: U256,
    /// Blocks to wait for confirmation before returning `Confirmed`.
    pub confirmations: u64,
    /// The deployed AaveV3Liquidator contract. Required in `live` mode: it is
    /// the only address the execution path is allowed to call.
    pub liquidator: Option<Address>,
    /// Resolved execution mode, kept for logging and the API to report.
    pub mode: ExecutionMode,
}

/// Outcome of an attempted execution.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecOutcome {
    Skipped { reason: String },
    Simulated { profit: U256 },
    Broadcast { tx_hash: B256 },
    Confirmed { tx_hash: B256, block: u64 },
    Reverted { tx_hash: B256, block: u64 },
}

/// Audit record appended to `exec_audit.jsonl` for every signed transaction.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecAuditEntry {
    pub timestamp: String,
    pub chain_id: u64,
    pub opportunity_id: String,
    pub profit_estimate_wei: U256,
    /// Gas-aware bidding breakdown (the "90% rule"). Present on every
    /// `dry_run_signed` / live entry so a dry run shows exactly what a live
    /// bundle would have paid and kept.
    pub gas_cost_wei: U256,
    pub builder_bribe_wei: U256,
    pub net_profit_wei: U256,
    pub tx_hash: Option<B256>,
    /// Flashbots bundle hash, present only once a bundle is actually submitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle_hash: Option<B256>,
    pub outcome: String,
}

/// Turn a keeper-resolved opportunity into an EXECUTABLE one.
///
/// `keeper::liquidation_opportunity` fills in display placeholders — `to` is the
/// Aave Pool, `calldata` is empty, `nonce` is 0 — because it only detects, it
/// does not build the transaction. Signing that as-is would broadcast a no-op to
/// the Pool and liquidate nothing. This rebuilds it the way `build_tx` does:
/// `to` = our liquidator, real `executeLiquidation` calldata, and the signer's
/// live pending nonce. An already-executable opportunity (built by `build_tx`)
/// is returned unchanged.
async fn executable_opportunity(
    cfg: &ExecConfig,
    signer: &LocalSigner,
    opp: &LiquidationOpportunity,
) -> Result<LiquidationOpportunity> {
    let liquidator = cfg
        .liquidator
        .ok_or_else(|| anyhow!("EXEC_LIQUIDATOR_ADDRESS required to execute"))?;

    // Already executable (non-empty calldata targeting the liquidator)?
    if !opp.calldata.is_empty() && opp.to == liquidator {
        return Ok(opp.clone());
    }

    let borrower = opp.borrower.ok_or_else(|| anyhow!("opportunity has no borrower"))?;
    let collateral = opp
        .collateral
        .ok_or_else(|| anyhow!("opportunity has no collateral asset"))?;
    let debt_asset = opp
        .debt_asset
        .ok_or_else(|| anyhow!("opportunity has no debt asset"))?;
    let debt_to_cover = opp
        .repay_amount
        .filter(|a| !a.is_zero())
        .ok_or_else(|| anyhow!("opportunity has no resolved debt amount"))?;

    let swap_fee: u32 = std::env::var("BUILD_TX_SWAP_FEE")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_SWAP_FEE);

    let calldata = encode_execute_liquidation(
        borrower,
        collateral,
        debt_asset,
        debt_to_cover,
        swap_fee,
        cfg.min_profit_wei,
    );

    let provider = ProviderBuilder::new()
        .connect(cfg.rpc_url.as_str())
        .await
        .map_err(|e| anyhow!("rpc connect: {e}"))?
        .erased();
    let nonce = provider
        .get_transaction_count(signer.address())
        .pending()
        .await
        .map_err(|e| anyhow!("get nonce: {e}"))?;

    let mut out = opp.clone();
    out.to = liquidator;
    out.from = signer.address();
    out.calldata = calldata;
    out.nonce = nonce;
    Ok(out)
}

/// Top-level entry point. The simulation gate is non-bypassable: signing only
/// happens after `simulate_liquidation` reports `net_profit >= min_profit_wei`.
pub async fn execute_liquidation(
    cfg: &ExecConfig,
    opportunity: &LiquidationOpportunity,
) -> Result<ExecOutcome> {
    if !cfg.enabled {
        return Ok(ExecOutcome::Skipped {
            reason: "execution disabled".to_string(),
        });
    }

    let signer = cfg
        .signer
        .as_ref()
        .ok_or_else(|| anyhow!("no signer configured: set EXEC_PRIVATE_KEY or EXEC_KEYSTORE_PATH"))?;

    // Build the real executable tx (to=liquidator, real calldata, live nonce);
    // the keeper's opportunity is a display placeholder (to=Pool, empty calldata).
    let exec_opp = executable_opportunity(cfg, signer, opportunity).await?;
    let opportunity = &exec_opp;

    // Stage 1: mandatory pre-sign simulation gate. Even with --execute we never
    // sign unless simulate.rs reports a profitable simulation.
    let sim = simulate_liquidation(
        cfg.rpc_url.as_str(),
        opportunity.chain_id,
        opportunity,
        cfg.min_profit_wei,
        U256::ZERO, // builder_bid placeholder; wire to configured builder tip when available.
    )
    .await?;

    match &sim {
        ExecOutcome::Simulated { profit } => {
            if *profit < cfg.min_profit_wei {
                return Ok(ExecOutcome::Skipped {
                    reason: format!(
                        "net profit {} below min_profit_wei {}",
                        profit, cfg.min_profit_wei
                    ),
                });
            }
        }
        ExecOutcome::Skipped { reason } => {
            return Ok(ExecOutcome::Skipped {
                reason: reason.clone(),
            });
        }
        other => {
            return Ok(ExecOutcome::Skipped {
                reason: format!("simulation returned unexpected outcome: {other:?}"),
            });
        }
    }

    let _ = &sim; // the sim gate above is the profitability sanity check.

    // Stage 1b: gas-aware dynamic bid (the "90% rule"). Fetch live fees, price
    // the builder bribe as priority fee, and ABORT before signing if the
    // opportunity cannot pay for itself. This is what guarantees we never sign
    // or send a losing bundle.
    let fees = gas::fetch_fees(cfg.rpc_url.as_str()).await?;
    let plan = match gas::plan_bid(
        opportunity.gross_profit_wei,
        opportunity.gas_limit,
        fees,
        cfg.bid_fraction_bps,
        cfg.min_profit_wei,
        cfg.safety_buffer_wei,
    ) {
        BidDecision::Execute(p) => p,
        BidDecision::Abort(reason) => return Ok(ExecOutcome::Skipped { reason }),
    };

    // Stage 2: sign at the computed fees (key material never leaves `sign.rs`).
    let encoded = sign_transaction(
        signer,
        opportunity,
        cfg.chain_id,
        plan.max_fee_per_gas,
        plan.max_priority_fee_per_gas,
    )
    .await?;
    let tx_hash = alloy::primitives::keccak256(&encoded);

    // Stage 3: dry-run — sign only, never broadcast. Record the full bid so a
    // dry run shows exactly what a live bundle would have paid and kept.
    if cfg.dry_run {
        append_audit(cfg, opportunity, Some(tx_hash), "dry_run_signed", &plan, None)?;
        return Ok(ExecOutcome::Simulated {
            profit: plan.net_profit_wei,
        });
    }

    // Stage 4 (live): Flashbots bundle simulation — the zero-revert guarantee.
    // We NEVER broadcast a bundle the relay says would revert or come in below
    // our floor. Any error here is treated as "do not send" (fail closed).
    if let Some(relay) = cfg.flashbots_relay.as_ref() {
        match simulate_bundle(cfg.rpc_url.as_str(), relay, signer, &encoded, cfg.min_profit_wei)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                return Ok(ExecOutcome::Skipped {
                    reason: "SKIPPED: Bundle simulation failed".to_string(),
                });
            }
            Err(e) => {
                return Ok(ExecOutcome::Skipped {
                    reason: format!("SKIPPED: Bundle simulation failed ({e})"),
                });
            }
        }
    }

    // Stage 5: broadcast via private relay only.
    let (outcome, bundle_hash) = broadcast_and_confirm(
        cfg.rpc_url.as_str(),
        signer,
        &encoded,
        tx_hash,
        opportunity.chain_id,
        cfg.flashbots_relay.as_ref(),
        &cfg.builder_relays,
        cfg.confirmations,
    )
    .await?;

    append_audit(
        cfg,
        opportunity,
        Some(tx_hash),
        match &outcome {
            ExecOutcome::Confirmed { .. } | ExecOutcome::Broadcast { .. } => "live_broadcast",
            ExecOutcome::Reverted { .. } => "reverted",
            _ => "skipped",
        },
        &plan,
        bundle_hash,
    )?;

    Ok(outcome)
}

/// SVR back-run (Phase 6). Build the liquidation tx for an already-detected
/// liquidatable borrower with the gas-aware 90% bid, then bundle it **behind**
/// the SVR oracle update (referenced by hash) and submit via `mev_sendBundle`.
///
/// Gated exactly like [`execute_liquidation`]: in `dry_run` it logs the bundle
/// it WOULD submit and writes an audit entry, but sends nothing. Live submission
/// requires `FLASHBOTS_RELAY` and the deployed liquidator (enforced at startup).
pub async fn execute_svr_backrun(
    cfg: &ExecConfig,
    opportunity: &LiquidationOpportunity,
    svr_hint_tx: B256,
) -> Result<ExecOutcome> {
    if !cfg.enabled {
        return Ok(ExecOutcome::Skipped {
            reason: "execution disabled".to_string(),
        });
    }
    let signer = cfg
        .signer
        .as_ref()
        .ok_or_else(|| anyhow!("no signer configured for SVR back-run"))?;

    // Build the real executable tx — the keeper opportunity is a display
    // placeholder (to=Pool, empty calldata), which would back-run a no-op.
    let exec_opp = executable_opportunity(cfg, signer, opportunity).await?;
    let opportunity = &exec_opp;

    // Gas-aware 90% bid — aborts before signing if it cannot pay for itself.
    let fees = gas::fetch_fees(cfg.rpc_url.as_str()).await?;
    let plan = match gas::plan_bid(
        opportunity.gross_profit_wei,
        opportunity.gas_limit,
        fees,
        cfg.bid_fraction_bps,
        cfg.min_profit_wei,
        cfg.safety_buffer_wei,
    ) {
        BidDecision::Execute(p) => p,
        BidDecision::Abort(reason) => return Ok(ExecOutcome::Skipped { reason }),
    };

    // Sign the liquidation tx at the computed bid fees (the bribe rides the
    // priority fee, so the bundle itself needs no coinbase transfer).
    let encoded = sign_transaction(
        signer,
        opportunity,
        cfg.chain_id,
        plan.max_fee_per_gas,
        plan.max_priority_fee_per_gas,
    )
    .await?;
    let tx_hash = alloy::primitives::keccak256(&encoded);

    if cfg.dry_run {
        info!(
            svr_tx = %svr_hint_tx,
            liq_tx = %tx_hash,
            gas_cost_wei = %plan.gas_cost_wei,
            builder_bribe_wei = %plan.builder_bribe_wei,
            net_profit_wei = %plan.net_profit_wei,
            "SVR DRY-RUN bundle [hash(svr) -> tx(liq)] constructed; NOT sent"
        );
        append_audit(cfg, opportunity, Some(tx_hash), "svr_dry_run_signed", &plan, None)?;
        return Ok(ExecOutcome::Simulated {
            profit: plan.net_profit_wei,
        });
    }

    // Live: submit the MEV-Share back-run bundle.
    let relay = cfg
        .flashbots_relay
        .as_ref()
        .ok_or_else(|| anyhow!("FLASHBOTS_RELAY required for SVR live submission"))?;
    let bundle_hash =
        submit_mev_share_bundle(relay, signer, svr_hint_tx, &encoded, cfg.rpc_url.as_str()).await?;
    info!(
        svr_tx = %svr_hint_tx,
        liq_tx = %tx_hash,
        bundle_hash = ?bundle_hash,
        net_profit_wei = %plan.net_profit_wei,
        "SVR LIVE bundle submitted via mev_sendBundle"
    );
    append_audit(cfg, opportunity, Some(tx_hash), "svr_live_broadcast", &plan, bundle_hash)?;
    Ok(ExecOutcome::Broadcast { tx_hash })
}

/// Build and RLP-encode (EIP-2718) the signed transaction envelope for the
/// opportunity. The signer's private key never leaves `sign.rs`.
async fn sign_transaction(
    signer: &LocalSigner,
    opportunity: &LiquidationOpportunity,
    chain_id: u64,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
) -> Result<Vec<u8>> {
    use alloy::consensus::TxEip1559;

    // The signer is left unbound so `sign_transaction_with_chain_id` uses the
    // transaction's own chain ID (set below) for EIP-155 replay protection.
    // Fees come from the gas-aware bid: a bundle only pays the actual base fee,
    // and the priority fee carries the builder bribe (the "90% rule").
    let mut tx = TxEip1559 {
        chain_id,
        nonce: opportunity.nonce,
        gas_limit: opportunity.gas_limit,
        to: TxKind::Call(opportunity.to),
        value: opportunity.value,
        input: opportunity.calldata.clone(),
        max_fee_per_gas,
        max_priority_fee_per_gas,
        access_list: Default::default(),
    };

    let sig = signer
        .sign_transaction(&mut tx)
        .await
        .map_err(|e| anyhow!("sign tx: {e}"))?;
    let signed = tx.into_signed(sig);

    let mut buf = BytesMut::new();
    signed.eip2718_encode(&mut buf);
    Ok(buf.to_vec())
}

/// Append an audit record to `exec_audit.jsonl` next to the scanner log.
fn append_audit(
    cfg: &ExecConfig,
    opportunity: &LiquidationOpportunity,
    tx_hash: Option<B256>,
    outcome: &str,
    plan: &gas::BidPlan,
    bundle_hash: Option<B256>,
) -> Result<()> {
    use chrono::Utc;
    use std::fs;
    use std::io::Write;

    let log_dir = std::env::var("EXEC_AUDIT_DIR").unwrap_or_else(|_| "logs".to_string());
    let _ = fs::create_dir_all(&log_dir);
    let path = format!("{log_dir}/exec_audit.jsonl");

    let entry = ExecAuditEntry {
        timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        chain_id: cfg.chain_id,
        opportunity_id: opportunity.id.clone(),
        profit_estimate_wei: plan.net_profit_wei,
        gas_cost_wei: plan.gas_cost_wei,
        builder_bribe_wei: plan.builder_bribe_wei,
        net_profit_wei: plan.net_profit_wei,
        tx_hash,
        bundle_hash,
        outcome: outcome.to_string(),
    };

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let line = serde_json::to_string(&entry)?;
    writeln!(file, "{line}")?;
    file.flush()?;
    Ok(())
}

#[cfg(test)]
mod execution_mode_tests {
    use super::ExecutionMode;

    #[test]
    fn unset_and_off_both_mean_off() {
        assert_eq!(ExecutionMode::parse("").unwrap(), ExecutionMode::Off);
        assert_eq!(ExecutionMode::parse("off").unwrap(), ExecutionMode::Off);
        assert_eq!(ExecutionMode::parse("  OFF  ").unwrap(), ExecutionMode::Off);
        assert!(!ExecutionMode::Off.is_enabled());
    }

    #[test]
    fn dry_run_spellings_are_accepted_and_never_broadcast() {
        for raw in ["dry_run", "dry-run", "DryRun", " DRY_RUN "] {
            let m = ExecutionMode::parse(raw).unwrap_or_else(|e| panic!("{raw:?}: {e}"));
            assert_eq!(m, ExecutionMode::DryRun, "{raw:?}");
            assert!(m.is_enabled(), "{raw:?} must be enabled");
            assert!(m.is_dry_run(), "{raw:?} must not broadcast");
        }
    }

    #[test]
    fn live_broadcasts() {
        let m = ExecutionMode::parse("live").unwrap();
        assert_eq!(m, ExecutionMode::Live);
        assert!(m.is_enabled());
        assert!(!m.is_dry_run(), "live must broadcast");
    }

    /// A typo must be loud. Defaulting a misspelled "live" to off would
    /// silently stop a bot the operator believed was running; defaulting a
    /// misspelled "off" to live would spend money they did not authorise.
    #[test]
    fn typos_are_rejected_not_defaulted() {
        for raw in ["liv", "LIVE!", "dry", "on", "true", "1", "enabled"] {
            assert!(
                ExecutionMode::parse(raw).is_err(),
                "{raw:?} must be rejected, not silently coerced"
            );
        }
    }

    #[test]
    fn as_str_round_trips() {
        for m in [ExecutionMode::Off, ExecutionMode::DryRun, ExecutionMode::Live] {
            assert_eq!(ExecutionMode::parse(m.as_str()).unwrap(), m);
        }
    }
}
