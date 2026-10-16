#!/usr/bin/env python3
"""Local dashboard with durable commands executed by the account owner.

python3 scripts/dashboard/server.py start|status|stop
"""
from __future__ import annotations

import argparse
import datetime as dt
import fcntl
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import importlib.util
from http.cookies import SimpleCookie
import math
import os
from pathlib import Path
import re
import secrets
import signal
import subprocess
import sys
import threading
import time
from urllib.parse import parse_qs, urlsplit

ROOT = Path(__file__).resolve().parents[2]
ASSETS = Path(__file__).resolve().parent
WORK = Path(os.environ.get("CAMPAIGN_WORK_DIR", ROOT / "target/sandbox-campaign"))
SERVICE = "tinkoff-sandbox-dashboard"
UTC = dt.timezone.utc
CONTROL_SPEC = importlib.util.spec_from_file_location("dashboard_control", ASSETS / "control.py")
control_module = importlib.util.module_from_spec(CONTROL_SPEC)
CONTROL_SPEC.loader.exec_module(control_module)
CHART_INTERVALS = {"5s", "10s", "30s", "5m", "30m", "day"}


def read_json(path):
    try:
        return json.loads(path.read_text())
    except (OSError, ValueError):
        return None


def number(value):
    try:
        result = float(value)
        return result if math.isfinite(result) else None
    except (TypeError, ValueError):
        return None


def money(value):
    if not isinstance(value, dict):
        return None
    units, nano = number(value.get("units", 0)), number(value.get("nano", 0))
    return units + nano / 1e9 if units is not None and nano is not None else None


def pick(value, keys):
    return {key: value.get(key) for key in keys}


def modified(path):
    try:
        return dt.datetime.fromtimestamp(path.stat().st_mtime, UTC).isoformat()
    except OSError:
        return None


def alive(pid, marker):
    if not isinstance(pid, int) or pid <= 0:
        return False
    try:
        process = subprocess.run(["ps", "-p", str(pid), "-o", "command="], capture_output=True, text=True, timeout=2)
        return process.returncode == 0 and marker in process.stdout
    except (OSError, subprocess.SubprocessError):
        return False


def case_assets(work, case, instruments, benchmark):
    # The campaign generates literal TOML universe entries. Never infer a
    # strategy's actual universe from its name or expose the config itself.
    path = (work / case.get("config", "")).resolve()
    if not path.is_relative_to((work / "configs").resolve()):
        return []
    try:
        config = path.read_text()
    except OSError:
        return []
    ids = re.findall(r'\[\[universe\]\]\s*\nuid\s*=\s*"([^"]+)"', config)
    assets = [{"uid": uid, "ticker": instruments.get(uid, uid), "benchmark": False} for uid in ids]
    if benchmark and re.search(r'benchmark_uid\s*=\s*"' + re.escape(benchmark["uid"]) + r'"', config):
        assets.append({"uid": benchmark["uid"], "ticker": benchmark["ticker"], "benchmark": True})
    return assets


def market_snapshot(work, uid, interval):
    if interval not in CHART_INTERVALS:
        return None
    source = read_json(work / "reports/market-chart.json") or {}
    asset = source.get("assets", {}).get(uid)
    if asset is None:
        return None
    series = asset.get("series", {}).get(interval, {})
    return {"uid": uid, "ticker": asset.get("ticker"), "interval": interval,
            "observed_at": series.get("observed_at"), "error": bool(series.get("error")),
            "retry_at": series.get("retry_at"), "loading": not bool(series.get("observed_at")),
            "candles": [pick(c, ["time", "open", "high", "low", "close", "volume", "complete"])
                        for c in series.get("candles", [])]}


