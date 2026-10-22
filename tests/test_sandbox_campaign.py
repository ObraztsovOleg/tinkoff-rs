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


class TradingWindowTests(unittest.TestCase):
    def day(self, date, trading=True):
        return {"date": date, "isTradingDay": trading, "startTime": date + "T06:50:00Z" if trading else None,
                "endTime": date + "T15:39:59Z" if trading else None}

    def test_consecutive_chosen_days_form_one_window_with_warmup_and_grace(self):
        days = [self.day("2026-10-19"), self.day("2026-10-20"), self.day("2026-10-21"), self.day("2026-10-22"),
                self.day("2026-10-23"), self.day("2026-10-24", trading=False), self.day("2026-10-26")]
        windows = campaign.trading_windows(days, ["2026-10-19", "2026-10-20", "2026-10-22", "2026-10-24", "2026-10-26", "2026-11-30"])
        self.assertEqual([w["days"] for w in windows], [["2026-10-19", "2026-10-20"], ["2026-10-22"], ["2026-10-26"]])
        self.assertEqual(windows[0]["begin"], "2026-10-19T05:50:00+00:00")
        self.assertEqual(windows[0]["open"], "2026-10-19T06:50:00Z")
        self.assertEqual(windows[0]["end"], "2026-10-20T15:41:59+00:00")
        # A weekend between chosen trading days does not split the window.
        self.assertEqual([w["days"] for w in campaign.trading_windows(days, ["2026-10-23", "2026-10-26"])], [["2026-10-23", "2026-10-26"]])
        self.assertEqual(campaign.trading_windows(days, []), [])

    def test_calendar_refresh_starts_now_and_keeps_recent_known_days(self):
        current = campaign.now()
        today = current.date().isoformat()
        old, recent = (current - dt.timedelta(days=30)).date().isoformat(), (current - dt.timedelta(days=2)).date().isoformat()
        api = Mock()
        api.call.return_value = {"exchanges": [{"days": [{"date": today + "T00:00:00Z", "isTradingDay": True,
                                                          "startTime": today + "T06:50:00Z", "endTime": today + "T15:39:59Z"}]}]}
        state = {"calendar": [self.day(old), self.day(recent)]}
        campaign.refresh_calendar(state, api)
        starts = [call.args[1]["from"] for call in api.call.call_args_list]
        self.assertTrue(all(campaign.timestamp(value) >= current for value in starts), starts)
        self.assertEqual([d["date"] for d in state["calendar"]], [recent, today])

    def test_calendar_prefers_refreshed_days_over_prepared_sessions(self):
        state = {"sessions": [{"date": "2026-10-19T00:00:00Z", "startTime": "a", "endTime": "b"}],
                 "calendar": [{"date": "2026-10-19", "isTradingDay": True, "startTime": "c", "endTime": "d"}, self.day("2026-10-20")]}
        known = campaign.calendar(state)
        self.assertEqual([d["date"] for d in known], ["2026-10-19", "2026-10-20"])
        self.assertEqual(known[0]["startTime"], "c")



