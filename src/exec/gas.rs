//! Gas-aware dynamic bidding for live liquidation bundles — the "90% rule".
//!
//! The economics are a pure function, [`plan_bid`], so they are unit-testable
//! with no RPC. The only I/O is [`fetch_fees`], which reads the current base
//! fee and a suggested priority fee from the node.
//!
//! The model (priority-fee bribe, no contract change):
//!
//! ```text
//! gas_cost   = gas_limit * (base_fee + priority_floor)
//! surplus    = gross_profit - gas_cost
//! bribe      = surplus * BID_FRACTION          (paid to the builder as extra tip)
//! net        = surplus - bribe                 (what we keep)
//! ```
//!
//! We abort unless `gross_profit >= max(min_profit, gas_cost + buffer)` **and**
//! `net >= min_profit`, so a losing bundle is never signed or sent.

use alloy::primitives::U256;
use alloy::providers::{Provider, ProviderBuilder};
use anyhow::{anyhow, Result};

/// A live network fee snapshot, wei per gas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeeEnvelope {
    pub base_fee_per_gas: u128,
    pub priority_fee_per_gas: u128,
}

/// Read the latest block's base fee and a suggested priority fee from the RPC.
///
/// The target block is `latest + 1`, whose base fee can differ from `latest`'s
/// by at most 1/8; [`plan_bid`] adds generous headroom to `max_fee_per_gas` so
/// that drift never invalidates the bundle.
pub async fn fetch_fees(rpc_url: &str) -> Result<FeeEnvelope> {
    use alloy::eips::BlockNumberOrTag;
    let provider = ProviderBuilder::new()
        .connect(rpc_url)
        .await
        .map_err(|e| anyhow!("rpc connect: {e}"))?
        .erased();

    let base_fee_per_gas = match provider
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await
    {
        Ok(Some(b)) => b.header.base_fee_per_gas.unwrap_or(0) as u128,
        Ok(None) => return Err(anyhow!("no latest block for base fee")),
        Err(e) => return Err(anyhow!("get latest block: {e}")),
    };

    let priority_fee_per_gas = provider
        .get_max_priority_fee_per_gas()
        .await
        .map_err(|e| anyhow!("max priority fee: {e}"))?;

    Ok(FeeEnvelope {
        base_fee_per_gas,
        priority_fee_per_gas,
    })
}

/// `BID_FRACTION` env (0.0–1.0) as basis points; default 9000 (90%).
pub fn bid_fraction_bps_from_env() -> u32 {
    std::env::var("BID_FRACTION")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .map(|f| (f.clamp(0.0, 1.0) * 10_000.0).round() as u32)
        .unwrap_or(9_000)
}

/// `EXEC_GAS_SAFETY_BUFFER_WEI` env; default 0.
pub fn safety_buffer_from_env() -> U256 {
    std::env::var("EXEC_GAS_SAFETY_BUFFER_WEI")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(U256::ZERO)
}

/// The computed bid for one liquidation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BidPlan {
    pub gas_cost_wei: U256,
    pub builder_bribe_wei: U256,
    pub net_profit_wei: U256,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
}

/// Either a priced, profitable bid, or an abort with a human-readable reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BidDecision {
    Execute(BidPlan),
    Abort(String),
}

fn u256_to_u128_sat(v: U256) -> u128 {
    u128::try_from(v).unwrap_or(u128::MAX)
}