def snapshot(work):
    """Explicit output schema: never expose campaign.json, config, errors or logs."""
    campaign = read_json(work / "campaign.json") or {}
    database = read_json(work / "reports/database.json")
    broker = read_json(work / "reports/broker-latest.json")
    warnings = []
    if not campaign:
        warnings.append("Кампания ещё не подготовлена.")
    if database is None:
        warnings.append("Снимок состояния стратегий недоступен.")
    if broker is None:
        warnings.append("Снимок брокера недоступен.")
    database, broker = database or {}, broker or {}
    source_cases = list(campaign.get("cases", {}).values())
    instruments = {i["uid"]: i["ticker"] for i in campaign.get("instruments", [])}
    risk = {r["account_id"]: r for r in database.get("risk", [])}
    names = {c["strategy_id"]: c["kind"] for c in source_cases}
    accounts = {}
    for case in source_cases:
        account_id = case["account_id"]
        if account_id in accounts:
            continue
        candidates = [c for c in source_cases if c["account_id"] == account_id]
        data = next((broker.get("accounts", {}).get(c["kind"]) for c in candidates
                     if "portfolio" in broker.get("accounts", {}).get(c["kind"], {})
                     and broker["accounts"][c["kind"]].get("account_id") == account_id), {})
        portfolio = data.get("portfolio", {})
        current_risk = risk.get(account_id, {})
        positions = []
        cash = None
        for position in portfolio.get("positions", []):
            quantity = money(position.get("quantity"))
            if position.get("figi") == "RUB000UTSTOM" and position.get("instrumentType") == "currency":
                cash = quantity
                continue
            if quantity == 0:
                continue
            uid = position.get("instrumentUid")
            positions.append({"instrument_id": uid, "instrument": instruments.get(uid, position.get("ticker") or uid),
                              "quantity": quantity, "lots": money(position.get("quantityLots")),
                              "price": money(position.get("currentPrice")),
                              "currency": position.get("currentPrice", {}).get("currency"),
                              "expected_yield": money(position.get("expectedYield"))})
        active = [{"id": o.get("orderId"), "instrument_id": o.get("instrumentUid"), "instrument": instruments.get(o.get("instrumentUid"), o.get("instrumentUid")),
                   "side": o.get("direction"), "status": o.get("executionReportStatus"),
                   "lots": number(o.get("lotsRequested")), "executed": number(o.get("lotsExecuted")),
                   "time": o.get("orderDate")}
                  for o in data.get("orders", {}).get("orders", [])]
        operations = [{"id": o.get("id"), "time": o.get("date"), "type": o.get("type"),
                       "name": o.get("name"), "instrument": instruments.get(o.get("instrumentUid"), o.get("ticker")),
                       "payment": money(o.get("payment")), "currency": o.get("payment", {}).get("currency"),
                       "commission": money(o.get("commission")), "quantity": number(o.get("quantityDone")),
                       "status": o.get("state")}
                      for o in data.get("operations", [])]
        accounts[account_id] = {"id": account_id, "cases": [c["kind"] for c in candidates],
                                "owner": names.get(current_risk.get("strategy_id")),
                                "equity": money(portfolio.get("totalAmountPortfolio")), "cash": cash,
                                "available": bool(data), "positions": positions,
                                "active_orders": active, "operations": operations}
    cases = []
    for case in source_cases:
        current = risk.get(case["account_id"], {})
        owned = current.get("strategy_id") == case["strategy_id"]
        risk_state = current.get("state", {}) if owned else {}
        lifecycle = case.get("lifecycle_check", {})
        sessions = campaign.get("sessions", [])
        first = 0 if case.get("cohort", 0) == 0 else case["cohort"] + 1
        opening = sessions[first].get("startTime") if first < len(sessions) else None
        launch_at = (dt.datetime.fromisoformat(opening.replace("Z", "+00:00")) - dt.timedelta(hours=1)).isoformat() if opening else None
        row = pick(case, ["kind", "strategy_id", "account_id", "state", "cohort", "predecessor",
                          "natural_decision_count", "natural_target_count", "nonzero_target_count",
                          "strategy_filled_intents", "failed_batches", "preflight_result", "readiness_observed"])
        row.update(owns_account=owned, running=alive(case.get("pid"), str(work / case["config"])),
                   launch_at=launch_at, opening_at=opening,
                   assets=case_assets(work, case, instruments, campaign.get("benchmark")),
                   restart=case.get("restart_result") or ("passed" if lifecycle.get("risk_epoch_preserved") else None),
                   lease=case.get("lease_check") or lifecycle.get("lease_check"),
                   risk={"saved": owned, "blocked": bool(risk_state.get("blocked")),
                         "budget": number(risk_state.get("budget_rub")), "epoch": current.get("epoch") if owned else None})
        cases.append(row)
    orders = []
    requests = {}
    for intent in database.get("intent_audit", []):
        row = pick(intent, ["id", "request_id", "account_id", "strategy_id", "side", "status", "lots",
                            "lots_executed", "exchange_order_id", "created_at", "updated_at", "instrument_id",
                            "limit_price", "target_lots", "kind", "batch_id"])
        row.update(strategy=names.get(intent.get("strategy_id"), intent.get("strategy_id")),
                   instrument=instruments.get(intent.get("instrument_id"), intent.get("instrument_id")),
                   maintenance=str(intent.get("event_id", "")).startswith(("cleanup:", "risk:")),
                   manual=str(intent.get("event_id", "")).startswith("manual:"))
        orders.append(row)
        requests[intent.get("request_id")] = row
    fills = []
    for fill in database.get("fills", []):
        intent = requests.get(fill.get("request_id"), {})
        row = pick(fill, ["trade_id", "request_id", "lots", "price", "executed_at"])
        row.update(strategy=intent.get("strategy"), account_id=intent.get("account_id"),
                   instrument=intent.get("instrument"), instrument_id=intent.get("instrument_id"),
                   side=intent.get("side"), maintenance=intent.get("maintenance"), manual=intent.get("manual"))
        fills.append(row)
    targets = []
    for batch in database.get("batches", []):
        target = batch.get("target") or {}
        targets.append({"strategy": names.get(batch.get("strategy_id")), "batch_id": target.get("batch_id"),
                        "status": batch.get("status"), "generated_at": target.get("generated_at"),
                        "valid_until": target.get("valid_until"),
                        "maintenance": str(batch.get("event_id", "")).startswith("cleanup:"),
                        "weights": [{"instrument_id": w.get("instrument_id"), "weight": number(w.get("weight"))}
                                    for w in target.get("weights", [])]})
    sessions = [pick(day, ["date", "startTime", "endTime"]) for day in campaign.get("sessions", [])]
    return {"campaign": {"id": campaign.get("id"), "phase": campaign.get("phase"), "sessions": sessions,
                          "supervisor_running": alive(campaign.get("supervisor_pid"), "sandbox-campaign.py supervise")},
            "updated": {"broker": broker.get("observed_at"), "database": modified(work / "reports/database.json"),
                        "campaign": modified(work / "campaign.json")},
            "cases": cases, "accounts": list(accounts.values()), "orders": orders, "fills": fills, "targets": targets,
            "warnings": warnings}


