"""Opt-in real PostgreSQL checks, using an isolated pre-migrated test database.

SANDBOX_CONTROL_TEST_CONTAINER=tinkoff-controls-test python3 -m unittest discover -s tests -p test_sandbox_control_postgres.py
Never points at the live campaign database.
"""
import concurrent.futures
import copy
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock
import uuid

SPEC = importlib.util.spec_from_file_location("postgres_dashboard_control", Path(__file__).resolve().parents[1] / "scripts/dashboard/control.py")
control = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(control)
SPEC = importlib.util.spec_from_file_location("postgres_campaign_control", Path(__file__).resolve().parents[1] / "scripts/campaign_control.py")
campaign_control = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(campaign_control)
CONTAINER = os.environ.get("SANDBOX_CONTROL_TEST_CONTAINER")


@unittest.skipUnless(CONTAINER, "Set SANDBOX_CONTROL_TEST_CONTAINER for private PostgreSQL integration")
class ControlPostgresTests(unittest.TestCase):
    def execute(self, sql):
        result = subprocess.run(["docker", "exec", "-i", CONTAINER, "psql", "-X", "-qAt", "-U", "tinkoff_campaign", "-d", "dashboard_controls", "-v", "ON_ERROR_STOP=1"], input=sql, text=True, capture_output=True, check=True)
        return result.stdout

    def query(self, sql):
        # json_agg puts each record on its own line, so decode the whole JSON value
        # (the same way scripts/dashboard/control.py does), not only the last line.
        output = self.execute(sql)
        decoder = json.JSONDecoder()
        for start in [i for i, char in enumerate(output) if char in "{[" and (i == 0 or output[i - 1] == "\n")]:
            try:
                return decoder.raw_decode(output[start:])[0]
            except json.JSONDecodeError:
                continue
        raise AssertionError("psql returned no JSON value: " + output)

    def setUp(self):
        self.sid = "dashboard-test-" + str(uuid.uuid4())
        self.aid = "fixture-" + str(uuid.uuid4())
        self.bid = str(uuid.uuid4())
        self.rid = str(uuid.uuid4())
        self.state = {"cases": {"test": {"strategy_id": self.sid, "account_id": self.aid, "kind": "test"}}}
        self.service = control.Control(Path("unused"))
        self.service.state = Mock(return_value=self.state)
        self.service.sql = self.query
        self.execute(f"""
            INSERT INTO strategy_instances(id,kind,account_id,runtime_version) VALUES('{self.sid}','test','{self.aid}',2);
            INSERT INTO strategy_controls(strategy_id,account_id,enabled,revision) VALUES('{self.sid}','{self.aid}',true,7);
            INSERT INTO rebalance_batches(id,event_id,strategy_id,account_id,target,effective_risk,sizing_capital,status,created_at)
              VALUES('{self.bid}','natural:test','{self.sid}','{self.aid}','{{}}','{{}}',1000,'planned',now());
            INSERT INTO batch_controls(batch_id,status,revision) VALUES('{self.bid}','awaiting',7);
        """)
        self.ids = []

    def tearDown(self):
        self.execute(f"""
            DELETE FROM performance_samples WHERE run_id IN(SELECT id FROM strategy_runs WHERE strategy_id='{self.sid}');
            DELETE FROM strategy_runs WHERE strategy_id='{self.sid}';
            DELETE FROM fills WHERE request_id IN(SELECT request_id FROM trade_intents WHERE strategy_id='{self.sid}');
            DELETE FROM orders WHERE intent_id IN(SELECT id FROM trade_intents WHERE strategy_id='{self.sid}');
            DELETE FROM trade_intents WHERE strategy_id='{self.sid}';
            DELETE FROM batch_controls WHERE batch_id='{self.bid}';
            DELETE FROM rebalance_batches WHERE strategy_id='{self.sid}';
            DELETE FROM control_commands WHERE strategy_id='{self.sid}';
            DELETE FROM strategy_controls WHERE strategy_id='{self.sid}';
            DELETE FROM strategy_instances WHERE id='{self.sid}';
        """)

    def command(self, kind, payload=None):
        return {"id": str(uuid.uuid4()), "kind": kind, "strategy_id": self.sid, "account_id": self.aid, "payload": payload or {}}

    def controls(self):
        return self.query(f"SELECT row_to_json(c) FROM strategy_controls c WHERE strategy_id='{self.sid}'")

    def test_concurrent_duplicate_pause_is_inserted_and_gated_once(self):
        value = self.command("pause")
        with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
            results = list(pool.map(lambda _: self.service.enqueue(copy.deepcopy(value)), range(3)))
        self.assertEqual({r["command"]["id"] for r in results}, {value["id"]})
        row = self.controls()
        self.assertFalse(row["enabled"])
        self.assertTrue(row["needs_service"])
        self.assertEqual(row["revision"], 8)
        with self.assertRaises(control.ClientError) as raised:
            self.service.enqueue({**value, "kind": "resume"})
        self.assertEqual(raised.exception.status, 409)
        self.assertEqual(self.controls()["revision"], 8)

    def test_routed_request_retains_identity_and_accepts_retry_of_archived_revision(self):
        value = self.command("resume")
        self.service.enqueue(value)
        self.execute(f"INSERT INTO strategy_state(strategy_id,state) VALUES('{self.sid}','{{}}')")
        with tempfile.TemporaryDirectory() as temporary:
            work = Path(temporary)
            (work / "configs").mkdir()
            (work / "configs/old.toml").write_text(f'strategy_id = "{self.sid}"\n')
            case = self.state["cases"]["test"]
            case["config"] = "configs/old.toml"
            campaign = SimpleNamespace(WORK=work, run_sql=lambda _, q: self.execute(q).strip(), persist=Mock())
            campaign_control.renew_returning_strategy(campaign, self.state, case, str(uuid.uuid4()))
        new_id = case["strategy_id"]
        self.addCleanup(lambda: self.execute(f"DELETE FROM strategy_controls WHERE strategy_id='{new_id}'"))
        self.assertEqual(self.controls()["superseded_by"], new_id)
        result = self.service.enqueue(copy.deepcopy(value))["command"]
        self.assertEqual(result["strategy_id"], self.sid)
        self.assertEqual(result["execution_strategy_id"], new_id)
        self.assertEqual(result["id"], value["id"])
        self.assertEqual(result["payload"], value["payload"])
        self.assertEqual(self.execute(f"SELECT count(*) FROM control_commands WHERE COALESCE(execution_strategy_id,strategy_id)='{new_id}' AND status='queued'").strip(), "1")
        with self.assertRaises(control.ClientError):
            self.service.enqueue({**value, "kind": "pause"})
        with self.assertRaises(control.ClientError):
            self.service.enqueue({**value, "id": str(uuid.uuid4())})

    def test_stale_snapshot_cannot_enqueue_after_owner_handoff(self):
        self.execute(f"UPDATE strategy_controls SET superseded_by='new-owner' WHERE strategy_id='{self.sid}'")
        with self.assertRaises(control.ClientError):
            self.service.enqueue(self.command("pause"))
        self.assertEqual(self.controls()["revision"], 7)

    def test_universe_transition_blocks_new_commands_but_existing_retry_survives(self):
        value = self.command("resume")
        self.service.enqueue(value)
        transition = str(uuid.uuid4())
        self.execute(f"INSERT INTO control_commands(id,strategy_id,account_id,kind) VALUES('{transition}','{self.sid}','{self.aid}','universe')")
        self.assertEqual(self.service.enqueue(value)["command"]["id"], value["id"])
        with self.assertRaises(control.ClientError):
            self.service.enqueue(self.command("pause"))

    def test_approval_and_rejectable_budget_do_not_invalidate_plan_on_enqueue(self):
        for kind, payload in [("approve_batch", {"batch_id": self.bid}), ("budget", {"operation": "subtract", "amount": "500.00"}), ("pay_in", {"amount": "100.00"}), ("execution_mode", {"mode": "confirm"})]:
            self.service.enqueue(self.command(kind, payload))
            self.assertEqual(self.controls()["revision"], 7)
            self.assertTrue(self.controls()["enabled"])
        row = self.query(f"SELECT row_to_json(c) FROM batch_controls c WHERE batch_id='{self.bid}'")
        self.assertEqual(row["revision"], 7)
        self.assertEqual(row["status"], "awaiting")

    def test_cancel_batch_gates_whole_batch_and_blocks_protective_and_foreign(self):
        value = self.command("cancel_batch", {"batch_id": self.bid})
        self.service.enqueue(value)
        row = self.query(f"SELECT row_to_json(c) FROM batch_controls c WHERE batch_id='{self.bid}'")
        self.assertEqual(row["status"], "rejected")
        self.execute(f"UPDATE batch_controls SET revision=-1,status='approved' WHERE batch_id='{self.bid}'")
        for bid in [self.bid, str(uuid.uuid4())]:
            with self.assertRaises(control.ClientError) as raised:
                self.service.enqueue(self.command("cancel_batch", {"batch_id": bid}))
            self.assertEqual(raised.exception.status, 409)
        self.assertEqual(self.query(f"SELECT row_to_json(c) FROM batch_controls c WHERE batch_id='{self.bid}'")["status"], "approved")

    def test_snapshot_run_and_cashflow_ranking_uses_real_fill_count(self):
        intent, request = str(uuid.uuid4()), str(uuid.uuid4())
        self.execute(f"""
            INSERT INTO strategy_runs(id,strategy_id,account_id,classification,started_at,anchor_equity,principal,anchor_external_cash_flows,last_equity,last_external_cash_flows)
              VALUES('{self.rid}','{self.sid}','{self.aid}','clean',now()-interval '1 hour',1000,500,0,1620,600);
            INSERT INTO trade_intents(id,request_id,event_id,strategy_id,account_id,instrument_id,side,kind,lots,status,created_at)
              VALUES('{intent}','{request}','natural-fill','{self.sid}','{self.aid}','fixture-instrument','buy','market',1,'filled',now());
            INSERT INTO orders(request_id,intent_id,status,lots_requested,lots_executed,observed_at)
              VALUES('{request}','{intent}','filled',1,1,now());
            INSERT INTO fills(trade_id,request_id,price,lots,executed_at) VALUES('{intent}','{request}',100,1,now());
        """)
        result = self.service.data()
        run = next(r for r in result["ranking"]["clean"] if r["strategy_id"] == self.sid)
        self.assertEqual(run["pnl_rub"], 20)
        self.assertEqual(run["return_percent"], 4)
        self.assertEqual(run["fills"], 1)
        self.execute(f"UPDATE strategy_runs SET classification='mixed',interventions='[\"manual_order\"]' WHERE id='{self.rid}'")
        result = self.service.data()
        self.assertFalse(any(r["strategy_id"] == self.sid for r in result["ranking"]["clean"]))
        self.assertTrue(any(r["strategy_id"] == self.sid for r in result["ranking"]["mixed"]))


if __name__ == "__main__":
    unittest.main()
