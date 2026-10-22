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
extra = {"uid": EXTRA, "ticker": "TEST", "name": "Добавленная акция", "lot": 1, "benchmark": False, "kind": "share", "tradable": True, "reason": ""}
# Search results of other types: a continuous futures series and an option the stand does not trade.
brent = {"uid": "futures:BR", "ticker": "BR", "name": "Brent · непрерывный фьючерс", "lot": 1, "kind": "futures", "tradable": True, "reason": "",
         "continuous": True, "contract": "BRX6", "roll_at": "2026-10-28T07:00:00+00:00", "point_value": 828.879, "margin_buy": 9000.0}
option = {"uid": "33333333-3333-4333-8333-333333333333", "ticker": "SBER-OPT", "name": "Опцион на Сбербанк", "lot": 1, "kind": "option",
          "tradable": False, "reason": "этот тип инструментов стенд пока не торгует"}
started = now - dt.timedelta(minutes=20)
# Six future MOEX-like trading days and one weekend day for the schedule picker.
calendar = []
for offset in range(1, 8):
    day = (now + dt.timedelta(days=offset)).date().isoformat()
    trading = offset != 4
    calendar.append({"date": day, "isTradingDay": trading, "startTime": day + "T06:50:00Z" if trading else None, "endTime": day + "T15:39:59Z" if trading else None})
TRADING = [d["date"] for d in calendar if d["isTradingDay"]]


def window(days):
    first, last = next(d for d in calendar if d["date"] == days[0]), next(d for d in calendar if d["date"] == days[-1])
    return {"begin": (dt.datetime.fromisoformat(first["startTime"].replace("Z", "+00:00")) - dt.timedelta(hours=1)).isoformat(),
            "open": first["startTime"], "end": last["endTime"], "days": days}


case = {"kind": "pairs-trading", "strategy_id": "fixture", "account_id": "fixture-account", "state": "running", "running": True, "service_only": False, "last_started": started.isoformat(), "cohort": 0, "assets": [asset], "risk": {"budget": 1000000, "saved": True}, "owns_account": True, "readiness_observed": True, "launch_at": window([TRADING[0]])["begin"], "opening_at": window([TRADING[0]])["open"], "window_end_at": window([TRADING[0]])["end"],
        "schedule": {"run_mode": "schedule", "active": False, "close_after": True, "windows": [window([TRADING[0]])]}, "blocks": []}

# A portfolio already promoted to a real account: switched off, live trading not permitted.
LIVE = "2000123456"
live_case = {"kind": "live-pairs", "title": "Пары · реальный", "strategy_id": "fixture-live", "account_id": LIVE, "contour": "production",
             "state": "paused", "running": False, "service_only": False, "cohort": None, "assets": [asset],
             "risk": {"budget": 30000, "saved": False}, "owns_account": False, "readiness_observed": False, "preflight_result": "passed",
             "schedule": {"run_mode": "always", "active": False, "close_after": False, "windows": []}, "blocks": [], "source": "pairs-trading",
             "promoted_at": stamp, "risk_limits": {"budget": "30000.00", "daily_loss_percent": "2", "max_drawdown_percent": "10"}}

# A portfolio with one ai-model block on its own sandbox account.
ai_case = {"kind": "ai-news", "title": "ИИ по новостям", "strategy_id": "fixture-ai", "account_id": "ai-account", "contour": "sandbox", "ai_model": True,
           "state": "running", "running": True, "service_only": False, "cohort": None, "assets": [asset, extra],
           "risk": {"budget": 1000000, "saved": True}, "owns_account": True, "readiness_observed": True, "preflight_result": "passed",
           "schedule": {"run_mode": "always", "active": False, "close_after": False, "windows": []},
           "blocks": [{"share": "100", "instruments": [{"uid": UID, "ticker": "SBER"}, {"uid": EXTRA, "ticker": "TEST"}],
                       "strategies": [{"kind": "ai-model", "weight": "1", "params": {"model_profile": "claude", "instructions": "Покупай на хороших новостях"}}]}]}


