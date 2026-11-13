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


AAVE_V3_POOL_HEX = "0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2"
AAVE_V3_ORACLE_HEX = "0x54586bE62E3c3580375aE3723C145253060Ca0C2"
LIQ_FILTERS = ("all", "liquidatable", "at_risk")
DEFAULT_USD_INR = 86.0


def _liquidation_snapshot_path(log_path: Path | None = None) -> Path:
    override = os.getenv("MEMPOLSCAN_LIQ_PATH", "").strip()
    if override:
        return Path(override)
    base = log_path if log_path is not None else LOG_PATH
    return base.parent / "liquidations.json"


def _read_liquidation_snapshot(log_path: Path | None = None) -> dict[str, object] | None:
    path = _liquidation_snapshot_path(log_path)
    if not path.is_file():
        return None
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
        return data if isinstance(data, dict) else None
    except (OSError, json.JSONDecodeError, TypeError):
        return None


def _resolve_http_rpc() -> str:
    for key in ("HTTP_RPC", "WS_RPC"):
        raw = os.getenv(key, "").strip().strip('"').strip("'")
        if not raw:
            continue
        if raw.startswith("https://") or raw.startswith("http://"):
            return raw
        if raw.startswith("wss://"):
            return "https://" + raw[len("wss://") :]
        if raw.startswith("ws://"):
            return "http://" + raw[len("ws://") :]
    return "https://ethereum-rpc.publicnode.com"


def usd_to_paise(usd_val: float, usd_inr: float) -> int:
    if not usd_val or usd_inr <= 0:
        return 0
    return int(round(usd_val * usd_inr * 100.0))


def format_usd_as_inr(usd_val: float, usd_inr: float, *, plus: bool = False) -> str:
    return format_inr_paise(usd_to_paise(usd_val, usd_inr), plus=plus)


def resolve_usd_inr(eth_inr: float, eth_price_usd: float) -> float:
    if eth_inr > 0 and eth_price_usd > 100.0:
        derived = eth_inr / eth_price_usd
        if 60.0 <= derived <= 130.0:
            return derived
    return DEFAULT_USD_INR


def compute_flash_plan_dict(
    collateral_usd: float,
    debt_usd: float,
    health_factor: float,
    eth_price_usd: float,
    base_fee_gwei: float = 1.0,
    close_factor: float = 0.50,
) -> tuple[float, float, dict[str, object]]:
    safe_hf = health_factor if (health_factor > 0) else 99.0
    drop_pct = 0.0 if safe_hf <= 1.0 else max(0.0, min(99.99, (1.0 - (1.0 / safe_hf)) * 100.0))
    eth_liq_usd = (eth_price_usd / safe_hf) if (eth_price_usd > 0 and 0 < safe_hf < 50.0) else 0.0

    cf = max(0.05, min(1.0, close_factor))
    bonus_rate = 0.05
    flash_fee_rate = 0.0005
    dex_fee_rate = 0.001
    est_gas_eth = (380_000 * max(0.1, base_fee_gwei)) / 1e9
    est_gas_usd = est_gas_eth * max(1.0, eth_price_usd)

    def _eval(col_val: float) -> tuple[float, float, float, float, float, float]:
        if debt_usd <= 0.0 or col_val <= 0.0:
            return (0.0, 0.0, 0.0, 0.0, 0.0, 0.0)
        debt_to_cover = max(0.0, min(debt_usd * cf, col_val / (1.0 + bonus_rate)))
        seized = min(col_val, debt_to_cover * (1.0 + bonus_rate))
        gross = max(0.0, seized - debt_to_cover)
        f_fee = debt_to_cover * flash_fee_rate
        d_fee = seized * dex_fee_rate
        net = gross - f_fee - d_fee - est_gas_usd
        return (debt_to_cover, seized, gross, f_fee, d_fee, net)

    if safe_hf < 1.0:
        dtc, seized, gross, f_fee, d_fee, raw_net = _eval(collateral_usd)
    else:
        dtc, seized, gross, f_fee, d_fee, raw_net = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0)

    trig_col = collateral_usd * (0.99 / safe_hf) if (1.0 <= safe_hf < 50.0) else collateral_usd
    _, _, _, _, _, trig_net = _eval(trig_col)
    net_usd = raw_net if safe_hf < 1.0 else 0.0

    return (
        round(drop_pct, 2),
        round(eth_liq_usd, 2),
        {
            "close_factor_pct": round(cf * 100.0, 2),
            "liquidation_bonus_pct": 5.0,
            "flash_fee_pct": 0.05,
            "debt_to_cover_usd": round(dtc, 2),
            "seized_collateral_usd": round(seized, 2),
            "gross_bounty_usd": round(gross, 2),
            "flash_loan_fee_usd": round(f_fee, 2),
            "dex_swap_fee_usd": round(d_fee, 2),
            "est_gas_usd": round(est_gas_usd, 2),
            "net_profit_usd": round(net_usd, 2),
            "trigger_net_profit_usd": round(trig_net, 2),
            "profitable_now": bool(safe_hf < 1.0 and net_usd > 0.0),
        },
    )


