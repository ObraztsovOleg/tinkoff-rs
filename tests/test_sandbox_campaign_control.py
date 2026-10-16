"""Supervisor transition tests with no broker writes or running campaigns."""
import copy
import importlib.util
import json
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import Mock
import uuid

SPEC = importlib.util.spec_from_file_location("campaign_control_tests", Path(__file__).resolve().parents[1] / "scripts/campaign_control.py")
controls = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(controls)


class CampaignControlTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.work = Path(temporary.name)
        (self.work / "configs").mkdir()
        (self.work / "configs/first.toml").write_text('strategy_id = "first"\n[[universe]]\nuid = "old"\n[[strategies]]\nkind = "moving-average-trend"\n')
        self.state = {"mode": "campaign", "phase": "observing", "instruments": [{"uid": "old", "ticker": "OLD"}, {"uid": "new", "ticker": "NEW"}], "cases": {"test": {"kind": "test", "strategy_id": "first", "account_id": "account", "config": "configs/first.toml", "state": "paused", "pid": 123}}}
        self.campaign = SimpleNamespace(WORK=self.work, run_sql=Mock(side_effect=self.sql), bot=Mock(), stop_process=Mock(), persist=Mock(), process_alive=Mock(return_value=False))
        self.enabled = False
        self.command = {"id": str(uuid.uuid4()), "strategy_id": "first", "account_id": "account", "kind": "universe", "payload": {"instruments": ["new"]}, "result": {}}

    def sql(self, state, query):
        if query.startswith("SELECT row_to_json"):
            return json.dumps({"enabled": self.enabled})
        if query.startswith("SELECT coalesce(json_agg"):
            return "[]"
        return ""

    def test_universe_rejects_enabled_or_unknown_instrument_before_stopping_runtime(self):
        self.enabled = True
        with self.assertRaisesRegex(ValueError, "выключите"):
            controls.universe(self.campaign, self.state, self.command)
        self.enabled = False
        self.command["payload"]["instruments"] = ["unknown"]
        with self.assertRaisesRegex(ValueError, "каталог"):
            controls.universe(self.campaign, self.state, self.command)
        self.campaign.bot.assert_not_called()
        self.campaign.stop_process.assert_not_called()
        self.assertEqual(self.state["cases"]["test"]["strategy_id"], "first")

    def test_config_failure_never_stops_current_runtime(self):
        self.campaign.bot.return_value = SimpleNamespace(returncode=1, stderr="incompatible universe")
        with self.assertRaisesRegex(ValueError, "требованиям"):
            controls.universe(self.campaign, self.state, self.command)
        self.campaign.stop_process.assert_not_called()
        self.assertEqual(self.state["cases"]["test"]["strategy_id"], "first")

    def test_unresolved_positions_prevent_ownership_transfer_and_warmup(self):
        self.campaign.bot.side_effect = [SimpleNamespace(returncode=0, stderr=""), SimpleNamespace(returncode=1, stderr="different owner"), SimpleNamespace(returncode=1, stderr="portfolio not flat")]
        with self.assertRaisesRegex(ValueError, "закройте позиции"):
            controls.universe(self.campaign, self.state, self.command)
        self.assertEqual(self.state["cases"]["test"]["strategy_id"], "first")
        self.assertEqual([call.args[2] for call in self.campaign.bot.call_args_list], ["check-config", "risk-status", "risk-reset"])
        self.campaign.persist.assert_not_called()

    def test_success_preserves_old_revision_and_pauses_new_strategy_after_warmup(self):
        self.campaign.bot.side_effect = [SimpleNamespace(returncode=0, stderr=""), SimpleNamespace(returncode=1, stderr="different owner"), SimpleNamespace(returncode=0, stderr=""), SimpleNamespace(returncode=0, stderr="")]
        controls.universe(self.campaign, self.state, self.command)
        case = self.state["cases"]["test"]
        self.assertEqual(case["state"], "paused")
        self.assertEqual(case["revisions"], [{"strategy_id": "first", "config": "configs/first.toml"}])
        self.assertNotEqual(case["strategy_id"], "first")
        self.assertNotIn("pid", case)
        self.assertEqual(case["preflight_result"], "passed")
        self.assertIn('--startup-only', self.campaign.bot.call_args_list[-1].args)
        new_config = (self.work / case["config"]).read_text()
        self.assertIn('uid = "new"', new_config)
        self.assertNotIn('uid = "old"', new_config)
        self.assertIn('uid = "old"', (self.work / 'configs/first.toml').read_text())

    def test_free_mode_pauses_once_and_does_not_rewrite_campaign_schedule(self):
        self.state["sessions"] = [{"date": "2026-10-02"}]
        original = copy.deepcopy(self.state["sessions"])
        value = {"id": str(uuid.uuid4()), "kind": "mode", "payload": {"mode": "free"}}
        controls.mode(self.campaign, self.state, value)
        sql = self.campaign.run_sql.call_args.args[1]
        self.assertIn("FOR UPDATE", sql)
        self.assertIn("mode FROM dashboard_settings WHERE id)<>'free'", sql)
        self.assertIn("enabled=false", sql)
        self.assertIn("ON CONFLICT DO NOTHING", sql)
        self.assertEqual(self.state["mode"], "free")
        self.assertEqual(self.state["sessions"], original)
        self.campaign.bot.assert_not_called()

    def test_universe_waits_for_capacity_before_creating_or_starting_revision(self):
        for n in range(4):
            self.state["cases"][str(n)] = {"kind": str(n), "strategy_id": str(n), "pid": n+10, "config": str(n)+".toml"}
        self.campaign.process_alive.return_value = True
        controls.universe(self.campaign, self.state, self.command)
        self.campaign.bot.assert_not_called()
        self.campaign.stop_process.assert_not_called()
        self.assertEqual(self.state["cases"]["test"]["strategy_id"], "first")
        self.assertIn("'running'", self.campaign.run_sql.call_args.args[1])
        self.assertEqual(len(list((self.work / "configs").iterdir())), 1)

    def test_universe_waits_for_already_queued_account_command(self):
        self.campaign.run_sql.side_effect = lambda state, query: "t" if query.startswith("SELECT EXISTS") else self.sql(state, query)
        controls.universe(self.campaign, self.state, self.command)
        self.campaign.bot.assert_not_called()
        self.campaign.stop_process.assert_not_called()
        self.assertIn("'running'", self.campaign.run_sql.call_args.args[1])

    def test_account_return_creates_fresh_revision_and_routes_without_mutating_request(self):
        self.campaign.run_sql.side_effect = lambda _, query: "t" if query.startswith("SELECT EXISTS") else ""
        case = self.state["cases"]["test"]
        controls.renew_returning_strategy(self.campaign, self.state, case, "previous-epoch")
        first_id, first_config = case["strategy_id"], case["config"]
        self.assertNotEqual(first_id, "first")
        self.assertEqual(case["revisions"], [{"strategy_id": "first", "config": "configs/first.toml"}])
        query = self.campaign.run_sql.call_args.args[1]
        self.assertIn("SET execution_strategy_id=", query)
        self.assertNotIn("SET strategy_id=", query)
        self.assertNotIn("SET payload=", query)
        self.assertIn("ON CONFLICT DO NOTHING", query)
        # Replay after SQL commit but before campaign.json persistence is deterministic.
        original = {"kind": "test", "strategy_id": "first", "account_id": "account", "config": "configs/first.toml"}
        controls.renew_returning_strategy(self.campaign, self.state, original, "previous-epoch")
        self.assertEqual((original["strategy_id"], original["config"]), (first_id, first_config))
        self.campaign.bot.assert_not_called()

    def test_unused_successor_keeps_original_strategy_id(self):
        self.campaign.run_sql.side_effect = lambda *_: "f"
        controls.renew_returning_strategy(self.campaign, self.state, self.state["cases"]["test"], "epoch")
        self.assertEqual(self.state["cases"]["test"]["strategy_id"], "first")
        self.campaign.persist.assert_not_called()


if __name__ == "__main__":
    unittest.main()