def ago(**delta):
    return (now - dt.timedelta(**delta)).isoformat()


def order(request, side, lots, when):
    return {"id": request, "request_id": request, "account_id": "fixture-account", "strategy_id": "fixture", "strategy": "pairs-trading",
            "instrument_id": UID, "instrument": "SBER", "side": side, "status": "filled", "lots": lots, "lots_executed": lots,
            "created_at": when, "updated_at": when, "maintenance": False, "manual": False, "batch_id": None}


# Day candles cover the last 60 days: two priced fills and one fill without a price are inside,
# one fill is older than the chart and must be reported as outside the period.
orders = [order("req-buy", "buy", 3, ago(days=10)), order("req-sell", "sell", 2, ago(days=2)),
          order("req-old", "buy", 1, ago(days=200)), order("req-unpriced", "buy", 4, ago(hours=30))]
fills = [{"trade_id": "trade-buy", "request_id": "req-buy", "lots": 3, "price": "103.5", "executed_at": ago(days=10)},
         {"trade_id": "trade-sell", "request_id": "req-sell", "lots": 2, "price": "105.2", "executed_at": ago(days=2)},
         {"trade_id": "trade-old", "request_id": "req-old", "lots": 1, "price": "90", "executed_at": ago(days=200)}]
for fill in fills:
    fill.update(strategy="pairs-trading", account_id="fixture-account", instrument="SBER", instrument_id=UID,
                side=next(o["side"] for o in orders if o["request_id"] == fill["request_id"]), maintenance=False, manual=False)
FAKE_TOKEN = "t." + "FIXTURExNOTxAxREALxTOKEN0123456789"


def line(minutes, level, target, text):
    return f"{(started + dt.timedelta(minutes=minutes)).strftime('%Y-%m-%dT%H:%M:%S.%fZ')} {level:>5} {target}: {text}"


log_lines = [line(0, "INFO", "tinkoff_bot", "waiting for candle subscription acknowledgements strategy_id=fixture subscriptions=2"),
             line(0.1, "INFO", "tinkoff_rs::stream", "candle subscriptions acknowledged pending=1"),
             line(0.2, "INFO", "tinkoff_rs::stream", "candle subscriptions acknowledged pending=0"),
             line(1, "INFO", "tinkoff_bot", "warmup progress completed=2 total=2"),
             line(1.1, "INFO", "tinkoff_bot", "warmup complete; Sandbox runtime is live strategy_id=fixture components=1 kinds=pairs-trading"),
             line(5, "INFO", "tinkoff_bot", f"live decision observed event_id=candle:{UID}:day:{ago(days=1)} duplicate=false target=true legs=1"),
             line(6, "ERROR", "tinkoff_bot", f"command failed error=fixture failure with {FAKE_TOKEN}")]
log_lines += [line(10 + i, "WARN", "tinkoff_bot", f"decision uncovered: market orders unavailable event_id=risk-check:{i}") for i in range(3)]
OPEN_ID = f"schedule:SessionOpen:{now.date().isoformat()}:{now.date().isoformat()}T06:10:00+00:00"
log_lines += [line(13, "WARN", "tinkoff_bot", f"session event missed: the computer was asleep or the process was busy event_id={OPEN_ID}"),
              line(13.1, "INFO", "tinkoff_bot", f"session-open decision deferred; retrying every minute until the session closes event_id={OPEN_ID} until={now.date().isoformat()}T15:44:59Z"),
              line(14, "INFO", "tinkoff_bot", f"retrying deferred session-open decision event_id={OPEN_ID}:retry:1"),
              line(14.2, "INFO", "tinkoff_bot", "deferred session-open decision completed handled=Decided")]
