"""Unit tests for the fork-quality summaries served by deploy/hits_api.py."""

from __future__ import annotations

import json
import sys
import unittest
from datetime import datetime, timezone
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "deploy"))

import hits_api  # noqa: E402

FIXTURES = Path(__file__).resolve().parent / "fixtures"
BASE = datetime(2026, 11, 13, 12, 0, 0, tzinfo=timezone.utc)
NOW = BASE.timestamp() + 5
MAX_LAG = 3
POLL_SECONDS = 3.0


def load(name: str) -> list[dict[str, object]]:
    text = (FIXTURES / name).read_text(encoding="utf-8")
    return [json.loads(line) for line in text.splitlines() if line.strip()]


def summary(name: str, **kwargs: object) -> dict[str, object]:
    kwargs.setdefault("filter_name", "all")
    kwargs.setdefault("max_lag", MAX_LAG)
    kwargs.setdefault("poll_seconds", POLL_SECONDS)
    kwargs.setdefault("now", NOW)
    return hits_api.summarise_quality(load(name), **kwargs)


class LabelKindTests(unittest.TestCase):
    def test_known_prefixes(self) -> None:
        for kind in ("candidate", "state_dependent", "sim_failed", "no_code", "stale_fork"):
            self.assertEqual(hits_api.label_kind(f"{kind}:claim"), kind)

    def test_reason_stays_out_of_kind(self) -> None:
        self.assertEqual(hits_api.label_kind("sim_failed:claim:already claimed"), "sim_failed")

    def test_unknown_and_missing(self) -> None:
        self.assertEqual(hits_api.label_kind(None), "other")
        self.assertEqual(hits_api.label_kind(""), "other")
        self.assertEqual(hits_api.label_kind("mystery:thing"), "other")


class BannerTests(unittest.TestCase):
    def test_old_records_without_fork_are_unknown(self) -> None:
        banner = summary("old_records.jsonl")["banner"]
        self.assertEqual(banner["state"], "unknown")
        self.assertEqual(banner["text"], "fork data unavailable for this window")
        self.assertEqual(banner["counts"]["missing_fork"], 3)
        self.assertIsNone(banner["lag"])

    def test_all_green_window_is_green(self) -> None:
        banner = summary("all_green.jsonl")["banner"]
        self.assertEqual(banner["state"], "green")
        self.assertEqual(banner["window"], 5)
        self.assertEqual(banner["lag"], 0)
        self.assertEqual(banner["max_observed_lag"], 0)
        self.assertFalse(banner["stalled"])
        self.assertIn("current fork", banner["text"])

    def test_mixed_window_is_amber(self) -> None:
        banner = summary("mixed.jsonl")["banner"]
        self.assertEqual(banner["state"], "amber")
        self.assertEqual(banner["counts"]["stale_fork"], 1)
        self.assertEqual(banner["counts"]["no_code"], 1)
        self.assertIn("1 stale_fork", banner["text"])
        self.assertIn("1 no_code", banner["text"])

    def test_over_twenty_percent_stale_is_red(self) -> None:
        banner = summary("all_stale.jsonl")["banner"]
        self.assertEqual(banner["state"], "red")
        self.assertIn("100.0%", banner["text"])

    def test_empty_window_is_unknown(self) -> None:
        banner = summary("empty.jsonl")["banner"]
        self.assertEqual(banner["state"], "unknown")
        self.assertEqual(banner["text"], "no observations in the feed yet")
        self.assertEqual(banner["window"], 0)

    def test_missing_head_is_not_green(self) -> None:
        banner = summary("missing_head.jsonl")["banner"]
        self.assertEqual(banner["state"], "amber")
        self.assertIsNone(banner["lag"])
        self.assertIsNone(banner["max_observed_lag"])

    def test_stalled_monitor_is_red(self) -> None:
        banner = summary("stalled.jsonl")["banner"]
        self.assertEqual(banner["state"], "red")
        self.assertTrue(banner["stalled"])
        self.assertIn("stalled", banner["text"])

    def test_banner_window_is_bounded(self) -> None:
        records = load("all_green.jsonl") * 14
        banner = hits_api.summarise_banner(
            records, max_lag=MAX_LAG, poll_seconds=POLL_SECONDS, now=NOW, window=50
        )
        self.assertEqual(banner["window"], 50)


