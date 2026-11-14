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
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use url::Url;

mod broadcast;
mod sign;
mod simulate;

pub use broadcast::broadcast_and_confirm;
pub use sign::load_signer_from_env;
pub use simulate::simulate_liquidation;
pub use simulate::simulate_with_backend;
pub use simulate::{simulate_raw_call, RawSimulation};

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
    /// Blocks to wait for confirmation before returning `Confirmed`.
    pub confirmations: u64,
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
    pub tx_hash: Option<B256>,
    pub outcome: String,
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

    // Stage 2: sign (key material never leaves `sign.rs`).
    let encoded = sign_transaction(signer, opportunity, cfg.chain_id).await?;
    let tx_hash = alloy::primitives::keccak256(&encoded);

    // Stage 3: dry-run — sign only, never broadcast.
    if cfg.dry_run {
        append_audit(cfg, opportunity, Some(tx_hash), "dry_run_signed", &sim)?;
        return Ok(ExecOutcome::Simulated {
            profit: opportunity.gross_profit_wei,
        });
    }

    // Stage 4: broadcast via private relay only.
    let outcome = broadcast_and_confirm(
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
            ExecOutcome::Confirmed { .. } => "confirmed",
            ExecOutcome::Reverted { .. } => "reverted",
            ExecOutcome::Broadcast { .. } => "broadcast",
            _ => "skipped",
        },
        &sim,
    )?;

    Ok(outcome)
}

/// Build and RLP-encode (EIP-2718) the signed transaction envelope for the
/// opportunity. The signer's private key never leaves `sign.rs`.
async fn sign_transaction(
    signer: &LocalSigner,
    opportunity: &LiquidationOpportunity,
    chain_id: u64,
) -> Result<Vec<u8>> {
    use alloy::consensus::TxEip1559;

    // The signer is left unbound so `sign_transaction_with_chain_id` uses the
    // transaction's own chain ID (set below) for EIP-155 replay protection.
    let mut tx = TxEip1559 {
        chain_id,
        nonce: opportunity.nonce,
        gas_limit: opportunity.gas_limit,
        to: TxKind::Call(opportunity.to),
        value: opportunity.value,
        input: opportunity.calldata.clone(),
        max_fee_per_gas: 0,
        max_priority_fee_per_gas: 0,
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
    sim: &ExecOutcome,
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
        profit_estimate_wei: match sim {
            ExecOutcome::Simulated { profit } => *profit,
            _ => opportunity.gross_profit_wei,
        },
        tx_hash,
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
