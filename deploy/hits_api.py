#!/usr/bin/env python3
"""Loopback-only, read-only view of the Rust Revm scanner's JSONL observations."""

from __future__ import annotations

import json
import os
import threading
import time
import urllib.request
from datetime import datetime
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse


def _default_log_path() -> Path:
    override = os.getenv("MEMPOLSCAN_LOG_PATH", "").strip() or os.getenv("LOG_PATH", "").strip()
    if override:
        return Path(override)
    prod = Path("/opt/mempoolscanner/logs/hits.jsonl")
    if prod.parent.exists():
        return prod
    return Path("logs/hits.jsonl")


LOG_PATH = _default_log_path()
HOST = "127.0.0.1"
PORT = 8765
MAX_LIMIT = 100

FORK_MAX_LAG_DEFAULT = 3
FORK_POLL_SECONDS_DEFAULT = 3.0
BANNER_WINDOW = 50
STALE_RED_SHARE = 0.2
STALL_MULTIPLIER = 5

QUALITY_KINDS = ("candidate", "state_dependent", "sim_failed", "no_code", "stale_fork", "other")
FRONTRUN_STATUSES = ("extractable", "beneficiary_locked", "sender_protected", "not_applicable")
HIT_FILTERS = ("all", "ok", "failed", "extractable")

DEFAULT_ETH_INR = 260000.0
FX_CACHE_TTL_SECONDS = 180.0
_fx_lock = threading.Lock()
_fx_state: dict[str, object] = {
    "eth_inr": 0.0,
    "source": "reference",
    "updated_at": 0.0,
    "refreshing": False,
}


def default_eth_inr() -> float:
    return _env_float("MEMPOLSCAN_ETH_INR", DEFAULT_ETH_INR, minimum=1.0)


def _format_indian_int(n: int) -> str:
    s = str(abs(n))
    if len(s) <= 3:
        return s
    last3 = s[-3:]
    rest = s[:-3]
    groups: list[str] = []
    while len(rest) > 2:
        groups.append(rest[-2:])
        rest = rest[:-2]
    if rest:
        groups.append(rest)
    groups.reverse()
    return ",".join(groups) + "," + last3


def format_inr_paise(paise: int, *, plus: bool = False) -> str:
    neg = paise < 0
    abs_p = abs(paise)
    rupees = abs_p // 100
    rem = abs_p % 100
    prefix = "-" if neg else ("+" if (plus and abs_p > 0) else "")
    return f"{prefix}₹{_format_indian_int(rupees)}.{rem:02d}"


