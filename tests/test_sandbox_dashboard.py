"""Dashboard boundary tests: snapshots, shared accounts, and read-only HTTP."""
import http.client
import importlib.util
import json
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("dashboard", Path(__file__).resolve().parents[1] / "scripts/dashboard/server.py")
dashboard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(dashboard)


class DashboardTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.work = Path(self.temporary.name)
        (self.work / "reports").mkdir()
        self.write("campaign.json", {
            "id": "campaign", "database_password": "TOP_SECRET", "database_url": "SECRET_URL",
            "cases": {name: {"kind": name, "account_id": "shared", "strategy_id": name,
                              "config": name + ".toml", "state": "prepared", "cohort": i}
                      for i, name in enumerate(["first", "second"])},
            "instruments": [{"uid": "share", "ticker": "SBER"}],
        })
        self.write("reports/database.json", {"risk": [{"account_id": "shared", "strategy_id": "first",
                                                       "epoch": "epoch", "state": {"budget_rub": "1000000"}}],
             "intent_audit": [{"id": "intent", "request_id": "request", "strategy_id": "first",
                                "account_id": "shared", "event_id": "cleanup:example:leg:0", "reason": "TOP_SECRET",
                                "instrument_id": "share", "lots": 1, "status": "filled"}],
             "fills": [{"trade_id": "trade", "request_id": "request", "price": "101.25", "lots": 1}]})
        self.broker = {"observed_at": "2026-10-01T09:00:00Z", "accounts": {"first": {
            "account_id": "shared", "portfolio": {"totalAmountPortfolio": {"units": "1000000", "nano": 250000000},
            "positions": [{"figi": "RUB000UTSTOM", "instrumentType": "currency", "quantity": {"units": "1000000"}},
                          {"instrumentUid": "share", "quantity": {"units": "-10"}, "quantityLots": {"units": "-1"},
                           "currentPrice": {"units": "101", "nano": 250000000, "currency": "rub"}}]},
            "orders": {"orders": []}, "operations": []}}}
        self.broker["accounts"]["second"] = self.broker["accounts"]["first"]
        self.write("reports/broker-latest.json", self.broker)

    def test_schedule_and_universe_come_from_campaign_not_inferred_from_name(self):
        state = dashboard.read_json(self.work / "campaign.json")
        state["sessions"] = [{"startTime": "2026-10-02T06:10:00Z"}, {"startTime": "2026-10-05T06:10:00Z"}, {"startTime": "2026-10-06T06:10:00Z"}]
        state["cases"]["first"]["config"] = "configs/first.toml"
        (self.work / "configs").mkdir()
        (self.work / "configs/first.toml").write_text('[[universe]]\nuid = "share"\n\n[[strategies]]\nkind = "example"\n')
        self.write("campaign.json", state)
        result = dashboard.snapshot(self.work)
        self.assertEqual(result["cases"][0]["assets"], [{"uid": "share", "ticker": "SBER", "benchmark": False}])
        self.assertEqual(result["cases"][0]["launch_at"], "2026-10-02T05:10:00+00:00")
        self.assertEqual(result["cases"][1]["launch_at"], "2026-10-06T05:10:00+00:00")

    def test_market_snapshot_is_filtered_and_does_not_expose_cache_metadata(self):
        self.write("reports/market-chart.json", {"secret": "TOP_SECRET", "assets": {"share": {"ticker": "SBER", "series": {"day": {
            "observed_at": "2026-10-01T09:00:00Z", "error": False,
            "candles": [{"time": "2026-09-30T00:00:00Z", "open": 100, "close": 105, "secret": "TOP_SECRET"}]}}}}})
        result = dashboard.market_snapshot(self.work, "share", "day")
        self.assertEqual(result["candles"][0]["close"], 105)
        self.assertNotIn("TOP_SECRET", json.dumps(result))
        self.assertIsNone(dashboard.market_snapshot(self.work, "share", "unknown"))
        self.assertIsNone(dashboard.market_snapshot(self.work, "../campaign.json", "day"))

    def write(self, name, value):
        (self.work / name).write_text(json.dumps(value))

    def test_shared_account_is_counted_once_and_future_owner_is_not_assigned_positions(self):
        result = dashboard.snapshot(self.work)
        self.assertEqual(len(result["accounts"]), 1)
        account = result["accounts"][0]
        self.assertEqual(account["equity"], 1000000.25)
        self.assertEqual(account["owner"], "first")
        self.assertEqual(account["cash"], 1000000)
        self.assertEqual(len(account["positions"]), 1)
        self.assertEqual(account["positions"][0]["lots"], -1)
        self.assertEqual(account["positions"][0]["instrument"], "SBER")
        self.assertTrue(result["cases"][0]["owns_account"])
        self.assertFalse(result["cases"][1]["owns_account"])

    def test_response_excludes_credentials_and_marks_maintenance_fills(self):
        result = dashboard.snapshot(self.work)
        encoded = json.dumps(result)
        for forbidden in ["TOP_SECRET", "SECRET_URL", "database_password", "database_url", "reason"]:
            self.assertNotIn(forbidden, encoded)
        self.assertTrue(result["orders"][0]["maintenance"])
        self.assertTrue(result["fills"][0]["maintenance"])
        self.assertEqual(result["fills"][0]["strategy"], "first")

    def test_missing_broker_snapshot_is_unknown_not_zero(self):
        (self.work / "reports/broker-latest.json").unlink()
        result = dashboard.snapshot(self.work)
        self.assertIsNone(result["accounts"][0]["equity"])
        self.assertFalse(result["accounts"][0]["available"])
        self.assertTrue(result["warnings"])

    def test_history_is_persistent_and_deduplicated_by_broker_time(self):
        feed = dashboard.Feed(self.work)
        feed.update()
        feed.update()
        restarted = dashboard.Feed(self.work)
        restarted.update()
        self.assertEqual(len(restarted.history), 1)
        self.broker["observed_at"] = "2026-10-01T09:05:00Z"
        self.write("reports/broker-latest.json", self.broker)
        restarted.update()
        self.assertEqual(len(restarted.history), 2)
        self.assertEqual(sum(restarted.history[0]["accounts"].values()), 1000000.25)
        self.assertEqual((self.work / "dashboard-equity.jsonl").stat().st_mode & 0o777, 0o600)

    def test_partial_history_does_not_claim_a_complete_portfolio(self):
        state = dashboard.read_json(self.work / "campaign.json")
        state["cases"]["second"]["account_id"] = "other"
        self.write("campaign.json", state)
        self.broker["accounts"].pop("second")
        self.write("reports/broker-latest.json", self.broker)
        feed = dashboard.Feed(self.work)
        feed.update()
        self.assertFalse(feed.history[0]["complete"])

    def test_http_exposes_only_assets_and_sanitized_get_endpoints(self):
        feed = dashboard.Feed(self.work)
        feed.update()
        server = dashboard.ThreadingHTTPServer(("127.0.0.1", 0), dashboard.Handler)
        server.feed = feed
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=2)
            for path in ["/campaign.json", "/../campaign.json", "/reports/database.json", "/%2e%2e/campaign.json"]:
                connection.request("GET", path)
                response = connection.getresponse()
                self.assertEqual(response.status, 404)
                response.read()
            connection.request("GET", "/api/snapshot")
            response = connection.getresponse()
            self.assertEqual(response.status, 200)
            self.assertEqual(response.getheader("Cache-Control"), "no-store")
            self.assertNotIn(b"TOP_SECRET", response.read())
            connection.request("GET", "/api/snapshot", headers={"Host": "untrusted.example"})
            response = connection.getresponse()
            self.assertEqual(response.status, 403)
            response.read()
            connection.request("POST", "/api/snapshot", body="{}")
            response = connection.getresponse()
            self.assertEqual(response.status, 404)
            response.read()
            connection.close()
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == "__main__":
    unittest.main()