ai_account = {"id": "ai-account", "contour": "sandbox", "owner": "ai-news", "cases": ["ai-news"], "available": True, "equity": 1000000, "cash": 800000, "positions": [], "active_orders": [], "operations": []}
live_account = {"id": LIVE, "contour": "production", "owner": None, "cases": ["live-pairs"], "available": True, "equity": 30000, "cash": 30000, "positions": [], "active_orders": [], "operations": []}
account = {"id": "fixture-account", "contour": "sandbox", "owner": "pairs-trading", "cases": ["pairs-trading"], "available": True, "equity": 1010000, "cash": 1000000, "positions": [], "active_orders": [], "operations": []}
control = {"available": True, "mode": "campaign", "max_runtimes": 4, "runtime_cap": 10, "budget_history": [],
           # The second row is another portfolio queued on the same account: its day is not selectable here.
           "cases": [{"strategy_id": "fixture", "account_id": "fixture-account", "enabled": True, "execution_mode": "confirm", "execution_budget": 1000000,
                      "run_mode": "schedule", "trading_days": [TRADING[0]], "close_after": True, "superseded_by": None},
                     {"strategy_id": "queued-portfolio", "account_id": "fixture-account", "enabled": True, "execution_mode": "auto", "execution_budget": None,
                      "run_mode": "schedule", "trading_days": [TRADING[1]], "close_after": True, "superseded_by": None},
                     {"strategy_id": "fixture-live", "account_id": LIVE, "contour": "production", "enabled": False, "execution_mode": "confirm",
                      "execution_budget": 30000, "run_mode": "always", "trading_days": [], "close_after": False, "superseded_by": None}],
           "commands": [], "catalog": [], "batches": [{"batch_id": PLAN, "status": "awaiting", "revision": 0}], "ranking": {"clean": [], "mixed": [], "unranked": [], "history": []}}
control["live"] = {"status": {"token": True, "halted": False, "ready": True},
                   "settings": {"min_sessions": 5, "min_fills": 1, "min_ai_decisions": 5, "require_no_risk_block": True},
                   "halted": False, "halted_at": None, "halt_reason": None, "accounts_observed_at": stamp, "accounts_error": None,
                   "accounts": [{"id": "2000999999", "name": "Робот", "type": "ACCOUNT_TYPE_TINKOFF", "status": "ACCOUNT_STATUS_OPEN",
                                 "access_level": "ACCOUNT_ACCESS_LEVEL_FULL_ACCESS", "cash_rub": 50000, "holdings": 0, "orders": 0, "eligible": True, "problems": []},
                                {"id": LIVE, "name": "Пары", "type": "ACCOUNT_TYPE_TINKOFF_IIS", "status": "ACCOUNT_STATUS_OPEN",
                                 "access_level": "ACCOUNT_ACCESS_LEVEL_FULL_ACCESS", "cash_rub": 30000, "holdings": 0, "orders": 0, "eligible": False,
                                 "problems": ["уже подключён к стенду: live-pairs"]}],
                   "gate": [{"strategy_id": "fixture", "kind": "pairs-trading", "title": "pairs-trading", "enabled": True, "sessions": 6, "fills": 2, "risk_blocks": 0,
                             "last_block_at": None, "failures": [], "passed": True}]}
control["cases"].append({"strategy_id": "fixture-ai", "account_id": "ai-account", "contour": "sandbox", "enabled": True, "execution_mode": "auto",
                         "execution_budget": None, "run_mode": "always", "trading_days": [], "close_after": False, "superseded_by": None})
control["ai_models"] = {"path": "/run/ai/models.toml", "exists": True, "default": "claude", "models": [
    {"name": "claude", "provider": "anthropic", "model": "claude-opus-5-5", "url": "https://api.anthropic.com/v1", "token": "file", "web_search": True, "default": True,
     "prices": {"input": 4, "output": 20, "cache_read": 0.2, "cache_write": 5, "web_search_per_1000": 10}, "prices_source": "built-in"},
    {"name": "local", "provider": "openai", "model": "qwen2.5:14b", "url": "http://host.docker.internal:11434/v1", "token": "env:LOCAL_KEY", "web_search": False, "default": False,
     "prices": None, "prices_source": None}], "budget_in_file": 5}
