//! Stage 2 & Stage 3: Selector registry, Alloy typed ABI calldata decoder,
//! recipient-substitution rewriter for generalized front-run probing, and revert decoder.

use alloy::primitives::{keccak256, Address, Bytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::LazyLock;

// Separate tiny modules avoid overloaded Solidity name collisions in `sol!`.
mod defs {
    use alloy::sol;

    pub mod claim_0 {
        use super::sol;
        sol! { function claim() external; }
    }
    pub mod claim_u256 {
        use super::sol;
        sol! { function claim(uint256 amount) external; }
    }
    pub mod claim_addr {
        use super::sol;
        sol! { function claim(address account) external; }
    }
    pub mod claim_addr_u256 {
        use super::sol;
        sol! { function claim(address account, uint256 amount) external; }
    }
    pub mod claim_merkle {
        use super::sol;
        sol! { function claim(address account, uint256 amount, bytes32[] merkleProof) external; }
    }
    pub mod claim_merkle_indexed {
        use super::sol;
        sol! { function claim(uint256 index, address account, uint256 amount, bytes32[] merkleProof) external; }
    }
    pub mod claim_merkl_batch {
        use super::sol;
        sol! { function claim(address[] users, address[] tokens, uint256[] amounts, bytes32[][] proofs) external; }
    }
    pub mod withdraw_u256 {
        use super::sol;
        sol! { function withdraw(uint256 amount) external; }
    }
    pub mod withdraw_0 {
        use super::sol;
        sol! { function withdraw() external; }
    }
    pub mod aave_withdraw {
        use super::sol;
        sol! { function withdraw(address asset, uint256 amount, address to) external; }
    }
    pub mod erc4626_redeem {
        use super::sol;
        sol! { function redeem(uint256 shares, address receiver, address owner) external; }
    }
    pub mod universal_execute_deadline {
        use super::sol;
        sol! { function execute(bytes commands, bytes[] inputs, uint256 deadline) external; }
    }
    pub mod universal_execute {
        use super::sol;
        sol! { function execute(bytes commands, bytes[] inputs) external; }
    }
    pub mod v2_swaps {
        use super::sol;
        sol! {
            function swapExactTokensForTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline) external;
            function swapTokensForExactTokens(uint256 amountOut, uint256 amountInMax, address[] path, address to, uint256 deadline) external;
            function swapExactETHForTokens(uint256 amountOutMin, address[] path, address to, uint256 deadline) external;
            function swapTokensForExactETH(uint256 amountOut, uint256 amountInMax, address[] path, address to, uint256 deadline) external;
            function swapExactTokensForETH(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline) external;
            function swapETHForExactTokens(uint256 amountOut, address[] path, address to, uint256 deadline) external;
            function swapExactTokensForTokensSupportingFeeOnTransferTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline) external;
            function swapExactETHForTokensSupportingFeeOnTransferTokens(uint256 amountOutMin, address[] path, address to, uint256 deadline) external;
            function swapExactTokensForETHSupportingFeeOnTransferTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline) external;
        }
    }
    pub mod v3_router_v1 {
        use super::sol;
        sol! {
            struct ExactInputSingleParams {
                address tokenIn;
                address tokenOut;
                uint24 fee;
                address recipient;
                uint256 deadline;
                uint256 amountIn;
                uint256 amountOutMinimum;
                uint160 sqrtPriceLimitX96;
            }
            struct ExactInputParams {
                bytes path;
                address recipient;
                uint256 deadline;
                uint256 amountIn;
                uint256 amountOutMinimum;
            }
            struct ExactOutputSingleParams {
                address tokenIn;
                address tokenOut;
                uint24 fee;
                address recipient;
                uint256 deadline;
                uint256 amountOut;
                uint256 amountInMaximum;
                uint160 sqrtPriceLimitX96;
            }
            struct ExactOutputParams {
                bytes path;
                address recipient;
                uint256 deadline;
                uint256 amountOut;
                uint256 amountInMaximum;
            }
            function exactInputSingle(ExactInputSingleParams params) external;
            function exactInput(ExactInputParams params) external;
            function exactOutputSingle(ExactOutputSingleParams params) external;
            function exactOutput(ExactOutputParams params) external;
        }
    }
    pub mod v3_router_v2 {
        use super::sol;
        sol! {
            struct ExactInputSingleParamsV2 {
                address tokenIn;
                address tokenOut;
                uint24 fee;
                address recipient;
                uint256 amountIn;
                uint256 amountOutMinimum;
                uint160 sqrtPriceLimitX96;
            }
            struct ExactInputParamsV2 {
                bytes path;
                address recipient;
                uint256 amountIn;
                uint256 amountOutMinimum;
            }
            struct ExactOutputSingleParamsV2 {
                address tokenIn;
                address tokenOut;
                uint24 fee;
                address recipient;
                uint256 amountOut;
                uint256 amountInMaximum;
                uint160 sqrtPriceLimitX96;
            }
            struct ExactOutputParamsV2 {
                bytes path;
                address recipient;
                uint256 amountOut;
                uint256 amountInMaximum;
            }
            function exactInputSingle(ExactInputSingleParamsV2 params) external;
            function exactInput(ExactInputParamsV2 params) external;
            function exactOutputSingle(ExactOutputSingleParamsV2 params) external;
            function exactOutput(ExactOutputParamsV2 params) external;
        }
    }
    pub mod v3_pool {
        use super::sol;
        sol! {
            function swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160 sqrtPriceLimitX96, bytes data) external;
        }
    }
    pub mod liquidations {
        use super::sol;
        sol! {
            function liquidate(address borrower, address collateral) external;
            function liquidateBorrow(address borrower, uint256 repayAmount, address cTokenCollateral) external;
            function liquidationCall(address collateralAsset, address debtAsset, address user, uint256 debtToCover, bool receiveAToken) external;
        }
    }
}