/// Pure "90% rule" economics. No I/O — fully unit-testable.
///
/// * `gross_profit_wei`  — payout before gas and bribe (the simulated gross).
/// * `gas_limit`         — the tx gas limit (an upper bound on actual usage).
/// * `fees`              — live base fee + suggested priority fee.
/// * `bid_fraction_bps`  — builder's share of the surplus, in basis points.
/// * `min_profit_wei`    — the floor we must still keep after the bribe.
/// * `safety_buffer_wei` — added to gas cost for the coverage gate.
pub fn plan_bid(
    gross_profit_wei: U256,
    gas_limit: u64,
    fees: FeeEnvelope,
    bid_fraction_bps: u32,
    min_profit_wei: U256,
    safety_buffer_wei: U256,
) -> BidDecision {
    let gl = U256::from(gas_limit);
    let per_gas = U256::from(fees.base_fee_per_gas).saturating_add(U256::from(fees.priority_fee_per_gas));
    let gas_cost_wei = gl.saturating_mul(per_gas);

    // Coverage gate: must clear gas + buffer, and the configured floor.
    let effective_min = (gas_cost_wei.saturating_add(safety_buffer_wei)).max(min_profit_wei);
    if gross_profit_wei < effective_min {
        return BidDecision::Abort(format!(
            "gross {gross_profit_wei} wei below effective min {effective_min} (gas_cost {gas_cost_wei})"
        ));
    }

    // Split the surplus: builder gets bid_fraction, we keep the remainder.
    let surplus = gross_profit_wei.saturating_sub(gas_cost_wei);
    let builder_bribe_wei =
        surplus.saturating_mul(U256::from(bid_fraction_bps)) / U256::from(10_000u32);
    let net_profit_wei = surplus.saturating_sub(builder_bribe_wei);

    if net_profit_wei < min_profit_wei {
        return BidDecision::Abort(format!(
            "net {net_profit_wei} wei below min_profit {min_profit_wei} after {builder_bribe_wei} bribe"
        ));
    }

    // Pay the bribe as extra priority on top of the network floor. Actual gas
    // used <= gas_limit, so the realised bribe is <= target (never an overpay).
    let extra_tip_per_gas = if gas_limit == 0 {
        U256::ZERO
    } else {
        builder_bribe_wei / gl
    };
    let max_priority_fee_per_gas = fees
        .priority_fee_per_gas
        .saturating_add(u256_to_u128_sat(extra_tip_per_gas));
    // Generous base-fee headroom — a bundle only ever pays the actual base fee,
    // so overshooting max_fee is free and protects against base-fee drift.
    let max_fee_per_gas = fees
        .base_fee_per_gas
        .saturating_mul(2)
        .saturating_add(max_priority_fee_per_gas);

    BidDecision::Execute(BidPlan {
        gas_cost_wei,
        builder_bribe_wei,
        net_profit_wei,
        max_fee_per_gas,
        max_priority_fee_per_gas,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fees(base: u128, prio: u128) -> FeeEnvelope {
        FeeEnvelope { base_fee_per_gas: base, priority_fee_per_gas: prio }
    }

    #[test]
    fn ninety_percent_split_and_fee_wiring() {
        // gas_limit 1_000_000, base 20 gwei, prio 1 gwei.
        let gl = 1_000_000u64;
        let f = fees(20_000_000_000, 1_000_000_000);
        // gas_cost = 1e6 * 21e9 = 2.1e16
        let gas_cost = U256::from(21_000_000_000_000_000u128);
        // gross = gas_cost + 1e18 surplus
        let surplus = U256::from(1_000_000_000_000_000_000u128);
        let gross = gas_cost + surplus;
        match plan_bid(gross, gl, f, 9_000, U256::ZERO, U256::ZERO) {
            BidDecision::Execute(p) => {
                assert_eq!(p.gas_cost_wei, gas_cost);
                // bribe = 90% of surplus, net = 10%.
                assert_eq!(p.builder_bribe_wei, surplus * U256::from(9u8) / U256::from(10u8));
                assert_eq!(p.net_profit_wei, surplus - p.builder_bribe_wei);
                // priority = floor + bribe/gas_limit.
                let extra = u256_to_u128_sat(p.builder_bribe_wei / U256::from(gl));
                assert_eq!(p.max_priority_fee_per_gas, 1_000_000_000 + extra);
                assert!(p.max_fee_per_gas > p.max_priority_fee_per_gas);
            }
            BidDecision::Abort(r) => panic!("should execute: {r}"),
        }
    }

    #[test]
    fn aborts_when_gross_below_gas_plus_buffer() {
        let f = fees(20_000_000_000, 1_000_000_000);
        let buffer = U256::from(1_000_000_000_000_000u128); // 0.001 ETH
        // gross barely above gas but below gas+buffer.
        let gas_cost = U256::from(21_000_000_000_000_000u128);
        let gross = gas_cost + U256::from(1u8);
        assert!(matches!(
            plan_bid(gross, 1_000_000, f, 9_000, U256::ZERO, buffer),
            BidDecision::Abort(_)
        ));
    }

    #[test]
    fn aborts_when_net_below_min_profit() {
        let f = fees(20_000_000_000, 1_000_000_000);
        let gas_cost = U256::from(21_000_000_000_000_000u128);
        let surplus = U256::from(1_000_000_000_000_000_000u128);
        let gross = gas_cost + surplus;
        // net would be 10% of surplus = 1e17; require more than that.
        let min = U256::from(2_000_000_000_000_000_000u128);
        assert!(matches!(
            plan_bid(gross, 1_000_000, f, 9_000, min, U256::ZERO),
            BidDecision::Abort(_)
        ));
    }

    #[test]
    fn bid_fraction_env_defaults_to_ninety() {
        // Not setting the env var -> 9000 bps.
        assert_eq!(bid_fraction_bps_from_env(), 9_000);
    }
}