today = now.astimezone(dt.timezone(dt.timedelta(hours=3))).date().isoformat()
past_day = (now.astimezone(dt.timezone(dt.timedelta(hours=3))) - dt.timedelta(days=2)).date().isoformat()
control["ai_usage"] = [{"day": today, "purpose": "decision", "cost_rub": 103.6, "unpriced": 0, "requests": 40, "input_tokens": 900000, "cache_read_tokens": 700000, "output_tokens": 20000},
                       {"day": today, "purpose": "tagging", "cost_rub": 4.14, "unpriced": 0, "requests": 30, "input_tokens": 300000, "cache_read_tokens": 0, "output_tokens": 30000},
                       {"day": past_day, "purpose": "decision", "cost_rub": 207.2, "unpriced": 2, "requests": 80, "input_tokens": 1800000, "cache_read_tokens": 1000000, "output_tokens": 40000}]
control["ai_theses"] = [
    {"id": "t1", "strategy_id": "fixture-ai", "ticker": "SBER", "direction": "long", "thesis": "Снижение ставки поддержит маржу банка",
     "horizon_days": 60, "entry_price": 280.5, "conditions": [{"kind": "price_below", "value": 265}, {"kind": "date", "value": "2026-12-20"}],
     "opened_at": ago(days=3), "closed_at": None, "close_reason": None, "close_price": None, "result_percent": None, "inherited": False},
    {"id": "t2", "strategy_id": "fixture-ai", "ticker": "TEST", "direction": "short", "thesis": "Слабый отчёт", "horizon_days": None,
     "entry_price": 100, "conditions": [], "opened_at": ago(days=10), "closed_at": ago(days=1), "close_reason": "отчёт вышел",
     "close_price": 94, "result_percent": 6.0, "inherited": True}]
control["ai_memory"] = [{"id": "m2", "strategy_id": "fixture-ai", "note": "Внеочередные решения на шуме цены чаще проигрывают индексу",
                         "tickers": [], "created_at": ago(days=1), "expires_at": None, "inherited_from": None, "kind": "lesson"},
                        {"id": "m1", "strategy_id": "fixture-ai", "note": "SBER: держу до отчёта, выход при пробое 270", "tickers": ["SBER"],
                          "created_at": ago(days=2), "expires_at": None, "inherited_from": "fixture-ai-old"}]
control["ai_decisions"] = [
    {"id": "d3", "strategy_id": "fixture-ai", "account_id": "ai-account", "kind": "decision", "status": "ok", "session_date": None, "profile": "claude",
     "provider": "anthropic", "model": "claude:claude-opus-5-5", "weights": [], "rationale": "Новости уже в цене, держу позиции.", "sources": [],
     "tool_calls": {"calls": [{"tool": "get_news"}], "details": "4 новостей о бумагах блока за 30 мин: Сбербанк отчитался"}, "error": None,
     "duration_ms": 9000, "created_at": ago(minutes=2), "trigger": "news_burst", "hold": True, "cost_usd": 0.0412, "cost_rub": 3.41,
     "usage": {"requests": 5, "input_tokens": 3000, "cache_read_tokens": 40000, "cache_write_tokens": 6000, "output_tokens": 900, "web_searches": 0}},
    {"id": "d4", "strategy_id": "fixture-ai", "account_id": "ai-account", "kind": "decision", "status": "failed", "session_date": None, "profile": "claude",
     "provider": None, "model": None, "weights": [], "rationale": None, "sources": [], "tool_calls": {"details": "SBER +2.4% с прошлого решения"},
     "error": "daily AI budget exhausted: 503 of 500 RUB spent; out-of-schedule decisions resume tomorrow", "duration_ms": 0, "created_at": ago(minutes=1), "trigger": "price_jump", "hold": False},
    {"id": "d2", "strategy_id": "fixture-ai", "account_id": "ai-account", "kind": "decision", "status": "failed", "session_date": None, "profile": "claude",
     "provider": None, "model": None, "weights": [], "rationale": None, "sources": [], "tool_calls": {}, "error": "claude (claude-opus-5-5): HTTP 529: overloaded",
     "duration_ms": 1500, "created_at": ago(minutes=5)},
    {"id": "d1", "strategy_id": "fixture-ai", "account_id": "ai-account", "kind": "decision", "status": "ok", "session_date": None, "profile": "claude",
     "provider": "anthropic", "model": "claude:claude-opus-5-5", "weights": [{"ticker": "SBER", "instrument_id": UID, "weight_percent": 20}, {"ticker": "TEST", "instrument_id": EXTRA, "weight_percent": 0}],
     "rationale": "Сбербанк повысил прогноз прибыли, RSI 55 — тренд вверх без перекупленности.",
     "outcome": {"1d": {"portfolio": 0.42, "imoex": 0.1}, "5d": {"portfolio": -0.3, "imoex": 0.8}},
     "sources": [{"id": "tinvest:1001", "title": "Сбербанк: решения совета директоров о дивидендах", "source": "eDisclosure", "sources": ["eDisclosure"], "kind": "disclosure",
                  "published": ago(minutes=30), "read": True, "cited": True},
                 {"title": "Сбербанк повысил прогноз", "link": "https://example.ru/news/1", "source": "example.ru", "published": ago(hours=2)},
                 {"title": "javascript ссылка", "link": "javascript:alert(1)", "source": "bad", "published": ago(hours=3)}],
     "tool_calls": {"calls": [{"tool": "get_news"}, {"tool": "get_indicators"}], "feed_errors": [], "confidence_percent": 70},
     "duration_ms": 42000, "created_at": ago(hours=1)}]
