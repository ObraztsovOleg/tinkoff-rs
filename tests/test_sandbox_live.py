"""Real-account guardrails. Every broker call is mocked; no token is read or sent."""
import datetime as dt
import importlib.util
import json
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch
import uuid

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("campaign_live", ROOT / "scripts/sandbox-campaign.py")
campaign = importlib.util.module_from_spec(spec)
spec.loader.exec_module(campaign)
controls = campaign.control_plane
spec = importlib.util.spec_from_file_location("control_live", ROOT / "scripts/dashboard/control.py")
dashboard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(dashboard)
UIDS = [str(uuid.UUID(int=i + 1)) for i in range(10)]
LIVE_ACCOUNT = "2000123456"


def setUpModule():
    # Tests never read the operator's shell config or environment: a real token may be there.
    for module in (campaign, dashboard.campaign):
        patcher = patch.object(module, "fish_literal", return_value=None)
        patcher.start()
        unittest.addModuleCleanup(patcher.stop)
    environment = patch.dict(os.environ, {"TINKOFF_TOKEN_PRODUCTION": "", "TINKOFF_TOKEN_PRODUCTION_FILE": ""})
    environment.start()
    unittest.addModuleCleanup(environment.stop)


class LiveEnvironmentTests(unittest.TestCase):
    """Which token and permit each child process receives."""

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.secret = Path(directory.name) / "live"
        self.secret.write_text("fake-live-token\n")
        self.sandbox_case = {"kind": "s", "strategy_id": "s", "account_id": "acc", "config": "s.toml"}
        self.live_case = dict(self.sandbox_case, contour="production", account_id=LIVE_ACCOUNT)

    def env(self, case, halted=False, secret=True):
        variables = {"DATABASE_URL": "postgres://db", "TINKOFF_TOKEN_PRODUCTION": "", "TINKOFF_ALLOW_LIVE_TRADING": "true",
                     "TINKOFF_TOKEN_PRODUCTION_FILE": str(self.secret) if secret else ""}
        with patch.dict(os.environ, variables), patch.object(campaign, "token", return_value="fake-sandbox-token"):
            return campaign.environment({"live": {"halted": halted}}, case)

    def test_sandbox_process_never_gets_the_production_token(self):
        env = self.env(self.sandbox_case)
        self.assertEqual(env["TINKOFF_TOKEN"], "fake-sandbox-token")
        for key in ("TINKOFF_TOKEN_PRODUCTION", "TINKOFF_TOKEN_PRODUCTION_FILE", "TINKOFF_ALLOW_LIVE_TRADING"):
            self.assertFalse(key in env, key)

    def test_live_process_gets_only_the_production_token(self):
        env = self.env(self.live_case)
        self.assertEqual(env["TINKOFF_TOKEN_PRODUCTION"], "fake-live-token")
        for key in ("TINKOFF_TOKEN", "TINKOFF_TOKEN_PRODUCTION_FILE", "TINKOFF_ALLOW_LIVE_TRADING"):
            self.assertFalse(key in env, key)
        self.secret.write_text("")
        self.assertFalse("TINKOFF_TOKEN_PRODUCTION" in self.env(self.live_case))

    def test_token_sources_and_status_without_exposing_the_secret(self):
        with patch.dict(os.environ, {"TINKOFF_TOKEN_PRODUCTION_FILE": str(self.secret)}):
            status = campaign.live_status({})
            self.assertEqual(status, {"token": True, "halted": False, "ready": True})
            self.assertEqual(campaign.live_status({"live": {"halted": True}})["ready"], False)
        self.assertNotIn("fake-live-token", json.dumps(status))
        with patch.dict(os.environ, {"TINKOFF_TOKEN_PRODUCTION": "fake-env-token"}):
            self.assertEqual(campaign.live_token(), "fake-env-token")
        with patch.object(campaign, "fish_literal", return_value="fake-fish-token"):
            self.assertEqual(campaign.live_token(), "fake-fish-token")
        self.assertEqual(campaign.live_status({}), {"token": False, "halted": False, "ready": False})

    def test_production_client_is_read_only(self):
        with patch.dict(os.environ, {"TINKOFF_TOKEN_PRODUCTION_FILE": str(self.secret)}):
            api = campaign.API(production=True)
        self.assertTrue(api.base.startswith("https://invest-public-api.tbank.ru/"))
        with patch.object(campaign.subprocess, "run") as run:
            for method, mutate in [("OrdersService/PostOrder", True), ("OrdersService/PostOrder", False),
                                   ("OrdersService/CancelOrder", False), ("OperationsService/GetPortfolio", True)]:
                with self.assertRaises(RuntimeError):
                    api.call(method, {}, mutate=mutate)
            run.assert_not_called()

    def test_stand_never_liquidates_a_real_account(self):
        with self.assertRaises(RuntimeError):
            campaign.cleanup_case({}, self.live_case)