class RollupTests(unittest.TestCase):
    def test_unavailable_when_no_record_has_fork(self) -> None:
        rollup = summary("old_records.jsonl")["rollup"]
        self.assertFalse(rollup["available"])
        self.assertEqual(rollup["total"], 3)
        self.assertEqual(rollup["with_fork"], 0)
        self.assertIsNone(rollup["counts"])
        self.assertIsNone(rollup["percent"])
        self.assertEqual(rollup["note"], "fork data unavailable for this range")

    def test_empty_window_offers_no_counts(self) -> None:
        rollup = summary("empty.jsonl")["rollup"]
        self.assertFalse(rollup["available"])
        self.assertEqual(rollup["total"], 0)

    def test_all_green_counts_and_lag(self) -> None:
        rollup = summary("all_green.jsonl")["rollup"]
        self.assertTrue(rollup["available"])
        self.assertEqual(rollup["counts"]["candidate"], 5)
        self.assertEqual(rollup["percent"]["candidate"], 100.0)
        self.assertEqual(rollup["lag"], {"median": 0.0, "max": 0})
        self.assertEqual(rollup["checked_share"], 100.0)

    def test_mixed_counts_percentages_and_lag(self) -> None:
        rollup = summary("mixed.jsonl")["rollup"]
        self.assertEqual(
            rollup["counts"],
            {
                "candidate": 3,
                "state_dependent": 1,
                "sim_failed": 1,
                "no_code": 1,
                "stale_fork": 1,
                "other": 0,
            },
        )
        self.assertEqual(rollup["percent"]["candidate"], 42.9)
        self.assertEqual(rollup["percent"]["stale_fork"], 14.3)
        self.assertEqual(rollup["lag"], {"median": 1.0, "max": 9})
        self.assertEqual(rollup["with_fork"], 7)
        self.assertEqual(rollup["checked_share"], 100.0)

    def test_filter_narrows_the_rollup(self) -> None:
        rollup = summary("mixed.jsonl", filter_name="failed")["rollup"]
        self.assertEqual(rollup["total"], 2)
        self.assertEqual(rollup["with_fork"], 2)
        self.assertEqual(rollup["counts"]["no_code"], 1)
        self.assertEqual(rollup["counts"]["sim_failed"], 1)
        self.assertEqual(rollup["counts"]["candidate"], 0)

    def test_unknown_filter_falls_back_to_all(self) -> None:
        result = summary("mixed.jsonl", filter_name="bogus")
        self.assertEqual(result["window"]["filter"], "all")
        self.assertEqual(result["rollup"]["total"], 7)

    def test_all_stale_rollup(self) -> None:
        rollup = summary("all_stale.jsonl")["rollup"]
        self.assertEqual(rollup["counts"]["stale_fork"], 5)
        self.assertEqual(rollup["percent"]["stale_fork"], 100.0)
        self.assertEqual(rollup["lag"], {"median": 8.0, "max": 8})

    def test_missing_lag_degrades_to_null(self) -> None:
        rollup = summary("missing_head.jsonl")["rollup"]
        self.assertTrue(rollup["available"])
        self.assertEqual(rollup["lag"], {"median": None, "max": None})
        self.assertEqual(rollup["counts"]["candidate"], 4)


