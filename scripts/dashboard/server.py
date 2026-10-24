#!/usr/bin/env python3
"""Local dashboard with durable commands executed by the account owner.

python3 scripts/dashboard/server.py start|status|stop
"""
from __future__ import annotations

import argparse
import datetime as dt
import fcntl
import getpass
import hashlib
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
# Runtime logs per strategy: main process, preflight warmup and account cleanup.
LOG_SOURCES = {"main": "", "preflight": ".preflight", "cleanup": ".cleanup"}
LOG_TAIL_BYTES = 512 * 1024
LOG_LINE = re.compile(r"^(?P<time>\d{4}-\d\d-\d\dT[\d:.]+Z)\s+(?P<level>TRACE|DEBUG|INFO|WARN|ERROR)\s+(?P<target>[\w:]+):\s?(?P<body>.*)$")
LOG_FIELD = re.compile(r"(?:^|\s)([a-z_][a-z0-9_.]*)=")
ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")
# Tokens are never logged by design; mask them anyway in case an upstream error echoes one.
SECRETS = [(re.compile(r"\bt\.[A-Za-z0-9_-]{20,}"), "t.***"),
           (re.compile(r"(?i)\b(bearer\s+)[^\s\"',]+"), r"\1***"),
           (re.compile(r"(postgres(?:ql)?://[^:/\s@]+:)[^@\s]+@"), r"\1***@")]


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


def account_value(portfolio):
    """Account value without the futures notional T-Invest adds to the portfolio
    total: a futures result is already in cash as settled variation margin."""
    total = money(portfolio.get("totalAmountPortfolio"))
    futures = money(portfolio.get("totalAmountFutures")) or 0
    return None if total is None else total - futures


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


def uses_ai_model(work, case):
    """True when the strategy config contains an ai-model component."""
    path = (work / case.get("config", "")).resolve()
    if not path.is_relative_to((work / "configs").resolve()):
        return False
    try:
        return 'kind = "ai-model"' in path.read_text()
    except OSError:
        return False


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


def redact(text):
    text = ANSI.sub("", text)
    for pattern, replacement in SECRETS:
        text = pattern.sub(replacement, text)
    return text


def parse_log_line(line):
    """tracing fmt line -> time, level, target, message and key=value fields."""
    line = redact(line)[:4000]
    match = LOG_LINE.match(line)
    if not match:
        return {"time": None, "level": "RAW", "target": None, "message": line, "fields": {}}
    body = match["body"]
    keys = list(LOG_FIELD.finditer(body))
    fields = {}
    for index, key in enumerate(keys):
        end = keys[index + 1].start() if index + 1 < len(keys) else len(body)
        fields[key[1]] = body[key.end():end].strip()
    message = body[:keys[0].start()].strip() if keys else body.strip()
    return {"time": match["time"], "level": match["level"], "target": match["target"], "message": message, "fields": fields}


def strategy_log(work, kind, source="main", limit=300):
    """Tail of one strategy log, parsed and redacted. Reads at most LOG_TAIL_BYTES."""
    if source not in LOG_SOURCES or not re.fullmatch(r"[a-z0-9][a-z0-9-]*", kind or ""):
        raise ValueError("unknown log")
    directory = work / "logs"
    available = [name for name, suffix in LOG_SOURCES.items() if (directory / f"{kind}{suffix}.log").is_file()]
    path = directory / f"{kind}{LOG_SOURCES[source]}.log"
    result = {"strategy": kind, "source": source, "sources": available, "exists": path.is_file(),
              "size": 0, "modified": None, "truncated": False, "entries": []}
    if not result["exists"]:
        return result
    with path.open("rb") as log:
        log.seek(0, os.SEEK_END)
        size = log.tell()
        start = max(0, size - LOG_TAIL_BYTES)
        log.seek(start)
        chunk = log.read()
    if start:
        # Drop the partial first line of a tail read.
        chunk = chunk.split(b"\n", 1)[1] if b"\n" in chunk else b""
    lines = [line for line in chunk.decode(errors="replace").splitlines() if line.strip()]
    limit = max(1, min(int(limit), 1000))
    result.update(size=size, modified=modified(path), truncated=bool(start) or len(lines) > limit,
                  entries=[parse_log_line(line) for line in lines[-limit:]])
    return result