run = {"id": "fixture-run", "strategy_id": "fixture", "strategy": "pairs-trading", "classification": "clean", "started_at": stamp, "principal": 1000000, "pnl_rub": 10000, "return_percent": 1, "max_drawdown_percent": .25, "fills": 2, "rank": 1, "ranked": True}
control["ranking"]["clean"] = [run]
control["ranking"]["history"] = [run]
past = (now - dt.timedelta(days=1)).date().isoformat()
calendar.insert(0, {"date": past, "isTradingDay": True, "startTime": past + "T06:10:00Z", "endTime": past + "T15:54:59Z"})
suspensions = [{"from": past + "T00:00:00Z", "to": past + "T15:50:00Z"}]
storage = {"disk": {"level": "warn", "free_bytes": 2 * 1024 ** 3, "path": "/run/ai", "database_bytes": 23 * 1024 ** 2, "checked_at": stamp,
                    "readings": [{"path": "/run/ai", "free_bytes": 2 * 1024 ** 3, "total_bytes": 228 * 1024 ** 3}, {"path": "/data", "free_bytes": 14 * 1024 ** 3, "total_bytes": 98 * 1024 ** 3}]},
           "maintenance": {"day": today, "at": ago(hours=5), "deleted": {"risk_check_events": 5600, "performance_samples": 0, "ai_usage": 0}, "rotated_logs": [], "database_bytes": 23 * 1024 ** 2, "seconds": 1.4},
           "backups": {"status": {"at": ago(hours=4), "ok": True, "message": "tinkoff_campaign-20261020.dump 5000000 bytes"},
                       "files": [{"name": "tinkoff_campaign-20261020.dump", "bytes": 5000000, "at": ago(hours=4)}]}}
# An old scenario that gave its account to a newer portfolio: shown in the archive only.
old_case = {"kind": "old-scenario", "strategy_id": "fixture-old", "account_id": "fixture-account", "state": "cleaned", "running": False,
            "service_only": False, "cohort": 1, "assets": [asset], "risk": {"budget": None, "saved": False}, "owns_account": False,
            "readiness_observed": False, "preflight_result": None, "contour": "sandbox", "schedule": {"run_mode": "always", "active": False, "close_after": False, "windows": []},
            "blocks": [], "restart": None, "lease": None}