def wei_to_paise(wei: int, eth_inr: float) -> int:
    if wei == 0 or eth_inr <= 0:
        return 0
    rate_paise = int(round(eth_inr * 100.0))
    sign = -1 if wei < 0 else 1
    return sign * ((abs(wei) * rate_paise) // (10**18))


def _fetch_remote_eth_inr(timeout: float = 1.8) -> tuple[float, str] | None:
    endpoints = (
        (
            "https://api.coinbase.com/v2/exchange-rates?currency=ETH",
            "coinbase",
        ),
        (
            "https://api.coingecko.com/api/v3/simple/price?ids=ethereum&vs_currencies=inr",
            "coingecko",
        ),
    )
    headers = {"User-Agent": "MempoolObservatory/2.0", "Accept": "application/json"}
    for url, label in endpoints:
        try:
            req = urllib.request.Request(url, headers=headers)
            with urllib.request.urlopen(req, timeout=timeout) as resp:  # noqa: S310 - fixed HTTPS FX endpoints
                data = json.loads(resp.read().decode("utf-8"))
            if label == "coinbase":
                raw_rate = data.get("data", {}).get("rates", {}).get("INR")
            else:
                raw_rate = data.get("ethereum", {}).get("inr")
            rate = float(raw_rate)
            if rate > 1000.0:
                return (rate, label)
        except Exception:
            continue
    return None


def _refresh_fx_worker() -> None:
    fetched = _fetch_remote_eth_inr(timeout=2.0)
    with _fx_lock:
        if fetched is not None:
            _fx_state["eth_inr"] = fetched[0]
            _fx_state["source"] = fetched[1]
        elif float(_fx_state.get("eth_inr") or 0.0) <= 0.0:
            _fx_state["eth_inr"] = default_eth_inr()
            _fx_state["source"] = "reference"
        _fx_state["updated_at"] = time.time()
        _fx_state["refreshing"] = False


def get_live_eth_inr() -> tuple[float, str]:
    override = os.getenv("MEMPOLSCAN_ETH_INR", "").strip()
    if override:
        try:
            val = float(override)
            if val > 0:
                return (val, "env")
        except ValueError:
            pass

    now = time.time()
    need_sync = False
    spawn_bg = False
    with _fx_lock:
        cur = float(_fx_state.get("eth_inr") or 0.0)
        updated = float(_fx_state.get("updated_at") or 0.0)
        refreshing = bool(_fx_state.get("refreshing"))
        if cur <= 0.0 and not refreshing:
            _fx_state["refreshing"] = True
            need_sync = True
        elif (now - updated) >= FX_CACHE_TTL_SECONDS and not refreshing:
            _fx_state["refreshing"] = True
            spawn_bg = True

    if need_sync:
        _refresh_fx_worker()
    elif spawn_bg:
        threading.Thread(target=_refresh_fx_worker, daemon=True).start()

    with _fx_lock:
        final_rate = float(_fx_state.get("eth_inr") or 0.0)
        final_src = str(_fx_state.get("source") or "reference")
    if final_rate <= 0.0:
        return (default_eth_inr(), "reference")
    return (final_rate, final_src)


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


def decision_status(record: dict[str, object]) -> str:
    decision = record.get("decision")
    if isinstance(decision, dict):
        status = str(decision.get("status", "")).strip()
        if status == "extractable":
            gross = as_int(decision.get("gross_value_wei")) or 0
            net = as_int(decision.get("net_profit_wei")) or 0
            if gross <= 0 and net <= 0:
                return "not_applicable"
            return "extractable"
        if status in FRONTRUN_STATUSES:
            return status
    return "not_applicable"


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
    if filter_name == "extractable":
        return decision_status(record) == "extractable"
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


def summarise_pipeline(records: list[dict[str, object]]) -> dict[str, object]:
    """Summarise 5-stage Rust Alloy + Revm telemetry, ABI categories, and front-run scoring."""
    fr_counts = {status: 0 for status in FRONTRUN_STATUSES}
    categories = {"claim": 0, "withdraw": 0, "swap": 0, "liquidate": 0}
    would_frontrun = 0
    with_decision = 0
    extractable_gross_wei = 0
    extractable_net_wei = 0
    extractable_token_gains = 0

    ingest_us: list[int] = []
    decode_us: list[int] = []
    sim_us: list[int] = []
    probe_us: list[int] = []
    total_us: list[int] = []

    for record in records:
        decision = record.get("decision")
        if isinstance(decision, dict) and decision:
            with_decision += 1
            st = decision_status(record)
            fr_counts[st] += 1
            if bool(decision.get("would_frontrun")):
                would_frontrun += 1
            if st == "extractable":
                gross = as_int(decision.get("gross_value_wei"))
                if gross is not None and gross > 0:
                    extractable_gross_wei += gross
                net = as_int(decision.get("net_profit_wei"))
                if net is not None and net > 0:
                    extractable_net_wei += net
                tg = as_int(decision.get("probe_token_gains"))
                if tg is not None and tg > 0:
                    extractable_token_gains += tg

        decoded = record.get("decoded")
        if isinstance(decoded, dict):
            cat = str(decoded.get("category", "")).strip()
            if cat in categories:
                categories[cat] += 1

        telemetry = record.get("telemetry")
        if isinstance(telemetry, dict):
            for bucket, key in (
                (ingest_us, "ingest_us"),
                (decode_us, "decode_us"),
                (sim_us, "sim_us"),
                (probe_us, "probe_us"),
                (total_us, "total_us"),
            ):
                val = as_int(telemetry.get(key))
                if val is not None:
                    bucket.append(val)

    return {
        "engine": "rust-alloy-revm",
        "with_decision": with_decision,
        "would_frontrun": would_frontrun,
        "frontrun_counts": fr_counts,
        "extractable_gross_wei": str(extractable_gross_wei),
        "extractable_net_wei": str(extractable_net_wei),
        "extractable_token_gains": extractable_token_gains,
        "categories": categories,
        "latency_us": {
            "median_ingest": median(ingest_us),
            "median_decode": median(decode_us),
            "median_sim": median(sim_us),
            "median_probe": median(probe_us),
            "median_total": median(total_us),
        },
    }


def summarise_extractable(
    records: list[dict[str, object]],
    *,
    meta: dict[str, object] | None = None,
    eth_inr: float | None = None,
    rate_source: str = "reference",
) -> dict[str, object]:
    """Aggregate live open extractable count, total gross/net INR & wei value, and pruned count."""
    resolved_rate = default_eth_inr() if eth_inr is None else eth_inr
    open_count = 0
    profitable_count = 0
    total_gross_wei = 0
    total_net_wei = 0
    recent_tx_value_wei = 0
    active_tx_count = 0
    token_transfers = 0

    for record in records:
        if bool(record.get("closed")):
            continue
        active_tx_count += 1
        tx_val = as_int(record.get("value")) or 0
        decision = record.get("decision")
        dec_gross = as_int(decision.get("gross_value_wei")) if isinstance(decision, dict) else 0
        row_wei = max(tx_val, dec_gross or 0)
        if row_wei > 0:
            recent_tx_value_wei += row_wei

        if decision_status(record) != "extractable":
            continue
        open_count += 1
        if isinstance(decision, dict):
            if bool(decision.get("would_frontrun")):
                profitable_count += 1
            gross = as_int(decision.get("gross_value_wei")) or 0
            net = as_int(decision.get("net_profit_wei")) or 0
            eff_gross = gross if gross > 0 else max(0, net)
            if eff_gross > 0:
                total_gross_wei += eff_gross
            if net > 0:
                total_net_wei += net
            tg = as_int(decision.get("probe_token_gains"))
            if tg is not None and tg > 0:
                token_transfers += tg

    meta_dict = meta if isinstance(meta, dict) else {}
    closed_removed = as_int(meta_dict.get("total_pruned")) or 0
    total_observed = as_int(meta_dict.get("total_observed"))
    if total_observed is None:
        total_observed = len(records)

    gross_paise = wei_to_paise(total_gross_wei, resolved_rate)
    net_paise = wei_to_paise(total_net_wei, resolved_rate)
    recent_paise = wei_to_paise(recent_tx_value_wei, resolved_rate)

    return {
        "currency": "INR",
        "eth_inr_rate": round(resolved_rate, 2),
        "rate_source": rate_source,
        "open_count": open_count,
        "profitable_count": profitable_count,
        "active_tx_count": active_tx_count,
        "total_gross_wei": str(total_gross_wei),
        "total_net_wei": str(total_net_wei),
        "recent_tx_value_wei": str(recent_tx_value_wei),
        "total_gross_paise": str(gross_paise),
        "total_net_paise": str(net_paise),
        "total_gross_inr": format_inr_paise(gross_paise),
        "total_net_inr": format_inr_paise(net_paise, plus=net_paise > 0),
        "recent_tx_value_inr": format_inr_paise(recent_paise),
        "token_transfers": token_transfers,
        "closed_removed": closed_removed,
        "total_observed": total_observed,
    }


def _read_scanner_meta(log_path: Path | None = None) -> dict[str, object]:
    target_log = LOG_PATH if log_path is None else log_path
    meta_path = target_log.parent / "scanner_meta.json"
    if not meta_path.is_file():
        return {}
    try:
        data = json.loads(meta_path.read_text(encoding="utf-8"))
        return data if isinstance(data, dict) else {}
    except (OSError, json.JSONDecodeError):
        return {}


def summarise_quality(
    records: list[dict[str, object]],
    *,
    limit: int = MAX_LIMIT,
    filter_name: str = "all",
    max_lag: int | None = None,
    poll_seconds: float | None = None,
    now: float | None = None,
    banner_window: int = BANNER_WINDOW,
    meta: dict[str, object] | None = None,
    eth_inr: float | None = None,
    rate_source: str = "reference",
) -> dict[str, object]:
    """Summarise fork quality and Revm 5-stage pipeline telemetry."""
    ordered = [
        record
        for record in records
        if isinstance(record, dict) and not bool(record.get("closed"))
    ]
    resolved_filter = filter_name if filter_name in HIT_FILTERS else "all"
    filtered = [record for record in ordered if matches_filter(record, resolved_filter)]
    resolved_max_lag = fork_max_lag() if max_lag is None else max_lag
    resolved_poll = fork_poll_seconds() if poll_seconds is None else poll_seconds
    resolved_rate = default_eth_inr() if eth_inr is None else eth_inr
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
        "fx": {
            "currency": "INR",
            "eth_inr": round(resolved_rate, 2),
            "source": rate_source,
        },
        "banner": summarise_banner(
            ordered,
            max_lag=resolved_max_lag,
            poll_seconds=resolved_poll,
            now=time.time() if now is None else now,
            window=banner_window,
        ),
        "extractable": summarise_extractable(
            ordered,
            meta=meta,
            eth_inr=resolved_rate,
            rate_source=rate_source,
        ),
        "rollup": summarise_rollup(filtered),
        "pipeline": summarise_pipeline(filtered),
    }


