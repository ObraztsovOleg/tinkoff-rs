"""Campaign supervisor resilience; no broker requests, tokens or Docker required."""
import datetime as dt
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

spec = importlib.util.spec_from_file_location("campaign", Path(__file__).resolve().parents[1] / "scripts/sandbox-campaign.py")
campaign = importlib.util.module_from_spec(spec)
spec.loader.exec_module(campaign)

NOW = dt.datetime(2026, 10, 19, 7, 0, tzinfo=dt.timezone.utc)


def case(name="example", **values):
    base = dict(kind=name, strategy_id=name, account_id="account-" + name, config=name + ".toml",
                state="prepared", attempts=0, cohort=0)
    base.update(values)
    return base


class FakeApi:
    """Records calls; answers from a {method: response or callable} table."""

    def __init__(self, answers):
        self.answers = answers
        self.calls = []

    def call(self, method, body=None, *, mutate=False):
        self.calls.append((method, body, mutate))
        answer = self.answers[method]
        return answer(body) if callable(answer) else answer


class RestartBudgetTests(unittest.TestCase):
    def test_first_crashes_restart_immediately(self):
        for attempts in range(campaign.RESTART_BURST):
            self.assertIsNone(campaign.restart_not_before(case(attempts=attempts, last_started=NOW.isoformat())))

    def test_repeated_crashes_back_off_but_never_give_up(self):
        delays = []
        for attempts in range(campaign.RESTART_BURST, campaign.RESTART_BURST + 8):
            retry_at = campaign.restart_not_before(case(attempts=attempts, last_started=NOW.isoformat()))
            delays.append(retry_at - NOW)
        self.assertEqual(delays[0], dt.timedelta(minutes=1))
        self.assertEqual(delays[1], dt.timedelta(minutes=2))
        self.assertEqual(sorted(delays), delays)
        self.assertEqual(delays[-1], campaign.RESTART_MAX_DELAY)

    def test_healthy_runtime_refills_the_budget(self):
        recent = case(attempts=2, last_started=(NOW - dt.timedelta(minutes=5)).isoformat())
        campaign.refill_restart_budget(recent, NOW)
        self.assertEqual(recent["attempts"], 2)
        healthy = case(attempts=2, last_started=(NOW - campaign.HEALTHY_RUNTIME).isoformat())
        campaign.refill_restart_budget(healthy, NOW)
        self.assertEqual(healthy["attempts"], 0)

    def test_planned_restart_does_not_spend_the_crash_budget(self):
        item = case(attempts=1)
        state = {"cases": {"example": item}}
        with tempfile.TemporaryDirectory() as temporary, patch.object(campaign, "WORK", Path(temporary)), \
                patch.object(campaign, "process_alive", return_value=False), patch.object(campaign, "persist"), \
                patch.object(campaign, "environment", return_value={}), \
                patch.object(campaign.subprocess, "Popen", return_value=Mock(pid=4242)):
            self.assertTrue(campaign.launch(state, item, planned=True))
            self.assertEqual(item["attempts"], 1)
            self.assertNotIn("launches", item)
            self.assertTrue(campaign.launch(state, item))
            self.assertEqual(item["attempts"], 2)
            self.assertEqual(item["launches"], 1)


class SupervisorTests(unittest.TestCase):
    def test_supervisor_survives_a_failed_pass(self):
        state = {"id": "test", "prepared": True, "phase": "observing", "cases": {}}
        with tempfile.TemporaryDirectory() as temporary, patch.object(campaign, "WORK", Path(temporary)):
            work = Path(temporary)
            campaign.save(work / "campaign.json", state)
            calls = []

            def tick(*_):
                calls.append(1)
                if len(calls) == 1:
                    raise RuntimeError("docker: Cannot connect to the Docker daemon")
                (work / "PARK").touch()

            with patch.object(campaign, "API"), patch.object(campaign.control_plane, "initialize"), \
                    patch.object(campaign, "supervise_tick", side_effect=tick), patch.object(campaign, "report"), \
                    patch.object(campaign.time, "sleep"), patch.object(campaign.sys, "platform", "linux"), \
                    patch.object(campaign.signal, "signal"):
                campaign.supervise()
            self.assertEqual(len(calls), 2)
            self.assertIn("supervisor pass failed; retrying", (work / "supervisor.log").read_text())
            self.assertEqual(campaign.load()["phase"], "parked")

    def test_broker_audit_stops_early_without_network(self):
        state = {"created_at": NOW.isoformat(), "cases": {name: case(name) for name in ["a", "b", "c"]}}

        def offline(_body):
            raise RuntimeError("SandboxService/GetSandboxOperationsByCursor: curl 6: Could not resolve host")

        api = FakeApi({"SandboxService/GetSandboxOperationsByCursor": offline})
        with tempfile.TemporaryDirectory() as temporary, patch.object(campaign, "WORK", Path(temporary)):
            campaign.audit_accounts(state, api)
            audit = json.loads((Path(temporary) / "reports" / "broker-latest.json").read_text())
        self.assertEqual(len(api.calls), 1)
        self.assertIn("curl", audit["accounts"]["a"]["error"])
        self.assertEqual(audit["accounts"]["c"]["error"], "deferred: broker API unreachable")


