"""Read-only pending-transaction observer with local-fork call simulation.

Every RPC this monitor sends is a read: eth_subscribe, eth_getTransactionByHash,
eth_blockNumber, eth_getCode and eth_call. It never signs, constructs, or
broadcasts a transaction, and it stops at fork simulation by design.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import logging
import os
import queue as thread_queue
import sys
from dataclasses import dataclass
from datetime import datetime, timezone
from logging.handlers import QueueHandler, QueueListener
from pathlib import Path
from typing import Any

from dotenv import load_dotenv
from eth_utils import keccak
from web3 import AsyncWeb3
from web3.providers import AsyncHTTPProvider, WebsocketProviderV2


SIGNATURES = {
    "claim": "claim()",
    "claim_uint256": "claim(uint256)",
    "claim_address_uint256": "claim(address,uint256)",
    "claim_address_uint256_bytes32_array": "claim(address,uint256,bytes32[])",
    "withdraw_uint256": "withdraw(uint256)",
    "uniswap_v2_swap_exact_tokens_for_tokens": "swapExactTokensForTokens(uint256,uint256,address[],address,uint256)",
    "uniswap_v2_swap_tokens_for_exact_tokens": "swapTokensForExactTokens(uint256,uint256,address[],address,uint256)",
    "uniswap_v2_swap_exact_eth_for_tokens": "swapExactETHForTokens(uint256,address[],address,uint256)",
    "uniswap_v2_swap_tokens_for_exact_eth": "swapTokensForExactETH(uint256,uint256,address[],address,uint256)",
    "uniswap_v2_swap_exact_tokens_for_eth": "swapExactTokensForETH(uint256,uint256,address[],address,uint256)",
    "uniswap_v2_swap_eth_for_exact_tokens": "swapETHForExactTokens(uint256,address[],address,uint256)",
    "uniswap_v2_swap_exact_tokens_supporting_fee": "swapExactTokensForTokensSupportingFeeOnTransferTokens(uint256,uint256,address[],address,uint256)",
    "uniswap_v2_swap_exact_eth_supporting_fee": "swapExactETHForTokensSupportingFeeOnTransferTokens(uint256,address[],address,uint256)",
    "uniswap_v2_swap_exact_tokens_for_eth_supporting_fee": "swapExactTokensForETHSupportingFeeOnTransferTokens(uint256,uint256,address[],address,uint256)",
    "uniswap_v3_exact_input_single": "exactInputSingle((address,address,uint24,address,uint256,uint256,uint256,uint160))",
    "uniswap_v3_exact_input_single_v2": "exactInputSingle((address,address,uint24,address,uint256,uint256,uint160))",
    "uniswap_v3_exact_input": "exactInput((bytes,address,uint256,uint256,uint256))",
    "uniswap_v3_exact_input_v2": "exactInput((bytes,address,uint256,uint256))",
    "uniswap_v3_exact_output_single": "exactOutputSingle((address,address,uint24,address,uint256,uint256,uint256,uint160))",
    "uniswap_v3_exact_output_single_v2": "exactOutputSingle((address,address,uint24,address,uint256,uint160))",
    "uniswap_v3_exact_output": "exactOutput((bytes,address,uint256,uint256,uint256))",
    "uniswap_v3_exact_output_v2": "exactOutput((bytes,address,uint256,uint256))",
    "uniswap_v3_pool_swap": "swap(address,bool,int256,uint160,bytes)",
    "liquidate": "liquidate(address,address)",
    "liquidate_borrow": "liquidateBorrow(address,uint256,address)",
}


def selector_for(signature: str) -> str:
    """Return the first four bytes of the Keccak-256 signature hash."""
    return "0x" + keccak(text=signature)[:4].hex()


# Edit SIGNATURES to add protocol-specific function signatures.
WATCH = {selector_for(signature): name for name, signature in SIGNATURES.items()}

ERROR_STRING_SELECTOR = selector_for("Error(string)")
PANIC_SELECTOR = selector_for("Panic(uint256)")

PANIC_REASONS = {
    0x00: "generic compiler panic",
    0x01: "assertion failed",
    0x11: "arithmetic overflow or underflow",
    0x12: "division by zero",
    0x21: "invalid enum value",
    0x22: "invalid storage byte array",
    0x31: "pop on empty array",
    0x32: "array index out of bounds",
    0x41: "out of memory",
    0x51: "call to uninitialised function",
}


@dataclass(frozen=True)
class Settings:
    """Tunables for how much the monitor trusts a single fork read."""

    max_lag: int
    depth: int
    poll_seconds: float


def one_line(exc: BaseException) -> str:
    """Collapse a provider error to a single short line suitable for JSONL."""
    return " ".join(str(exc).split())[:180] or type(exc).__name__


def decode_revert_reason(payload: str) -> tuple[str, str] | None:
    """Decode a revert payload into (kind, human-readable reason)."""
    raw = payload[2:] if payload.startswith("0x") else payload
    try:
        data = bytes.fromhex(raw)
    except ValueError:
        return None
    if len(data) < 4:
        return None
    head = "0x" + data[:4].hex()
    body = data[4:]
    if head == ERROR_STRING_SELECTOR and len(body) >= 64:
        try:
            offset = int.from_bytes(body[0:32], "big")
            length = int.from_bytes(body[offset : offset + 32], "big")
            text = body[offset + 32 : offset + 32 + length]
        except (IndexError, ValueError):
            return "revert", "unreadable Error(string)"
        return "revert", text.decode("utf-8", "replace")[:180]
    if head == PANIC_SELECTOR and len(body) >= 32:
        code = int.from_bytes(body[0:32], "big")
        return "panic", PANIC_REASONS.get(code, f"undefined panic 0x{code:02x}")
    return "custom", head


def revert_payload(exc: BaseException) -> str | None:
    """Pull the revert data out of whichever shape the provider client used."""
    candidates: list[Any] = [getattr(exc, "data", None)]
    if exc.args:
        candidates.append(exc.args[0])
    for candidate in candidates:
        if isinstance(candidate, str) and candidate.startswith("0x") and len(candidate) >= 10:
            return candidate
        if isinstance(candidate, dict):
            inner = candidate.get("data")
            if isinstance(inner, str) and inner.startswith("0x") and len(inner) >= 10:
                return inner
    return None


class ForkMonitor:
    """Tracks the fork head and the upstream head so a stale snapshot stays visible."""

    def __init__(self, fork: AsyncWeb3, websocket: AsyncWeb3 | None, interval: float) -> None:
        self._fork = fork
        self._websocket = websocket
        self._interval = interval
        self._lock = asyncio.Lock()
        self.block: int | None = None
        self.head: int | None = None

    @property
    def lag(self) -> int | None:
        if self.block is None or self.head is None:
            return None
        return max(0, self.head - self.block)

    async def refresh(self) -> None:
        async with self._lock:
            try:
                self.block = int(await self._fork.eth.block_number)
            except Exception as exc:
                logging.warning("Fork head unavailable: %s", one_line(exc))
            if self._websocket is not None:
                try:
                    self.head = int(await self._websocket.eth.block_number)
                except Exception as exc:
                    logging.debug("Provider head unavailable: %s", one_line(exc))

    async def current_block(self) -> int | None:
        if self.block is None:
            await self.refresh()
        return self.block

    async def poll(self) -> None:
        previous: int | None = None
        while True:
            await self.refresh()
            if self.block is not None and self.block != previous:
                if previous is None:
                    logging.info(
                        "Fork head block %d; provider head %s; lag %s",
                        self.block,
                        self.head,
                        self.lag,
                    )
                else:
                    logging.info("Fork advanced to block %d; lag %s", self.block, self.lag)
                previous = self.block
            await asyncio.sleep(self._interval)


class ForkCache:
    """Bounded memo for fork reads, keyed by block.

    The block is part of the key rather than a single rolling scope, because one
    observation reads both the fork head and a block below it. Entries for blocks
    far behind the newest block are dropped so the memo cannot grow without bound.
    """

    def __init__(self, capacity: int = 4096, window: int = 64) -> None:
        self._capacity = capacity
        self._window = window
        self._newest: int | None = None
        self._values: dict[tuple[Any, ...], Any] = {}

    def get(self, block: int, key: tuple[Any, ...]) -> Any | None:
        return self._values.get((block, *key))

    def set(self, block: int, key: tuple[Any, ...], value: Any) -> None:
        if self._newest is None or block > self._newest:
            self._newest = block
            cutoff = block - self._window
            for stale in [entry for entry in self._values if entry[0] < cutoff]:
                del self._values[stale]
        composite = (block, *key)
        if composite not in self._values and len(self._values) >= self._capacity:
            self._values.pop(next(iter(self._values)))
        self._values[composite] = value


class PendingTransaction:
    """Compact transaction snapshot retained only while a hit is processed."""

    __slots__ = (
        "tx_hash",
        "sender",
        "to_address",
        "value_wei",
        "selector",
        "gas_limit",
        "tip_wei",
        "timestamp",
        "calldata",
    )

    def __init__(self, tx: Any, tx_hash: str, selector: str) -> None:
        self.tx_hash = tx_hash
        self.sender = tx.get("from")
        self.to_address = tx.get("to")
        self.value_wei = int(tx.get("value", 0))
        self.selector = selector
        self.gas_limit = int(tx.get("gas", 0))
        # Preserve the sender-provided priority fee when the transaction exposes it.
        self.tip_wei = (
            int(tx["maxPriorityFeePerGas"])
            if tx.get("maxPriorityFeePerGas") is not None
            else None
        )
        self.timestamp = datetime.now(timezone.utc).isoformat(timespec="milliseconds")
        self.calldata = str(tx.get("input") or "0x")

    def call_object(self) -> dict[str, Any]:
        """Build the eth_call input from the observed transaction fields."""
        call = {
            "from": self.sender,
            "to": self.to_address,
            "value": self.value_wei,
            "data": self.calldata,
        }
        if self.gas_limit:
            call["gas"] = self.gas_limit
        return call

    def log_record(
        self,
        name: str,
        simulation: dict[str, Any],
        label: str,
        fork: dict[str, Any] | None = None,
    ) -> dict[str, Any]:
        return {
            "name": name,
            "hash": self.tx_hash,
            "from": self.sender,
            "to": self.to_address,
            "value": self.value_wei,
            "selector": self.selector,
            "gas": self.gas_limit,
            "tip": self.tip_wei,
            "ts": self.timestamp,
            "fork": fork or {},
            "sim": simulation,
            "label": label,
        }


def classify(name: str, simulation: dict[str, Any], fork: dict[str, Any]) -> str:
    """Turn a fork read into a label that says how much weight the result carries."""
    if fork.get("code_present") is False:
        return f"no_code:{name}"
    if fork.get("stale"):
        return f"stale_fork:{name}"
    if not simulation.get("ok"):
        return f"sim_failed:{name}:{simulation.get('result_or_error', 'unknown error')}"[:240]
    if fork.get("stable") is False:
        return f"state_dependent:{name}"
    return f"candidate:{name}"


def append_json_line(record: dict[str, Any]) -> None:
    path = Path("logs") / "hits.jsonl"
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as log_file:
        log_file.write(json.dumps(record, separators=(",", ":"), sort_keys=True) + "\n")


async def jsonl_writer(records: asyncio.Queue[Any]) -> None:
    while True:
        record = await records.get()
        try:
            if record is None:
                return
            await asyncio.to_thread(append_json_line, record)
        finally:
            records.task_done()


async def code_present(fork: AsyncWeb3, address: str, block: int, cache: ForkCache) -> bool:
    """Whether the target holds contract code at this block.

    A call to an address without code returns empty data and no error, which
    would otherwise be logged as a successful candidate read.
    """
    key = ("code", address)
    cached = cache.get(block, key)
    if cached is not None:
        return bool(cached)
    try:
        present = len(await fork.eth.get_code(address, block_identifier=block)) > 0
    except Exception as exc:
        # An inconclusive read must not downgrade the observation, so assume code.
        logging.debug("Code lookup failed for %s: %s", address, one_line(exc))
        return True
    cache.set(block, key, present)
    return present


async def call_at_block(
    fork: AsyncWeb3,
    pending: PendingTransaction,
    block: int,
    cache: ForkCache,
) -> dict[str, Any]:
    """Replay the observed calldata against the fork pinned to one block."""
    key = ("call", pending.sender, pending.to_address, pending.value_wei, pending.calldata)
    cached = cache.get(block, key)
    if cached is not None:
        return dict(cached)
    try:
        result = await fork.eth.call(pending.call_object(), block_identifier=block)
        outcome: dict[str, Any] = {"ok": True, "result_or_error": "0x" + bytes(result).hex()}
    except Exception as exc:
        outcome = {"ok": False, "result_or_error": one_line(exc)}
        payload = revert_payload(exc)
        if payload:
            outcome["revert_data"] = payload[:522]
            decoded = decode_revert_reason(payload)
            if decoded:
                outcome["revert_kind"], outcome["revert_reason"] = decoded
    cache.set(block, key, outcome)
    return dict(outcome)


async def simulate(
    fork: AsyncWeb3,
    pending: PendingTransaction,
    monitor: ForkMonitor,
    cache: ForkCache,
    settings: Settings,
) -> tuple[dict[str, Any], dict[str, Any]]:
    """Replay one pending call and record how much the result can be trusted."""
    block = await monitor.current_block()
    fork_info: dict[str, Any] = {
        "block": block,
        "head": monitor.head,
        "lag": monitor.lag,
        "depth": settings.depth,
        "code_present": None,
        "stale": monitor.lag is not None and monitor.lag > settings.max_lag,
        "stable": True,
        "checked": [],
        "reference": None,
    }
    if block is None:
        return {"ok": False, "result_or_error": "fork head unavailable"}, fork_info

    if not await code_present(fork, pending.to_address, block, cache):
        fork_info["code_present"] = False
        return {
            "ok": False,
            "result_or_error": "no contract code at target on the fork",
        }, fork_info
    fork_info["code_present"] = True

    primary = await call_at_block(fork, pending, block, cache)
    fork_info["checked"].append(block)

    if settings.depth > 0 and block - settings.depth >= 0:
        reference_block = block - settings.depth
        reference = await call_at_block(fork, pending, reference_block, cache)
        fork_info["checked"].append(reference_block)
        fork_info["reference"] = {"block": reference_block, "ok": reference["ok"]}
        # A result that flips when only the block changes depends on fork state,
        # so it is reported separately instead of as a plain candidate.
        fork_info["stable"] = reference["ok"] == primary["ok"]

    return primary, fork_info


async def process_hash(
    tx_hash: str,
    websocket: AsyncWeb3,
    fork: AsyncWeb3,
    records: asyncio.Queue[Any],
    monitor: ForkMonitor,
    cache: ForkCache,
    settings: Settings,
) -> None:
    try:
        tx = await websocket.eth.get_transaction(tx_hash)
    except Exception as exc:
        logging.debug("Transaction unavailable %s: %s", tx_hash, one_line(exc))
        return

    if not tx.get("to"):
        return

    calldata = str(tx.get("input") or "0x")
    if len(calldata) < 10:
        return

    selector = "0x" + calldata[2:10].lower()
    name = WATCH.get(selector)
    if name is None:
        return

    pending = PendingTransaction(tx, tx_hash, selector)
    simulation, fork_info = await simulate(fork, pending, monitor, cache, settings)
    label = classify(name, simulation, fork_info)

    record = pending.log_record(name, simulation, label, fork_info)
    logging.info("hit %s", json.dumps(record, separators=(",", ":"), sort_keys=True))
    await records.put(record)


async def transaction_worker(
    hashes: asyncio.Queue[Any],
    websocket: AsyncWeb3,
    fork: AsyncWeb3,
    records: asyncio.Queue[Any],
    monitor: ForkMonitor,
    cache: ForkCache,
    settings: Settings,
) -> None:
    while True:
        tx_hash = await hashes.get()
        try:
            if tx_hash is None:
                return
            await process_hash(str(tx_hash), websocket, fork, records, monitor, cache, settings)
        except Exception:
            logging.exception("Transaction processing failed")
        finally:
            hashes.task_done()


async def stop_worker_pool(
    hashes: asyncio.Queue[Any],
    workers: list[asyncio.Task[Any]],
    records: asyncio.Queue[Any],
    writer: asyncio.Task[Any],
) -> None:
    for _ in workers:
        await hashes.put(None)
    await hashes.join()
    await asyncio.gather(*workers, return_exceptions=True)
    await records.join()
    await records.put(None)
    await writer


def load_settings() -> Settings:
    """Read the fork-trust tunables, falling back to defaults on bad input."""

    def read_int(name: str, default: int, minimum: int = 0) -> int:
        raw = os.getenv(name, "").strip()
        if not raw:
            return default
        try:
            return max(minimum, int(raw))
        except ValueError:
            logging.warning("Ignoring invalid %s=%r; using %d", name, raw, default)
            return default

    def read_float(name: str, default: float, minimum: float = 0.5) -> float:
        raw = os.getenv(name, "").strip()
        if not raw:
            return default
        try:
            return max(minimum, float(raw))
        except ValueError:
            logging.warning("Ignoring invalid %s=%r; using %.1f", name, raw, default)
            return default

    return Settings(
        max_lag=read_int("FORK_MAX_LAG", 3),
        depth=read_int("FORK_CHECK_DEPTH", 1),
        poll_seconds=read_float("FORK_POLL_SECONDS", 3.0),
    )


async def run() -> None:
    load_dotenv()
    ws_rpc = os.getenv("WS_RPC", "").strip()
    anvil_rpc = os.getenv("ANVIL_RPC", "").strip()
    if not ws_rpc or not anvil_rpc:
        raise RuntimeError("WS_RPC and ANVIL_RPC must be set in the environment")

    settings = load_settings()
    websocket = AsyncWeb3.persistent_websocket(WebsocketProviderV2(ws_rpc))
    fork = AsyncWeb3(AsyncHTTPProvider(anvil_rpc))
    await websocket.provider.connect()

    monitor = ForkMonitor(fork, websocket, settings.poll_seconds)
    cache = ForkCache()

    hashes: asyncio.Queue[Any] = asyncio.Queue(maxsize=10_000)
    records: asyncio.Queue[Any] = asyncio.Queue(maxsize=10_000)
    writer = asyncio.create_task(jsonl_writer(records), name="jsonl-writer")
    poller = asyncio.create_task(monitor.poll(), name="fork-monitor")
    workers = [
        asyncio.create_task(
            transaction_worker(hashes, websocket, fork, records, monitor, cache, settings),
            name=f"tx-worker-{index}",
        )
        for index in range(16)
    ]
    subscription_id: Any = None
    try:
        subscription_id = await websocket.eth.subscribe("newPendingTransactions")
        logging.info("Subscribed to pending transaction hashes; watching %d selectors", len(WATCH))
        logging.info(
            "Fork trust: max lag %d blocks, stability depth %d, poll every %.1fs",
            settings.max_lag,
            settings.depth,
            settings.poll_seconds,
        )
        async for message in websocket.ws.process_subscriptions():
            tx_hash = message.get("result")
            if isinstance(tx_hash, str):
                await hashes.put(tx_hash)
    finally:
        if subscription_id is not None:
            try:
                await websocket.eth.unsubscribe(subscription_id)
            except Exception as exc:
                logging.warning("Could not unsubscribe cleanly: %s", one_line(exc))
        poller.cancel()
        with contextlib.suppress(asyncio.CancelledError):
            await poller
        await stop_worker_pool(hashes, workers, records, writer)
        try:
            await fork.provider.disconnect()
        finally:
            await websocket.provider.disconnect()
        logging.info("Disconnected from the WebSocket provider and local fork")


def main() -> None:
    log_records: thread_queue.Queue[logging.LogRecord] = thread_queue.Queue()
    stdout_handler = logging.StreamHandler(sys.stdout)
    stdout_handler.setFormatter(logging.Formatter("%(asctime)s %(levelname)s %(message)s"))
    root_logger = logging.getLogger()
    root_logger.handlers.clear()
    root_logger.setLevel(os.getenv("LOG_LEVEL", "INFO").upper())
    root_logger.addHandler(QueueHandler(log_records))
    listener = QueueListener(log_records, stdout_handler, respect_handler_level=True)
    listener.start()
    try:
        asyncio.run(run())
    except KeyboardInterrupt:
        logging.info("Shutdown complete")
    finally:
        listener.stop()


if __name__ == "__main__":
    main()
