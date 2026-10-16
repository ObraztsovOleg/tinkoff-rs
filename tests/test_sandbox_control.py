"""Durable control boundary, rankings and same-origin command authorization."""
import copy
import http.client
import importlib.util
import json
from pathlib import Path
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

    def test_catalog_checks_currency_and_exchange_and_type(self):
        raw = {"uid": UID, "instrumentType": "share", "currency": "rub", "realExchange": "REAL_EXCHANGE_MOEX", "apiTradeAvailableFlag": True, "ticker": "SBER", "lot": 10}
        self.assertEqual(control.instrument_metadata(raw)["lot"], 10)
        for changes in [{"currency": "usd"}, {"realExchange": "REAL_EXCHANGE_RTS"}, {"instrumentType": "bond"}, {"apiTradeAvailableFlag": False}]:
            with self.assertRaises(control.ClientError):
                control.instrument_metadata({**raw, **changes})

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


if __name__ == "__main__":
    unittest.main()