control["ai_now"] = [{"strategy_id": "fixture-ai", "activity": {"state": "thinking", "trigger": "news_burst", "started_at": ago(minutes=1), "profile": "claude"},
                      "last": {"created_at": ago(minutes=40), "status": "ok", "trigger": "scheduled", "hold": False, "weights": [{"ticker": "SBER", "weight_percent": 20}],
                               "cost_rub": 3.3, "model": "claude-opus-5-5", "profile": "claude"},
                      "today": {"decisions": 3, "failures": 1, "cost_rub": 9.95}}]
control["ai_rating"] = [{"model": "claude-opus-5-5", "provider": "anthropic", "profiles": "claude", "strategies": ["fixture-ai"], "decisions": 14, "failures": 1, "holds": 5,
                         "avg_ms": 31000, "cost_rub": 58.0, "h5": {"n": 10, "beat": 7, "excess": 0.42}, "h20": {"n": 0, "beat": 0, "excess": None}},
                        {"model": "qwen2.5:14b", "provider": "openai", "profiles": "local", "strategies": [], "decisions": 6, "failures": 0, "holds": 2,
                         "avg_ms": 9000, "cost_rub": 0, "h5": {"n": 4, "beat": 1, "excess": -0.3}, "h20": {"n": 0, "beat": 0, "excess": None}}]
control["min_trade_percent"] = "0.5"
control["ai_budget_rub"] = "500.00"
control["usd_rub"] = {"currency": "usd", "rub": 82.8879, "source": "T-Invest USD000UTSTOM", "observed_at": stamp}
control["commissions"] = [
    {"contour": "production", "kind": "default", "rate_percent": 0.05, "source": "tariff", "tariff": "trader", "trades": 0, "volume_rub": 0, "commission_rub": 0, "updated_at": stamp},
    {"contour": "production", "kind": "futures", "rate_percent": 0.04, "source": "tariff", "tariff": "trader", "trades": 0, "volume_rub": 0, "commission_rub": 0, "updated_at": stamp},
    {"contour": "sandbox", "kind": "share", "rate_percent": 0.05, "source": "measured", "tariff": "sandbox", "trades": 12, "volume_rub": 2548596, "commission_rub": 1274.3, "updated_at": stamp}]
snapshot = {"campaign": {"id": "BROWSER FIXTURE — NO TRADING", "phase": "observing", "supervisor_running": True, "sessions": [], "calendar": calendar, "suspensions": suspensions}, "cases": [case, live_case, ai_case, old_case], "accounts": [account, live_account, ai_account], "orders": orders, "fills": fills, "targets": [{"strategy": "pairs-trading", "batch_id": PLAN, "status": "planned", "generated_at": stamp, "valid_until": (now + dt.timedelta(hours=1)).isoformat(), "weights": []}], "warnings": [], "updated": {"broker": stamp, "database": stamp, "campaign": stamp}, "history": [], "storage": storage, "control": control}


documents = [
    {"id": "tinvest:1001", "source": "eDisclosure", "kind": "disclosure", "title": "Сбербанк: решения совета директоров о дивидендах", "url": "",
     "published_at": ago(minutes=30), "tickers": ["SBER"], "priority": True, "importance": None, "status": "new", "preview": "Совет директоров рекомендовал дивиденды 37 рублей на акцию", "sources": 1,
     "lead": "", "body": "Совет директоров рекомендовал дивиденды 37 рублей на акцию.\nДата закрытия реестра — 18 июля.", "tables": ["Показатель | Значение"],
     "fetched_at": ago(minutes=29), "event_type": "dividends", "summary": "Совет рекомендовал 37 ₽ на акцию.", "importance": 4,
     "topics": [{"topic": "dividends", "direction": 1, "strength": 2}], "same_event": [{"id": "rss:abc", "source": "kommersant.ru", "title": "Сбер рекомендовал дивиденды", "published_at": ago(minutes=20)}]},
    {"id": "tinvest:1002", "source": "tass", "kind": "news", "title": "Нефть Brent подорожала до 110 долларов", "url": "https://example.ru/oil", "published_at": ago(hours=2),
     "tickers": [], "priority": False, "importance": None, "status": "new", "preview": "Котировки растут на фоне перебоев поставок", "sources": 3}]
