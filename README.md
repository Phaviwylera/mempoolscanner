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
| **5. Access-Control Replay & Scoring** | `src/sim.rs` | Runs read-only counterfactual caller-substitution and ABI recipient-rewrite replays in ephemeral `revm` `CacheDB` overlays using a dummy address (`0x0000…00c0ffee`), classifying each call as `extractable`, `beneficiary_locked`, or `sender_protected` with off-chain gas and value estimates (never signed or broadcast). |

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

## Execution module (opt-in)

The `src/exec/` module is an **optional, feature-flagged** execution layer that lets the
operator act on `liquidate` opportunities they themselves detect, using their own wallet and
capital. It is **off by default** and introduces no keys, signing, or broadcasting unless
`--execute` is passed.

### Flags

| Flag | Effect |
| --- | --- |
| `--execute` | Opt into the execution module (default OFF). Without it, the binary is byte-identical to the read-only build. |
| `--dry-run` | Requires `--execute`. Signs transactions but never broadcasts (testnet/dev safety valve). |

### Environment variables

| Variable | Required | Effect |
| --- | --- | --- |
| `EXEC_PRIVATE_KEY` | one of the two | Raw 32-byte private key as hex. Mutually exclusive with keystore. |
| `EXEC_KEYSTORE_PATH` | one of the two | Path to an encrypted JSON keystore (decrypted with `EXEC_KEYSTORE_PASSWORD`). |
| `EXEC_KEYSTORE_PASSWORD` | with keystore | Password to decrypt the keystore. |
| `EXEC_RPC_URL` | yes | State RPC used for the pre-sign re-simulation and confirmation polling. |
| `EXEC_CHAIN_ID` | no (default `1`) | Chain ID the signer is bound to (EIP-155 replay protection). |
| `FLASHBOTS_RELAY` | no | Flashbots Protect / MEV-Share relay URL (primary broadcast path). |
| `BUILDER_RELAYS` | no | Comma-separated builder-relay URLs (secondary fan-out path). |
| `EXEC_MIN_PROFIT_WEI` | no (default `0`) | Minimum net profit (wei) required to sign. |
| `EXEC_CONFIRMATIONS` | no (default `1`) | Blocks to wait before returning `Confirmed`. |
| `EXEC_AUDIT_DIR` | no (default `logs`) | Directory for `exec_audit.jsonl`. |

### Safety rails

1. `--execute` defaults to `false`; without it the binary behaves exactly as before.
2. The simulation gate (`src/exec/simulate.rs`) is **non-bypassable**: a transaction is signed
   only after a profitable re-simulation at `latest_block + 1` reports `net_profit >= min_profit_wei`
   (net = gross − gas_cost − builder_bid).
3. The signer is loaded **only** from `EXEC_PRIVATE_KEY` or `EXEC_KEYSTORE_PATH` +
   `EXEC_KEYSTORE_PASSWORD`. It is never accepted as a CLI flag, config-file field, or function
   argument, and key bytes are zeroized on drop (`zeroize`).
4. Broadcasting is **private-relay only**: Flashbots/MEV-Share first, then builder relays. With no
   private relay configured the tx is `Skipped("no_private_relay")`. The public mempool is never
   used (`eth_sendRawTransaction` exists only inside `src/exec/broadcast.rs`).
5. If `--execute` is passed and no key material is present in the environment, the process exits
   with a clear error before touching the network.
6. Every signed tx is appended to `exec_audit.jsonl` with timestamp, chain_id, opportunity id,
   profit estimate, tx hash, and outcome.

### Tests

- `src/exec/simulate.rs` — unit tests against `revm` with an in-memory mocked state (no network).
- `tests/exec_sepolia.rs` — `#[ignore]` integration test; runs only when `SEPOLIA_RPC_URL` is set,
  asserts the target is Sepolia (never mainnet), and that the gate rejects an unprofitable
  opportunity.
