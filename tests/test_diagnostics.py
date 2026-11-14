import importlib.util
import io
import json
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("diagnostic_api", Path(__file__).parents[1] / "deploy" / "hits_api.py")
api = importlib.util.module_from_spec(spec)
spec.loader.exec_module(api)

class DiagnosticsTests(unittest.TestCase):
    def setUp(self):
        api._diagnostic_records.clear()

    def test_retains_pruned_rows_and_deduplicates(self):
        row = {"hash": "0x1", "ts": "now", "sim": {"ok": False, "result_or_error": "revm error"}}
        api.record_diagnostics([row])
        api.record_diagnostics([row])
        api.record_diagnostics([])
        self.assertEqual(api.diagnostic_snapshot()["records"], [row])

    def test_bounded_history_keeps_newest(self):
        with patch.object(api, "DIAGNOSTIC_LIMIT", 2):
            api.record_diagnostics([{"hash": str(i)} for i in (3, 2, 1)])
        self.assertEqual([r["hash"] for r in api.diagnostic_snapshot()["records"]], ["2", "3"])

    def test_redacts_urls_and_credentials_but_keeps_error(self):
        clean = api.redact_diagnostics({"sim": {"result_or_error": "failed https://rpc.test/v2/SECRET retry"}, "api_key": "secret"})
        self.assertEqual(clean["sim"]["result_or_error"], "failed [redacted URL] retry")
        self.assertEqual(clean["api_key"], "[redacted]")

    def test_download_headers_and_json(self):
        handler = object.__new__(api.HitsHandler)
        handler.wfile = io.BytesIO()
        headers = {}
        handler.send_response = lambda status: self.assertEqual(status, 200)
        handler.send_header = lambda k, v: headers.update({k: v})
        handler.end_headers = lambda: None
        handler._serve_diagnostics()
        self.assertIn("attachment", headers["Content-Disposition"])
        self.assertEqual(int(headers["Content-Length"]), len(handler.wfile.getvalue()))
        self.assertEqual(json.loads(handler.wfile.getvalue())["format"], "mempool-diagnostics-v1")

if __name__ == "__main__":
    unittest.main()