class LiveSupervisorTests(unittest.TestCase):
    def tick(self, state, stopping=False, live=None):
        row = {"strategy_id": "live", "enabled": True, "needs_service": False, "run_mode": "always", "trading_days": [], "close_after": False}
        controls_data = {"mode": "free", "max_runtimes": 1, "cases": [row], "owners": [], "pending": [], "closed": [], "live": live or {"halted": False}}
        status = dict({"token": True, "halted": False}, **(live or {}))
        status["ready"] = status["token"] and not status["halted"]
        with tempfile.TemporaryDirectory() as directory, patch.object(campaign, "WORK", Path(directory)), \
                patch.object(campaign.control_plane, "process", return_value=controls_data), \
                patch.object(campaign, "live_status", return_value=status), \
                patch.object(campaign, "process_alive", return_value=True), \
                patch.object(campaign, "launch", return_value=True) as launch, \
                patch.object(campaign, "stop_process") as stop, \
                patch.object(campaign, "cleanup_case") as cleanup, \
                patch.object(campaign, "runtime_ready", return_value=False), patch.object(campaign, "persist"):
            if stopping:
                (Path(directory) / "STOP").touch()
            campaign.supervise_tick(state, None, {"audit": float("inf"), "report": float("inf"), "calendar": float("inf")})
        return launch, stop, cleanup

    def state(self):
        return {"sessions": [], "calendar": [], "cases": {"live": {"kind": "live", "strategy_id": "live", "account_id": LIVE_ACCOUNT, "config": "configs/live.toml",
                                                                    "state": "running", "attempts": 0, "contour": "production", "pid": 7}}}

    def test_campaign_stop_stops_the_runtime_but_keeps_real_positions(self):
        state = self.state()
        _, stop, cleanup = self.tick(state, stopping=True)
        stop.assert_called_once()
        cleanup.assert_not_called()
        self.assertEqual(state["cases"]["live"]["state"], "stopped; real positions kept")
        self.assertEqual(state["phase"], "complete")

    def test_emergency_stop_or_missing_permit_stops_the_runtime(self):
        for live, expected in [({"halted": True}, "emergency stop"), ({"token": False}, "real token not connected")]:
            state = self.state()
            launch, stop, cleanup = self.tick(state, live=live)
            stop.assert_called_once()
            launch.assert_not_called()
            cleanup.assert_not_called()
            self.assertEqual(state["cases"]["live"]["state"], expected)

    def test_live_runtime_is_outside_the_sandbox_process_limit(self):
        state = self.state()
        state["max_runtimes"] = 1
        state["cases"]["sandbox"] = {"kind": "sandbox", "strategy_id": "sandbox", "account_id": "acc", "config": "configs/sandbox.toml", "pid": 99}
        live_case = dict(state["cases"]["live"], pid=None)
        with tempfile.TemporaryDirectory() as directory, patch.object(campaign, "WORK", Path(directory)), \
                patch.object(campaign, "process_alive", side_effect=lambda pid, _marker: pid == 99), \
                patch.object(campaign, "live_status", return_value={"ready": True}), \
                patch.object(campaign, "environment", return_value={}), \
                patch.object(campaign.subprocess, "Popen", return_value=SimpleNamespace(pid=5)), patch.object(campaign, "persist"):
            self.assertTrue(campaign.launch(state, live_case))
            with patch.object(campaign, "live_status", return_value={"ready": False}):
                self.assertFalse(campaign.launch(state, dict(live_case, pid=None)))


def positions(securities=0, orders=0, cash="50000"):
    return {"securities": [{"balance": "1", "blocked": "0"}] * securities, "money": [{"currency": "rub", "units": cash, "nano": 0}], "blocked": [],
            "_orders": orders}


