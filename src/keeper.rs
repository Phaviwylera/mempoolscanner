//! Aave V3 keeper — polls borrower health factors on Pyth price updates.
//!
//! When the oracle publishes a new ETH/asset price, the keeper re-checks all
//! tracked borrowers via Aave V3 `Pool.getUserAccountData`. Borrowers with
//! health factor < 1.0 are flagged as liquidatable and pushed into the
//! shared `recent_opportunities` map so the API server can surface them.

use crate::exec::LiquidationOpportunity;
use alloy::primitives::{address, Address, U256};
use alloy::providers::{Provider, ProviderBuilder, RootProvider};
use alloy::sol_types::SolValue;
use alloy::transports::http::{Client, Http};
use alloy::transports::layers::RetryMiddleware;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

// Aave V3 Ethereum Mainnet Pool address.
pub const AAVE_V3_POOL: Address = address!("87870bca3f3fd6335c3f4ce8392d69350b4fa4e2");

// Aave V3 governance flash-loan premium: 5 bps (0.05%).
pub const FLASHLOAN_PREMIUM_BPS: u64 = 5;

// Standard Aave V3 liquidation bonus: 500 bps (5.00%).
pub const LIQUIDATION_BONUS_BPS: u64 = 500;

// Estimated gas units for atomic flash loan + liquidation + swap.
pub const FLASH_LIQ_GAS_UNITS: u64 = 380_000;

// Type alias for the HTTP provider used throughout the keeper.
type HttpProvider = RootProvider<Http<RetryMiddleware<Client>>>;

mod aave_pool {
    alloy::sol!(
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
    );
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
    rpc_url: String,
    provider: HttpProvider,
    pool: alloy::contract::Contract<HttpProvider>,
    recent_opportunities: Arc<RwLock<HashMap<String, LiquidationOpportunity>>>,
    tracked_borrowers: Arc<RwLock<Vec<Address>>>,
    operator_address: Option<Address>,
}

impl Keeper {
    /// Create a new keeper.
    pub async fn new(
        rpc_url: &str,
        recent_opportunities: Arc<RwLock<HashMap<String, LiquidationOpportunity>>>,
    ) -> Result<Self> {
        let provider: HttpProvider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
        let pool = alloy::contract::Contract::new(
            AAVE_V3_POOL,
            aave_pool::IPool::SOL_INTERFACE,
            provider.clone(),
        );

        Ok(Self {
            rpc_url: rpc_url.to_string(),
            provider,
            pool,
            recent_opportunities,
            tracked_borrowers: Arc::new(RwLock::new(Vec::new())),
            operator_address: None,
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
        // Re-poll on demand for fresh data.
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
                        info!(
                            "Borrower {} is liquidatable: HF={}, debt={}, collateral={}",
                            addr,
                            hf,
                            data.total_debt_base,
                            data.total_collateral_base
                        );

                        // Push into recent_opportunities for the API server.
                        let opportunity = self.liquidation_opportunity(addr, &health);
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

    /// Call Aave V3 Pool.getUserAccountData for a borrower.
    async fn get_user_account_data(&self, user: Address) -> Result<UserAccountData> {
        let result: (U256, U256, U256, U256, U256, U256) = self
            .pool
            .method("getUserAccountData", user)?
            .call()
            .await?;

        Ok(UserAccountData {
            total_collateral_base: result.0,
            total_debt_base: result.1,
            available_borrows_base: result.2,
            current_liquidation_threshold: result.3,
            ltv: result.4,
            health_factor: result.5,
        })
    }

    /// Build a `LiquidationOpportunity` for a liquidatable borrower.
    fn liquidation_opportunity(&self, borrower: Address, health: &BorrowerHealth) -> LiquidationOpportunity {
        let net_profit = health.net_profit_estimate;
        let debt_to_cover = health.total_debt_base;

        LiquidationOpportunity {
            id: format!("{borrower:#x}"),
            chain_id: 1,
            category: "liquidate".to_string(),
            to: AAVE_V3_POOL,
            from: self.operator_address.unwrap_or(Address::ZERO),
            value: U256::ZERO,
            gas_limit: FLASH_LIQ_GAS_UNITS,
            nonce: 0,
            calldata: alloy::primitives::Bytes::new(),
            gross_profit_wei: net_profit,
            borrower: Some(borrower),
            collateral: None,
            debt_asset: None,
            repay_amount: Some(debt_to_cover),
            selector: "0x".to_string(),
            summary: format!("Aave V3 liquidation of {borrower:#x} (HF={})", health.health_factor),
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

        let net = collateral_seized_gross
            .saturating_sub(debt_to_cover)
            .saturating_sub(premium)
            .saturating_sub(gas_cost)
            .saturating_sub(slippage);

        net
    }
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
