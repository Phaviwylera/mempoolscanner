//! Stages 4 & 5: In-process `revm` transaction simulation, block-stability check,
//! synthetic caller/recipient substitution front-runnability probe, and profit scoring.

use crate::abi::{decode_revert_bytes, rewrite_recipient_calldata, DecodedCall, SelectorSpec};
use crate::state::{
    one_line_str, BlockSnapshot, ForkStatus, PinnedStateView, StateBackend,
};
use revm::bytecode::Bytecode;
use revm::context::TxEnv;
use revm::context_interface::block::BlobExcessGasAndPrice;
use revm::context_interface::result::{ExecutionResult, Output};
use revm::database::CacheDB;
use revm::primitives::eip4844::BLOB_BASE_FEE_UPDATE_FRACTION_PRAGUE;
use revm::primitives::{address, b256, hardfork::SpecId, Address, Bytes, Log, TxKind, B256, KECCAK_EMPTY, U256};
use revm::state::AccountInfo;
use revm::{AsyncDb, Context, ExecuteEvm, MainBuilder, MainContext};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Synthetic local-only probe account used inside ephemeral `CacheDB` overlays
/// to test whether a transaction is `msg.sender`-protected, beneficiary-locked,
/// or extractable. Never backed by a key, never signed, never broadcast.
pub const PROBE_ADDRESS: Address = address!("0000000000000000000000000000000000c0ffee");

const TRANSFER_TOPIC: B256 =
    b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");

const PROBE_SEED_ETH: u128 = 1_000_000_000_000_000_000_000; // 1,000 ETH virtual balance