sol! {
    error Error(string reason);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Claim,
    Withdraw,
    Swap,
    Liquidate,
}

impl Category {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claim => "claim",
            Self::Withdraw => "withdraw",
            Self::Swap => "swap",
            Self::Liquidate => "liquidate",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct SelectorSpec {
    pub selector: [u8; 4],
    pub selector_hex: String,
    pub name: &'static str,
    pub signature: &'static str,
    pub category: Category,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DecodedCall {
    pub signature: String,
    pub category: String,
    pub decode_ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recipient: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_in: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_out: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount_in: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount_out_min: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline: Option<u64>,
    pub summary: String,
}

pub fn selector_for_signature(signature: &str) -> [u8; 4] {
    let hash = keccak256(signature.as_bytes());
    [hash[0], hash[1], hash[2], hash[3]]
}

const RAW_SIGNATURES: &[(&str, &str, Category)] = &[
    // Claim / Airdrop (7)
    ("claim", "claim()", Category::Claim),
    ("claim_uint256", "claim(uint256)", Category::Claim),
    ("claim_address", "claim(address)", Category::Claim),
    ("claim_address_uint256", "claim(address,uint256)", Category::Claim),
    (
        "claim_address_uint256_bytes32_array",
        "claim(address,uint256,bytes32[])",
        Category::Claim,
    ),
    (
        "claim_merkle_indexed",
        "claim(uint256,address,uint256,bytes32[])",
        Category::Claim,
    ),
    (
        "claim_merkl_batch",
        "claim(address[],address[],uint256[],bytes32[][])",
        Category::Claim,
    ),
    // Withdraw / Vault (4)
    ("withdraw_uint256", "withdraw(uint256)", Category::Withdraw),
    ("withdraw_all", "withdraw()", Category::Withdraw),
    (
        "aave_v3_withdraw",
        "withdraw(address,uint256,address)",
        Category::Withdraw,
    ),
    (
        "erc4626_redeem",
        "redeem(uint256,address,address)",
        Category::Withdraw,
    ),
    // DEX Swaps: Universal Router + Uniswap V2 + Uniswap V3 (20)
    (
        "universal_router_execute",
        "execute(bytes,bytes[],uint256)",
        Category::Swap,
    ),
    (
        "universal_router_execute_no_deadline",
        "execute(bytes,bytes[])",
        Category::Swap,
    ),
    (
        "uniswap_v2_swap_exact_tokens_for_tokens",
        "swapExactTokensForTokens(uint256,uint256,address[],address,uint256)",
        Category::Swap,
    ),
    (
        "uniswap_v2_swap_tokens_for_exact_tokens",
        "swapTokensForExactTokens(uint256,uint256,address[],address,uint256)",
        Category::Swap,
    ),
    (
        "uniswap_v2_swap_exact_eth_for_tokens",
        "swapExactETHForTokens(uint256,address[],address,uint256)",
        Category::Swap,
    ),
    (
        "uniswap_v2_swap_tokens_for_exact_eth",
        "swapTokensForExactETH(uint256,uint256,address[],address,uint256)",
        Category::Swap,
    ),
    (
        "uniswap_v2_swap_exact_tokens_for_eth",
        "swapExactTokensForETH(uint256,uint256,address[],address,uint256)",
        Category::Swap,
    ),
    (
        "uniswap_v2_swap_eth_for_exact_tokens",
        "swapETHForExactTokens(uint256,address[],address,uint256)",
        Category::Swap,
    ),
    (
        "uniswap_v2_swap_exact_tokens_supporting_fee",
        "swapExactTokensForTokensSupportingFeeOnTransferTokens(uint256,uint256,address[],address,uint256)",
        Category::Swap,
    ),
    (
        "uniswap_v2_swap_exact_eth_supporting_fee",
        "swapExactETHForTokensSupportingFeeOnTransferTokens(uint256,address[],address,uint256)",
        Category::Swap,
    ),
    (
        "uniswap_v2_swap_exact_tokens_for_eth_supporting_fee",
        "swapExactTokensForETHSupportingFeeOnTransferTokens(uint256,uint256,address[],address,uint256)",
        Category::Swap,
    ),
    (
        "uniswap_v3_exact_input_single",
        "exactInputSingle((address,address,uint24,address,uint256,uint256,uint256,uint160))",
        Category::Swap,
    ),
    (
        "uniswap_v3_exact_input_single_v2",
        "exactInputSingle((address,address,uint24,address,uint256,uint256,uint160))",
        Category::Swap,
    ),
    (
        "uniswap_v3_exact_input",
        "exactInput((bytes,address,uint256,uint256,uint256))",
        Category::Swap,
    ),
    (
        "uniswap_v3_exact_input_v2",
        "exactInput((bytes,address,uint256,uint256))",
        Category::Swap,
    ),
    (
        "uniswap_v3_exact_output_single",
        "exactOutputSingle((address,address,uint24,address,uint256,uint256,uint256,uint160))",
        Category::Swap,
    ),
    (
        "uniswap_v3_exact_output_single_v2",
        "exactOutputSingle((address,address,uint24,address,uint256,uint256,uint160))",
        Category::Swap,
    ),
    (
        "uniswap_v3_exact_output",
        "exactOutput((bytes,address,uint256,uint256,uint256))",
        Category::Swap,
    ),
    (
        "uniswap_v3_exact_output_v2",
        "exactOutput((bytes,address,uint256,uint256))",
        Category::Swap,
    ),
    (
        "uniswap_v3_pool_swap",
        "swap(address,bool,int256,uint160,bytes)",
        Category::Swap,
    ),
    // Liquidations (3)
    ("liquidate", "liquidate(address,address)", Category::Liquidate),
    (
        "liquidate_borrow",
        "liquidateBorrow(address,uint256,address)",
        Category::Liquidate,
    ),
    (
        "aave_v3_liquidation_call",
        "liquidationCall(address,address,address,uint256,bool)",
        Category::Liquidate,
    ),
];

pub static REGISTRY: LazyLock<Vec<SelectorSpec>> = LazyLock::new(|| {
    RAW_SIGNATURES
        .iter()
        .map(|&(name, signature, category)| {
            let selector = selector_for_signature(signature);
            SelectorSpec {
                selector,
                selector_hex: format!("0x{}", hex::encode(selector)),
                name,
                signature,
                category,
            }
        })
        .collect()
});

static LOOKUP: LazyLock<HashMap<[u8; 4], usize>> = LazyLock::new(|| {
    REGISTRY
        .iter()
        .enumerate()
        .map(|(idx, spec)| (spec.selector, idx))
        .collect()
});

pub fn match_calldata(calldata: &[u8]) -> Option<&'static SelectorSpec> {
    if calldata.len() < 4 {
        return None;
    }
    let sel = [calldata[0], calldata[1], calldata[2], calldata[3]];
    LOOKUP.get(&sel).map(|&idx| &REGISTRY[idx])
}

fn short_addr(addr: &Address) -> String {
    let s = format!("{addr:#x}");
    format!("{}…{}", &s[..6], &s[s.len() - 4..])
}

fn u64_clamped(v: U256) -> Option<u64> {
    u64::try_from(v).ok()
}

fn decode_v3_path_tokens(path: &[u8]) -> (Option<Address>, Option<Address>) {
    if path.len() < 43 {
        return (None, None);
    }
    let first = Address::from_slice(&path[0..20]);
    let last = Address::from_slice(&path[path.len() - 20..]);
    (Some(first), Some(last))
}

/// Decode calldata using Alloy's typed `SolCall` decoders into structured fields.
pub fn decode_calldata(spec: &SelectorSpec, data: &[u8], value: U256) -> DecodedCall {
    let mut base = DecodedCall {
        signature: spec.signature.to_string(),
        category: spec.category.as_str().to_string(),
        decode_ok: false,
        recipient: None,
        token_in: None,
        token_out: None,
        amount_in: None,
        amount_out_min: None,
        deadline: None,
        summary: format!("{} (raw {}B)", spec.name, data.len()),
    };

    match spec.name {
        "claim" => {
            if defs::claim_0::claimCall::abi_decode(data).is_ok() {
                base.decode_ok = true;
                base.summary = "claim()".to_string();
            }
        }
        "claim_uint256" => {
            if let Ok(c) = defs::claim_u256::claimCall::abi_decode(data) {
                base.decode_ok = true;
                base.amount_in = Some(c.amount.to_string());
                base.summary = format!("claim(amount={})", c.amount);
            }
        }
        "claim_address" => {
            if let Ok(c) = defs::claim_addr::claimCall::abi_decode(data) {
                base.decode_ok = true;
                base.recipient = Some(format!("{:#x}", c.account));
                base.summary = format!("claim(account={})", short_addr(&c.account));
            }
        }
        "claim_address_uint256" => {
            if let Ok(c) = defs::claim_addr_u256::claimCall::abi_decode(data) {
                base.decode_ok = true;
                base.recipient = Some(format!("{:#x}", c.account));
                base.amount_in = Some(c.amount.to_string());
                base.summary = format!(
                    "claim(account={}, amount={})",
                    short_addr(&c.account),
                    c.amount
                );
            }
        }
        "claim_address_uint256_bytes32_array" => {
            if let Ok(c) = defs::claim_merkle::claimCall::abi_decode(data) {
                base.decode_ok = true;
                base.recipient = Some(format!("{:#x}", c.account));
                base.amount_in = Some(c.amount.to_string());
                base.summary = format!(
                    "claim(account={}, amount={}, proof[{}])",
                    short_addr(&c.account),
                    c.amount,
                    c.merkleProof.len()
                );
            }
        }
        "claim_merkle_indexed" => {
            if let Ok(c) = defs::claim_merkle_indexed::claimCall::abi_decode(data) {
                base.decode_ok = true;
                base.recipient = Some(format!("{:#x}", c.account));
                base.amount_in = Some(c.amount.to_string());
                base.summary = format!(
                    "claim(idx={}, account={}, amount={}, proof[{}])",
                    c.index,
                    short_addr(&c.account),
                    c.amount,
                    c.merkleProof.len()
                );
            }
        }
        "claim_merkl_batch" => {
            if let Ok(c) = defs::claim_merkl_batch::claimCall::abi_decode(data) {
                base.decode_ok = true;
                if let Some(first_user) = c.users.first() {
                    base.recipient = Some(format!("{first_user:#x}"));
                }
                if let Some(first_tok) = c.tokens.first() {
                    base.token_out = Some(format!("{first_tok:#x}"));
                }
                if let Some(first_amt) = c.amounts.first() {
                    base.amount_in = Some(first_amt.to_string());
                }
                base.summary = format!(
                    "claim(users[{}], tokens[{}])",
                    c.users.len(),
                    c.tokens.len()
                );
            }
        }
        "withdraw_uint256" => {
            if let Ok(c) = defs::withdraw_u256::withdrawCall::abi_decode(data) {
                base.decode_ok = true;
                base.amount_in = Some(c.amount.to_string());
                base.summary = format!("withdraw(amount={})", c.amount);
            }
        }
        "withdraw_all" => {
            if defs::withdraw_0::withdrawCall::abi_decode(data).is_ok() {
                base.decode_ok = true;
                base.summary = "withdraw()".to_string();
            }
        }
        "aave_v3_withdraw" => {
            if let Ok(c) = defs::aave_withdraw::withdrawCall::abi_decode(data) {
                base.decode_ok = true;
                base.token_out = Some(format!("{:#x}", c.asset));
                base.amount_in = Some(c.amount.to_string());
                base.recipient = Some(format!("{:#x}", c.to));
                base.summary = format!(
                    "withdraw(asset={}, amount={}, to={})",
                    short_addr(&c.asset),
                    c.amount,
                    short_addr(&c.to)
                );
            }
        }
        "erc4626_redeem" => {
            if let Ok(c) = defs::erc4626_redeem::redeemCall::abi_decode(data) {
                base.decode_ok = true;
                base.amount_in = Some(c.shares.to_string());
                base.recipient = Some(format!("{:#x}", c.receiver));
                base.summary = format!(
                    "redeem(shares={}, receiver={}, owner={})",
                    c.shares,
                    short_addr(&c.receiver),
                    short_addr(&c.owner)
                );
            }
        }
        "universal_router_execute" => {
            if let Ok(c) = defs::universal_execute_deadline::executeCall::abi_decode(data) {
                base.decode_ok = true;
                base.deadline = u64_clamped(c.deadline);
                if !value.is_zero() {
                    base.amount_in = Some(value.to_string());
                }
                base.summary = format!(
                    "UniversalRouter.execute(cmds=0x{}, inputs[{}])",
                    hex::encode(&c.commands),
                    c.inputs.len()
                );
            }
        }
        "universal_router_execute_no_deadline" => {
            if let Ok(c) = defs::universal_execute::executeCall::abi_decode(data) {
                base.decode_ok = true;
                if !value.is_zero() {
                    base.amount_in = Some(value.to_string());
                }
                base.summary = format!(
                    "UniversalRouter.execute(cmds=0x{}, inputs[{}])",
                    hex::encode(&c.commands),
                    c.inputs.len()
                );
            }
        }
        "uniswap_v2_swap_exact_tokens_for_tokens" => {
            if let Ok(c) = defs::v2_swaps::swapExactTokensForTokensCall::abi_decode(data) {
                populate_v2_swap(
                    &mut base,
                    "swapExactTokensForTokens",
                    Some(c.amountIn),
                    Some(c.amountOutMin),
                    &c.path,
                    &c.to,
                    c.deadline,
                );
            }
        }
        "uniswap_v2_swap_tokens_for_exact_tokens" => {
            if let Ok(c) = defs::v2_swaps::swapTokensForExactTokensCall::abi_decode(data) {
                populate_v2_swap(
                    &mut base,
                    "swapTokensForExactTokens",
                    Some(c.amountInMax),
                    Some(c.amountOut),
                    &c.path,
                    &c.to,
                    c.deadline,
                );
            }
        }
        "uniswap_v2_swap_exact_eth_for_tokens" => {
            if let Ok(c) = defs::v2_swaps::swapExactETHForTokensCall::abi_decode(data) {
                populate_v2_swap(
                    &mut base,
                    "swapExactETHForTokens",
                    Some(value),
                    Some(c.amountOutMin),
                    &c.path,
                    &c.to,
                    c.deadline,
                );
            }
        }
        "uniswap_v2_swap_tokens_for_exact_eth" => {
            if let Ok(c) = defs::v2_swaps::swapTokensForExactETHCall::abi_decode(data) {
                populate_v2_swap(
                    &mut base,
                    "swapTokensForExactETH",
                    Some(c.amountInMax),
                    Some(c.amountOut),
                    &c.path,
                    &c.to,
                    c.deadline,
                );
            }
        }
        "uniswap_v2_swap_exact_tokens_for_eth" => {
            if let Ok(c) = defs::v2_swaps::swapExactTokensForETHCall::abi_decode(data) {
                populate_v2_swap(
                    &mut base,
                    "swapExactTokensForETH",
                    Some(c.amountIn),
                    Some(c.amountOutMin),
                    &c.path,
                    &c.to,
                    c.deadline,
                );
            }
        }
        "uniswap_v2_swap_eth_for_exact_tokens" => {
            if let Ok(c) = defs::v2_swaps::swapETHForExactTokensCall::abi_decode(data) {
                populate_v2_swap(
                    &mut base,
                    "swapETHForExactTokens",
                    Some(value),
                    Some(c.amountOut),
                    &c.path,
                    &c.to,
                    c.deadline,
                );
            }
        }
        "uniswap_v2_swap_exact_tokens_supporting_fee" => {
            if let Ok(c) =
                defs::v2_swaps::swapExactTokensForTokensSupportingFeeOnTransferTokensCall::abi_decode(
                    data,
                )
            {
                populate_v2_swap(
                    &mut base,
                    "swapExactTokensSupportingFee",
                    Some(c.amountIn),
                    Some(c.amountOutMin),
                    &c.path,
                    &c.to,
                    c.deadline,
                );
            }
        }
        "uniswap_v2_swap_exact_eth_supporting_fee" => {
            if let Ok(c) =
                defs::v2_swaps::swapExactETHForTokensSupportingFeeOnTransferTokensCall::abi_decode(
                    data,
                )
            {
                populate_v2_swap(
                    &mut base,
                    "swapExactETHSupportingFee",
                    Some(value),
                    Some(c.amountOutMin),
                    &c.path,
                    &c.to,
                    c.deadline,
                );
            }
        }
        "uniswap_v2_swap_exact_tokens_for_eth_supporting_fee" => {
            if let Ok(c) =
                defs::v2_swaps::swapExactTokensForETHSupportingFeeOnTransferTokensCall::abi_decode(
                    data,
                )
            {
                populate_v2_swap(
                    &mut base,
                    "swapExactTokensForETHSupportingFee",
                    Some(c.amountIn),
                    Some(c.amountOutMin),
                    &c.path,
                    &c.to,
                    c.deadline,
                );
            }
        }
        "uniswap_v3_exact_input_single" => {
            if let Ok(c) = defs::v3_router_v1::exactInputSingleCall::abi_decode(data) {
                let p = c.params;
                base.decode_ok = true;
                base.token_in = Some(format!("{:#x}", p.tokenIn));
                base.token_out = Some(format!("{:#x}", p.tokenOut));
                base.recipient = Some(format!("{:#x}", p.recipient));
                base.amount_in = Some(p.amountIn.to_string());
                base.amount_out_min = Some(p.amountOutMinimum.to_string());
                base.deadline = u64_clamped(p.deadline);
                base.summary = format!(
                    "exactInputSingle({} → {}, fee={}, in={})",
                    short_addr(&p.tokenIn),
                    short_addr(&p.tokenOut),
                    p.fee,
                    p.amountIn
                );
            }
        }
        "uniswap_v3_exact_input_single_v2" => {
            if let Ok(c) = defs::v3_router_v2::exactInputSingleCall::abi_decode(data) {
                let p = c.params;
                base.decode_ok = true;
                base.token_in = Some(format!("{:#x}", p.tokenIn));
                base.token_out = Some(format!("{:#x}", p.tokenOut));
                base.recipient = Some(format!("{:#x}", p.recipient));
                base.amount_in = Some(p.amountIn.to_string());
                base.amount_out_min = Some(p.amountOutMinimum.to_string());
                base.summary = format!(
                    "exactInputSingle({} → {}, fee={}, in={})",
                    short_addr(&p.tokenIn),
                    short_addr(&p.tokenOut),
                    p.fee,
                    p.amountIn
                );
            }
        }
        "uniswap_v3_exact_input" => {
            if let Ok(c) = defs::v3_router_v1::exactInputCall::abi_decode(data) {
                let p = c.params;
                base.decode_ok = true;
                let (tin, tout) = decode_v3_path_tokens(&p.path);
                base.token_in = tin.map(|a| format!("{a:#x}"));
                base.token_out = tout.map(|a| format!("{a:#x}"));
                base.recipient = Some(format!("{:#x}", p.recipient));
                base.amount_in = Some(p.amountIn.to_string());
                base.amount_out_min = Some(p.amountOutMinimum.to_string());
                base.deadline = u64_clamped(p.deadline);
                base.summary = format!(
                    "exactInput(in={}, minOut={}, to={})",
                    p.amountIn,
                    p.amountOutMinimum,
                    short_addr(&p.recipient)
                );
            }
        }
        "uniswap_v3_exact_input_v2" => {
            if let Ok(c) = defs::v3_router_v2::exactInputCall::abi_decode(data) {
                let p = c.params;
                base.decode_ok = true;
                let (tin, tout) = decode_v3_path_tokens(&p.path);
                base.token_in = tin.map(|a| format!("{a:#x}"));
                base.token_out = tout.map(|a| format!("{a:#x}"));
                base.recipient = Some(format!("{:#x}", p.recipient));
                base.amount_in = Some(p.amountIn.to_string());
                base.amount_out_min = Some(p.amountOutMinimum.to_string());
                base.summary = format!(
                    "exactInput(in={}, minOut={}, to={})",
                    p.amountIn,
                    p.amountOutMinimum,
                    short_addr(&p.recipient)
                );
            }
        }
        "uniswap_v3_exact_output_single" => {
            if let Ok(c) = defs::v3_router_v1::exactOutputSingleCall::abi_decode(data) {
                let p = c.params;
                base.decode_ok = true;
                base.token_in = Some(format!("{:#x}", p.tokenIn));
                base.token_out = Some(format!("{:#x}", p.tokenOut));
                base.recipient = Some(format!("{:#x}", p.recipient));
                base.amount_in = Some(p.amountInMaximum.to_string());
                base.amount_out_min = Some(p.amountOut.to_string());
                base.deadline = u64_clamped(p.deadline);
                base.summary = format!(
                    "exactOutputSingle({} → {}, out={})",
                    short_addr(&p.tokenIn),
                    short_addr(&p.tokenOut),
                    p.amountOut
                );
            }
        }
        "uniswap_v3_exact_output_single_v2" => {
            if let Ok(c) = defs::v3_router_v2::exactOutputSingleCall::abi_decode(data) {
                let p = c.params;
                base.decode_ok = true;
                base.token_in = Some(format!("{:#x}", p.tokenIn));
                base.token_out = Some(format!("{:#x}", p.tokenOut));
                base.recipient = Some(format!("{:#x}", p.recipient));
                base.amount_in = Some(p.amountInMaximum.to_string());
                base.amount_out_min = Some(p.amountOut.to_string());
                base.summary = format!(
                    "exactOutputSingle({} → {}, out={})",
                    short_addr(&p.tokenIn),
                    short_addr(&p.tokenOut),
                    p.amountOut
                );
            }
        }
        "uniswap_v3_exact_output" => {
            if let Ok(c) = defs::v3_router_v1::exactOutputCall::abi_decode(data) {
                let p = c.params;
                base.decode_ok = true;
                base.recipient = Some(format!("{:#x}", p.recipient));
                base.amount_in = Some(p.amountInMaximum.to_string());
                base.amount_out_min = Some(p.amountOut.to_string());
                base.deadline = u64_clamped(p.deadline);
                base.summary = format!("exactOutput(out={}, to={})", p.amountOut, short_addr(&p.recipient));
            }
        }
        "uniswap_v3_exact_output_v2" => {
            if let Ok(c) = defs::v3_router_v2::exactOutputCall::abi_decode(data) {
                let p = c.params;
                base.decode_ok = true;
                base.recipient = Some(format!("{:#x}", p.recipient));
                base.amount_in = Some(p.amountInMaximum.to_string());
                base.amount_out_min = Some(p.amountOut.to_string());
                base.summary = format!("exactOutput(out={}, to={})", p.amountOut, short_addr(&p.recipient));
            }
        }
        "uniswap_v3_pool_swap" => {
            if let Ok(c) = defs::v3_pool::swapCall::abi_decode(data) {
                base.decode_ok = true;
                base.recipient = Some(format!("{:#x}", c.recipient));
                base.summary = format!(
                    "pool.swap(to={}, zeroForOne={}, amt={})",
                    short_addr(&c.recipient),
                    c.zeroForOne,
                    c.amountSpecified
                );
            }
        }
        "liquidate" => {
            if let Ok(c) = defs::liquidations::liquidateCall::abi_decode(data) {
                base.decode_ok = true;
                base.token_out = Some(format!("{:#x}", c.collateral));
                base.summary = format!(
                    "liquidate(borrower={}, collateral={})",
                    short_addr(&c.borrower),
                    short_addr(&c.collateral)
                );
            }
        }
        "liquidate_borrow" => {
            if let Ok(c) = defs::liquidations::liquidateBorrowCall::abi_decode(data) {
                base.decode_ok = true;
                base.amount_in = Some(c.repayAmount.to_string());
                base.token_out = Some(format!("{:#x}", c.cTokenCollateral));
                base.summary = format!(
                    "liquidateBorrow(borrower={}, repay={}, cToken={})",
                    short_addr(&c.borrower),
                    c.repayAmount,
                    short_addr(&c.cTokenCollateral)
                );
            }
        }
        "aave_v3_liquidation_call" => {
            if let Ok(c) = defs::liquidations::liquidationCallCall::abi_decode(data) {
                base.decode_ok = true;
                base.token_in = Some(format!("{:#x}", c.debtAsset));
                base.token_out = Some(format!("{:#x}", c.collateralAsset));
                base.amount_in = Some(c.debtToCover.to_string());
                base.summary = format!(
                    "liquidationCall(user={}, debt={}, collateral={})",
                    short_addr(&c.user),
                    short_addr(&c.debtAsset),
                    short_addr(&c.collateralAsset)
                );
            }
        }
        _ => {}
    }

    base
}

fn populate_v2_swap(
    base: &mut DecodedCall,
    fn_label: &str,
    amount_in: Option<U256>,
    amount_out_min: Option<U256>,
    path: &[Address],
    to: &Address,
    deadline: U256,
) {
    base.decode_ok = true;
    base.recipient = Some(format!("{to:#x}"));
    base.token_in = path.first().map(|a| format!("{a:#x}"));
    base.token_out = path.last().map(|a| format!("{a:#x}"));
    base.amount_in = amount_in.map(|v| v.to_string());
    base.amount_out_min = amount_out_min.map(|v| v.to_string());
    base.deadline = u64_clamped(deadline);

    let route = match (path.first(), path.last()) {
        (Some(first), Some(last)) => format!("{} → {}", short_addr(first), short_addr(last)),
        _ => "path[]".to_string(),
    };
    base.summary = format!("{fn_label}({route}, hops={}, to={})", path.len(), short_addr(to));
}

/// Re-encodes calldata substituting an explicit beneficiary/recipient address with `probe`
/// when the ABI carries an address recipient parameter. Used purely inside local Revm
/// memory to test whether a call's payout is cryptographically/storage-bound to the original
/// recipient or redirects to an arbitrary caller-supplied address.
pub fn rewrite_recipient_calldata(
    spec: &SelectorSpec,
    data: &[u8],
    probe: Address,
) -> Option<Bytes> {
    let encoded: Vec<u8> = match spec.name {
        "claim_address" => {
            let mut c = defs::claim_addr::claimCall::abi_decode(data).ok()?;
            if c.account == probe {
                return None;
            }
            c.account = probe;
            c.abi_encode()
        }
        "claim_address_uint256" => {
            let mut c = defs::claim_addr_u256::claimCall::abi_decode(data).ok()?;
            if c.account == probe {
                return None;
            }
            c.account = probe;
            c.abi_encode()
        }
        "claim_address_uint256_bytes32_array" => {
            let mut c = defs::claim_merkle::claimCall::abi_decode(data).ok()?;
            if c.account == probe {
                return None;
            }
            c.account = probe;
            c.abi_encode()
        }
        "claim_merkle_indexed" => {
            let mut c = defs::claim_merkle_indexed::claimCall::abi_decode(data).ok()?;
            if c.account == probe {
                return None;
            }
            c.account = probe;
            c.abi_encode()
        }
        "aave_v3_withdraw" => {
            let mut c = defs::aave_withdraw::withdrawCall::abi_decode(data).ok()?;
            if c.to == probe {
                return None;
            }
            c.to = probe;
            c.abi_encode()
        }
        "erc4626_redeem" => {
            let mut c = defs::erc4626_redeem::redeemCall::abi_decode(data).ok()?;
            if c.receiver == probe {
                return None;
            }
            c.receiver = probe;
            c.abi_encode()
        }
        "uniswap_v2_swap_exact_eth_for_tokens" => {
            let mut c = defs::v2_swaps::swapExactETHForTokensCall::abi_decode(data).ok()?;
            if c.to == probe {
                return None;
            }
            c.to = probe;
            c.abi_encode()
        }
        "uniswap_v2_swap_eth_for_exact_tokens" => {
            let mut c = defs::v2_swaps::swapETHForExactTokensCall::abi_decode(data).ok()?;
            if c.to == probe {
                return None;
            }
            c.to = probe;
            c.abi_encode()
        }
        "uniswap_v3_pool_swap" => {
            let mut c = defs::v3_pool::swapCall::abi_decode(data).ok()?;
            if c.recipient == probe {
                return None;
            }
            c.recipient = probe;
            c.abi_encode()
        }
        _ => return None,
    };
    Some(Bytes::from(encoded))
}

const ERROR_STRING_SELECTOR: [u8; 4] = [0x08, 0xc3, 0x79, 0xa0];
const PANIC_SELECTOR: [u8; 4] = [0x4e, 0x48, 0x7b, 0x71];

fn panic_reason(code: u64) -> String {
    match code {
        0x00 => "generic compiler panic".to_string(),
        0x01 => "assertion failed".to_string(),
        0x11 => "arithmetic overflow or underflow".to_string(),
        0x12 => "division by zero".to_string(),
        0x21 => "invalid enum value".to_string(),
        0x22 => "invalid storage byte array".to_string(),
        0x31 => "pop on empty array".to_string(),
        0x32 => "array index out of bounds".to_string(),
        0x41 => "out of memory".to_string(),
        0x51 => "call to uninitialised function".to_string(),
        other => format!("undefined panic 0x{other:02x}"),
    }
}

fn known_custom_error(sel: [u8; 4]) -> Option<&'static str> {
    match sel {
        [0x09, 0xbe, 0xe1, 0x82] => Some("InvalidProof()"),
        [0x64, 0x6c, 0xf5, 0x58] => Some("AlreadyClaimed()"),
        [0x20, 0x3d, 0x82, 0xd8] => Some("Expired()"),
        [0x82, 0xb4, 0x29, 0x00] => Some("Unauthorized()"),
        [0xf4, 0xd6, 0x78, 0xb8] => Some("InsufficientBalance()"),
        [0x13, 0xbe, 0x25, 0x2b] => Some("InsufficientAllowance()"),
        _ => None,
    }
}