class AccountAdoptionTests(unittest.TestCase):
    @staticmethod
    def positions(busy):
        return lambda body: {"money": [], "blocked": [], "futures": [], "options": [],
                             "securities": [{"balance": "10" if body["accountId"] in busy else "0", "blocked": "0"}]}

    def test_adopts_only_open_empty_unused_accounts(self):
        accounts = [
            {"id": "closed", "status": "ACCOUNT_STATUS_CLOSED", "openedDate": "2026-01-01"},
            {"id": "busy", "status": "ACCOUNT_STATUS_OPEN", "openedDate": "2026-01-02"},
            {"id": "used", "status": "ACCOUNT_STATUS_OPEN", "openedDate": "2026-01-03"},
            {"id": "orders", "status": "ACCOUNT_STATUS_OPEN", "openedDate": "2026-01-04"},
            {"id": "idle", "status": "ACCOUNT_STATUS_OPEN", "openedDate": "2026-01-05", "name": "old-campaign"},
        ]
        api = FakeApi({
            "SandboxService/GetSandboxPositions": self.positions({"busy"}),
            "SandboxService/GetSandboxOrders": lambda body: {"orders": [{"orderId": "x"}] if body["accountId"] == "orders" else []},
        })
        state = {"cases": {"x": case("x", account_id="used")}}
        self.assertEqual(campaign.adopt_idle_account(state, api, accounts), "idle")
        self.assertEqual(state["adopted_accounts"]["idle"]["previous_name"], "old-campaign")
        self.assertFalse(any(mutate for _, _, mutate in api.calls), "adoption must never change an account")

    def test_no_idle_account_returns_none(self):
        api = FakeApi({"SandboxService/GetSandboxPositions": self.positions({"a"}),
                       "SandboxService/GetSandboxOrders": {"orders": []}})
        state = {"cases": {}}
        self.assertIsNone(campaign.adopt_idle_account(state, api, [{"id": "a", "status": "ACCOUNT_STATUS_OPEN"}]))
        self.assertNotIn("adopted_accounts", state)

    def funding(self, balance, **extra):
        item = case("adopted", account_id="acc", **extra)
        state = {"cases": {"adopted": item}, "adopted_accounts": {"acc": {}}}
        api = FakeApi({"SandboxService/GetSandboxPortfolio": {"totalAmountPortfolio": balance},
                       "SandboxService/SandboxPayIn": {}})
        ok = Mock(returncode=0, stdout="", stderr="")
        with patch.object(campaign, "persist"), patch.object(campaign, "bot", return_value=ok):
            campaign.initialize_accounts(state, api)
        return item, [call for call in api.calls if call[0] == "SandboxService/SandboxPayIn"]

    def test_adopted_account_with_full_budget_is_not_topped_up(self):
        item, payins = self.funding({"units": "1000000", "nano": 0})
        self.assertTrue(item["funded"])
        self.assertEqual(payins, [])

    def test_adopted_account_receives_only_the_shortfall_once(self):
        item, payins = self.funding({"units": "999000", "nano": 500000000})
        self.assertEqual(len(payins), 1)
        self.assertEqual(payins[0][1]["amount"]["units"], "1000")
        self.assertTrue(payins[0][2], "pay-in is a mutation")
        self.assertEqual(item["topup_requested"], 1000)

    def test_uncertain_top_up_is_never_repeated(self):
        with self.assertRaisesRegex(RuntimeError, "top-up is uncertain"):
            self.funding({"units": "999000", "nano": 0}, topup_requested=1000)


class ArchiveTests(unittest.TestCase):
    def prepare(self, temporary, phase):
        work = Path(temporary) / "sandbox-campaign"
        (work / "logs").mkdir(parents=True)
        (work / "logs" / "keep.log").write_text("history\n")
        campaign.save(work / "campaign.json", {"id": "sandbox-old", "prepared": True, "phase": phase,
                                               "container": "tinkoff-rs-sandbox-old", "cases": {"a": case("a")}})
        return work

    def test_finished_campaign_is_moved_not_deleted(self):
        with tempfile.TemporaryDirectory() as temporary:
            work = self.prepare(temporary, "complete")
            with patch.object(campaign, "WORK", work), patch.object(campaign, "process_alive", return_value=False):
                campaign.archive()
            destination = Path(temporary) / "sandbox-campaign-archive" / "sandbox-old"
            self.assertFalse(work.exists())
            self.assertEqual((destination / "logs" / "keep.log").read_text(), "history\n")
            self.assertEqual(json.loads((destination / "campaign.json").read_text())["id"], "sandbox-old")

    def test_running_or_unfinished_campaign_is_not_archived(self):
        with tempfile.TemporaryDirectory() as temporary:
            work = self.prepare(temporary, "observing")
            with patch.object(campaign, "WORK", work), patch.object(campaign, "process_alive", return_value=False):
                with self.assertRaisesRegex(RuntimeError, "phase is observing"):
                    campaign.archive()
            self.assertTrue((work / "campaign.json").exists())
        with tempfile.TemporaryDirectory() as temporary:
            work = self.prepare(temporary, "complete")
            with patch.object(campaign, "WORK", work), patch.object(campaign, "process_alive", return_value=True):
                with self.assertRaisesRegex(RuntimeError, "Supervisor is running"):
                    campaign.archive()
            self.assertTrue((work / "campaign.json").exists())


class PowerTests(unittest.TestCase):
    def test_battery_power_is_reported_on_macos(self):
        battery = Mock(stdout="Now drawing from 'Battery Power'\n")
        with patch.object(campaign.sys, "platform", "darwin"), patch.object(campaign.subprocess, "run", return_value=battery):
            self.assertEqual(len(campaign.power_warnings()), 1)
        mains = Mock(stdout="Now drawing from 'AC Power'\n")
        with patch.object(campaign.sys, "platform", "darwin"), patch.object(campaign.subprocess, "run", return_value=mains):
            self.assertEqual(campaign.power_warnings(), [])
        with patch.object(campaign.sys, "platform", "linux"):
            self.assertEqual(campaign.power_warnings(), [])


if __name__ == "__main__":
    unittest.main()