control["collector"] = [{"source": "tinvest", "last_success": ago(minutes=1), "last_error": None, "added_last": 12, "added_total": 1370},
                        {"source": "rss", "last_success": ago(minutes=3), "last_error": "finam.ru: unavailable", "added_last": 4, "added_total": 260}]
control["topics"] = dashboard.control_module.TOPICS
control["exposures"] = [
    {"ticker": "LKOH", "name": "ЛУКОЙЛ", "sector": "нефть и газ", "note": "Экспортёр нефти: выигрывает от роста Brent и слабого рубля.",
     "drafted_at": ago(days=1), "confirmed_at": None, "draft_error": None,
     "exposures": {"oil": {"value": 2.0, "note": "основной доход", "origin": "model"}, "ruble": {"value": -1.0, "note": "экспортная выручка", "origin": "model"},
                   "sanctions": {"value": 1.5, "note": "", "origin": "operator"}},
     "measured": {"oil": {"suggested": 1.0, "effect": 0.62, "correlation": 0.41, "observations": 14, "at": ago(hours=2)},
                  "ruble": {"suggested": 0.5, "effect": 0.3, "correlation": 0.2, "observations": 10, "at": ago(hours=2)},
                  "sanctions": {"suggested": 1.5, "effect": 1.1, "correlation": 0.3, "observations": 9, "at": ago(hours=2)}}},
    {"ticker": "SBER", "name": "Сбербанк", "sector": "банки", "note": "", "drafted_at": None, "confirmed_at": None, "draft_error": None, "exposures": {},
     "measured": {"key_rate": {"suggested": -1.5, "effect": -1.2, "correlation": -0.52, "observations": 11, "at": ago(hours=2)}}}]
control["documents"] = {"total": 1630, "day": 1412, "newest": ago(minutes=1), "kinds": {"news": 1250, "disclosure": 121, "article": 41}, "sources": ["eDisclosure", "tass", "kommersant.ru"]}


