#!/usr/bin/env python3
"""Loopback-only, read-only view of the scanner's recent JSONL observations."""

from __future__ import annotations

import json
import os
import time
from datetime import datetime
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

LOG_PATH = Path("/opt/mempoolscanner/logs/hits.jsonl")
HOST = "127.0.0.1"
PORT = 8765
MAX_LIMIT = 100

# Fork-quality reporting. The defaults mirror mempool_scanner.load_settings();
# the service reads the same FORK_* variables from the scanner's .env file.
FORK_MAX_LAG_DEFAULT = 3
FORK_POLL_SECONDS_DEFAULT = 3.0
BANNER_WINDOW = 50
STALE_RED_SHARE = 0.2
STALL_MULTIPLIER = 5

QUALITY_KINDS = ("candidate", "state_dependent", "sim_failed", "no_code", "stale_fork", "other")
HIT_FILTERS = ("all", "ok", "failed")


def _env_int(name: str, default: int, minimum: int = 0) -> int:
    raw = os.getenv(name, "").strip()
    if not raw:
        return default
    try:
        return max(minimum, int(raw))
    except ValueError:
        return default


def _env_float(name: str, default: float, minimum: float = 0.5) -> float:
    raw = os.getenv(name, "").strip()
    if not raw:
        return default
    try:
        return max(minimum, float(raw))
    except ValueError:
        return default


def fork_max_lag() -> int:
    """Threshold above which the scanner labels a hit stale_fork."""
    return _env_int("FORK_MAX_LAG", FORK_MAX_LAG_DEFAULT)


def fork_poll_seconds() -> float:
    """How often the scanner re-reads the fork head."""
    return _env_float("FORK_POLL_SECONDS", FORK_POLL_SECONDS_DEFAULT)


def label_kind(label: object) -> str:
    """Classify a record by the prefix of its label, without inventing new names."""
    head = str(label or "").split(":", 1)[0].strip()
    return head if head in QUALITY_KINDS else "other"


def fork_evidence(record: dict[str, object]) -> dict[str, object] | None:
    """Return the record's fork object, or None when it predates the field."""
    fork = record.get("fork")
    if isinstance(fork, dict) and fork:
        return fork
    return None


def sim_ok(record: dict[str, object]) -> bool:
    sim = record.get("sim")
    return bool(sim.get("ok")) if isinstance(sim, dict) else False


def as_int(value: object) -> int | None:
    """Coerce a JSONL value to int, tolerating strings and nulls."""
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, float):
        return int(value)
    if isinstance(value, str):
        try:
            return int(value.strip())
        except ValueError:
            return None
    return None


def parse_ts(value: object) -> float | None:
    """Parse a record timestamp into epoch seconds."""
    text = str(value or "").strip()
    if not text:
        return None
    try:
        return datetime.fromisoformat(text.replace("Z", "+00:00")).timestamp()
    except ValueError:
        return None


def percent(part: int, whole: int) -> float | None:
    if whole <= 0:
        return None
    return round(100.0 * part / whole, 1)


def median(values: list[int]) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    middle = len(ordered) // 2
    if len(ordered) % 2:
        return float(ordered[middle])
    return round((ordered[middle - 1] + ordered[middle]) / 2, 1)


def matches_filter(record: dict[str, object], filter_name: str) -> bool:
    if filter_name == "ok":
        return sim_ok(record)
    if filter_name == "failed":
        return not sim_ok(record)
    return True


def record_is_healthy(fork: dict[str, object], max_lag: int) -> bool:
    """A record is healthy only when the fork fields positively confirm it."""
    lag = as_int(fork.get("lag"))
    return lag is not None and lag <= max_lag and fork.get("code_present") is True


def summarise_banner(
    recent: list[dict[str, object]],
    *,
    max_lag: int,
    poll_seconds: float,
    now: float,
    window: int = BANNER_WINDOW,
) -> dict[str, object]:
    """One-line fork health for the most recent records."""
    sample = recent[:window]
    newest_ts = sample[0].get("ts") if sample else None
    newest = parse_ts(newest_ts)
    stalled = bool(
        newest is not None and poll_seconds > 0 and now - newest > STALL_MULTIPLIER * poll_seconds
    )
    forks = [fork for fork in (fork_evidence(record) for record in sample) if fork]
    observed = len(forks)
    missing = len(sample) - observed
    kinds = [label_kind(record.get("label")) for record in sample if fork_evidence(record)]
    stale = kinds.count("stale_fork")
    no_code = kinds.count("no_code")
    lags = [lag for lag in (as_int(fork.get("lag")) for fork in forks) if lag is not None]
    newest_lag = next(
        (lag for lag in (as_int(fork.get("lag")) for fork in forks) if lag is not None), None
    )

    if not sample:
        state, text = "unknown", "no observations in the feed yet"
    elif not observed:
        state, text = "unknown", "fork data unavailable for this window"
    elif stalled:
        state = "red"
        text = f"no new observation for {int(now - newest)}s — monitor may be stalled"
    elif stale / len(sample) > STALE_RED_SHARE:
        state = "red"
        text = (
            f"{stale} of {len(sample)} recent observations "
            f"({percent(stale, len(sample))}%) report a stale fork"
        )
    elif stale or no_code or missing or not all(
        record_is_healthy(fork, max_lag) for fork in forks
    ):
        state = "amber"
        reasons = []
        if stale:
            reasons.append(f"{stale} stale_fork")
        if no_code:
            reasons.append(f"{no_code} no_code")
        if missing:
            reasons.append(f"{missing} without fork data")
        if not reasons:
            reasons.append("lag or bytecode evidence incomplete")
        text = ", ".join(reasons) + f" in the last {len(sample)} observations"
    else:
        state = "green"
        text = f"all {len(sample)} recent observations come from a current fork"

    return {
        "state": state,
        "text": text,
        "window": len(sample),
        "stalled": stalled,
        "lag": newest_lag,
        "max_observed_lag": max(lags) if lags else None,
        "max_lag": max_lag,
        "newest_ts": str(newest_ts) if newest_ts else None,
        "counts": {"stale_fork": stale, "no_code": no_code, "missing_fork": missing},
    }


