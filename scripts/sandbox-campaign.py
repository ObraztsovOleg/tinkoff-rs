#!/usr/bin/env python3
"""Five real MOEX sessions. Credentials stay in process memory; artifacts are private.

python3 scripts/sandbox-campaign.py prepare|start|status|stop|park|archive
Internal `supervise` survives the invoking terminal; `stop` requests cleanup only.
`archive` moves a finished campaign aside; nothing (accounts, database, files) is deleted.
"""
from __future__ import annotations

import argparse
import contextlib
import datetime as dt
import fcntl
import json
import importlib.util
import os
from pathlib import Path
import re
import secrets
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import time
from types import SimpleNamespace
from urllib.parse import parse_qsl, unquote, urlsplit

ROOT = Path(__file__).resolve().parents[1]
_control_spec = importlib.util.spec_from_file_location("campaign_control", ROOT / "scripts/campaign_control.py")
control_plane = importlib.util.module_from_spec(_control_spec)
_control_spec.loader.exec_module(control_plane)
WORK = Path(os.environ.get("CAMPAIGN_WORK_DIR", ROOT / "target" / "sandbox-campaign"))
BINARY = Path(os.environ.get("TINKOFF_BOT_BINARY", ROOT / "target" / "debug" / "tinkoff-bot"))
UTC = dt.timezone.utc
COHORTS = [
    ["pairs-trading", "market-intraday-momentum", "intraday-same-clock", "weighted-ensemble"],
    ["moving-average-trend", "range-breakout", "time-series-momentum", "cross-sectional-momentum"],
    ["intermediate-momentum", "fifty-two-week-high", "residual-momentum", "short-term-reversal"],
    ["low-volatility", "betting-against-beta", "equal-risk-contribution", "volatility-managed"],
]
PREFERRED = "SBER GAZP LKOH GMKN NVTK ROSN TATN CHMF MTSS MAGN ALRS MOEX SNGS NLMK".split()


def now():
    return dt.datetime.now(UTC)


def timestamp(value):
    return dt.datetime.fromisoformat(value.replace("Z", "+00:00"))


