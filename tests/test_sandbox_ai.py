# -*- coding: utf-8 -*-
"""AI model strategy plumbing: block settings, background jobs and the dashboard.
No model, broker or token is contacted."""
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch
import uuid

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("campaign_ai", ROOT / "scripts/sandbox-campaign.py")
campaign = importlib.util.module_from_spec(spec)
spec.loader.exec_module(campaign)
controls = campaign.control_plane
spec = importlib.util.spec_from_file_location("control_ai", ROOT / "scripts/dashboard/control.py")
dashboard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(dashboard)
UIDS = [str(uuid.UUID(int=i + 1)) for i in range(3)]


def block(**params):
    strategy = {"kind": "ai-model", "weight": "1"}
    if params:
        strategy["params"] = params
    return [{"instruments": UIDS, "share": "100", "strategies": [strategy]}]


class AiBlockTests(unittest.TestCase):
    def test_settings_are_validated_and_never_carry_a_connection(self):
        controls.validate_blocks(block(model_profile="claude", instructions="Покупай на новостях", news_hours=12,
                                       max_weight_percent="30", allow_short=False, max_tool_rounds=6,
                                       decision_interval_minutes=60, news_burst_items=3, price_jump_percent="2"))
        controls.validate_blocks(block(decision_interval_minutes=0, news_burst_items=0))
        controls.validate_blocks(block(material_importance=4, market_move_percent="2", commodity_move_percent="3", currency_move_percent="1.5"))
        controls.validate_blocks(block(material_importance=0, market_move_percent="0"))
        for params in [{"token": "x"}, {"url": "http://x"}, {"model_profile": "bad name"}, {"news_hours": 0},
                       {"news_hours": "12"}, {"max_weight_percent": "150"}, {"allow_short": "yes"}, {"instructions": "x" * 4001},
                       {"decision_interval_minutes": 45}, {"news_burst_items": 99}, {"price_jump_percent": "80"},
                       {"material_importance": 6}, {"market_move_percent": "abc"}, {"currency_move_percent": "60"}]:
            with self.subTest(params), self.assertRaises(ValueError):
                controls.validate_blocks(block(**params))
        trend = [{"instruments": UIDS, "share": "100", "strategies": [{"kind": "moving-average-trend", "weight": "1", "params": {}}]}]
        with self.assertRaises(ValueError):
            controls.validate_blocks(trend)

    def test_block_settings_are_written_into_the_strategy_toml(self):
        state = {"benchmark": {"uid": "bench"}}
        text = controls.strategy_block(SimpleNamespace(ROOT=ROOT), state, "ai-model", "0.5", UIDS,
                                       {"model_profile": "local", "instructions": 'Цитата "в кавычках"\nи перенос', "news_hours": 6, "allow_short": True})
        self.assertIn('kind = "ai-model"\ninstruments = ' + json.dumps(UIDS), text)
        self.assertIn('model_profile = "local"', text)
        self.assertIn('instructions = "Цитата \\"в кавычках\\"\\nи перенос"', text)
        self.assertIn("news_hours = 6", text)
        self.assertIn("allow_short = true", text)
        self.assertIn('signal_weight = "0.5"', text)
        self.assertEqual(text.count("model_profile"), 1, "the example line is replaced, not duplicated")
        bot = ROOT / "target/debug/tinkoff-bot"
        if bot.exists():
            # The real parser of the bot accepts what the builder writes.
            with tempfile.TemporaryDirectory() as directory:
                config = Path(directory) / "ai.toml"
                header = 'contour = "sandbox"\naccount_id = "acc"\nstrategy_id = "ai-test"\n\n[risk]\nbudget_rub = "1000.00"\n'
                config.write_text(header + "".join(f'\n[[universe]]\nuid = "{uid}"\n' for uid in UIDS) + "\n" + text)
                result = subprocess.run([str(bot), "--config", str(config), "check-config"], capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn("kinds=ai-model", result.stdout)


class AiJobTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.work = Path(directory.name)
        (self.work / "configs").mkdir()
        (self.work / "configs/ai.toml").write_text('[[strategies]]\nkind = "ai-model"\n')
        (self.work / "configs/trend.toml").write_text('[[strategies]]\nkind = "moving-average-trend"\n')
        self.state = {"cases": {"ai": {"kind": "ai", "strategy_id": "camp-ai", "account_id": "acc", "config": "configs/ai.toml"},
                                "trend": {"kind": "trend", "strategy_id": "camp-trend", "account_id": "acc2", "config": "configs/trend.toml"}}}
        self.alive = True
        self.finished = []
        self.campaign = SimpleNamespace(WORK=self.work, BINARY=Path("/bin/tinkoff-bot"), subprocess=Mock(), os=Mock(), signal=campaign.signal,
                                        now=campaign.now, timestamp=campaign.timestamp, persist=Mock(), environment=Mock(return_value={}),
                                        process_alive=Mock(side_effect=lambda pid, marker: self.alive), run_sql=Mock(side_effect=self.sql))
        self.campaign.subprocess.Popen.return_value = SimpleNamespace(pid=42)
        self.campaign.subprocess.STDOUT = subprocess.STDOUT

    def sql(self, _state, query):
        self.finished.append(query)
        return ""

    def run_job(self, kind, payload, output):
        command = {"id": str(uuid.uuid4()), "kind": kind, "payload": payload}
        self.alive = True
        controls.ai_job(self.campaign, self.state, command)
        self.assertIn("'running'", self.finished[-1])
        args = self.campaign.subprocess.Popen.call_args.args[0]
        controls.ai_job(self.campaign, self.state, command)  # still running: nothing changes
        Path(self.state["ai_jobs"][command["id"]]["output"]).write_text(output)
        self.alive = False
        controls.ai_job(self.campaign, self.state, command)
        self.assertNotIn(command["id"], self.state["ai_jobs"])
        return args, json.loads(self.finished[-1].split("result='", 1)[1].split("'::jsonb", 1)[0].replace("''", "'"))

    def test_model_check_runs_in_background_and_reports_the_reply(self):
        args, result = self.run_job("ai_check", {"profile": "claude"}, 'INFO starting\n{"profile":"claude","reply":"готов","latency_ms":350}\n')
        self.assertEqual(args[-3:], ["ai-check", "--profile", "claude"])
        self.assertEqual(result["reply"], "готов")
        self.assertIn("350 мс", result["message"])

    def test_preview_reports_weights_and_failures_explain_the_reason(self):
        preview = {"previews": [{"weights": [{"ticker": "SBER", "weight_percent": 20}], "profile": "claude"}]}
        args, result = self.run_job("ai_preview", {"strategy_id": "camp-ai"}, json.dumps(preview) + "\n")
        self.assertEqual(args[-1], "ai-preview")
        self.assertIn("SBER 20%", result["message"])
        with self.assertRaises(ValueError) as error:
            self.run_job("ai_preview", {"strategy_id": "camp-ai"}, 'ERROR tinkoff_bot: command failed error=AI model token variable OPENAI_API_KEY is empty\n{"previews":[{"error":"AI model token variable OPENAI_API_KEY is empty"}]}\n')
        self.assertIn("OPENAI_API_KEY", str(error.exception))
        with self.assertRaises(ValueError):
            controls.ai_job(self.campaign, self.state, {"id": str(uuid.uuid4()), "kind": "ai_preview", "payload": {"strategy_id": "camp-trend"}})


class AiDashboardTests(unittest.TestCase):
    STATE = {"cases": {"ai": {"kind": "ai", "strategy_id": "camp-ai", "account_id": "acc"}}}

    def command(self, kind, payload):
        return {"id": str(uuid.uuid4()), "kind": kind, "strategy_id": None, "account_id": None, "payload": payload}

    def test_commands_are_global_and_validated(self):
        dashboard.validate(self.command("ai_check", {"profile": "claude"}), self.STATE)
        dashboard.validate(self.command("ai_check", {}), self.STATE)
        dashboard.validate(self.command("ai_preview", {"strategy_id": "camp-ai"}), self.STATE)
        for kind, payload in [("ai_check", {"profile": "../x"}), ("ai_preview", {"strategy_id": "other"}), ("ai_check", {"token": "x"})]:
            with self.subTest(kind), self.assertRaises(dashboard.ClientError):
                dashboard.validate(self.command(kind, payload), self.STATE)
        with self.assertRaises(dashboard.ClientError):
            dashboard.validate({**self.command("ai_check", {}), "strategy_id": "camp-ai", "account_id": "acc"}, self.STATE)

    def test_profiles_come_from_the_bot_and_are_cached_until_the_file_changes(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "models.toml"
            path.write_text("default = 'claude'\n")
            summary = {"path": str(path), "profiles": {"default": "claude", "models": [{"name": "claude", "provider": "anthropic", "token": "file"}]}}
            service = dashboard.Control(Path(directory))
            with patch.dict(dashboard.os.environ, {"AI_MODELS_FILE": str(path)}), \
                    patch.object(dashboard.subprocess, "run", return_value=SimpleNamespace(stdout="log line\n" + json.dumps(summary))) as run:
                first = service.ai_models()
                service.ai_models()
                self.assertEqual(run.call_count, 1, "unchanged file is not re-read")
                self.assertEqual((first["default"], first["models"][0]["token"]), ("claude", "file"))
                self.assertEqual(run.call_args.args[0][-1], "ai-profiles")
            with patch.dict(dashboard.os.environ, {"AI_MODELS_FILE": str(Path(directory) / "missing.toml")}):
                self.assertFalse(service.ai_models()["exists"])


class TopicTests(unittest.TestCase):
    def test_dashboard_topics_match_the_model_taxonomy(self):
        import re
        rust = re.findall(r'Topic\s*\{\s*id:\s*"([a-z_]+)",\s*name:\s*"([^"]+)"', (ROOT / "src/bin/support/documents/topics.rs").read_text(encoding="utf-8"))
        self.assertEqual(dict(rust), dashboard.TOPICS)

    def test_exposure_edits_are_validated(self):
        command = lambda payload, kind="exposure_set": {"id": str(uuid.uuid4()), "kind": kind, "strategy_id": None, "account_id": None, "payload": payload}
        checked = dashboard.validate(command({"ticker": "LKOH", "topic": "oil", "sensitivity": "1.5", "note": " экспортёр "}), {})
        self.assertEqual((checked["payload"]["sensitivity"], checked["payload"]["note"]), ("1.5", "экспортёр"))
        dashboard.validate(command({"ticker": "LKOH", "topic": "oil", "sensitivity": None, "note": ""}), {})
        dashboard.validate(command({"ticker": "SBER"}, "exposure_confirm"), {})
        for payload in [{"ticker": "lk'oh", "topic": "oil", "sensitivity": "1"}, {"ticker": "LKOH", "topic": "weather", "sensitivity": "1"},
                        {"ticker": "LKOH", "topic": "oil", "sensitivity": "3"}, {"ticker": "LKOH", "topic": "oil", "sensitivity": "0.25"},
                        {"ticker": "LKOH", "topic": "oil", "sensitivity": "1", "note": "x" * 301}]:
            with self.subTest(payload), self.assertRaises(dashboard.ClientError):
                dashboard.validate(command(payload), {})


if __name__ == "__main__":
    unittest.main()