def storage(work, campaign):
    """Disk space, nightly maintenance and backups, without paths of the host."""
    disk = campaign.get("disk") or {}
    directory = work / "backups"
    files = sorted(directory.glob("*.dump"), key=lambda f: f.stat().st_mtime, reverse=True)[:5] if directory.is_dir() else []
    return {"disk": dict(pick(disk, ["level", "free_bytes", "path", "database_bytes", "checked_at"]),
                         readings=[pick(r, ["path", "free_bytes", "total_bytes"]) for r in disk.get("readings", [])]),
            "maintenance": pick(campaign.get("maintenance") or {}, ["day", "at", "deleted", "rotated_logs", "database_bytes", "seconds", "error", "retry_at"]),
            "backups": {"status": read_json(directory / "status.json"),
                        # Off-site copies: when and where to, never the credentials.
                        "offsite": pick(read_json(directory / "offsite.json") or {}, ["ok", "at", "latest", "removed", "bucket", "prefix", "error"]),
                        "files": [{"name": f.name, "bytes": f.stat().st_size,
                                   "at": dt.datetime.fromtimestamp(f.stat().st_mtime, UTC).isoformat()} for f in files]}}


def snapshot(work):
    """Explicit output schema: never expose campaign.json, config or errors. Logs have their own redacted endpoint."""
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
                                "contour": "production" if any(c.get("contour") == "production" for c in candidates) else "sandbox",
                                "owner": names.get(current_risk.get("strategy_id")),
                                "equity": account_value(portfolio), "cash": cash,
                                "available": bool(data), "positions": positions,
                                "active_orders": active, "operations": operations}
    cases = []
    for case in source_cases:
        current = risk.get(case["account_id"], {})
        owned = current.get("strategy_id") == case["strategy_id"]
        risk_state = current.get("state", {}) if owned else {}
        lifecycle = case.get("lifecycle_check", {})
        schedule = case.get("schedule") or {}
        windows = [pick(w, ["begin", "open", "end", "days"]) for w in schedule.get("windows", [])]
        # The current or next operator window; past windows only matter for history.
        upcoming = next((w for w in windows if w.get("end") and w["end"] > dt.datetime.now(UTC).isoformat()), None)
        row = pick(case, ["kind", "strategy_id", "account_id", "state", "cohort", "predecessor", "title",
                          "natural_decision_count", "natural_target_count", "nonzero_target_count",
                          "strategy_filled_intents", "failed_batches", "preflight_result", "readiness_observed",
                          "service_only", "last_started", "retry_at", "launches", "source", "promoted_at"])
        # Real accounts are marked everywhere; the UI never offers sandbox-only actions there.
        row["contour"] = "production" if case.get("contour") == "production" else "sandbox"
        row["ai_model"] = uses_ai_model(work, case)
        if row["contour"] == "production":
            row["risk_limits"] = pick(case.get("risk_limits") or {}, ["budget", "daily_loss_percent", "max_drawdown_percent"])
        row.update(owns_account=owned, running=alive(case.get("pid"), str(work / case["config"])),
                   launch_at=upcoming and upcoming["begin"], opening_at=upcoming and upcoming["open"],
                   window_end_at=upcoming and upcoming["end"],
                   schedule={"run_mode": schedule.get("run_mode"), "active": bool(schedule.get("active")),
                             "close_after": schedule.get("close_after"), "windows": windows},
                   blocks=[{"share": b.get("share"), "instruments": [{"uid": uid, "ticker": instruments.get(uid, uid)} for uid in b.get("instruments", [])],
                            "strategies": [pick(x, ["kind", "weight", "params"]) for x in b.get("strategies", [])]} for b in case.get("blocks", [])],
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
                        "status": batch.get("status"), "reason": batch.get("reason"),
                        "generated_at": target.get("generated_at"),
                        "valid_until": target.get("valid_until"),
                        "maintenance": str(batch.get("event_id", "")).startswith("cleanup:"),
                        "weights": [{"instrument_id": w.get("instrument_id"), "weight": number(w.get("weight"))}
                                    for w in target.get("weights", [])]})
    sessions = [pick(day, ["date", "startTime", "endTime"]) for day in campaign.get("sessions", [])]
    known = {d["date"][:10]: dict(pick(d, ["startTime", "endTime"]), date=d["date"][:10], isTradingDay=True) for d in campaign.get("sessions", []) if d.get("date")}
    known.update({d["date"]: pick(d, ["date", "isTradingDay", "startTime", "endTime"]) for d in campaign.get("calendar", []) if d.get("date")})
    suspensions = [pick(x, ["from", "to"]) for x in campaign.get("suspensions", [])][-10:]
    return {"campaign": {"id": campaign.get("id"), "phase": campaign.get("phase"), "sessions": sessions, "suspensions": suspensions,
                         "calendar": sorted(known.values(), key=lambda d: d["date"]),
                          "supervisor_running": alive(campaign.get("supervisor_pid"), "sandbox-campaign.py supervise")},
            "updated": {"broker": broker.get("observed_at"), "database": modified(work / "reports/database.json"),
                        "campaign": modified(work / "campaign.json")},
            "cases": cases, "accounts": list(accounts.values()), "orders": orders, "fills": fills, "targets": targets,
            "storage": storage(work, campaign), "warnings": warnings}


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
                  "/logs.js": ("logs.js", "text/javascript; charset=utf-8"),
                  "/live.js": ("live.js", "text/javascript; charset=utf-8"),
                  "/nav.js": ("nav.js", "text/javascript; charset=utf-8"),
                  "/ai.js": ("ai.js", "text/javascript; charset=utf-8"),
                  "/storage.js": ("storage.js", "text/javascript; charset=utf-8"),
                  "/documents.js": ("documents.js", "text/javascript; charset=utf-8"),
                  "/exposures.js": ("exposures.js", "text/javascript; charset=utf-8"),
                  "/portfolios.js": ("portfolios.js", "text/javascript; charset=utf-8"),
                  "/aiboard.js": ("aiboard.js", "text/javascript; charset=utf-8"),
                  "/costs.js": ("costs.js", "text/javascript; charset=utf-8"),
                  "/backtest.js": ("backtest.js", "text/javascript; charset=utf-8"),
                  "/theme.js": ("theme.js", "text/javascript; charset=utf-8"),
                  "/dark.css": ("dark.css", "text/css; charset=utf-8"),
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
            elif route == "/api/documents":
                query = {k: v[0] for k, v in parse_qs(urlsplit(self.path).query).items()}
                self.respond(self.require_control().documents(query))
            elif route == "/api/document":
                document_id = parse_qs(urlsplit(self.path).query).get("id", [""])[0]
                self.respond(self.require_control().document(document_id))
            elif route == "/api/instruments/search":
                params = parse_qs(urlsplit(self.path).query)
                self.respond(self.require_control().search(params.get("q", [""])[0], params.get("kind", [""])[0]))
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
            elif route == "/api/logs":
                query = parse_qs(urlsplit(self.path).query)
                kind, source = query.get("strategy", [""])[0], query.get("source", ["main"])[0]
                current = json.loads(self.server.feed.data()) or {}
                # Only campaign strategies: the name becomes part of a file path.
                if kind not in {c["kind"] for c in current.get("cases", [])} or source not in LOG_SOURCES:
                    self.send_error(404)
                    return
                limit = query.get("limit", ["300"])[0]
                self.respond(strategy_log(self.server.feed.work, kind, source, int(limit) if limit.isdigit() else 300))
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

    def pin_accepted(self, body, session):
        """Real-money commands need the operator PIN once it is set; an accepted PIN
        unlocks the session for a while. Answers 401 itself when it is missing or wrong."""
        pin = getattr(self.server, "pin", None)
        if pin is None or not pin.configured() or session.get("pin_until", 0) > time.monotonic():
            return True
        if not control_module.pin_required(body, self.require_control().state()):
            return True
        entered = self.headers.get("X-Operator-Pin", "")
        if not entered:
            self.respond({"error": "Действие с реальным счётом: введите PIN оператора", "pin_required": True}, 401)
            return False
        accepted, message = pin.check(entered)
        if not accepted:
            self.respond({"error": message, "pin_required": True}, 401)
            return False
        session["pin_until"] = time.monotonic() + PIN_UNLOCK_SECONDS
        return True

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
            if not self.pin_accepted(body, session[1]):
                return
            self.respond(self.require_control().enqueue(body), 202)
        except control_module.ClientError as error:
            self.respond({"error": str(error)}, error.status)
        except (ValueError, UnicodeError):
            self.respond({"error": "Некорректный JSON"}, 400)
        except (OSError, KeyError, RuntimeError, subprocess.SubprocessError):
            self.respond({"error": "Команда не подтверждена. Проверьте её статус перед повтором."}, 503)