class FakeLiveAPI:
    def __init__(self, accounts, holdings):
        self.accounts, self.holdings, self.calls = accounts, holdings, []

    def call(self, method, body=None, mutate=False):
        self.calls.append(method)
        assert not mutate and method in campaign.LIVE_READ_METHODS, method
        if method == "UsersService/GetAccounts":
            return {"accounts": self.accounts}
        if method == "OperationsService/GetPositions":
            return self.holdings[body["accountId"]]
        if method == "OrdersService/GetOrders":
            return {"orders": [{}] * self.holdings[body["accountId"]]["_orders"]}
        raise AssertionError(method)


def account(identifier, access="ACCOUNT_ACCESS_LEVEL_FULL_ACCESS", status="ACCOUNT_STATUS_OPEN", kind="ACCOUNT_TYPE_TINKOFF"):
    return {"id": identifier, "name": "robot " + identifier[-2:], "type": kind, "status": status, "accessLevel": access}


class LiveControlPlaneTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.work = Path(directory.name)
        (self.work / "configs").mkdir()
        (self.work / "configs/value.toml").write_text('contour = "sandbox"\naccount_id = "acc"\nstrategy_id = "camp-value"\napp_name = "App.test"\n'
                                                     'database_connections = 5\n\n[risk]\nbudget_rub = "1000000.00"\n\n'
                                                     + "".join(f'[[universe]]\nuid = "{uid}"\n\n' for uid in UIDS)
                                                     + '[[strategies]]\nsignal_weight = "1"\nkind = "value-composite"\n')
        self.source = {"kind": "value", "title": "Value", "strategy_id": "camp-value", "account_id": "acc", "config": "configs/value.toml", "state": "running"}
        self.state = {"id": "camp", "cases": {"value": self.source}}
        self.gate = {"strategy_id": "camp-value", "sessions": 5, "fills": 3, "risk_blocks": 0, "last_block_at": None}
        self.settings = {"halted": False, "min_sessions": 5, "min_fills": 1, "require_no_risk_block": True}
        self.executed = []
        self.api = FakeLiveAPI([account(LIVE_ACCOUNT), account("2000000002")], {LIVE_ACCOUNT: positions(), "2000000002": positions(securities=2, orders=1)})
        self.bot_calls = []

        def run_sql(_state, query):
            self.executed.append(query)
            if "FROM live_settings" in query:
                return json.dumps(self.settings)
            if query.startswith("SELECT coalesce(json_agg(t)"):
                return json.dumps([self.gate])
            return ""

        def bot(_state, case, action, *args, **kwargs):
            self.bot_calls.append((case["strategy_id"], action, args))
            return SimpleNamespace(returncode=0, stdout="", stderr="")
        self.campaign = SimpleNamespace(WORK=self.work, run_sql=Mock(side_effect=run_sql), bot=Mock(side_effect=bot), persist=Mock(),
                                        API=Mock(return_value=self.api), rub_value=campaign.rub_value, now=campaign.now)
        self.command = {"id": str(uuid.uuid4()), "kind": "live_promote", "strategy_id": None, "account_id": None, "result": {},
                        "payload": {"source_strategy_id": "camp-value", "account_id": LIVE_ACCOUNT, "kind": "live-value", "title": "Value · реальный",
                                    "budget": "30000.00", "daily_loss_percent": "2", "max_drawdown_percent": "10", "confirm": LIVE_ACCOUNT[-4:]}}

    def test_survey_accepts_only_empty_dedicated_accounts(self):
        used = {"2000000009": "other"}
        api = FakeLiveAPI([], {LIVE_ACCOUNT: positions(), "2000000002": positions(securities=2, orders=1), "2000000009": positions(), "2000000004": positions(cash="0")})
        ok = controls.survey_account(self.campaign, api, account(LIVE_ACCOUNT), used)
        self.assertTrue(ok["eligible"])
        self.assertEqual((ok["cash_rub"], ok["holdings"], ok["orders"]), (50000, 0, 0))
        busy = controls.survey_account(self.campaign, api, account("2000000002"), used)
        self.assertFalse(busy["eligible"])
        self.assertEqual(len(busy["problems"]), 2)
        for row, text in [(account("2000000009"), "уже подключён"), (account(LIVE_ACCOUNT, access="ACCOUNT_ACCESS_LEVEL_READ_ONLY"), "только для чтения"),
                          (account(LIVE_ACCOUNT, status="ACCOUNT_STATUS_CLOSED"), "не открыт"), (account(LIVE_ACCOUNT, kind="ACCOUNT_TYPE_INVEST_BOX"), "не брокерский"),
                          (account("2000000004"), "нет свободных рублей")]:
            result = controls.survey_account(self.campaign, api, row, used)
            self.assertFalse(result["eligible"])
            self.assertTrue(any(text in problem for problem in result["problems"]), result["problems"])

    def test_gate_lists_every_unmet_condition(self):
        self.assertEqual(controls.gate_failures(self.gate, self.settings), [])
        failures = controls.gate_failures({"sessions": 2, "fills": 0, "risk_blocks": 1}, self.settings)
        self.assertEqual(len(failures), 3)
        self.assertEqual(controls.gate_failures({"sessions": 5, "fills": 1, "risk_blocks": 1}, dict(self.settings, require_no_risk_block=False)), [])
        # An AI portfolio is judged by decisions of its model, not by fills: it may hold cash.
        ai = {"sessions": 5, "fills": 0, "risk_blocks": 0, "ai_decisions": 2}
        self.assertEqual(controls.gate_failures(ai, dict(self.settings, min_ai_decisions=5), ai=True), ["решений модели 2 из 5"])
        self.assertEqual(controls.gate_failures(dict(ai, ai_decisions=5), dict(self.settings, min_ai_decisions=5), ai=True), [])

    def test_account_refresh_stores_the_read_only_survey(self):
        command = {"id": str(uuid.uuid4()), "kind": "live_accounts", "payload": {}, "result": {}}
        controls.live_accounts(self.campaign, self.state, command)
        stored = next(q for q in self.executed if "SET accounts=" in q)
        self.assertIn(LIVE_ACCOUNT, stored)
        self.assertIn("succeeded", self.executed[-1])
        self.assertTrue(all(method in campaign.LIVE_READ_METHODS for method in self.api.calls))

    def test_promotion_copies_the_tested_composition_switched_off_with_confirmation(self):
        controls.live_promote(self.campaign, self.state, self.command)
        case = self.state["cases"]["live-value"]
        self.assertEqual((case["contour"], case["account_id"], case["preflight_result"], case["source"]), ("production", LIVE_ACCOUNT, "passed", "value"))
        source = (self.work / case["config"]).read_text()
        for line in ['contour = "production"', f'account_id = "{LIVE_ACCOUNT}"', 'strategy_id = "camp-live-live-value"', 'app_name = "App.test"',
                     'budget_rub = "30000.00"', 'daily_loss_percent = "2"', 'max_drawdown_percent = "10"', 'kind = "value-composite"', UIDS[9]]:
            self.assertIn(line, source)
        insert = next(q for q in self.executed if "INSERT INTO strategy_controls" in q)
        self.assertIn("false,'confirm','30000.00','always','[]',false,true,'production'", insert)
        self.assertEqual([action for _, action, _ in self.bot_calls], ["check-config", "run"])
        self.assertEqual(self.bot_calls[-1][2], ("--startup-only",))
        self.assertTrue((self.work / "logs/live-value.preflight.log").exists())
        # A retry finishes without touching the account again.
        self.bot_calls.clear()
        controls.live_promote(self.campaign, self.state, self.command)
        self.assertEqual(self.bot_calls, [])

    def test_promotion_refuses_failed_gate_busy_account_budget_or_halt(self):
        cases = [("gate", lambda: self.gate.update(sessions=1), "Порог допуска"),
                 ("account", lambda: self.command["payload"].update(account_id="2000000002"), "Счёт не подходит"),
                 ("budget", lambda: self.command["payload"].update(budget="60000.00"), "Бюджет больше"),
                 ("halt", lambda: self.settings.update(halted=True), "аварийная остановка")]
        for name, change, message in cases:
            with self.subTest(name):
                self.setUp()
                change()
                with self.assertRaises(ValueError) as error:
                    controls.live_promote(self.campaign, self.state, self.command)
                self.assertIn(message, str(error.exception))
                self.assertNotIn("live-value", self.state["cases"])
                self.assertEqual(self.bot_calls, [])

    def test_initialize_restores_a_real_row_switched_off_with_confirmation(self):
        self.state["cases"]["live"] = dict(self.source, kind="live", strategy_id="camp-live", account_id=LIVE_ACCOUNT, contour="production")
        controls.initialize(self.campaign, self.state)
        sandbox, live = [q for q in self.executed if "INSERT INTO strategy_controls" in q]
        self.assertNotIn("camp-live", sandbox)
        self.assertIn(f"('camp-live','{LIVE_ACCOUNT}',false,'production','confirm',false,'always',true)", live)

    def test_real_case_composition_and_account_are_never_rebuilt(self):
        live = dict(self.source, kind="live", strategy_id="camp-live", account_id=LIVE_ACCOUNT, contour="production")
        self.state["cases"]["live"] = live
        with self.assertRaises(ValueError):
            controls.revise(self.campaign, self.state, {"id": str(uuid.uuid4())}, live, [], lambda _id: "")
        command = {"id": str(uuid.uuid4()), "payload": {"account_id": LIVE_ACCOUNT, "kind": "other", "blocks": [{"instruments": UIDS, "share": "100", "strategies": [{"kind": "value-composite", "weight": "1"}]}]}}
        with self.assertRaises(ValueError) as error:
            controls.create_portfolio(self.campaign, self.state, command)
        self.assertIn("реальный счёт", str(error.exception))


