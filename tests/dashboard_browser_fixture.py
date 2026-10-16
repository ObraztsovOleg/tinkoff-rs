"""Isolated browser fixture. No token, database, broker or live campaign access."""
import datetime as dt
import importlib.util
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
from http.server import ThreadingHTTPServer

spec = importlib.util.spec_from_file_location("dashboard", Path(__file__).resolve().parents[1] / "scripts/dashboard/server.py")
dashboard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(dashboard)
UID = "11111111-1111-4111-8111-111111111111"
EXTRA = "22222222-2222-4222-8222-222222222222"
PLAN = "33333333-3333-4333-8333-333333333333"
now = dt.datetime.now(dt.timezone.utc)
stamp = now.isoformat()
asset = {"uid": UID, "ticker": "SBER", "name": "Тестовая акция", "lot": 10, "benchmark": False}
extra = {"uid": EXTRA, "ticker": "TEST", "name": "Добавленная акция", "lot": 1, "benchmark": False}
case = {"kind": "pairs-trading", "strategy_id": "fixture", "account_id": "fixture-account", "state": "running", "running": True, "cohort": 0, "assets": [asset], "risk": {"budget": 1000000, "saved": True}, "owns_account": True, "readiness_observed": True, "launch_at": stamp, "opening_at": stamp}
account = {"id": "fixture-account", "owner": "pairs-trading", "cases": ["pairs-trading"], "available": True, "equity": 1010000, "cash": 1000000, "positions": [], "active_orders": [], "operations": []}
control = {"available": True, "mode": "campaign", "cases": [{"strategy_id": "fixture", "account_id": "fixture-account", "enabled": True, "execution_mode": "confirm", "execution_budget": 1000000}], "commands": [], "catalog": [], "batches": [{"batch_id": PLAN, "status": "awaiting", "revision": 0}], "ranking": {"clean": [], "mixed": [], "unranked": [], "history": []}}
run = {"id": "fixture-run", "strategy_id": "fixture", "strategy": "pairs-trading", "classification": "clean", "started_at": stamp, "principal": 1000000, "pnl_rub": 10000, "return_percent": 1, "max_drawdown_percent": .25, "fills": 2, "rank": 1, "ranked": True}
control["ranking"]["clean"] = [run]
control["ranking"]["history"] = [run]
snapshot = {"campaign": {"id": "BROWSER FIXTURE — NO TRADING", "phase": "observing", "supervisor_running": True, "sessions": []}, "cases": [case], "accounts": [account], "orders": [], "fills": [], "targets": [{"strategy": "pairs-trading", "batch_id": PLAN, "status": "planned", "generated_at": stamp, "valid_until": (now + dt.timedelta(hours=1)).isoformat(), "weights": []}], "warnings": [], "updated": {"broker": stamp, "database": stamp, "campaign": stamp}, "history": [], "control": control}


class FakeControl:
    def data(self):
        return control

    def search(self, _):
        return {"instruments": [extra]}

    def get_command(self, command_id):
        return {"command": next(c for c in control["commands"] if c["id"] == command_id)}

    def enqueue(self, value):
        command = dashboard.control_module.validate(value, {"cases": {"fixture": case}})
        existing = next((c for c in control["commands"] if c["id"] == command["id"]), None)
        if existing:
            return {"command": existing}
        kind, payload = command["kind"], command["payload"]
        if kind in {"pause", "manual_order"}:
            control["cases"][0]["enabled"] = False
        elif kind == "resume":
            control["cases"][0]["enabled"] = True
        elif kind == "mode":
            control["mode"] = payload["mode"]
        elif kind == "catalog_add":
            control["catalog"] = [extra]
        elif kind == "budget":
            control["cases"][0]["execution_budget"] = float(payload["amount"])
        command.update(status="succeeded", result={"message": "Fixture only"}, created_at=stamp, updated_at=stamp)
        control["commands"].insert(0, command)
        return {"command": command}


with tempfile.TemporaryDirectory(prefix="dashboard-browser-") as temporary:
    work = Path(temporary)
    (work / "reports").mkdir()
    assets = {}
    for a in [asset, extra]:
        series = {}
        for interval, seconds in {"5s": 5, "10s": 10, "30s": 30, "5m": 300, "30m": 1800, "day": 86400}.items():
            candles = [{"time": (now-dt.timedelta(seconds=seconds*(60-i))).isoformat(), "open": 100+i/10, "high": 101+i/10, "low": 99+i/10, "close": 100.5+i/10, "volume": 10, "complete": True} for i in range(60)]
            series[interval] = {"candles": candles, "observed_at": stamp, "error": False}
        assets[a["uid"]] = {"ticker": a["ticker"], "series": series}
    (work / "reports/market-chart.json").write_text(json.dumps({"assets": assets}))
    server = ThreadingHTTPServer(("127.0.0.1", 18766), dashboard.Handler)
    server.feed = SimpleNamespace(work=work, data=lambda: json.dumps(snapshot).encode())
    server.control = FakeControl()
    print("Browser fixture listening on 18766; no broker or database", flush=True)
    server.serve_forever()