class Feed:
    """Samples broker snapshots independently; never writes campaign state."""
    def __init__(self, work, control=None):
        self.work = work
        self.history_path = work / "dashboard-equity.jsonl"
        self.history = []
        if self.history_path.exists():
            for line in self.history_path.read_text().splitlines():
                try:
                    entry = json.loads(line)
                    if isinstance(entry.get("accounts"), dict) and entry.get("time"):
                        self.history.append(entry)
                except ValueError:
                    pass
        self.lock = threading.Lock()
        self.current = None
        self.control = control

    def update(self):
        data = snapshot(self.work)
        if self.control:
            try:
                current_control = self.control.data()
                controls = {row["strategy_id"]: row for row in current_control["cases"]}
                catalog = {row["uid"]: row for row in current_control["catalog"]}
                data["control"] = current_control
                catalog_path = self.work / "dashboard-catalog.json"
                catalog_temp = catalog_path.with_suffix(".tmp")
                catalog_temp.write_text(json.dumps(current_control["catalog"], ensure_ascii=False))
                catalog_temp.replace(catalog_path)
                for row in data["cases"]:
                    row["control"] = controls.get(row["strategy_id"], {"enabled": current_control["mode"] == "campaign", "execution_mode": "auto"})
                    for asset in row["assets"]:
                        asset["ticker"] = catalog.get(asset["uid"], {}).get("ticker", asset["ticker"])
                approvals = {row["batch_id"]: row for row in current_control["batches"]}
                for target in data["targets"]:
                    target["execution_control"] = approvals.get(target["batch_id"])
            except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError):
                data["control"] = {"available": False, "message": "Управление недоступно: проверьте подключение и миграции БД"}
        stamp = data["updated"]["broker"]
        values = {a["id"]: a["equity"] for a in data["accounts"] if a["equity"] is not None}
        if stamp and values and (not self.history or stamp > self.history[-1]["time"]):
            # A partial account read stays partial, never an artificial portfolio loss.
            point = {"time": stamp, "accounts": values, "complete": len(values) == len(data["accounts"])}
            self.work.mkdir(parents=True, exist_ok=True)
            with self.history_path.open("a") as output:
                output.write(json.dumps(point, allow_nan=False) + "\n")
            self.history_path.chmod(0o600)
            self.history.append(point)
        data["history"] = self.history[-6000:]
        with self.lock:
            self.current = data

    def data(self):
        with self.lock:
            return json.dumps(self.current, ensure_ascii=False, allow_nan=False).encode()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def valid_host(self):
        port = str(self.server.server_port)
        ports = {port, os.environ.get("DASHBOARD_PUBLIC_PORT", port)}
        return self.headers.get("Host") in {f"{host}:{p}" for host in ["127.0.0.1", "localhost"] for p in ports}

    def respond(self, data, status=200, mime="application/json; charset=utf-8", cookie=None):
        if not isinstance(data, bytes):
            data = json.dumps(data, ensure_ascii=False, allow_nan=False).encode()
        self.send_response(status)
        self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Content-Security-Policy", "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'")
        if cookie:
            self.send_header("Set-Cookie", cookie)
        self.end_headers()
        try:
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def session(self, create=False):
        if not hasattr(self.server, "sessions"):
            self.server.sessions = {}
            self.server.sessions_lock = threading.Lock()
        cookie = SimpleCookie()
        try:
            cookie.load(self.headers.get("Cookie", ""))
        except Exception:
            return None
        entry = cookie.get("sandbox_session")
        key = entry.value if entry else None
        with self.server.sessions_lock:
            current = time.monotonic()
            self.server.sessions = {k: v for k, v in self.server.sessions.items() if v["expires"] > current}
            value = self.server.sessions.get(key)
            if not value and create:
                if len(self.server.sessions) >= 256:
                    self.server.sessions.pop(next(iter(self.server.sessions)))
                key = secrets.token_urlsafe(32)
                value = {"csrf_token": secrets.token_urlsafe(32), "expires": current + 43200}
                self.server.sessions[key] = value
            return (key, value) if value else None

    def require_control(self):
        control = getattr(self.server, "control", None)
        if control is None:
            raise control_module.ClientError("Управление недоступно: проверьте подключение и миграции БД", 503)
        return control

    def subscribe_chart(self, uid, interval):
        if not hasattr(self.server, "chart_lock"):
            self.server.chart_lock = threading.Lock()
        work = self.server.feed.work
        with self.server.chart_lock:
            path = work / "chart-subscriptions.json"
            values = read_json(path) or {}
            current = time.time()
            values = {k: v for k, v in values.items() if v.get("expires_at", 0) > current}
            # Bound the shared collector's work even if many browser tabs are open.
            if len(values) < 12 or uid + ":" + interval in values:
                values[uid + ":" + interval] = {"uid": uid, "interval": interval, "expires_at": current + 25}
            temporary = path.with_suffix(".tmp")
            temporary.write_text(json.dumps(values))
            temporary.replace(path)

    def do_GET(self):
        if not self.valid_host():
            self.send_error(403)
            return
        route = urlsplit(self.path).path
        assets = {"/": ("index.html", "text/html; charset=utf-8"),
                  "/app.js": ("app.js", "text/javascript; charset=utf-8"),
                  "/detail.js": ("detail.js", "text/javascript; charset=utf-8"),
                  "/controls.js": ("controls.js", "text/javascript; charset=utf-8"),
                  "/style.css": ("style.css", "text/css; charset=utf-8")}
        try:
            if route == "/api/session":
                key, session = self.session(create=True)
                self.respond({"csrf_token": session["csrf_token"]}, cookie=f"sandbox_session={key}; Path=/; HttpOnly; SameSite=Strict; Max-Age=43200")
            elif route == "/api/snapshot":
                self.respond(self.server.feed.data())
            elif route == "/api/control":
                self.respond(self.require_control().data())
            elif route == "/api/ranking":
                self.respond(self.require_control().data()["ranking"])
            elif route.startswith("/api/commands/"):
                self.respond(self.require_control().get_command(route.rsplit("/", 1)[-1]))
            elif route == "/api/instruments/search":
                query = parse_qs(urlsplit(self.path).query).get("q", [""])[0]
                self.respond(self.require_control().search(query))
            elif route == "/api/market":
                query = parse_qs(urlsplit(self.path).query)
                uid, interval = query.get("instrument", [""])[0], query.get("interval", [""])[0]
                current = json.loads(self.server.feed.data()) or {}
                allowed = {a["uid"] for c in current.get("cases", []) for a in c.get("assets", [])}
                allowed |= {a["uid"] for a in current.get("control", {}).get("catalog", [])}
                if uid not in allowed or interval not in CHART_INTERVALS:
                    self.send_error(404)
                    return
                self.subscribe_chart(uid, interval)
                series = market_snapshot(self.server.feed.work, uid, interval)
                self.respond(series or {"uid": uid, "interval": interval, "candles": [], "loading": True, "error": False, "observed_at": None})
            elif route == "/api/health":
                self.respond({"service": SERVICE, "pid": os.getpid()})
            elif route in assets:
                filename, mime = assets[route]
                self.respond((ASSETS / filename).read_bytes(), mime=mime)
            else:
                self.send_error(404)
        except control_module.ClientError as error:
            self.respond({"error": str(error)}, error.status)
        except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError):
            self.respond({"error": "Сервис временно недоступен. Проверьте соединение и миграции БД."}, 503)

    def do_POST(self):
        if not self.valid_host():
            self.send_error(403)
            return
        if urlsplit(self.path).path != "/api/commands":
            self.send_error(404)
            return
        session = self.session()
        if (not session or self.headers.get("Origin") != "http://" + self.headers.get("Host", "")
                or not secrets.compare_digest(self.headers.get("X-CSRF-Token", ""), session[1]["csrf_token"])):
            self.respond({"error": "Обновите страницу: локальная сессия недействительна"}, 403)
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if self.headers.get("Content-Type", "").split(";", 1)[0] != "application/json" or not 0 < length <= 16384:
                raise control_module.ClientError("Ожидается JSON, не более 16 КБ")
            body = json.loads(self.rfile.read(length))
            self.respond(self.require_control().enqueue(body), 202)
        except control_module.ClientError as error:
            self.respond({"error": str(error)}, error.status)
        except (ValueError, UnicodeError):
            self.respond({"error": "Некорректный JSON"}, 400)
        except (OSError, KeyError, RuntimeError, subprocess.SubprocessError):
            self.respond({"error": "Команда не подтверждена. Проверьте её статус перед повтором."}, 503)


