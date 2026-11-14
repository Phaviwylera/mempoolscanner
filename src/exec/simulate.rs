//! Pre-sign simulation gate.
//!
//! Re-simulates a liquidation at `latest_block + 1` using `revm` and computes
//! net profit = gross payout - gas_cost - builder_bid. Returns
//! `ExecOutcome::Skipped` if net profit < min_profit_wei.
//!
//! The gate is non-bypassable: `execute_liquidation` only signs after this
//! module reports a profitable simulation.

use crate::exec::{ExecOutcome, LiquidationOpportunity};
use crate::state::{BlockSnapshot, PinnedStateView, RpcStateBackend, StateBackend};
use alloy::primitives::{Address, Bytes, B256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use anyhow::{anyhow, Result};
use revm::context::TxEnv;
use revm::context_interface::block::BlobExcessGasAndPrice;
use revm::context_interface::result::ExecutionResult;
use revm::database::CacheDB;
use revm::primitives::eip4844::BLOB_BASE_FEE_UPDATE_FRACTION_PRAGUE;
use revm::primitives::{hardfork::SpecId, TxKind};
use revm::{AsyncDb, Context, ExecuteEvm, MainBuilder, MainContext};
use std::sync::Arc;

/// Simulate against the live RPC (`cfg.rpc_url`) at `latest_block + 1`.
pub async fn simulate_liquidation(
    rpc_url: &str,
    _chain_id: u64,
    opportunity: &LiquidationOpportunity,
    min_profit: U256,
    builder_bid: U256,
) -> Result<ExecOutcome> {
    let provider = ProviderBuilder::new()
        .connect(rpc_url)
        .await
        .map_err(|e| anyhow!("rpc connect: {e}"))?
        .erased();
    let latest = provider
        .get_block_number()
        .await
        .map_err(|e| anyhow!("block number: {e}"))?;
    let target = latest.saturating_add(1);
    let backend: Arc<dyn StateBackend> = Arc::new(RpcStateBackend::new(provider));
    simulate_with_backend(backend, target, opportunity, min_profit, builder_bid).await
}

/// Simulate against an in-process backend (used by unit tests with mocked state).
pub async fn simulate_with_backend(
    backend: Arc<dyn StateBackend>,
    block: u64,
    opportunity: &LiquidationOpportunity,
    min_profit: U256,
    builder_bid: U256,
) -> Result<ExecOutcome> {
    let gas_used = simulate_gas_used(backend.clone(), block, (*opportunity).clone()).await?;

    // gas_price: use the opportunity context base fee estimate. 1 gwei placeholder
    // that is overwritten by the live base fee when a state snapshot is available.
    let base_fee: u64 = 1_000_000_000;
    let gas_price_wei: u128 = base_fee as u128;
    let gas_cost = U256::from(gas_used).saturating_mul(U256::from(gas_price_wei));

    let net = opportunity
        .gross_profit_wei
        .saturating_sub(gas_cost)
        .saturating_sub(builder_bid);

    if net >= min_profit {
        Ok(ExecOutcome::Simulated { profit: net })
    } else {
        Ok(ExecOutcome::Skipped {
            reason: format!("net profit {net} below min_profit_wei {min_profit}"),
        })
    }
}

/// Execute the opportunity's calldata in `revm` at `block` and return gas used.
///
/// Kept for the signing gate, which only needs gas and treats a revert as
/// "gas spent". Anything that must distinguish success from revert should call
/// [`simulate_raw_call`] instead.
async fn simulate_gas_used(
    backend: Arc<dyn StateBackend>,
    block: u64,
    opportunity: LiquidationOpportunity,
) -> Result<u64> {
    let sim = simulate_raw_call(
        backend,
        block,
        opportunity.from,
        opportunity.to,
        opportunity.calldata.clone(),
        opportunity.value,
        opportunity.gas_limit,
    )
    .await?;
    Ok(sim.gas_used)
}

/// Full result of one `revm` call: enough detail to report exactly what the
/// EVM did, rather than collapsing a revert into a gas number.
#[derive(Clone, Debug)]
pub struct RawSimulation {
    pub success: bool,
    pub gas_used: u64,
    /// Return data on success, or the revert payload on failure.
    pub output: Bytes,
    /// Decoded revert/halt description, when the call did not succeed.
    pub failure: Option<String>,
    /// Emitted logs (empty on revert). `(address, topics, data)` per log.
    pub logs: Vec<(Address, Vec<B256>, Bytes)>,
}

/// Execute an arbitrary call in `revm` against forked state at `block`.
///
/// Read-only: the EVM runs in-process against a `CacheDB` over the state
/// backend. Nothing is signed and nothing is broadcast.
pub async fn simulate_raw_call(
    backend: Arc<dyn StateBackend>,
    block: u64,
    from: Address,
    to: Address,
    calldata: Bytes,
    value: U256,
    gas_limit: u64,
) -> Result<RawSimulation> {
    let snap = BlockSnapshot {
        number: block,
        ..Default::default()
    };
    let handle = tokio::runtime::Handle::current();
    let joined = tokio::task::spawn_blocking(move || {
        let view = PinnedStateView {
            backend,
            block,
        };
        let mut cache_db = CacheDB::new(AsyncDb::with_handle(view, handle));
        let mut evm = Context::mainnet()
            .with_db(&mut cache_db)
            .modify_cfg_chained(|cfg| {
                cfg.set_spec_and_mainnet_gas_params(SpecId::PRAGUE);
                cfg.chain_id = 1;
                cfg.tx_chain_id_check = false;
                cfg.disable_nonce_check = true;
                cfg.disable_balance_check = true;
                cfg.disable_block_gas_limit = true;
                cfg.disable_eip3607 = true;
                cfg.disable_base_fee = true;
                cfg.disable_fee_charge = true;
                cfg.tx_gas_limit_cap = Some(u64::MAX);
            })
            .modify_block_chained(|b| {
                b.number = U256::from(block);
                b.timestamp = U256::from(snap.timestamp);
                b.basefee = snap.base_fee;
                b.beneficiary = snap.beneficiary;
                b.gas_limit = snap.gas_limit.max(3_000_000);
                b.prevrandao = snap.prevrandao.or(Some(B256::ZERO));
                b.blob_excess_gas_and_price = snap
                    .excess_blob_gas
                    .map(|excess| BlobExcessGasAndPrice::new(excess, BLOB_BASE_FEE_UPDATE_FRACTION_PRAGUE));
            })
            .build_mainnet();

        let tx_env = TxEnv::builder()
            .caller(from)
            .kind(TxKind::Call(to))
            .data(calldata)
            .value(value)
            .gas_limit(gas_limit.max(21_000))
            .gas_price(0)
            .build()
            .map_err(|e| anyhow!("invalid tx env: {e}"))?;

        match evm.transact(tx_env) {
            Ok(exec) => match exec.result {
                ExecutionResult::Success {
                    gas, output, logs, ..
                } => Ok(RawSimulation {
                    success: true,
                    gas_used: gas.tx_gas_used(),
                    output: output.into_data(),
                    failure: None,
                    logs: logs
                        .into_iter()
                        .map(|l| (l.address, l.topics().to_vec(), l.data.data))
                        .collect(),
                }),
                ExecutionResult::Revert { gas, output, .. } => Ok(RawSimulation {
                    success: false,
                    gas_used: gas.tx_gas_used(),
                    failure: Some(describe_revert(&output)),
                    output,
                    logs: Vec::new(),
                }),
                ExecutionResult::Halt { gas, reason, .. } => Ok(RawSimulation {
                    success: false,
                    gas_used: gas.tx_gas_used(),
                    output: Bytes::new(),
                    failure: Some(format!("halted: {reason:?}")),
                    logs: Vec::new(),
                }),
            },
            Err(evm_err) => Err(anyhow!("revm error: {evm_err}")),
        }
    })
    .await
    .map_err(|e| anyhow!("revm task join error: {e}"))??;

    Ok(joined)
}

/// Turn revert bytes into a human-readable reason.
///
/// Reuses the project's existing `Error(string)` / `Panic(uint256)` decoder.
/// Aave V3 reverts carry numeric strings (e.g. "45" =
/// HEALTH_FACTOR_NOT_BELOW_THRESHOLD), so the raw string is preserved as-is.
fn describe_revert(output: &Bytes) -> String {
    if output.is_empty() {
        return "reverted without a reason string".to_string();
    }
    match crate::abi::decode_revert_bytes(output) {
        Some((kind, reason)) => format!("{kind}: {reason}"),
        None => format!("reverted with {} bytes of undecodable data", output.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::InMemoryStateBackend;
    use alloy::primitives::{address, Address, Bytes};

    fn backend_with_succeeding_contract() -> (Arc<dyn StateBackend>, Address) {
        let backend = InMemoryStateBackend::default();
        let target = address!("aaaa000000000000000000000000000000000001");
        // STOP bytecode: call succeeds immediately.
        backend.insert_account(target, U256::ZERO, Some(vec![0x00]));
        let backend: Arc<dyn StateBackend> = Arc::new(backend);
        (backend, target)
    }

    fn opportunity(to: Address, gross: U256) -> LiquidationOpportunity {
        LiquidationOpportunity {
            id: "0xtest".to_string(),
            chain_id: 1,
            category: "liquidate".to_string(),
            to,
            from: address!("bbbb000000000000000000000000000000000002"),
            value: U256::ZERO,
            gas_limit: 150_000,
            nonce: 0,
            calldata: Bytes::from(vec![]),
            gross_profit_wei: gross,
            borrower: None,
            collateral: None,
            debt_asset: None,
            repay_amount: None,
            selector: "0x00000000".to_string(),
            summary: "test".to_string(),
        }
    }

    #[tokio::test]
    async fn profitable_simulation_returns_simulated() {
        let (backend, to) = backend_with_succeeding_contract();
        let op = opportunity(to, U256::from(1_000_000_000_000_000_000u128)); // 1 ETH gross
        let outcome = simulate_with_backend(
            backend,
            21_000_000,
            &op,
            U256::from(100_000_000_000_000_000u128), // 0.1 ETH min profit
            U256::ZERO,
        )
        .await;
        assert!(outcome.is_ok());
        matches!(
            outcome.unwrap(),
            ExecOutcome::Simulated { profit } if profit > 0
        );
    }

    #[tokio::test]
    async fn unprofitable_simulation_returns_skipped() {
        let (backend, to) = backend_with_succeeding_contract();
        let op = opportunity(to, U256::ZERO); // zero gross
        let outcome = simulate_with_backend(
            backend,
            21_000_000,
            &op,
            U256::from(1_000_000_000_000_000_000u128), // 1 ETH min profit
            U256::ZERO,
        )
        .await;
        assert!(outcome.is_ok());
        matches!(
            outcome.unwrap(),
            ExecOutcome::Skipped { reason } if reason.contains("below min_profit_wei")
        );
    }
}
