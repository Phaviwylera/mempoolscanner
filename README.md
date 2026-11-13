# Mempool Scanner (Rust · Alloy · Revm)

A high-throughput, strictly **read-only** Ethereum pending-transaction observer and vulnerability/opportunity simulator built in Rust with **Alloy** and in-process **Revm**.

The scanner never holds keys, never signs transactions, and never constructs or broadcasts live bundles. It executes all simulations inside ephemeral in-memory `CacheDB` overlays to measure execution validity, state stability, generalized front-runnability, and off-chain net profit.

## 5-Stage Pipeline Architecture

| Stage | Module | What it does |
| --- | --- | --- |
| **1. Ingestion** | `src/pipeline.rs` | Connects via Alloy over WebSocket (`WS_RPC`) or local IPC (`IPC_RPC`). Subscribes to full pending transaction bodies (`eth_subscribe("newPendingTransactions", true)`) for 0-hop ingestion, automatically falling back to hash streams + bounded concurrent fetch if needed. |
| **2. ABI Decoding** | `src/abi.rs` | Decodes calldata with compile-time `alloy::sol!` `SolCall` types across 34 DeFi selectors (claims/airdrops, vault/Aave withdrawals, Uniswap Universal Router / V2 / V3 swaps, and Compound/Aave liquidations), plus `Error(string)`, `Panic(uint256)`, and custom 4-byte reverts. |
| **3. Pattern Match** | `src/abi.rs` | O(1) `[u8; 4]` selector table classifying targets into `claim`, `withdraw`, `swap`, and `liquidate`. |
| **4. Revm Simulation** | `src/sim.rs`, `src/state.rs` | Executes the pending call in-process with `revm` (`v43`) against pinned upstream state (`HTTP_RPC` or `ANVIL_RPC`) backed by a shared multi-block bytecode/account/storage cache, plus a stability replay at `block - FORK_CHECK_DEPTH`. |
| **5. Probe & Profit Scoring** | `src/sim.rs` | Runs synthetic caller-substitution and ABI recipient-rewrite probes in `revm` using a non-existent virtual probe address (`0x0000…00c0ffee`), inspects native ETH state diffs and ERC-20 `Transfer` logs, and classifies each call as `extractable`, `beneficiary_locked`, or `sender_protected` with gas, 90% builder bribe, and net profit estimates. |

## Local Setup & Build

### Requirements

- Rust toolchain (`rustup`, Rust 1.94+)
- Python 3.10+ (for the read-only loopback JSONL API and dashboard tests)
- A WebSocket RPC (`WS_RPC`, or `IPC_RPC` for a local node) and HTTP RPC (`HTTP_RPC`)

```bash
cp .env.example .env
# Edit WS_RPC and HTTP_RPC in .env
cargo test --all-targets
cargo run --release
```

Inspect watched selectors:

```bash
cargo run --release -- --dump-selectors
```

Watch live JSONL observations:

```bash
tail -f logs/hits.jsonl | jq -c '{name, label, status: .decision.status, profit: .decision.net_profit_wei, sim_us: .telemetry.sim_us}'
```

## Configuration

| Variable | Default | Effect |
| --- | --- | --- |
| `WS_RPC` | — | WebSocket RPC endpoint for pending transaction subscriptions. |
| `IPC_RPC` | unset | Optional local IPC socket path (overrides `WS_RPC` for pubsub when set). |
| `HTTP_RPC` | — | Primary HTTP RPC used by in-process `revm` for pinned block state reads (`lag = 0`). |
| `ANVIL_RPC` | unset | Fallback state RPC when `HTTP_RPC` is unset (or when `PREFER_ANVIL=1`). |
| `FORK_MAX_LAG` | `3` | Maximum block lag before an observation is labelled `stale_fork`. |
| `FORK_CHECK_DEPTH` | `1` | Blocks below head replayed for the `state_dependent` stability check. |
| `FORK_POLL_SECONDS` | `3` | Interval for refreshing latest block header and lag metrics. |
| `WORKER_CONCURRENCY` | `16` | Maximum concurrent `revm` simulation tasks. |
| `SIM_TIMEOUT_MS` | `6000` | Per-simulation timeout guard in milliseconds. |

## Front-Run Probe & Fork Labels

### Fork Quality Labels (`label`)

- `candidate:<name>` — Succeeded at the state head and at `block - depth`.
- `state_dependent:<name>` — Succeeded at head, but differed at `block - depth`.
- `stale_fork:<name>` — State source trailed upstream head beyond `FORK_MAX_LAG`.
- `no_code:<name>` — Target held no contract bytecode (EOA or undeployed target).
- `sim_failed:<name>:<reason>` — Reverted or halted in `revm` (`sim.revert_reason` carries decoded text).

### Generalized Front-Runnability Status (`decision.status`)

- `extractable` — Synthetic probe captured positive native ETH or ERC-20 tokens via caller substitution or ABI recipient rewrite (`decision.would_frontrun` indicates whether gross value exceeds outbid gas + 90% builder bribe).
- `beneficiary_locked` — Probe call succeeded without reverting, **but** tokens/ETH still went to the original recipient (`0` value captured by probe — e.g. Merkle claims where proof binds the payout address).
- `sender_protected` — Probe call reverted when `msg.sender` / recipient changed.
- `not_applicable` — Primary simulation reverted, target had no code, or state was stale.

## EC2 Dashboard & Automated Deployment

The Ohio EC2 instance serves `web/` through Nginx and proxies `/api/hits` and `/api/quality` (`filter=all|ok|extractable|failed`) to `deploy/hits_api.py` on `127.0.0.1:8765`.

On every push to `main`, `.github/workflows/deploy.yml`:
1. Runs Python API unit tests and `cargo test --all-targets` (including in-memory `revm` contract tests).
2. Compiles a static `x86_64-unknown-linux-musl` `mempool-scanner` binary and publishes a checksummed build release.
3. Deploys the commit to EC2 over AWS Systems Manager (`deploy/setup_host.sh` + `deploy/deploy_release.sh`).
4. Runs `deploy/verify_release.sh` to verify `/api/quality`, `mempolscan.service`, and live JSONL observations.

## Responsible Use

Use only for authorized smart-contract security research and defensive observability. Coordinate third-party vulnerability disclosures through the protocol's security contact, Immunefi, or SEAL 911. Never take unauthorized on-chain action based on monitor output.
