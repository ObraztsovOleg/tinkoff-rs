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
import unittest.mock
from unittest.mock import Mock
import uuid

SPEC = importlib.util.spec_from_file_location("postgres_dashboard_control", Path(__file__).resolve().parents[1] / "scripts/dashboard/control.py")
control = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(control)
SPEC = importlib.util.spec_from_file_location("postgres_campaign_control", Path(__file__).resolve().parents[1] / "scripts/campaign_control.py")
campaign_control = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(campaign_control)
CONTAINER = os.environ.get("SANDBOX_CONTROL_TEST_CONTAINER")


def setUpModule():
    # Control.data() reports the real-account status; never read the operator's token here.
    patcher = unittest.mock.patch.object(control.campaign, "fish_literal", return_value=None)
    patcher.start()
    unittest.addModuleCleanup(patcher.stop)
    environment = unittest.mock.patch.dict(os.environ, {"TINKOFF_TOKEN_PRODUCTION": "", "TINKOFF_TOKEN_PRODUCTION_FILE": ""})
    environment.start()
    unittest.addModuleCleanup(environment.stop)


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

    def test_schedule_and_runtime_limit_apply_at_once_with_journal_and_overlap_check(self):
        day, other_day = "2099-10-19", "2099-10-20"
        self.state["calendar"] = [{"date": d, "isTradingDay": True, "startTime": d + "T06:50:00Z", "endTime": d + "T15:39:59Z"} for d in (day, other_day)]
        other = "dashboard-test-other-" + str(uuid.uuid4())
        self.execute(f"INSERT INTO strategy_controls(strategy_id,account_id,enabled,run_mode,trading_days) VALUES('{other}','{self.aid}',true,'schedule','[\"{other_day}\"]')")
        self.addCleanup(self.execute, f"DELETE FROM strategy_controls WHERE strategy_id='{other}'")
        value = self.command("schedule", {"run_mode": "schedule", "days": [day], "close_after": False})
        row = self.service.enqueue(copy.deepcopy(value))["command"]
        self.assertEqual(row["status"], "succeeded")
        self.assertEqual(row["result"]["before"]["run_mode"], "schedule")
        self.assertEqual(self.service.enqueue(copy.deepcopy(value))["command"]["id"], value["id"], "retry must not apply twice")
        saved = self.controls()
        self.assertEqual((saved["trading_days"], saved["close_after"], saved["schedule_initialized"]), ([day], False, True))
        with self.assertRaisesRegex(control.ClientError, "уже занят"):
            self.service.enqueue(self.command("schedule", {"run_mode": "schedule", "days": [other_day], "close_after": True}))
        self.assertEqual(self.controls()["trading_days"], [day])
        before = self.query("SELECT json_build_object('value',max_runtimes) FROM dashboard_settings WHERE id")["value"]
        self.addCleanup(self.execute, f"UPDATE dashboard_settings SET max_runtimes={before} WHERE id")
        limit = {"id": str(uuid.uuid4()), "kind": "runtime_limit", "strategy_id": None, "account_id": None, "payload": {"value": 7}}
        self.addCleanup(self.execute, f"DELETE FROM control_commands WHERE id='{limit['id']}'")
        row = self.service.enqueue(copy.deepcopy(limit))["command"]
        self.assertEqual((row["status"], row["result"]["before"]["value"], row["result"]["after"]["value"]), ("succeeded", before, 7))
        self.assertEqual(self.query("SELECT json_build_object('value',max_runtimes) FROM dashboard_settings WHERE id")["value"], 7)
        self.assertEqual(self.service.data()["max_runtimes"], 7)

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
        for kind, payload in [("approve_batch", {"batch_id": self.bid}), ("budget", {"amount": "500.00"}), ("pay_in", {"amount": "100.00"}), ("execution_mode", {"mode": "confirm"})]:
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


    def test_live_constraints_gate_history_and_emergency_stop(self):
        live = self.sid + "-live"
        try:
            # A real account always confirms plans and never closes on a schedule.
            for row in ["'auto',false", "'confirm',true"]:
                with self.assertRaises(subprocess.CalledProcessError):
                    self.execute(f"INSERT INTO strategy_controls(strategy_id,account_id,contour,execution_mode,close_after) VALUES('{live}','{self.aid}-live','production',{row})")
            self.execute(f"INSERT INTO strategy_controls(strategy_id,account_id,contour,execution_mode,close_after,enabled) VALUES('{live}','{self.aid}-live','production','confirm',false,true)")
            # Risk history survives the current state being cleared.
            blocked = json.dumps({"blocked": {"reason": "daily_loss", "blocked_at": "2026-10-19T08:00:00Z", "session_date": "2026-10-19", "detail": "daily loss limit reached"}})
            self.execute(f"INSERT INTO account_risk_state(account_id,strategy_id,epoch,state,runtime_version,updated_at) VALUES('{self.aid}','{self.sid}','{uuid.uuid4()}','{blocked}',2,now())")
            self.execute(f"UPDATE account_risk_state SET state='{blocked}' WHERE account_id='{self.aid}'")
            self.execute(f"""UPDATE account_risk_state SET state='{{"blocked":null}}' WHERE account_id='{self.aid}'""")
            for event in ["schedule:SessionOpen:2026-10-19:2026-10-19T06:50:00+00:00", "schedule:SessionOpen:2026-10-20:2026-10-20T06:50:00+00:00",
                          "schedule:SessionClose:2026-10-20:2026-10-20T15:39:00+00:00", "candle:x:day:2026-10-20T00:00:00+00:00"]:
                self.execute(f"INSERT INTO processed_events(strategy_id,event_id) VALUES('{self.sid}','{event}')")
            gate = self.query(campaign_control.GATE_SQL.format(where=f" AND c.strategy_id='{self.sid}'"))
            self.assertEqual((gate[0]["sessions"], gate[0]["fills"], gate[0]["risk_blocks"]), (2, 0, 1))
            self.service.enqueue({"id": str(uuid.uuid4()), "kind": "live_halt", "strategy_id": None, "account_id": None, "payload": {"reason": "test"}})
            stopped = self.query(f"SELECT json_build_object('halted',(SELECT halted FROM live_settings),'enabled',(SELECT enabled FROM strategy_controls WHERE strategy_id='{live}'),'sandbox',(SELECT enabled FROM strategy_controls WHERE strategy_id='{self.sid}'))")
            self.assertEqual(stopped, {"halted": True, "enabled": False, "sandbox": True})
            self.service.enqueue({"id": str(uuid.uuid4()), "kind": "live_resume", "strategy_id": None, "account_id": None, "payload": {}})
            self.assertEqual(self.query(f"SELECT json_build_object('halted',(SELECT halted FROM live_settings),'enabled',(SELECT enabled FROM strategy_controls WHERE strategy_id='{live}'))"),
                             {"halted": False, "enabled": False})
        finally:
            self.execute(f"""
                DELETE FROM processed_events WHERE strategy_id='{self.sid}';
                DELETE FROM risk_block_events WHERE strategy_id='{self.sid}';
                DELETE FROM account_risk_state WHERE account_id='{self.aid}';
                DELETE FROM strategy_controls WHERE strategy_id='{live}';
                DELETE FROM control_commands WHERE kind IN ('live_halt','live_resume') AND payload->>'reason' IS DISTINCT FROM 'keep';
                UPDATE live_settings SET halted=false,halt_reason=NULL,halted_at=NULL;
            """)

    def test_revision_and_promotion_copy_active_model_notes_once(self):
        new = self.sid + "-u1"
        self.addCleanup(self.execute, f"DELETE FROM ai_memory WHERE strategy_id IN ('{self.sid}','{new}'); DELETE FROM ai_theses WHERE strategy_id IN ('{self.sid}','{new}'); DELETE FROM strategy_lineage WHERE strategy_id='{new}'")
        self.execute(f"""
            INSERT INTO ai_theses(id,strategy_id,ticker,direction,thesis,entry_price,conditions) VALUES
              (gen_random_uuid(),'{self.sid}','LKOH','long','Нефть растёт',7000,'[{{"kind":"price_below","value":6500}}]');
            INSERT INTO ai_theses(id,strategy_id,ticker,direction,thesis,closed_at) VALUES(gen_random_uuid(),'{self.sid}','GAZP','short','Закрыт',now());
            INSERT INTO ai_memory(id,strategy_id,note,kind) VALUES(gen_random_uuid(),'{self.sid}','Не входить до отчёта','lesson');
            INSERT INTO ai_memory(id,strategy_id,note,tickers) VALUES(gen_random_uuid(),'{self.sid}','SBER: держу до отчёта','["SBER"]');
            INSERT INTO ai_memory(id,strategy_id,note,closed_at) VALUES(gen_random_uuid(),'{self.sid}','закрыта',now());
            INSERT INTO ai_memory(id,strategy_id,note,expires_at) VALUES(gen_random_uuid(),'{self.sid}','истекла',now()-interval '1 day');
        """)
        # A retried revision must not duplicate the notes.
        for _ in range(2):
            self.execute("BEGIN; " + campaign_control.inherit_memory(self.sid, new, "revision") + "COMMIT;")
        notes = self.query(f"SELECT coalesce(json_agg(json_build_object('note',note,'from',inherited_from,'tickers',tickers,'kind',kind) ORDER BY kind),'[]') FROM ai_memory WHERE strategy_id='{new}'")
        self.assertEqual(notes, [{"note": "Не входить до отчёта", "from": self.sid, "tickers": [], "kind": "lesson"},
                                 {"note": "SBER: держу до отчёта", "from": self.sid, "tickers": ["SBER"], "kind": "note"}])
        theses = self.query(f"SELECT coalesce(json_agg(json_build_object('ticker',ticker,'entry',entry_price,'conditions',conditions,'from',inherited_from)),'[]') FROM ai_theses WHERE strategy_id='{new}'")
        self.assertEqual(theses, [{"ticker": "LKOH", "entry": 7000, "conditions": [{"kind": "price_below", "value": 6500}], "from": self.sid}], "only open theses move")
        self.assertEqual(self.query(f"SELECT row_to_json(l) FROM (SELECT parent_id,relation FROM strategy_lineage WHERE strategy_id='{new}') l"),
                         {"parent_id": self.sid, "relation": "revision"})

    def test_nightly_retention_keeps_replay_markers_and_thins_old_samples(self):
        self.execute(f"""
            INSERT INTO processed_events(strategy_id,event_id,processed_at) VALUES
              ('{self.sid}','risk-check:1',now()-interval '10 days'),
              ('{self.sid}','risk-check:2',now()),
              ('{self.sid}','candle:x:1d:old',now()-interval '40 days'),
              ('{self.sid}','schedule:SessionOpen:old',now()-interval '40 days');
        """)
        self.execute(f"""
            INSERT INTO strategy_runs(id,strategy_id,account_id,classification,started_at,anchor_equity,principal,anchor_external_cash_flows,last_equity,last_external_cash_flows)
              VALUES('{self.rid}','{self.sid}','{self.aid}','clean',now()-interval '400 days',1000,1000,0,1000,0);
            INSERT INTO performance_samples(run_id,observed_at,equity,external_cash_flows) VALUES
              ('{self.rid}',date_trunc('hour',now()-interval '40 days')+interval '5 minutes',1000,0),
              ('{self.rid}',date_trunc('hour',now()-interval '40 days')+interval '25 minutes',1001,0),
              ('{self.rid}',date_trunc('hour',now()-interval '40 days')+interval '55 minutes',1002,0),
              ('{self.rid}',date_trunc('day',now()-interval '380 days')+interval '1 hour',990,0),
              ('{self.rid}',date_trunc('day',now()-interval '380 days')+interval '20 hours',995,0),
              ('{self.rid}',now()-interval '1 hour',1010,0),
              ('{self.rid}',now()-interval '30 minutes',1011,0);
        """)
        self.addCleanup(self.execute, f"DELETE FROM processed_events WHERE strategy_id='{self.sid}'")
        deleted = self.query(control.campaign.RETENTION_SQL)
        self.assertEqual((deleted["risk_check_events"], deleted["performance_samples"]), (1, 3))
        events = self.query(f"SELECT json_agg(event_id ORDER BY event_id) FROM processed_events WHERE strategy_id='{self.sid}'")
        self.assertEqual(events, ["candle:x:1d:old", "risk-check:2", "schedule:SessionOpen:old"], "warmup and session-open markers stay")
        equity = self.query(f"SELECT json_agg(equity::int ORDER BY observed_at) FROM performance_samples WHERE run_id='{self.rid}'")
        self.assertEqual(equity, [995, 1002, 1010, 1011], "the last sample of each old day or hour stays, recent ones all")

    def test_document_feed_searches_full_text_in_any_case_and_shows_other_sources(self):
        ids = [f"test:{uuid.uuid4().hex[:12]}" for _ in range(3)]
        self.addCleanup(self.execute, "DELETE FROM documents WHERE id IN (" + ",".join(f"'{i}'" for i in ids) + ")")
        self.execute(f"""
            INSERT INTO documents(id,source,kind,title,body,published_at,tickers,content_hash,simhash,cluster_id) VALUES
              ('{ids[0]}','eDisclosure','disclosure','Сбербанк: решения совета директоров','Совет директоров рекомендовал выплатить дивиденды.',now()-interval '1 hour','{{SBER}}','h0',0,'{ids[0]}'),
              ('{ids[1]}','kommersant.ru','article','Сбер рекомендовал дивиденды','Банк направит половину прибыли акционерам.',now()-interval '30 minutes','{{}}','h1',0,'{ids[0]}'),
              ('{ids[2]}','tass','news','Нефть дорожает','Brent превысила 110 долларов за баррель.',now()-interval '10 days','{{}}','h2',0,'{ids[2]}');
            INSERT INTO document_chunks(document_id,ord,text,context,published_at)
              SELECT id,0,title || E'\n' || body,source,published_at FROM documents WHERE id IN ('{ids[0]}','{ids[1]}','{ids[2]}');
        """)
        found = self.service.documents({"q": "дивидендов", "days": "3"})["documents"]
        mine = [d for d in found if d["id"] in ids]
        self.assertEqual([d["id"] for d in mine], [ids[1], ids[0]], "inflected words match, newest first")
        self.assertEqual(mine[0]["sources"], 2, "the event has two sources")
        self.assertEqual([d["id"] for d in self.service.documents({"ticker": "SBER", "days": "3"})["documents"] if d["id"] in ids], [ids[0]])
        self.assertFalse([d for d in self.service.documents({"q": "нефть", "days": "3"})["documents"] if d["id"] in ids], "older than the period")
        self.assertTrue([d for d in self.service.documents({"q": "нефть", "days": "30", "kind": "news"})["documents"] if d["id"] == ids[2]])
        document = self.service.document(ids[0])
        self.assertEqual((document["kind"], [x["id"] for x in document["same_event"]]), ("disclosure", [ids[1]]))
        for bad in [{"kind": "rumor"}, {"ticker": "SB'ER"}, {"days": "x"}]:
            with self.subTest(bad), self.assertRaises(control.ClientError):
                self.service.documents(bad)
        with self.assertRaises(control.ClientError):
            self.service.document("x'; DROP TABLE documents; --")

    def test_operator_exposures_survive_model_drafts_and_are_journaled(self):
        ticker = "T" + uuid.uuid4().hex[:6].upper()
        self.addCleanup(self.execute, f"DELETE FROM instrument_profiles WHERE ticker='{ticker}'; DELETE FROM control_commands WHERE kind LIKE 'exposure_%' AND payload->>'ticker'='{ticker}'")
        command = lambda kind, payload: {"id": str(uuid.uuid4()), "kind": kind, "strategy_id": None, "account_id": None, "payload": dict(payload, ticker=ticker)}
        with self.assertRaises(control.ClientError):
            self.service.enqueue(command("exposure_confirm", {}))
        self.execute(f"""INSERT INTO instrument_profiles(ticker,name,drafted_at) VALUES('{ticker}','Нефтяник',now());
            INSERT INTO instrument_exposures(ticker,topic,sensitivity,note) VALUES('{ticker}','oil',1.0,'черновик'),('{ticker}','ruble',-1.0,'экспорт')""")
        result = self.service.enqueue(command("exposure_set", {"topic": "oil", "sensitivity": "2", "note": "главный драйвер"}))
        self.assertEqual(result["command"]["status"], "succeeded")
        self.assertEqual(result["command"]["result"]["before"]["sensitivity"], 1.0)
        self.service.enqueue(command("exposure_set", {"topic": "ruble", "sensitivity": None, "note": ""}))
        self.service.enqueue(command("exposure_confirm", {}))
        # A fresh model draft rewrites only model values.
        self.execute(f"""INSERT INTO instrument_exposures(ticker,topic,sensitivity,note) VALUES('{ticker}','oil',0.5,'новый черновик')
            ON CONFLICT (ticker,topic) DO UPDATE SET sensitivity=EXCLUDED.sensitivity WHERE instrument_exposures.origin='model'""")
        profile = next(p for p in self.service.data()["exposures"] if p["ticker"] == ticker)
        self.assertEqual(profile["exposures"], {"oil": {"value": 2.0, "note": "главный драйвер", "origin": "operator"}})
        self.assertIsNotNone(profile["confirmed_at"])
        # The stand's measured reaction is shown next to it, never applied by itself.
        self.execute(f"""INSERT INTO exposure_suggestions(ticker,topic,observations,effect_percent,correlation,suggested)
            VALUES('{ticker}','oil',14,0.62,0.41,1.0)""")
        self.addCleanup(self.execute, f"DELETE FROM exposure_suggestions WHERE ticker='{ticker}'")
        profile = next(p for p in self.service.data()["exposures"] if p["ticker"] == ticker)
        self.assertEqual({k: v for k, v in profile["measured"]["oil"].items() if k != "at"},
                         {"suggested": 1.0, "effect": 0.62, "correlation": 0.41, "observations": 14})
        self.assertEqual(profile["exposures"]["oil"]["value"], 2.0)
        self.service.enqueue(command("exposure_redraft", {}))
        self.assertIsNone(next(p for p in self.service.data()["exposures"] if p["ticker"] == ticker)["drafted_at"])


    def test_ai_models_are_ranked_by_decision_outcomes_and_monitored(self):
        strategy = "ai-" + uuid.uuid4().hex[:8]
        model = "test-model-" + uuid.uuid4().hex[:6]
        self.addCleanup(self.execute, f"DELETE FROM ai_outcomes WHERE decision_id IN (SELECT id FROM ai_decisions WHERE strategy_id='{strategy}'); "
                                      f"DELETE FROM ai_decisions WHERE strategy_id='{strategy}'; DELETE FROM ai_activity WHERE strategy_id='{strategy}'")
        first, second = uuid.uuid4(), uuid.uuid4()
        self.execute(f"""INSERT INTO ai_decisions(id,strategy_id,account_id,kind,status,profile,provider,model,weights,hold,cost_usd,duration_ms,trigger,created_at) VALUES
            ('{first}','{strategy}','acc','decision','ok','sonnet','anthropic','{model}','[{{"ticker":"SBER","weight_percent":20}}]',false,0.05,30000,'session_open',now()-interval '1 day'),
            ('{second}','{strategy}','acc','decision','ok','sonnet','anthropic','{model}','[]',true,0.03,20000,'scheduled',now()-interval '1 minute'),
            ('{uuid.uuid4()}','{strategy}','acc','decision','failed','sonnet','anthropic',NULL,'[]',false,NULL,500,'news_burst',now());
            INSERT INTO ai_outcomes(decision_id,horizon_days,portfolio_return,benchmark_return) VALUES ('{first}',5,1.5,0.5),('{second}',5,-0.2,0.3);
            INSERT INTO ai_activity(strategy_id,state,trigger,profile) VALUES('{strategy}','thinking','news_burst','sonnet')""")
        data = self.service.data()
        rating = next(r for r in data["ai_rating"] if r["model"] == model)
        self.assertEqual((rating["decisions"], rating["holds"], rating["h5"]["n"], rating["h5"]["beat"]), (2, 1, 2, 1))
        self.assertAlmostEqual(float(rating["h5"]["excess"]), 0.25)
        self.assertEqual(rating["h20"]["n"], 0)
        now = next(r for r in data["ai_now"] if r["strategy_id"] == strategy)
        self.assertEqual((now["activity"]["state"], now["activity"]["trigger"]), ("thinking", "news_burst"))
        self.assertEqual(now["last"]["status"], "failed", "the latest request is the failed one")


    def test_trade_band_is_set_immediately_and_commission_rates_are_reported(self):
        self.addCleanup(self.execute, "UPDATE dashboard_settings SET min_trade_percent=0.5 WHERE id; DELETE FROM commission_rates WHERE contour='sandbox'")
        self.execute("""INSERT INTO commission_rates(contour,kind,rate_percent,source,tariff,trades,volume_rub,commission_rub)
            VALUES('sandbox','share',0.05,'measured','sandbox',12,2548596,1274.3),('sandbox','default',0.05,'tariff','sandbox',0,0,0)
            ON CONFLICT DO NOTHING""")
        result = self.service.enqueue({"id": str(uuid.uuid4()), "kind": "trade_band", "strategy_id": None, "account_id": None, "payload": {"percent": "1.25"}})
        self.assertEqual(result["command"]["status"], "succeeded")
        data = self.service.data()
        self.assertEqual(float(data["min_trade_percent"]), 1.25)
        share = next(r for r in data["commissions"] if r["kind"] == "share")
        self.assertEqual((share["source"], share["trades"]), ("measured", 12))
        with self.assertRaises(control.ClientError):
            self.service.enqueue({"id": str(uuid.uuid4()), "kind": "trade_band", "strategy_id": None, "account_id": None, "payload": {"percent": "25"}})


    def test_ai_spend_is_reported_in_rubles_and_the_daily_limit_is_set_in_rubles(self):
        strategy = "rub-" + uuid.uuid4().hex[:8]
        self.addCleanup(self.execute, f"UPDATE dashboard_settings SET ai_budget_rub=500 WHERE id; DELETE FROM ai_usage WHERE strategy_id='{strategy}'; "
                                      "DELETE FROM fx_rates WHERE currency='usd'")
        self.execute(f"""INSERT INTO fx_rates(currency,rub,source) VALUES('usd',80,'test') ON CONFLICT (currency) DO UPDATE SET rub=80;
            INSERT INTO ai_usage(purpose,strategy_id,requests,cost_usd,cost_rub) VALUES('decision','{strategy}',1,0.5,41.0),('tagging','{strategy}',1,0.1,NULL)""")
        result = self.service.enqueue({"id": str(uuid.uuid4()), "kind": "ai_budget", "strategy_id": None, "account_id": None, "payload": {"rub": "750"}})
        self.assertEqual(result["command"]["status"], "succeeded")
        data = self.service.data()
        self.assertEqual((float(data["ai_budget_rub"]), float(data["usd_rub"]["rub"])), (750.0, 80.0))
        today = {r["purpose"]: float(r["cost_rub"]) for r in data["ai_usage"]}
        self.assertGreaterEqual(today["decision"], 41.0)
        self.assertGreaterEqual(today["tagging"], 8.0, "an old dollar cost is converted at the current rate")


if __name__ == "__main__":
    unittest.main()
