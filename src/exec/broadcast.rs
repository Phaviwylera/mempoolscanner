//! Private-relay broadcasting (Flashbots / builder relays).
//!
//! The public mempool is never used. If no private relay is configured the
//! transaction is reported as `Skipped("no_private_relay")`.

use crate::exec::ExecOutcome;
use alloy::primitives::{B256, Bytes};
use alloy::providers::{ext::MevApi, Provider, ProviderBuilder, DynProvider};
use alloy_rpc_types_mev::{EthBundleHash, EthSendBundle};
use alloy::signers::local::PrivateKeySigner as LocalSigner;
use anyhow::{anyhow, Result};
use std::time::Duration;
use url::Url;

/// Send a signed transaction to a private relay and wait for confirmations.
///
/// Primary path: Flashbots Protect / MEV-Share `eth_sendBundle` with
/// `with_auth(signer)`.
/// Secondary path: fan-out to each configured builder relay.
/// Returns `Ok(None)` when no private relay is available.
#[allow(clippy::too_many_arguments)]
pub async fn broadcast_and_confirm(
    rpc_url: &str,
    signer: &LocalSigner,
    encoded_tx: &[u8],
    tx_hash: B256,
    chain_id: u64,
    flashbots_relay: Option<&Url>,
    builder_relays: &[Url],
    confirmations: u64,
) -> Result<ExecOutcome> {
    let _ = chain_id;
    let provider = ProviderBuilder::new()
        .connect(rpc_url)
        .await
        .map_err(|e| anyhow!("rpc connect: {e}"))?
        .erased();
    let latest = provider
        .get_block_number()
        .await
        .map_err(|e| anyhow!("block number: {e}"))?;
    let target_block = latest.saturating_add(1);

    let bundle_hash = send_bundle_private(
        &provider,
        signer,
        encoded_tx,
        target_block,
        flashbots_relay,
        builder_relays,
    )
    .await?;

    let bundle_hash = match bundle_hash {
        Some(h) => h,
        None => {
            return Ok(ExecOutcome::Skipped {
                reason: "no_private_relay".to_string(),
            })
        }
    };
    let _ = bundle_hash;

    // Wait for `confirmations` blocks by polling the transaction receipt.
    let mined_block = latest;
    for _ in 0..120 {
        if let Some(receipt) = provider
            .get_transaction_receipt(tx_hash)
            .await
            .map_err(|e| anyhow!("receipt poll: {e}"))?
        {
            if let Some(block) = receipt.block_number {
                if block >= mined_block.saturating_add(confirmations) {
                    if receipt.status() {
                        return Ok(ExecOutcome::Confirmed { tx_hash, block });
                    } else {
                        return Ok(ExecOutcome::Reverted { tx_hash, block });
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    Err(anyhow!("confirmation timeout for {tx_hash:#x}"))
}

/// Submit the signed transaction to a private relay.
///
/// Returns the bundle hash on success, or `None` when no private relay is
/// configured / all attempts failed.
async fn send_bundle_private(
    _provider: &DynProvider,
    signer: &LocalSigner,
    encoded_tx: &[u8],
    target_block: u64,
    flashbots_relay: Option<&Url>,
    builder_relays: &[Url],
) -> Result<Option<B256>> {
    let encoded_tx = encoded_tx.to_vec();
    let bundle = move || EthSendBundle {
        txs: vec![Bytes::from(encoded_tx.clone())],
        block_number: target_block,
        ..Default::default()
    };

    // Primary: Flashbots Protect / MEV-Share with Flashbots auth signature.
    if let Some(relay) = flashbots_relay {
        let fb = ProviderBuilder::new()
            .connect(relay.as_str())
            .await
            .map_err(|e| anyhow!("flashbots connect: {e}"))?
            .erased();
        let res: Option<EthBundleHash> = fb
            .send_bundle(bundle())
            .with_auth(signer.clone())
            .await
            .map_err(|e| anyhow!("flashbots send_bundle: {e}"))?;
        return Ok(res.map(|h| h.bundle_hash));
    }

    // Secondary: fan-out to builder relays; stop on first acceptance.
    for relay in builder_relays {
        let Ok(fb) = ProviderBuilder::new().connect(relay.as_str()).await else {
            continue;
        };
        let fb = fb.erased();
        if let Ok(Some(h)) = fb.send_bundle(bundle()).await {
            return Ok(Some(h.bundle_hash));
        }
    }

    Ok(None)
}
