//! Read-only Aave V3 Ethereum Mainnet Liquidation Monitor & Flash-Loan Simulator.
//!
//! Operates completely independently of the mempool scanner (`hits.jsonl`), writing
//! atomic snapshots to `logs/liquidations.json` so `/liquidations.html` and
//! `/api/liquidations` never disturb existing mempool observation workflows.
//! Strictly observational: no private keys, no signing, no transaction broadcasting.

use crate::state::{one_line_str, BlockSnapshot, ForkMonitor, PinnedStateView, StateBackend};
use alloy::eips::BlockNumberOrTag;
use alloy::network::TransactionBuilder;
use alloy::primitives::{address, Address, Bytes, TxKind, B256, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::{Filter, Log as RpcLog, TransactionRequest};
use alloy::sol_types::{SolCall, SolEvent};
use chrono::{SecondsFormat, Utc};
use futures_util::stream::{self, StreamExt};
use revm::bytecode::Bytecode;
use revm::context::TxEnv;
use revm::context_interface::block::BlobExcessGasAndPrice;
use revm::context_interface::result::{ExecutionResult, Output};
use revm::database::CacheDB;
use revm::primitives::eip4844::BLOB_BASE_FEE_UPDATE_FRACTION_PRAGUE;
use revm::primitives::{hardfork::SpecId, KECCAK_EMPTY};
use revm::state::AccountInfo;
use revm::{AsyncDb, Context, ExecuteEvm, MainBuilder, MainContext};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info};

/// Aave V3 Ethereum Mainnet Pool proxy (`IPool`).
pub const AAVE_V3_POOL: Address = address!("87870bca3f3fd6335c3f4ce8392d69350b4fa4e2");

/// Aave V3 Ethereum Mainnet Price Oracle (`IAaveOracle`, 8-decimal USD base).
pub const AAVE_V3_ORACLE: Address = address!("54586be62e3c3580375ae3723c145253060ca0c2");

/// Canonical Wrapped Ether on Ethereum Mainnet.
pub const WETH_MAINNET: Address = address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");

/// Synthetic read-only caller for `revm` view calls.
const LIQ_VIEW_CALLER: Address = address!("0000000000000000000000000000000000aaee03");

/// Aave V3 mainnet governance flash-loan premium: 5 bps (`0.05%`).
pub const FLASHLOAN_PREMIUM_BPS: u64 = 5;

/// Standard Aave V3 ETH/major-collateral liquidation bonus: 500 bps (`5.00%`).
pub const DEFAULT_LIQ_BONUS_BPS: u64 = 500;

/// Estimated gas units for atomic `flashLoanSimple` + `liquidationCall` + Uniswap V3 swap.
pub const FLASH_LIQ_GAS_UNITS: u64 = 380_000;

const BACKFILL_CHUNKS: u64 = 12;
const BACKFILL_CHUNK_BLOCKS: u64 = 500;
const MAX_TRACKED_BORROWERS: usize = 96;
const MAX_RECENT_LIQ_EVENTS: usize = 40;
const MIN_DEBT_BASE_USD_8DEC: u128 = 50_000_000; // $0.50 minimum active debt

mod aave_abi {
    use alloy::sol;