/// Decode EVM revert bytes into `(kind, human_readable_reason)`.
pub fn decode_revert_bytes(data: &[u8]) -> Option<(String, String)> {
    if data.len() < 4 {
        return None;
    }
    let head = [data[0], data[1], data[2], data[3]];
    let body = &data[4..];

    if head == ERROR_STRING_SELECTOR && body.len() >= 64 {
        let offset = U256::from_be_slice(&body[0..32]);
        if let Ok(off) = usize::try_from(offset) {
            if off + 32 <= body.len() {
                let len_u256 = U256::from_be_slice(&body[off..off + 32]);
                if let Ok(len) = usize::try_from(len_u256) {
                    let end = (off + 32 + len).min(body.len());
                    if off + 32 <= end {
                        let text = String::from_utf8_lossy(&body[off + 32..end]);
                        let trimmed: String = text.chars().take(180).collect();
                        return Some(("revert".to_string(), trimmed));
                    }
                }
            }
        }
        return Some(("revert".to_string(), "unreadable Error(string)".to_string()));
    }

    if head == PANIC_SELECTOR && body.len() >= 32 {
        let code_u256 = U256::from_be_slice(&body[0..32]);
        let code = u64::try_from(code_u256).unwrap_or(0xff);
        return Some(("panic".to_string(), panic_reason(code)));
    }

    if let Some(known) = known_custom_error(head) {
        return Some(("custom".to_string(), known.to_string()));
    }

    Some(("custom".to_string(), format!("0x{}", hex::encode(head))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    #[test]
    fn registry_contains_all_unique_selectors() {
        assert_eq!(REGISTRY.len(), 34);
        assert_eq!(LOOKUP.len(), 34);
        let claim_spec = match_calldata(&[0x4e, 0x71, 0xd9, 0x2d]).expect("claim() selector");
        assert_eq!(claim_spec.name, "claim");
    }

    #[test]
    fn decodes_and_rewrites_claim_with_recipient() {
        let victim = address!("1111111111111111111111111111111111111111");
        let probe = address!("0000000000000000000000000000000000c0ffee");
        let call = defs::claim_addr_u256::claimCall {
            account: victim,
            amount: U256::from(500_000u64),
        };
        let raw = call.abi_encode();
        let spec = match_calldata(&raw).expect("matches claim(address,uint256)");
        assert_eq!(spec.name, "claim_address_uint256");

        let decoded = decode_calldata(spec, &raw, U256::ZERO);
        assert!(decoded.decode_ok);
        assert_eq!(
            decoded.recipient.as_deref(),
            Some("0x1111111111111111111111111111111111111111")
        );
        assert_eq!(decoded.amount_in.as_deref(), Some("500000"));

        let rewritten = rewrite_recipient_calldata(spec, &raw, probe).expect("rewrites account");
        let redecoded = decode_calldata(spec, &rewritten, U256::ZERO);
        assert_eq!(
            redecoded.recipient.as_deref(),
            Some("0x0000000000000000000000000000000000c0ffee")
        );
    }

    #[test]
    fn decodes_revert_error_and_panic() {
        use alloy::sol_types::SolError;
        let encoded = Error {
            reason: "already claimed".to_string(),
        }
        .abi_encode();
        let (kind, reason) = decode_revert_bytes(&encoded).expect("decodes Error(string)");
        assert_eq!(kind, "revert");
        assert_eq!(reason, "already claimed");

        let mut panic_bytes = vec![0x4e, 0x48, 0x7b, 0x71];
        panic_bytes.extend_from_slice(&U256::from(0x11u64).to_be_bytes::<32>());
        let (pkind, preason) = decode_revert_bytes(&panic_bytes).expect("decodes Panic(uint256)");
        assert_eq!(pkind, "panic");
        assert_eq!(preason, "arithmetic overflow or underflow");
    }
}