def _enrich_position_inr(pos: dict[str, object], usd_inr: float) -> dict[str, object]:
    out = dict(pos)
    col_usd = float(out.get("collateral_usd") or 0.0)
    debt_usd = float(out.get("debt_usd") or 0.0)
    avail_usd = float(out.get("available_borrows_usd") or 0.0)
    eth_liq_usd = float(out.get("eth_liq_price_usd") or 0.0)

    out["collateral_inr"] = format_usd_as_inr(col_usd, usd_inr)
    out["debt_inr"] = format_usd_as_inr(debt_usd, usd_inr)
    out["available_borrows_inr"] = format_usd_as_inr(avail_usd, usd_inr)
    out["eth_liq_price_inr"] = format_usd_as_inr(eth_liq_usd, usd_inr) if eth_liq_usd > 0 else "—"

    raw_plan = out.get("plan")
    plan = dict(raw_plan) if isinstance(raw_plan, dict) else {}
    plan["debt_to_cover_inr"] = format_usd_as_inr(float(plan.get("debt_to_cover_usd") or 0.0), usd_inr)
    plan["seized_collateral_inr"] = format_usd_as_inr(
        float(plan.get("seized_collateral_usd") or 0.0), usd_inr
    )
    plan["gross_bounty_inr"] = format_usd_as_inr(
        float(plan.get("gross_bounty_usd") or 0.0), usd_inr, plus=True
    )
    plan["flash_loan_fee_inr"] = format_usd_as_inr(
        float(plan.get("flash_loan_fee_usd") or 0.0), usd_inr
    )
    plan["dex_swap_fee_inr"] = format_usd_as_inr(float(plan.get("dex_swap_fee_usd") or 0.0), usd_inr)
    plan["est_gas_inr"] = format_usd_as_inr(float(plan.get("est_gas_usd") or 0.0), usd_inr)
    plan["net_profit_inr"] = format_usd_as_inr(
        float(plan.get("net_profit_usd") or 0.0), usd_inr, plus=True
    )
    plan["trigger_net_profit_inr"] = format_usd_as_inr(
        float(plan.get("trigger_net_profit_usd") or 0.0), usd_inr, plus=True
    )
    out["plan"] = plan
    return out


def inspect_aave_v3_wallet(
    address_raw: str,
    *,
    eth_price_usd: float = 2650.0,
    base_fee_gwei: float = 1.0,
    usd_inr: float = DEFAULT_USD_INR,
    timeout: float = 3.5,
) -> dict[str, object]:
    cleaned = address_raw.strip()
    if (
        len(cleaned) != 42
        or not cleaned.startswith(("0x", "0X"))
        or any(ch not in "0123456789abcdefABCDEF" for ch in cleaned[2:])
    ):
        return {"ok": False, "error": "Enter a valid 42-character 0x Ethereum address."}

    addr_lc = "0x" + cleaned[2:].lower()
    call_data = "0xbf92857c" + ("0" * 24) + cleaned[2:].lower()
    rpc_url = _resolve_http_rpc()
    req_body = json.dumps(
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_call",
            "params": [{"to": AAVE_V3_POOL_HEX, "data": call_data}, "latest"],
        }
    ).encode("utf-8")
    try:
        req = urllib.request.Request(
            rpc_url,
            data=req_body,
            headers={
                "Content-Type": "application/json",
                "Accept": "application/json",
                "User-Agent": "MempoolObservatory/2.0",
            },
            method="POST",
        )
        with urllib.request.urlopen(req, timeout=timeout) as resp:  # noqa: S310 - configured Ethereum JSON-RPC
            rpc_json = json.loads(resp.read().decode("utf-8"))
        hex_res = str(rpc_json.get("result") or "")
        if not hex_res.startswith("0x") or len(hex_res) < 2 + 384:
            return {"ok": False, "user": addr_lc, "error": "Aave V3 Pool returned empty account data."}
        raw = bytes.fromhex(hex_res[2 : 2 + 384])
        col_base = int.from_bytes(raw[0:32], "big")
        debt_base = int.from_bytes(raw[32:64], "big")
        avail_base = int.from_bytes(raw[64:96], "big")
        liq_bps = int.from_bytes(raw[96:128], "big")
        ltv_bps = int.from_bytes(raw[128:160], "big")
        hf_wad = int.from_bytes(raw[160:192], "big")

        col_usd = round(col_base / 1e8, 2)
        debt_usd = round(debt_base / 1e8, 2)
        avail_usd = round(avail_base / 1e8, 2)
        hf = 99.99 if (debt_base == 0 or hf_wad >= 100 * (10**18)) else round(hf_wad / 1e18, 4)
        status = (
            "no_debt"
            if debt_base == 0
            else ("liquidatable" if hf < 1.0 else ("critical" if hf < 1.05 else ("at_risk" if hf < 1.20 else "healthy")))
        )
        drop_pct, eth_liq_usd, plan = compute_flash_plan_dict(
            col_usd, debt_usd, hf, eth_price_usd, base_fee_gwei
        )
        pos = _enrich_position_inr(
            {
                "ok": True,
                "has_debt": debt_base > 0,
                "user": addr_lc,
                "collateral_usd": col_usd,
                "debt_usd": debt_usd,
                "available_borrows_usd": avail_usd,
                "liq_threshold_bps": liq_bps,
                "liq_threshold_pct": round(liq_bps / 100.0, 2),
                "ltv_bps": ltv_bps,
                "ltv_pct": round(ltv_bps / 100.0, 2),
                "health_factor": hf,
                "status": status,
                "price_drop_pct_to_liq": drop_pct,
                "eth_liq_price_usd": eth_liq_usd,
                "last_reserve_symbol": "LIVE RPC",
                "revm_verified": False,
                "plan": plan,
            },
            usd_inr,
        )
        return pos
    except Exception as exc:
        return {"ok": False, "user": addr_lc, "error": f"RPC query failed: {type(exc).__name__}"}