    sol! {
        event Borrow(
            address indexed reserve,
            address user,
            address indexed onBehalfOf,
            uint256 amount,
            uint8 interestRateMode,
            uint256 borrowRate,
            uint16 indexed referralCode
        );

        event Repay(
            address indexed reserve,
            address indexed user,
            address indexed repayer,
            uint256 amount,
            bool useATokens
        );

        event LiquidationCall(
            address indexed collateralAsset,
            address indexed debtAsset,
            address indexed user,
            uint256 debtToCover,
            uint256 liquidatedCollateralAmount,
            address liquidator,
            bool receiveAToken
        );

        function getUserAccountData(address user) external view returns (
            uint256 totalCollateralBase,
            uint256 totalDebtBase,
            uint256 availableBorrowsBase,
            uint256 currentLiquidationThreshold,
            uint256 ltv,
            uint256 healthFactor
        );

        function getAssetPrice(address asset) external view returns (uint256);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DecodedAccountData {
    pub total_collateral_base: u128,
    pub total_debt_base: u128,
    pub available_borrows_base: u128,
    pub current_liq_threshold_bps: u64,
    pub ltv_bps: u64,
    pub health_factor_wad: u128,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FlashLiquidationPlan {
    pub close_factor_pct: f64,
    pub liquidation_bonus_pct: f64,
    pub flash_fee_pct: f64,
    pub debt_to_cover_usd: f64,
    pub seized_collateral_usd: f64,
    pub gross_bounty_usd: f64,
    pub flash_loan_fee_usd: f64,
    pub dex_swap_fee_usd: f64,
    pub est_gas_usd: f64,
    pub net_profit_usd: f64,
    pub trigger_net_profit_usd: f64,
    pub profitable_now: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackedBorrowerPosition {
    pub user: String,
    pub collateral_usd: f64,
    pub debt_usd: f64,
    pub available_borrows_usd: f64,
    pub liq_threshold_bps: u64,
    pub liq_threshold_pct: f64,
    pub ltv_bps: u64,
    pub ltv_pct: f64,
    pub health_factor: f64,
    pub status: String,
    pub price_drop_pct_to_liq: f64,
    pub eth_liq_price_usd: f64,
    pub last_reserve_symbol: String,
    pub last_seen_block: u64,
    pub revm_verified: bool,
    pub revm_gas_used: u64,
    pub revm_sim_ms: f64,
    pub plan: FlashLiquidationPlan,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObservedLiquidationEvent {
    pub tx_hash: String,
    pub block_number: u64,
    pub user: String,
    pub liquidator: String,
    pub collateral_asset: String,
    pub collateral_symbol: String,
    pub debt_asset: String,
    pub debt_symbol: String,
    pub debt_to_cover_fmt: String,
    pub liquidated_collateral_fmt: String,
    pub debt_to_cover_usd_est: f64,
    pub receive_a_token: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiquidationSnapshot {
    pub protocol: String,
    pub pool_address: String,
    pub oracle_address: String,
    pub head_block: u64,
    pub scanned_from_block: u64,
    pub scanned_blocks_total: u64,
    pub discovered_borrowers_total: usize,
    pub active_borrowers_count: usize,
    pub eth_price_usd: f64,
    pub base_fee_gwei: f64,
    pub flashloan_premium_bps: u64,
    pub default_liq_bonus_bps: u64,
    pub updated_ts: String,
    pub status: String,
    pub positions: Vec<TrackedBorrowerPosition>,
    pub recent_liquidations: Vec<ObservedLiquidationEvent>,
}

#[derive(Clone, Debug)]
struct DiscoveredBorrowerMeta {
    last_seen_block: u64,
    last_reserve: Address,
    last_health_factor: f64,
}

pub fn asset_symbol_and_decimals(asset: Address) -> (&'static str, u8, bool) {
    // Returns (symbol, decimals, is_usd_pegged)
    if asset == WETH_MAINNET {
        ("WETH", 18, false)
    } else if asset == address!("7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0") {
        ("wstETH", 18, false)
    } else if asset == address!("cd5fe23c85820f7b72d0926fc9b05b43e359b7ee") {
        ("weETH", 18, false)
    } else if asset == address!("be9895146f7af43049ca1c1ae358b0541ea49704") {
        ("cbETH", 18, false)
    } else if asset == address!("ae78736cd615f374d3085123a210448e74fc6393") {
        ("rETH", 18, false)
    } else if asset == address!("2260fac5e5542a773aa44fbcfedf7c193bc2c599") {
        ("WBTC", 8, false)
    } else if asset == address!("cbb7c0000ab88b473b1f5afd9ef808440eed33bf") {
        ("cbBTC", 8, false)
    } else if asset == address!("18084fba666a33d37592fa2633fd49a74dd93a88") {
        ("tBTC", 18, false)
    } else if asset == address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48") {
        ("USDC", 6, true)
    } else if asset == address!("dac17f958d2ee523a2206206994597c13d831ec7") {
        ("USDT", 6, true)
    } else if asset == address!("6b175474e89094c44da98b954eedeac495271d0f") {
        ("DAI", 18, true)
    } else if asset == address!("40d16fc0246ad3160ccc09b8d0d3a2cd28ae6c2f") {
        ("GHO", 18, true)
    } else if asset == address!("4c9edd5852cd905f086c759e8383e09bff1e68b3") {
        ("USDe", 18, true)
    } else if asset == address!("9d39a5de30e57443bff2a8307a4256c8797a3497") {
        ("sUSDe", 18, true)
    } else if asset == address!("6c3ea9036406852006290770bedfcaba0e23a0e8") {
        ("PYUSD", 6, true)
    } else if asset == address!("5f98805a4e8be255a32880fdec7f6728c6568ba0") {
        ("LUSD", 18, true)
    } else if asset == address!("853d955acef822db058eb8505911ed77f175b99e") {
        ("FRAX", 18, true)
    } else if asset == address!("514910771af9ca656af840dff83e8264ecf986ca") {
        ("LINK", 18, false)
    } else if asset == address!("7fc66500c84a76ad7e9c93437bfc5ac33e2ddae9") {
        ("AAVE", 18, false)
    } else {
        ("ERC20", 18, false)
    }
}

fn format_units_compact(amount: U256, decimals: u8) -> (String, f64) {
    let scale = 10u128.pow((decimals as u32).min(18));
    let raw = u128::try_from(amount).unwrap_or(u128::MAX);
    let val = if scale > 0 {
        (raw as f64) / (scale as f64)
    } else {
        raw as f64
    };
    let formatted = if val >= 1_000.0 {
        format!("{val:.2}")
    } else if val >= 1.0 {
        format!("{val:.4}")
    } else {
        format!("{val:.6}")
    };
    (formatted, val)
}

/// Decodes 192-byte ABI return payload from `IPool.getUserAccountData(address)`.
pub fn decode_user_account_data(bytes: &[u8]) -> Option<DecodedAccountData> {
    if bytes.len() < 192 {
        return None;
    }
    let col = U256::from_be_slice(&bytes[0..32]);
    let debt = U256::from_be_slice(&bytes[32..64]);
    let avail = U256::from_be_slice(&bytes[64..96]);
    let liq_thresh = U256::from_be_slice(&bytes[96..128]);
    let ltv = U256::from_be_slice(&bytes[128..160]);
    let hf = U256::from_be_slice(&bytes[160..192]);

    Some(DecodedAccountData {
        total_collateral_base: u128::try_from(col).unwrap_or(u128::MAX),
        total_debt_base: u128::try_from(debt).unwrap_or(u128::MAX),
        available_borrows_base: u128::try_from(avail).unwrap_or(u128::MAX),
        current_liq_threshold_bps: u64::try_from(liq_thresh).unwrap_or(0),
        ltv_bps: u64::try_from(ltv).unwrap_or(0),
        health_factor_wad: u128::try_from(hf).unwrap_or(u128::MAX),
    })
}

pub fn classify_health_factor(hf: f64) -> &'static str {
    if hf < 1.0 {
        "liquidatable"
    } else if hf < 1.05 {
        "critical"
    } else if hf < 1.20 {
        "at_risk"
    } else {
        "healthy"
    }
}

/// Computes the 5-step atomic flash-loan liquidation sequence and trigger economics.
pub fn compute_flash_liquidation_plan(
    collateral_usd: f64,
    debt_usd: f64,
    health_factor: f64,
    eth_price_usd: f64,
    base_fee_wei: u64,
    close_factor_override: Option<f64>,
) -> (f64, f64, FlashLiquidationPlan) {
    let safe_hf = if health_factor.is_finite() && health_factor > 0.0 {
        health_factor
    } else {
        99.0
    };

    let price_drop_pct_to_liq = if safe_hf <= 1.0 {
        0.0
    } else {
        ((1.0 - (1.0 / safe_hf)) * 100.0).clamp(0.0, 99.99)
    };

    let eth_liq_price_usd = if eth_price_usd > 0.0 && safe_hf > 0.0 && safe_hf < 50.0 {
        eth_price_usd / safe_hf
    } else {
        0.0
    };

    // Standard 50% close factor (or 100% if explicitly overridden).
    let close_factor = close_factor_override.unwrap_or(0.50).clamp(0.05, 1.0);
    let bonus_rate = (DEFAULT_LIQ_BONUS_BPS as f64) / 10_000.0; // 0.05 (5%)
    let flash_fee_rate = (FLASHLOAN_PREMIUM_BPS as f64) / 10_000.0; // 0.0005 (0.05%)
    let dex_fee_rate = 0.001; // 0.10% Uniswap V3 pool fee + price impact

    let est_gas_eth = ((FLASH_LIQ_GAS_UNITS as u128) * (base_fee_wei.max(100_000_000) as u128))
        as f64
        / 1e18_f64;
    let est_gas_usd = est_gas_eth * eth_price_usd.max(1.0);

    // Helper for computing a liquidation execution at a given collateral value.
    let eval_at_collateral = |col_val: f64| -> (f64, f64, f64, f64, f64, f64) {
        if debt_usd <= 0.0 || col_val <= 0.0 {
            return (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        }
        let max_by_close = debt_usd * close_factor;
        let max_by_col = col_val / (1.0 + bonus_rate);
        let debt_to_cover = max_by_close.min(max_by_col).max(0.0);
        let seized_col = (debt_to_cover * (1.0 + bonus_rate)).min(col_val);
        let gross_bounty = (seized_col - debt_to_cover).max(0.0);
        let flash_fee = debt_to_cover * flash_fee_rate;
        let dex_fee = seized_col * dex_fee_rate;
        let net = gross_bounty - flash_fee - dex_fee - est_gas_usd;
        (debt_to_cover, seized_col, gross_bounty, flash_fee, dex_fee, net)
    };

    let (
        debt_to_cover_usd,
        seized_collateral_usd,
        gross_bounty_usd,
        flash_loan_fee_usd,
        dex_swap_fee_usd,
        raw_net_usd,
    ) = if safe_hf < 1.0 {
        eval_at_collateral(collateral_usd)
    } else {
        (0.0, 0.0, 0.0, 0.0, 0.0, 0.0)
    };

    // Projected bounty at the exact crossing point where HF hits 0.99.
    let trigger_collateral_usd = if safe_hf >= 1.0 && safe_hf < 50.0 {
        collateral_usd * (0.99 / safe_hf)
    } else {
        collateral_usd
    };
    let (_, _, _, _, _, trigger_net_profit_usd) = eval_at_collateral(trigger_collateral_usd);

    let net_profit_usd = if safe_hf < 1.0 { raw_net_usd } else { 0.0 };
    let profitable_now = safe_hf < 1.0 && net_profit_usd > 0.0;

    let plan = FlashLiquidationPlan {
        close_factor_pct: round2(close_factor * 100.0),
        liquidation_bonus_pct: round2(bonus_rate * 100.0),
        flash_fee_pct: round4(flash_fee_rate * 100.0),
        debt_to_cover_usd: round2(debt_to_cover_usd),
        seized_collateral_usd: round2(seized_collateral_usd),
        gross_bounty_usd: round2(gross_bounty_usd),
        flash_loan_fee_usd: round2(flash_loan_fee_usd),
        dex_swap_fee_usd: round2(dex_swap_fee_usd),
        est_gas_usd: round2(est_gas_usd),
        net_profit_usd: round2(net_profit_usd),
        trigger_net_profit_usd: round2(trigger_net_profit_usd),
        profitable_now,
    };

    (round2(price_drop_pct_to_liq), round2(eth_liq_price_usd), plan)
}

fn round2(v: f64) -> f64 {
    if !v.is_finite() {
        0.0
    } else {
        (v * 100.0).round() / 100.0
    }
}

fn round4(v: f64) -> f64 {
    if !v.is_finite() {
        0.0
    } else {
        (v * 10_000.0).round() / 10_000.0
    }
}

/// Executes `IPool.getUserAccountData(user)` inside in-process `revm` against `StateBackend`.
pub async fn verify_account_in_revm(
    user: Address,
    pool: Address,
    snap: &BlockSnapshot,
    backend: Arc<dyn StateBackend>,
) -> Option<(DecodedAccountData, u64, f64)> {
    let calldata = Bytes::from(aave_abi::getUserAccountDataCall { user }.abi_encode());
    let snap_owned = snap.clone();
    let handle = tokio::runtime::Handle::current();
    let started = Instant::now();

    let fut = tokio::task::spawn_blocking(move || {
        let view = PinnedStateView {
            backend,
            block: snap_owned.number,
        };
        let mut cache_db = CacheDB::new(AsyncDb::with_handle(view, handle));
        cache_db.insert_account_info(
            LIQ_VIEW_CALLER,
            AccountInfo {
                balance: U256::from(10_000_000_000_000_000_000u128),
                nonce: 0,
                code_hash: KECCAK_EMPTY,
                code: Some(Bytecode::default()),
                ..Default::default()
            },
        );

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
                b.number = U256::from(snap_owned.number);
                b.timestamp = U256::from(snap_owned.timestamp);
                b.basefee = snap_owned.base_fee;
                b.beneficiary = snap_owned.beneficiary;
                b.gas_limit = snap_owned.gas_limit.max(5_000_000);
                b.prevrandao = snap_owned.prevrandao.or(Some(B256::ZERO));
                b.blob_excess_gas_and_price = snap_owned.excess_blob_gas.map(|excess| {
                    BlobExcessGasAndPrice::new(excess, BLOB_BASE_FEE_UPDATE_FRACTION_PRAGUE)
                });
            })
            .build_mainnet();

        let tx_env = TxEnv::builder()
            .caller(LIQ_VIEW_CALLER)
            .kind(TxKind::Call(pool))
            .data(calldata)
            .value(U256::ZERO)
            .gas_limit(3_500_000)
            .gas_price(snap_owned.base_fee as u128)
            .build()
            .ok()?;

        match evm.transact(tx_env).ok()?.result {
            ExecutionResult::Success { gas, output, .. } => {
                let bytes = match output {
                    Output::Call(b) | Output::Create(b, _) => b,
                };
                let decoded = decode_user_account_data(&bytes)?;
                Some((decoded, gas.tx_gas_used()))
            }
            _ => None,
        }
    });

    match tokio::time::timeout(Duration::from_secs(4), fut).await {
        Ok(Ok(Some((decoded, gas_used)))) => {
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            Some((decoded, gas_used, round2(ms)))
        }
        _ => None,
    }
}

async fn fetch_oracle_eth_usd(provider: &DynProvider) -> Option<f64> {
    let call = aave_abi::getAssetPriceCall {
        asset: WETH_MAINNET,
    };
    let req = TransactionRequest::default()
        .with_to(AAVE_V3_ORACLE)
        .with_call(&call);
    let raw = tokio::time::timeout(Duration::from_secs(4), provider.call(req))
        .await
        .ok()?
        .ok()?;
    if raw.len() < 32 {
        return None;
    }
    let price_8dec = u128::try_from(U256::from_be_slice(&raw[0..32])).ok()?;
    let usd = (price_8dec as f64) / 1e8_f64;
    if usd > 100.0 && usd < 1_000_000.0 {
        Some(round2(usd))
    } else {
        None
    }
}

async fn fetch_user_account_rpc(provider: &DynProvider, user: Address) -> Option<DecodedAccountData> {
    let call = aave_abi::getUserAccountDataCall { user };
    let req = TransactionRequest::default()
        .with_to(AAVE_V3_POOL)
        .with_call(&call);
    let raw = tokio::time::timeout(Duration::from_secs(4), provider.call(req))
        .await
        .ok()?
        .ok()?;
    decode_user_account_data(&raw)
}

async fn fetch_pool_logs_resilient(
    provider: &DynProvider,
    from_block: u64,
    to_block: u64,
) -> Vec<RpcLog> {
    if from_block > to_block {
        return Vec::new();
    }
    let sigs = vec![
        aave_abi::Borrow::SIGNATURE_HASH,
        aave_abi::Repay::SIGNATURE_HASH,
        aave_abi::LiquidationCall::SIGNATURE_HASH,
    ];
    let filter = Filter::new()
        .address(AAVE_V3_POOL)
        .from_block(BlockNumberOrTag::Number(from_block))
        .to_block(BlockNumberOrTag::Number(to_block))
        .event_signature(sigs.clone());

    match tokio::time::timeout(Duration::from_secs(6), provider.get_logs(&filter)).await {
        Ok(Ok(logs)) => logs,
        Ok(Err(err)) => {
            let span = to_block.saturating_sub(from_block);
            if span > 150 {
                debug!(
                    "Bisecting Aave V3 get_logs {from_block}..={to_block} after RPC error: {}",
                    one_line_str(&err.to_string())
                );
                let mut out = Vec::new();
                let mut cur = from_block;
                while cur <= to_block {
                    let end = (cur + 149).min(to_block);
                    let sub_filter = Filter::new()
                        .address(AAVE_V3_POOL)
                        .from_block(BlockNumberOrTag::Number(cur))
                        .to_block(BlockNumberOrTag::Number(end))
                        .event_signature(sigs.clone());
                    if let Ok(Ok(mut sub_logs)) =
                        tokio::time::timeout(Duration::from_secs(5), provider.get_logs(&sub_filter))
                            .await
                    {
                        out.append(&mut sub_logs);
                    }
                    cur = end.saturating_add(1);
                }
                out
            } else {
                Vec::new()
            }
        }
        Err(_) => Vec::new(),
    }
}

fn ingest_pool_logs(
    logs: &[RpcLog],
    eth_price_usd: f64,
    borrowers: &mut HashMap<Address, DiscoveredBorrowerMeta>,
    liq_events: &mut VecDeque<ObservedLiquidationEvent>,
) {
    for log in logs {
        let block_num = log.block_number.unwrap_or(0);
        let Some(&topic0) = log.topic0() else {
            continue;
        };

        if topic0 == aave_abi::Borrow::SIGNATURE_HASH {
            if let Ok(decoded) = log.log_decode::<aave_abi::Borrow>() {
                let d = &decoded.inner.data;
                let target = if d.onBehalfOf != Address::ZERO {
                    d.onBehalfOf
                } else {
                    d.user
                };
                if target != Address::ZERO {
                    let entry = borrowers.entry(target).or_insert(DiscoveredBorrowerMeta {
                        last_seen_block: block_num,
                        last_reserve: d.reserve,
                        last_health_factor: 99.0,
                    });
                    if block_num >= entry.last_seen_block {
                        entry.last_seen_block = block_num;
                        entry.last_reserve = d.reserve;
                    }
                }
            }
        } else if topic0 == aave_abi::Repay::SIGNATURE_HASH {
            if let Ok(decoded) = log.log_decode::<aave_abi::Repay>() {
                let d = &decoded.inner.data;
                if d.user != Address::ZERO {
                    let entry = borrowers.entry(d.user).or_insert(DiscoveredBorrowerMeta {
                        last_seen_block: block_num,
                        last_reserve: d.reserve,
                        last_health_factor: 99.0,
                    });
                    if block_num >= entry.last_seen_block {
                        entry.last_seen_block = block_num;
                        entry.last_reserve = d.reserve;
                    }
                }
            }
        } else if topic0 == aave_abi::LiquidationCall::SIGNATURE_HASH {
            if let Ok(decoded) = log.log_decode::<aave_abi::LiquidationCall>() {
                let d = &decoded.inner.data;
                if d.user != Address::ZERO {
                    let entry = borrowers.entry(d.user).or_insert(DiscoveredBorrowerMeta {
                        last_seen_block: block_num,
                        last_reserve: d.debtAsset,
                        last_health_factor: 1.0,
                    });
                    if block_num >= entry.last_seen_block {
                        entry.last_seen_block = block_num;
                        entry.last_reserve = d.debtAsset;
                    }
                }

                let tx_hash = log
                    .transaction_hash
                    .map(|h| format!("{h:#x}"))
                    .unwrap_or_default();
                let already_recorded = !tx_hash.is_empty()
                    && liq_events
                        .iter()
                        .any(|ev| ev.tx_hash == tx_hash && ev.user == format!("{:#x}", d.user));
                if !already_recorded {
                    let (col_sym, col_dec, _) = asset_symbol_and_decimals(d.collateralAsset);
                    let (debt_sym, debt_dec, debt_is_usd) = asset_symbol_and_decimals(d.debtAsset);
                    let (col_fmt, _) = format_units_compact(d.liquidatedCollateralAmount, col_dec);
                    let (debt_fmt, debt_units) = format_units_compact(d.debtToCover, debt_dec);
                    let debt_usd_est = if debt_is_usd {
                        round2(debt_units)
                    } else if debt_sym.contains("ETH") {
                        round2(debt_units * eth_price_usd.max(1.0))
                    } else {
                        0.0
                    };

                    liq_events.push_back(ObservedLiquidationEvent {
                        tx_hash,
                        block_number: block_num,
                        user: format!("{:#x}", d.user),
                        liquidator: format!("{:#x}", d.liquidator),
                        collateral_asset: format!("{:#x}", d.collateralAsset),
                        collateral_symbol: col_sym.to_string(),
                        debt_asset: format!("{:#x}", d.debtAsset),
                        debt_symbol: debt_sym.to_string(),
                        debt_to_cover_fmt: format!("{debt_fmt} {debt_sym}"),
                        liquidated_collateral_fmt: format!("{col_fmt} {col_sym}"),
                        debt_to_cover_usd_est: debt_usd_est,
                        receive_a_token: d.receiveAToken,
                    });
                    while liq_events.len() > MAX_RECENT_LIQ_EVENTS {
                        liq_events.pop_front();
                    }
                }
            }
        }
    }
}

fn select_borrower_candidates(
    borrowers: &HashMap<Address, DiscoveredBorrowerMeta>,
) -> Vec<(Address, DiscoveredBorrowerMeta)> {
    let mut all: Vec<(Address, DiscoveredBorrowerMeta)> =
        borrowers.iter().map(|(&a, m)| (a, m.clone())).collect();
    // Prioritize known low-HF borrowers first, then most recently active borrowers.
    all.sort_by(|a, b| {
        a.1.last_health_factor
            .partial_cmp(&b.1.last_health_factor)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.1.last_seen_block.cmp(&a.1.last_seen_block))
    });
    all.truncate(MAX_TRACKED_BORROWERS);
    all
}

async fn evaluate_tracked_positions(
    provider: &DynProvider,
    backend: Arc<dyn StateBackend>,
    snap: &BlockSnapshot,
    eth_price_usd: f64,
    borrowers: &mut HashMap<Address, DiscoveredBorrowerMeta>,
) -> Vec<TrackedBorrowerPosition> {
    let candidates = select_borrower_candidates(borrowers);
    if candidates.is_empty() {
        return Vec::new();
    }

    let base_fee_wei = snap.base_fee;
    let results: Vec<Option<(Address, TrackedBorrowerPosition)>> = stream::iter(candidates)
        .map(|(addr, meta)| {
            let prov = provider.clone();
            async move {
                let acct = fetch_user_account_rpc(&prov, addr).await?;
                if acct.total_debt_base < MIN_DEBT_BASE_USD_8DEC {
                    return None;
                }
                let collateral_usd = (acct.total_collateral_base as f64) / 1e8_f64;
                let debt_usd = (acct.total_debt_base as f64) / 1e8_f64;
                let avail_usd = (acct.available_borrows_base as f64) / 1e8_f64;
                let hf = if acct.health_factor_wad >= 100_000_000_000_000_000_000u128 {
                    99.99
                } else {
                    (acct.health_factor_wad as f64) / 1e18_f64
                };
                let status = classify_health_factor(hf).to_string();
                let (drop_pct, eth_liq_usd, plan) = compute_flash_liquidation_plan(
                    collateral_usd,
                    debt_usd,
                    hf,
                    eth_price_usd,
                    base_fee_wei,
                    None,
                );
                let (sym, _, _) = asset_symbol_and_decimals(meta.last_reserve);
                Some((
                    addr,
                    TrackedBorrowerPosition {
                        user: format!("{addr:#x}"),
                        collateral_usd: round2(collateral_usd),
                        debt_usd: round2(debt_usd),
                        available_borrows_usd: round2(avail_usd),
                        liq_threshold_bps: acct.current_liq_threshold_bps,
                        liq_threshold_pct: round2((acct.current_liq_threshold_bps as f64) / 100.0),
                        ltv_bps: acct.ltv_bps,
                        ltv_pct: round2((acct.ltv_bps as f64) / 100.0),
                        health_factor: round4(hf),
                        status,
                        price_drop_pct_to_liq: drop_pct,
                        eth_liq_price_usd: eth_liq_usd,
                        last_reserve_symbol: sym.to_string(),
                        last_seen_block: meta.last_seen_block,
                        revm_verified: false,
                        revm_gas_used: 0,
                        revm_sim_ms: 0.0,
                        plan,
                    },
                ))
            }
        })
        .buffer_unordered(6)
        .collect()
        .await;

    let mut active_addrs = HashMap::new();
    let mut positions = Vec::new();
    for item in results.into_iter().flatten() {
        active_addrs.insert(item.0, item.1.health_factor);
        positions.push((item.0, item.1));
    }

    // Update cached health factors and drop repaid/zero-debt accounts when map grows large.
    for (addr, hf) in &active_addrs {
        if let Some(m) = borrowers.get_mut(addr) {
            m.last_health_factor = *hf;
        }
    }
    if borrowers.len() > MAX_TRACKED_BORROWERS * 2 {
        borrowers.retain(|addr, _| active_addrs.contains_key(addr));
    }

    // Sort by lowest Health Factor first so liquidatable & critical borrowers lead the table.
    positions.sort_by(|a, b| {
        a.1.health_factor
            .partial_cmp(&b.1.health_factor)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b.1.debt_usd
                    .partial_cmp(&a.1.debt_usd)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });

    // Run in-process Revm verification on the lowest-HF borrower (plus any liquidatable ones, max 2).
    let mut revm_checks = 0usize;
    for (addr, pos) in &mut positions {
        let should_verify = revm_checks == 0 || (pos.health_factor < 1.05 && revm_checks < 2);
        if should_verify {
            revm_checks += 1;
            if let Some((_, gas_used, sim_ms)) =
                verify_account_in_revm(*addr, AAVE_V3_POOL, snap, backend.clone()).await
            {
                pos.revm_verified = true;
                pos.revm_gas_used = gas_used;
                pos.revm_sim_ms = sim_ms;
            }
        }
    }

    positions.into_iter().map(|(_, p)| p).collect()
}

fn sibling_liq_path(hits_log_path: &Path) -> PathBuf {
    match hits_log_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join("liquidations.json"),
        _ => PathBuf::from("liquidations.json"),
    }
}

fn write_snapshot_atomic(path: &Path, snap: &LiquidationSnapshot) {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }
    let tmp_path = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(".liquidations.json.tmp"),
        _ => PathBuf::from(".liquidations.json.tmp"),
    };
    if let Ok(payload) = serde_json::to_vec(snap) {
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

/// Long-running background task monitoring Aave V3 Ethereum Mainnet borrowers.
pub async fn liquidation_watcher_loop(
    hits_log_path: PathBuf,
    provider: DynProvider,
    monitor: Arc<ForkMonitor>,
    backend: Arc<dyn StateBackend>,
) {
    let out_path = sibling_liq_path(&hits_log_path);
    let mut borrowers: HashMap<Address, DiscoveredBorrowerMeta> = HashMap::new();
    let mut liq_events: VecDeque<ObservedLiquidationEvent> = VecDeque::new();

    // Seed initial empty "syncing" snapshot if none exists on disk yet.
    if !out_path.exists() {
        let initial = LiquidationSnapshot {
            protocol: "Aave V3 · Ethereum Mainnet".to_string(),
            pool_address: format!("{AAVE_V3_POOL:#x}"),
            oracle_address: format!("{AAVE_V3_ORACLE:#x}"),
            head_block: 0,
            scanned_from_block: 0,
            scanned_blocks_total: 0,
            discovered_borrowers_total: 0,
            active_borrowers_count: 0,
            eth_price_usd: 2650.0,
            base_fee_gwei: 1.0,
            flashloan_premium_bps: FLASHLOAN_PREMIUM_BPS,
            default_liq_bonus_bps: DEFAULT_LIQ_BONUS_BPS,
            updated_ts: now_iso(),
            status: "syncing".to_string(),
            positions: Vec::new(),
            recent_liquidations: Vec::new(),
        };
        write_snapshot_atomic(&out_path, &initial);
    }

    // Wait briefly until ForkMonitor reports a head block.
    let mut head_block = loop {
        if let Some(b) = monitor.current_status().await.block {
            break b;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };

    let mut eth_price_usd = fetch_oracle_eth_usd(&provider).await.unwrap_or(2650.0);
    let mut scanned_from_block = head_block;
    let mut last_scanned_head = head_block;

    // Progressive startup backfill: scan recent Aave V3 Pool events backwards in chunks,
    // publishing the first live snapshot immediately after chunk 0.
    let mut cursor_to = head_block;
    for chunk_idx in 0..BACKFILL_CHUNKS {
        if cursor_to == 0 {
            break;
        }
        let from_b = cursor_to.saturating_sub(BACKFILL_CHUNK_BLOCKS - 1);
        let logs = fetch_pool_logs_resilient(&provider, from_b, cursor_to).await;
        ingest_pool_logs(&logs, eth_price_usd, &mut borrowers, &mut liq_events);
        scanned_from_block = from_b;

        let should_publish =
            chunk_idx == 0 || chunk_idx == 3 || chunk_idx + 1 == BACKFILL_CHUNKS;
        if should_publish && !borrowers.is_empty() {
            let snap = monitor.snapshot_for(head_block).await;
            let positions = evaluate_tracked_positions(
                &provider,
                backend.clone(),
                &snap,
                eth_price_usd,
                &mut borrowers,
            )
            .await;
            let mut recent: Vec<ObservedLiquidationEvent> = liq_events.iter().cloned().collect();
            recent.sort_by(|a, b| b.block_number.cmp(&a.block_number));

            let snapshot = LiquidationSnapshot {
                protocol: "Aave V3 · Ethereum Mainnet".to_string(),
                pool_address: format!("{AAVE_V3_POOL:#x}"),
                oracle_address: format!("{AAVE_V3_ORACLE:#x}"),
                head_block,
                scanned_from_block,
                scanned_blocks_total: head_block.saturating_sub(scanned_from_block) + 1,
                discovered_borrowers_total: borrowers.len(),
                active_borrowers_count: positions.len(),
                eth_price_usd,
                base_fee_gwei: round4((snap.base_fee as f64) / 1e9_f64),
                flashloan_premium_bps: FLASHLOAN_PREMIUM_BPS,
                default_liq_bonus_bps: DEFAULT_LIQ_BONUS_BPS,
                updated_ts: now_iso(),
                status: if chunk_idx + 1 == BACKFILL_CHUNKS {
                    "live".to_string()
                } else {
                    "backfilling".to_string()
                },
                positions,
                recent_liquidations: recent,
            };
            write_snapshot_atomic(&out_path, &snapshot);
        }

        if from_b == 0 {
            break;
        }
        cursor_to = from_b - 1;
        tokio::time::sleep(Duration::from_millis(120)).await;
    }

    info!(
        "Aave V3 liquidation monitor synced: discovered {} borrowers across blocks {}..={}",
        borrowers.len(),
        scanned_from_block,
        head_block
    );

    // Steady-state polling loop every 15 seconds.
    let mut interval = tokio::time::interval(Duration::from_secs(15));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;

        let Some(latest_head) = monitor.current_status().await.block else {
            continue;
        };
        head_block = latest_head;

        if let Some(fresh_eth) = fetch_oracle_eth_usd(&provider).await {
            eth_price_usd = fresh_eth;
        }

        if head_block > last_scanned_head {
            let from_new = last_scanned_head + 1;
            let span_from = from_new.max(head_block.saturating_sub(BACKFILL_CHUNK_BLOCKS));
            let new_logs = fetch_pool_logs_resilient(&provider, span_from, head_block).await;
            ingest_pool_logs(&new_logs, eth_price_usd, &mut borrowers, &mut liq_events);
            last_scanned_head = head_block;
        }

        let snap = monitor.snapshot_for(head_block).await;
        let positions = evaluate_tracked_positions(
            &provider,
            backend.clone(),
            &snap,
            eth_price_usd,
            &mut borrowers,
        )
        .await;

        let mut recent: Vec<ObservedLiquidationEvent> = liq_events.iter().cloned().collect();
        recent.sort_by(|a, b| b.block_number.cmp(&a.block_number));

        let snapshot = LiquidationSnapshot {
            protocol: "Aave V3 · Ethereum Mainnet".to_string(),
            pool_address: format!("{AAVE_V3_POOL:#x}"),
            oracle_address: format!("{AAVE_V3_ORACLE:#x}"),
            head_block,
            scanned_from_block,
            scanned_blocks_total: head_block.saturating_sub(scanned_from_block) + 1,
            discovered_borrowers_total: borrowers.len(),
            active_borrowers_count: positions.len(),
            eth_price_usd,
            base_fee_gwei: round4((snap.base_fee as f64) / 1e9_f64),
            flashloan_premium_bps: FLASHLOAN_PREMIUM_BPS,
            default_liq_bonus_bps: DEFAULT_LIQ_BONUS_BPS,
            updated_ts: now_iso(),
            status: "live".to_string(),
            positions,
            recent_liquidations: recent,
        };
        write_snapshot_atomic(&out_path, &snapshot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::InMemoryStateBackend;

    #[test]
    fn alice_flash_liquidation_math_matches_protocol_example() {
        // Stage A: Alice deposits 5 ETH @ $2,000 ($10,000), LT = 80%, borrows $6,000 USDC -> HF = 1.3333
        let (drop_pct, eth_liq_usd, safe_plan) = compute_flash_liquidation_plan(
            10_000.0,
            6_000.0,
            8_000.0 / 6_000.0,
            2_000.0,
            1_000_000_000, // 1 gwei
            Some(0.50),
        );
        assert_eq!(drop_pct, 25.0);
        assert_eq!(eth_liq_usd, 1_500.0);
        assert!(!safe_plan.profitable_now);
        assert!(safe_plan.trigger_net_profit_usd > 130.0);

        // Stage B: ETH drops to $1,400 -> $7,000 collateral, $6,000 debt, HF = 0.9333 (< 1.0)
        // Bob flash-borrows $3,000 (50% close factor), seizes $3,150 ETH (+5% bonus),
        // pays $1.50 flash loan fee (0.05%), swaps on DEX, and nets positive bounty.
        let (liq_drop, _, liq_plan) = compute_flash_liquidation_plan(
            7_000.0,
            6_000.0,
            5_600.0 / 6_000.0,
            1_400.0,
            1_000_000_000, // 1 gwei
            Some(0.50),
        );
        assert_eq!(liq_drop, 0.0);
        assert_eq!(liq_plan.debt_to_cover_usd, 3_000.0);
        assert_eq!(liq_plan.seized_collateral_usd, 3_150.0);
        assert_eq!(liq_plan.gross_bounty_usd, 150.0);
        assert_eq!(liq_plan.flash_loan_fee_usd, 1.5);
        assert!(liq_plan.profitable_now);
        assert!(liq_plan.net_profit_usd > 140.0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn decode_user_account_data_and_revm_verification_succeeds() {
        let backend = Arc::new(InMemoryStateBackend::default());
        let mock_pool = address!("87870bca3f3fd6335c3f4ce8392d69350b4fa4e2");
        let alice = address!("aaaa000000000000000000000000000000000077");

        // Build 192-byte return payload:
        // collateral = $7,000 (700_000_000_000), debt = $6,000 (600_000_000_000),
        // avail = 0, liq_threshold = 8000, ltv = 7500, hf = 0.9333e18
        let mut ret = [0u8; 192];
        ret[0..32].copy_from_slice(&U256::from(700_000_000_000u128).to_be_bytes::<32>());
        ret[32..64].copy_from_slice(&U256::from(600_000_000_000u128).to_be_bytes::<32>());
        ret[96..128].copy_from_slice(&U256::from(8_000u64).to_be_bytes::<32>());
        ret[128..160].copy_from_slice(&U256::from(7_500u64).to_be_bytes::<32>());
        ret[160..192]
            .copy_from_slice(&U256::from(933_333_333_333_333_333u128).to_be_bytes::<32>());

        // Tiny EVM runtime bytecode that stores the 6 words into memory 0..192 and RETURNs(0, 192).
        let mut code = Vec::new();
        for i in 0..6 {
            code.push(0x7f); // PUSH32
            code.extend_from_slice(&ret[i * 32..(i + 1) * 32]);
            code.push(0x60); // PUSH1 offset
            code.push((i * 32) as u8);
            code.push(0x52); // MSTORE
        }
        code.extend_from_slice(&[0x60, 0xc0, 0x60, 0x00, 0xf3]); // PUSH1 192, PUSH1 0, RETURN

        backend.insert_account(mock_pool, U256::ZERO, Some(code));

        let snap = BlockSnapshot::default();
        let (decoded, gas_used, _) =
            verify_account_in_revm(alice, mock_pool, &snap, backend)
                .await
                .expect("revm verification should succeed");

        assert_eq!(decoded.total_collateral_base, 700_000_000_000);
        assert_eq!(decoded.total_debt_base, 600_000_000_000);
        assert_eq!(decoded.current_liq_threshold_bps, 8_000);
        assert!(decoded.health_factor_wad < 1_000_000_000_000_000_000);
        assert!(gas_used >= 21_000);
    }
}