class ReaderTests(unittest.TestCase):
    """The JSONL reader must keep its payload shape while passing fork through."""

    EXPECTED_KEYS = {
        "name",
        "hash",
        "from",
        "to",
        "value",
        "selector",
        "gas",
        "tip",
        "ts",
        "fork",
        "sim",
        "label",
    }

    def test_payload_shape_is_unchanged(self) -> None:
        with mock.patch.object(hits_api, "LOG_PATH", FIXTURES / "all_green.jsonl"):
            hits = hits_api.HitsHandler._read_recent(50)
        self.assertEqual(len(hits), 5)
        self.assertEqual(set(hits[0]), self.EXPECTED_KEYS)
        self.assertEqual(hits[0]["fork"]["block"], 100)
        # Newest first: the last line of the fixture is returned first.
        self.assertTrue(hits[0]["hash"].endswith("5"))

    def test_old_records_carry_an_empty_fork_object(self) -> None:
        with mock.patch.object(hits_api, "LOG_PATH", FIXTURES / "old_records.jsonl"):
            hits = hits_api.HitsHandler._read_recent(50)
        self.assertTrue(all(hit["fork"] == {} for hit in hits))
        summary_from_reader = hits_api.summarise_quality(
            hits, max_lag=MAX_LAG, poll_seconds=POLL_SECONDS, now=NOW
        )
        self.assertFalse(summary_from_reader["rollup"]["available"])
        self.assertEqual(summary_from_reader["banner"]["state"], "unknown")

    def test_empty_log_returns_nothing(self) -> None:
        with mock.patch.object(hits_api, "LOG_PATH", FIXTURES / "empty.jsonl"):
            self.assertEqual(hits_api.HitsHandler._read_recent(50), [])

    def test_missing_log_returns_nothing(self) -> None:
        with mock.patch.object(hits_api, "LOG_PATH", FIXTURES / "does-not-exist.jsonl"):
            self.assertEqual(hits_api.HitsHandler._read_recent(50), [])


class HelperTests(unittest.TestCase):
    def test_as_int_tolerates_noise(self) -> None:
        self.assertEqual(hits_api.as_int("7"), 7)
        self.assertEqual(hits_api.as_int(4.9), 4)
        self.assertIsNone(hits_api.as_int(None))
        self.assertIsNone(hits_api.as_int(True))
        self.assertIsNone(hits_api.as_int("n/a"))

    def test_median_even_and_odd(self) -> None:
        self.assertEqual(hits_api.median([3, 1, 2]), 2.0)
        self.assertEqual(hits_api.median([4, 1]), 2.5)
        self.assertIsNone(hits_api.median([]))

    def test_thresholds_read_from_environment(self) -> None:
        with mock.patch.dict(
            "os.environ", {"FORK_MAX_LAG": "9", "FORK_POLL_SECONDS": "6.5"}, clear=False
        ):
            self.assertEqual(hits_api.fork_max_lag(), 9)
            self.assertEqual(hits_api.fork_poll_seconds(), 6.5)
        with mock.patch.dict(
            "os.environ", {"FORK_MAX_LAG": "nope", "FORK_POLL_SECONDS": "bad"}, clear=False
        ):
            self.assertEqual(hits_api.fork_max_lag(), hits_api.FORK_MAX_LAG_DEFAULT)
            self.assertEqual(hits_api.fork_poll_seconds(), hits_api.FORK_POLL_SECONDS_DEFAULT)

    def test_bounded_limit(self) -> None:
        self.assertEqual(hits_api.bounded_limit("10", 50), 10)
        self.assertEqual(hits_api.bounded_limit("0", 50), 1)
        self.assertEqual(hits_api.bounded_limit("500", 50), hits_api.MAX_LIMIT)
        self.assertEqual(hits_api.bounded_limit("junk", 50), 50)


