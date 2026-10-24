"""Durable control boundary, rankings and same-origin command authorization."""
import copy
import http.client
import importlib.util
import json
import math
from pathlib import Path
import statistics
import tempfile
import threading
import unittest
from unittest.mock import Mock, patch
import uuid

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("control_tests", ROOT / "scripts/dashboard/control.py")
control = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(control)
SPEC = importlib.util.spec_from_file_location("server_tests", ROOT / "scripts/dashboard/server.py")
server_module = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(server_module)
UID = "11111111-1111-4111-8111-111111111111"
STATE = {"cases": {"first": {"strategy_id": "first", "account_id": "account", "kind": "first"}}}


def command(kind, payload=None):
    return {"id": str(uuid.uuid4()), "strategy_id": "first", "account_id": "account", "kind": kind, "payload": payload or {}}


class ControlTests(unittest.TestCase):
    def test_archived_revision_only_accepts_identical_existing_request(self):
        service = control.Control(Path("unused"))
        case = dict(STATE["cases"]["first"], strategy_id="new", revisions=[{"strategy_id": "first"}])
        service.state = Mock(return_value={"cases": {"first": case}})
        request = command("resume")
        stored = {**request, "execution_strategy_id": "new", "status": "queued"}
        service.get_command = Mock(return_value={"command": stored})
        self.assertEqual(service.enqueue(request)["command"], stored)
        with self.assertRaises(control.ClientError):
            service.enqueue({**request, "kind": "pause"})
        service.get_command.side_effect = control.ClientError("not found", 404)
        with self.assertRaises(control.ClientError):
            service.enqueue(command("resume"))

    def test_sql_decodes_multiple_records_and_transaction_output(self):
        service = control.Control(Path("unused"))
        service.state = Mock(return_value=STATE)
        expected = {"cases": [{"strategy_id": "first"}, {"strategy_id": "second"}], "available": True}
        for text in [json.dumps(expected, indent=2), "BEGIN\n1\n" + json.dumps(expected, indent=2) + "\nCOMMIT"]:
            with patch.object(control.campaign, "run_sql", return_value=text):
                self.assertEqual(service.sql("fixture"), expected)

    def test_foreign_account_unknown_kind_and_unknown_fields_are_rejected(self):
        bad = command("pause")
        bad["account_id"] = "other"
        for value in [bad, command("withdraw"), command(["pause"]), command("execution_mode", {"mode": []}),
                      {**command("pause"), "secret": True}, command("pause", {"ignored": True})]:
            with self.assertRaises(control.ClientError):
                control.validate(value, STATE)

    def test_manual_orders_use_integer_lots_and_exact_decimal_price(self):
        payload = {"instrument_id": UID, "side": "buy", "lots": 2, "order_type": "limit", "price": "123.000000001"}
        self.assertEqual(control.validate(command("manual_order", payload), STATE)["payload"]["price"], "123.000000001")
        for changes in [{"lots": True}, {"lots": 0}, {"lots": 1.1}, {"price": "NaN"}, {"price": "0"}, {"side": "other"}, {"order_type": "market"}]:
            with self.assertRaises(control.ClientError):
                control.validate(command("manual_order", {**payload, **changes}), STATE)

    def test_money_does_not_accept_nonfinite_float_or_subkopeck_amount(self):
        for value in ["1.001", "NaN", "Infinity", "-1", "0", 1.0, "1e9999999999999"]:
            with self.subTest(value=value), self.assertRaises(control.ClientError):
                control.validate(command("budget", {"operation": "set", "amount": value}), STATE)
        self.assertEqual(control.validate(command("pay_in", {"amount": "123.45"}), STATE)["payload"]["amount"], "123.45")

    def test_global_commands_cannot_impersonate_strategy(self):
        with self.assertRaises(control.ClientError):
            control.validate(command("mode", {"mode": "free"}), STATE)
        self.assertEqual(control.validate({"id": str(uuid.uuid4()), "kind": "mode", "payload": {"mode": "free"}}, STATE)["payload"]["mode"], "free")

    def test_catalog_describes_any_type_and_says_what_the_stand_trades(self):
        raw = {"uid": UID, "instrumentType": "share", "currency": "rub", "realExchange": "REAL_EXCHANGE_MOEX", "apiTradeAvailableFlag": True, "ticker": "SBER", "lot": 10}
        share = control.instrument_metadata(raw)
        self.assertEqual((share["lot"], share["kind"], share["tradable"]), (10, "share", True))
        for changes, reason in [({"currency": "usd"}, "не в рублях"), ({"realExchange": "REAL_EXCHANGE_RTS"}, "не на Мосбирже"),
                                ({"instrumentType": "option"}, "не торгует"), ({"apiTradeAvailableFlag": False}, "через API")]:
            meta = control.instrument_metadata({**raw, **changes})
            self.assertFalse(meta["tradable"])
            self.assertIn(reason, meta["reason"])
        # A futures contract: its series, the ruble value of a point and margins; a short is a sale.
        future = control.instrument_metadata({**raw, "instrumentType": "futures", "ticker": "BRX6", "lot": 1, "shortEnabledFlag": False},
                                             {"basicAsset": "Brent", "lastTradeDate": "2026-11-02T18:45:00Z", "minPriceIncrement": {"units": "0", "nano": 10000000},
                                              "minPriceIncrementAmount": {"units": "7", "nano": 500000000}, "initialMarginOnBuy": {"units": "9000", "nano": 0}})
        self.assertEqual((future["series"], future["point_value"], future["margin_buy"], future["short_enabled"], future["tradable"]), ("BR", 750.0, 9000.0, True, True))
        self.assertIsNone(control.series_code("USDRUBF"), "a perpetual future has no series to roll")
        bond = control.instrument_metadata({**raw, "instrumentType": "bond", "ticker": "SU26238RMFS4", "lot": 1},
                                           {"nominal": {"currency": "rub", "units": "1000", "nano": 0}, "aciValue": {"units": "12", "nano": 340000000}, "maturityDate": "2041-05-15T00:00:00Z"})
        self.assertEqual((bond["nominal"], bond["aci"], bond["tradable"]), (1000.0, 12.34, True))
        self.assertEqual(control.instrument_id("futures:BR"), "futures:BR")
        with self.assertRaises(control.ClientError):
            control.instrument_id("futures:BR; DROP")

    def test_budget_is_set_only_and_difference_is_computed_by_runtime(self):
        self.assertEqual(control.validate(command("budget", {"amount": "900000"}), STATE)["payload"], {"amount": "900000", "operation": "set"})
        for operation in ["add", "subtract"]:
            with self.assertRaisesRegex(control.ClientError, "новой суммой"):
                control.validate(command("budget", {"operation": operation, "amount": "1"}), STATE)

    def test_schedule_requires_known_future_trading_days_without_account_overlap(self):
        future = "2099-10-19"
        state = dict(STATE, calendar=[{"date": future, "isTradingDay": True, "startTime": future + "T06:50:00Z", "endTime": future + "T15:39:59Z"},
                                      {"date": "2099-10-20", "isTradingDay": False, "startTime": None, "endTime": None},
                                      {"date": "2020-01-10", "isTradingDay": True, "startTime": "2020-01-10T06:50:00Z", "endTime": "2020-01-10T15:39:59Z"}])
        payload = control.validate(command("schedule", {"run_mode": "schedule", "days": [future], "close_after": True}), state)["payload"]
        control.check_schedule(state, payload, {}, [])
        for days, message in [(["2099-10-20"], "не торговый"), (["2099-12-31"], "не торговый"), (["2020-01-10"], "завершилась")]:
            with self.assertRaisesRegex(control.ClientError, message):
                control.check_schedule(state, dict(payload, days=days), {}, [])
        # An already scheduled past day may stay in the list.
        control.check_schedule(state, dict(payload, days=["2020-01-10"]), {"trading_days": ["2020-01-10"]}, [])
        with self.assertRaisesRegex(control.ClientError, "уже занят стратегией first"):
            control.check_schedule(state, payload, {}, [{"strategy_id": "first", "run_mode": "schedule", "trading_days": [future]}])
        for bad in [{"run_mode": "later", "days": [], "close_after": True}, {"run_mode": "always", "days": ["19.10.2099"], "close_after": True},
                    {"run_mode": "always", "days": [future, future], "close_after": True}, {"run_mode": "always", "days": [], "close_after": "yes"}]:
            with self.assertRaises(control.ClientError):
                control.validate(command("schedule", bad), state)

    def test_runtime_limit_is_global_bounded_and_capped_by_tariff_streams(self):
        global_command = dict(command("runtime_limit", {"value": 6}), strategy_id=None, account_id=None)
        self.assertEqual(control.validate(global_command, STATE)["payload"], {"value": 6})
        for value in [0, 11, "4", 2.5]:
            with self.assertRaises(control.ClientError):
                control.validate(dict(global_command, payload={"value": value}), STATE)
        with self.assertRaises(control.ClientError):
            control.validate(command("runtime_limit", {"value": 4}), STATE)
        self.assertEqual(control.runtime_cap({"limits": {"streamLimits": [{"limit": 32}, {"limit": 11}]}}), 10)
        self.assertEqual(control.runtime_cap({"limits": {"streamLimits": [{"limit": 6}]}}), 5)
        self.assertEqual(control.runtime_cap({}), 10)

    def test_portfolio_payload_normalizes_instruments_and_checks_blocks(self):
        body = command("portfolio", {"title": "  Мой счёт ", "blocks": [{"instruments": [UID.upper()], "share": "100", "strategies": [{"kind": "moving-average-trend", "weight": "1"}]}]})
        payload = control.validate(body, STATE)["payload"]
        self.assertEqual(payload["title"], "Мой счёт")
        self.assertEqual(payload["blocks"][0]["instruments"], [UID])
        with self.assertRaisesRegex(control.ClientError, "100%"):
            control.validate(command("portfolio", {"title": "x", "blocks": [{"instruments": [UID], "share": "50", "strategies": [{"kind": "range-breakout"}]}]}), STATE)
        with self.assertRaisesRegex(control.ClientError, "UUID"):
            control.validate(command("portfolio", {"title": "x", "blocks": [{"instruments": ["SBER"], "share": "100", "strategies": [{"kind": "range-breakout"}]}]}), STATE)

    def test_portfolio_create_is_account_level_and_checks_name_and_blocks(self):
        payload = {"account_id": "account", "kind": "value-composite", "title": "Value", "blocks": [{"instruments": [UID], "share": "100", "strategies": [{"kind": "dividend-month", "weight": "1"}]}]}
        body = dict(command("portfolio_create", payload), strategy_id=None, account_id=None)
        self.assertEqual(control.validate(copy.deepcopy(body), STATE)["payload"]["kind"], "value-composite")
        for change, message in [({"account_id": "other"}, "не принадлежит"), ({"kind": "Bad Name"}, "латиница"), ({"kind": "first"}, "уже есть"), ({"run_mode": "later"}, "Режим")]:
            with self.assertRaisesRegex(control.ClientError, message):
                control.validate(dict(copy.deepcopy(body), payload=dict(payload, **change)), STATE)
        with self.assertRaisesRegex(control.ClientError, "не меньше 10"):
            control.validate(dict(copy.deepcopy(body), payload=dict(payload, blocks=[{"instruments": [UID], "share": "100", "strategies": [{"kind": "value-composite"}]}])), STATE)

    def test_universe_duplicates_and_empty_selection_rejected(self):
        for values in [[], [UID, UID], ["invalid"]]:
            with self.assertRaises(control.ClientError):
                control.validate(command("universe", {"instruments": values}), STATE)

    def test_queue_lock_precedes_insert_and_gate_and_duplicate_does_not_repeat_mutation(self):
        instance = control.Control(Path("unused"))
        instance.state = Mock(return_value=STATE)
        value = command("pause")
        instance.sql = Mock(return_value={**value, "status": "queued"})
        result = instance.enqueue(copy.deepcopy(value))
        query = instance.sql.call_args.args[0]
        self.assertLess(query.index("FOR UPDATE"), query.index("INSERT INTO control_commands"))
        self.assertIn("enabled=false,", query)
        self.assertIn("needs_service=true", query)
        self.assertIn("AND EXISTS(SELECT 1 FROM inserted)", query)
        self.assertEqual(result["command"]["id"], value["id"])
        instance.sql.return_value = {**value, "kind": "resume"}
        with self.assertRaises(control.ClientError) as raised:
            instance.enqueue(copy.deepcopy(value))
        self.assertEqual(raised.exception.status, 409)

    def test_cancel_batch_guards_owner_and_protection_and_gates_before_ack(self):
        instance = control.Control(Path("unused"))
        instance.state = Mock(return_value=STATE)
        value = command("cancel_batch", {"batch_id": UID})
        instance.sql = Mock(return_value={**value, "status": "queued"})
        instance.enqueue(value)
        query = instance.sql.call_args.args[0]
        for expected in ["strategy_id=E'first' AND account_id=E'account'", "bc.revision=-1", "event_id NOT LIKE 'cleanup:%'", "status='rejected'", "FOR UPDATE"]:
            self.assertIn(expected, query)

    def test_rankings_exclude_cashflow_and_separate_managed_and_no_trade_runs(self):
        base = {"id": UID, "strategy_id": "first", "account_id": "account", "classification": "clean", "started_at": "2026-10-01T10:00:00Z", "anchor_equity": "1000", "principal": "500", "anchor_external_cash_flows": "0", "last_equity": "1620", "last_external_cash_flows": "600", "max_drawdown_percent": "2", "fills": 2}
        managed = {**base, "id": "other", "strategy_id": "second", "classification": "mixed", "last_equity": "1700"}
        empty = {**base, "strategy_id": "third", "fills": 0}
        output = control.ranking([base, managed, empty])
        self.assertEqual(output["clean"][0]["pnl_rub"], 20)
        self.assertEqual(output["clean"][0]["return_percent"], 4)
        self.assertEqual(output["clean"][0]["rank"], 1)
        self.assertEqual(output["mixed"][0]["return_percent"], 20)
        self.assertEqual(len(output["unranked"]), 1)
        self.assertNotIn("rank", output["unranked"][0])

    def test_rankings_keep_history_and_latest_same_classification(self):
        base = {"strategy_id": "same", "classification": "clean", "anchor_equity": 100, "last_equity": 110, "anchor_external_cash_flows": 0, "last_external_cash_flows": 0, "principal": 100, "fills": 1}
        result = control.ranking([{**base, "started_at": "2026-01-01", "id": "old"}, {**base, "started_at": "2026-02-01", "id": "new", "last_equity": 109}])
        self.assertEqual(result["clean"][0]["id"], "new")
        self.assertEqual(len(result["history"]), 2)

    def test_revised_strategies_share_rank_slot_but_keep_account_run_history(self):
        base = {"classification": "clean", "anchor_equity": 100, "last_equity": 110,
                "anchor_external_cash_flows": 0, "last_external_cash_flows": 0, "principal": 100, "fills": 1}
        old = {**base, "strategy_id": "old", "account_id": "one", "started_at": "2026-01-01", "id": "old-run"}
        new = {**base, "strategy_id": "new", "account_id": "two", "started_at": "2026-02-01", "id": "new-run"}
        result = control.ranking([old, new], {"old": "pairs", "new": "pairs"})
        self.assertEqual([r["id"] for r in result["clean"]], ["new-run"])
        self.assertEqual({r["account_id"] for r in result["history"]}, {"one", "two"})

    def test_risk_metrics_use_daily_returns_net_of_deposits_and_the_benchmark_over_the_same_days(self):
        # 1000 → +1% → −2% → deposit of 500 with no gain → +3% → +1%; the deposit is not a return.
        days = [["2026-10-19", "1010", "0"], ["2026-10-20", "989.8", "0"], ["2026-10-21", "1489.8", "500"],
                ["2026-10-22", "1534.494", "500"], ["2026-10-23", "1549.83894", "500"]]
        benchmark = {"ticker": "SBMX", "closes": {"2026-10-16": 10.0, "2026-10-19": 10.1, "2026-10-23": 10.5, "2026-10-26": 11.0}}
        m = control.risk_metrics(1000.0, 0.0, days, benchmark, "2026-10-18T17:40:00Z")
        returns = [0.01, -0.02, 0.0, 0.03, 0.01]
        self.assertEqual(m["days"], 5)
        self.assertEqual([p[1] for p in m["curve"]][-1], round(((1.01 * 0.98 * 1.03 * 1.01) - 1) * 100, 3))
        self.assertAlmostEqual(m["worst_day_percent"], -2.0)
        mean = sum(returns) / 5
        deviation = statistics.stdev(returns)
        self.assertAlmostEqual(m["volatility_percent"], deviation * math.sqrt(365) * 100)
        self.assertAlmostEqual(m["sharpe"], mean / deviation * math.sqrt(365))
        self.assertAlmostEqual(m["sortino"], mean / math.sqrt(0.02 ** 2 / 5) * math.sqrt(365))
        # From the close before the run began to the close of its last day; later closes are ignored.
        self.assertEqual(m["benchmark"], "SBMX")
        self.assertAlmostEqual(m["benchmark_percent"], 5.0)
        few = control.risk_metrics(1000.0, 0.0, days[:3], None, "2026-10-18")
        self.assertEqual((few["days"], few["sharpe"], few["benchmark_percent"]), (3, None, None), "too few days for ratios")
        self.assertEqual(control.risk_metrics(None, 0.0, None)["days"], 0)

    def test_ranking_carries_metrics_commissions_and_excess_over_the_benchmark(self):
        run = {"id": UID, "strategy_id": "first", "account_id": "account", "classification": "clean", "started_at": "2026-10-18T10:00:00Z",
               "anchor_equity": "1000", "principal": "1000", "anchor_external_cash_flows": "0", "last_equity": "1030",
               "last_external_cash_flows": "0", "fills": 3, "days": [["2026-10-19", "1030", "0"]],
               "costs": {"trades": 3, "volume_rub": "90000", "commission_rub": "45.5"}}
        benchmark = {"ticker": "SBMX", "closes": {"2026-10-17": 10.0, "2026-10-19": 10.1}}
        row = control.ranking([run], None, benchmark)["clean"][0]
        self.assertEqual((row["commission_rub"], row["trades"]), (45.5, 3))
        self.assertAlmostEqual(row["metrics"]["excess_percent"], 2.0)
        self.assertEqual(control.ranking([{**run, "costs": None, "days": None}])["clean"][0]["commission_rub"], 0)

    def test_benchmark_closes_come_from_the_chart_cache_by_moscow_day(self):
        with tempfile.TemporaryDirectory() as folder:
            (Path(folder) / "reports").mkdir()
            (Path(folder) / "reports" / "market-chart.json").write_text(json.dumps({"assets": {"b": {"series": {"day": {"candles": [
                {"time": "2026-10-22T21:00:00Z", "close": 10.5}, {"time": "2026-10-21T21:00:00Z", "close": 10.0}]}}}}}))
            closes = control.benchmark_closes(folder, {"uid": "b", "ticker": "SBMX"})
            self.assertEqual(closes, {"ticker": "SBMX", "closes": {"2026-10-23": 10.5, "2026-10-22": 10.0}})
            self.assertIsNone(control.benchmark_closes(folder, {"uid": "missing", "ticker": "X"}))
            self.assertIsNone(control.benchmark_closes(folder, None))

    def test_http_requires_session_csrf_origin_and_host_before_queueing(self):
        application = server_module.ThreadingHTTPServer(("127.0.0.1", 0), server_module.Handler)
        application.control = Mock()
        application.control.enqueue.return_value = {"command": {"status": "queued"}}
        thread = threading.Thread(target=application.serve_forever, daemon=True)
        thread.start()
        connection = http.client.HTTPConnection("127.0.0.1", application.server_port, timeout=3)
        try:
            connection.request("GET", "/api/session")
            response = connection.getresponse()
            cookie = response.getheader("Set-Cookie")
            csrf = json.loads(response.read())["csrf_token"]
            self.assertIn("HttpOnly", cookie)
            self.assertIn("SameSite=Strict", cookie)
            headers = {"Content-Type": "application/json", "Origin": f"http://127.0.0.1:{application.server_port}", "Cookie": cookie.split(";", 1)[0], "X-CSRF-Token": csrf}
            for mutation in [{"Origin": "http://evil.example"}, {"X-CSRF-Token": "wrong"}, {"Cookie": ""}, {"Host": "evil.example"}]:
                connection.request("POST", "/api/commands", body=json.dumps(command("pause")), headers={**headers, **mutation})
                response = connection.getresponse()
                self.assertEqual(response.status, 403)
                response.read()
            application.control.enqueue.assert_not_called()
            connection.request("POST", "/api/commands", body=json.dumps(command("pause")), headers=headers)
            response = connection.getresponse()
            self.assertEqual(response.status, 202)
            response.read()
            application.control.enqueue.assert_called_once()
        finally:
            connection.close()
            application.shutdown()
            application.server_close()
            thread.join()


    def test_operator_pin_is_hashed_and_locks_after_five_wrong_entries(self):
        with tempfile.TemporaryDirectory() as folder:
            pin = server_module.OperatorPin(Path(folder) / "operator-pin.json")
            pin.ITERATIONS = 1000
            self.assertFalse(pin.configured())
            self.assertEqual(pin.check("anything"), (True, ""), "no PIN set: nothing to check")
            with self.assertRaises(ValueError):
                pin.set("12")
            pin.set("4321")
            stored = (Path(folder) / "operator-pin.json").read_text()
            self.assertNotIn("4321", stored)
            self.assertEqual(oct((Path(folder) / "operator-pin.json").stat().st_mode & 0o777), "0o600")
            self.assertEqual(pin.check("4321"), (True, ""))
            for left in range(4, 0, -1):
                self.assertEqual(pin.check("0000"), (False, f"Неверный PIN: осталось попыток {left}"))
            accepted, message = pin.check("0000")
            self.assertFalse(accepted)
            self.assertIn("заблокирован", message)
            self.assertFalse(pin.check("4321")[0], "even the right PIN waits out the lockout")
            pin.locked_until = 0
            self.assertTrue(pin.check("4321")[0])

    def test_pin_is_required_for_real_money_commands_only(self):
        state = {"cases": {"live": {"strategy_id": "live", "account_id": "real", "contour": "production"},
                           "sandbox": {"strategy_id": "sandbox", "account_id": "acc"}}}
        on = lambda kind, strategy=None: control.pin_required({"kind": kind, "strategy_id": strategy}, state)
        self.assertTrue(on("live_promote"))
        self.assertTrue(on("live_resume"))
        self.assertTrue(on("live_gate"))
        self.assertTrue(on("resume", "live"))
        self.assertTrue(on("approve_batch", "live"))
        self.assertFalse(on("pause", "live"), "stopping never waits for the PIN")
        self.assertFalse(on("live_halt"))
        self.assertFalse(on("resume", "sandbox"))
        self.assertFalse(on("ai_budget"))

    def test_http_asks_for_the_pin_before_a_real_money_command_and_unlocks_the_session(self):
        application = server_module.ThreadingHTTPServer(("127.0.0.1", 0), server_module.Handler)
        application.control = Mock()
        application.control.state.return_value = {"cases": {"live": {"strategy_id": "live", "account_id": "real", "contour": "production"}}}
        application.control.enqueue.return_value = {"command": {"status": "queued"}}
        folder = tempfile.TemporaryDirectory()
        application.pin = server_module.OperatorPin(Path(folder.name) / "operator-pin.json")
        application.pin.ITERATIONS = 1000
        application.pin.set("2468")
        thread = threading.Thread(target=application.serve_forever, daemon=True)
        thread.start()
        connection = http.client.HTTPConnection("127.0.0.1", application.server_port, timeout=3)
        live = lambda kind: dict(command(kind), strategy_id="live", account_id="real")

        def post(body, **extra):
            connection.request("POST", "/api/commands", body=json.dumps(body), headers={**headers, **extra})
            response = connection.getresponse()
            return response.status, json.loads(response.read())
        try:
            connection.request("GET", "/api/session")
            response = connection.getresponse()
            cookie = response.getheader("Set-Cookie")
            csrf = json.loads(response.read())["csrf_token"]
            headers = {"Content-Type": "application/json", "Origin": f"http://127.0.0.1:{application.server_port}",
                       "Cookie": cookie.split(";", 1)[0], "X-CSRF-Token": csrf}
            self.assertEqual(post(live("pause"))[0], 202, "pause needs no PIN")
            status, value = post(live("resume"))
            self.assertEqual((status, value["pin_required"]), (401, True))
            status, value = post(live("resume"), **{"X-Operator-Pin": "1111"})
            self.assertEqual(status, 401)
            self.assertIn("осталось попыток 4", value["error"])
            self.assertEqual(application.control.enqueue.call_count, 1)
            self.assertEqual(post(live("resume"), **{"X-Operator-Pin": "2468"})[0], 202)
            self.assertEqual(post(live("cleanup"))[0], 202, "the session stays unlocked for a while")
            self.assertEqual(application.control.enqueue.call_count, 3)
        finally:
            connection.close()
            application.shutdown()
            application.server_close()
            thread.join()
            folder.cleanup()

if __name__ == "__main__":
    unittest.main()