#[derive(Clone, Debug)]
pub struct ObservedTx {
    pub hash: String,
    pub from: Address,
    pub to: Address,
    pub nonce: Option<u64>,
    pub value: U256,
    pub gas_limit: u64,
    pub tip_wei: Option<u128>,
    pub max_fee_wei: Option<u128>,
    pub calldata: Bytes,
    pub selector_hex: String,
    pub timestamp_iso: String,
    pub ingest_us: u64,
    pub source: &'static str,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TokenTransfer {
    pub token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decimals: Option<u8>,
    pub from: String,
    pub to: String,
    pub amount: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SimOutcome {
    pub ok: bool,
    pub engine: String,
    pub gas_used: u64,
    pub result_or_error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revert_data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revert_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revert_reason: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub transfers: Vec<TokenTransfer>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReferenceCheck {
    pub block: u64,
    pub ok: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForkEvidence {
    pub block: Option<u64>,
    pub head: Option<u64>,
    pub lag: Option<u64>,
    pub depth: u64,
    pub code_present: Option<bool>,
    pub stale: bool,
    pub stable: bool,
    pub checked: Vec<u64>,
    pub reference: Option<ReferenceCheck>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DecisionReport {
    /// `"extractable"` | `"beneficiary_locked"` | `"sender_protected"` | `"not_applicable"`
    pub status: String,
    /// `"caller_substitution"` | `"recipient_rewrite"` | `"none"`
    pub probe_mode: String,
    pub probe_ok: bool,
    pub probe_gas_used: u64,
    pub would_frontrun: bool,
    pub gross_value_wei: String,
    pub gas_cost_wei: String,
    pub priority_fee_wei: String,
    pub builder_bribe_wei: String,
    pub net_profit_wei: String,
    pub probe_token_gains: usize,
    pub verdict: String,
}

impl DecisionReport {
    pub fn not_applicable(reason: &str) -> Self {
        Self {
            status: "not_applicable".to_string(),
            probe_mode: "none".to_string(),
            probe_ok: false,
            probe_gas_used: 0,
            would_frontrun: false,
            gross_value_wei: "0".to_string(),
            gas_cost_wei: "0".to_string(),
            priority_fee_wei: "0".to_string(),
            builder_bribe_wei: "0".to_string(),
            net_profit_wei: "0".to_string(),
            probe_token_gains: 0,
            verdict: reason.to_string(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StageTelemetry {
    pub source: String,
    pub ingest_us: u64,
    pub decode_us: u64,
    pub sim_us: u64,
    pub probe_us: u64,
    pub total_us: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HitRecord {
    pub name: String,
    pub hash: String,
    pub from: String,
    pub to: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub nonce: Option<u64>,
    pub value: U256String,
    pub selector: String,
    pub gas: u64,
    pub tip: Option<u128>,
    pub ts: String,
    pub fork: ForkEvidence,
    pub sim: SimOutcome,
    pub decoded: DecodedCall,
    pub decision: DecisionReport,
    pub telemetry: StageTelemetry,
    pub label: String,
}

/// Serializes as integer when small or string/number compatibly; `hits_api.py` normalizes to str.
pub type U256String = String;

struct RawExecOutcome {
    sim: SimOutcome,
    actor_eth_gain_wei: u128,
    actor_eth_spent_wei: u128,
    actor_token_wei_equiv: u128,
    actor_token_spent_wei: u128,
    actor_token_transfers: usize,
}

impl RawExecOutcome {
    fn failed(sim: SimOutcome) -> Self {
        Self {
            sim,
            actor_eth_gain_wei: 0,
            actor_eth_spent_wei: 0,
            actor_token_wei_equiv: 0,
            actor_token_spent_wei: 0,
            actor_token_transfers: 0,
        }
    }

    fn net_inflow_wei(&self) -> u128 {
        let inflow = self
            .actor_eth_gain_wei
            .saturating_add(self.actor_token_wei_equiv);
        let outflow = self
            .actor_eth_spent_wei
            .saturating_add(self.actor_token_spent_wei);
        inflow.saturating_sub(outflow)
    }
}

struct KnownToken {
    addr: Address,
    symbol: &'static str,
    decimals: u8,
    /// Approximate wei per 1 whole token (10^decimals base units) for off-chain scoring.
    eth_wei_per_unit: u128,
}

const KNOWN_TOKENS: &[KnownToken] = &[
    KnownToken {
        addr: address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
        symbol: "WETH",
        decimals: 18,
        eth_wei_per_unit: 1_000_000_000_000_000_000,
    },
    KnownToken {
        addr: address!("7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0"),
        symbol: "wstETH",
        decimals: 18,
        eth_wei_per_unit: 1_180_000_000_000_000_000,
    },
    KnownToken {
        addr: address!("ae7ab96520DE3A18E5e111B5EaAb095312D7fE84"),
        symbol: "stETH",
        decimals: 18,
        eth_wei_per_unit: 1_000_000_000_000_000_000,
    },
    KnownToken {
        addr: address!("ae78736Cd615f374D3085123A210448E74Fc6393"),
        symbol: "rETH",
        decimals: 18,
        eth_wei_per_unit: 1_120_000_000_000_000_000,
    },
    KnownToken {
        addr: address!("Be9895146f7AF43049ca1c1AE358B0541Ea49704"),
        symbol: "cbETH",
        decimals: 18,
        eth_wei_per_unit: 1_080_000_000_000_000_000,
    },
    KnownToken {
        addr: address!("Cd5fE23C85820F7B72D0926FC9b05b43E359b7ee"),
        symbol: "weETH",
        decimals: 18,
        eth_wei_per_unit: 1_050_000_000_000_000_000,
    },
    KnownToken {
        addr: address!("bf5495Efe5DB9ce00f80364C8B423567e58d2110"),
        symbol: "ezETH",
        decimals: 18,
        eth_wei_per_unit: 1_030_000_000_000_000_000,
    },
    KnownToken {
        addr: address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
        symbol: "USDC",
        decimals: 6,
        eth_wei_per_unit: 330_000_000_000_000,
    },
    KnownToken {
        addr: address!("dAC17F958D2ee523a2206206994597C13D831ec7"),
        symbol: "USDT",
        decimals: 6,
        eth_wei_per_unit: 330_000_000_000_000,
    },
    KnownToken {
        addr: address!("6B175474E89094C44Da98b954EedeAC495271d0F"),
        symbol: "DAI",
        decimals: 18,
        eth_wei_per_unit: 330_000_000_000_000,
    },
    KnownToken {
        addr: address!("4c9EDD5852cd905f086C759E8383e09bff1E68B3"),
        symbol: "USDe",
        decimals: 18,
        eth_wei_per_unit: 330_000_000_000_000,
    },
    KnownToken {
        addr: address!("9D39A5DE30e57443BfF2A8307A4256c8797A3497"),
        symbol: "sUSDe",
        decimals: 18,
        eth_wei_per_unit: 365_000_000_000_000,
    },
    KnownToken {
        addr: address!("853d955aCEf822Db058eb8505911ED77F175b99e"),
        symbol: "FRAX",
        decimals: 18,
        eth_wei_per_unit: 330_000_000_000_000,
    },
    KnownToken {
        addr: address!("6c3ea9036406852006290770BEdFcAbA0e23A0e8"),
        symbol: "PYUSD",
        decimals: 6,
        eth_wei_per_unit: 330_000_000_000_000,
    },
    KnownToken {
        addr: address!("40D16FC0246aD3160Ccc09B8D0D3A2cD28aE6C2f"),
        symbol: "GHO",
        decimals: 18,
        eth_wei_per_unit: 330_000_000_000_000,
    },
    KnownToken {
        addr: address!("f939E0A03FB07F59A73314E73794Be0E57ac1b4E"),
        symbol: "crvUSD",
        decimals: 18,
        eth_wei_per_unit: 330_000_000_000_000,
    },
    KnownToken {
        addr: address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599"),
        symbol: "WBTC",
        decimals: 8,
        eth_wei_per_unit: 28_000_000_000_000_000_000,
    },
    KnownToken {
        addr: address!("cbB7C0000aB88B473b1f5aFd9ef808440eed33Bf"),
        symbol: "cbBTC",
        decimals: 8,
        eth_wei_per_unit: 28_000_000_000_000_000_000,
    },
    KnownToken {
        addr: address!("18084fbA666a33d37592fA2633fD49a74DD93a88"),
        symbol: "tBTC",
        decimals: 18,
        eth_wei_per_unit: 28_000_000_000_000_000_000,
    },
    KnownToken {
        addr: address!("1f9840a85d5aF5bf1D1762F925BDADdC4201F984"),
        symbol: "UNI",
        decimals: 18,
        eth_wei_per_unit: 3_000_000_000_000_000,
    },
    KnownToken {
        addr: address!("7Fc66500c84A76Ad7e9c93437bFc5Ac33E2DDaE9"),
        symbol: "AAVE",
        decimals: 18,
        eth_wei_per_unit: 60_000_000_000_000_000,
    },
    KnownToken {
        addr: address!("514910771AF9Ca656af840dff83E8264EcF986CA"),
        symbol: "LINK",
        decimals: 18,
        eth_wei_per_unit: 5_000_000_000_000_000,
    },
    KnownToken {
        addr: address!("5A98FcBEA516Cf06857215779Fd812CA3beF1B32"),
        symbol: "LDO",
        decimals: 18,
        eth_wei_per_unit: 400_000_000_000_000,
    },
    KnownToken {
        addr: address!("808507121B80c02388fAd14726482e061B8da827"),
        symbol: "PENDLE",
        decimals: 18,
        eth_wei_per_unit: 1_200_000_000_000_000,
    },
    KnownToken {
        addr: address!("57e114B691Db790C35207b2e685D4A43181e6061"),
        symbol: "ENA",
        decimals: 18,
        eth_wei_per_unit: 180_000_000_000_000,
    },
    KnownToken {
        addr: address!("fAbA6f8e4a5E8Ab82F62fe7C39859FA577269BE3"),
        symbol: "ONDO",
        decimals: 18,
        eth_wei_per_unit: 300_000_000_000_000,
    },
    KnownToken {
        addr: address!("6982508145454Ce325dDbE47a25d4ec3d2311933"),
        symbol: "PEPE",
        decimals: 18,
        eth_wei_per_unit: 3_000_000_000,
    },
    KnownToken {
        addr: address!("95aD61b0a150d79219dCF64E1E6Cc01f0B64C4cE"),
        symbol: "SHIB",
        decimals: 18,
        eth_wei_per_unit: 5_000_000_000,
    },
];

fn lookup_token(addr: Address) -> Option<&'static KnownToken> {
    KNOWN_TOKENS.iter().find(|t| t.addr == addr)
}

fn token_wei_equivalent(token: Address, amount: U256) -> u128 {
    if amount.is_zero() {
        return 0;
    }
    if let Some(meta) = lookup_token(token) {
        let scale = 10u128.pow(meta.decimals as u32);
        let amt_u128 = u128::try_from(amount).unwrap_or(u128::MAX);
        return amt_u128.saturating_mul(meta.eth_wei_per_unit) / scale;
    }
    0
}

fn parse_transfers_and_gains(
    logs: &[Log],
    actor: Address,
) -> (Vec<TokenTransfer>, u128, u128, usize) {
    let mut out = Vec::new();
    let mut actor_wei_equiv: u128 = 0;
    let mut actor_spent_wei: u128 = 0;
    let mut actor_count: usize = 0;
    let mut actor_unpriced_count: usize = 0;
    let mut max_pool_priced_leg_wei: u128 = 0;

    for log in logs {
        let topics = log.data.topics();
        if topics.len() == 3 && topics[0] == TRANSFER_TOPIC && log.data.data.len() >= 32 {
            let from = Address::from_slice(&topics[1].as_slice()[12..32]);
            let to = Address::from_slice(&topics[2].as_slice()[12..32]);
            let amount = U256::from_be_slice(&log.data.data[0..32]);

            let meta = lookup_token(log.address);
            let leg_wei = token_wei_equivalent(log.address, amount);
            if to == actor && !amount.is_zero() {
                actor_count += 1;
                if leg_wei > 0 {
                    actor_wei_equiv = actor_wei_equiv.saturating_add(leg_wei);
                } else {
                    actor_unpriced_count += 1;
                }
            } else if from == actor && !amount.is_zero() {
                actor_spent_wei = actor_spent_wei.saturating_add(leg_wei);
            } else if leg_wei > max_pool_priced_leg_wei {
                max_pool_priced_leg_wei = leg_wei;
            }
            if out.len() < 12 {
                out.push(TokenTransfer {
                    token: format!("{:#x}", log.address),
                    symbol: meta.map(|m| m.symbol.to_string()),
                    decimals: meta.map(|m| m.decimals),
                    from: format!("{from:#x}"),
                    to: format!("{to:#x}"),
                    amount: amount.to_string(),
                });
            }
        }
    }

    // If actor received an unlisted ERC-20 via an internal pool swap that moved a known
    // priced asset (WETH/USDC/USDT/DAI/etc.) in the same transaction, use 98% of that leg.
    if actor_unpriced_count > 0 && actor_wei_equiv == 0 && max_pool_priced_leg_wei > 0 {
        actor_wei_equiv = max_pool_priced_leg_wei.saturating_mul(98) / 100;
    }

    (out, actor_wei_equiv, actor_spent_wei, actor_count)
}

#[allow(clippy::too_many_arguments)]
async fn execute_revm_call(
    backend: Arc<dyn StateBackend>,
    snap: &BlockSnapshot,
    caller: Address,
    to: Address,
    value: U256,
    calldata: Bytes,
    gas_limit: u64,
    seed_probe: bool,
) -> RawExecOutcome {
    let pre_balance = if seed_probe {
        U256::from(PROBE_SEED_ETH).saturating_add(value)
    } else {
        backend
            .basic(snap.number, caller)
            .await
            .ok()
            .flatten()
            .map(|a| a.balance)
            .unwrap_or(U256::ZERO)
    };

    let snap_owned = snap.clone();
    let handle = tokio::runtime::Handle::current();

    let joined = tokio::task::spawn_blocking(move || {
        let view = PinnedStateView {
            backend,
            block: snap_owned.number,
        };
        let mut cache_db = CacheDB::new(AsyncDb::with_handle(view, handle));

        if seed_probe {
            cache_db.insert_account_info(
                caller,
                AccountInfo {
                    balance: pre_balance,
                    nonce: 0,
                    code_hash: KECCAK_EMPTY,
                    code: Some(Bytecode::default()),
                    ..Default::default()
                },
            );
        }

        let effective_gas_limit = if gas_limit >= 21_000 {
            gas_limit
        } else {
            3_000_000
        };

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
                b.gas_limit = snap_owned.gas_limit.max(effective_gas_limit);
                b.prevrandao = snap_owned.prevrandao.or(Some(B256::ZERO));
                b.blob_excess_gas_and_price = snap_owned.excess_blob_gas.map(|excess| {
                    BlobExcessGasAndPrice::new(excess, BLOB_BASE_FEE_UPDATE_FRACTION_PRAGUE)
                });
            })
            .build_mainnet();

        let tx_res = TxEnv::builder()
            .caller(caller)
            .kind(TxKind::Call(to))
            .data(calldata)
            .value(value)
            .gas_limit(effective_gas_limit)
            .gas_price(snap_owned.base_fee as u128)
            .build();

        let tx_env = match tx_res {
            Ok(t) => t,
            Err(err) => {
                return RawExecOutcome::failed(SimOutcome {
                    ok: false,
                    engine: "revm".to_string(),
                    gas_used: 0,
                    result_or_error: format!("invalid tx env: {err:?}"),
                    revert_data: None,
                    revert_kind: None,
                    revert_reason: None,
                    transfers: Vec::new(),
                });
            }
        };

        match evm.transact(tx_env) {
            Ok(exec) => match exec.result {
                ExecutionResult::Success {
                    gas,
                    logs,
                    output,
                    ..
                } => {
                    let data = match output {
                        Output::Call(b) | Output::Create(b, _) => b,
                    };
                    let post_balance = exec
                        .state
                        .get(&caller)
                        .map(|acc| acc.info.balance)
                        .unwrap_or(pre_balance);
                    let eth_gain = if post_balance > pre_balance {
                        u128::try_from(post_balance - pre_balance).unwrap_or(u128::MAX)
                    } else {
                        0
                    };
                    let eth_spent = if pre_balance > post_balance {
                        u128::try_from(pre_balance - post_balance).unwrap_or(u128::MAX)
                    } else {
                        0
                    };
                    let (transfers, tok_wei, tok_spent, tok_count) =
                        parse_transfers_and_gains(&logs, caller);
                    RawExecOutcome {
                        sim: SimOutcome {
                            ok: true,
                            engine: "revm".to_string(),
                            gas_used: gas.tx_gas_used(),
                            result_or_error: format!("0x{}", hex::encode(&data)),
                            revert_data: None,
                            revert_kind: None,
                            revert_reason: None,
                            transfers,
                        },
                        actor_eth_gain_wei: eth_gain,
                        actor_eth_spent_wei: eth_spent,
                        actor_token_wei_equiv: tok_wei,
                        actor_token_spent_wei: tok_spent,
                        actor_token_transfers: tok_count,
                    }
                }
                ExecutionResult::Revert { gas, output, .. } => {
                    let hex_payload = format!("0x{}", hex::encode(&output));
                    let decoded = decode_revert_bytes(&output);
                    let msg = match &decoded {
                        Some((kind, reason)) => format!("execution reverted ({kind}): {reason}"),
                        None if output.is_empty() => "execution reverted".to_string(),
                        None => format!(
                            "execution reverted: {}",
                            hex_payload.chars().take(66).collect::<String>()
                        ),
                    };
                    RawExecOutcome::failed(SimOutcome {
                        ok: false,
                        engine: "revm".to_string(),
                        gas_used: gas.tx_gas_used(),
                        result_or_error: one_line_str(&msg),
                        revert_data: if output.is_empty() {
                            None
                        } else {
                            Some(hex_payload.chars().take(522).collect())
                        },
                        revert_kind: decoded.as_ref().map(|(k, _)| k.clone()),
                        revert_reason: decoded.map(|(_, r)| r),
                        transfers: Vec::new(),
                    })
                }
                ExecutionResult::Halt { reason, gas, .. } => RawExecOutcome::failed(SimOutcome {
                    ok: false,
                    engine: "revm".to_string(),
                    gas_used: gas.tx_gas_used(),
                    result_or_error: one_line_str(&format!("halted: {reason:?}")),
                    revert_data: None,
                    revert_kind: Some("halt".to_string()),
                    revert_reason: Some(format!("{reason:?}")),
                    transfers: Vec::new(),
                }),
            },
            Err(evm_err) => RawExecOutcome::failed(SimOutcome {
                ok: false,
                engine: "revm".to_string(),
                gas_used: 0,
                result_or_error: one_line_str(&format!("revm error: {evm_err}")),
                revert_data: None,
                revert_kind: None,
                revert_reason: None,
                transfers: Vec::new(),
            }),
        }
    })
    .await;

    match joined {
        Ok(outcome) => outcome,
        Err(join_err) => RawExecOutcome::failed(SimOutcome {
            ok: false,
            engine: "revm".to_string(),
            gas_used: 0,
            result_or_error: one_line_str(&format!("revm task join error: {join_err}")),
            revert_data: None,
            revert_kind: None,
            revert_reason: None,
            transfers: Vec::new(),
        }),
    }
}

pub fn classify_label(name: &str, sim: &SimOutcome, fork: &ForkEvidence) -> String {
    if fork.code_present == Some(false) {
        return format!("no_code:{name}");
    }
    if fork.stale {
        return format!("stale_fork:{name}");
    }
    if !sim.ok {
        let mut s = format!("sim_failed:{name}:{}", sim.result_or_error);
        s.truncate(240);
        return s;
    }
    if !fork.stable {
        return format!("state_dependent:{name}");
    }
    format!("candidate:{name}")
}

fn score_probe(
    mode: &'static str,
    probe: &RawExecOutcome,
    base_fee_wei: u64,
    victim_tip_wei: Option<u128>,
) -> DecisionReport {
    if !probe.sim.ok {
        let detail = probe
            .sim
            .revert_reason
            .as_deref()
            .unwrap_or(&probe.sim.result_or_error);
        return DecisionReport {
            status: "sender_protected".to_string(),
            probe_mode: mode.to_string(),
            probe_ok: false,
            probe_gas_used: probe.sim.gas_used,
            would_frontrun: false,
            gross_value_wei: "0".to_string(),
            gas_cost_wei: "0".to_string(),
            priority_fee_wei: "0".to_string(),
            builder_bribe_wei: "0".to_string(),
            net_profit_wei: "0".to_string(),
            probe_token_gains: 0,
            verdict: format!("PROTECTED · caller probe reverted ({detail})"),
        };
    }

    let gross_inflow = probe
        .actor_eth_gain_wei
        .saturating_add(probe.actor_token_wei_equiv);
    let gross_outflow = probe
        .actor_eth_spent_wei
        .saturating_add(probe.actor_token_spent_wei);

    if gross_inflow == 0 && gross_outflow == 0 && probe.actor_token_transfers == 0 {
        return DecisionReport {
            status: "beneficiary_locked".to_string(),
            probe_mode: mode.to_string(),
            probe_ok: true,
            probe_gas_used: probe.sim.gas_used,
            would_frontrun: false,
            gross_value_wei: "0".to_string(),
            gas_cost_wei: "0".to_string(),
            priority_fee_wei: "0".to_string(),
            builder_bribe_wei: "0".to_string(),
            net_profit_wei: "0".to_string(),
            probe_token_gains: 0,
            verdict: "BENEFICIARY LOCKED · call succeeded for probe but payout stayed bound to original recipient".to_string(),
        };
    }

    if gross_inflow <= gross_outflow {
        let reason = if gross_outflow > 0 {
            "PAYABLE SWAP · caller must supply own ETH/token input; no unguarded value extracted"
        } else {
            "UNPRICED TOKEN · unknown ERC-20 transfer has no verifiable market value"
        };
        return DecisionReport::not_applicable(reason);
    }

    let gross_wei = gross_inflow - gross_outflow;
    let tip = victim_tip_wei.unwrap_or(1_000_000_000);
    let outbid_tip = (tip + 1_000_000_000).max(tip.saturating_mul(110) / 100);
    let gas_price = (base_fee_wei as u128).saturating_add(outbid_tip);
    let gas_cost = (probe.sim.gas_used as u128).saturating_mul(gas_price);

    if gross_wei > gas_cost {
        let pre_bribe = gross_wei - gas_cost;
        let bribe = pre_bribe.saturating_mul(90) / 100; // 90% builder bribe
        let net = (pre_bribe - bribe).max(1);
        let inr_approx = (net as f64 / 1e18) * 260_000.0;
        DecisionReport {
            status: "extractable".to_string(),
            probe_mode: mode.to_string(),
            probe_ok: true,
            probe_gas_used: probe.sim.gas_used,
            would_frontrun: true,
            gross_value_wei: gross_wei.to_string(),
            gas_cost_wei: gas_cost.to_string(),
            priority_fee_wei: outbid_tip.to_string(),
            builder_bribe_wei: bribe.to_string(),
            net_profit_wei: net.to_string(),
            probe_token_gains: probe.actor_token_transfers,
            verdict: format!(
                "READ-ONLY EXTRACTABLE ({mode}) · counterfactual replay est. +₹{inr_approx:.2} net"
            ),
        }
    } else {
        DecisionReport::not_applicable(
            "BELOW GAS COST · priority outbid gas exceeds captured value",
        )
    }
}

pub struct SimulationParams {
    pub max_lag: u64,
    pub depth: u64,
    pub timeout: Duration,
}

#[allow(clippy::too_many_arguments)]
pub async fn evaluate_transaction(
    spec: &'static SelectorSpec,
    decoded: DecodedCall,
    decode_us: u64,
    observed: ObservedTx,
    fork_status: ForkStatus,
    ref_snapshot: Option<BlockSnapshot>,
    backend: Arc<dyn StateBackend>,
    params: &SimulationParams,
) -> HitRecord {
    let eval_start = Instant::now();
    let stale = fork_status
        .lag
        .map(|l| l > params.max_lag)
        .unwrap_or(false);

    let mut fork_ev = ForkEvidence {
        block: fork_status.block,
        head: fork_status.head,
        lag: fork_status.lag,
        depth: params.depth,
        code_present: None,
        stale,
        stable: true,
        checked: Vec::new(),
        reference: None,
    };

    let Some(block_num) = fork_status.block else {
        let sim = SimOutcome {
            ok: false,
            engine: "revm".to_string(),
            gas_used: 0,
            result_or_error: "fork head unavailable".to_string(),
            revert_data: None,
            revert_kind: None,
            revert_reason: None,
            transfers: Vec::new(),
        };
        let label = classify_label(spec.name, &sim, &fork_ev);
        return finish_record(
            spec,
            decoded,
            observed,
            fork_ev,
            sim,
            DecisionReport::not_applicable("SKIPPED · state head unavailable"),
            decode_us,
            0,
            0,
            eval_start,
            label,
        );
    };

    let has_code = match tokio::time::timeout(
        params.timeout,
        backend.has_code(block_num, observed.to),
    )
    .await
    {
        Ok(Ok(present)) => present,
        // Match previous conservative behavior: inconclusive read assumes code.
        _ => true,
    };

    if !has_code {
        fork_ev.code_present = Some(false);
        let sim = SimOutcome {
            ok: false,
            engine: "revm".to_string(),
            gas_used: 0,
            result_or_error: "no contract code at target on the fork".to_string(),
            revert_data: None,
            revert_kind: None,
            revert_reason: None,
            transfers: Vec::new(),
        };
        let label = classify_label(spec.name, &sim, &fork_ev);
        return finish_record(
            spec,
            decoded,
            observed,
            fork_ev,
            sim,
            DecisionReport::not_applicable("NO CODE · target is an EOA or undeployed address"),
            decode_us,
            0,
            0,
            eval_start,
            label,
        );
    }
    fork_ev.code_present = Some(true);

    let head_snap = fork_status.snapshot.unwrap_or(BlockSnapshot {
        number: block_num,
        ..Default::default()
    });

    // Stage 4: Primary Revm simulation at `block_num`.
    let sim_t0 = Instant::now();
    let primary = match tokio::time::timeout(
        params.timeout,
        execute_revm_call(
            backend.clone(),
            &head_snap,
            observed.from,
            observed.to,
            observed.value,
            observed.calldata.clone(),
            observed.gas_limit,
            false,
        ),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_) => RawExecOutcome::failed(SimOutcome {
            ok: false,
            engine: "revm".to_string(),
            gas_used: 0,
            result_or_error: "simulation timed out".to_string(),
            revert_data: None,
            revert_kind: None,
            revert_reason: None,
            transfers: Vec::new(),
        }),
    };
    let sim_us = sim_t0.elapsed().as_micros() as u64;
    fork_ev.checked.push(block_num);

    // Block stability check at `block_num - depth`.
    if params.depth > 0 && block_num >= params.depth {
        let ref_block = block_num - params.depth;
        let rsnap = ref_snapshot.unwrap_or(BlockSnapshot {
            number: ref_block,
            timestamp: head_snap.timestamp.saturating_sub(12 * params.depth),
            ..head_snap.clone()
        });
        if let Ok(ref_outcome) = tokio::time::timeout(
            params.timeout,
            execute_revm_call(
                backend.clone(),
                &rsnap,
                observed.from,
                observed.to,
                observed.value,
                observed.calldata.clone(),
                observed.gas_limit,
                false,
            ),
        )
        .await
        {
            fork_ev.checked.push(ref_block);
            fork_ev.reference = Some(ReferenceCheck {
                block: ref_block,
                ok: ref_outcome.sim.ok,
            });
            fork_ev.stable = ref_outcome.sim.ok == primary.sim.ok;
        }
    }

    // Stage 5: Generalized front-runnability probe in Revm (warm cache -> sub-millisecond).
    let probe_t0 = Instant::now();
    let decision = if !primary.sim.ok {
        DecisionReport::not_applicable("SKIPPED · victim call reverted on fork")
    } else if stale {
        DecisionReport::not_applicable("SKIPPED · fork lag exceeds threshold")
    } else {
        // Pass A: Caller substitution (`caller = PROBE_ADDRESS`, original calldata).
        let pass_a = tokio::time::timeout(
            params.timeout,
            execute_revm_call(
                backend.clone(),
                &head_snap,
                PROBE_ADDRESS,
                observed.to,
                observed.value,
                observed.calldata.clone(),
                observed.gas_limit,
                true,
            ),
        )
        .await
        .ok();

        let a_captured = pass_a
            .as_ref()
            .map(|p| p.sim.ok && p.net_inflow_wei() > 0)
            .unwrap_or(false);

        if a_captured {
            score_probe(
                "caller_substitution",
                pass_a.as_ref().unwrap(),
                head_snap.base_fee,
                observed.tip_wei,
            )
        } else if let Some(rewritten_data) =
            rewrite_recipient_calldata(spec, &observed.calldata, PROBE_ADDRESS)
        {
            // Pass B: Recipient rewrite (`caller = PROBE_ADDRESS`, `recipient = PROBE_ADDRESS`).
            let pass_b = tokio::time::timeout(
                params.timeout,
                execute_revm_call(
                    backend.clone(),
                    &head_snap,
                    PROBE_ADDRESS,
                    observed.to,
                    observed.value,
                    rewritten_data,
                    observed.gas_limit,
                    true,
                ),
            )
            .await
            .ok();

            let b_captured = pass_b
                .as_ref()
                .map(|p| p.sim.ok && p.net_inflow_wei() > 0)
                .unwrap_or(false);

            if b_captured {
                score_probe(
                    "recipient_rewrite",
                    pass_b.as_ref().unwrap(),
                    head_snap.base_fee,
                    observed.tip_wei,
                )
            } else if let Some(pa) = pass_a.as_ref() {
                // If Pass A succeeded without capturing (e.g. Merkle proof bound to victim),
                // report `beneficiary_locked`; otherwise report `sender_protected`.
                score_probe(
                    "caller_substitution",
                    pa,
                    head_snap.base_fee,
                    observed.tip_wei,
                )
            } else {
                DecisionReport::not_applicable("PROBE TIMEOUT")
            }
        } else if let Some(pa) = pass_a.as_ref() {
            score_probe(
                "caller_substitution",
                pa,
                head_snap.base_fee,
                observed.tip_wei,
            )
        } else {
            DecisionReport::not_applicable("PROBE TIMEOUT")
        }
    };
    let probe_us = probe_t0.elapsed().as_micros() as u64;

    let label = classify_label(spec.name, &primary.sim, &fork_ev);
    finish_record(
        spec,
        decoded,
        observed,
        fork_ev,
        primary.sim,
        decision,
        decode_us,
        sim_us,
        probe_us,
        eval_start,
        label,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish_record(
    spec: &'static SelectorSpec,
    decoded: DecodedCall,
    observed: ObservedTx,
    fork: ForkEvidence,
    sim: SimOutcome,
    decision: DecisionReport,
    decode_us: u64,
    sim_us: u64,
    probe_us: u64,
    eval_start: Instant,
    label: String,
) -> HitRecord {
    let eval_us = eval_start.elapsed().as_micros() as u64;
    let total_us = observed.ingest_us + decode_us + eval_us;
    HitRecord {
        name: spec.name.to_string(),
        hash: observed.hash,
        from: format!("{:#x}", observed.from),
        to: format!("{:#x}", observed.to),
        nonce: observed.nonce,
        value: observed.value.to_string(),
        selector: observed.selector_hex,
        gas: observed.gas_limit,
        tip: observed.tip_wei,
        ts: observed.timestamp_iso,
        fork,
        sim,
        decoded,
        decision,
        telemetry: StageTelemetry {
            source: observed.source.to_string(),
            ingest_us: observed.ingest_us,
            decode_us,
            sim_us,
            probe_us,
            total_us,
        },
        label,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::{decode_calldata, match_calldata};
    use crate::state::InMemoryStateBackend;
    use revm::primitives::StorageKey;

    fn base_status(block: u64) -> ForkStatus {
        ForkStatus {
            block: Some(block),
            head: Some(block),
            lag: Some(0),
            snapshot: Some(BlockSnapshot {
                number: block,
                timestamp: 1_794_560_000,
                base_fee: 1_000_000_000, // 1 gwei
                beneficiary: Address::ZERO,
                gas_limit: 36_000_000,
                prevrandao: Some(B256::ZERO),
                excess_blob_gas: Some(0),
            }),
        }
    }

    /// Minimal EVM runtime bytecode for an UNGUARDED `claim()` faucet:
    /// `if (sload(0) != 0) revert; sstore(0, 1); call(gas, caller, selfbalance, 0, 0, 0, 0); stop`
    fn unguarded_claim_bytecode() -> Vec<u8> {
        // PUSH1 0x00 SLOAD ISZERO PUSH1 0x0b JUMPI PUSH1 0x00 DUP1 REVERT
        // JUMPDEST PUSH1 0x01 PUSH1 0x00 SSTORE
        // PUSH1 0x00 DUP1 DUP1 DUP1 SELFBALANCE CALLER GAS CALL STOP
        hex::decode("60005415600b57600080fd5b6001600055600080808047335af100").unwrap()
    }

    /// Minimal EVM runtime bytecode for a SENDER-PROTECTED `claim()`:
    /// Requires `caller == sload(0)` (authorized owner), else reverts; sends 1 ETH to caller.
    fn sender_protected_claim_bytecode() -> Vec<u8> {
        // PUSH1 0x00 SLOAD CALLER EQ PUSH1 0x0c JUMPI PUSH1 0x00 DUP1 REVERT
        // JUMPDEST PUSH1 0x00 DUP1 DUP1 DUP1 SELFBALANCE CALLER GAS CALL STOP
        hex::decode("6000543314600c57600080fd5b600080808047335af100").unwrap()
    }

    /// Minimal EVM runtime bytecode for a BENEFICIARY-LOCKED `claim(address,uint256)`:
    /// Anyone may relay the call, BUT `calldataload(4)` MUST equal `sload(0)` (locked victim)
    /// and ETH is sent to `sload(0)` (the victim), never to `caller`!
    fn beneficiary_locked_claim_bytecode() -> Vec<u8> {
        // PUSH1 0x04 CALLDATALOAD PUSH1 0x00 SLOAD EQ PUSH1 0x0e JUMPI PUSH1 0x00 DUP1 REVERT
        // JUMPDEST PUSH1 0x00 DUP1 DUP1 DUP1 SELFBALANCE PUSH1 0x00 SLOAD GAS CALL STOP
        hex::decode("60043560005414600e57600080fd5b6000808080476000545af100").unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn detects_unguarded_extractable_claim_and_scores_profit() {
        let backend = Arc::new(InMemoryStateBackend::default());
        let target = address!("aaaa000000000000000000000000000000000001");
        let victim = address!("bbbb000000000000000000000000000000000002");
        let one_eth = U256::from(1_000_000_000_000_000_000u128);

        backend.insert_account(target, one_eth, Some(unguarded_claim_bytecode()));
        backend.insert_account(victim, one_eth, None);

        let calldata = Bytes::from(vec![0x4e, 0x71, 0xd9, 0x2d]); // claim()
        let spec = match_calldata(&calldata).unwrap();
        let decoded = decode_calldata(spec, &calldata, U256::ZERO);
        let observed = ObservedTx {
            hash: "0x1111".to_string(),
            from: victim,
            to: target,
            nonce: Some(0),
            value: U256::ZERO,
            gas_limit: 150_000,
            tip_wei: Some(1_000_000_000),
            max_fee_wei: Some(20_000_000_000),
            calldata,
            selector_hex: spec.selector_hex.clone(),
            timestamp_iso: "2026-11-13T12:00:00.000Z".to_string(),
            ingest_us: 120,
            source: "ws_full",
        };

        let record = evaluate_transaction(
            spec,
            decoded,
            8,
            observed,
            base_status(21_000_000),
            None,
            backend,
            &SimulationParams {
                max_lag: 3,
                depth: 1,
                timeout: Duration::from_secs(2),
            },
        )
        .await;

        assert!(record.sim.ok);
        assert_eq!(record.label, "candidate:claim");
        assert_eq!(record.decision.status, "extractable");
        assert_eq!(record.decision.probe_mode, "caller_substitution");
        assert!(record.decision.would_frontrun);
        assert_eq!(record.decision.gross_value_wei, one_eth.to_string());
        let net: u128 = record.decision.net_profit_wei.parse().unwrap();
        assert!(net > 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn classifies_sender_protected_contract_accurately() {
        let backend = Arc::new(InMemoryStateBackend::default());
        let target = address!("aaaa000000000000000000000000000000000003");
        let victim = address!("bbbb000000000000000000000000000000000004");
        let one_eth = U256::from(1_000_000_000_000_000_000u128);

        backend.insert_account(target, one_eth, Some(sender_protected_claim_bytecode()));
        backend.insert_storage(
            target,
            StorageKey::ZERO,
            U256::from_be_slice(victim.as_slice()),
        );
        backend.insert_account(victim, one_eth, None);

        let calldata = Bytes::from(vec![0x4e, 0x71, 0xd9, 0x2d]);
        let spec = match_calldata(&calldata).unwrap();
        let decoded = decode_calldata(spec, &calldata, U256::ZERO);
        let observed = ObservedTx {
            hash: "0x2222".to_string(),
            from: victim,
            to: target,
            nonce: Some(0),
            value: U256::ZERO,
            gas_limit: 150_000,
            tip_wei: Some(1_000_000_000),
            max_fee_wei: Some(20_000_000_000),
            calldata,
            selector_hex: spec.selector_hex.clone(),
            timestamp_iso: "2026-11-13T12:00:00.000Z".to_string(),
            ingest_us: 90,
            source: "ws_full",
        };

        let record = evaluate_transaction(
            spec,
            decoded,
            5,
            observed,
            base_status(21_000_000),
            None,
            backend,
            &SimulationParams {
                max_lag: 3,
                depth: 1,
                timeout: Duration::from_secs(2),
            },
        )
        .await;

        assert!(record.sim.ok);
        assert_eq!(record.label, "candidate:claim");
        assert_eq!(record.decision.status, "sender_protected");
        assert!(!record.decision.would_frontrun);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn distinguishes_beneficiary_locked_merkle_style_claim() {
        let backend = Arc::new(InMemoryStateBackend::default());
        let target = address!("aaaa000000000000000000000000000000000005");
        let victim = address!("bbbb000000000000000000000000000000000006");
        let one_eth = U256::from(1_000_000_000_000_000_000u128);

        backend.insert_account(target, one_eth, Some(beneficiary_locked_claim_bytecode()));
        backend.insert_storage(
            target,
            StorageKey::ZERO,
            U256::from_be_slice(victim.as_slice()),
        );
        backend.insert_account(victim, one_eth, None);

        // ABI encode `claim(address,uint256)` -> selector `0xaad3ec96`
        let mut raw = vec![0xaa, 0xd3, 0xec, 0x96];
        let mut word1 = [0u8; 32];
        word1[12..32].copy_from_slice(victim.as_slice());
        raw.extend_from_slice(&word1);
        raw.extend_from_slice(&one_eth.to_be_bytes::<32>());
        let calldata = Bytes::from(raw);

        let spec = match_calldata(&calldata).unwrap();
        let decoded = decode_calldata(spec, &calldata, U256::ZERO);
        let observed = ObservedTx {
            hash: "0x3333".to_string(),
            from: victim,
            to: target,
            nonce: Some(0),
            value: U256::ZERO,
            gas_limit: 150_000,
            tip_wei: Some(1_000_000_000),
            max_fee_wei: Some(20_000_000_000),
            calldata,
            selector_hex: spec.selector_hex.clone(),
            timestamp_iso: "2026-11-13T12:00:00.000Z".to_string(),
            ingest_us: 75,
            source: "ws_full",
        };

        let record = evaluate_transaction(
            spec,
            decoded,
            6,
            observed,
            base_status(21_000_000),
            None,
            backend,
            &SimulationParams {
                max_lag: 3,
                depth: 1,
                timeout: Duration::from_secs(2),
            },
        )
        .await;

        assert!(record.sim.ok);
        // Probe Pass A succeeds on-chain, Pass B (recipient rewrite) reverts,
        // and PROBE_ADDRESS receives 0 -> accurately marked `beneficiary_locked`!
        assert_eq!(record.decision.status, "beneficiary_locked");
        assert!(record.decision.probe_ok);
        assert!(!record.decision.would_frontrun);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn payable_swap_spending_own_eth_is_not_falsely_extractable() {
        let backend = Arc::new(InMemoryStateBackend::default());
        let target = address!("aaaa000000000000000000000000000000000099");
        let sender = address!("bbbb000000000000000000000000000000000099");
        let one_eth = U256::from(1_000_000_000_000_000_000u128);
        let tenth_eth = U256::from(100_000_000_000_000_000u128);

        // Emits ERC-20 Transfer(0x0, msg.sender, 1000) while keeping caller's msg.value.
        let swap_like_code = hex::decode(
            "6103e86000523360007fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef60206000a300",
        )
        .unwrap();

        backend.insert_account(target, U256::ZERO, Some(swap_like_code));
        backend.insert_account(sender, one_eth, None);

        // UniversalRouter.execute(bytes,bytes[]) selector `0x24856bc3`
        let calldata = Bytes::from(vec![0x24, 0x85, 0x6b, 0xc3]);
        let spec = match_calldata(&calldata).unwrap();
        let decoded = decode_calldata(spec, &calldata, tenth_eth);
        let observed = ObservedTx {
            hash: "0x4444".to_string(),
            from: sender,
            to: target,
            nonce: Some(1),
            value: tenth_eth,
            gas_limit: 180_000,
            tip_wei: Some(1_000_000_000),
            max_fee_wei: Some(20_000_000_000),
            calldata,
            selector_hex: spec.selector_hex.clone(),
            timestamp_iso: "2026-11-13T12:00:00.000Z".to_string(),
            ingest_us: 80,
            source: "ws_full",
        };

        let record = evaluate_transaction(
            spec,
            decoded,
            5,
            observed,
            base_status(21_000_000),
            None,
            backend,
            &SimulationParams {
                max_lag: 3,
                depth: 1,
                timeout: Duration::from_secs(2),
            },
        )
        .await;

        assert!(record.sim.ok);
        assert_eq!(record.decision.status, "not_applicable");
        assert!(!record.decision.would_frontrun);
        assert_eq!(record.decision.gross_value_wei, "0");
    }
}
