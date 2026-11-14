//! Integration tests for the opt-in execution module.
//!
//! `#[ignore]` by default: only runs when `SEPOLIA_RPC_URL` is set. It asserts
//! the simulation gate rejects a deliberately unprofitable opportunity and that
//! the module never targets mainnet.

use mempool_scanner::exec::{simulate_with_backend, ExecOutcome, LiquidationOpportunity};
use mempool_scanner::state::InMemoryStateBackend;
use alloy::primitives::{address, Bytes, ChainId, U256};
use alloy::providers::{Provider, ProviderBuilder};
use std::sync::Arc;

#[tokio::test]
#[ignore]
async fn sepolia_gate_rejects_unprofitable_and_never_mainnet() {
    let rpc = std::env::var("SEPOLIA_RPC_URL").expect("SEPOLIA_RPC_URL is required to run this test");

    // Fail fast if we are ever pointed at mainnet.
    let provider = ProviderBuilder::new()
        .connect(&rpc)
        .await
        .expect("connect to SEPOLIA_RPC_URL");
    let chain_id = provider.get_chain_id().await.expect("read chain id");
    assert_ne!(chain_id, ChainId::from(1u64), "refusing to target Ethereum mainnet");
    assert_eq!(chain_id, ChainId::from(11155111u64), "expected sepolia (chain id 11155111)");
    drop(provider);

    // Gate: a deliberately unprofitable opportunity (zero gross) must be
    // rejected by the simulation gate. No signer and no network needed here
    // because we simulate against the in-memory backend directly.
    let backend = InMemoryStateBackend::default();
    let target = address!("aaaa000000000000000000000000000000000001");
    backend.insert_account(target, U256::ZERO, Some(vec![0x00])); // STOP: call succeeds
    let backend: Arc<dyn mempool_scanner::state::StateBackend> = Arc::new(backend);

    let opportunity = LiquidationOpportunity {
        id: "0xtest-sepolia".to_string(),
        chain_id: 11155111,
        category: "liquidate".to_string(),
        to: target,
        from: address!("bbbb000000000000000000000000000000000002"),
        value: U256::ZERO,
        gas_limit: 500_000,
        nonce: 0,
        calldata: Bytes::from(vec![]),
        gross_profit_wei: U256::ZERO, // deliberately unprofitable
        borrower: None,
        collateral: None,
        debt_asset: None,
        repay_amount: None,
        selector: "0x00000000".to_string(),
        summary: "unprofitable sepolia liquidation".to_string(),
    };

    let outcome = simulate_with_backend(
        backend,
        1,
        &opportunity,
        U256::from(1_000_000_000_000_000_000u128), // 1 ETH min profit
        U256::ZERO,
    )
    .await
    .expect("simulation gate must return");

    assert!(
        matches!(outcome, ExecOutcome::Skipped { .. }),
        "gate must reject unprofitable opportunity, got {outcome:?}"
    );
}