def summarise_liquidations(
    snapshot: dict[str, object] | None,
    *,
    limit: int = 60,
    filter_name: str = "all",
    eth_inr: float = DEFAULT_ETH_INR,
    rate_source: str = "reference",
    inspected: dict[str, object] | None = None,
) -> dict[str, object]:
    snap = snapshot if isinstance(snapshot, dict) else {}
    resolved_eth_inr = eth_inr if eth_inr > 0 else default_eth_inr()
    raw_eth_usd = float(snap.get("eth_price_usd") or 0.0)
    eth_price_usd = raw_eth_usd if raw_eth_usd > 100.0 else round(resolved_eth_inr / DEFAULT_USD_INR, 2)
    usd_inr = resolve_usd_inr(resolved_eth_inr, eth_price_usd)

    raw_positions = snap.get("positions")
    pos_list = [p for p in raw_positions if isinstance(p, dict)] if isinstance(raw_positions, list) else []

    enriched_all = [_enrich_position_inr(p, usd_inr) for p in pos_list]
    enriched_all.sort(key=lambda p: (float(p.get("health_factor") or 99.99), -float(p.get("debt_usd") or 0.0)))

    liquidatable_count = 0
    critical_count = 0
    at_risk_count = 0
    lowest_hf: float | None = None
    liq_bounty_usd = 0.0
    liq_net_usd = 0.0
    at_risk_debt_usd = 0.0
    total_debt_usd = 0.0
    total_col_usd = 0.0

    for p in enriched_all:
        hf = float(p.get("health_factor") or 99.99)
        d_usd = float(p.get("debt_usd") or 0.0)
        c_usd = float(p.get("collateral_usd") or 0.0)
        total_debt_usd += d_usd
        total_col_usd += c_usd
        if lowest_hf is None or hf < lowest_hf:
            lowest_hf = hf
        plan = p.get("plan") if isinstance(p.get("plan"), dict) else {}
        if hf < 1.0:
            liquidatable_count += 1
            at_risk_count += 1
            at_risk_debt_usd += d_usd
            liq_bounty_usd += float(plan.get("gross_bounty_usd") or 0.0)
            liq_net_usd += max(0.0, float(plan.get("net_profit_usd") or 0.0))
        elif hf < 1.05:
            critical_count += 1
            at_risk_count += 1
            at_risk_debt_usd += d_usd
        elif hf < 1.20:
            at_risk_count += 1
            at_risk_debt_usd += d_usd

    chosen_filter = filter_name if filter_name in LIQ_FILTERS else "all"
    if chosen_filter == "liquidatable":
        filtered = [p for p in enriched_all if float(p.get("health_factor") or 99.99) < 1.0]
    elif chosen_filter == "at_risk":
        filtered = [p for p in enriched_all if float(p.get("health_factor") or 99.99) < 1.20]
    else:
        filtered = enriched_all

    raw_events = snap.get("recent_liquidations")
    events_in = [e for e in raw_events if isinstance(e, dict)] if isinstance(raw_events, list) else []
    enriched_events: list[dict[str, object]] = []
    for ev in events_in[:40]:
        ev_copy = dict(ev)
        d_est = float(ev_copy.get("debt_to_cover_usd_est") or 0.0)
        ev_copy["debt_to_cover_inr"] = format_usd_as_inr(d_est, usd_inr) if d_est > 0 else "—"
        enriched_events.append(ev_copy)

    return {
        "protocol": str(snap.get("protocol") or "Aave V3 · Ethereum Mainnet"),
        "pool_address": str(snap.get("pool_address") or AAVE_V3_POOL_HEX),
        "oracle_address": str(snap.get("oracle_address") or AAVE_V3_ORACLE_HEX),
        "status": str(snap.get("status") or ("empty" if not snapshot else "live")),
        "head_block": int(snap.get("head_block") or 0),
        "scanned_from_block": int(snap.get("scanned_from_block") or 0),
        "scanned_blocks_total": int(snap.get("scanned_blocks_total") or 0),
        "discovered_borrowers_total": int(snap.get("discovered_borrowers_total") or len(enriched_all)),
        "base_fee_gwei": float(snap.get("base_fee_gwei") or 1.0),
        "flashloan_premium_bps": int(snap.get("flashloan_premium_bps") or 5),
        "default_liq_bonus_bps": int(snap.get("default_liq_bonus_bps") or 500),
        "updated_ts": str(snap.get("updated_ts") or ""),
        "filter": chosen_filter,
        "fx": {
            "currency": "INR",
            "eth_inr": round(resolved_eth_inr, 2),
            "eth_usd": round(eth_price_usd, 2),
            "usd_inr": round(usd_inr, 4),
            "eth_inr_formatted": format_inr_paise(int(round(resolved_eth_inr * 100.0))),
            "source": rate_source,
        },
        "summary": {
            "tracked_count": len(enriched_all),
            "shown_count": min(limit, len(filtered)),
            "liquidatable_count": liquidatable_count,
            "critical_count": critical_count,
            "at_risk_count": at_risk_count,
            "lowest_hf": round(lowest_hf, 4) if lowest_hf is not None else None,
            "total_liquidatable_bounty_usd": round(liq_bounty_usd, 2),
            "total_liquidatable_bounty_inr": format_usd_as_inr(liq_bounty_usd, usd_inr),
            "total_liquidatable_net_usd": round(liq_net_usd, 2),
            "total_liquidatable_net_inr": format_usd_as_inr(liq_net_usd, usd_inr, plus=(liq_net_usd > 0)),
            "at_risk_debt_usd": round(at_risk_debt_usd, 2),
            "at_risk_debt_inr": format_usd_as_inr(at_risk_debt_usd, usd_inr),
            "total_tracked_debt_usd": round(total_debt_usd, 2),
            "total_tracked_debt_inr": format_usd_as_inr(total_debt_usd, usd_inr),
            "total_tracked_collateral_usd": round(total_col_usd, 2),
            "total_tracked_collateral_inr": format_usd_as_inr(total_col_usd, usd_inr),
        },
        "positions": filtered[:limit],
        "recent_liquidations": enriched_events,
        "inspected": inspected,
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
        elif parsed.path == "/liquidations":
            self._serve_liquidations(parse_qs(parsed.query))
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

    def _serve_liquidations(self, query: dict[str, list[str]]) -> None:
        limit = bounded_limit(query.get("limit", ["60"])[0], 60)
        filter_name = query.get("filter", ["all"])[0]
        if filter_name not in LIQ_FILTERS:
            filter_name = "all"
        inspect_addr = query.get("inspect", [""])[0].strip()

        snapshot = _read_liquidation_snapshot()
        eth_inr, rate_source = get_live_eth_inr()
        raw_eth_usd = float((snapshot or {}).get("eth_price_usd") or 0.0)
        eth_usd = raw_eth_usd if raw_eth_usd > 100.0 else round(eth_inr / DEFAULT_USD_INR, 2)
        base_fee_gwei = float((snapshot or {}).get("base_fee_gwei") or 1.0)
        usd_inr = resolve_usd_inr(eth_inr, eth_usd)

        inspected = None
        if inspect_addr:
            inspected = inspect_aave_v3_wallet(
                inspect_addr,
                eth_price_usd=eth_usd,
                base_fee_gwei=base_fee_gwei,
                usd_inr=usd_inr,
            )

        res = summarise_liquidations(
            snapshot,
            limit=limit,
            filter_name=filter_name,
            eth_inr=eth_inr,
            rate_source=rate_source,
            inspected=inspected,
        )
        payload = json.dumps(res, separators=(",", ":")).encode("utf-8")
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
