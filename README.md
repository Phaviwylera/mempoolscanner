# Mempool Scanner

A read-only Ethereum pending-transaction observer for authorized smart-contract security research. It watches configured four-byte selectors, replays matching calldata with `eth_call` against a local Anvil fork, and records the outcome as JSON Lines.

The monitor does not sign, construct, or broadcast transactions. It stops at fork simulation by design. A `candidate:<name>` label means only that the observed call returned successfully against the fork state at simulation time; it is not proof that a third-party contract is vulnerable or that a later transaction would succeed. The local fork is a snapshot and pending transactions may be missing or private from the selected provider.

## Requirements

- Python 3.10 or later
- Foundry (`anvil`)
- A WebSocket RPC endpoint that supports `newPendingTransactions` and transaction lookup
- An HTTP RPC endpoint for the Anvil fork

## Setup and run

```bash
python3 -m venv venv && source venv/bin/activate
pip install -r requirements.txt
cp .env.example .env
```

Set `WS_RPC`, `HTTP_RPC`, and `ANVIL_RPC` in `.env`. The example uses public Ethereum endpoints for learning; providers may rate-limit requests or omit mempool subscriptions. Use an endpoint that supports the pending transaction subscription for the chain you are authorized to observe.

Optional fork-trust settings, shown with their defaults:

| Variable | Default | Effect |
| --- | --- | --- |
| `FORK_MAX_LAG` | `3` | How many blocks the fork may trail the provider head before a result is labelled `stale_fork`. |
| `FORK_CHECK_DEPTH` | `1` | How many blocks below the fork head are replayed for the stability check. `0` replays only at the head. |
| `FORK_POLL_SECONDS` | `3` | How often the fork and provider heads are re-read. |

Install Foundry and start an Ethereum mainnet fork in a separate terminal:

```bash
curl -L https://foundry.paradigm.xyz | bash && foundryup
set -a; source .env; set +a
anvil --fork-url "$HTTP_RPC" --port 8545
```

Anvil follows the latest available block by default when no fork block is specified.

Start the monitor from the repository directory:

```bash
source venv/bin/activate
python mempool_scanner.py
```

The monitor subscribes to pending transaction hashes, fetches their read-only transaction data, matches configured selectors, and invokes the same calldata through `eth_call` on `ANVIL_RPC`. Each replay is pinned to an explicit fork block instead of `pending`, so a recorded hit can be reproduced later. It records matched transactions in `logs/hits.jsonl` and writes the same hit as a structured log line to stdout.

```bash
tail -f logs/hits.jsonl | jq -c '{name, from, to, value, label}'
```

`tip` is the transaction's reported `maxPriorityFeePerGas` in wei when the provider exposes it; it is `null` for transactions without that field. `gas` is the transaction gas limit. `value` is in wei.

### What the label means

Every replay records the `fork` block it ran against (`block`), the upstream head at that moment (`head`), the lag between them (`lag`), the stability depth (`depth`), whether the target held contract code (`code_present`), and the blocks it compared (`checked`). The label states how much weight the result carries:

| Label | Meaning |
| --- | --- |
| `candidate:<name>` | The call succeeded at the fork head and at the reference block. |
| `state_dependent:<name>` | The call succeeded at the fork head but its outcome differs at `block - depth`, so the result depends on which block is simulated. |
| `stale_fork:<name>` | The fork trails the provider head by more than `FORK_MAX_LAG` blocks, so the observation may describe older state. |
| `no_code:<name>` | The target held no contract code on the fork. A call to a non-contract returns empty data without reverting, so it is never a real match. |
| `sim_failed:<name>:<reason>` | The replay reverted. `sim.revert_reason` carries the decoded `Error(string)` or `Panic(uint256)` text when the provider returned it. |

Only `candidate:` shows that the fork state allowed the call at simulation time. It is still not proof that a third-party contract is vulnerable, and it is not a signal to take on-chain action.

## Reading fork quality

A `candidate:` label is only worth as much as the fork it came from. The dashboard reads the `fork` object on each record so a reader can tell a live-state success from an artifact of a stale or limited snapshot.

### Fork fields

| Field | Meaning | What to do about it |
| --- | --- | --- |
| `fork.block` | The block the replay was pinned to. | Quote it in a write-up; replaying that block reproduces the result. |
| `fork.head` | The upstream head when the block numbers were last read. | If it is well ahead of `block`, the fork is behind. |
| `fork.lag` | `head - block`, floored at zero. `null` when either head was unreadable. | Anything above `FORK_MAX_LAG` is untrustworthy, which the scanner already labels `stale_fork`. |
| `fork.depth` | How many blocks below the head are replayed. | `0` means no stability check ran. |
| `fork.code_present` | Whether the target held contract code at that block. | `false` means the call could not have been real: an EOA or empty address returns success. |
| `fork.checked` | The blocks actually replayed. | A single entry means only the head was replayed. |
| `fork.reference` | `{block, ok}` for the stability read, or `null`. | A different `ok` from `sim.ok` is why a record reads `state_dependent`. |

Records written before this field existed carry no `fork` object. The dashboard shows them as "not recorded" and leaves them out of the percentages rather than counting them as healthy.

### Quality labels

| Label | Meaning | What to do about it |
| --- | --- | --- |
| `candidate:` | Succeeded at the head and at the reference block. | The only label describing a live-state success. Still not proof of a vulnerability. |
| `state_dependent:` | Succeeded at the head but differed below it. | Re-check by hand: the answer depends on which block is simulated. |
| `stale_fork:` | The fork trailed the provider beyond `FORK_MAX_LAG`. | Restart Anvil and re-observe before trusting that window. |
| `no_code:` | The target held no bytecode. | Not a finding; usually a selector collision or a call to an EOA. |
| `sim_failed:` | The replay reverted. | `sim.revert_reason` carries the decoded `Error(string)` or `Panic(uint256)` text when the provider returned it. |