def serve(port):
    WORK.mkdir(parents=True, exist_ok=True)
    lock = (WORK / "dashboard.lock").open("a")
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    control = control_module.Control(WORK)
    feed = Feed(WORK, control)
    feed.update()
    server = ThreadingHTTPServer((os.environ.get("DASHBOARD_BIND", "127.0.0.1"), port), Handler)
    server.feed = feed
    server.control = control
    server.sessions = {}
    server.sessions_lock = threading.Lock()
    server.chart_lock = threading.Lock()
    state = {"pid": os.getpid(), "port": server.server_port}
    (WORK / "dashboard.json").write_text(json.dumps(state))
    stopped = threading.Event()
    with (WORK / "market-feed.log").open("a") as log:
        market = subprocess.Popen([sys.executable, str(ASSETS / "market_feed.py"), "--parent-pid", str(os.getpid())],
                                  stdout=log, stderr=subprocess.STDOUT)

    def sample():
        while not stopped.wait(10):
            try:
                feed.update()
            except (OSError, ValueError, KeyError, TypeError):
                # Keep the last valid snapshot; the UI will show its increasing age.
                print("Dashboard snapshot unavailable; keeping previous data", flush=True)

    def shutdown(*_):
        stopped.set()
        threading.Thread(target=server.shutdown, daemon=True).start()

    signal.signal(signal.SIGTERM, shutdown)
    signal.signal(signal.SIGINT, shutdown)
    threading.Thread(target=sample, daemon=True).start()
    print(f"Dashboard: http://127.0.0.1:{server.server_port}", flush=True)
    try:
        server.serve_forever()
    finally:
        stopped.set()
        server.server_close()
        market.terminate()
        market.wait(timeout=5)


