//! Pyth price oracle WebSocket client.
//!
//! Connects to Pyth's Hermes WebSocket (`wss://hermes.pyth.network`) and
//! subscribes to configured price feeds. When a price update arrives, the
//! oracle invokes a callback to trigger the keeper's health factor check.
//!
//! No mempool scanning is performed — this is purely an oracle-triggered
//! liquidation keeper.

use alloy::primitives::U256;
use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

/// Price update received from Pyth.
#[derive(Clone, Debug, Deserialize)]
pub struct PriceUpdate {
    pub price_feed_id: String,
    pub price: U256,
    pub expo: i32,
    pub publish_time: u64,
    pub confidence: U256,
}

/// Pyth price feed subscription message.
#[derive(Clone, Debug, serde::Serialize)]
struct PythSubscription {
    #[serde(rename = "type")]
    msg_type: String,
    data: Vec<PythFeedData>,
}

/// Pyth feed data for subscription.
#[derive(Clone, Debug, serde::Serialize)]
struct PythFeedData {
    feed: String,
}

/// Pyth oracle client.
pub struct PythOracle {
    ws_url: String,
    feed_ids: Vec<String>,
    on_price_update: Option<Arc<dyn Fn(PriceUpdate) + Send + Sync>>,
}

impl PythOracle {
    /// Create a new Pyth oracle client.
    pub fn new(ws_url: &str, feed_ids: Vec<String>) -> Self {
        Self {
            ws_url: ws_url.to_string(),
            feed_ids,
            on_price_update: None,
        }
    }

    /// Set a callback invoked on every price update.
    pub fn on_price_update<F>(&mut self, callback: F)
    where
        F: Fn(PriceUpdate) + Send + Sync + 'static,
    {
        self.on_price_update = Some(Arc::new(callback));
    }

    /// Run the WebSocket client loop. Reconnects with backoff on disconnect.
    pub async fn run(self) -> Result<()> {
        let subscribe_msg = PythSubscription {
            msg_type: "subscribe".to_string(),
            data: self
                .feed_ids
                .iter()
                .map(|f| PythFeedData { feed: f.clone() })
                .collect(),
        };
        let subscribe_payload = serde_json::to_string(&subscribe_msg)?;

        let on_price_update = self.on_price_update.clone();

        loop {
            match connect_async(&self.ws_url).await {
                Ok((ws_stream, _)) => {
                    info!("Connected to Pyth WebSocket at {}", self.ws_url);
                    let (mut sink, mut stream) = ws_stream.split();

                    // Send subscription.
                    if let Err(e) = sink.send(Message::Text(subscribe_payload.clone())).await {
                        warn!("Failed to send Pyth subscription: {e}");
                        continue;
                    }

                    // Read messages until disconnect.
                    while let Some(msg) = stream.next().await {
                        match msg {
                            Ok(Message::Text(text)) => {
                                if let Err(e) = Self::handle_message(&text, &on_price_update) {
                                    warn!("Error handling Pyth message: {e}");
                                }
                            }
                            Ok(Message::Close(..)) => {
                                info!("Pyth WebSocket closed by server");
                                break;
                            }
                            Err(e) => {
                                warn!("Pyth WebSocket error: {e}");
                                break;
                            }
                            _ => {}
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to connect to Pyth WebSocket: {e}; retrying in 5s");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    }

    /// Handle a single Pyth WebSocket message.
    fn handle_message(
        text: &str,
        on_price_update: &Option<Arc<dyn Fn(PriceUpdate) + Send + Sync>>,
    ) -> Result<()> {
        let value: serde_json::Value = serde_json::from_str(text)?;

        // Pyth v2 price update format has a "ptype" field.
        let ptype = value.get("ptype").and_then(|v| v.as_str()).unwrap_or("");
        if ptype != "price" {
            return Ok(()); // ignore non-price messages (heartbeats, etc.)
        }

        let feed_id = value
            .get("price_feed_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let price_str = value.get("price").and_then(|v| v.as_str()).unwrap_or("0");
        let price = U256::from_str_radix(
            price_str.strip_prefix("0x").unwrap_or(price_str),
            16,
        )
        .unwrap_or(U256::ZERO);

        let expo = value.get("expo").and_then(|v| v.as_i64()).unwrap_or(-8) as i32;
        let publish_time = value
            .get("publish_time")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let confidence_str = value
            .get("confidence")
            .and_then(|v| v.as_str())
            .unwrap_or("0");
        let confidence = U256::from_str_radix(
            confidence_str.strip_prefix("0x").unwrap_or(confidence_str),
            16,
        )
        .unwrap_or(U256::ZERO);

        let update = PriceUpdate {
            price_feed_id: feed_id,
            price,
            expo,
            publish_time,
            confidence,
        };

        if let Some(cb) = on_price_update {
            cb(update);
        }

        Ok(())
    }
}
