"""Telegram notifications of the stand: trades, risk blocks, futures rolls, AI model
failures, problems of processes and backups, and a daily summary.

The supervisor calls `tick` every minute. Without the settings file (NOTIFY_CONFIG,
KEY=VALUE lines with TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID) nothing is sent.
What was already reported is remembered in <work>/notify.json, so a restart does
not repeat messages. Standard library only; the token never reaches logs.
"""
import datetime as dt
import html
import json
from pathlib import Path
import urllib.error
import urllib.request

MSK = dt.timezone(dt.timedelta(hours=3))
REQUIRED = ("TELEGRAM_BOT_TOKEN", "TELEGRAM_CHAT_ID")
# Telegram limits a message to 4096 characters.
LIMIT = 4000


def load_config(path):
    try:
        text = Path(path).read_text()
    except OSError:
        return None
    config = {}
    for line in text.splitlines():
        line = line.strip()
        if line and not line.startswith("#") and "=" in line:
            key, value = line.split("=", 1)
            config[key.strip()] = value.strip().strip('"').strip("'")
    missing = [key for key in REQUIRED if not config.get(key)]
    if missing:
        raise ValueError("notification settings miss " + ", ".join(missing))
    config.setdefault("NOTIFY_DAILY_HOUR", "19")
    config.setdefault("NOTIFY_TRADES", "yes")
    return config


def send(config, text):
    """One message through the Bot API; the URL with the token is never logged."""
    body = json.dumps({"chat_id": config["TELEGRAM_CHAT_ID"], "text": text[:LIMIT], "parse_mode": "HTML",
                       "disable_web_page_preview": True}).encode()
    request = urllib.request.Request(f"https://api.telegram.org/bot{config['TELEGRAM_BOT_TOKEN']}/sendMessage",
                                     data=body, headers={"Content-Type": "application/json"}, method="POST")
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            response.read()
    except urllib.error.HTTPError as error:
        raise RuntimeError(f"Telegram answered HTTP {error.code}") from None
    except (urllib.error.URLError, OSError) as error:
        raise RuntimeError(f"Telegram is unreachable: {getattr(error, 'reason', error)}") from None


def literal(value):
    return "'" + str(value).replace("'", "''") + "'"


EVENTS_SQL = """SELECT json_build_object(
  'fills', coalesce((SELECT json_agg(t ORDER BY t.executed_at) FROM (
     SELECT f.executed_at, i.strategy_id, i.instrument_id, i.side, f.lots, f.price, coalesce(b.event_id, i.event_id) AS event_id
     FROM fills f JOIN trade_intents i ON i.request_id = f.request_id LEFT JOIN rebalance_batches b ON b.id = i.batch_id
     WHERE f.executed_at > {since} ORDER BY f.executed_at LIMIT 200) t), '[]'),
  'blocks', coalesce((SELECT json_agg(r ORDER BY r.recorded_at) FROM (
     SELECT strategy_id, recorded_at, reason, detail FROM risk_block_events WHERE recorded_at > {since} LIMIT 50) r), '[]'),
  'ai_failures', coalesce((SELECT json_agg(a ORDER BY a.created_at) FROM (
     SELECT strategy_id, created_at, left(error, 300) AS error FROM ai_decisions
     WHERE created_at > {since} AND kind = 'decision' AND status <> 'ok' LIMIT 50) a), '[]'))"""


SUMMARY_SQL = """SELECT coalesce(json_agg(json_build_object('account_id', r.account_id, 'equity', r.last_equity,
  'start', (SELECT (s.state->>'day_start_equity')::numeric FROM account_risk_state s WHERE s.account_id = r.account_id))), '[]')
  FROM strategy_runs r WHERE r.ended_at IS NULL AND r.last_equity IS NOT NULL"""


def events(campaign, state, since):
    output = campaign.run_sql(state, EVENTS_SQL.format(since=literal(since.isoformat()) + "::timestamptz"))
    return json.loads(output) if output.strip() else {"fills": [], "blocks": [], "ai_failures": []}


def names(state):
    """Portfolio titles by strategy id and tickers by instrument id."""
    cases = state.get("cases", {}).values()
    portfolios = {c["strategy_id"]: c.get("title") or c["kind"] for c in cases}
    tickers = {i["uid"]: i.get("ticker") or i["uid"] for i in state.get("instruments", [])}
    for case in cases:
        for block in case.get("blocks", []):
            for uid in block.get("instruments", []):
                if isinstance(uid, str) and uid.startswith("futures:"):
                    tickers.setdefault(uid, uid.split(":", 1)[1])
    return portfolios, tickers