def health(state):
    if not state:
        return False
    connection = http.client.HTTPConnection("127.0.0.1", state["port"], timeout=2)
    try:
        connection.request("GET", "/api/health")
        response = connection.getresponse()
        return response.status == 200 and json.loads(response.read()) == {"service": SERVICE, "pid": state["pid"]}
    except (OSError, ValueError, http.client.HTTPException):
        return False
    finally:
        connection.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["start", "serve", "status", "stop"])
    parser.add_argument("--port", type=int, default=8765)
    args = parser.parse_args()
    os.umask(0o077)
    if args.action == "serve":
        serve(args.port)
        return
    state = read_json(WORK / "dashboard.json")
    running = health(state)
    if args.action == "stop":
        if running:
            os.kill(state["pid"], signal.SIGTERM)
            for _ in range(30):
                if not health(state):
                    break
                time.sleep(0.1)
        print("Dashboard stopped; campaign continues unchanged")
    elif args.action == "status" or running:
        print(f"http://127.0.0.1:{state['port']} (PID {state['pid']})" if running else "Dashboard is not running")
    else:
        WORK.mkdir(parents=True, exist_ok=True)
        with (WORK / "dashboard.log").open("a") as log:
            child = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), "serve", "--port", str(args.port)],
                                     stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        for _ in range(60):
            if child.poll() is not None:
                raise RuntimeError("Dashboard failed to start; see target/sandbox-campaign/dashboard.log")
            state = read_json(WORK / "dashboard.json")
            if health(state):
                print(f"http://127.0.0.1:{state['port']} (PID {state['pid']})")
                return
            time.sleep(0.25)
        raise RuntimeError("Dashboard startup timed out; see target/sandbox-campaign/dashboard.log")


if __name__ == "__main__":
    main()