def bounded_limit(raw: object, default: int) -> int:
    try:
        requested = int(str(raw))
    except (TypeError, ValueError):
        return default
    return min(MAX_LIMIT, max(1, requested))


class HitsHandler(BaseHTTPRequestHandler):
    server_version = "MempoolReadOnlyAPI/2.0"

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
        eth_inr, rate_source = get_live_eth_inr()
        payload = json.dumps(
            {
                "hits": hits,
                "fx": {
                    "currency": "INR",
                    "eth_inr": round(eth_inr, 2),
                    "source": rate_source,
                },
            },
            separators=(",", ":"),
        ).encode("utf-8")
        self._send_json(payload)

    def _serve_quality(self, query: dict[str, list[str]]) -> None:
        limit = bounded_limit(query.get("limit", [str(MAX_LIMIT)])[0], MAX_LIMIT)
        filter_name = query.get("filter", ["all"])[0]
        if filter_name not in HIT_FILTERS:
            filter_name = "all"
        records = self._read_recent(limit)
        eth_inr, rate_source = get_live_eth_inr()
        summary = summarise_quality(
            records,
            limit=limit,
            filter_name=filter_name,
            meta=_read_scanner_meta(),
            eth_inr=eth_inr,
            rate_source=rate_source,
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
            if not isinstance(record, dict) or bool(record.get("closed")):
                continue
            if decision_status(record) == "beneficiary_locked":
                continue
            tip_raw = record.get("tip")
            entry: dict[str, object] = {
                "name": str(record.get("name", ""))[:96],
                "hash": str(record.get("hash", ""))[:80],
                "from": str(record.get("from", ""))[:80],
                "to": str(record.get("to", ""))[:80],
                "value": str(record.get("value", "0")),
                "selector": str(record.get("selector", ""))[:16],
                "gas": str(record.get("gas", "0")),
                "tip": "" if tip_raw is None else str(tip_raw),
                "ts": str(record.get("ts", ""))[:48],
                "fork": record.get("fork", {}) if isinstance(record.get("fork"), dict) else {},
                "sim": record.get("sim", {}) if isinstance(record.get("sim"), dict) else {},
                "label": str(record.get("label", ""))[:240],
            }
            for extra in ("decoded", "decision", "telemetry"):
                sub = record.get(extra)
                if isinstance(sub, dict) and sub:
                    entry[extra] = sub
            hits.append(entry)
        hits.reverse()
        return hits

    def log_message(self, _format: str, *_args: object) -> None:
        return


if __name__ == "__main__":
    ThreadingHTTPServer((HOST, PORT), HitsHandler).serve_forever()
