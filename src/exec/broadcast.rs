//! Private-relay broadcasting (Flashbots / builder relays).
//!
//! The public mempool is never used. If no private relay is configured the
//! transaction is reported as `Skipped("no_private_relay")`.

use crate::exec::ExecOutcome;
use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{B256, Bytes, U256};
use alloy::providers::{ext::MevApi, DynProvider, Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner as LocalSigner;
use alloy_rpc_types_mev::{
    BundleItem, EthBundleHash, EthCallBundle, EthSendBundle, MevSendBundle, ProtocolVersion,
};
use anyhow::{anyhow, Result};
use std::time::Duration;
use tracing::{info, warn};
use url::Url;

/// Send a signed transaction to a private relay and wait for confirmations.
///
/// Primary path: Flashbots Protect / MEV-Share `eth_sendBundle` with
/// `with_auth(signer)`. Secondary path: fan-out to each configured builder
/// relay. Returns `(Skipped("no_private_relay"), None)` when no relay is set.
///
/// Returns the terminal [`ExecOutcome`] plus the Flashbots bundle hash (when a
/// bundle was actually submitted) for the audit log.
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
) -> Result<(ExecOutcome, Option<B256>)> {
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
            return Ok((
                ExecOutcome::Skipped {
                    reason: "no_private_relay".to_string(),
                },
                None,
            ))
        }
    };

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
                        return Ok((ExecOutcome::Confirmed { tx_hash, block }, Some(bundle_hash)));
                    } else {
                        return Ok((ExecOutcome::Reverted { tx_hash, block }, Some(bundle_hash)));
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    Err(anyhow!("confirmation timeout for {tx_hash:#x}"))
}

/// Flashbots bundle simulation (`eth_callBundle`) — the zero-revert guarantee.
///
/// Simulates the signed bundle against the pending block on the Flashbots relay
/// and returns `false` if any transaction reverts or the bundle would pay the
/// builder nothing. A live broadcast must gate on this: it is what stops us
/// paying gas for a front-run or a bad swap.
pub async fn simulate_bundle(
    rpc_url: &str,
    relay: &Url,
    signer: &LocalSigner,
    encoded_tx: &[u8],
    min_profit: U256,
) -> Result<bool> {
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

    let fb = ProviderBuilder::new()
        .connect(relay.as_str())
        .await
        .map_err(|e| anyhow!("flashbots connect: {e}"))?
        .erased();

    let call = EthCallBundle {
        txs: vec![Bytes::from(encoded_tx.to_vec())],
        block_number: target_block,
        state_block_number: BlockNumberOrTag::Latest,
        ..Default::default()
    };

    let resp = match fb
        .call_bundle(call)
        .with_auth(signer.clone())
        .await
        .map_err(|e| anyhow!("flashbots call_bundle: {e}"))?
    {
        Some(r) => r,
        None => {
            // Relay returned no simulation result — fail closed (do not send).
            warn!("bundle sim: relay returned no response");
            return Ok(false);
        }
    };

    // Zero-revert guarantee: any tx-level revert fails the whole bundle. A
    // reverting call populates `revert` with the revert data.
    for r in &resp.results {
        if r.revert.is_some() {
            warn!("bundle sim: tx reverted (revert={:?})", r.revert);
            return Ok(false);
        }
    }

    // `coinbase_diff` is what the builder would receive this block. If it is
    // zero the bundle pays nothing and will not be included.
    if resp.coinbase_diff.is_zero() {
        warn!("bundle sim: zero coinbase_diff — builder would be paid nothing");
        return Ok(false);
    }

    let _ = min_profit; // profit floor is enforced pre-sign by plan_bid.
    info!(
        "bundle sim OK: coinbase_diff={} gas_used={}",
        resp.coinbase_diff, resp.total_gas_used
    );
    Ok(true)
}

/// Submit a MEV-Share back-run bundle via `mev_sendBundle`: the SVR update
/// transaction referenced **by hash** (the relay substitutes the real tx), then
/// our liquidation transaction. The 90%-rule bribe is already baked into the
/// liquidation tx's priority fee by `gas::plan_bid` before signing.
///
/// Returns the bundle hash on success.
pub async fn submit_mev_share_bundle(
    relay: &Url,
    signer: &LocalSigner,
    svr_hint_tx: B256,
    encoded_liq_tx: &[u8],
    rpc_url: &str,
) -> Result<Option<B256>> {
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

    let fb = ProviderBuilder::new()
        .connect(relay.as_str())
        .await
        .map_err(|e| anyhow!("flashbots connect: {e}"))?
        .erased();

    let body = vec![
        // tx[0]: the SVR oracle update, back-run by hash reference (the relay
        // substitutes the real transaction).
        BundleItem::Hash { hash: svr_hint_tx },
        // tx[1]: our liquidation — must land, so canRevert = false.
        BundleItem::Tx {
            tx: Bytes::from(encoded_liq_tx.to_vec()),
            can_revert: false,
        },
    ];
    let bundle = MevSendBundle::new(
        target_block,
        Some(target_block + 2),
        ProtocolVersion::V0_1,
        body,
    );

    let resp: Option<EthBundleHash> = fb
        .send_mev_bundle(bundle)
        .with_auth(signer.clone())
        .await
        .map_err(|e| anyhow!("mev_sendBundle: {e}"))?;
    Ok(resp.map(|h| h.bundle_hash))
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
