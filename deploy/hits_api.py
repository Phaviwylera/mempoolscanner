#!/usr/bin/env python3
"""Loopback-only, read-only view of the scanner's recent JSONL observations."""

from __future__ import annotations

import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

LOG_PATH = Path("/opt/mempoolscanner/logs/hits.jsonl")
HOST = "127.0.0.1"
PORT = 8765
MAX_LIMIT = 100


class HitsHandler(BaseHTTPRequestHandler):
    server_version = "MempoolReadOnlyAPI/1.0"

    def do_GET(self) -> None:  # noqa: N802 - stdlib handler API
        parsed = urlparse(self.path)
        if parsed.path != "/hits":
            self.send_error(404)
            return

        try:
            requested = int(parse_qs(parsed.query).get("limit", ["50"])[0])
        except (TypeError, ValueError):
            requested = 50
        limit = min(MAX_LIMIT, max(1, requested))
        hits = self._read_recent(limit)
        payload = json.dumps({"hits": hits}, separators=(",", ":")).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.end_headers()
        self.wfile.write(payload)

    @staticmethod
    def _read_recent(limit: int) -> list[dict[str, object]]:
        if not LOG_PATH.is_file():
            return []
        try:
            with LOG_PATH.open("rb") as source:
                position = source.seek(0, 2)
                chunks: list[bytes] = []
                newline_count = 0
                while position > 0 and newline_count <= limit:
                    size = min(8192, position)
                    position -= size
                    source.seek(position)
                    chunk = source.read(size)
                    chunks.insert(0, chunk)
                    newline_count += chunk.count(b"\n")
                lines = b"".join(chunks).splitlines()[-limit:]
        except OSError:
            return []

        hits: list[dict[str, object]] = []
        for raw_line in lines:
            try:
                record = json.loads(raw_line.decode("utf-8"))
            except (json.JSONDecodeError, TypeError):
                continue
            if not isinstance(record, dict):
                continue
            # Whitelist only the fields used by the read-only dashboard.
            hits.append(
                {
                    "name": str(record.get("name", ""))[:96],
                    "hash": str(record.get("hash", ""))[:80],
                    "from": str(record.get("from", ""))[:80],
                    "to": str(record.get("to", ""))[:80],
                    "value": str(record.get("value", "0")),
                    "selector": str(record.get("selector", ""))[:16],
                    "gas": str(record.get("gas", "0")),
                    "tip": str(record.get("tip", "")),
                    "ts": str(record.get("ts", ""))[:48],
                    "sim": record.get("sim", {}) if isinstance(record.get("sim"), dict) else {},
                    "label": str(record.get("label", ""))[:240],
                }
            )
        hits.reverse()
        return hits

    def log_message(self, _format: str, *_args: object) -> None:
        return


if __name__ == "__main__":
    ThreadingHTTPServer((HOST, PORT), HitsHandler).serve_forever()

