import datetime as dt
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import Mock, patch

spec = importlib.util.spec_from_file_location("market_feed", Path(__file__).resolve().parents[1] / "scripts/dashboard/market_feed.py")
feed = importlib.util.module_from_spec(spec)
spec.loader.exec_module(feed)


class MarketFeedTests(unittest.TestCase):
    def test_all_intervals_respect_both_period_and_count_limits(self):
        end = dt.datetime(2026, 10, 1, 12, tzinfo=dt.timezone.utc)
        for name, (_, retention, seconds, maximum, limit) in feed.INTERVALS.items():
            with self.subTest(interval=name):
                windows = list(feed.windows(end - retention, end, name))
                self.assertTrue(windows)
                self.assertEqual(windows[-1][1], end)
                for beginning, ending in windows:
                    self.assertLessEqual(ending - beginning, maximum)
                    self.assertLessEqual((ending - beginning).total_seconds() / seconds + 1, limit)
                    self.assertGreater(ending - beginning, dt.timedelta(seconds=seconds))
        self.assertGreater(len(list(feed.windows(end - dt.timedelta(days=14), end, "5m"))), 1)

    def test_fast_candles_are_fetched_only_for_active_known_subscriptions(self):
        now = dt.datetime(2026, 10, 1, 12, tzinfo=dt.timezone.utc)
        api = Mock()
        api.call.return_value = {"candles": []}
        cache = {}
        requests = {"active": {"uid": "uid", "interval": "5s", "expires_at": now.timestamp() + 20},
                    "expired": {"uid": "uid", "interval": "10s", "expires_at": now.timestamp() - 1},
                    "foreign": {"uid": "other", "interval": "30s", "expires_at": now.timestamp() + 20}}
        with patch.object(feed.campaign, "now", return_value=now):
            feed.update(api, {"instruments": [{"uid": "uid", "ticker": "SBER"}]}, cache, Mock(), requests, background_limit=0)
            self.assertEqual(api.call.call_count, 1)
            self.assertEqual(api.call.call_args.args[1]["interval"], "CANDLE_INTERVAL_5_SEC")
            feed.update(api, {"instruments": [{"uid": "uid", "ticker": "SBER"}]}, cache, Mock(), requests, background_limit=0)
            self.assertEqual(api.call.call_count, 1)

    def test_backoff_preserves_prices_and_prevents_repeated_failed_requests(self):
        now = dt.datetime(2026, 10, 1, 12, tzinfo=dt.timezone.utc)
        api = Mock()
        api.call.side_effect = RuntimeError("rate limited")
        cache = {}
        request = {"active": {"uid": "uid", "interval": "5s", "expires_at": now.timestamp() + 20}}
        with patch.object(feed.campaign, "now", return_value=now):
            for _ in range(2):
                feed.update(api, {"instruments": [{"uid": "uid", "ticker": "SBER"}]}, cache, Mock(), request, background_limit=0)
        self.assertEqual(api.call.call_count, 1)
        self.assertTrue(cache["assets"]["uid"]["series"]["5s"]["retry_at"])

    def test_candle_refresh_replaces_forming_bar_without_duplicates(self):
        now = dt.datetime(2026, 10, 1, 12, tzinfo=dt.timezone.utc)
        stamp = "2026-10-01T11:30:00Z"
        bar = {"time": stamp, "open": {"units": "100"}, "high": {"units": "102"},
               "low": {"units": "99"}, "close": {"units": "101", "nano": 500000000}, "volume": "10", "isComplete": True}
        api = Mock()
        api.call.return_value = {"candles": [bar]}
        cache = {"assets": {"uid": {"ticker": "SBER", "series": {"30m": {
            "observed_at": now.isoformat(), "candles": [{"time": stamp, "close": 100, "complete": False}]}}}}}
        with patch.object(feed.campaign, "now", return_value=now):
            feed.update(api, {"instruments": [{"uid": "uid", "ticker": "SBER"}]}, cache, Mock())
        result = cache["assets"]["uid"]["series"]["30m"]
        self.assertEqual(len(result["candles"]), 1)
        self.assertTrue(result["candles"][0]["complete"])
        self.assertEqual(result["candles"][0]["close"], 101.5)
        for call in api.call.call_args_list:
            self.assertEqual(call.args[0], "MarketDataService/GetCandles")
            self.assertNotIn("mutate", call.kwargs)
            request = call.args[1]
            span = feed.campaign.timestamp(request["to"]) - feed.campaign.timestamp(request["from"])
            self.assertLessEqual(span, dt.timedelta(days=365 if request["interval"] == "CANDLE_INTERVAL_DAY" else 14))

    def test_failed_refresh_keeps_previous_prices_and_timestamp(self):
        cache = {"assets": {"uid": {"ticker": "SBER", "series": {"day": {
            "observed_at": "2026-10-01T09:00:00Z", "candles": [{"time": "2026-09-30T00:00:00Z", "close": 100}]}}}}}
        api = Mock()
        api.call.side_effect = RuntimeError("network unavailable")
        feed.update(api, {"instruments": [{"uid": "uid", "ticker": "SBER"}]}, cache, Mock())
        result = cache["assets"]["uid"]["series"]["day"]
        self.assertTrue(result["error"])
        self.assertEqual(result["observed_at"], "2026-10-01T09:00:00Z")
        self.assertEqual(result["candles"][0]["close"], 100)


if __name__ == "__main__":
    unittest.main()