class SupervisorScheduleTests(unittest.TestCase):
    """supervise_tick follows the operator schedule; every broker-side action is mocked."""

    def tick(self, controls, state, alive=False):
        def cleaned(_state, case):
            case["state"] = "cleaned"
        with patch.object(campaign.control_plane, "process", return_value=controls), \
                patch.object(campaign, "process_alive", return_value=alive), \
                patch.object(campaign, "launch", return_value=True) as launch, \
                patch.object(campaign, "stop_process") as stop, \
                patch.object(campaign, "cleanup_case", side_effect=cleaned) as cleanup, \
                patch.object(campaign, "runtime_ready", return_value=False), patch.object(campaign, "persist"):
            campaign.supervise_tick(state, None, {"audit": float("inf"), "report": float("inf"), "calendar": float("inf")})
        return launch, stop, cleanup

    def setup(self, start, end, **control):
        day = start.date().isoformat()
        state = {"sessions": [], "calendar": [{"date": day, "isTradingDay": True, "startTime": start.isoformat(), "endTime": end.isoformat()}],
                 "cases": {"a": {"kind": "a", "strategy_id": "a", "account_id": "acc", "config": "configs/a.toml", "state": "prepared", "attempts": 0, "cohort": 0}}}
        row = dict({"strategy_id": "a", "enabled": True, "needs_service": False, "run_mode": "schedule", "trading_days": [day], "close_after": True}, **control)
        return state, {"mode": "campaign", "max_runtimes": 4, "cases": [row], "owners": [], "pending": [], "closed": []}

    def test_trades_inside_the_window_and_stops_outside_it(self):
        current = campaign.now()
        state, controls = self.setup(current - dt.timedelta(minutes=30), current + dt.timedelta(hours=3))
        launch, _, _ = self.tick(controls, state)
        self.assertEqual(launch.call_args.kwargs, {"service_only": False})
        self.assertTrue(state["cases"]["a"]["schedule"]["active"])
        state, controls = self.setup(current + dt.timedelta(hours=5), current + dt.timedelta(hours=9))
        state["cases"]["a"].update(pid=123, state="running")
        launch, stop, _ = self.tick(controls, state, alive=True)
        launch.assert_not_called()
        stop.assert_called_once()
        self.assertEqual(state["cases"]["a"]["state"], "waiting for window")

    def test_finished_window_closes_positions_once_when_requested(self):
        current = campaign.now()
        state, controls = self.setup(current - dt.timedelta(hours=8), current - dt.timedelta(hours=1))
        _, _, cleanup = self.tick(controls, state)
        cleanup.assert_called_once()
        self.assertEqual(state["cases"]["a"]["closed_window"], state["cases"]["a"]["schedule"]["windows"][0]["end"])
        _, _, cleanup = self.tick(controls, state)
        cleanup.assert_not_called()
        state, controls = self.setup(current - dt.timedelta(hours=8), current - dt.timedelta(hours=1), close_after=False)
        launch, _, cleanup = self.tick(controls, state)
        cleanup.assert_not_called()
        launch.assert_not_called()

    def test_always_mode_trades_without_days_and_respects_runtime_limit(self):
        current = campaign.now()
        state, controls = self.setup(current + dt.timedelta(days=3), current + dt.timedelta(days=3, hours=8), run_mode="always", trading_days=[])
        launch, _, _ = self.tick(controls, state)
        self.assertEqual(launch.call_args.kwargs, {"service_only": False})
        self.assertEqual(state["cases"]["a"]["schedule"]["windows"], [])
        # launch() counts live runtimes against the operator limit.
        state["max_runtimes"] = 1
        state["cases"]["b"] = {"kind": "b", "strategy_id": "b", "account_id": "other", "config": "configs/b.toml", "pid": 99}
        with patch.object(campaign, "process_alive", side_effect=lambda pid, _marker: pid == 99):
            self.assertFalse(campaign.launch(state, state["cases"]["a"]))
        self.assertEqual(state["cases"]["a"]["state"], "waiting for runtime slot")