class OperatorPin:
    """PIN of the operator for real-money commands: a PBKDF2 hash in
    <work>/operator-pin.json, set with `server.py set-pin`. Five wrong entries in
    a row lock it for 15 minutes. Without the file no PIN is asked."""
    ITERATIONS = 600_000
    ATTEMPTS = 5
    LOCKOUT = 900

    def __init__(self, path):
        self.path = Path(path)
        self.failures = 0
        self.locked_until = 0.0
        self.lock = threading.Lock()

    def stored(self):
        try:
            value = json.loads(self.path.read_text())
            return value if {"salt", "iterations", "hash"} <= set(value) else None
        except (OSError, ValueError):
            return None

    def configured(self):
        return self.stored() is not None

    @staticmethod
    def digest(pin, salt, iterations):
        return hashlib.pbkdf2_hmac("sha256", pin.encode(), bytes.fromhex(salt), iterations).hex()

    def set(self, pin):
        if not 4 <= len(pin) <= 64 or pin != pin.strip():
            raise ValueError("PIN: от 4 до 64 символов, без пробелов по краям")
        salt = secrets.token_hex(16)
        value = {"salt": salt, "iterations": self.ITERATIONS, "hash": self.digest(pin, salt, self.ITERATIONS),
                 "set_at": dt.datetime.now(dt.timezone.utc).isoformat()}
        temporary = self.path.with_suffix(".tmp")
        temporary.write_text(json.dumps(value))
        os.chmod(temporary, 0o600)
        temporary.replace(self.path)

    def check(self, pin):
        """(accepted, message for the operator)."""
        stored = self.stored()
        if stored is None:
            return True, ""
        with self.lock:
            wait = self.locked_until - time.monotonic()
            if wait > 0:
                return False, f"PIN заблокирован после {self.ATTEMPTS} ошибок: повторите через {math.ceil(wait / 60)} мин"
            if secrets.compare_digest(self.digest(str(pin), stored["salt"], int(stored["iterations"])), stored["hash"]):
                self.failures = 0
                return True, ""
            self.failures += 1
            if self.failures >= self.ATTEMPTS:
                self.failures = 0
                self.locked_until = time.monotonic() + self.LOCKOUT
                return False, f"Неверный PIN. Ввод заблокирован на {self.LOCKOUT // 60} минут"
            return False, f"Неверный PIN: осталось попыток {self.ATTEMPTS - self.failures}"


# An accepted PIN unlocks real-money commands of this browser session for 10 minutes.
PIN_UNLOCK_SECONDS = 600


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
    server.pin = OperatorPin(WORK / "operator-pin.json")
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
    parser.add_argument("action", choices=["start", "serve", "status", "stop", "set-pin", "remove-pin"])
    parser.add_argument("--port", type=int, default=8765)
    args = parser.parse_args()
    os.umask(0o077)
    if args.action in ("set-pin", "remove-pin"):
        pin = OperatorPin(WORK / "operator-pin.json")
        if args.action == "remove-pin":
            pin.path.unlink(missing_ok=True)
            print("PIN removed: real-account commands are confirmed by the account number only")
            return
        first = getpass.getpass("New operator PIN: ")
        if first != getpass.getpass("Repeat the PIN: "):
            raise SystemExit("The PINs differ; nothing was changed")
        try:
            pin.set(first)
        except ValueError as error:
            raise SystemExit(str(error)) from None
        print("PIN set: the dashboard asks for it before real-account commands")
        return
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