class SleepAndThemeTests(unittest.TestCase):
    def test_sleep_intervals_merge_maintenance_wakes(self):
        state = {}
        base = dt.datetime(2026, 10, 19, 0, 0, tzinfo=dt.timezone.utc).timestamp()
        with patch.object(campaign, "supervisor_log"):
            campaign.note_suspension(state, base, base + 900)
            campaign.note_suspension(state, base + 905, base + 1800)  # five seconds awake in between
            campaign.note_suspension(state, base + 7200, base + 7500)
        self.assertEqual([(s["from"][11:16], s["to"][11:16]) for s in state["suspensions"]], [("00:00", "00:30"), ("02:00", "02:05")])

    def test_dark_stylesheet_is_generated_from_the_current_style(self):
        spec = importlib.util.spec_from_file_location("theme", ROOT / "scripts/dashboard/theme.py")
        theme = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(theme)
        self.assertEqual((ROOT / "scripts/dashboard/dark.css").read_text(), theme.build(), "run python3 scripts/dashboard/theme.py")
        # Light surfaces turn dark, dark text turns light, white text on buttons stays.
        self.assertLess(theme.colorsys.rgb_to_hls(*(v / 255 for v in theme.rgb(theme.convert("#ffffff", "background"))[0]))[1], 0.15)
        self.assertGreater(theme.colorsys.rgb_to_hls(*(v / 255 for v in theme.rgb(theme.convert("#192e34", "color"))[0]))[1], 0.75)
        self.assertEqual(theme.convert("white", "color"), "white")
        self.assertTrue(theme.scope(".button").startswith(':where([data-theme="dark"])'))