### What the dashboard shows

- **Fork-health banner.** One line over the newest 50 records. `FORK HEALTH: GREEN` when every record has `lag <= FORK_MAX_LAG` and `code_present`; `AMBER` when any `stale_fork`, `no_code`, or record without fork evidence appears; `RED` when more than 20% of the window is `stale_fork`. It also reads `RED` and reports "monitor may be stalled" when the newest record is older than five times `FORK_POLL_SECONDS`, the failure mode where a dead fork looks healthy because nothing is arriving. The state is always spelled out in text, so it does not depend on colour.
- **Fork-quality rollup.** Counts and percentages per label over the current filter window, median and maximum lag, and the share of records where the stability check ran. When no record in the range carries fork evidence it says "fork data unavailable for this range" instead of showing zeros, because zeros would read as "everything is fine".
- **Per-hit fork evidence.** The latest-match panel lists that record's `fork` fields with a stale lag marked, and shows `stale_fork`, `no_code`, and `state_dependent` with the same weight as `candidate`.

Percentages use the records that actually carry fork evidence as their base; the rollup reports `total` and `with_fork` alongside them so the base is never implied.

### Quality endpoint

`GET /api/quality?limit=100&filter=all` is read-only. `filter` accepts `all`, `ok`, or `failed`, matching the dashboard toggle. `GET /api/hits` is unchanged, byte for byte.

```json
{
  "window": {"requested": 100, "returned": 100, "filtered": 100, "filter": "all", "banner_window": 50, "max_lag": 3, "poll_seconds": 3.0},
  "banner": {"state": "amber", "text": "1 stale_fork, 1 no_code in the last 50 observations", "window": 50, "stalled": false, "lag": 1, "max_observed_lag": 9, "max_lag": 3, "newest_ts": "2026-11-13T12:00:00.000+00:00", "counts": {"stale_fork": 1, "no_code": 1, "missing_fork": 0}},
  "rollup": {"available": true, "total": 100, "with_fork": 92, "counts": {"candidate": 60, "state_dependent": 12, "sim_failed": 8, "no_code": 5, "stale_fork": 7, "other": 0}, "percent": {"candidate": 65.2, "state_dependent": 13.0, "sim_failed": 8.7, "no_code": 5.4, "stale_fork": 7.6, "other": 0.0}, "lag": {"median": 1.0, "max": 9}, "checked_share": 87.0, "note": null}
}
```

The API service reads `FORK_MAX_LAG` and `FORK_POLL_SECONDS` from the same `.env` the scanner uses, so the thresholds it reports are the ones actually running. Both fall back to the documented defaults when absent or invalid.

### Verification status

Two RPC shapes remain unverified end-to-end against a live endpoint:

- `eth_call` with an explicit integer block against a forked Anvil.
- `eth_blockNumber` on the persistent WebSocket provider while `process_subscriptions()` is iterating, which is how the provider head and therefore `fork.lag` are measured.

The dashboard reports what those calls return and does not assume a shape for either. If the provider head cannot be read, `fork.head` and `fork.lag` are `null`, the record is never treated as healthy, and the banner reports amber rather than green.

## Selector configuration

Edit `SIGNATURES` in `mempool_scanner.py` to add or remove human-readable function signatures. `WATCH` is derived from those signatures with `eth_utils.keccak`, taking the first four bytes.

The built-in patterns cover common `claim`, `withdraw`, Uniswap V2/V3 swap, and `liquidate` calls. Function names and tuple layouts can vary across contract versions; validate the signature against the authorized target's ABI before relying on a match.

## Shutdown

Press Ctrl-C. The monitor attempts to unsubscribe, flush queued JSONL records, and disconnect both RPC providers.

## Public dashboard

The Ohio EC2 deployment serves the read-only dashboard at `http://<instance-public-ip>/`. It polls `/api/hits` for the recent observations and `/api/quality` for fork quality every five seconds; both endpoints read the latest log entries and expose no write or transaction-broadcast operation. Each push to `main` is deployed to the instance through GitHub Actions and AWS Systems Manager.

### Deployment verification

After deploying, the workflow runs `deploy/verify_release.sh` on the instance over SSM. It:

- fetches `http://127.0.0.1/api/quality?limit=1` through nginx and requires a `banner.state` of `green`, `amber`, `red`, or `unknown`, which also proves the `/api/quality` proxy location is in place;
- requires `mempolscan.service` to be active;
- waits up to 180 seconds for a record to be appended to `logs/hits.jsonl` and fails if the count does not move, printing the current banner and the last 60 journal lines as diagnostics.

The wait is measured against the record count at the start of verification, so observations left over from an earlier deploy cannot make a broken scanner look healthy. A failure marks the workflow red but does not roll back: the new commit stays live until an earlier one is deployed.

Run the same check by hand with a longer window:

```bash
sudo bash /opt/mempoolscanner/deploy/verify_release.sh 600
```

For a dry run off-instance, override `MEMPOLSCAN_ROOT`, `MEMPOLSCAN_QUALITY_URL` and `MEMPOLSCAN_SKIP_SERVICE_CHECK`.

## Responsible use

Use only for contracts and bug-bounty programs you are authorized to test. Findings on third-party contracts should go through coordinated disclosure using the project's security contact, Immunefi, or SEAL 911. Do not take on-chain action based on this monitor's output.



