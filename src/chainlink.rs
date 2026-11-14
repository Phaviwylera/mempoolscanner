//! Chainlink ETH/USD price feed reader with a 60-second cache.
//!
//! Replaces the hardcoded `fx.eth_usd = 2650.0` reference value in the API.
//! The feed is read over plain HTTP JSON-RPC; one call every 60s at most, so
//! polling the dashboard every 5 seconds cannot spam the upstream node.
//!
//! This is a *read*. Nothing here signs or broadcasts.

use alloy::primitives::{address, Address};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::sol;
use anyhow::{anyhow, Result};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::warn;

/// Chainlink ETH/USD aggregator, Ethereum mainnet.
pub const CHAINLINK_ETH_USD: Address = address!("5f4eC3Df9cbd43714FE2740f5E3616155c5b8419");

/// The ETH/USD feed reports with 8 decimals.
pub const FEED_DECIMALS: i32 = 8;

/// How long a reading stays fresh.
const TTL: Duration = Duration::from_secs(60);

sol! {
    #[sol(rpc)]
    interface AggregatorV3Interface {
        function latestRoundData() external view returns (
            uint80 roundId,
            int256 answer,
            uint256 startedAt,
            uint256 updatedAt,
            uint80 answeredInRound
        );
        function decimals() external view returns (uint8);
    }
}

/// A single ETH/USD reading.
#[derive(Clone, Debug)]
pub struct EthUsdQuote {
    /// Price in USD, already scaled down by `FEED_DECIMALS`.
    pub price_usd: f64,
    /// The feed's own `updatedAt` (unix seconds), not our fetch time.
    pub feed_updated_at: u64,
    pub round_id: String,
    /// True when this value came from cache after a failed refresh.
    pub stale: bool,
}

struct Cached {
    quote: EthUsdQuote,
    fetched: Instant,
}

/// Cached Chainlink ETH/USD reader.
pub struct ChainlinkEthUsd {
    provider: DynProvider,
    cache: RwLock<Option<Cached>>,
}

impl ChainlinkEthUsd {
    /// Build a reader against an HTTP RPC endpoint (e.g. `settings.state_rpc`).
    pub fn new(rpc_url: &str) -> Result<Self> {
        let provider = ProviderBuilder::new()
            .connect_http(rpc_url.parse()?)
            .erased();
        Ok(Self {
            provider,
            cache: RwLock::new(None),
        })
    }

    /// Cached price. `None` only when the feed has never been read successfully,
    /// so a caller that gets `None` must say so rather than invent a number.
    pub async fn quote(&self) -> Option<EthUsdQuote> {
        if let Some(c) = self.cache.read().await.as_ref() {
            if c.fetched.elapsed() < TTL {
                return Some(c.quote.clone());
            }
        }

        match self.fetch().await {
            Ok(quote) => {
                *self.cache.write().await = Some(Cached {
                    quote: quote.clone(),
                    fetched: Instant::now(),
                });
                Some(quote)
            }
            Err(e) => {
                warn!("chainlink ETH/USD read failed: {e}");
                // Serve the last good value, flagged stale, rather than nothing.
                self.cache.read().await.as_ref().map(|c| {
                    let mut q = c.quote.clone();
                    q.stale = true;
                    q
                })
            }
        }
    }

    /// Uncached read of `latestRoundData()`.
    async fn fetch(&self) -> Result<EthUsdQuote> {
        let feed = AggregatorV3Interface::new(CHAINLINK_ETH_USD, &self.provider);
        let round = feed.latestRoundData().call().await?;

        // `answer` is int256. A non-positive answer means a broken or paused
        // feed; treat it as an error instead of publishing a bogus price.
        if round.answer.is_negative() || round.answer.is_zero() {
            return Err(anyhow!("non-positive answer: {}", round.answer));
        }

        // Decimal-string -> f64 avoids juggling I256 arithmetic. The feed's
        // 8-decimal range is far inside f64's exact-integer window.
        let scaled: f64 = round
            .answer
            .to_string()
            .parse()
            .map_err(|e| anyhow!("unparseable answer {}: {e}", round.answer))?;

        Ok(EthUsdQuote {
            price_usd: scaled / 10f64.powi(FEED_DECIMALS),
            feed_updated_at: round.updatedAt.to_string().parse().unwrap_or(0),
            round_id: round.roundId.to_string(),
            stale: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_address_matches_chainlink_eth_usd() {
        assert_eq!(
            CHAINLINK_ETH_USD.to_string().to_lowercase(),
            "0x5f4ec3df9cbd43714fe2740f5e3616155c5b8419"
        );
    }

    #[test]
    fn eight_decimal_scaling_is_correct() {
        // 265012345678 raw == $2650.12345678
        let raw: f64 = 265_012_345_678.0;
        let scaled = raw / 10f64.powi(FEED_DECIMALS);
        assert!((scaled - 2_650.123_456_78).abs() < 1e-6, "got {scaled}");
    }

    #[test]
    fn builder_rejects_a_malformed_url() {
        assert!(ChainlinkEthUsd::new("not a url").is_err());
    }
}