class PipelineSummaryTests(unittest.TestCase):
    def test_rust_revm_pipeline_telemetry_and_extractable_filter(self) -> None:
        records = [
            {
                "name": "claim",
                "hash": "0xaa",
                "from": "0x11",
                "to": "0x22",
                "value": "0",
                "selector": "0x4e71d92d",
                "gas": 120000,
                "tip": 1000000000,
                "ts": "2026-11-13T12:00:00.000Z",
                "fork": {"block": 100, "head": 100, "lag": 0, "code_present": True, "checked": [100, 99]},
                "sim": {"ok": True, "engine": "revm", "gas_used": 44000, "result_or_error": "0x"},
                "decoded": {"signature": "claim()", "category": "claim", "decode_ok": True, "summary": "claim()"},
                "decision": {"status": "extractable", "would_frontrun": True, "net_profit_wei": "90000000000000000"},
                "telemetry": {"ingest_us": 100, "decode_us": 6, "sim_us": 420, "probe_us": 180, "total_us": 706},
                "label": "candidate:claim",
            },
            {
                "name": "claim_merkle_indexed",
                "hash": "0xbb",
                "from": "0x33",
                "to": "0x44",
                "value": "0",
                "selector": "0x2e7ba6ef",
                "gas": 150000,
                "tip": 1000000000,
                "ts": "2026-11-13T12:00:01.000Z",
                "fork": {"block": 100, "head": 100, "lag": 0, "code_present": True, "checked": [100, 99]},
                "sim": {"ok": True, "engine": "revm", "gas_used": 62000, "result_or_error": "0x"},
                "decoded": {"signature": "claim(uint256,address,uint256,bytes32[])", "category": "claim", "decode_ok": True, "summary": "claim(...)"},
                "decision": {"status": "beneficiary_locked", "would_frontrun": False, "net_profit_wei": "0"},
                "telemetry": {"ingest_us": 80, "decode_us": 10, "sim_us": 500, "probe_us": 200, "total_us": 790},
                "label": "candidate:claim_merkle_indexed",
            },
        ]
        res = hits_api.summarise_quality(
            records, filter_name="all", max_lag=MAX_LAG, poll_seconds=POLL_SECONDS, now=NOW
        )
        pipe = res["pipeline"]
        self.assertEqual(pipe["engine"], "rust-alloy-revm")
        self.assertEqual(pipe["would_frontrun"], 1)
        self.assertEqual(pipe["frontrun_counts"]["extractable"], 1)
        self.assertEqual(pipe["frontrun_counts"]["beneficiary_locked"], 1)
        self.assertEqual(pipe["categories"]["claim"], 2)
        self.assertEqual(pipe["latency_us"]["median_sim"], 460.0)

        ext_only = hits_api.summarise_quality(
            records, filter_name="extractable", max_lag=MAX_LAG, poll_seconds=POLL_SECONDS, now=NOW
        )
        self.assertEqual(ext_only["window"]["filtered"], 1)
        self.assertEqual(ext_only["pipeline"]["frontrun_counts"]["extractable"], 1)

    def test_extractable_bar_totals_and_closed_removal(self) -> None:
        records = [
            {
                "name": "claim",
                "hash": "0x01",
                "from": "0x11",
                "to": "0x22",
                "value": "0",
                "selector": "0x4e71d92d",
                "gas": 120000,
                "tip": 1000000000,
                "ts": "2026-11-13T12:00:00.000Z",
                "fork": {"block": 100, "head": 100, "lag": 0, "code_present": True, "checked": [100, 99]},
                "sim": {"ok": True, "engine": "revm", "gas_used": 44000, "result_or_error": "0x"},
                "decision": {
                    "status": "extractable",
                    "would_frontrun": True,
                    "gross_value_wei": "500000000000000000",
                    "net_profit_wei": "45000000000000000",
                    "probe_token_gains": 1,
                },
                "label": "candidate:claim",
            },
            {
                "name": "withdraw",
                "hash": "0x02",
                "from": "0x33",
                "to": "0x44",
                "value": "0",
                "selector": "0x2e1a7d4d",
                "gas": 140000,
                "tip": 1000000000,
                "ts": "2026-11-13T12:00:02.000Z",
                "fork": {"block": 100, "head": 100, "lag": 0, "code_present": True, "checked": [100, 99]},
                "sim": {"ok": True, "engine": "revm", "gas_used": 51000, "result_or_error": "0x"},
                "decision": {
                    "status": "extractable",
                    "would_frontrun": True,
                    "gross_value_wei": "1250000000000000000",
                    "net_profit_wei": "110000000000000000",
                    "probe_token_gains": 0,
                },
                "label": "candidate:withdraw",
            },
            {
                "name": "claim",
                "hash": "0x03",
                "closed": True,
                "decision": {
                    "status": "extractable",
                    "would_frontrun": True,
                    "gross_value_wei": "9000000000000000000",
                    "net_profit_wei": "900000000000000000",
                },
                "label": "candidate:claim",
            },
        ]
        res = hits_api.summarise_quality(
            records,
            filter_name="failed",
            max_lag=MAX_LAG,
            poll_seconds=POLL_SECONDS,
            now=NOW,
            meta={"total_pruned": 7, "total_observed": 25},
        )
        bar = res["extractable"]
        self.assertEqual(bar["open_count"], 2)
        self.assertEqual(bar["profitable_count"], 2)
        self.assertEqual(bar["total_gross_wei"], "1750000000000000000")
        self.assertEqual(bar["total_net_wei"], "155000000000000000")
        self.assertEqual(bar["currency"], "INR")
        self.assertEqual(bar["token_transfers"], 1)
        self.assertEqual(bar["closed_removed"], 7)
        self.assertEqual(bar["total_observed"], 25)

    def test_inr_conversion_and_indian_grouping(self) -> None:
        records = [
            {
                "name": "claim",
                "hash": "0x01",
                "decision": {
                    "status": "extractable",
                    "would_frontrun": True,
                    "gross_value_wei": "1750000000000000000",  # 1.75 ETH * ₹260,000 = ₹4,55,000.00
                    "net_profit_wei": "155000000000000000",  # 0.155 ETH * ₹260,000 = +₹40,300.00
                    "probe_token_gains": 1,
                },
                "label": "candidate:claim",
            }
        ]
        res = hits_api.summarise_quality(
            records,
            max_lag=MAX_LAG,
            poll_seconds=POLL_SECONDS,
            now=NOW,
            eth_inr=260000.0,
            rate_source="coinbase",
        )
        self.assertEqual(res["fx"], {"currency": "INR", "eth_inr": 260000.0, "source": "coinbase"})
        self.assertEqual(res["extractable"]["total_gross_inr"], "₹4,55,000.00")
        self.assertEqual(res["extractable"]["total_net_inr"], "+₹40,300.00")

        empty_res = hits_api.summarise_quality(
            [], max_lag=MAX_LAG, poll_seconds=POLL_SECONDS, now=NOW, eth_inr=260000.0
        )
        self.assertEqual(empty_res["extractable"]["total_gross_inr"], "₹0.00")
        self.assertEqual(empty_res["extractable"]["total_net_inr"], "₹0.00")

    def test_zero_value_legacy_extractable_is_excluded_and_recent_value_sums(self) -> None:
        records = [
            {
                "name": "universal_router_execute",
                "hash": "0xdead",
                "value": "100000000000000000",  # 0.1 ETH = ₹26,000.00
                "decision": {
                    "status": "extractable",
                    "would_frontrun": True,
                    "gross_value_wei": "0",
                    "net_profit_wei": "0",
                    "probe_token_gains": 1,
                },
                "label": "candidate:universal_router_execute",
            },
            {
                "name": "claim",
                "hash": "0xbeef",
                "value": "0",
                "decision": {
                    "status": "extractable",
                    "would_frontrun": True,
                    "gross_value_wei": "500000000000000000",  # 0.5 ETH = ₹1,30,000.00
                    "net_profit_wei": "45000000000000000",  # 0.045 ETH = +₹11,700.00
                    "probe_token_gains": 0,
                },
                "label": "candidate:claim",
            },
        ]
        res = hits_api.summarise_quality(
            records, max_lag=MAX_LAG, poll_seconds=POLL_SECONDS, now=NOW, eth_inr=260000.0
        )
        bar = res["extractable"]
        self.assertEqual(bar["open_count"], 1)
        self.assertEqual(bar["total_gross_inr"], "₹1,30,000.00")
        self.assertEqual(bar["total_net_inr"], "+₹11,700.00")
        self.assertEqual(bar["active_tx_count"], 2)
        self.assertEqual(bar["recent_tx_value_inr"], "₹1,56,000.00")