def save(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")
    temporary.chmod(0o600)
    temporary.replace(path)


def load():
    return json.loads((WORK / "campaign.json").read_text())


def persist(state):
    save(WORK / "campaign.json", state)


def fish_literal(name):
    """A literal `set ... NAME value` from the user's fish config; the config is never executed."""
    with contextlib.suppress(OSError):
        for line in (Path.home() / ".config/fish/conf.d/env.fish").read_text().splitlines():
            if not line.lstrip().startswith("set "):
                continue
            with contextlib.suppress(ValueError):
                words = shlex.split(line, comments=True)
                if name in words:
                    index = words.index(name)
                    if len(words) == index + 2:
                        value = words[index + 1].rstrip(";")
                        if value and not any(c in value for c in "$()\n\r"):
                            return value
    return None


def token():
    if os.environ.get("TINKOFF_TOKEN_FILE"):
        value = Path(os.environ["TINKOFF_TOKEN_FILE"]).read_text().strip()
        if not value or any(c in value for c in "\n\r"):
            raise RuntimeError("Sandbox token secret must contain one nonempty line")
        return value
    value = fish_literal("TINKOFF_TOKEN_SANDBOX")
    if value:
        return value
    raise RuntimeError("TINKOFF_TOKEN_SANDBOX must be a nonempty literal fish assignment")


def live_token():
    """Real-account token: TINKOFF_TOKEN_PRODUCTION from the environment, the Compose
    secret file or a literal fish assignment. Never argv, YAML or logs."""
    value = os.environ.get("TINKOFF_TOKEN_PRODUCTION", "").strip()
    if not value and os.environ.get("TINKOFF_TOKEN_PRODUCTION_FILE"):
        try:
            value = Path(os.environ["TINKOFF_TOKEN_PRODUCTION_FILE"]).read_text().strip()
        except OSError:
            raise RuntimeError("Real-account token secret is unreadable") from None
    if not value and not os.environ.get("TINKOFF_TOKEN_PRODUCTION_FILE"):
        value = fish_literal("TINKOFF_TOKEN_PRODUCTION") or ""
    if not value or any(c in value for c in "\n\r"):
        raise RuntimeError("Real-account token is not connected")
    return value


def is_live(case):
    return bool(case) and case.get("contour") == "production"


def live_status(state):
    """Whether real accounts can trade: a token and no emergency stop. Never the secret itself."""
    try:
        live_token()
        connected = True
    except RuntimeError:
        connected = False
    halted = bool((state.get("live") or {}).get("halted"))
    return {"token": connected, "halted": halted, "ready": connected and not halted}


SANDBOX_REST = "https://sandbox-invest-public-api.tbank.ru/rest/tinkoff.public.invest.api.contract.v1."
PRODUCTION_REST = "https://invest-public-api.tbank.ru/rest/tinkoff.public.invest.api.contract.v1."
# The supervisor only reads real accounts; orders go through the bot's live permit.
LIVE_READ_METHODS = {"UsersService/GetAccounts", "OperationsService/GetPortfolio", "OperationsService/GetPositions",
                     "OrdersService/GetOrders", "OperationsService/GetOperationsByCursor",
                     # Tariff and futures point values for the commission rates (read-only).
                     "UsersService/GetInfo", "InstrumentsService/GetFuturesMargin"}
# Commission rates (tariff and charged commissions) and the USD/RUB rate for AI spend.
COMMISSIONS_SECONDS = 6 * 3600


class API:
    def __init__(self, production=False):
        self.production = production
        self.secret = live_token() if production else token()
        self.base = PRODUCTION_REST if production else SANDBOX_REST
        self.last_call = 0.0

    def call(self, method, body=None, *, mutate=False):
        if self.production and (mutate or method not in LIVE_READ_METHODS):
            raise RuntimeError("Real-account access here is read-only: " + method)
        for attempt in range(4 if not mutate else 1):
            time.sleep(max(0, 0.4 - (time.monotonic() - self.last_call)))
            self.last_call = time.monotonic()
            result = subprocess.run([
                "/usr/bin/curl", "--silent", "--show-error", "--connect-timeout", "10", "--max-time", "30",
                "--request", "POST", "--header", "Content-Type: application/json", "--data", json.dumps(body or {}),
                "--config", "-", "--write-out", "\n%{http_code}",
                self.base + method,
            ], input="header = " + json.dumps("Authorization: Bearer " + self.secret) + "\n", text=True, capture_output=True)
            if result.returncode:
                raise RuntimeError(f"{method}: curl {result.returncode}: " + result.stderr.replace(self.secret, "[redacted]"))
            payload, status = result.stdout.rsplit("\n", 1)
            if status == "200":
                return json.loads(payload)
            if not mutate and status in {"429", "500", "502", "503", "504"} and attempt < 3:
                time.sleep(2 ** (attempt + 1))
                continue
            raise RuntimeError(f"{method}: HTTP {status}: " + payload.replace(self.secret, "[redacted]")[:1000])
        raise RuntimeError(f"{method}: retry limit reached")


def command(args, **kwargs):
    return subprocess.run(args, check=True, text=True, capture_output=True, **kwargs).stdout.strip()


def environment(state, case=None):
    """Child environment: a real-account case gets only TINKOFF_TOKEN_PRODUCTION,
    a Sandbox case only the sandbox TINKOFF_TOKEN."""
    env = os.environ.copy()
    # TINKOFF_ALLOW_LIVE_TRADING is obsolete; a stray value never reaches a runtime.
    for key in ("TINKOFF_TOKEN", "TINKOFF_TOKEN_FILE", "TINKOFF_TOKEN_PRODUCTION", "TINKOFF_TOKEN_PRODUCTION_FILE", "TINKOFF_ALLOW_LIVE_TRADING"):
        env.pop(key, None)
    env.update(DATABASE_URL=database_url(state), RUST_LOG="tinkoff_bot=info,tinkoff_rs=info", NO_COLOR="1")
    if is_live(case):
        # check-config and migrate need no token; the bot reports a missing one itself.
        with contextlib.suppress(RuntimeError):
            env["TINKOFF_TOKEN_PRODUCTION"] = live_token()
    else:
        env["TINKOFF_TOKEN"] = token()
    return env


def database_url(state):
    return os.environ.get("DATABASE_URL") or state["database_url"]


def external_database():
    return bool(os.environ.get("DATABASE_URL"))


def psql_environment(state):
    # libpq does not expand a URI supplied through PGDATABASE like psql -d does.
    # Keep credentials out of argv while giving each connection field explicitly.
    url = urlsplit(database_url(state))
    if url.scheme not in {"postgres", "postgresql"} or not url.hostname:
        raise RuntimeError("Expected a PostgreSQL connection URL")
    env = dict(os.environ, PGHOST=unquote(url.hostname), PGPORT=str(url.port or 5432),
               PGDATABASE=unquote(url.path.lstrip("/")), PGUSER=unquote(url.username or ""),
               PGPASSWORD=unquote(url.password or ""))
    options = {"sslmode": "PGSSLMODE", "sslrootcert": "PGSSLROOTCERT", "sslcert": "PGSSLCERT",
               "sslkey": "PGSSLKEY", "connect_timeout": "PGCONNECT_TIMEOUT", "options": "PGOPTIONS"}
    for key, value in parse_qsl(url.query):
        if key not in options:
            raise RuntimeError("Unsupported PostgreSQL connection option: " + key)
        env[options[key]] = value
    return env


def bot(state, case, action, *args, timeout=180):
    return subprocess.run([str(BINARY), "--config", str(WORK / case["config"]), action, *args],
                          env=environment(state, case), text=True, capture_output=True, timeout=timeout)


def scheduled_sessions(api):
    start = now()
    data = api.call("InstrumentsService/TradingSchedules", {"exchange": "MOEX", "from": start.isoformat(), "to": (start + dt.timedelta(days=12)).isoformat()})
    days = [day for exchange in data.get("exchanges", []) for day in exchange.get("days", [])
            if day.get("isTradingDay") and timestamp(day["startTime"]) > start + dt.timedelta(hours=1)]
    if len(days) < 5:
        raise RuntimeError("Fewer than five upcoming full sessions returned; rerun preparation later")
    return days[:5]


def refresh_calendar(state, api):
    """MOEX trading days two weeks ahead; recent past days stay for running windows.

    The API rejects a `from` before the current date, so past days come only from
    earlier refreshes and the prepared sessions.
    """
    start = now()
    days = {}
    # T-Invest serves the schedule at most 14 days ahead; hourly refreshes extend it.
    data = api.call("InstrumentsService/TradingSchedules", {"exchange": "MOEX", "from": start.isoformat(), "to": (start + dt.timedelta(days=13)).isoformat()})
    for exchange in data.get("exchanges", []):
        for day in exchange.get("days", []):
            days[day["date"][:10]] = {"date": day["date"][:10], "isTradingDay": bool(day.get("isTradingDay")),
                                      "startTime": day.get("startTime"), "endTime": day.get("endTime")}
    cutoff = (now() - dt.timedelta(days=7)).date().isoformat()
    merged = {d["date"]: d for d in state.get("calendar", []) if d["date"] >= cutoff}
    merged.update(days)
    state["calendar"] = sorted(merged.values(), key=lambda d: d["date"])
    state["calendar_updated"] = now().isoformat()


def calendar(state):
    """Known trading days: refreshed calendar, else the prepared campaign sessions."""
    known = {d["date"]: d for d in ({"date": s["date"][:10], "isTradingDay": True, "startTime": s["startTime"], "endTime": s["endTime"]}
                                    for s in state.get("sessions", []))}
    known.update({d["date"]: d for d in state.get("calendar", [])})
    return sorted(known.values(), key=lambda d: d["date"])


def trading_windows(days_known, chosen):
    """Consecutive chosen trading days form one window: warm-up 1 h before, stop 2 min after."""
    chosen, windows, block = set(chosen or []), [], []
    for day in days_known:
        if not (day.get("isTradingDay") and day.get("startTime") and day.get("endTime")):
            continue
        if day["date"] in chosen:
            block.append(day)
        elif block:
            windows.append(block)
            block = []
    if block:
        windows.append(block)
    return [{"begin": (timestamp(b[0]["startTime"]) - dt.timedelta(hours=1)).isoformat(), "open": b[0]["startTime"],
             "end": (timestamp(b[-1]["endTime"]) + dt.timedelta(minutes=2)).isoformat(), "days": [d["date"] for d in b]}
            for b in windows]


def history(api, uid, interval, count, days, page_days, limit):
    end = now()
    beginning = end - dt.timedelta(days=days)
    found = {}
    while end > beginning and len(found) < count:
        start = max(beginning, end - dt.timedelta(days=page_days))
        data = api.call("MarketDataService/GetCandles", {"instrumentId": uid, "interval": interval,
            "from": start.isoformat(), "to": end.isoformat(), "limit": limit})
        for candle in data.get("candles", []):
            if candle.get("isComplete"):
                found[candle["time"]] = candle
        end = start
    return sorted(found.values(), key=lambda c: c["time"])[-count:]


def select_instruments(api):
    shares = api.call("InstrumentsService/Shares", {"instrumentStatus": "INSTRUMENT_STATUS_BASE"})["instruments"]
    eligible = {s["ticker"]: s for s in shares if s.get("currency") == "rub" and s.get("realExchange") == "REAL_EXCHANGE_MOEX"
                and s.get("apiTradeAvailableFlag") and s.get("shortEnabledFlag") and s.get("classCode") == "TQBR" and int(s.get("lot", 0)) > 0}
    selected = []
    for ticker in PREFERRED + sorted(set(eligible) - set(PREFERRED)):
        if ticker not in eligible:
            continue
        item = eligible[ticker]
        uid = item["uid"]
        monthly = history(api, uid, "CANDLE_INTERVAL_MONTH", 61, 365 * 7, 365 * 5, 120)
        daily = history(api, uid, "CANDLE_INTERVAL_DAY", 253, 850, 365, 2400)
        intraday = history(api, uid, "CANDLE_INTERVAL_30_MIN", 1001, 168, 14, 1200)
        if min(len(monthly) - 61, len(daily) - 253, len(intraday) - 1001) < 0:
            print(f"history incomplete: {ticker}", flush=True)
            continue
        selected.append({"ticker": ticker, "uid": uid, "lot": item["lot"], "history_counts": [len(monthly), len(daily), len(intraday)]})
        print(f"history verified: {ticker}", flush=True)
        if len(selected) == 10:
            break
    if len(selected) != 10:
        raise RuntimeError("Could not find ten eligible instruments with full default history")
    etfs = api.call("InstrumentsService/Etfs", {"instrumentStatus": "INSTRUMENT_STATUS_BASE"})["instruments"]
    for ticker in ["SBMX", "TMOS"]:
        for item in etfs:
            if item.get("ticker") == ticker and item.get("currency") == "rub" and item.get("realExchange") == "REAL_EXCHANGE_MOEX":
                if len(history(api, item["uid"], "CANDLE_INTERVAL_DAY", 253, 850, 365, 2400)) == 253:
                    return selected, {"ticker": ticker, "uid": item["uid"]}
    raise RuntimeError("No SBMX/TMOS benchmark with sufficient history")


def database(state):
    if external_database():
        command(["psql", "-X", "-v", "ON_ERROR_STOP=1", "-At", "-c", "SELECT 1"],
                env=psql_environment(state))
        return
    name = state["container"]
    existing = subprocess.run(["docker", "inspect", name], text=True, capture_output=True)
    if existing.returncode:
        env = os.environ.copy()
        env["POSTGRES_PASSWORD"] = state["database_password"]
        command(["docker", "run", "--detach", "--name", name, "--restart", "unless-stopped",
                 "--publish", f"127.0.0.1:{state['database_port']}:5432", "--env", "POSTGRES_PASSWORD",
                 "--env", "POSTGRES_USER=tinkoff_campaign", "--env", "POSTGRES_DB=tinkoff_campaign",
                 "--volume", name + ":/var/lib/postgresql/data", "postgres:17-bookworm"], env=env)
    else:
        command(["docker", "start", name])
    for _ in range(60):
        result = subprocess.run(["docker", "exec", name, "pg_isready", "-U", "tinkoff_campaign"], capture_output=True)
        if result.returncode == 0:
            return
        time.sleep(1)
    raise RuntimeError("Dedicated PostgreSQL did not become ready")


def write_config(state, kind, account):
    text = (ROOT / "examples/configs" / (kind + ".toml")).read_text()
    strategy_block = text[text.index("[[strategies]]"):]
    strategy_block = re.sub(r'benchmark_uid\s*=\s*"[^"]+"', 'benchmark_uid = ' + json.dumps(state["benchmark"]["uid"]), strategy_block)
    pool = state["instruments"][:1] if kind == "market-intraday-momentum" else state["instruments"]
    if kind == "weighted-ensemble":
        pool = pool[:2]
    strategy_id = state["id"] + "-" + kind
    text = '\n'.join([
        'contour = "sandbox"', 'account_id = ' + json.dumps(account), 'strategy_id = ' + json.dumps(strategy_id),
        'app_name = "ObraztsovOleg.tinkoff-rs.campaign"', 'database_connections = 5',
        '\n[risk]', 'budget_rub = "1000000.00"', 'daily_loss_percent = "5.0"', 'max_drawdown_percent = "25.0"',
    ]) + '\n'
    for item in pool:
        text += '\n[[universe]]\nuid = ' + json.dumps(item["uid"]) + '\n'
    text += '\n' + strategy_block
    relative = "configs/" + kind + ".toml"
    path = WORK / relative
    path.parent.mkdir(exist_ok=True)
    path.write_text(text)
    path.chmod(0o600)
    return {"kind": kind, "strategy_id": strategy_id, "account_id": account, "config": relative,
            "state": "prepared", "attempts": 0, "cohort": next(i for i, group in enumerate(COHORTS) if kind in group),
            "verified_defaults": strategy_block}


def prepare(allow_account_reuse=False, adopt_idle_accounts=False):
    if (WORK / "campaign.json").exists():
        state = load()
        if state.get("prepared"):
            print("Already prepared: " + state["id"])
            return
    else:
        identity = "sandbox-" + now().strftime("%Y%m%dT%H%M%S")
        state = {"id": identity, "cases": {}, "created_at": now().isoformat()}
        if external_database():
            state["database_mode"] = "external"
        else:
            with socket.socket() as sock:
                sock.bind(("127.0.0.1", 0))
                port = sock.getsockname()[1]
            password = secrets.token_hex(24)
            state.update(container="tinkoff-rs-" + identity, database_port=port,
                         database_password=password, database_url=f"postgres://tinkoff_campaign:{password}@127.0.0.1:{port}/tinkoff_campaign")
        persist(state)
    if allow_account_reuse:
        state["allow_account_reuse"] = True
        persist(state)
    if adopt_idle_accounts:
        state["adopt_idle_accounts"] = True
        persist(state)
    api = API()
    limits = api.call("UsersService/GetUserTariff")
    slots = [entry["limit"] - entry.get("open", 0) for entry in limits["streamLimits"] if any(x.endswith("/MarketDataStream") for x in entry["streams"])]
    if not slots or min(slots) < 4:
        raise RuntimeError("Four market-data streams are required for the five-session roster")
    state["limits"] = limits
    if "instruments" not in state:
        state["instruments"], state["benchmark"] = select_instruments(api)
        persist(state)
    database(state)
    accounts = api.call("SandboxService/GetSandboxAccounts").get("accounts", [])
    for group in COHORTS:
        for kind in group:
            name = state["id"] + "-" + kind
            if kind in state["cases"]:
                continue
            matches = [a for a in accounts if a.get("name") == name and a.get("status") != "ACCOUNT_STATUS_CLOSED"]
            if len(matches) > 1:
                raise RuntimeError("Ambiguous bootstrap account: " + name)
            predecessor = None
            if matches:
                account = matches[0]["id"]
            else:
                try:
                    if state.get("account_limit"):
                        raise RuntimeError("35001: recorded sandbox account limit")
                    account = api.call("SandboxService/OpenSandboxAccount", {"name": name}, mutate=True)["accountId"]
                except RuntimeError as error:
                    if "35001" not in str(error):
                        raise
                    # Лимит счетов достигнут. Сначала (с явного разрешения) берётся
                    # уже открытый пустой счёт, затем — счёт завершённой когорты.
                    adopted = adopt_idle_account(state, api, accounts) if state.get("adopt_idle_accounts") else None
                    if adopted:
                        account = adopted
                    else:
                        if not state.get("allow_account_reuse"):
                            raise RuntimeError("Sandbox account limit reached. Reuse needs explicit approval: "
                                               "--adopt-idle-accounts (existing empty accounts) and/or --allow-account-reuse") from error
                        cohort = next(i for i, group in enumerate(COHORTS) if kind in group)
                        latest = {}
                        for existing in state["cases"].values():
                            latest[existing["account_id"]] = existing
                        candidates = sorted((c for c in latest.values() if c["cohort"] < cohort), key=lambda c: c["cohort"])
                        if not candidates:
                            raise RuntimeError("Account cap cannot accommodate four independent runtimes")
                        previous = candidates[0]
                        account, predecessor = previous["account_id"], previous["kind"]
                    state["account_limit"] = len({c["account_id"] for c in state["cases"].values()} | {account})
            state["cases"][kind] = write_config(state, kind, account)
            state["cases"][kind]["funded"] = False
            if predecessor:
                state["cases"][kind]["predecessor"] = predecessor
            persist(state)
    initialize_accounts(state, api)
    state["sessions"] = scheduled_sessions(api)
    state["prepared"] = True
    state["phase"] = "prepared"
    persist(state)
    report(state)
    accounts = len({case["account_id"] for case in state["cases"].values()})
    print(f"Prepared {len(state['cases'])} scenarios on {accounts} sandbox accounts; artifacts: {WORK}")


CAMPAIGN_BUDGET_RUB = 1000000


def account_is_idle(api, account_id):
    """True only for an account without securities, derivatives, blocked money or active orders."""
    positions = api.call("SandboxService/GetSandboxPositions", {"accountId": account_id})
    for group in ("securities", "futures", "options"):
        for position in positions.get(group, []):
            if int(position.get("balance", 0) or 0) or int(position.get("blocked", 0) or 0):
                return False
    for money in positions.get("blocked", []):
        if int(money.get("units", "0") or 0) or int(money.get("nano", 0) or 0):
            return False
    return not api.call("SandboxService/GetSandboxOrders", {"accountId": account_id}).get("orders")


def adopt_idle_account(state, api, accounts):
    """Use an already open, empty sandbox account instead of closing anything.

    Only with explicit --adopt-idle-accounts: another campaign on the same token
    must not be running, because its account lease lives in a different database.
    """
    used = {case["account_id"] for case in state["cases"].values()}
    for account in sorted(accounts, key=lambda a: (a.get("openedDate", ""), a.get("id", ""))):
        if account.get("status") != "ACCOUNT_STATUS_OPEN" or account.get("id") in used:
            continue
        if account_is_idle(api, account["id"]):
            state.setdefault("adopted_accounts", {})[account["id"]] = {"previous_name": account.get("name", ""), "adopted_at": now().isoformat()}
            print(f"adopted idle sandbox account: {account.get('name') or account['id']}", flush=True)
            return account["id"]
    return None


def rub_value(money):
    return int(money.get("units", "0") or 0) + int(money.get("nano", 0) or 0) / 1_000_000_000


def initialize_accounts(state, api):
    # Balance inspection makes resuming an ambiguous pay-in safe: never repeat a credited deposit.
    adopted = state.get("adopted_accounts", {})
    for case in state["cases"].values():
        if not case.get("funded"):
            portfolio = api.call("SandboxService/GetSandboxPortfolio", {"accountId": case["account_id"], "currency": "RUB"})
            value = rub_value(portfolio.get("totalAmountPortfolio", {}))
            if case["account_id"] in adopted:
                # Прежний остаток сохраняется; недостающее до бюджета пополняется один раз.
                if value < CAMPAIGN_BUDGET_RUB:
                    if case.get("topup_requested"):
                        raise RuntimeError("Previous top-up is uncertain; check sandbox operations before retrying: " + case["kind"])
                    shortfall = int(CAMPAIGN_BUDGET_RUB - value) + (0 if float(value).is_integer() else 1)
                    case["topup_requested"] = shortfall
                    persist(state)
                    api.call("SandboxService/SandboxPayIn", {"accountId": case["account_id"], "amount": {"currency": "rub", "units": str(shortfall), "nano": 0}}, mutate=True)
            elif value == 0:
                api.call("SandboxService/SandboxPayIn", {"accountId": case["account_id"], "amount": {"currency": "rub", "units": str(CAMPAIGN_BUDGET_RUB), "nano": 0}}, mutate=True)
            elif int(value) != CAMPAIGN_BUDGET_RUB:
                raise RuntimeError("Unexpected balance during bootstrap: " + case["kind"])
            case["funded"] = True
            persist(state)
        result = bot(state, case, "check-config")
        if result.returncode:
            raise RuntimeError(result.stdout + result.stderr)
    result = bot(state, next(iter(state["cases"].values())), "migrate")
    if result.returncode:
        raise RuntimeError(result.stdout + result.stderr)


def process_alive(pid, marker):
    if not pid:
        return False
    result = subprocess.run(["ps", "-p", str(pid), "-o", "command="], text=True, capture_output=True)
    return result.returncode == 0 and marker in result.stdout


def stop_process(case):
    pid = case.get("pid")
    if process_alive(pid, str(WORK / case["config"])):
        os.kill(pid, signal.SIGINT)
        for _ in range(45):
            if not process_alive(pid, str(WORK / case["config"])):
                break
            time.sleep(1)
        else:
            # The lease remains the authority; never start cleanup alongside an unresponsive runtime.
            raise RuntimeError("Runtime did not stop; cleanup blocked for " + case["kind"])
    case.pop("pid", None)


def launch(state, case, service_only=False, planned=False):
    """Start a runtime. A planned restart (restart check) does not spend the crash budget."""
    # The limit covers Sandbox runtimes; a real account uses its own token and API limits.
    if not is_live(case) and sum(process_alive(c.get("pid"), str(WORK / c["config"])) for c in state["cases"].values()
                                 if not is_live(c)) >= state.get("max_runtimes", 4):
        case["state"] = "waiting for runtime slot"
        return False
    if is_live(case) and not live_status(state)["ready"]:
        case["state"] = "real token not connected"
        return False
    if case.get("predecessor") and not case.get("ownership_transferred"):
        previous = state["cases"][case["predecessor"]]
        if previous["state"] != "cleaned":
            case["state"] = "waiting for predecessor cleanup"
            return False
        own_status = bot(state, case, "risk-status")
        if own_status.returncode:
            old_status = bot(state, previous, "risk-status")
            if old_status.returncode:
                case["transfer_error"] = "Cannot verify previous risk owner: " + old_status.stdout + old_status.stderr
                persist(state)
                return False
            save(WORK / "reports" / (previous["kind"] + "-final-risk.json"), json.loads(old_status.stdout))
            control_plane.renew_returning_strategy(SimpleNamespace(**globals()), state, case, json.loads(old_status.stdout)["epoch"])
            result = bot(state, case, "risk-reset", "--reason", "sandbox account cap: transfer cleaned campaign account")
            if result.returncode:
                case["transfer_error"] = result.stdout + result.stderr
                persist(state)
                return False
        case["ownership_transferred"] = True
        persist(state)
    path = WORK / "logs" / (case["kind"] + ".log")
    path.parent.mkdir(exist_ok=True)
    case["log_offset"] = path.stat().st_size if path.exists() else 0
    with path.open("a") as log:
        child = subprocess.Popen([str(BINARY), "--config", str(WORK / case["config"]), "run"] + (["--service-only"] if service_only else []),
                                 env=environment(state, case), stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    case.update(pid=child.pid, state="paused; risk monitor" if service_only else "running", service_only=service_only,
                attempts=case["attempts"] + (0 if planned else 1), last_started=now().isoformat())
    case.pop("retry_at", None)
    if not planned:
        case["launches"] = case.get("launches", 0) + 1
    persist(state)
    return True


def runtime_ready(case):
    if not process_alive(case.get("pid"), str(WORK / case["config"])):
        return False
    path = WORK / "logs" / (case["kind"] + ".log")
    if not path.exists():
        return False
    with path.open("rb") as log:
        log.seek(case.get("log_offset", 0))
        return b"runtime is live" in log.read()


def cleanup_case(state, case):
    if is_live(case):
        # Liquidating a real account is only the operator's explicit, confirmed command.
        raise RuntimeError("Real account positions are closed only by an operator command: " + case["kind"])
    if case.get("predecessor") and not case.get("ownership_transferred"):
        case["state"] = "cleaned"
        case["coverage_note"] = "Not started; predecessor owns account and its cleanup"
        persist(state)
        return
    stop_process(case)
    try:
        result = bot(state, case, "cleanup", "--reason", "five-session sandbox campaign teardown", timeout=150)
        log = result.stdout + result.stderr
        done = result.returncode == 0
    except subprocess.TimeoutExpired:
        log, done = "cleanup timed out; reconciliation required on next attempt\n", False
    path = WORK / "logs" / (case["kind"] + ".cleanup.log")
    with path.open("a") as output:
        output.write(log)
    case["state"] = "cleaned" if done else "cleanup_pending"
    case["last_cleanup"] = now().isoformat()
    persist(state)


def sql_snapshot(state):
    query = """SELECT json_build_object(
      'risk', (SELECT coalesce(json_agg(t),'[]'::json) FROM (SELECT account_id,strategy_id,epoch,state,updated_at FROM account_risk_state) t),
      'strategy_state', (SELECT coalesce(json_agg(t),'[]'::json) FROM (SELECT strategy_id,version,updated_at FROM strategy_state) t),
      'intent_audit', (SELECT coalesce(json_agg(t),'[]'::json) FROM (SELECT i.*,o.exchange_order_id,o.lots_executed FROM trade_intents i LEFT JOIN orders o ON o.intent_id=i.id) t),
      'fills', (SELECT coalesce(json_agg(t),'[]'::json) FROM fills t),
      'decisions', (SELECT coalesce(json_agg(t),'[]'::json) FROM (SELECT strategy_id,count(*) AS events FROM processed_events WHERE event_id NOT LIKE 'candle:%' GROUP BY strategy_id) t),
      'batches', (SELECT coalesce(json_agg(t),'[]'::json) FROM (SELECT strategy_id,event_id,status,target,reason FROM rebalance_batches) t),
      'orders', (SELECT coalesce(json_agg(t),'[]'::json) FROM (SELECT i.strategy_id,i.status,count(*) AS count FROM trade_intents i JOIN rebalance_batches b ON b.id=i.batch_id WHERE i.runtime_version=2 AND b.event_id !~ '^(cleanup:|risk:|manual:|approval-refresh:|risk-check:)' GROUP BY i.strategy_id,i.status) t),
      'control_commands', (SELECT coalesce(json_agg(t),'[]'::json) FROM (SELECT id,strategy_id,kind,status,created_at FROM control_commands ORDER BY created_at DESC LIMIT 1000) t),
      'runs', (SELECT coalesce(json_agg(t),'[]'::json) FROM strategy_runs t)
    );"""
    return json.loads(run_sql(state, query))


def run_sql(state, query):
    if external_database():
        data = command(["psql", "-X", "-v", "ON_ERROR_STOP=1", "-At", "-c", query],
                       env=psql_environment(state))
    else:
        data = command(["docker", "exec", state["container"], "psql", "-U", "tinkoff_campaign", "-d", "tinkoff_campaign", "-At", "-c", query])
    return data


def park_local():
    """Quiesce even a pre-PARK supervisor without liquidating the campaign."""
    state = load()
    pid = state.get("supervisor_pid")
    active = process_alive(pid, "sandbox-campaign.py supervise")
    if active:
        os.kill(pid, signal.SIGSTOP)
    try:
        state = load()
        for case in state["cases"].values():
            stop_process(case)
        owners = run_sql(state, "SELECT count(*) FROM pg_locks WHERE locktype='advisory' AND granted AND database=(SELECT oid FROM pg_database WHERE datname=current_database())")
        if owners != "0":
            raise RuntimeError("Account leases remain active; export is blocked")
        if active:
            # Only the frozen scheduler is terminated; every trading runtime has
            # already shut down gracefully and its database lease is released.
            os.kill(pid, signal.SIGKILL)
        state["phase"] = "parked"
        state.pop("supervisor_pid", None)
        (WORK / "PARK").touch()
        persist(state)
    except BaseException:
        if active and process_alive(pid, "sandbox-campaign.py supervise"):
            os.kill(pid, signal.SIGCONT)
        raise


def report(state):
    snapshot = None
    try:
        snapshot = sql_snapshot(state)
        save(WORK / "reports" / "database.json", snapshot)
    except (subprocess.CalledProcessError, json.JSONDecodeError) as error:
        state["report_error"] = str(error)
    lines = ["# Sandbox campaign " + state["id"], "", "Updated: " + now().isoformat(), "Phase: " + state.get("phase", "preparing"), "",
             "| Strategy | Process | Warmup / subscriptions | Natural decisions / targets | Strategy executions |", "|---|---|---|---|---|"]
    for case in state["cases"].values():
        batches = [b for b in (snapshot or {}).get("batches", []) if b["strategy_id"] == case["strategy_id"] and not b["event_id"].startswith(("cleanup:", "risk:", "manual:", "approval-refresh:", "risk-check:"))]
        nonzero = [b for b in batches if any(float(w["weight"]) != 0 for w in b["target"]["weights"])]
        log_path = WORK / "logs" / (case["kind"] + ".log")
        log = log_path.read_bytes()[case.get("revision_log_offset", 0):].decode(errors="replace") if log_path.exists() else ""
        case["readiness_observed"] = "runtime is live" in log
        case["natural_target_count"] = len(batches)
        case["nonzero_target_count"] = len(nonzero)
        orders = [o for o in (snapshot or {}).get("orders", []) if o["strategy_id"] == case["strategy_id"]]
        # Maintenance orders are not evidence of strategy executions; only completed natural batches with nonzero targets qualify here.
        filled = any(o["status"] == "filled" for o in orders)
        case["strategy_filled_intents"] = sum(o["count"] for o in orders if o["status"] == "filled")
        case["failed_batches"] = sum(b["status"] == "failed" for b in batches)
        decisions = {match.group(1) for line in log.splitlines()
                     if "live decision observed" in line and "duplicate=false" in line and "event_id=risk-check:" not in line
                     for match in [re.search(r"event_id=([^ ]+)", line)] if match}
        case["natural_decision_count"] = len(decisions)
        ready = "passed" if case["readiness_observed"] or case.get("preflight_result") == "passed" else "pending / uncovered"
        result = f"{case['natural_decision_count']} decisions; {len(batches)} targets ({len(batches)-len(nonzero)} zero)"
        lines.append(f"| {case['kind']} | {case['state']} | {ready} | {result} | {'observed; see audit' if filled else 'uncovered'} |")
    lines += ["", "Zero targets are valid decisions. No signal, failed startup and no filled order are separate coverage gaps.",
              "Cleanup trades are maintenance. Five sessions do not establish profitability or cover all rare signals."]
    if state.get("sessions"):
        lines += ["", "Scheduled sessions (UTC dates): " + ", ".join(d["date"][:10] for d in state["sessions"])]
    commands = (snapshot or {}).get("control_commands", [])
    lines += ["", "Control mode: " + state.get("mode", "campaign"),
              "User interventions do not extend the scheduled observation window."]
    for entry in commands:
        lines.append(f"- Command {entry['id']}: {entry['kind']}; {entry['status']}; strategy={entry.get('strategy_id') or 'campaign'}; {entry['created_at']}")
    for run in (snapshot or {}).get("runs", []):
        lines.append(f"- Statistical run {run['id']}: strategy={run['strategy_id']}; {run['classification']}; interventions={json.dumps(run['interventions'], ensure_ascii=False)}")
    for case in state["cases"].values():
        risk = next((r for r in (snapshot or {}).get("risk", []) if r["strategy_id"] == case["strategy_id"]), None)
        saved = any(r["strategy_id"] == case["strategy_id"] for r in (snapshot or {}).get("strategy_state", []))
        lines.append(f"- {case['kind']}: state_saved={saved}; risk_epoch={'saved' if risk else 'pending or archived'}; restart={case.get('restart_result', 'uncovered')}; lease={case.get('lease_check', 'uncovered')}; failed_batches={case['failed_batches']}; launches={case.get('launches', 0)}")
    path = WORK / "reports" / "latest.md"
    path.parent.mkdir(exist_ok=True)
    path.write_text("\n".join(lines) + "\n")
    (path.parent / (now().date().isoformat() + ".md")).write_text(path.read_text())
    if state.get("phase") == "complete":
        (path.parent / "final.md").write_text(path.read_text())
    persist(state)


def verify_restart(state, case):
    """Outside the session: reject concurrent owner, restart and compare risk epoch."""
    if case.get("restart_checked"):
        return
    before = bot(state, case, "risk-status")
    if before.returncode:
        return
    duplicate = bot(state, case, "run", timeout=60)
    if duplicate.returncode == 0 or "already" not in (duplicate.stdout + duplicate.stderr).lower():
        case["lease_check"] = "failed"
        raise RuntimeError("Concurrent owner rejection was not demonstrated")
    case["lease_check"] = "passed"
    # risk-status emits JSON (without log lines on success).
    epoch = json.loads(before.stdout)["epoch"]
    stop_process(case)
    if not launch(state, case, planned=True):
        raise RuntimeError("Restart launch was blocked")
    case["epoch_before_restart"] = epoch
    case["restart_checked"] = True
    persist(state)


SANDBOX_READS = {"operations": "SandboxService/GetSandboxOperationsByCursor", "portfolio": "SandboxService/GetSandboxPortfolio",
                 "orders": "SandboxService/GetSandboxOrders"}
LIVE_READS = {"operations": "OperationsService/GetOperationsByCursor", "portfolio": "OperationsService/GetPortfolio",
              "orders": "OrdersService/GetOrders"}


def audit_accounts(state, api):
    audit = {"observed_at": now().isoformat(), "accounts": {}}
    accounts = {}
    live_api = None
    for kind, case in state["cases"].items():
        try:
            if case["account_id"] in accounts:
                audit["accounts"][kind] = accounts[case["account_id"]]
                continue
            client, reads = api, SANDBOX_READS
            if is_live(case):
                # Read-only client: the real account is only observed here.
                live_api = live_api or API(production=True)
                client, reads = live_api, LIVE_READS
            body = {"accountId": case["account_id"]}
            operations, cursor = [], None
            while True:
                request = dict(body, **{"from": case.get("promoted_at") or state["created_at"], "to": audit["observed_at"], "limit": 1000})
                if cursor:
                    request["cursor"] = cursor
                page = client.call(reads["operations"], request)
                operations.extend(page.get("items", []))
                if not page.get("hasNext"):
                    break
                next_cursor = page.get("nextCursor")
                if not next_cursor or next_cursor == cursor:
                    raise RuntimeError("Operations pagination did not advance")
                cursor = next_cursor
            accounts[case["account_id"]] = audit["accounts"][kind] = {
                "account_id": case["account_id"],
                "portfolio": client.call(reads["portfolio"], body),
                "orders": client.call(reads["orders"], body),
                "operations": operations,
            }
        except RuntimeError as error:
            audit["accounts"][kind] = {"error": str(error)}
            if ": curl " in str(error):
                # Сети нет: не тратить минуты на таймауты по каждому счёту.
                for rest in state["cases"]:
                    audit["accounts"].setdefault(rest, {"error": "deferred: broker API unreachable"})
                break
    save(WORK / "reports" / "broker-latest.json", audit)
    save(WORK / "reports" / (now().date().isoformat() + "-broker.json"), audit)


# Аварийные выходы runtime: первые RESTART_BURST перезапусков без паузы,
# дальше пауза растёт от минуты до RESTART_MAX_DELAY. Runtime, проработавший
# HEALTHY_RUNTIME, снова получает полный запас быстрых перезапусков.
RESTART_BURST = 3
RESTART_MAX_DELAY = dt.timedelta(minutes=15)
HEALTHY_RUNTIME = dt.timedelta(minutes=15)


def restart_not_before(case):
    attempts = case.get("attempts", 0)
    if attempts < RESTART_BURST or not case.get("last_started"):
        return None
    delay = min(dt.timedelta(minutes=2 ** (attempts - RESTART_BURST)), RESTART_MAX_DELAY)
    return timestamp(case["last_started"]) + delay


def refill_restart_budget(case, current):
    if case.get("attempts") and case.get("last_started") and current - timestamp(case["last_started"]) >= HEALTHY_RUNTIME:
        case["attempts"] = 0


# A wall-clock gap between supervisor passes means the computer (or its VM) was
# asleep: every runtime was frozen and the trading sessions in between were missed.
SUSPENSION_GAP = 120
SUSPENSION_MERGE = dt.timedelta(minutes=5)


def note_suspension(state, before, after):
    """Record a sleep interval; short maintenance wakes merge into one interval."""
    start, end = dt.datetime.fromtimestamp(before, UTC), dt.datetime.fromtimestamp(after, UTC)
    history = state.setdefault("suspensions", [])
    if history and start - timestamp(history[-1]["to"]) <= SUSPENSION_MERGE:
        history[-1]["to"] = end.isoformat()
    else:
        history.append({"from": start.isoformat(), "to": end.isoformat()})
    del history[:-20]
    supervisor_log(f"stand was suspended from {start.isoformat()} to {end.isoformat()}; runtimes were frozen")


def supervisor_log(message):
    with (WORK / "supervisor.log").open("a") as log:
        log.write(now().isoformat() + " " + message + "\n")


# Nightly upkeep at 03:30 Moscow time, outside every trading session.
MAINTENANCE_AT_UTC = dt.time(0, 30)
MAINTENANCE_RETRY = dt.timedelta(hours=1)
# Risk-check event ids carry their own timestamp and are never replayed, so an
# old marker protects nothing. Candle, schedule and advice markers stay: warmup
# and the session-open retry rely on them.
RETENTION_SQL = """WITH events AS (
    DELETE FROM processed_events WHERE event_id LIKE 'risk-check:%' AND processed_at < now() - interval '7 days' RETURNING 1),
  samples AS (
    DELETE FROM performance_samples p WHERE p.observed_at < now() - interval '30 days' AND EXISTS (
      SELECT 1 FROM performance_samples q WHERE q.run_id = p.run_id AND q.observed_at > p.observed_at
        AND date_trunc(CASE WHEN p.observed_at < now() - interval '365 days' THEN 'day' ELSE 'hour' END, q.observed_at)
          = date_trunc(CASE WHEN p.observed_at < now() - interval '365 days' THEN 'day' ELSE 'hour' END, p.observed_at))
    RETURNING 1),
  usage AS (DELETE FROM ai_usage WHERE at < now() - interval '400 days' RETURNING 1)
SELECT json_build_object('risk_check_events', (SELECT count(*) FROM events), 'performance_samples', (SELECT count(*) FROM samples),
  'ai_usage', (SELECT count(*) FROM usage))"""
# Text logs of runtimes and services: the newest LOG_KEEP_BYTES stay in `<name>.1`.
LOG_LIMIT_BYTES = 50 * 1024 * 1024
LOG_KEEP_BYTES = 10 * 1024 * 1024
DISK_CHECK_SECONDS = 300


# Document collector for the AI model strategy: one process for the stand.
COLLECTOR_MARKER = "tinkoff-bot collect"
COLLECTOR_RESTART = dt.timedelta(minutes=1)


def uses_ai_model(case):
    path = WORK / case["config"]
    return path.exists() and 'kind = "ai-model"' in path.read_text()


def collector_alive(state):
    return process_alive(state.get("collector", {}).get("pid"), "collect")


def stop_collector(state):
    record = state.get("collector", {})
    if collector_alive(state):
        os.kill(record["pid"], signal.SIGINT)
        for _ in range(30):
            if not collector_alive(state):
                break
            time.sleep(1)
    record.pop("pid", None)


def supervise_collector(state, desired, stopping):
    """Keep the collector running while an AI portfolio is on; stop it on a
    stand stop, park or critically low disk space."""
    record = state.setdefault("collector", {})
    low_disk = state.get("disk", {}).get("level") == "critical"
    wanted = desired and not stopping and not low_disk
    if not wanted:
        if collector_alive(state):
            stop_collector(state)
            supervisor_log("document collector stopped: " + ("disk space is critically low" if low_disk else "no AI portfolio is on"))
        record["state"] = "disk space low" if low_disk and desired else "off"
        return
    if collector_alive(state):
        record["state"] = "running"
        return
    if record.get("exited_at") and now() - timestamp(record["exited_at"]) < COLLECTOR_RESTART:
        record["state"] = "restart backoff"
        return
    if record.get("pid"):
        # It was running and exited: note it, restart after a pause.
        record.update(pid=None, exited_at=now().isoformat(), restarts=record.get("restarts", 0) + 1, state="restart backoff")
        supervisor_log("document collector exited; restarting in a minute")
        return
    path = WORK / "logs" / "collector.log"
    path.parent.mkdir(exist_ok=True)
    with path.open("a") as log:
        child = subprocess.Popen([str(BINARY), "collect"], env=environment(state, None), stdout=log,
                                 stderr=subprocess.STDOUT, start_new_session=True)
    record.update(pid=child.pid, started=now().isoformat(), state="running", exited_at=None)


def maintenance_due(state, current):
    record = state.get("maintenance", {})
    if record.get("retry_at") and current < timestamp(record["retry_at"]):
        return False
    due = dt.datetime.combine(current.date(), MAINTENANCE_AT_UTC, UTC)
    return current >= due and record.get("day") != current.date().isoformat()


def rotate_logs():
    """Keep the newest part of a long log; writers append, so truncation is safe."""
    rotated = []
    for path in sorted(list((WORK / "logs").glob("*.log")) + list(WORK.glob("*.log"))):
        if path.stat().st_size <= LOG_LIMIT_BYTES:
            continue
        with path.open("rb") as log:
            log.seek(-LOG_KEEP_BYTES, os.SEEK_END)
            tail = log.read()
        previous = path.with_name(path.name + ".1")
        previous.write_bytes(tail[tail.find(b"\n") + 1:])
        previous.chmod(0o600)
        with path.open("r+b") as log:
            log.truncate(0)
        rotated.append(path.name)
    return rotated


def maintenance(state):
    """Retention, statistics and log rotation once a day; failures retry hourly."""
    current = now()
    record = state.setdefault("maintenance", {})
    started = time.monotonic()
    try:
        deleted = json.loads(run_sql(state, RETENTION_SQL))
        run_sql(state, "VACUUM (ANALYZE) processed_events, performance_samples, ai_usage")
        rotated = rotate_logs()
        for case in state["cases"].values():
            if case["kind"] + ".log" in rotated:
                case["log_offset"] = 0
        size = int(run_sql(state, "SELECT pg_database_size(current_database())"))
    except (RuntimeError, subprocess.SubprocessError, OSError, ValueError) as error:
        record.update(retry_at=(current + MAINTENANCE_RETRY).isoformat(), error=str(error)[:500])
        supervisor_log("maintenance failed; retrying in an hour: " + str(error))
        return
    state["maintenance"] = {"day": current.date().isoformat(), "at": current.isoformat(), "deleted": deleted,
                            "rotated_logs": rotated, "database_bytes": size, "seconds": round(time.monotonic() - started, 1)}
    supervisor_log("maintenance done: deleted " + json.dumps(deleted) + f", database {size // 1048576} MiB")


def disk_paths():
    """Watched file systems: in Compose a bind mount shows the Mac disk, /data the VM disk."""
    raw = os.environ.get("DISK_WATCH_PATHS")
    return [Path(p) for p in raw.split(":") if p] if raw else [WORK]


def disk_status(state):
    """Free space on the tightest watched disk. A critical level stops document
    collection and backups; trading keeps running."""
    warn = float(os.environ.get("DISK_WARN_GB", "3")) * 1024 ** 3
    critical = float(os.environ.get("DISK_CRITICAL_GB", "1")) * 1024 ** 3
    readings = []
    for path in disk_paths():
        with contextlib.suppress(OSError):
            usage = shutil.disk_usage(path)
            readings.append({"path": str(path), "free_bytes": usage.free, "total_bytes": usage.total})
    if not readings:
        return
    tightest = min(readings, key=lambda r: r["free_bytes"])
    level = "critical" if tightest["free_bytes"] < critical else "warn" if tightest["free_bytes"] < warn else "ok"
    previous = state.get("disk", {}).get("level")
    database = None
    with contextlib.suppress(RuntimeError, subprocess.SubprocessError, OSError, ValueError, KeyError):
        database = int(run_sql(state, "SELECT pg_database_size(current_database())"))
    state["disk"] = {"level": level, "free_bytes": tightest["free_bytes"], "path": tightest["path"], "readings": readings,
                     "database_bytes": database, "checked_at": now().isoformat()}
    marker = WORK / "DISK_LOW"
    if level == "critical":
        marker.touch()
    else:
        marker.unlink(missing_ok=True)
    if level != previous and level != "ok":
        supervisor_log(f"disk space {level}: {tightest['free_bytes'] // 1048576} MiB free on {tightest['path']}")


def backups():
    """Nightly copies written by the backup service, newest first."""
    directory = WORK / "backups"
    files = sorted(directory.glob("*.dump"), key=lambda f: f.stat().st_mtime, reverse=True) if directory.is_dir() else []
    status = None
    with contextlib.suppress(OSError, ValueError):
        status = json.loads((directory / "status.json").read_text())
    return {"status": status, "files": [{"name": f.name, "bytes": f.stat().st_size,
                                         "at": dt.datetime.fromtimestamp(f.stat().st_mtime, UTC).isoformat()} for f in files]}


def supervise_tick(state, api, clocks):
    """One scheduling pass. Raises only for local failures; the caller retries."""
    controls = control_plane.process(SimpleNamespace(**globals()), state)
    desired = {c["strategy_id"]: c for c in controls["cases"]}
    owners = {c["account_id"]: c["strategy_id"] for c in controls["owners"]}
    pending_commands = {c["strategy_id"]: c["id"] for c in controls.get("pending", [])}
    current = now()
    stopping = (WORK / "STOP").exists()
    days_known = calendar(state)
    state["live"] = controls.get("live") or {}
    live = live_status(state)
    for case in state["cases"].values():
        control = desired.get(case["strategy_id"], {})
        enabled = control.get("enabled", True)
        service = control.get("needs_service", False)
        run_mode = control.get("run_mode", "schedule")
        windows = trading_windows(days_known, control.get("trading_days", [])) if run_mode == "schedule" else []
        window = next((w for w in windows if timestamp(w["begin"]) <= current < timestamp(w["end"])), None)
        finished = max((w["end"] for w in windows if timestamp(w["end"]) <= current), default=None)
        # The dashboard reads the operator schedule from here; no window is inferred elsewhere.
        case["schedule"] = {"run_mode": run_mode, "windows": windows, "active": window is not None,
                            "close_after": control.get("close_after", True)}
        pending_id = pending_commands.get(case["strategy_id"])
        if pending_id and pending_id != case.get("last_control_command"):
            case.update(attempts=0, last_control_command=pending_id)
        if case["strategy_id"] in controls.get("closed", []) and not pending_id:
            stop_process(case)
            case["state"] = "cleaned"
        active = enabled and (run_mode == "always" or window is not None)
        if is_live(case) and (stopping or not live["ready"]):
            # A real account is never liquidated by the stand: campaign stop, the
            # emergency stop or a removed token only stop its runtime.
            if process_alive(case.get("pid"), str(WORK / case["config"])):
                stop_process(case)
            case["state"] = ("stopped; real positions kept" if stopping else
                             "emergency stop" if live["halted"] else "real token not connected")
            continue
        if case["state"] == "cleaned" and not active and not service:
            continue
        window_closed = (not is_live(case) and run_mode == "schedule" and case["schedule"]["close_after"] and finished
                         and not window and case.get("closed_window") != finished)
        if stopping or window_closed:
            if case.get("last_cleanup") and current - timestamp(case["last_cleanup"]) < dt.timedelta(minutes=5):
                continue
            try:
                cleanup_case(state, case)
                if case["state"] == "cleaned" and window_closed:
                    case["closed_window"] = finished
            except (RuntimeError, subprocess.SubprocessError, OSError) as error:
                case.update(state="cleanup_pending", last_cleanup=now().isoformat(), error=str(error))
                persist(state)
            continue
        live = process_alive(case.get("pid"), str(WORK / case["config"]))
        if live and case.get("service_only") and active:
            stop_process(case)
            case["attempts"] = 0
            live = False
        if not enabled and live:
            case["state"] = "paused; risk monitor"
            if not service:
                stop_process(case)
                case["state"] = "paused"
            continue
        if not active and not service:
            if live:
                # Outside the operator's window: stop trading, keep positions unless close_after.
                stop_process(case)
            if case["state"] not in {"cleaned", "cleanup_pending"}:
                case["state"] = "paused" if not enabled else "waiting for window"
            continue
        owner = owners.get(case["account_id"])
        predecessor = state["cases"].get(case.get("predecessor"), {})
        if owner and owner != case["strategy_id"]:
            # Any owner that has closed its positions may hand the account over.
            previous = next((c for c in state["cases"].values() if c["strategy_id"] == owner), None)
            if previous and previous["state"] == "cleaned":
                case.update(predecessor=previous["kind"], ownership_transferred=False)
                predecessor = previous
        if owner and owner != case["strategy_id"] and not (predecessor.get("strategy_id") == owner and predecessor.get("state") == "cleaned"):
            case["state"] = "waiting for account owner"
            continue
        if live:
            refill_restart_budget(case, current)
        else:
            if case["state"] == "cleaned":
                case["attempts"] = 0
            retry_at = restart_not_before(case)
            if retry_at and current < retry_at:
                # Runtime сам переживает обрыв сети; сюда попадают редкие
                # аварийные выходы. Вместо отказа до конца окна — пауза и повтор.
                case.update(state="restart backoff", retry_at=retry_at.isoformat(timespec="seconds"))
                continue
            if not launch(state, case, service_only=not active):
                continue
        # The lease self-test starts a duplicate runtime; never against a real account.
        if active and window and not is_live(case) and runtime_ready(case) and current < timestamp(window["open"]) - dt.timedelta(minutes=15):
            try:
                verify_restart(state, case)
            except (RuntimeError, subprocess.SubprocessError, ValueError) as error:
                case["verification_error"] = str(error)
                persist(state)
        if case.get("restart_checked") and not case.get("restart_result") and runtime_ready(case):
            result = bot(state, case, "risk-status")
            if result.returncode == 0:
                case["restart_result"] = "passed" if json.loads(result.stdout)["epoch"] == case["epoch_before_restart"] else "failed"
                persist(state)
    if "collector" in clocks:
        ai_on = any(desired.get(c["strategy_id"], {}).get("enabled", True) and uses_ai_model(c)
                    for c in state["cases"].values() if c["state"] != "cleaned" or desired.get(c["strategy_id"], {}).get("enabled"))
        supervise_collector(state, ai_on, stopping)
    if stopping:
        state["phase"] = "complete" if all(c["state"] == "cleaned" for c in state["cases"].values() if not is_live(c)) else "cleanup_only"
    else:
        state["phase"] = "observing"
    if time.monotonic() - clocks["calendar"] > 3600:
        clocks["calendar"] = time.monotonic()
        try:
            refresh_calendar(state, api)
        except RuntimeError as error:
            # Retry in five minutes instead of an hour; the last known days stay in use.
            clocks["calendar"] = time.monotonic() - 3300
            supervisor_log("calendar refresh failed; using the last known days: " + str(error))
    if time.monotonic() - clocks["audit"] > 300:
        clocks["audit"] = time.monotonic()
        audit_accounts(state, api)
    # A pass without these clocks (tests, one-off ticks) skips disk and upkeep work.
    if time.monotonic() - clocks.get("disk", float("inf")) > DISK_CHECK_SECONDS:
        clocks["disk"] = time.monotonic()
        disk_status(state)
    if time.monotonic() - clocks.get("commissions", float("inf")) > COMMISSIONS_SECONDS:
        clocks["commissions"] = time.monotonic()
        try:
            control_plane.refresh_usd_rub(SimpleNamespace(**globals()), state, api)
            control_plane.calibrate_commissions(SimpleNamespace(**globals()), state, api)
        except (RuntimeError, ValueError, KeyError, OSError) as error:
            print("commission rates or the USD/RUB rate were not refreshed:", str(error)[:300], flush=True)
            clocks["commissions"] = time.monotonic() - COMMISSIONS_SECONDS + 600
    if time.monotonic() - clocks.get("maintenance", float("inf")) > 60:
        clocks["maintenance"] = time.monotonic()
        if maintenance_due(state, now()):
            maintenance(state)
    if time.monotonic() - clocks["report"] > 60:
        clocks["report"] = time.monotonic()
        report(state)
    persist(state)


def supervise():
    if (WORK / "MOVED_TO_COMPOSE").exists():
        raise RuntimeError("Campaign was exported to Compose; local trading is disabled")
    state = load()
    if not state.get("prepared"):
        raise RuntimeError("Run prepare first")
    lock = (WORK / "supervisor.lock").open("a")
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    state["supervisor_pid"] = os.getpid()
    state["phase"] = "observing"
    persist(state)
    # -i: без сна по простою, -m: диск не засыпает, -s: система не спит при питании
    # от сети. Закрытая крышка без внешнего монитора всё равно усыпит Mac.
    sleeper = subprocess.Popen(["/usr/bin/caffeinate", "-ims", "-w", str(os.getpid())]) if sys.platform == "darwin" else None
    # -inf: the first pass reports immediately even right after a VM boot (small monotonic clock).
    clocks = {"report": float("-inf"), "audit": float("-inf"), "calendar": float("-inf"), "disk": float("-inf"), "maintenance": float("-inf"), "collector": 0,
              "commissions": float("-inf")}
    api = API()
    control_plane.initialize(SimpleNamespace(**globals()), state)
    signal.signal(signal.SIGTERM, lambda *_: (WORK / "STOP").touch())
    signal.signal(signal.SIGINT, lambda *_: (WORK / "STOP").touch())
    last_pass = time.time()
    try:
        while True:
            if time.time() - last_pass > SUSPENSION_GAP:
                note_suspension(state, last_pass, time.time())
                persist(state)
            last_pass = time.time()
            if (WORK / "PARK").exists():
                stop_collector(state)
                for case in state["cases"].values():
                    stop_process(case)
                state["phase"] = "parked"
                report(state)
                return
            try:
                supervise_tick(state, api, clocks)
            except (RuntimeError, subprocess.SubprocessError, OSError, ValueError) as error:
                # Docker, PostgreSQL или сеть временно недоступны (например, после
                # сна компьютера). Наблюдатель не должен из-за этого останавливаться.
                supervisor_log("supervisor pass failed; retrying: " + str(error))
                time.sleep(10)
            time.sleep(1)
    finally:
        if sleeper:
            sleeper.terminate()
        persist(state)
        lock.close()


def start():
    if (WORK / "MOVED_TO_COMPOSE").exists():
        raise RuntimeError("Campaign was exported to Compose; local trading is disabled")
    state = load()
    if process_alive(state.get("supervisor_pid"), "sandbox-campaign.py supervise"):
        print("Supervisor already running")
        return
    if not state.get("prepared"):
        raise RuntimeError("Run prepare first")
    if (WORK / "STOP").exists():
        raise RuntimeError("Campaign is stopped; resume cleanup with supervise, not a fresh trading run")
    (WORK / "PARK").unlink(missing_ok=True)
    if state.get("phase") == "prepared" and now() >= timestamp(state["sessions"][0]["startTime"]) and not any(c["attempts"] for c in state["cases"].values()):
        state["sessions"] = scheduled_sessions(API())
        persist(state)
    with (WORK / "supervisor.log").open("a") as log:
        child = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), "supervise"], stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    print(f"Supervisor started: PID {child.pid}; first session {state['sessions'][0]['date']}")
    for warning in power_warnings():
        print("Warning: " + warning)


