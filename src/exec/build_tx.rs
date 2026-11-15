//! Manual, offline liquidation transaction builder.
//!
//! This module turns a detected opportunity into a **signed raw transaction
//! hex string** that the operator can inspect and broadcast themselves (e.g.
//! via Flashbots). It is deliberately separate from the autonomous execution
//! path in `mod.rs`:
//!
//!   * It NEVER broadcasts. There is no `eth_sendRawTransaction`, no relay
//!     call, nothing that touches the network beyond read-only RPC (nonce, gas,
//!     and the revm pre-flight simulation).
//!   * It signs exactly one transaction and returns the bytes. What happens to
//!     those bytes is the operator's decision, made outside this process.
//!
//! The target is the deployed `AaveV3Liquidator` contract's `executeLiquidation`
//! entrypoint, not the Aave pool directly.

use crate::exec::{
    simulate_raw_call_with_overrides, CodeOverride, LiquidationOpportunity,
};
use crate::state::{RpcStateBackend, StateBackend};
use alloy::consensus::{SignableTransaction, TxEip1559};
use alloy::network::TxSigner;
use alloy::primitives::{Address, Bytes, B256, TxKind, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner as LocalSigner;
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{anyhow, Result};
use bytes::BytesMut;
use std::sync::Arc;
use tracing::warn;

sol! {
    // Matches contracts/src/AaveV3Liquidator.sol exactly.
    interface IAaveV3Liquidator {
        function executeLiquidation(
            address user,
            address collateralAsset,
            address debtAsset,
            uint256 debtToCover,
            uint24 swapFee,
            uint256 minProfit
        ) external;
    }
}

/// Minimal EVM stub that returns `price` (as a 32-byte word) for ANY call —
/// enough to satisfy an Aave oracle source's `latestAnswer()`:
/// `PUSH32 price; PUSH1 0; MSTORE; PUSH1 32; PUSH1 0; RETURN`.
fn stub_price_bytecode(price: U256) -> Bytes {
    let mut code = Vec::with_capacity(40);
    code.push(0x7f); // PUSH32
    code.extend_from_slice(&price.to_be_bytes::<32>());
    code.extend_from_slice(&[0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3]);
    Bytes::from(code)
}

/// Build the fork-only oracle overrides that drop a borrower's health factor to
/// roughly `target_hf_wad`, so a not-yet-underwater position's liquidation
/// executes in the pre-flight. Returns the overrides plus a human note.
///
/// It overrides **every** collateral reserve's price source (not just the one
/// being seized): HF is the sum over all collaterals, so dropping a single
/// asset is not enough when a borrower has several. Each source price is scaled
/// by `(target/current)^2` — the square adds margin so the health factor lands
/// comfortably below 1.0 even when a collateral asset is *also* a debt asset
/// (crushing its price lowers both sides, muting the HF drop). Over-crushing is
/// safe: Aave caps the seizure at the borrower's actual balance and the
/// (still real-priced on Uniswap) seized collateral repays the flash loan.
///
/// This affects the SIMULATION ONLY — the signed transaction is unaffected.
pub async fn compute_mock_hf_overrides(
    keeper: &crate::keeper::Keeper,
    user: Address,
    target_hf_wad: U256,
) -> (Vec<CodeOverride>, Option<String>) {
    let Some(current_hf) = keeper.user_health_factor(user).await else {
        return (
            Vec::new(),
            Some("--mock-hf: could not read the live health factor; no override applied".into()),
        );
    };
    if current_hf <= target_hf_wad {
        return (
            Vec::new(),
            Some(format!(
                "--mock-hf: borrower HF {current_hf} already at/below target {target_hf_wad}; no override applied"
            )),
        );
    }

    let sources = keeper.collateral_price_sources(user).await;
    if sources.is_empty() {
        return (
            Vec::new(),
            Some("--mock-hf: no collateral price sources resolved; no override applied".into()),
        );
    }

    // factor_ppm = (target/current)^2, in parts-per-million, clamped to [0.1%, 95%].
    const PPM: u128 = 1_000_000;
    let ratio_ppm = (target_hf_wad.saturating_mul(U256::from(PPM)) / current_hf.max(U256::from(1u8)))
        .to::<u128>()
        .min(PPM);
    let mut factor_ppm = ratio_ppm.saturating_mul(ratio_ppm) / PPM;
    factor_ppm = factor_ppm.clamp(1_000, 950_000);

    let overrides: Vec<CodeOverride> = sources
        .iter()
        .map(|(source, price)| {
            let new_price = (price.saturating_mul(U256::from(factor_ppm)) / U256::from(PPM))
                .max(U256::from(1u8));
            CodeOverride {
                address: *source,
                code: stub_price_bytecode(new_price),
            }
        })
        .collect();

    let note = format!(
        "pre-flight only: {} collateral oracle source(s) scaled to {}.{:04}% of live price (HF {} → ~target {}). Signed tx unaffected.",
        overrides.len(),
        factor_ppm / 10_000,
        factor_ppm % 10_000,
        current_hf,
        target_hf_wad,
    );
    warn!("{note}");
    (overrides, Some(note))
}

/// Aave V3.1 close-factor threshold: below this health factor the whole debt
/// may be liquidated; at or above it, only 50%.
const CLOSE_FACTOR_HF_THRESHOLD_WAD: u128 = 950_000_000_000_000_000; // 0.95e18

/// Default Uniswap V3 fee tier (0.30%) used for the collateral→debt swap when
/// the caller does not override it. 500 / 3000 / 10000 are the common tiers.
pub const DEFAULT_SWAP_FEE: u32 = 3000;

/// Inputs fully resolved to on-chain values — no lookups left to do.
#[derive(Clone, Debug)]
pub struct BuildTxInputs {
    pub rpc_url: String,
    pub chain_id: u64,
    pub liquidator: Address,
    pub user: Address,
    pub collateral: Address,
    pub debt_asset: Address,
    pub debt_to_cover: U256,
    pub swap_fee: u32,
    pub min_profit: U256,
    pub gas_limit: u64,
    /// TESTING ONLY, fork-only. Precomputed Aave oracle-source bytecode
    /// overrides applied to the revm pre-flight (see `compute_mock_hf_overrides`).
    /// They affect the SIMULATION ONLY — the signed transaction is identical
    /// with or without them. Empty in normal operation.
    pub oracle_overrides: Vec<CodeOverride>,
    /// Human note describing any `--mock-hf` override, surfaced in the report.
    pub mock_hf_note: Option<String>,
}

/// Everything an operator needs to inspect and broadcast the transaction.
#[derive(Clone, Debug, serde::Serialize)]
pub struct BuildTxReport {
    /// 0x-prefixed EIP-2718 signed transaction, ready for `eth_sendRawTransaction`.
    pub raw_tx_hex: String,
    pub tx_hash: String,
    pub from: String,
    pub to: String,
    pub nonce: u64,
    pub chain_id: u64,
    pub gas_limit: u64,
    pub max_fee_per_gas: String,
    pub max_priority_fee_per_gas: String,
    /// 0x-hex of the `executeLiquidation` calldata (the tx `input`).
    pub calldata: String,
    pub user: String,
    pub collateral_asset: String,
    pub debt_asset: String,
    pub debt_to_cover: String,
    pub swap_fee: u32,
    pub min_profit: String,
    /// revm pre-flight result. `false` means the liquidation would revert as
    /// built (wrong executor key, below close factor, unprofitable swap, …).
    pub simulated_ok: bool,
    pub sim_gas_used: u64,
    pub sim_revert_reason: Option<String>,
    /// Set only when `--mock-hf` was used: a human note describing the fork-only
    /// oracle override applied to the pre-flight (never to the signed tx).
    pub mock_hf_note: Option<String>,
    /// Always false. This builder does not broadcast.
    pub broadcast: bool,
}

/// ABI-encode `executeLiquidation(...)`.
pub fn encode_execute_liquidation(
    user: Address,
    collateral: Address,
    debt_asset: Address,
    debt_to_cover: U256,
    swap_fee: u32,
    min_profit: U256,
) -> Bytes {
    use alloy::primitives::aliases::U24;
    // uint24 fee tier. `U24` cannot hold every u32, so clamp the (already
    // small: 500/3000/10000) fee into range rather than risk a panic.
    let fee = U24::try_from(swap_fee).unwrap_or_else(|_| U24::from(DEFAULT_SWAP_FEE as u16));

    IAaveV3Liquidator::executeLiquidationCall {
        user,
        collateralAsset: collateral,
        debtAsset: debt_asset,
        debtToCover: debt_to_cover,
        swapFee: fee,
        minProfit: min_profit,
    }
    .abi_encode()
    .into()
}

/// Apply the Aave close factor to a borrower's total debt. Health factors at or
/// above 0.95 permit only a 50% liquidation; below, the full debt.
pub fn close_factor_debt(total_debt: U256, health_factor_wad: U256) -> U256 {
    if health_factor_wad < U256::from(CLOSE_FACTOR_HF_THRESHOLD_WAD) {
        total_debt
    } else {
        total_debt / U256::from(2u8)
    }
}

/// Build and sign the liquidation transaction. Reads nonce + gas from the RPC
/// and runs a revm pre-flight, but broadcasts nothing.
pub async fn build_signed_liquidation_tx(
    signer: &LocalSigner,
    inputs: &BuildTxInputs,
) -> Result<BuildTxReport> {
    if inputs.debt_to_cover.is_zero() {
        return Err(anyhow!(
            "debt_to_cover is zero: the keeper has no resolved debt for this borrower"
        ));
    }

    let from = signer.address();

    let calldata = encode_execute_liquidation(
        inputs.user,
        inputs.collateral,
        inputs.debt_asset,
        inputs.debt_to_cover,
        inputs.swap_fee,
        inputs.min_profit,
    );

    // --- read-only RPC: nonce + gas ---
    let provider = ProviderBuilder::new()
        .connect(&inputs.rpc_url)
        .await
        .map_err(|e| anyhow!("rpc connect: {e}"))?
        .erased();

    let nonce = provider
        .get_transaction_count(from)
        .pending()
        .await
        .map_err(|e| anyhow!("get nonce: {e}"))?;

    // eth_gasPrice ≈ base + tip; double it for headroom. These are ESTIMATES —
    // the operator can re-price before broadcasting. priority <= max is enforced.
    let gas_price = provider.get_gas_price().await.unwrap_or(30_000_000_000);
    let priority = provider
        .get_max_priority_fee_per_gas()
        .await
        .unwrap_or(1_000_000_000);
    let max_fee_per_gas = gas_price.saturating_mul(2).max(gas_price + priority);
    let max_priority_fee_per_gas = priority.min(max_fee_per_gas);

    // --- revm pre-flight at latest block (profitability / revert gate) ---
    let block = provider
        .get_block_number()
        .await
        .map_err(|e| anyhow!("block number: {e}"))?;

    // --mock-hf: fork-only oracle overrides, precomputed by the caller.
    let overrides = inputs.oracle_overrides.clone();
    let mock_hf_note = inputs.mock_hf_note.clone();

    let backend: Arc<dyn StateBackend> = Arc::new(RpcStateBackend::new(provider));
    let sim = simulate_raw_call_with_overrides(
        backend,
        block,
        overrides,
        from,
        inputs.liquidator,
        calldata.clone(),
        U256::ZERO,
        inputs.gas_limit.max(1_000_000),
    )
    .await
    .map_err(|e| anyhow!("revm pre-flight: {e}"))?;

    // --- sign (EIP-1559) ---
    let mut tx = TxEip1559 {
        chain_id: inputs.chain_id,
        nonce,
        gas_limit: inputs.gas_limit,
        to: TxKind::Call(inputs.liquidator),
        value: U256::ZERO,
        input: calldata.clone(),
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
    let raw = buf.to_vec();
    let tx_hash: B256 = alloy::primitives::keccak256(&raw);

    Ok(BuildTxReport {
        raw_tx_hex: format!("0x{}", hex::encode(&raw)),
        tx_hash: format!("{tx_hash:#x}"),
        from: format!("{from:#x}"),
        to: format!("{:#x}", inputs.liquidator),
        nonce,
        chain_id: inputs.chain_id,
        gas_limit: inputs.gas_limit,
        max_fee_per_gas: max_fee_per_gas.to_string(),
        max_priority_fee_per_gas: max_priority_fee_per_gas.to_string(),
        calldata: format!("0x{}", hex::encode(&calldata)),
        user: format!("{:#x}", inputs.user),
        collateral_asset: format!("{:#x}", inputs.collateral),
        debt_asset: format!("{:#x}", inputs.debt_asset),
        debt_to_cover: inputs.debt_to_cover.to_string(),
        swap_fee: inputs.swap_fee,
        min_profit: inputs.min_profit.to_string(),
        simulated_ok: sim.success,
        sim_gas_used: sim.gas_used,
        sim_revert_reason: sim.failure,
        mock_hf_note,
        broadcast: false,
    })
}

/// Resolve a `LiquidationOpportunity` into `BuildTxInputs` and build the signed
/// tx. The opportunity must already carry `collateral` + `debt_asset` +
/// `repay_amount` (the keeper fills these from the data provider). `health_factor_wad`
/// drives the Aave close-factor cap on `debtToCover`.
#[allow(clippy::too_many_arguments)]
pub async fn build_tx_from_opportunity(
    signer: &LocalSigner,
    rpc_url: &str,
    chain_id: u64,
    liquidator: Address,
    opportunity: &LiquidationOpportunity,
    health_factor_wad: Option<U256>,
    swap_fee: u32,
    min_profit: U256,
    gas_limit: u64,
) -> Result<BuildTxReport> {
    let collateral = opportunity
        .collateral
        .ok_or_else(|| anyhow!("opportunity has no collateral asset resolved; the keeper could not read the borrower's reserves"))?;
    let debt_asset = opportunity
        .debt_asset
        .ok_or_else(|| anyhow!("opportunity has no debt asset resolved; the keeper could not read the borrower's reserves"))?;
    let user = opportunity
        .borrower
        .ok_or_else(|| anyhow!("opportunity has no borrower address"))?;
    let total_debt = opportunity
        .repay_amount
        .ok_or_else(|| anyhow!("opportunity has no debt amount resolved"))?;

    let debt_to_cover = match health_factor_wad {
        Some(hf) => close_factor_debt(total_debt, hf),
        // No HF: assume the conservative 50% partial-liquidation ceiling.
        None => total_debt / U256::from(2u8),
    };

    let inputs = BuildTxInputs {
        rpc_url: rpc_url.to_string(),
        chain_id,
        liquidator,
        user,
        collateral,
        debt_asset,
        debt_to_cover,
        swap_fee,
        min_profit,
        gas_limit,
        oracle_overrides: Vec::new(),
        mock_hf_note: None,
    };

    build_signed_liquidation_tx(signer, &inputs).await
}

/// Fully standalone builder: resolve the borrower's position **directly from
/// chain** (Aave data provider), apply the close factor, and build the signed
/// tx — with no dependency on the keeper's in-memory opportunity map or
/// `TRACKED_BORROWERS`. Works for any address with an open Aave V3 position.
///
/// `mock_hf_wad`, when set, drives both the close-factor sizing and the
/// pre-flight oracle override (testing).
#[allow(clippy::too_many_arguments)]
pub async fn build_tx_standalone(
    signer: &LocalSigner,
    rpc_url: &str,
    chain_id: u64,
    liquidator: Address,
    user: Address,
    mock_hf_wad: Option<U256>,
    swap_fee: u32,
    min_profit: U256,
    gas_limit: u64,
) -> Result<BuildTxReport> {
    use crate::keeper::Keeper;
    use std::collections::HashMap;
    use tokio::sync::RwLock;

    let keeper = Keeper::new(rpc_url, Arc::new(RwLock::new(HashMap::new()))).await?;

    let selection = keeper.resolve_reserves(user).await;
    let collateral = selection.collateral.ok_or_else(|| {
        anyhow!("could not resolve a collateral reserve for {user:#x}: no open Aave V3 position, or the data provider was unreachable")
    })?;
    let debt_asset = selection.debt_asset.ok_or_else(|| {
        anyhow!("could not resolve a debt reserve for {user:#x}: no open Aave V3 debt, or the data provider was unreachable")
    })?;
    if selection.debt_amount.is_zero() {
        return Err(anyhow!("resolved zero debt for {user:#x}; nothing to liquidate"));
    }

    // Size debtToCover against the mock HF when mocking, else the live HF.
    let hf = keeper.user_health_factor(user).await;
    let effective_hf = mock_hf_wad.or(hf);
    let debt_to_cover = match effective_hf {
        Some(h) => close_factor_debt(selection.debt_amount, h),
        None => selection.debt_amount / U256::from(2u8),
    };

    // Precompute the fork-only oracle overrides (all collateral sources).
    let (oracle_overrides, mock_hf_note) = match mock_hf_wad {
        Some(target) => compute_mock_hf_overrides(&keeper, user, target).await,
        None => (Vec::new(), None),
    };

    let inputs = BuildTxInputs {
        rpc_url: rpc_url.to_string(),
        chain_id,
        liquidator,
        user,
        collateral,
        debt_asset,
        debt_to_cover,
        swap_fee,
        min_profit,
        gas_limit,
        oracle_overrides,
        mock_hf_note,
    };

    build_signed_liquidation_tx(signer, &inputs).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    #[test]
    fn calldata_has_execute_liquidation_selector() {
        // selector = keccak256("executeLiquidation(address,address,address,uint256,uint24,uint256)")[0..4]
        let cd = encode_execute_liquidation(
            address!("1111111111111111111111111111111111111111"),
            address!("2222222222222222222222222222222222222222"),
            address!("3333333333333333333333333333333333333333"),
            U256::from(1000u64),
            DEFAULT_SWAP_FEE,
            U256::ZERO,
        );
        let expected = &alloy::primitives::keccak256(
            b"executeLiquidation(address,address,address,uint256,uint24,uint256)",
        )[0..4];
        assert_eq!(&cd[0..4], expected);
        // 4 selector + 6 * 32-byte words.
        assert_eq!(cd.len(), 4 + 6 * 32);
    }

    #[test]
    fn close_factor_full_below_threshold_half_above() {
        let debt = U256::from(1_000_000u64);
        // HF 0.90 < 0.95 -> full debt.
        assert_eq!(
            close_factor_debt(debt, U256::from(900_000_000_000_000_000u128)),
            debt
        );
        // HF 0.99 >= 0.95 -> half.
        assert_eq!(
            close_factor_debt(debt, U256::from(990_000_000_000_000_000u128)),
            debt / U256::from(2u8)
        );
    }
}
