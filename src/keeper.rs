//! Aave V3 keeper — polls borrower health factors on Pyth price updates.
//!
//! When the oracle publishes a new ETH/asset price, the keeper re-checks all
//! tracked borrowers via Aave V3 `Pool.getUserAccountData`. Borrowers with
//! health factor < 1.0 are flagged as liquidatable and pushed into the
//! shared `recent_opportunities` map so the API server can surface them.

use crate::exec::LiquidationOpportunity;
use alloy::primitives::{address, Address, Bytes, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use anyhow::Result;
use futures_util::{stream, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

// Aave V3 Ethereum Mainnet Pool address.
pub const AAVE_V3_POOL: Address = address!("87870bca3f3fd6335c3f4ce8392d69350b4fa4e2");

// Aave V3 Ethereum Mainnet `AaveProtocolDataProvider`.
//
// NOTE: the previous dev referred to this as "UiPoolDataProviderV3" and had the
// dead address `0x91c0d1A5…bC9e`. The live contract reported by the
// PoolAddressesProvider is the one below. It is the `AaveProtocolDataProvider`
// (a.k.a. PoolDataProvider): `getAllReservesTokens()` lists every reserve with
// its symbol, and `getUserReserveData(asset,user)` returns the borrower's
// *actual* current aToken/debt balances in asset units — no scaled-balance or
// liquidity-index math required, which is why it is preferred here over the
// UI data provider's scaled values.
pub const AAVE_PROTOCOL_DATA_PROVIDER: Address =
    address!("0a16f2fcc0d44fae41cc54e079281d84a363becd");

// Aave V3 Ethereum Mainnet Price Oracle (`IAaveOracle`, 8-decimal USD base).
pub const AAVE_V3_ORACLE: Address = address!("54586be62e3c3580375ae3723c145253060ca0c2");

// Aave V3 governance flash-loan premium: 5 bps (0.05%).
pub const FLASHLOAN_PREMIUM_BPS: u64 = 5;

// Standard Aave V3 liquidation bonus: 500 bps (5.00%).
pub const LIQUIDATION_BONUS_BPS: u64 = 500;

// Estimated gas units for atomic flash loan + liquidation + swap.
pub const FLASH_LIQ_GAS_UNITS: u64 = 380_000;

alloy::sol! {
    #[sol(rpc)]
    #[derive(Debug, PartialEq)]
    interface IPool {
        function getUserAccountData(address user) external view returns (
            uint256 totalCollateralBase,
            uint256 totalDebtBase,
            uint256 availableBorrowsBase,
            uint256 currentLiquidationThreshold,
            uint256 ltv,
            uint256 healthFactor
        );
    }
}

alloy::sol! {
    #[sol(rpc)]
    struct TokenData {
        string symbol;
        address tokenAddress;
    }

    #[sol(rpc)]
    interface IProtocolDataProvider {
        function getAllReservesTokens() external view returns (TokenData[] memory);

        // uint40 `stableRateLastUpdated` is declared as uint256 here: the field
        // is unused and both widths ABI-decode from the same 32-byte word, which
        // sidesteps alloy's non-standard-width integer aliases. The 4-byte
        // selector depends only on the argument types, so this is wire-correct.
        function getUserReserveData(address asset, address user) external view returns (
            uint256 currentATokenBalance,
            uint256 currentStableDebt,
            uint256 currentVariableDebt,
            uint256 principalStableDebt,
            uint256 scaledVariableDebt,
            uint256 stableBorrowRate,
            uint256 liquidityRate,
            uint256 stableRateLastUpdated,
            bool usageAsCollateralEnabled
        );
    }

    #[sol(rpc)]
    interface IAaveOracle {
        // Price in the oracle's USD base currency, 8 decimals.
        function getAssetPrice(address asset) external view returns (uint256);
        // The price feed source (adapter) the oracle reads for this asset.
        function getSourceOfAsset(address asset) external view returns (address);
    }

    #[sol(rpc)]
    interface IErc20Meta {
        function decimals() external view returns (uint8);
    }
}

/// Static metadata for one Aave reserve, cached after the first lookup.
#[derive(Clone, Debug)]
pub struct ReserveMeta {
    pub asset: Address,
    pub symbol: String,
    pub decimals: u8,
}

/// The single largest collateral and debt reserve for a borrower, chosen by
/// USD value (oracle price × actual balance), not by raw token balance — token
/// decimals and per-asset prices differ by orders of magnitude, so raw balances
/// are not comparable across reserves.
#[derive(Clone, Debug, Default)]
pub struct ReserveSelection {
    pub collateral: Option<Address>,
    pub collateral_symbol: Option<String>,
    pub collateral_usd: f64,
    pub debt_asset: Option<Address>,
    pub debt_symbol: Option<String>,
    pub debt_usd: f64,
    /// Actual debt owed on the chosen debt asset, in that asset's own units
    /// (variable + stable). This is the liquidation's `debtToCover` ceiling.
    pub debt_amount: U256,
}

/// Result of a borrower health check.
#[derive(Clone, Debug)]
pub struct BorrowerHealth {
    pub address: Address,
    pub health_factor: U256,        // scaled by 1e18 (1.0 = 1e18)
    pub total_collateral_base: U256, // in debt-asset base units
    pub total_debt_base: U256,      // in debt-asset base units
    pub available_borrows_base: U256,
    pub current_liquidation_threshold: U256,
    pub ltv: U256,
    pub is_liquidatable: bool,
    pub net_profit_estimate: U256,  // wei
}

/// Aave V3 keeper state.
#[derive(Clone)]
pub struct Keeper {
    provider: DynProvider,
    recent_opportunities: Arc<RwLock<HashMap<String, LiquidationOpportunity>>>,
    tracked_borrowers: Arc<RwLock<Vec<Address>>>,
    operator_address: Option<Address>,
    /// Reserve list + decimals, fetched once from the data provider and reused.
    reserves_cache: Arc<RwLock<Option<Vec<ReserveMeta>>>>,
}

impl Keeper {
    /// Create a new keeper.
    pub async fn new(
        rpc_url: &str,
        recent_opportunities: Arc<RwLock<HashMap<String, LiquidationOpportunity>>>,
    ) -> Result<Self> {
        let provider: DynProvider = ProviderBuilder::new()
            .connect_http(rpc_url.parse()?)
            .erased();

        Ok(Self {
            provider,
            recent_opportunities,
            tracked_borrowers: Arc::new(RwLock::new(Vec::new())),
            operator_address: None,
            reserves_cache: Arc::new(RwLock::new(None)),
        })
    }

    /// Set the operator address (the liquidator EOA).
    pub fn with_operator(mut self, addr: Address) -> Self {
        self.operator_address = Some(addr);
        self
    }

    /// Add a borrower address to track.
    pub async fn add_borrower(&self, addr: Address) {
        let mut g = self.tracked_borrowers.write().await;
        if !g.contains(&addr) {
            g.push(addr);
        }
    }

    /// Get all tracked borrowers.
    pub async fn tracked_borrowers(&self) -> Vec<Address> {
        self.tracked_borrowers.read().await.clone()
    }

    /// Get all borrower health snapshots.
    pub async fn all_health(&self) -> Vec<BorrowerHealth> {
        match self.poll_borrowers().await {
            Ok(list) => list,
            Err(e) => {
                warn!("Failed to poll borrowers: {e}");
                Vec::new()
            }
        }
    }

    /// Poll health factors for all tracked borrowers and update internal state.
    /// Liquidatable borrowers are pushed into `recent_opportunities`.
    pub async fn poll_borrowers(&self) -> Result<Vec<BorrowerHealth>> {
        let borrowers = self.tracked_borrowers.read().await.clone();
        let mut results = Vec::new();

        for addr in borrowers {
            match self.get_user_account_data(addr).await {
                Ok(data) => {
                    let hf = data.health_factor;
                    let is_liq = hf < U256::from(10u128.pow(18)); // HF < 1.0

                    let health = BorrowerHealth {
                        address: addr,
                        health_factor: hf,
                        total_collateral_base: data.total_collateral_base,
                        total_debt_base: data.total_debt_base,
                        available_borrows_base: data.available_borrows_base,
                        current_liquidation_threshold: data.current_liquidation_threshold,
                        ltv: data.ltv,
                        is_liquidatable: is_liq,
                        net_profit_estimate: if is_liq {
                            self.calculate_net_profit(&data)
                        } else {
                            U256::ZERO
                        },
                    };

                    results.push(health.clone());

                    if is_liq {
                        // Enrich with the borrower's largest collateral and debt
                        // reserve so the UI (and the manual tx builder) have the
                        // asset addresses, not just the borrower. Bounded so a
                        // slow data-provider read can never stall the poll loop.
                        let selection = match tokio::time::timeout(
                            std::time::Duration::from_secs(20),
                            self.resolve_reserves(addr),
                        )
                        .await
                        {
                            Ok(sel) => sel,
                            Err(_) => {
                                warn!("reserve resolution timed out for {addr}");
                                ReserveSelection::default()
                            }
                        };

                        info!(
                            "Borrower {} is liquidatable: HF={}, collateral={} (${:.2}), debt={} (${:.2})",
                            addr,
                            hf,
                            selection
                                .collateral_symbol
                                .as_deref()
                                .unwrap_or("?"),
                            selection.collateral_usd,
                            selection.debt_symbol.as_deref().unwrap_or("?"),
                            selection.debt_usd,
                        );

                        // Push into recent_opportunities for the API server.
                        let opportunity =
                            self.liquidation_opportunity(addr, &health, &selection);
                        let id = opportunity.id.clone();
                        self.recent_opportunities
                            .write()
                            .await
                            .insert(id, opportunity);
                    }
                }
                Err(e) => {
                    warn!("Failed to get account data for {}: {e}", addr);
                }
            }
        }

        Ok(results)
    }

    /// Reserve list (asset, symbol, decimals), fetched once and cached.
    ///
    /// Reserves are added to Aave rarely; a cached list avoids re-reading the
    /// whole catalogue on every liquidatable borrower. Decimals are read per
    /// asset via ERC-20 `decimals()` so USD ranking is exact rather than
    /// assuming 18.
    async fn reserves(&self) -> Result<Vec<ReserveMeta>> {
        if let Some(cached) = self.reserves_cache.read().await.as_ref() {
            return Ok(cached.clone());
        }

        let dp = IProtocolDataProvider::new(AAVE_PROTOCOL_DATA_PROVIDER, &self.provider);
        let tokens = dp.getAllReservesTokens().call().await?;

        // Read decimals concurrently (bounded) — one call per reserve.
        let metas: Vec<ReserveMeta> = stream::iter(tokens.into_iter())
            .map(|t| {
                let provider = self.provider.clone();
                async move {
                    let erc20 = IErc20Meta::new(t.tokenAddress, &provider);
                    let decimals = erc20.decimals().call().await.unwrap_or(18);
                    ReserveMeta {
                        asset: t.tokenAddress,
                        symbol: t.symbol,
                        decimals,
                    }
                }
            })
            .buffer_unordered(8)
            .collect()
            .await;

        *self.reserves_cache.write().await = Some(metas.clone());
        Ok(metas)
    }

    /// Find the borrower's single largest collateral and debt reserve by USD
    /// value. Returns an empty selection (all `None`) rather than erroring so a
    /// transient data-provider failure never aborts a poll.
    pub async fn resolve_reserves(&self, user: Address) -> ReserveSelection {
        let reserves = match self.reserves().await {
            Ok(r) => r,
            Err(e) => {
                warn!("reserve list unavailable for {user}: {e}");
                return ReserveSelection::default();
            }
        };

        let oracle = IAaveOracle::new(AAVE_V3_ORACLE, &self.provider);
        let dp = IProtocolDataProvider::new(AAVE_PROTOCOL_DATA_PROVIDER, &self.provider);

        // Per reserve: actual balances + oracle price, concurrently.
        let rows: Vec<(ReserveMeta, U256, U256, bool, U256)> = stream::iter(reserves.into_iter())
            .map(|meta| {
                let dp = dp.clone();
                let oracle = oracle.clone();
                async move {
                    let data = match dp.getUserReserveData(meta.asset, user).call().await {
                        Ok(d) => d,
                        Err(_) => return None,
                    };
                    let debt = data.currentVariableDebt.saturating_add(data.currentStableDebt);
                    // Skip the price read entirely for empty reserves.
                    if data.currentATokenBalance.is_zero() && debt.is_zero() {
                        return None;
                    }
                    let price = oracle.getAssetPrice(meta.asset).call().await.unwrap_or(U256::ZERO);
                    Some((
                        meta,
                        data.currentATokenBalance,
                        debt,
                        data.usageAsCollateralEnabled,
                        price,
                    ))
                }
            })
            .buffer_unordered(8)
            .filter_map(|x| async move { x })
            .collect()
            .await;

        let mut sel = ReserveSelection::default();
        for (meta, atoken, debt, is_collateral, price_8dec) in rows {
            let price_usd = u256_to_f64(price_8dec) / 1e8;
            let unit = 10f64.powi(meta.decimals as i32);

            if is_collateral && !atoken.is_zero() {
                let usd = u256_to_f64(atoken) / unit * price_usd;
                if usd > sel.collateral_usd {
                    sel.collateral_usd = usd;
                    sel.collateral = Some(meta.asset);
                    sel.collateral_symbol = Some(meta.symbol.clone());
                }
            }

            if !debt.is_zero() {
                let usd = u256_to_f64(debt) / unit * price_usd;
                if usd > sel.debt_usd {
                    sel.debt_usd = usd;
                    sel.debt_asset = Some(meta.asset);
                    sel.debt_symbol = Some(meta.symbol.clone());
                    sel.debt_amount = debt;
                }
            }
        }
        sel
    }

    /// Borrower health factor (1e18-scaled), or `None` on RPC failure. Used by
    /// the manual tx builder to apply the Aave close factor.
    pub async fn user_health_factor(&self, user: Address) -> Option<U256> {
        self.get_user_account_data(user)
            .await
            .ok()
            .map(|d| d.health_factor)
    }

    /// For each reserve the borrower uses as collateral, return
    /// `(asset, oracle_source, current_price_8dec)`. Used by the `--mock-hf`
    /// builder to crush every collateral source in the fork, not just the one
    /// being seized (a borrower with multiple collaterals needs all of them
    /// dropped for the health factor to fall).
    pub async fn collateral_price_sources(&self, user: Address) -> Vec<(Address, Address, U256)> {
        let reserves = match self.reserves().await {
            Ok(r) => r,
            Err(e) => {
                warn!("collateral_price_sources: reserve list unavailable: {e}");
                return Vec::new();
            }
        };
        let dp = IProtocolDataProvider::new(AAVE_PROTOCOL_DATA_PROVIDER, &self.provider);
        let oracle = IAaveOracle::new(AAVE_V3_ORACLE, &self.provider);

        // Lower concurrency + per-reserve retries: a flaky/rate-limited RPC was
        // silently dropping collaterals (`.ok()?`), leaving the mock-hf override
        // incomplete. Here a reserve is only skipped after it is CONFIRMED to
        // carry no collateral; a transient RPC error retries rather than drops.
        let out = stream::iter(reserves.into_iter())
            .map(|meta| {
                let dp = dp.clone();
                let oracle = oracle.clone();
                async move {
                    // getUserReserveData, retried.
                    let mut d = None;
                    for i in 0..5u64 {
                        match dp.getUserReserveData(meta.asset, user).call().await {
                            Ok(v) => {
                                d = Some(v);
                                break;
                            }
                            Err(_) => {
                                tokio::time::sleep(std::time::Duration::from_millis(150 * (i + 1)))
                                    .await
                            }
                        }
                    }
                    let d = d?;
                    if !d.usageAsCollateralEnabled || d.currentATokenBalance.is_zero() {
                        return None;
                    }
                    // This reserve IS collateral — resolve source + price, retried,
                    // so a transient RPC error never silently drops a real collateral.
                    let mut source = None;
                    for i in 0..5u64 {
                        match oracle.getSourceOfAsset(meta.asset).call().await {
                            Ok(v) => {
                                source = Some(v);
                                break;
                            }
                            Err(_) => {
                                tokio::time::sleep(std::time::Duration::from_millis(150 * (i + 1)))
                                    .await
                            }
                        }
                    }
                    let mut price = None;
                    for i in 0..5u64 {
                        match oracle.getAssetPrice(meta.asset).call().await {
                            Ok(v) => {
                                price = Some(v);
                                break;
                            }
                            Err(_) => {
                                tokio::time::sleep(std::time::Duration::from_millis(150 * (i + 1)))
                                    .await
                            }
                        }
                    }
                    let (source, price) = (source?, price?);
                    if price.is_zero() {
                        return None;
                    }
                    Some((meta.asset, source, price))
                }
            })
            // Low concurrency: a rate-limited RPC returning fewer collaterals is
            // worse than a slower-but-complete scan — a missed collateral makes
            // the mock-hf override insufficient.
            .buffer_unordered(2)
            .filter_map(|x| async move { x })
            .collect::<Vec<(Address, Address, U256)>>()
            .await;

        info!("collateral_price_sources({user:#x}): found {} collateral reserve(s)", out.len());
        out
    }

    /// Call Aave V3 Pool.getUserAccountData for a borrower.
    async fn get_user_account_data(&self, user: Address) -> Result<UserAccountData> {
        let pool = IPool::new(AAVE_V3_POOL, &self.provider);
        let data = pool.getUserAccountData(user).call().await?;

        Ok(UserAccountData {
            total_collateral_base: data.totalCollateralBase,
            total_debt_base: data.totalDebtBase,
            available_borrows_base: data.availableBorrowsBase,
            current_liquidation_threshold: data.currentLiquidationThreshold,
            ltv: data.ltv,
            health_factor: data.healthFactor,
        })
    }

    /// Build a `LiquidationOpportunity` for a liquidatable borrower, enriched
    /// with the reserve selection (largest collateral + debt asset).
    fn liquidation_opportunity(
        &self,
        borrower: Address,
        health: &BorrowerHealth,
        selection: &ReserveSelection,
    ) -> LiquidationOpportunity {
        let net_profit = health.net_profit_estimate;

        // `repay_amount` is now the actual debt owed on the chosen debt asset,
        // in that asset's own token units (what `debtToCover` must be). It falls
        // back to the USD-base total only when reserve resolution found nothing,
        // so the field is never silently empty.
        let repay_amount = if selection.debt_amount.is_zero() {
            health.total_debt_base
        } else {
            selection.debt_amount
        };

        let assets_summary = match (&selection.collateral_symbol, &selection.debt_symbol) {
            (Some(c), Some(d)) => format!(" · seize {c} / repay {d}"),
            _ => String::new(),
        };

        LiquidationOpportunity {
            id: format!("{borrower:#x}"),
            chain_id: 1,
            category: "liquidate".to_string(),
            to: AAVE_V3_POOL,
            from: self.operator_address.unwrap_or(Address::ZERO),
            value: U256::ZERO,
            gas_limit: FLASH_LIQ_GAS_UNITS,
            nonce: 0,
            calldata: Bytes::new(),
            gross_profit_wei: net_profit,
            borrower: Some(borrower),
            collateral: selection.collateral,
            debt_asset: selection.debt_asset,
            repay_amount: Some(repay_amount),
            selector: "0x".to_string(),
            summary: format!(
                "Aave V3 liquidation of {borrower:#x} (HF={}){assets_summary}",
                health.health_factor
            ),
        }
    }

    /// Calculate estimated net profit from liquidating this borrower.
    ///
    /// net_profit = collateral_seized_value - debt_to_cover - flash_loan_premium - gas_cost - slippage
    fn calculate_net_profit(&self, data: &UserAccountData) -> U256 {
        // Debt to cover: use the full debt base (conservative).
        let debt_to_cover = data.total_debt_base;

        // Flash loan premium: 0.05% of debt.
        let premium = debt_to_cover * U256::from(FLASHLOAN_PREMIUM_BPS) / U256::from(10_000);

        // Collateral seized: liquidation bonus applies to debt covered.
        let bonus = debt_to_cover * U256::from(LIQUIDATION_BONUS_BPS) / U256::from(10_000);
        let collateral_seized_gross = debt_to_cover + bonus;

        // Gas cost estimate (in wei): gas units * base fee (rough estimate 20 gwei).
        let gas_cost = U256::from(FLASH_LIQ_GAS_UNITS) * U256::from(20_000_000_000u128);

        // Slippage estimate: 0.5% of collateral value (conservative).
        let slippage = collateral_seized_gross * U256::from(5) / U256::from(1000);

        collateral_seized_gross
            .saturating_sub(debt_to_cover)
            .saturating_sub(premium)
            .saturating_sub(gas_cost)
            .saturating_sub(slippage)
    }
}

/// Best-effort `U256 -> f64` for USD ranking only. The decimal string route
/// matches `chainlink.rs`; magnitudes here (token balances, 8-dec prices) sit
/// far inside f64's range, and this value is never used for on-chain math.
fn u256_to_f64(v: U256) -> f64 {
    v.to_string().parse::<f64>().unwrap_or(f64::MAX)
}

/// Raw result from Aave V3 Pool.getUserAccountData.
#[derive(Debug, Clone)]
struct UserAccountData {
    total_collateral_base: U256,
    total_debt_base: U256,
    available_borrows_base: U256,
    current_liquidation_threshold: U256,
    ltv: U256,
    health_factor: U256,
}