def power_warnings():
    """macOS sleep stops every runtime; warn when the session is likely to sleep."""
    if sys.platform != "darwin":
        return []
    try:
        battery = subprocess.run(["/usr/bin/pmset", "-g", "batt"], text=True, capture_output=True, timeout=5).stdout
    except (OSError, subprocess.SubprocessError):
        return []
    if "AC Power" in battery:
        return []
    return ["the Mac runs on battery: caffeinate -s cannot prevent sleep, and a closed lid will pause all strategies; "
            "connect power and keep the lid open (or use an external display) during trading sessions"]


def archive():
    """Move a finished or parked campaign aside so a new one can be prepared. Nothing is deleted."""
    if not (WORK / "campaign.json").exists():
        raise RuntimeError("No campaign to archive in " + str(WORK))
    state = load()
    if process_alive(state.get("supervisor_pid"), "sandbox-campaign.py supervise"):
        raise RuntimeError("Supervisor is running; finish with stop (liquidation) or park before archiving")
    running = [case["kind"] for case in state["cases"].values() if process_alive(case.get("pid"), str(WORK / case["config"]))]
    if running:
        raise RuntimeError("Strategy runtimes are still running: " + ", ".join(running))
    if state.get("prepared") and state.get("phase") not in {"complete", "parked"}:
        raise RuntimeError(f"Campaign phase is {state.get('phase')}; finish it with stop or park it before archiving")
    dashboard = WORK / "dashboard.json"
    if dashboard.exists():
        with contextlib.suppress(ValueError, OSError):
            if process_alive(json.loads(dashboard.read_text()).get("pid"), "dashboard/server.py serve"):
                raise RuntimeError("Dashboard is running; stop it first: python3 scripts/dashboard/server.py stop")
    destination = WORK.parent / "sandbox-campaign-archive" / state["id"]
    if destination.exists():
        raise RuntimeError("Archive already exists: " + str(destination))
    destination.parent.mkdir(parents=True, exist_ok=True)
    WORK.rename(destination)
    kept = f"PostgreSQL container {state['container']} and " if state.get("container") else ""
    print(f"Archived {state['id']} to {destination}. {kept}sandbox accounts are kept untouched.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["prepare", "start", "status", "stop", "park", "archive", "supervise"])
    parser.add_argument("--allow-account-reuse", action="store_true", help="Explicit approval to transfer cleaned accounts between nonoverlapping cohorts")
    parser.add_argument("--adopt-idle-accounts", action="store_true", help="Explicit approval to use already open empty sandbox accounts when the account limit is reached")
    args = parser.parse_args()
    action = args.action
    os.umask(0o077)
    WORK.mkdir(parents=True, exist_ok=True)
    if action == "prepare":
        prepare(args.allow_account_reuse, args.adopt_idle_accounts)
    elif action == "archive":
        archive()
    elif action == "start":
        start()
    elif action == "supervise":
        supervise()
    elif action == "park":
        if external_database():
            (WORK / "PARK").touch()
            print("Park requested")
        else:
            park_local()
            print("Campaign parked; orders and positions preserved for recovery")
    elif action == "stop":
        if (WORK / "MOVED_TO_COMPOSE").exists():
            raise RuntimeError("Campaign was exported to Compose; request cleanup in the container")
        (WORK / "STOP").touch()
        state = load()
        if not process_alive(state.get("supervisor_pid"), "sandbox-campaign.py supervise"):
            with (WORK / "supervisor.log").open("a") as log:
                subprocess.Popen([sys.executable, str(Path(__file__).resolve()), "supervise"], stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        print("Cleanup requested; supervisor will stop runtimes and reconcile accounts")
    else:
        state = load()
        print(json.dumps({"id": state["id"], "phase": state.get("phase"), "supervisor_running": process_alive(state.get("supervisor_pid"), "sandbox-campaign.py supervise"),
                          "sessions": state.get("sessions"), "cases": [{k: v for k, v in c.items() if k not in {"verified_defaults"}} for c in state["cases"].values()],
                          "report": str(WORK / "reports/latest.md")}, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, subprocess.SubprocessError, OSError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)
