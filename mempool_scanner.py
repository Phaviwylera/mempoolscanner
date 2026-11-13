"""Read-only pending-transaction observer with local-fork call simulation."""

from __future__ import annotations

import asyncio
import json
import logging
import os
import queue as thread_queue
import sys
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
    "uniswap_v3_exact_output_single_v2": "exactOutputSingle((address,address,uint24,address,uint256,uint256,uint160))",
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

    def log_record(self, name: str, simulation: dict[str, Any], label: str) -> dict[str, Any]:
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
            "sim": simulation,
            "label": label,
        }


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


async def process_hash(
    tx_hash: str,
    websocket: AsyncWeb3,
    fork: AsyncWeb3,
    records: asyncio.Queue[Any],
) -> None:
    try:
        tx = await websocket.eth.get_transaction(tx_hash)
    except Exception as exc:
        logging.debug("Transaction unavailable %s: %s", tx_hash, str(exc).splitlines()[0][:160])
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
    try:
        result = await fork.eth.call(pending.call_object(), block_identifier="pending")
        simulation = {"ok": True, "result_or_error": "0x" + bytes(result).hex()}
        label = f"candidate:{name}"
    except Exception as exc:
        short_error = " ".join(str(exc).split())[:180] or type(exc).__name__
        simulation = {"ok": False, "result_or_error": short_error}
        label = f"sim_failed:{name}:{short_error}"

    record = pending.log_record(name, simulation, label)
    logging.info("hit %s", json.dumps(record, separators=(",", ":"), sort_keys=True))
    await records.put(record)


async def transaction_worker(
    hashes: asyncio.Queue[Any],
    websocket: AsyncWeb3,
    fork: AsyncWeb3,
    records: asyncio.Queue[Any],
) -> None:
    while True:
        tx_hash = await hashes.get()
        try:
            if tx_hash is None:
                return
            await process_hash(str(tx_hash), websocket, fork, records)
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


async def run() -> None:
    load_dotenv()
    ws_rpc = os.getenv("WS_RPC", "").strip()
    anvil_rpc = os.getenv("ANVIL_RPC", "").strip()
    if not ws_rpc or not anvil_rpc:
        raise RuntimeError("WS_RPC and ANVIL_RPC must be set in the environment")

    websocket = AsyncWeb3.persistent_websocket(WebsocketProviderV2(ws_rpc))
    fork = AsyncWeb3(AsyncHTTPProvider(anvil_rpc))
    await websocket.provider.connect()

    hashes: asyncio.Queue[Any] = asyncio.Queue(maxsize=10_000)
    records: asyncio.Queue[Any] = asyncio.Queue(maxsize=10_000)
    writer = asyncio.create_task(jsonl_writer(records), name="jsonl-writer")
    workers = [
        asyncio.create_task(transaction_worker(hashes, websocket, fork, records), name=f"tx-worker-{index}")
        for index in range(16)
    ]
    subscription_id: Any = None
    try:
        subscription_id = await websocket.eth.subscribe("newPendingTransactions")
        logging.info("Subscribed to pending transaction hashes; watching %d selectors", len(WATCH))
        async for message in websocket.ws.process_subscriptions():
            tx_hash = message.get("result")
            if isinstance(tx_hash, str):
                await hashes.put(tx_hash)
    finally:
        if subscription_id is not None:
            try:
                await websocket.eth.unsubscribe(subscription_id)
            except Exception as exc:
                logging.warning("Could not unsubscribe cleanly: %s", str(exc).splitlines()[0][:160])
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