def messages(found, state, config, seen_errors):
    """Texts for new events. AI errors repeat at most once a day per portfolio and text."""
    portfolios, tickers = names(state)
    out = []
    if config.get("NOTIFY_TRADES", "yes").lower() not in ("no", "false", "0"):
        by_portfolio = {}
        for fill in found["fills"]:
            by_portfolio.setdefault(fill["strategy_id"], []).append(fill)
        for strategy, fills in by_portfolio.items():
            lines = []
            for f in fills:
                action = "покупка" if "BUY" in str(f["side"]).upper() else "продажа"
                kind = str(f.get("event_id") or "")
                note = " (перенос фьючерса)" if kind.startswith("roll:") else " (защитное закрытие)" if kind.startswith(("risk:", "cleanup:")) else " (ручная)" if kind.startswith("manual:") else ""
                lines.append(f"• {action} {html.escape(str(tickers.get(f['instrument_id'], f['instrument_id'])))} × {f['lots']} лот. по {float(f['price']):g}{note}")
            out.append(f"<b>{html.escape(portfolios.get(strategy, strategy))}</b>: сделки\n" + "\n".join(lines[:30])
                       + (f"\n… и ещё {len(lines) - 30}" if len(lines) > 30 else ""))
    for block in found["blocks"]:
        out.append(f"⚠️ <b>{html.escape(portfolios.get(block['strategy_id'], block['strategy_id']))}</b>: риск-блокировка — "
                   f"{html.escape(str(block.get('reason') or ''))}. Новые позиции не открываются, позиции закрываются.")
    day = dt.datetime.now(MSK).date().isoformat()
    for failure in found["ai_failures"]:
        key = f"{day}|{failure['strategy_id']}|{(failure.get('error') or '')[:80]}"
        if key in seen_errors:
            continue
        seen_errors.add(key)
        error = failure.get("error") or ""
        reason = ("на счёте провайдера закончились средства" if "credit balance" in error
                  else "исчерпан дневной лимит расходов" if "budget exhausted" in error else html.escape(error[:200]))
        out.append(f"🤖 <b>{html.escape(portfolios.get(failure['strategy_id'], failure['strategy_id']))}</b>: модель не приняла решение — {reason}")
    return out


def problems(state, work):
    """Ongoing troubles worth one message when they appear: stopped processes,
    critical disk space, failed copies. Keys identify each trouble."""
    out = {}
    for kind, case in state.get("cases", {}).items():
        if case.get("state") in ("restart backoff", "failed"):
            out[f"process|{kind}"] = f"❗ Процесс портфеля <b>{html.escape(case.get('title') or kind)}</b> упал и перезапускается"
    if (state.get("disk") or {}).get("level") == "critical":
        out["disk"] = "❗ На диске критически мало места: сбор документов и копии остановлены"
    for name, label in (("status.json", "резервная копия"), ("offsite.json", "копия вне компьютера")):
        try:
            status = json.loads((Path(work) / "backups" / name).read_text())
        except (OSError, ValueError):
            continue
        if status.get("ok") is False:
            out[f"backup|{name}"] = f"❗ Не удалась {label}: {html.escape(str(status.get('message') or status.get('error') or ''))[:200]}"
    return out


def daily_summary(state, accounts):
    """Value of each running portfolio's account and its change over the day."""
    lines = []
    for case in state.get("cases", {}).values():
        account = accounts.get(case["account_id"])
        if not account or case.get("state") != "running":
            continue
        equity, start = account
        change = f" ({(equity / start - 1) * 100:+.2f}% за день)" if start else ""
        lines.append(f"• {html.escape(case.get('title') or case['kind'])}: {equity:,.0f} ₽{change}".replace(",", " "))
    return "📊 <b>Итоги дня</b>\n" + ("\n".join(lines) if lines else "работающих портфелей нет")


def tick(campaign, state, work, config, now=None, sender=send):
    """New events since the last call, troubles that appeared, the daily summary."""
    now = now or dt.datetime.now(dt.timezone.utc)
    path = Path(work) / "notify.json"
    try:
        memory = json.loads(path.read_text())
    except (OSError, ValueError):
        # The first run starts from now: history is not replayed.
        memory = {"since": now.isoformat(), "problems": [], "errors": [], "summary_day": None}
    since = dt.datetime.fromisoformat(memory["since"])
    found = events(campaign, state, since)
    seen_errors = set(memory.get("errors", []))
    texts = messages(found, state, config, seen_errors)
    current = problems(state, work)
    texts += [text for key, text in current.items() if key not in memory.get("problems", [])]
    moscow = now.astimezone(MSK)
    if moscow.hour >= int(config.get("NOTIFY_DAILY_HOUR", 19)) and memory.get("summary_day") != moscow.date().isoformat():
        rows = json.loads(campaign.run_sql(state, SUMMARY_SQL) or "[]")
        accounts = {a["account_id"]: (float(a["equity"]), float(a["start"] or 0)) for a in rows if a.get("equity") is not None}
        texts.append(daily_summary(state, accounts))
        memory["summary_day"] = moscow.date().isoformat()
    # Messages go in one batch; the cursor moves only after they were sent.
    if texts:
        chunk = ""
        for text in texts:
            if len(chunk) + len(text) + 2 > LIMIT:
                sender(config, chunk)
                chunk = ""
            chunk += ("\n\n" if chunk else "") + text
        if chunk:
            sender(config, chunk)
    times = [dt.datetime.fromisoformat(str(item[key]).replace("Z", "+00:00"))
             for group, key in (("fills", "executed_at"), ("blocks", "recorded_at"), ("ai_failures", "created_at"))
             for item in found[group]]
    memory["since"] = max([since] + times).isoformat()
    memory["problems"] = sorted(current)
    memory["errors"] = sorted(e for e in seen_errors if e.startswith(moscow.date().isoformat()))
    path.write_text(json.dumps(memory, ensure_ascii=False))
    return texts