class FakeControl:
    def data(self):
        return control

    def documents(self, params):
        text = params.get("q", "").lower()
        return {"documents": [d for d in documents if text in d["title"].lower() and (not params.get("kind") or d["kind"] == params["kind"])]}

    def document(self, document_id):
        return next(d for d in documents if d["id"] == document_id)

    def search(self, _, kind=""):
        return {"instruments": [i for i in [extra, brent, option] if not kind or i["kind"] == kind]}

    def get_command(self, command_id):
        return {"command": next(c for c in control["commands"] if c["id"] == command_id)}

    def enqueue(self, value):
        state = {"cases": {"fixture": case, "live-pairs": live_case, "ai-news": ai_case}, "calendar": calendar}
        command = dashboard.control_module.validate(value, state)
        existing = next((c for c in control["commands"] if c["id"] == command["id"]), None)
        if existing:
            return {"command": existing}
        kind, payload = command["kind"], command["payload"]
        result = {"message": "Fixture only"}
        row = next((r for r in control["cases"] if r["strategy_id"] == command["strategy_id"]), control["cases"][0])
        if kind == "schedule":
            # The real day, past-session and same-account overlap checks.
            dashboard.control_module.check_schedule(state, payload, row, [r for r in control["cases"][1:] if r["account_id"] == row["account_id"]])
            result = {"before": {k: row[k] for k in ["run_mode", "trading_days", "close_after"]}, "message": "Fixture schedule"}
            row.update(run_mode=payload["run_mode"], trading_days=payload["days"], close_after=payload["close_after"])
            case["schedule"] = {"run_mode": payload["run_mode"], "active": False, "close_after": payload["close_after"],
                                "windows": [window([d]) for d in payload["days"]] if payload["run_mode"] == "schedule" else []}
        elif kind == "ai_budget":
            result = {"before": {"rub": control["ai_budget_rub"]}, "after": payload}
            control["ai_budget_rub"] = payload["rub"]
        elif kind == "trade_band":
            result = {"before": {"percent": control["min_trade_percent"]}, "after": payload}
            control["min_trade_percent"] = payload["percent"]
        elif kind == "runtime_limit":
            result = {"before": {"value": control["max_runtimes"]}, "after": payload}
            control["max_runtimes"] = payload["value"]
        elif kind == "budget":
            previous = row["execution_budget"]
            row["execution_budget"] = float(payload["amount"])
            result = {"execution_budget": payload["amount"], "previous_budget": str(previous), "delta": str(float(payload["amount"]) - previous)}
        elif kind == "portfolio":
            row["enabled"] = False
        elif kind == "live_halt":
            control["live"].update(halted=True, halted_at=stamp, halt_reason=payload["reason"])
            control["live"]["status"].update(halted=True, ready=False)
            control["cases"][2]["enabled"] = False
        elif kind == "live_resume":
            control["live"].update(halted=False, halt_reason=None)
            control["live"]["status"].update(halted=False, ready=True)
        elif kind == "ai_check":
            result = {"message": "Модель ответила за 300 мс: готов", "reply": "готов", "latency_ms": 300}
        elif kind == "ai_preview":
            result = {"message": "Пробный анализ: SBER 10%"}
            control["ai_decisions"].insert(0, dict(control["ai_decisions"][1], id="preview", kind="preview", created_at=stamp,
                                                   weights=[{"ticker": "SBER", "instrument_id": UID, "weight_percent": 10}]))
        elif kind == "live_gate":
            control["live"]["settings"] = dict(payload)
        elif kind == "exposure_set":
            profile = next(x for x in control["exposures"] if x["ticker"] == payload["ticker"])
            if payload["sensitivity"] is None:
                profile["exposures"].pop(payload["topic"], None)
            else:
                profile["exposures"][payload["topic"]] = {"value": float(payload["sensitivity"]), "note": payload["note"], "origin": "operator"}
        elif kind == "exposure_confirm":
            next(x for x in control["exposures"] if x["ticker"] == payload["ticker"])["confirmed_at"] = stamp
        if kind in {"pause", "manual_order"}:
            row["enabled"] = False
        elif kind == "resume":
            row["enabled"] = True
        elif kind == "mode":
            control["mode"] = payload["mode"]
        elif kind == "catalog_add":
            control["catalog"] = [extra]
        command.update(status="succeeded", result=result, created_at=stamp, updated_at=stamp)
        control["commands"].insert(0, command)
        if kind == "budget":
            control["budget_history"].insert(0, command)
        return {"command": command}


with tempfile.TemporaryDirectory(prefix="dashboard-browser-") as temporary:
    work = Path(temporary)
    (work / "reports").mkdir()
    assets = {}
    for a in [asset, extra]:
        series = {}
        for interval, seconds in {"5s": 5, "10s": 10, "30s": 30, "5m": 300, "30m": 1800, "day": 86400}.items():
            # 5-second candles: more than one screen, to exercise the visible window, pan and zoom.
            total = 600 if interval == "5s" else 60
            candles = [{"time": (now-dt.timedelta(seconds=seconds*(total-i))).isoformat(), "open": 100+i/10, "high": 101+i/10, "low": 99+i/10, "close": 100.5+i/10 if i % 3 else 99.5+i/10, "volume": 10, "complete": True} for i in range(total)]
            series[interval] = {"candles": candles, "observed_at": stamp, "error": False}
        assets[a["uid"]] = {"ticker": a["ticker"], "series": series}
    (work / "reports/market-chart.json").write_text(json.dumps({"assets": assets}))
    (work / "logs").mkdir()
    (work / "logs/pairs-trading.log").write_text("\n".join(log_lines) + "\n")
    server = ThreadingHTTPServer(("127.0.0.1", 18766), dashboard.Handler)
    server.feed = SimpleNamespace(work=work, data=lambda: json.dumps(snapshot).encode())
    server.control = FakeControl()
    print("Browser fixture listening on 18766; no broker or database", flush=True)
    server.serve_forever()