class LiquidationSummaryTests(unittest.TestCase):
    def test_aave_v3_flash_liquidation_math_and_inr_summary(self) -> None:
        drop_safe, trig_eth, safe_plan = hits_api.compute_flash_plan_dict(
            10000.0, 6000.0, 8000.0 / 6000.0, 2000.0, base_fee_gwei=1.0, close_factor=0.50
        )
        self.assertEqual(drop_safe, 25.0)
        self.assertEqual(trig_eth, 1500.0)
        self.assertFalse(safe_plan["profitable_now"])

        drop_liq, _, liq_plan = hits_api.compute_flash_plan_dict(
            7000.0, 6000.0, 5600.0 / 6000.0, 1400.0, base_fee_gwei=1.0, close_factor=0.50
        )
        self.assertEqual(drop_liq, 0.0)
        self.assertEqual(liq_plan["debt_to_cover_usd"], 3000.0)
        self.assertEqual(liq_plan["seized_collateral_usd"], 3150.0)
        self.assertEqual(liq_plan["gross_bounty_usd"], 150.0)
        self.assertEqual(liq_plan["flash_loan_fee_usd"], 1.5)
        self.assertTrue(liq_plan["profitable_now"])

        snapshot = {
            "status": "live",
            "head_block": 21200000,
            "scanned_from_block": 21194000,
            "scanned_blocks_total": 6001,
            "discovered_borrowers_total": 42,
            "eth_price_usd": 2000.0,
            "base_fee_gwei": 1.0,
            "positions": [
                {
                    "user": "0x1111111111111111111111111111111111111111",
                    "collateral_usd": 7000.0,
                    "debt_usd": 6000.0,
                    "available_borrows_usd": 0.0,
                    "liq_threshold_pct": 80.0,
                    "ltv_pct": 75.0,
                    "health_factor": 0.9333,
                    "status": "liquidatable",
                    "price_drop_pct_to_liq": 0.0,
                    "eth_liq_price_usd": 1500.0,
                    "plan": liq_plan,
                },
                {
                    "user": "0x2222222222222222222222222222222222222222",
                    "collateral_usd": 20000.0,
                    "debt_usd": 15000.0,
                    "available_borrows_usd": 500.0,
                    "liq_threshold_pct": 81.0,
                    "ltv_pct": 78.0,
                    "health_factor": 1.08,
                    "status": "at_risk",
                    "price_drop_pct_to_liq": 7.41,
                    "eth_liq_price_usd": 1851.85,
                    "plan": safe_plan,
                },
            ],
            "recent_liquidations": [],
        }

        # eth_inr = 170,000 and eth_price_usd = 2,000 => usd_inr = 85.0
        res = hits_api.summarise_liquidations(snapshot, eth_inr=170000.0, rate_source="coinbase")
        self.assertEqual(res["fx"]["usd_inr"], 85.0)
        self.assertEqual(res["summary"]["tracked_count"], 2)
        self.assertEqual(res["summary"]["liquidatable_count"], 1)
        self.assertEqual(res["summary"]["at_risk_count"], 2)
        self.assertEqual(res["summary"]["lowest_hf"], 0.9333)
        # $150 gross bounty * 85 INR/USD = ₹12,750.00
        self.assertEqual(res["summary"]["total_liquidatable_bounty_inr"], "₹12,750.00")

        only_liq = hits_api.summarise_liquidations(
            snapshot, filter_name="liquidatable", eth_inr=170000.0
        )
        self.assertEqual(len(only_liq["positions"]), 1)
        self.assertEqual(only_liq["positions"][0]["user"], "0x1111111111111111111111111111111111111111")

    def test_inspect_wallet_rejects_invalid_hex_cleanly(self) -> None:
        bad = hits_api.inspect_aave_v3_wallet("not-a-valid-address")
        self.assertFalse(bad["ok"])
        self.assertIn("error", bad)


if __name__ == "__main__":
    unittest.main()