def summarise_rollup(records: list[dict[str, object]]) -> dict[str, object]:
    """Label counts, lag distribution and stability coverage for a set of records."""
    total = len(records)
    observed = [record for record in records if fork_evidence(record)]
    if not observed:
        return {
            "available": False,
            "total": total,
            "with_fork": 0,
            "counts": None,
            "percent": None,
            "lag": {"median": None, "max": None},
            "checked_share": None,
            "note": "fork data unavailable for this range",
        }

    counts = {kind: 0 for kind in QUALITY_KINDS}
    for record in observed:
        counts[label_kind(record.get("label"))] += 1
    base = len(observed)
    forks = [fork for fork in (fork_evidence(record) for record in observed) if fork]
    lags = [lag for lag in (as_int(fork.get("lag")) for fork in forks) if lag is not None]
    checked = sum(
        1 for fork in forks if isinstance(fork.get("checked"), list) and fork.get("checked")
    )
    return {
        "available": True,
        "total": total,
        "with_fork": base,
        "counts": counts,
        "percent": {kind: percent(counts[kind], base) for kind in QUALITY_KINDS},
        "lag": {"median": median(lags), "max": max(lags) if lags else None},
        "checked_share": percent(checked, base),
        "note": None,
    }


def summarise_quality(
    records: list[dict[str, object]],
    *,
    limit: int = MAX_LIMIT,
    filter_name: str = "all",
    max_lag: int | None = None,
    poll_seconds: float | None = None,
    now: float | None = None,
    banner_window: int = BANNER_WINDOW,
) -> dict[str, object]:
    """Summarise fork quality for the banner and the current filter window."""
    ordered = [record for record in records if isinstance(record, dict)]
    resolved_filter = filter_name if filter_name in HIT_FILTERS else "all"
    filtered = [record for record in ordered if matches_filter(record, resolved_filter)]
    resolved_max_lag = fork_max_lag() if max_lag is None else max_lag
    resolved_poll = fork_poll_seconds() if poll_seconds is None else poll_seconds
    return {
        "window": {
            "requested": limit,
            "returned": len(ordered),
            "filtered": len(filtered),
            "filter": resolved_filter,
            "banner_window": banner_window,
            "max_lag": resolved_max_lag,
            "poll_seconds": resolved_poll,
        },
        "banner": summarise_banner(
            ordered,
            max_lag=resolved_max_lag,
            poll_seconds=resolved_poll,
            now=time.time() if now is None else now,
            window=banner_window,
        ),
        "rollup": summarise_rollup(filtered),
    }


def bounded_limit(raw: object, default: int) -> int:
    try:
        requested = int(str(raw))
    except (TypeError, ValueError):
        return default
    return min(MAX_LIMIT, max(1, requested))


class HitsHandler(BaseHTTPRequestHandler):
    server_version = "MempoolReadOnlyAPI/1.1"

    def do_GET(self) -> None:  # noqa: N802 - stdlib handler API
        parsed = urlparse(self.path)
        if parsed.path == "/hits":
            self._serve_hits(parse_qs(parsed.query))
        elif parsed.path == "/quality":
            self._serve_quality(parse_qs(parsed.query))
        else:
            self.send_error(404)

    def _serve_hits(self, query: dict[str, list[str]]) -> None:
        limit = bounded_limit(query.get("limit", ["50"])[0], 50)
        hits = self._read_recent(limit)
        payload = json.dumps({"hits": hits}, separators=(",", ":")).encode("utf-8")
        self._send_json(payload)

    def _serve_quality(self, query: dict[str, list[str]]) -> None:
        limit = bounded_limit(query.get("limit", [str(MAX_LIMIT)])[0], MAX_LIMIT)
        filter_name = query.get("filter", ["all"])[0]
        if filter_name not in HIT_FILTERS:
            filter_name = "all"
        records = self._read_recent(limit)
        summary = summarise_quality(
            records,
            limit=limit,
            filter_name=filter_name,
        )
        payload = json.dumps(summary, separators=(",", ":")).encode("utf-8")
        self._send_json(payload)

    def _send_json(self, payload: bytes) -> None:
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
                    "fork": record.get("fork", {}) if isinstance(record.get("fork"), dict) else {},
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
