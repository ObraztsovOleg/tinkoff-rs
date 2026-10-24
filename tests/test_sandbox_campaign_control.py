"""Supervisor transition tests with no broker writes or running campaigns."""
import copy
import importlib.util
import json
from pathlib import Path
import subprocess
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
        if "FROM account_risk_state" in query:
            return "t"  # the revised strategy's account already has a risk owner
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


ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/debug/tinkoff-bot"


def block(instruments, share, *kinds):
    return {"instruments": list(instruments), "share": share, "strategies": [{"kind": kind, "weight": "1"} for kind in kinds]}


class PortfolioTests(unittest.TestCase):
    """Operator portfolios: combinations share instruments, blocks never overlap."""

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.work = Path(temporary.name)
        (self.work / "configs").mkdir()
        self.ids = [str(uuid.UUID(int=i + 1)) for i in range(12)]
        (self.work / "configs/base.toml").write_text(
            'contour = "sandbox"\naccount_id = "account"\nstrategy_id = "old"\napp_name = "test.app"\ndatabase_connections = 5\n\n'
            '[risk]\nbudget_rub = "1000000.00"\ndaily_loss_percent = "5.0"\nmax_drawdown_percent = "25.0"\n\n'
            '[[universe]]\nuid = "' + self.ids[0] + '"\n\n[[strategies]]\nkind = "moving-average-trend"\n')
        self.case = {"kind": "base", "strategy_id": "old", "account_id": "account", "config": "configs/base.toml"}
        self.state = {"benchmark": {"uid": self.ids[11], "ticker": "BENCH"}, "cases": {"base": self.case}}
        self.campaign = SimpleNamespace(WORK=self.work, ROOT=ROOT)

    def test_blocks_must_be_disjoint_combinations_with_full_shares(self):
        a, b, c = self.ids[:3]
        controls.validate_blocks([block([a, b], "60", "pairs-trading", "equal-risk-contribution"), block([c], "40", "moving-average-trend")])
        cases = [
            ([block([a, b], "50", "pairs-trading"), block([b, a], "50", "equal-risk-contribution")], "объедините"),
            ([block([a, b], "50", "pairs-trading"), block([b, c], "50", "equal-risk-contribution")], "пересекаются"),
            ([block([a], "60", "moving-average-trend"), block([b], "30", "range-breakout")], "100%"),
            ([block([a], "100", "pairs-trading")], "не меньше 2"),
            ([block([a, b], "100", "market-intraday-momentum")], "ровно 1"),
            ([block([a], "100", "moving-average-trend", "moving-average-trend")], "дважды"),
            ([block([a], "100", "weighted-ensemble")], "неизвестная"),
            ([], "от 1 до 8"),
        ]
        for blocks, message in cases:
            with self.assertRaisesRegex(ValueError, message):
                controls.validate_blocks(blocks)
        ranked = self.ids[:10]
        with self.assertRaisesRegex(ValueError, "ориентир"):
            controls.validate_blocks([block(ranked[:9] + [self.ids[11]], "100", "residual-momentum")], self.ids[11])

    def test_generated_portfolio_splits_capital_and_passes_bot_check(self):
        a, b, c = self.ids[:3]
        blocks = [block([a, b], "60", "pairs-trading", "equal-risk-contribution"), block([c], "40", "moving-average-trend", "market-intraday-momentum")]
        source = controls.portfolio_source(self.campaign, self.state, self.case, "new-portfolio", blocks)
        self.assertIn('strategy_id = "new-portfolio"', source)
        self.assertEqual(source.count("[[universe]]"), 3)
        self.assertEqual(source.count("[[strategies]]"), 4)
        self.assertIn('signal_weight = "0.3"', source)  # 60% split between two strategies
        self.assertIn('signal_weight = "0.2"', source)
        self.assertIn(f'instruments = ["{c}"]', source)
        if not BINARY.exists():
            self.skipTest("build target/debug/tinkoff-bot to check the TOML with the bot")
        path = self.work / "portfolio.toml"
        path.write_text(source)
        result = subprocess.run([str(BINARY), "--config", str(path), "check-config"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("components=4", result.stdout)
        # The bot itself refuses overlapping blocks even if the dashboard check is bypassed.
        overlap = [block([a, b], "50", "pairs-trading"), block([b, c], "50", "equal-risk-contribution")]
        path.write_text(controls.portfolio_source(self.campaign, self.state, self.case, "overlap", overlap))
        result = subprocess.run([str(BINARY), "--config", str(path), "check-config"], capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("share instruments", result.stdout + result.stderr)

    def test_campaign_roster_converts_once_into_editable_trading_days(self):
        sessions = [{"date": f"2026-10-{day}T00:00:00Z"} for day in (19, 20, 21, 22, 23)]
        state = {"sessions": sessions, "cases": {"a": {"strategy_id": "a", "cohort": 0}, "b": {"strategy_id": "b", "cohort": 2}, "c": {"strategy_id": "c", "cohort": 3}}}
        executed = []
        campaign = SimpleNamespace(run_sql=lambda _, query: executed.append(query) or "")
        rows = {"cases": [{"strategy_id": "a", "schedule_initialized": False}, {"strategy_id": "b", "schedule_initialized": False}, {"strategy_id": "c", "schedule_initialized": True}]}
        self.assertTrue(controls.convert_schedules(campaign, state, rows))
        self.assertIn('\'["2026-10-19", "2026-10-20"]\'::jsonb', executed[0])
        self.assertIn('\'["2026-10-22"]\'::jsonb', executed[0])
        self.assertNotIn("strategy_id='c'", executed[0])
        self.assertIn("WHEN (SELECT mode FROM dashboard_settings WHERE id)='free' THEN 'always'", executed[0])
        self.assertFalse(controls.convert_schedules(campaign, state, {"cases": [{"strategy_id": "a", "schedule_initialized": True}]}))


class CreatePortfolioTests(unittest.TestCase):
    """A new case on an account: cleanup of the old owner, risk handoff, warmup."""

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.work = Path(temporary.name)
        (self.work / "configs").mkdir()
        self.ids = [str(uuid.UUID(int=i + 1)) for i in range(10)]
        (self.work / "configs/old.toml").write_text('contour = "sandbox"\naccount_id = "acc"\nstrategy_id = "camp-old"\n\n[risk]\nbudget_rub = "1000000.00"\n\n[[universe]]\nuid = "' + self.ids[0] + '"\n\n[[strategies]]\nkind = "moving-average-trend"\n')
        self.old = {"kind": "old", "strategy_id": "camp-old", "account_id": "acc", "config": "configs/old.toml", "state": "paused"}
        self.state = {"id": "camp", "instruments": [{"uid": uid, "ticker": f"T{i}"} for i, uid in enumerate(self.ids)], "cases": {"old": self.old}}
        self.enabled, self.executed, self.owner = [], [], "camp-old"

        def run_sql(_state, query):
            self.executed.append(query)
            if "json_agg(strategy_id)" in query:
                return json.dumps(self.enabled)
            if query.startswith("SELECT EXISTS"):
                return "f"
            if "FROM account_risk_state" in query:
                return self.owner
            if "json_agg(json_build_object('uid'" in query:
                return "[]"
            return ""

        def cleanup(_state, case):
            case["state"] = "cleaned"
        self.bot_calls = []

        def bot(_state, case, action, *args, **kwargs):
            self.bot_calls.append((case["strategy_id"], action))
            return SimpleNamespace(returncode=1 if action == "risk-status" else 0, stdout="", stderr="")
        self.campaign = SimpleNamespace(WORK=self.work, ROOT=ROOT, run_sql=Mock(side_effect=run_sql), bot=Mock(side_effect=bot),
                                        stop_process=Mock(), cleanup_case=Mock(side_effect=cleanup), persist=Mock(), process_alive=Mock(return_value=False))
        self.command = {"id": str(uuid.uuid4()), "kind": "portfolio_create", "strategy_id": None, "account_id": None, "result": {},
                        "payload": {"account_id": "acc", "kind": "value-momentum", "title": "Value + momentum",
                                    "blocks": [block(self.ids, "100", "value-composite", "cross-sectional-momentum")]}}

    def test_creates_case_after_flattening_previous_owner(self):
        controls.create_portfolio(self.campaign, self.state, self.command)
        case = self.state["cases"]["value-momentum"]
        self.assertEqual((case["strategy_id"], case["account_id"], case["preflight_result"]), ("camp-value-momentum", "acc", "passed"))
        self.assertEqual(self.old["state"], "cleaned")
        self.campaign.stop_process.assert_called_once_with(self.old)
        self.assertEqual([action for _, action in self.bot_calls], ["check-config", "risk-status", "risk-reset", "run"])
        source = (self.work / case["config"]).read_text()
        self.assertIn('strategy_id = "camp-value-momentum"', source)
        self.assertEqual(source.count('signal_weight = "0.5"'), 2)
        self.assertTrue(any("INSERT INTO strategy_controls" in q and "'always'" in q for q in self.executed))
        self.assertIn("succeeded", self.executed[-1])
        # A retry of the same command only finishes; it does not touch the account again.
        self.bot_calls.clear()
        controls.create_portfolio(self.campaign, self.state, self.command)
        self.assertEqual(self.bot_calls, [])

    def test_account_without_risk_owner_needs_no_handoff(self):
        self.owner = ""
        controls.create_portfolio(self.campaign, self.state, self.command)
        self.assertEqual([action for _, action in self.bot_calls], ["check-config", "run"])
        self.assertEqual(self.state["cases"]["value-momentum"]["preflight_result"], "passed")

    def test_refuses_while_a_strategy_of_the_account_is_enabled(self):
        self.enabled = ["camp-old"]
        with self.assertRaisesRegex(ValueError, "выключите стратегии этого счёта: old"):
            controls.create_portfolio(self.campaign, self.state, self.command)
        self.campaign.cleanup_case.assert_not_called()
        self.assertNotIn("value-momentum", self.state["cases"])
        for kind in ["Bad Name", "x"]:
            with self.assertRaisesRegex(ValueError, "латиница"):
                controls.create_portfolio(self.campaign, self.state, dict(self.command, payload=dict(self.command["payload"], kind=kind)))


class CommissionTests(unittest.TestCase):
    """The tariff gives the starting rates; commissions the broker charged replace them."""

    @staticmethod
    def money(value):
        units = int(value)
        return {"currency": "rub", "units": str(units), "nano": int(round((value - units) * 1e9))}

    def test_measured_rates_replace_the_tariff_once_there_is_enough_evidence(self):
        trade = lambda kind, payment, commission, **extra: dict({"type": "OPERATION_TYPE_BUY", "instrumentType": kind,
                                                                  "payment": self.money(-payment), "commission": self.money(-commission)}, **extra)
        pages = {"acc": [{"items": [trade("share", 100_000, 50)] * 6 + [{"type": "OPERATION_TYPE_BROKER_FEE", "instrumentType": "share",
                                                                        "payment": self.money(-50)}], "hasNext": True, "nextCursor": "c"},
                         {"items": [dict(trade("futures", 0, 24), price=self.money(65.0), quantityDone="1", instrumentUid="br")], "hasNext": False}]}
        calls = []

        def call(method, body):
            calls.append((method, body))
            if method == "InstrumentsService/GetFuturesMargin":
                return {"minPriceIncrement": self.money(0.01), "minPriceIncrementAmount": self.money(7.5)}
            return pages[body["accountId"]].pop(0)

        measured = controls.measure_commissions(SimpleNamespace(call=call), ["acc"], True, "2026-07-01T00:00:00+00:00")
        self.assertEqual(measured["share"]["trades"], 6, "fees are not trades")
        self.assertAlmostEqual(measured["futures"]["volume"], 48_750.0, msg="65 points × 750 ₽")
        self.assertEqual(calls[0][0], "SandboxService/GetSandboxOperationsByCursor")
        self.assertEqual(calls[1][1].get("cursor"), "c")
        rows = {r["kind"]: r for r in controls.commission_rows("trader", measured)}
        self.assertEqual((rows["share"]["source"], round(rows["share"]["rate"], 5)), ("measured", 0.05))
        self.assertEqual((rows["futures"]["source"], rows["futures"]["rate"]), ("tariff", 0.04), "one futures trade is not enough")
        self.assertEqual(rows["default"]["rate"], 0.05)
        self.assertEqual(controls.commission_rows("unknown-tariff", {})[0]["rate"], 0.3, "an unknown tariff is costed conservatively")

    def test_each_trade_and_its_commission_are_kept_for_the_portfolio_costs(self):
        item = lambda n: {"id": f"op{n}", "date": "2026-10-22T08:00:00Z", "type": "OPERATION_TYPE_SELL", "instrumentType": "share",
                          "payment": self.money(1000), "commission": self.money(-0.5)}
        api = SimpleNamespace(call=Mock(return_value={"items": [item(n) for n in range(5)] + [{"type": "OPERATION_TYPE_BUY",
                                                       "instrumentType": "share", "payment": self.money(-10)}], "hasNext": False}))
        trades = []
        controls.measure_commissions(api, ["acc"], True, "2026-10-01T00:00:00+00:00", trades=trades)
        self.assertEqual(len(trades), 5, "an operation without id or date is counted in rates only")
        self.assertEqual(trades[0], {"id": "op0", "account": "acc", "at": "2026-10-22T08:00:00Z", "kind": "share", "volume": 1000.0, "commission": 0.5})
        campaign = SimpleNamespace(run_sql=Mock(return_value=""))
        controls.store_trade_commissions(campaign, {}, trades, chunk=2)
        self.assertEqual(campaign.run_sql.call_count, 3, "rows go in chunks")
        sql = campaign.run_sql.call_args_list[0][0][1]
        self.assertIn("('op0','acc','2026-10-22T08:00:00Z'::timestamptz,'share','1000.00','0.50')", sql)
        self.assertIn("ON CONFLICT (operation_id) DO UPDATE", sql)

    def test_calibration_stores_the_rates_of_each_contour(self):
        campaign = SimpleNamespace(now=lambda: __import__("datetime").datetime(2026, 10, 21, tzinfo=__import__("datetime").timezone.utc),
                                   API=Mock(side_effect=RuntimeError("Real-account token is not connected")), run_sql=Mock(return_value=""))
        api = SimpleNamespace(call=Mock(return_value={"items": [], "hasNext": False}))
        controls.calibrate_commissions(campaign, {"cases": {"a": {"account_id": "acc", "contour": "sandbox"}}}, api)
        sql = campaign.run_sql.call_args[0][1]
        self.assertIn("DELETE FROM commission_rates WHERE contour='sandbox'", sql)
        self.assertIn("('sandbox','default','0.05000','tariff','sandbox'", sql)
        # With the real-account token connected the tariff is known before the first real portfolio.
        live = SimpleNamespace(call=Mock(side_effect=lambda method, body: {"tariff": "trader"} if method == "UsersService/GetInfo" else {"items": [], "hasNext": False}))
        campaign.API = Mock(return_value=live)
        campaign.run_sql.reset_mock()
        controls.calibrate_commissions(campaign, {"cases": {}}, api)
        sql = campaign.run_sql.call_args_list[-1][0][1]
        self.assertIn("('production','futures','0.04000','tariff','trader'", sql)
        self.assertIn("('production','share','0.05000','tariff','trader'", sql)


class UsdRubTests(unittest.TestCase):
    def test_the_rate_comes_from_the_exchange_and_implausible_values_are_refused(self):
        campaign = SimpleNamespace(run_sql=Mock(return_value=""))
        api = SimpleNamespace(call=Mock(return_value={"lastPrices": [{"price": {"units": "82", "nano": 887900000}}]}))
        self.assertAlmostEqual(controls.refresh_usd_rub(campaign, {}, api), 82.8879)
        self.assertIn("'82.887900'", campaign.run_sql.call_args[0][1])
        api.call.return_value = {"lastPrices": []}
        with self.assertRaises(ValueError):
            controls.refresh_usd_rub(campaign, {}, api)
