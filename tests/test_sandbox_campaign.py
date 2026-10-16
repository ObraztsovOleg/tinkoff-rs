"""Operational guardrails; no broker requests, tokens or Docker required."""
import importlib.util
import contextlib
import datetime as dt
import io
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch, Mock

spec = importlib.util.spec_from_file_location("campaign", Path(__file__).resolve().parents[1] / "scripts/sandbox-campaign.py")
campaign = importlib.util.module_from_spec(spec)
spec.loader.exec_module(campaign)


class CampaignTests(unittest.TestCase):
    def case(self, name, **values):
        return dict(kind=name, strategy_id=name, account_id="account", config=name + ".toml", state="prepared", attempts=0, **values)

    def test_successor_cannot_launch_until_predecessor_is_cleaned(self):
        old = self.case("old")
        new = self.case("new", predecessor="old")
        state = {"cases": {"old": old, "new": new}}
        with patch.object(campaign, "process_alive", return_value=False), patch.object(campaign, "bot") as bot:
            self.assertFalse(campaign.launch(state, new))
            bot.assert_not_called()
        self.assertEqual(new["state"], "waiting for predecessor cleanup")

    def test_unstarted_successor_cleanup_cannot_touch_previous_owner(self):
        new = self.case("new", predecessor="old")
        with patch.object(campaign, "persist"), patch.object(campaign, "bot") as bot, patch.object(campaign, "stop_process") as stop:
            campaign.cleanup_case({"cases": {"new": new}}, new)
            bot.assert_not_called()
            stop.assert_not_called()
        self.assertEqual(new["state"], "cleaned")
        self.assertIn("Not started", new["coverage_note"])

    def test_four_active_processes_prevent_another_launch(self):
        state = {"cases": {str(i): self.case(str(i), pid=i+100) for i in range(4)}}
        with patch.object(campaign, "process_alive", return_value=True), patch.object(campaign.subprocess, "Popen") as spawn:
            self.assertFalse(campaign.launch(state, self.case("extra")))
            spawn.assert_not_called()

    def test_transfer_requires_verified_previous_risk_owner(self):
        old = self.case("old")
        old["state"] = "cleaned"
        new = self.case("new", predecessor="old")
        state = {"cases": {"old": old, "new": new}}
        failure = Mock(returncode=1, stdout="", stderr="database unavailable")
        with patch.object(campaign, "process_alive", return_value=False), patch.object(campaign, "persist"), patch.object(campaign, "bot", return_value=failure) as bot, patch.object(campaign.subprocess, "Popen") as spawn:
            self.assertFalse(campaign.launch(state, new))
            self.assertEqual(bot.call_count, 2)
            spawn.assert_not_called()
        self.assertFalse(new.get("ownership_transferred"))

    def test_readiness_must_come_from_current_live_process(self):
        with tempfile.TemporaryDirectory() as temporary, patch.object(campaign, "WORK", Path(temporary)):
            logs = Path(temporary) / "logs"
            logs.mkdir()
            path = logs / "example.log"
            path.write_text("Sandbox runtime is live\n")
            case = self.case("example", pid=123, log_offset=path.stat().st_size)
            with patch.object(campaign, "process_alive", return_value=True):
                self.assertFalse(campaign.runtime_ready(case))
                with path.open("a") as log:
                    log.write("Sandbox runtime is live\n")
                self.assertTrue(campaign.runtime_ready(case))
            with patch.object(campaign, "process_alive", return_value=False):
                self.assertFalse(campaign.runtime_ready(case))

    def test_cleanup_timeout_keeps_case_pending_for_reconciliation(self):
        case = self.case("example")
        with tempfile.TemporaryDirectory() as temporary, patch.object(campaign, "WORK", Path(temporary)), patch.object(campaign, "stop_process"), patch.object(campaign, "persist"), patch.object(campaign, "bot", side_effect=subprocess.TimeoutExpired("bot", 150)):
            (Path(temporary) / "logs").mkdir()
            campaign.cleanup_case({"cases": {"example": case}}, case)
        self.assertEqual(case["state"], "cleanup_pending")

    def test_authorization_header_is_stdin_only(self):
        secret = "TEST_SECRET_NOT_REAL"
        with patch.object(campaign, "token", return_value=secret), patch.object(campaign.time, "sleep"), patch.object(campaign.subprocess, "run", return_value=Mock(returncode=0, stdout='{"accounts":[]}\n200')) as run:
            self.assertEqual(campaign.API().call("SandboxService/GetSandboxAccounts"), {"accounts": []})
            args, kwargs = run.call_args
            self.assertNotIn(secret, str(args))
            self.assertIn(secret, kwargs["input"])

    def test_mutation_is_not_retried_after_ambiguous_network_failure(self):
        with patch.object(campaign, "token", return_value="test"), patch.object(campaign.time, "sleep"), patch.object(campaign.subprocess, "run", return_value=Mock(returncode=28, stderr="timeout")) as run:
            with self.assertRaises(RuntimeError):
                campaign.API().call("SandboxService/SandboxPayIn", mutate=True)
            self.assertEqual(run.call_count, 1)

    def test_private_state_is_written_atomically(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "state.json"
            campaign.save(path, {"phase": "prepared"})
            self.assertEqual(json.loads(path.read_text()), {"phase": "prepared"})
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            self.assertFalse(path.with_suffix(".json.tmp").exists())

    def test_restart_does_not_extend_an_observation_window_with_no_launches(self):
        yesterday = campaign.now() - dt.timedelta(days=1)
        state = {"phase": "observing", "prepared": True, "cases": {"example": self.case("example")},
                 "sessions": [{"date": yesterday.isoformat(), "startTime": yesterday.isoformat()}]}
        with tempfile.TemporaryDirectory() as temporary, patch.object(campaign, "WORK", Path(temporary)), patch.object(campaign, "load", return_value=state), patch.object(campaign, "process_alive", return_value=False), patch.object(campaign, "scheduled_sessions") as schedule, patch.object(campaign.subprocess, "Popen", return_value=Mock(pid=123)), patch.object(campaign, "power_warnings", return_value=[]), contextlib.redirect_stdout(io.StringIO()):
            campaign.start()
            schedule.assert_not_called()


if __name__ == "__main__":
    unittest.main()