class LiveDashboardValidationTests(unittest.TestCase):
    STATE = {"cases": {"value": {"kind": "value", "strategy_id": "camp-value", "account_id": "acc"},
                       "live": {"kind": "live", "strategy_id": "camp-live", "account_id": LIVE_ACCOUNT, "contour": "production"}}}

    def command(self, kind, payload=None, strategy="camp-live", account=LIVE_ACCOUNT):
        return {"id": str(uuid.uuid4()), "kind": kind, "strategy_id": strategy, "account_id": account, "payload": payload or {}}

    def test_real_case_forbids_sandbox_only_actions(self):
        for kind, payload in [("pay_in", {"amount": "100"}), ("manual_order", {"instrument_id": UIDS[0], "side": "buy", "lots": 1, "order_type": "market"}),
                              ("universe", {"instruments": [UIDS[0]]}), ("execution_mode", {"mode": "auto"}),
                              ("schedule", {"run_mode": "always", "days": [], "close_after": True}), ("resume", {}), ("cleanup", {"confirm": "0000"})]:
            with self.subTest(kind), self.assertRaises(dashboard.ClientError):
                dashboard.validate(self.command(kind, payload), self.STATE)
        for kind, payload in [("resume", {"confirm": LIVE_ACCOUNT[-4:]}), ("cleanup", {"confirm": LIVE_ACCOUNT[-4:]}), ("pause", {}),
                              ("execution_mode", {"mode": "confirm"}), ("budget", {"amount": "1000"}),
                              ("schedule", {"run_mode": "always", "days": [], "close_after": False})]:
            with self.subTest(kind):
                dashboard.validate(self.command(kind, payload), self.STATE)
        # Sandbox cases keep their tools and need no confirmation code.
        dashboard.validate(self.command("pay_in", {"amount": "100"}, "camp-value", "acc"), self.STATE)
        dashboard.validate(self.command("resume", {}, "camp-value", "acc"), self.STATE)

    def test_promotion_requires_typed_confirmation_and_consistent_stops(self):
        payload = {"source_strategy_id": "camp-value", "account_id": "2000999999", "kind": "live-value", "title": "Value",
                   "budget": "30000", "daily_loss_percent": "2", "max_drawdown_percent": "10", "confirm": "9999"}
        validated = dashboard.validate(self.command("live_promote", dict(payload), None, None), self.STATE)
        self.assertEqual(validated["payload"]["budget"], "30000")
        for change in [{"confirm": "1234"}, {"daily_loss_percent": "20"}, {"max_drawdown_percent": "150"}, {"budget": "1.001"},
                       {"source_strategy_id": "camp-live"}, {"account_id": LIVE_ACCOUNT, "confirm": LIVE_ACCOUNT[-4:]}, {"kind": "Bad Name"}]:
            with self.subTest(change), self.assertRaises(dashboard.ClientError):
                dashboard.validate(self.command("live_promote", dict(payload, **change), None, None), self.STATE)
        with self.assertRaises(dashboard.ClientError):
            dashboard.validate(self.command("live_promote", payload), self.STATE)

    def test_gate_and_halt_settings_are_bounded(self):
        dashboard.validate(self.command("live_gate", {"min_sessions": 5, "min_fills": 0, "require_no_risk_block": True}, None, None), self.STATE)
        for payload in [{"min_sessions": 0, "min_fills": 1, "require_no_risk_block": True}, {"min_sessions": 5, "min_fills": -1, "require_no_risk_block": True},
                        {"min_sessions": 5, "min_fills": 1, "require_no_risk_block": "yes"}]:
            with self.assertRaises(dashboard.ClientError):
                dashboard.validate(self.command("live_gate", payload, None, None), self.STATE)
        self.assertEqual(dashboard.validate(self.command("live_halt", {}, None, None), self.STATE)["payload"]["reason"], "Аварийная остановка из панели")

    def test_halt_disables_every_real_strategy_in_one_transaction(self):
        service = dashboard.Control(Path("unused"))
        statements = []

        def sql(query):
            statements.append(query)
            if query.startswith("SELECT coalesce((SELECT row_to_json(c) FROM control_commands"):
                return {}
            return {"id": "x", "status": "succeeded"}
        service.sql = sql
        service.state = Mock(return_value=self.STATE)
        service.enqueue(self.command("live_halt", {"reason": "test"}, None, None))
        transaction = statements[-1]
        self.assertIn("SET halted=true", transaction)
        self.assertIn("UPDATE strategy_controls SET enabled=false", transaction)
        self.assertIn("contour='production'", transaction)
        self.assertTrue(transaction.startswith("BEGIN;") and transaction.rstrip().endswith("COMMIT;"))

    def test_gate_list_puts_ready_and_running_portfolios_first(self):
        service = dashboard.Control(Path("unused"))
        settings = {"halted": False, "halted_at": None, "halt_reason": None, "min_sessions": 5, "min_fills": 1, "min_ai_decisions": 5, "require_no_risk_block": True,
                    "accounts": [], "accounts_observed_at": None, "accounts_error": None}
        rows = [{"strategy_id": sid, "enabled": enabled, "sessions": sessions, "fills": fills, "risk_blocks": blocks}
                for sid, enabled, sessions, fills, blocks in [("old", False, 9, 9, 0), ("near", True, 4, 1, 0), ("ready", True, 6, 2, 0),
                                                              ("blocked", True, 9, 9, 1), ("unknown", True, 9, 9, 0)]]
        service.sql = Mock(side_effect=[settings, rows])
        state = {"cases": {sid: {"kind": sid, "strategy_id": sid} for sid in ["old", "near", "ready", "blocked"]}}
        result = service.live_data(state)
        self.assertEqual([r["kind"] for r in result["gate"]], ["ready", "old", "blocked", "near"])
        self.assertEqual(result["status"]["ready"], False)

    def test_resume_of_a_real_case_needs_a_ready_live_contour(self):
        service = dashboard.Control(Path("unused"))
        service.state = Mock(return_value=self.STATE)
        service.sql = Mock(return_value={"halted": True})
        with self.assertRaises(dashboard.ClientError) as error:
            service.enqueue(self.command("resume", {"confirm": LIVE_ACCOUNT[-4:]}))
        self.assertIn("аварийная остановка", str(error.exception))


if __name__ == "__main__":
    unittest.main()
