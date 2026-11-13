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

Install Foundry and start an Ethereum mainnet fork in a separate terminal:

```bash
curl -L https://foundry.paradigm.xyz | bash && foundryup
set -a; source .env; set +a
anvil --fork-url "$HTTP_RPC" --fork-block-number latest --port 8545
```

Start the monitor from the repository directory:

```bash
source venv/bin/activate
python mempool_scanner.py
```

The monitor subscribes to pending transaction hashes, fetches their read-only transaction data, matches configured selectors, and invokes the same calldata through `eth_call` on `ANVIL_RPC`. It records matched transactions in `logs/hits.jsonl` and writes the same hit as a structured log line to stdout.

```bash
tail -f logs/hits.jsonl | jq -c '{name, from, to, value, label}'
```

`tip` is the transaction's reported `maxPriorityFeePerGas` in wei when the provider exposes it; it is `null` for transactions without that field. `gas` is the transaction gas limit. `value` is in wei.

## Selector configuration

Edit `SIGNATURES` in `mempool_scanner.py` to add or remove human-readable function signatures. `WATCH` is derived from those signatures with `eth_utils.keccak`, taking the first four bytes.

The built-in patterns cover common `claim`, `withdraw`, Uniswap V2/V3 swap, and `liquidate` calls. Function names and tuple layouts can vary across contract versions; validate the signature against the authorized target's ABI before relying on a match.

## Shutdown

Press Ctrl-C. The monitor attempts to unsubscribe, flush queued JSONL records, and disconnect both RPC providers.

## Responsible use

Use only for contracts and bug-bounty programs you are authorized to test. Findings on third-party contracts should go through coordinated disclosure using the project's security contact, Immunefi, or SEAL 911. Do not take on-chain action based on this monitor's output.