class UpkeepTests(unittest.TestCase):
    """Nightly maintenance, log rotation and the disk guard; no database is touched."""

    def test_maintenance_runs_once_a_day_after_0330_moscow_and_retries_hourly(self):
        early = dt.datetime(2026, 10, 20, 0, 10, tzinfo=dt.timezone.utc)
        due = dt.datetime(2026, 10, 20, 0, 31, tzinfo=dt.timezone.utc)
        self.assertFalse(campaign.maintenance_due({}, early), "not before 03:30 MSK")
        self.assertTrue(campaign.maintenance_due({}, due))
        self.assertFalse(campaign.maintenance_due({"maintenance": {"day": "2026-10-20"}}, due), "once a day")
        failed = {"maintenance": {"day": "2026-10-19", "retry_at": (due + dt.timedelta(hours=1)).isoformat()}}
        self.assertFalse(campaign.maintenance_due(failed, due + dt.timedelta(minutes=30)))
        self.assertTrue(campaign.maintenance_due(failed, due + dt.timedelta(hours=1, minutes=1)))
        with tempfile.TemporaryDirectory() as temporary, patch.object(campaign, "WORK", Path(temporary)), \
                patch.object(campaign, "run_sql", side_effect=RuntimeError("database unavailable")):
            state = {"cases": {}}
            campaign.maintenance(state)
        self.assertIn("database unavailable", state["maintenance"]["error"])
        self.assertIn("retry_at", state["maintenance"])

    def test_long_logs_keep_their_newest_lines_and_writers_continue(self):
        with tempfile.TemporaryDirectory() as temporary, patch.object(campaign, "WORK", Path(temporary)), \
                patch.object(campaign, "LOG_LIMIT_BYTES", 1000), patch.object(campaign, "LOG_KEEP_BYTES", 300):
            logs = Path(temporary) / "logs"
            logs.mkdir()
            path = logs / "trend.log"
            path.write_text("".join(f"line {i:04d}\n" for i in range(200)))
            (logs / "small.log").write_text("short\n")
            with path.open("a") as writer:
                self.assertEqual(campaign.rotate_logs(), ["trend.log"])
                writer.write("after rotation\n")
            kept = (logs / "trend.log.1").read_text()
            self.assertTrue(kept.startswith("line ") and kept.endswith("line 0199\n") and len(kept) <= 300)
            self.assertEqual(path.read_text(), "after rotation\n", "an append-mode writer continues at the start")
            self.assertEqual((logs / "small.log").read_text(), "short\n")

    def test_disk_guard_marks_critical_space_and_reports_the_tightest_disk(self):
        usage = {"/mac": (500 * 1024 ** 3, 800 * 1024 ** 2), "/vm": (100 * 1024 ** 3, 14 * 1024 ** 3)}
        disk_usage = lambda path: Mock(total=usage[str(path)][0], free=usage[str(path)][1])
        with tempfile.TemporaryDirectory() as temporary, patch.object(campaign, "WORK", Path(temporary)), \
                patch.dict(campaign.os.environ, {"DISK_WATCH_PATHS": "/mac:/vm"}), patch.object(campaign.shutil, "disk_usage", side_effect=disk_usage), \
                patch.object(campaign, "run_sql", return_value="23068672"):
            state = {}
            campaign.disk_status(state)
            self.assertEqual((state["disk"]["level"], state["disk"]["path"], state["disk"]["database_bytes"]), ("critical", "/mac", 23068672))
            self.assertTrue((Path(temporary) / "DISK_LOW").exists(), "collection and backups stop")
            usage["/mac"] = (500 * 1024 ** 3, 2 * 1024 ** 3)
            campaign.disk_status(state)
            self.assertEqual(state["disk"]["level"], "warn")
            self.assertFalse((Path(temporary) / "DISK_LOW").exists())
            usage["/mac"] = (500 * 1024 ** 3, 20 * 1024 ** 3)
            campaign.disk_status(state)
            self.assertEqual((state["disk"]["level"], state["disk"]["path"]), ("ok", "/vm"))


class CollectorTests(unittest.TestCase):
    """The document collector runs only while an AI portfolio is on and the disk has room."""

    def test_collector_starts_restarts_after_a_pause_and_stops_on_low_disk(self):
        alive = {"value": False}
        with tempfile.TemporaryDirectory() as temporary, patch.object(campaign, "WORK", Path(temporary)), \
                patch.object(campaign, "process_alive", side_effect=lambda pid, marker: alive["value"] and pid == 77), \
                patch.object(campaign.subprocess, "Popen", return_value=Mock(pid=77)) as spawn, \
                patch.object(campaign, "environment", return_value={}), patch.object(campaign.os, "kill") as kill:
            state = {}
            campaign.supervise_collector(state, desired=False, stopping=False)
            spawn.assert_not_called()
            campaign.supervise_collector(state, desired=True, stopping=False)
            self.assertEqual(spawn.call_args.args[0][1:], ["collect"])
            alive["value"] = True
            campaign.supervise_collector(state, desired=True, stopping=False)
            self.assertEqual(state["collector"]["state"], "running")
            alive["value"] = False
            campaign.supervise_collector(state, desired=True, stopping=False)
            self.assertEqual((state["collector"]["state"], state["collector"]["restarts"]), ("restart backoff", 1))
            campaign.supervise_collector(state, desired=True, stopping=False)
            self.assertEqual(spawn.call_count, 1, "no restart within a minute")
            state["collector"]["exited_at"] = (campaign.now() - dt.timedelta(minutes=2)).isoformat()
            campaign.supervise_collector(state, desired=True, stopping=False)
            self.assertEqual(spawn.call_count, 2)
            alive["value"] = True
            state["disk"] = {"level": "critical"}
            def gone(*_):
                alive["value"] = False
            kill.side_effect = gone
            campaign.supervise_collector(state, desired=True, stopping=False)
            kill.assert_called_once()
            self.assertEqual(state["collector"]["state"], "disk space low")


if __name__ == "__main__":
    unittest.main()
