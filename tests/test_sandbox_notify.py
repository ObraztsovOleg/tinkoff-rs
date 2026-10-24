import datetime as dt
import importlib.util
import json
import re
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest

SPEC = importlib.util.spec_from_file_location("notify", Path(__file__).resolve().parents[1] / "scripts/notify.py")
notify = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(notify)

STATE = {"cases": {"futures-trend": {"kind": "futures-trend", "title": "Тренд на фьючерсах", "strategy_id": "s-futures", "account_id": "acc-f",
                                     "state": "running", "blocks": [{"instruments": ["futures:BR"]}]},
                   "ai-sonnet": {"kind": "ai-sonnet", "title": "ИИ: Sonnet", "strategy_id": "s-ai", "account_id": "acc-ai", "state": "restart backoff"}},
         "instruments": [{"uid": "sber-uid", "ticker": "SBER"}]}
CONFIG = {"TELEGRAM_BOT_TOKEN": "t", "TELEGRAM_CHAT_ID": "1", "NOTIFY_DAILY_HOUR": "19", "NOTIFY_TRADES": "yes"}
CREDIT = "strategy `ai-model` failed: sonnet: HTTP 400 Bad Request: Your credit balance is too low"


class FakeDatabase:
    def __init__(self):
        self.events = {"fills": [], "blocks": [], "ai_failures": []}

    def run_sql(self, state, query):
        if query.startswith("SELECT json_build_object("):
            # Like the database: only events after the cursor.
            since = dt.datetime.fromisoformat(re.search(r"'([^']+)'::timestamptz", query).group(1))
            keys = {"fills": "executed_at", "blocks": "recorded_at", "ai_failures": "created_at"}
            return json.dumps({group: [e for e in items if dt.datetime.fromisoformat(e[keys[group]]) > since]
                               for group, items in self.events.items()})
        return json.dumps([{"account_id": "acc-f", "equity": 1_010_000, "start": 1_000_000}])


class NotifyTests(unittest.TestCase):
    def test_events_are_sent_once_problems_when_they_appear_and_the_summary_once_a_day(self):
        database, sent = FakeDatabase(), []
        campaign = SimpleNamespace(run_sql=database.run_sql)
        sender = lambda config, text: sent.append(text)
        with tempfile.TemporaryDirectory() as work:
            morning = dt.datetime(2026, 10, 28, 7, 5, tzinfo=dt.timezone.utc)
            # The first run only remembers where it started, apart from current troubles.
            notify.tick(campaign, STATE, work, CONFIG, morning - dt.timedelta(minutes=10), sender)
            self.assertEqual(len(sent), 1)
            self.assertIn("ИИ: Sonnet</b> упал", sent[0])
            sent.clear()
            database.events = {
                "fills": [{"executed_at": "2026-10-28T07:00:30+00:00", "strategy_id": "s-futures", "instrument_id": "futures:BR",
                           "side": "ORDER_SIDE_SELL", "lots": 3, "price": "65.5", "event_id": "roll:futures:BR:3>0"},
                          {"executed_at": "2026-10-28T07:00:31+00:00", "strategy_id": "s-futures", "instrument_id": "sber-uid",
                           "side": "ORDER_SIDE_BUY", "lots": 2, "price": "300", "event_id": "schedule:x"}],
                "blocks": [{"strategy_id": "s-futures", "recorded_at": "2026-10-28T07:01:00+00:00", "reason": "daily loss", "detail": ""}],
                "ai_failures": [{"strategy_id": "s-ai", "created_at": "2026-10-28T07:02:00+00:00", "error": CREDIT},
                                {"strategy_id": "s-ai", "created_at": "2026-10-28T07:03:00+00:00", "error": CREDIT}]}
            notify.tick(campaign, STATE, work, CONFIG, morning, sender)
            text = "\n".join(sent)
            self.assertIn("продажа BR × 3 лот. по 65.5 (перенос фьючерса)", text)
            self.assertIn("покупка SBER × 2", text)
            self.assertIn("риск-блокировка — daily loss", text)
            self.assertEqual(text.count("закончились средства"), 1, "the same AI error once a day")
            self.assertNotIn("упал", text, "a known trouble is not repeated")
            # The cursor moved: the same events are not sent again.
            sent.clear()
            notify.tick(campaign, STATE, work, CONFIG, morning + dt.timedelta(minutes=1), sender)
            self.assertEqual(sent, [])
            memory = json.loads((Path(work) / "notify.json").read_text())
            self.assertEqual(memory["since"], "2026-10-28T07:03:00+00:00")
            # The daily summary after 19:00 MSK, once.
            database.events = {"fills": [], "blocks": [], "ai_failures": []}
            evening = dt.datetime(2026, 10, 28, 16, 1, tzinfo=dt.timezone.utc)
            notify.tick(campaign, STATE, work, CONFIG, evening, sender)
            notify.tick(campaign, STATE, work, CONFIG, evening + dt.timedelta(minutes=1), sender)
            summaries = [t for t in sent if "Итоги дня" in t]
            self.assertEqual(len(summaries), 1)
            self.assertIn("Тренд на фьючерсах: 1 010 000 ₽ (+1.00% за день)", summaries[0])

    def test_settings_need_a_token_and_a_chat(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "telegram.env"
            self.assertIsNone(notify.load_config(path))
            path.write_text("TELEGRAM_BOT_TOKEN=123:abc\n")
            with self.assertRaisesRegex(ValueError, "TELEGRAM_CHAT_ID"):
                notify.load_config(path)


if __name__ == "__main__":
    unittest.main()
