"""Supervisor-side durable mode, schedule conversion and immutable revisions."""
import datetime as dt
from decimal import Decimal, InvalidOperation
import json
from pathlib import Path
import re
import uuid

# Instruments each builtin kind needs in its own block: (minimum, maximum or None).
STRATEGY_KINDS = {
    "moving-average-trend": (1, None), "range-breakout": (1, None), "time-series-momentum": (1, None),
    "cross-sectional-momentum": (10, None), "intermediate-momentum": (10, None), "fifty-two-week-high": (10, None),
    "residual-momentum": (10, None), "pairs-trading": (2, None), "short-term-reversal": (10, None),
    "market-intraday-momentum": (1, 1), "intraday-same-clock": (10, None), "low-volatility": (10, None),
    "betting-against-beta": (10, None), "equal-risk-contribution": (2, None), "volatility-managed": (4, None),
    "value-composite": (10, None), "quality-profitability": (10, None), "risk-managed-momentum": (10, None),
    "return-seasonality": (10, None), "dividend-month": (1, None), "ai-model": (1, 50),
}
# Settings of an `ai-model` block the operator may change; the model connection
# itself (provider, url, token) lives in the AI model profiles file.
AI_PARAMS = {"model_profile", "instructions", "news_hours", "max_tool_rounds", "max_weight_percent", "allow_short",
             "decision_interval_minutes", "news_burst_items", "price_jump_percent", "material_importance",
             "market_move_percent", "commodity_move_percent", "currency_move_percent"}
PROFILE_NAME = re.compile(r"[A-Za-z0-9_-]{1,40}")
# Rules shared with the bot and the dashboard; build.rs turns the same file into
# Rust constants, so they cannot drift apart.
RULES = json.loads((Path(__file__).resolve().parents[1] / "contracts" / "stand-rules.json").read_text())
# Strategies computed from company reports and dividends trade shares only.
SHARE_ONLY_KINDS = frozenset(RULES["share_only_kinds"])
# A continuous futures series: the stand trades its current contract and rolls
# to the next one ROLL_DAYS before its last trading day, at the start of the
# main session (`futures:BR`, `futures:Si`).
SERIES_ID = re.compile(re.escape(RULES["futures"]["series_prefix"]) + r"([A-Za-z0-9]{1,4})")
# Dated MOEX futures: two-character code, month letter, year digit (BRX6, SiZ6).
DATED_FUTURES = re.compile(r"([A-Za-z0-9]{2})[" + RULES["futures"]["month_letters"] + r"]\d")
ROLL_DAYS = RULES["futures"]["roll_days"]
ROLL_TIME_UTC = dt.time.fromisoformat(RULES["futures"]["roll_time_utc"])
# Market topics of the AI model: id → name.
TOPICS = {topic["id"]: topic["name"] for topic in RULES["topics"]}


def roll_at(last_trade_date):
    """When a series leaves the contract with this last trading day."""
    last = dt.datetime.fromisoformat(last_trade_date.replace("Z", "+00:00"))
    return dt.datetime.combine(last.date() - dt.timedelta(days=ROLL_DAYS), ROLL_TIME_UTC, dt.timezone.utc)


def series_code(ticker):
    """`BRX6` → `BR`; None for perpetual futures and other instruments."""
    match = DATED_FUTURES.fullmatch(ticker or "")
    return match.group(1) if match else None
SUPERVISOR_COMMANDS = ("mode", "universe", "portfolio", "portfolio_create", "live_accounts", "live_promote", "ai_check", "ai_preview",
                       "backtest")
PORTFOLIO_NAME = re.compile(r"[a-z0-9][a-z0-9-]{2,40}")
# Columns a new revision inherits from the strategy it replaces.
INHERITED = "execution_mode,execution_budget,run_mode,trading_days,close_after,schedule_initialized"


def literal(value):
    return "'" + str(value).replace("'", "''") + "'"


def read(campaign, state):
    return json.loads(campaign.run_sql(state, """SELECT json_build_object(
      'mode',(SELECT mode FROM dashboard_settings WHERE id),
      'max_runtimes',(SELECT max_runtimes FROM dashboard_settings WHERE id),
      'cases',coalesce((SELECT json_agg(c) FROM strategy_controls c),'[]'),
      'owners',coalesce((SELECT json_agg(json_build_object('account_id',account_id,'strategy_id',strategy_id)) FROM account_risk_state),'[]'),
      'pending',coalesce((SELECT json_agg(q) FROM (SELECT id,COALESCE(execution_strategy_id,strategy_id) AS strategy_id,created_at FROM control_commands WHERE status IN ('queued','running') ORDER BY created_at) q),'[]'),
      'closed',coalesce((SELECT json_agg(c.strategy_id) FROM strategy_controls c WHERE NOT c.enabled AND NOT c.needs_service AND NOT c.cleanup_requested AND (SELECT q.kind FROM control_commands q WHERE COALESCE(q.execution_strategy_id,q.strategy_id)=c.strategy_id AND q.status='succeeded' AND q.kind IN ('cleanup','manual_order','resume','universe') ORDER BY q.created_at DESC LIMIT 1)='cleanup'),'[]'),
      'live',(SELECT row_to_json(l) FROM (SELECT halted,halted_at,min_sessions,min_fills,require_no_risk_block FROM live_settings WHERE id) l),
      'commands',coalesce((SELECT json_agg(q) FROM (SELECT * FROM control_commands WHERE kind IN ('mode','universe','portfolio','portfolio_create','live_accounts','live_promote','ai_check','ai_preview','backtest') AND status IN ('queued','running') ORDER BY created_at) q),'[]'))"""))


def initialize(campaign, state):
    cases = [c for c in state["cases"].values() if c.get("contour") != "production"]
    values = ",".join("(" + literal(c["strategy_id"]) + "," + literal(c["account_id"]) + ")" for c in cases)
    if values:
        campaign.run_sql(state, "INSERT INTO strategy_controls(strategy_id,account_id,enabled) SELECT v.strategy_id,v.account_id,(SELECT mode='campaign' FROM dashboard_settings WHERE id) FROM (VALUES " + values + ") AS v(strategy_id,account_id) ON CONFLICT DO NOTHING")
    # A missing real-account row comes back switched off and confirming every plan.
    live = ",".join("(" + literal(c["strategy_id"]) + "," + literal(c["account_id"]) + ",false,'production','confirm',false,'always',true)"
                    for c in state["cases"].values() if c.get("contour") == "production")
    if live:
        campaign.run_sql(state, "INSERT INTO strategy_controls(strategy_id,account_id,enabled,contour,execution_mode,close_after,run_mode,schedule_initialized) VALUES " + live + " ON CONFLICT DO NOTHING")


def cohort_days(state, case):
    """Trading dates the fixed campaign roster assigned to this case."""
    sessions = state.get("sessions", [])
    cohort = case.get("cohort", 0)
    first, last = (0, 1) if cohort == 0 else (cohort + 1, cohort + 1)
    return [day["date"][:10] for day in sessions[first:last + 1]]


def convert_schedules(campaign, state, controls):
    """One-time move from cohort windows to operator-editable trading days."""
    pending = {c["strategy_id"] for c in controls["cases"] if not c.get("schedule_initialized")}
    sql = "BEGIN; "
    for case in state["cases"].values():
        if case["strategy_id"] in pending:
            # Free mode ignored the calendar; keep that behaviour per account.
            sql += ("UPDATE strategy_controls SET trading_days=" + literal(json.dumps(cohort_days(state, case))) + "::jsonb,"
                    "run_mode=CASE WHEN (SELECT mode FROM dashboard_settings WHERE id)='free' THEN 'always' ELSE 'schedule' END,"
                    "close_after=true,schedule_initialized=true WHERE strategy_id=" + literal(case["strategy_id"]) + " AND NOT schedule_initialized; ")
    if sql != "BEGIN; ":
        campaign.run_sql(state, sql + "COMMIT;")
        return True
    return False


def inherit_memory(old_id, new_id, relation):
    """SQL: a revision or a real-account copy continues the AI model memory of its
    parent. Active notes, lessons and open theses are copied once (the lineage row
    guards retries); past decisions are read through the lineage up to the fork."""
    old, new = literal(old_id), literal(new_id)
    return ("WITH link AS (INSERT INTO strategy_lineage(strategy_id,parent_id,relation) VALUES(" + new + "," + old + ","
            + literal(relation) + ") ON CONFLICT DO NOTHING RETURNING strategy_id), "
            "notes AS (INSERT INTO ai_memory(id,strategy_id,note,tickers,created_at,expires_at,inherited_from,kind) "
            "SELECT gen_random_uuid(),link.strategy_id,m.note,m.tickers,m.created_at,m.expires_at," + old + ",m.kind FROM ai_memory m, link "
            "WHERE m.strategy_id=" + old + " AND m.closed_at IS NULL AND (m.expires_at IS NULL OR m.expires_at>now()) RETURNING 1) "
            "INSERT INTO ai_theses(id,strategy_id,ticker,direction,thesis,horizon_days,entry_price,conditions,sources,opened_at,inherited_from) "
            "SELECT gen_random_uuid(),link.strategy_id,t.ticker,t.direction,t.thesis,t.horizon_days,t.entry_price,t.conditions,t.sources,t.opened_at," + old + " "
            "FROM ai_theses t, link WHERE t.strategy_id=" + old + " AND t.closed_at IS NULL; ")


def finish(campaign, state, command, status, result):
    campaign.run_sql(state, "UPDATE control_commands SET status=" + literal(status) + ",result=" + literal(json.dumps(result)) + "::jsonb,updated_at=now() WHERE id=" + literal(command["id"]) + "::uuid")


def renew_returning_strategy(campaign, state, case, previous_epoch):
    """Never attach an old durable strategy state to a different account epoch."""
    old_id = case["strategy_id"]
    exists = campaign.run_sql(state, "SELECT EXISTS(SELECT 1 FROM strategy_state WHERE strategy_id=" + literal(old_id) + ")")
    if exists != "t":
        return
    suffix = uuid.uuid5(uuid.NAMESPACE_URL, old_id + ":" + str(previous_epoch)).hex[:16]
    new_id = old_id + "-r" + suffix
    relative = "configs/" + case["kind"] + "-return-" + suffix + ".toml"
    path = campaign.WORK / relative
    source = (campaign.WORK / case["config"]).read_text()
    source = re.sub(r'^strategy_id\s*=.*$', 'strategy_id = ' + json.dumps(new_id), source, flags=re.M)
    if path.exists() and path.read_text() != source:
        raise RuntimeError("Returning revision config does not match its immutable identity")
    if not path.exists():
        path.write_text(source)
        path.chmod(0o600)
    # Keep the request ID, payload and requested strategy immutable for retries.
    # Routing changes only while no runtime owns this returning strategy.
    campaign.run_sql(state, "BEGIN; SELECT strategy_id FROM strategy_controls WHERE strategy_id=" + literal(old_id) + " FOR UPDATE; "
        "INSERT INTO strategy_controls(strategy_id,account_id,enabled,needs_service," + INHERITED + ") SELECT " + literal(new_id) + ",account_id,enabled,needs_service," + INHERITED + " FROM strategy_controls WHERE strategy_id=" + literal(old_id) + " ON CONFLICT DO NOTHING; "
        "UPDATE control_commands SET execution_strategy_id=" + literal(new_id) + " WHERE COALESCE(execution_strategy_id,strategy_id)=" + literal(old_id) + " AND status IN ('queued','running') AND kind NOT IN ('mode','universe','portfolio'); "
        "UPDATE strategy_controls SET enabled=false,needs_service=false,revision=revision+1,superseded_by=" + literal(new_id) + " WHERE strategy_id=" + literal(old_id) + "; "
        + inherit_memory(old_id, new_id, "revision") + "COMMIT;")
    case.setdefault("revisions", []).append({"strategy_id": old_id, "config": case["config"]})
    case.update(strategy_id=new_id, config=relative, attempts=0, state="waiting for account transfer")
    log = campaign.WORK / "logs" / (case["kind"] + ".log")
    case["revision_log_offset"] = log.stat().st_size if log.exists() else 0
    for key in ["pid", "restart_checked", "restart_result", "readiness_observed", "preflight_result", "service_only"]:
        case.pop(key, None)
    campaign.persist(state)


def mode(campaign, state, command):
    desired = command["payload"]["mode"]
    if desired not in {"campaign", "free"}:
        raise ValueError("Unknown operating mode")
    # One transaction makes re-reading a running command safe after a crash.
    sql = "BEGIN; SELECT id FROM dashboard_settings WHERE id FOR UPDATE; "
    if desired == "free":
        for case in state["cases"].values():
            request = str(uuid.uuid5(uuid.UUID(command["id"]), case["strategy_id"]))
            sql += "INSERT INTO control_commands(id,strategy_id,account_id,kind,payload) SELECT " + ",".join([literal(request) + "::uuid", literal(case["strategy_id"]), literal(case["account_id"]), "'pause'", "'{}'::jsonb"]) + " WHERE (SELECT mode FROM dashboard_settings WHERE id)<>'free' AND EXISTS(SELECT 1 FROM account_risk_state WHERE strategy_id=" + literal(case["strategy_id"]) + ") ON CONFLICT DO NOTHING; "
        sql += "UPDATE strategy_controls c SET enabled=false,revision=revision+1,needs_service=EXISTS(SELECT 1 FROM account_risk_state r WHERE r.strategy_id=c.strategy_id),updated_at=now() WHERE (SELECT mode FROM dashboard_settings WHERE id)<>'free'; "
    sql += "UPDATE dashboard_settings SET mode=" + literal(desired) + ",updated_at=now() WHERE id; "
    sql += "UPDATE control_commands SET status='succeeded',result=" + literal(json.dumps({"message": "Режим изменён", "mode": desired})) + "::jsonb,updated_at=now() WHERE id=" + literal(command["id"]) + "::uuid; COMMIT;"
    campaign.run_sql(state, sql)
    state["mode"] = desired
    state["phase"] = "interactive" if desired == "free" else "observing"
    campaign.persist(state)


def known_instruments(campaign, state):
    catalog = json.loads(campaign.run_sql(state, "SELECT coalesce(json_agg(json_build_object('uid',uid,'metadata',metadata)),'[]') FROM instrument_catalog"))
    known = {x["uid"]: x["metadata"] for x in catalog}
    known.update({i["uid"]: i for i in state["instruments"]})
    return known


def revision_case(state, command):
    candidates = [c for c in state["cases"].values() if c["strategy_id"] in {command["strategy_id"], command.get("result", {}).get("new_strategy_id")}]
    if len(candidates) != 1:
        raise ValueError("Стратегия уже сменила ревизию")
    return candidates[0]


def revise(campaign, state, command, case, ids, source_for, case_fields=None):
    """Replace a disabled, settled strategy by a new immutable revision and warm it up.

    `source_for(new_id)` returns the complete TOML of the revision. The account
    changes owner only through risk-reset, which refuses open positions or orders.
    """
    if case.get("contour") == "production":
        raise ValueError("Состав реального портфеля не меняется: проверьте новый состав в Sandbox и переведите его заново")
    control = json.loads(campaign.run_sql(state, "SELECT row_to_json(c) FROM strategy_controls c WHERE strategy_id=" + literal(case["strategy_id"])))
    if control["enabled"]:
        raise ValueError("Сначала выключите стратегию; состав меняется после закрытия позиций")
    known = known_instruments(campaign, state)
    if any(uid not in known for uid in ids):
        raise ValueError("Сначала добавьте инструменты в каталог")
    busy = campaign.run_sql(state, "SELECT EXISTS(SELECT 1 FROM control_commands WHERE COALESCE(execution_strategy_id,strategy_id)=" + literal(case["strategy_id"]) + " AND id<>" + literal(command["id"]) + "::uuid AND status IN ('queued','running') AND kind NOT IN ('mode','universe','portfolio'))")
    if busy == "t":
        finish(campaign, state, command, "running", {**command.get("result", {}), "message": "Ожидание завершения предыдущих команд счёта"})
        return
    other_running = sum(campaign.process_alive(c.get("pid"), str(campaign.WORK / c["config"]))
                        for c in state["cases"].values() if c is not case)
    if other_running >= state.get("max_runtimes", 4):
        finish(campaign, state, command, "running", {**command.get("result", {}), "message": "Ожидание свободного места для прогрева"})
        return
    old_id = command["strategy_id"]
    new_id = command.get("result", {}).get("new_strategy_id") or old_id + "-u" + command["id"].replace("-", "")[:12]
    relative = "configs/" + case["kind"] + "-" + command["id"] + ".toml"
    path = campaign.WORK / relative
    if not path.exists():
        path.write_text(source_for(new_id))
        path.chmod(0o600)
    replacement = dict(case, strategy_id=new_id, config=relative)
    checked = campaign.bot(state, replacement, "check-config")
    if checked.returncode:
        detail = ((getattr(checked, "stdout", "") or "") + (checked.stderr or "")).strip().splitlines()
        raise ValueError("Состав не соответствует требованиям стратегии (число инструментов, benchmark, пересечения наборов)" + (": " + detail[-1][:300] if detail else ""))
    finish(campaign, state, command, "running", {"new_strategy_id": new_id, "message": "Проверка передачи счёта"})
    campaign.stop_process(case)
    # risk-reset itself holds the lease and refuses non-flat portfolios or unresolved orders.
    # An account that never had a risk owner needs no handoff: the first run creates it.
    owned = campaign.run_sql(state, "SELECT EXISTS(SELECT 1 FROM account_risk_state WHERE account_id=" + literal(case["account_id"]) + ")") == "t"
    current = campaign.bot(state, replacement, "risk-status") if owned else None
    if current is not None and current.returncode:
        reset = campaign.bot(state, replacement, "risk-reset", "--reason", "operator revision " + command["id"])
        if reset.returncode:
            raise ValueError("Смена состава заблокирована: закройте позиции, завершите заявки и проверьте владение счётом")
    campaign.run_sql(state, "BEGIN; UPDATE strategy_runs SET ended_at=now() WHERE account_id=" + literal(case["account_id"]) + " AND ended_at IS NULL AND strategy_id<>" + literal(new_id) + "; INSERT INTO strategy_controls(strategy_id,account_id,enabled," + INHERITED + ") SELECT " + literal(new_id) + ",account_id,false," + INHERITED + " FROM strategy_controls WHERE strategy_id=" + literal(old_id) + " ON CONFLICT DO NOTHING; UPDATE strategy_controls SET superseded_by=" + literal(new_id) + ",needs_service=false WHERE strategy_id=" + literal(old_id) + "; " + inherit_memory(old_id, new_id, "revision") + "COMMIT;")
    if case["strategy_id"] != new_id:
        case.setdefault("revisions", []).append({"strategy_id": case["strategy_id"], "config": case["config"]})
        log = campaign.WORK / "logs" / (case["kind"] + ".log")
        case["revision_log_offset"] = log.stat().st_size if log.exists() else 0
    case.update(strategy_id=new_id, config=relative, state="paused", attempts=0, ownership_transferred=True, **(case_fields or {}))
    for key in ["pid", "restart_checked", "restart_result", "readiness_observed", "preflight_result", "service_only"]:
        case.pop(key, None)
    for uid in ids:
        if not any(i["uid"] == uid for i in state["instruments"]):
            state["instruments"].append(dict(known[uid], uid=uid))
    campaign.persist(state)
    warmup = campaign.bot(state, case, "run", "--startup-only", timeout=900)
    if warmup.returncode:
        raise ValueError("Ревизия сохранена на паузе; прогрев не завершён. Проверьте историю инструментов")
    case["preflight_result"] = "passed"
    campaign.persist(state)
    finish(campaign, state, command, "succeeded", {"message": "Новый состав прогрет; стратегия остаётся выключенной", "strategy_id": new_id})


def universe(campaign, state, command):
    case = revision_case(state, command)
    ids = command["payload"]["instruments"]
    if not ids or len(set(ids)) != len(ids):
        raise ValueError("Нужен непустой набор разных инструментов")

    def source_for(new_id):
        source = (campaign.WORK / case["config"]).read_text()
        source = re.sub(r'^strategy_id\s*=.*$', lambda _: 'strategy_id = ' + json.dumps(new_id), source, flags=re.M)
        start, end = source.index("[[universe]]"), source.index("[[strategies]]")
        return source[:start] + "\n".join("[[universe]]\nuid = " + json.dumps(uid) + "\n" for uid in ids) + "\n" + source[end:]
    revise(campaign, state, command, case, ids, source_for)


def decimal_text(value, low, high, places):
    try:
        number = Decimal(str(value))
    except InvalidOperation:
        raise ValueError("Ожидается число") from None
    if not number.is_finite() or not low < number <= high or number.as_tuple().exponent < -places:
        raise ValueError(f"Число должно быть больше {low} и не больше {high}, до {places} знаков после запятой")
    return number


def validate_blocks(blocks, benchmark_uid=None):
    """Blocks are combinations: identical instruments inside, disjoint between blocks."""
    if not isinstance(blocks, list) or not 1 <= len(blocks) <= 8:
        raise ValueError("Портфель должен содержать от 1 до 8 блоков")
    shares, seen = Decimal(0), {}
    for index, block in enumerate(blocks, 1):
        if not isinstance(block, dict) or set(block) - {"instruments", "share", "strategies"}:
            raise ValueError(f"Блок {index}: некорректная структура")
        ids, strategies = block.get("instruments"), block.get("strategies")
        if not isinstance(ids, list) or not 1 <= len(ids) <= 50 or len(set(ids)) != len(ids):
            raise ValueError(f"Блок {index}: выберите от 1 до 50 разных инструментов")
        if not isinstance(strategies, list) or not 1 <= len(strategies) <= 6:
            raise ValueError(f"Блок {index}: добавьте от 1 до 6 стратегий")
        kinds = [s.get("kind") if isinstance(s, dict) else None for s in strategies]
        # Повтор допустим только у ИИ-моделей (соревнование на одном наборе):
        # решение адресуется компоненте по профилю, поэтому профили явные и разные.
        plain = [kind for kind in kinds if kind != "ai-model"]
        if len(set(plain)) != len(plain):
            raise ValueError(f"Блок {index}: стратегия указана дважды")
        profiles = [((s.get("params") or {}).get("model_profile") if isinstance(s, dict) else None)
                    for s in strategies if isinstance(s, dict) and s.get("kind") == "ai-model"]
        if len(profiles) > 1 and (not all(profiles) or len(set(profiles)) != len(profiles)):
            raise ValueError(f"Блок {index}: у каждой ИИ-модели блока должен быть свой профиль модели")
        for strategy in strategies:
            if set(strategy) - {"kind", "weight", "params"} or strategy.get("kind") not in STRATEGY_KINDS:
                raise ValueError(f"Блок {index}: неизвестная стратегия")
            if "params" in strategy:
                validate_ai_params(strategy, index)
            decimal_text(strategy.get("weight", "1"), 0, 1000, 4)
            low, high = STRATEGY_KINDS[strategy["kind"]]
            if len(ids) < low or (high is not None and len(ids) > high):
                need = f"ровно {low}" if high == low else f"не меньше {low}"
                raise ValueError(f"Блок {index}: {strategy['kind']} требует {need} инструментов")
            if benchmark_uid and strategy["kind"] in {"residual-momentum", "betting-against-beta"} and benchmark_uid in ids:
                raise ValueError(f"Блок {index}: ориентир рынка не может быть торгуемым инструментом")
        shares += decimal_text(block.get("share"), 0, 100, 2)
        for uid in ids:
            if uid in seen:
                same = set(blocks[seen[uid] - 1]["instruments"]) == set(ids)
                raise ValueError((f"Блоки {seen[uid]} и {index} торгуют одинаковым набором — объедините их в одну комбинацию или разнесите по разным счетам"
                                  if same else f"Блоки {seen[uid]} и {index} пересекаются по инструментам — на одном счёте наборы не должны пересекаться"))
            seen[uid] = index
    if shares != 100:
        raise ValueError(f"Доли блоков должны в сумме давать 100% (сейчас {shares}%)")


def validate_kinds(blocks, known):
    """Every instrument is in the catalog and fits the strategies of its block."""
    for index, block in enumerate(blocks, 1):
        missing = [uid for uid in block["instruments"] if uid not in known]
        if missing:
            raise ValueError("Сначала добавьте инструменты в каталог: " + ", ".join(missing[:5]))
        if any(s["kind"] in SHARE_ONLY_KINDS for s in block["strategies"]):
            other = [known[uid].get("ticker") or uid for uid in block["instruments"] if known[uid].get("kind", "share") != "share"]
            if other:
                raise ValueError(f"Блок {index}: стратегии на отчётности и дивидендах работают только с акциями ({', '.join(other[:5])})")


def validate_ai_params(strategy, index):
    """Operator settings of an `ai-model` block; never a token or URL."""
    params = strategy["params"]
    if strategy.get("kind") != "ai-model" or not isinstance(params, dict) or set(params) - AI_PARAMS:
        raise ValueError(f"Блок {index}: параметры есть только у ИИ-модели")
    if "model_profile" in params and not (isinstance(params["model_profile"], str) and PROFILE_NAME.fullmatch(params["model_profile"])):
        raise ValueError(f"Блок {index}: профиль модели — латиница, цифры, - и _")
    if "instructions" in params and not (isinstance(params["instructions"], str) and len(params["instructions"]) <= 4000):
        raise ValueError(f"Блок {index}: идея стратегии — до 4000 символов")
    if "decision_interval_minutes" in params and params["decision_interval_minutes"] not in (0, 15, 30, 60, 120, 240):
        raise ValueError(f"Блок {index}: пересмотр — 0, 15, 30, 60, 120 или 240 минут")
    for key in ("price_jump_percent", "market_move_percent", "commodity_move_percent", "currency_move_percent"):
        if key in params:
            decimal_text(params[key], -1, 50, 2)
    for key, low, high in (("news_hours", 1, 168), ("max_tool_rounds", 1, 20), ("news_burst_items", 0, 50), ("material_importance", 0, 5)):
        if key in params and not (type(params[key]) is int and low <= params[key] <= high):
            raise ValueError(f"Блок {index}: {key} — целое от {low} до {high}")
    if "max_weight_percent" in params:
        decimal_text(params["max_weight_percent"], 0, 100, 2)
    if "allow_short" in params and not isinstance(params["allow_short"], bool):
        raise ValueError(f"Блок {index}: allow_short — да или нет")


def toml_value(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    # JSON escapes are valid in TOML basic strings.
    return json.dumps(str(value), ensure_ascii=False)


def strategy_block(campaign, state, kind, weight, instruments, params=None):
    text = (campaign.ROOT / "examples/configs" / (kind + ".toml")).read_text()
    block = text[text.index("[[strategies]]"):]
    block = re.sub(r'benchmark_uid\s*=\s*"[^"]+"', lambda _: 'benchmark_uid = ' + json.dumps(state["benchmark"]["uid"]), block)
    block = re.sub(r'^signal_weight\s*=.*$', lambda _: 'signal_weight = ' + json.dumps(weight), block, flags=re.M)
    block = re.sub(r'^(kind\s*=.*)$', lambda match: match.group(1) + "\ninstruments = " + json.dumps(instruments), block, count=1, flags=re.M)
    for key, value in (params or {}).items():
        line = key + " = " + toml_value(value)
        if re.search(r"^" + key + r"\s*=", block, flags=re.M):
            block = re.sub(r"^" + key + r"\s*=.*$", lambda _: line, block, count=1, flags=re.M)
        else:
            block = block.rstrip() + "\n" + line + "\n"
    return block.strip() + "\n"


def portfolio_source(campaign, state, case, new_id, blocks):
    base = (campaign.WORK / case["config"]).read_text()
    header = re.sub(r'^strategy_id\s*=.*$', lambda _: 'strategy_id = ' + json.dumps(new_id), base[:base.index("[[universe]]")], flags=re.M)
    text = header.rstrip() + "\n"
    # The account universe is the union of block instruments; the bot checks overlaps.
    for uid in dict.fromkeys(uid for block in blocks for uid in block["instruments"]):
        text += "\n[[universe]]\nuid = " + json.dumps(uid) + "\n"
    for block in blocks:
        share = Decimal(str(block["share"]))
        total = sum(Decimal(str(s.get("weight", "1"))) for s in block["strategies"])
        for strategy in block["strategies"]:
            # signal_weight = block share (fraction of 1) × relative weight inside the combination.
            weight = (share / 100 * Decimal(str(strategy.get("weight", "1"))) / total).quantize(Decimal("0.000000001"))
            text += "\n" + strategy_block(campaign, state, strategy["kind"], format(weight.normalize(), "f"), block["instruments"], strategy.get("params"))
    return text


def portfolio(campaign, state, command):
    case = revision_case(state, command)
    payload = command["payload"]
    blocks = payload["blocks"]
    validate_blocks(blocks, state.get("benchmark", {}).get("uid"))
    validate_kinds(blocks, known_instruments(campaign, state))
    ids = [uid for block in blocks for uid in block["instruments"]]
    title = str(payload.get("title") or case.get("title") or case["kind"])[:60]
    revise(campaign, state, command, case, ids, lambda new_id: portfolio_source(campaign, state, case, new_id, blocks),
           {"title": title, "blocks": blocks})


def create_portfolio(campaign, state, command):
    """A new portfolio (case) on an account whose current strategies are switched off.

    The previous owner is flattened by cleanup; the new strategy takes the account
    through risk-reset, which refuses open positions or unresolved orders. Earlier
    cases stay in the list with their history and wait for the account if re-enabled.
    """
    payload = command["payload"]
    account, blocks = payload["account_id"], payload["blocks"]
    validate_blocks(blocks, state.get("benchmark", {}).get("uid"))
    kind = payload.get("kind") or "portfolio-" + command["id"][:8]
    if not PORTFOLIO_NAME.fullmatch(kind):
        raise ValueError("Имя портфеля: латиница, цифры и дефис, 3–41 символ")
    existing = state["cases"].get(kind)
    if existing and existing.get("created_by") != command["id"]:
        raise ValueError(f"Портфель {kind} уже есть в списке")
    neighbours = [c for c in state["cases"].values() if c["account_id"] == account and c is not existing]
    if not neighbours and not existing:
        raise ValueError("Счёт не принадлежит стенду")
    if any(c.get("contour") == "production" for c in neighbours):
        raise ValueError("Это реальный счёт: портфель на него переводится из Sandbox после проверки")
    if not existing:
        enabled = json.loads(campaign.run_sql(state, "SELECT coalesce(json_agg(strategy_id),'[]') FROM strategy_controls WHERE account_id=" + literal(account) + " AND enabled AND superseded_by IS NULL"))
        if enabled:
            raise ValueError("Сначала выключите стратегии этого счёта: " + ", ".join(c["kind"] for c in neighbours if c["strategy_id"] in enabled))
        busy = campaign.run_sql(state, "SELECT EXISTS(SELECT 1 FROM control_commands WHERE account_id=" + literal(account) + " AND status IN ('queued','running') AND strategy_id IS NOT NULL)")
        if busy == "t":
            finish(campaign, state, command, "running", {"message": "Ожидание завершения команд счёта"})
            return
        if sum(campaign.process_alive(c.get("pid"), str(campaign.WORK / c["config"])) for c in state["cases"].values() if c["account_id"] != account) >= state.get("max_runtimes", 4):
            finish(campaign, state, command, "running", {"message": "Ожидание свободного места для прогрева"})
            return
        known = known_instruments(campaign, state)
        validate_kinds(blocks, known)
        ids = [uid for block in blocks for uid in block["instruments"]]
        owner_id = campaign.run_sql(state, "SELECT coalesce((SELECT strategy_id FROM account_risk_state WHERE account_id=" + literal(account) + "),'')")
        owner = next((c for c in neighbours if c["strategy_id"] == owner_id), neighbours[0])
        new_id = state["id"] + "-" + kind
        relative = "configs/" + kind + ".toml"
        path = campaign.WORK / relative
        if not path.exists():
            path.write_text(portfolio_source(campaign, state, owner, new_id, blocks))
            path.chmod(0o600)
        case = {"kind": kind, "title": str(payload.get("title") or kind)[:60], "blocks": blocks, "strategy_id": new_id,
                "account_id": account, "config": relative, "state": "paused", "attempts": 0, "funded": True,
                "created_by": command["id"], "predecessor": owner["kind"], "ownership_transferred": False}
        checked = campaign.bot(state, case, "check-config")
        if checked.returncode:
            detail = ((getattr(checked, "stdout", "") or "") + (checked.stderr or "")).strip().splitlines()
            raise ValueError("Состав не соответствует требованиям стратегии" + (": " + detail[-1][:300] if detail else ""))
        finish(campaign, state, command, "running", {"message": "Освобождение счёта от " + owner["kind"], "kind": kind})
        for neighbour in neighbours:
            campaign.stop_process(neighbour)
        if owner["state"] != "cleaned":
            campaign.cleanup_case(state, owner)
            if owner["state"] != "cleaned":
                raise ValueError("Счёт не освобождён: закройте позиции и завершите заявки " + owner["kind"])
        # Without a risk owner the first run creates the epoch; reset applies only to a handoff.
        if owner_id and campaign.bot(state, case, "risk-status").returncode:
            reset = campaign.bot(state, case, "risk-reset", "--reason", "operator portfolio " + command["id"])
            if reset.returncode:
                raise ValueError("Передача счёта заблокирована: закройте позиции, завершите заявки и проверьте владение счётом")
        run_mode = payload.get("run_mode", "always")
        campaign.run_sql(state, "BEGIN; UPDATE strategy_runs SET ended_at=now() WHERE account_id=" + literal(account) + " AND ended_at IS NULL AND strategy_id<>" + literal(new_id) + "; "
                         "INSERT INTO strategy_controls(strategy_id,account_id,enabled,run_mode,trading_days,close_after,schedule_initialized) VALUES(" + literal(new_id) + "," + literal(account) + ",false," + literal(run_mode) + ",'[]',false,true) ON CONFLICT DO NOTHING; COMMIT;")
        case["ownership_transferred"] = True
        for uid in ids:
            if not any(i["uid"] == uid for i in state["instruments"]):
                state["instruments"].append(dict(known[uid], uid=uid))
        state["cases"][kind] = case
        campaign.persist(state)
        existing = case
    if existing.get("preflight_result") != "passed":
        warmup = campaign.bot(state, existing, "run", "--startup-only", timeout=900)
        if warmup.returncode:
            raise ValueError("Портфель создан на паузе; прогрев не завершён. Проверьте историю инструментов и справочные данные")
        existing["preflight_result"] = "passed"
        campaign.persist(state)
    finish(campaign, state, command, "succeeded", {"message": f"Портфель {kind} создан и прогрет; он выключен до «Включить»", "kind": kind, "strategy_id": existing["strategy_id"]})


# Sandbox evidence per current strategy revision: official session opens it was
# live for, fills of its own plans and recorded risk blocks.
GATE_SQL = """SELECT coalesce(json_agg(t),'[]'::json) FROM (SELECT c.strategy_id,c.account_id,c.enabled,
  (SELECT count(DISTINCT split_part(e.event_id,':',3)) FROM processed_events e
    WHERE e.strategy_id=c.strategy_id AND e.event_id LIKE 'schedule:SessionOpen:%') AS sessions,
  (SELECT count(*) FROM fills f JOIN trade_intents i ON i.request_id=f.request_id LEFT JOIN rebalance_batches b ON b.id=i.batch_id
    WHERE i.strategy_id=c.strategy_id AND coalesce(b.event_id,'') !~ '^(cleanup:|risk:|manual:)') AS fills,
  (SELECT count(*) FROM risk_block_events r WHERE r.strategy_id=c.strategy_id) AS risk_blocks,
  (SELECT count(*) FROM ai_decisions d WHERE d.strategy_id=c.strategy_id AND d.kind='decision' AND d.status='ok') AS ai_decisions,
  (SELECT max(r.blocked_at) FROM risk_block_events r WHERE r.strategy_id=c.strategy_id) AS last_block_at
  FROM strategy_controls c WHERE c.contour='sandbox' AND c.superseded_by IS NULL{where}) t"""
LIVE_ACCOUNT_TYPES = {"ACCOUNT_TYPE_TINKOFF": "брокерский", "ACCOUNT_TYPE_TINKOFF_IIS": "ИИС"}


def gate_stats(campaign, state, strategy_id):
    rows = json.loads(campaign.run_sql(state, GATE_SQL.format(where=" AND c.strategy_id=" + literal(strategy_id))))
    return rows[0] if rows else {"strategy_id": strategy_id, "sessions": 0, "fills": 0, "risk_blocks": 0, "ai_decisions": 0, "last_block_at": None}


def uses_ai(campaign, case):
    """The portfolio has an AI model block."""
    try:
        return 'kind = "ai-model"' in (campaign.WORK / case["config"]).read_text()
    except (OSError, KeyError):
        return False


def gate_failures(stats, settings, ai=False):
    """Unmet promotion conditions in operator language; empty means the gate passes.
    An AI portfolio is judged by decisions of its model: it may keep money in cash."""
    failures = []
    if stats["sessions"] < settings["min_sessions"]:
        failures.append(f"торговых сессий в песочнице {stats['sessions']} из {settings['min_sessions']}")
    if ai:
        need = settings.get("min_ai_decisions", 5)
        if stats.get("ai_decisions", 0) < need:
            failures.append(f"решений модели {stats.get('ai_decisions', 0)} из {need}")
    elif stats["fills"] < settings["min_fills"]:
        failures.append(f"исполненных сделок {stats['fills']} из {settings['min_fills']}")
    if settings["require_no_risk_block"] and stats["risk_blocks"]:
        failures.append("в песочнице срабатывала риск-блокировка")
    return failures


# Tariff defaults, % of a trade's volume, and how much evidence a measured rate needs.
TARIFF_RATES = RULES["commissions"]["tariffs"]
UNKNOWN_TARIFF = RULES["commissions"]["unknown_tariff"]
MEASURE_DAYS = RULES["commissions"]["measure_days"]
MEASURE_MIN_TRADES = RULES["commissions"]["min_trades"]
MEASURE_MIN_VOLUME = RULES["commissions"]["min_volume_rub"]


def money_value(value):
    """MoneyValue / Quotation of the REST gateway as a float."""
    return int((value or {}).get("units") or 0) + int((value or {}).get("nano") or 0) / 1e9


def measure_commissions(api, accounts, sandbox, since, point_values=None, trades=None):
    """Commission and volume of the stand's trades by instrument type. A futures
    trade has no cash payment: its volume is price × quantity × point value.
    Each trade is also appended to `trades` when a list is given."""
    method = "SandboxService/GetSandboxOperationsByCursor" if sandbox else "OperationsService/GetOperationsByCursor"
    point_values = {} if point_values is None else point_values
    totals = {}
    for account in accounts:
        cursor = ""
        for _ in range(50):
            body = {"accountId": account, "from": since, "limit": 1000, "withoutCommissions": False,
                    "withoutTrades": False, "state": "OPERATION_STATE_EXECUTED"}
            if cursor:
                body["cursor"] = cursor
            page = api.call(method, body)
            for item in page.get("items", []):
                if not str(item.get("type", "")).startswith(("OPERATION_TYPE_BUY", "OPERATION_TYPE_SELL")):
                    continue
                kind = item.get("instrumentType") or "default"
                volume = abs(money_value(item.get("payment")))
                if kind == "futures":
                    uid = item.get("instrumentUid")
                    if uid not in point_values:
                        margin = api.call("InstrumentsService/GetFuturesMargin", {"instrumentId": uid})
                        step = money_value(margin.get("minPriceIncrement"))
                        point_values[uid] = money_value(margin.get("minPriceIncrementAmount")) / step if step else 0.0
                    volume = abs(money_value(item.get("price"))) * int(item.get("quantityDone") or 0) * point_values[uid]
                if volume <= 0:
                    continue
                commission = abs(money_value(item.get("commission")))
                row = totals.setdefault(kind, {"trades": 0, "volume": 0.0, "commission": 0.0})
                row["trades"] += 1
                row["volume"] += volume
                row["commission"] += commission
                if trades is not None and item.get("id") and item.get("date"):
                    trades.append({"id": item["id"], "account": account, "at": item["date"], "kind": kind,
                                   "volume": volume, "commission": commission})
            cursor = page.get("nextCursor") or ""
            if not page.get("hasNext") or not cursor:
                break
    return totals


def commission_rows(tariff, measured):
    """Rate per instrument type: measured when there is enough evidence, else the tariff."""
    defaults = TARIFF_RATES.get(tariff, UNKNOWN_TARIFF)
    rows = []
    for kind in sorted(set(defaults) | set(measured) | {"default"}):
        row = measured.get(kind)
        if row and row["trades"] >= MEASURE_MIN_TRADES and row["volume"] >= MEASURE_MIN_VOLUME:
            rows.append({"kind": kind, "rate": row["commission"] / row["volume"] * 100, "source": "measured", **row})
        else:
            rows.append({"kind": kind, "rate": defaults.get(kind, defaults["default"]), "source": "tariff",
                         **(row or {"trades": 0, "volume": 0.0, "commission": 0.0})})
    return rows


def calibrate_commissions(campaign, state, sandbox_api):
    """Tariff of each contour and the commission rates the planner and the AI use."""
    since = (campaign.now() - dt.timedelta(days=MEASURE_DAYS)).isoformat()
    contours = [("sandbox", sandbox_api)]
    try:
        contours.append(("production", campaign.API(production=True)))
    except RuntimeError:
        pass
    for contour, api in contours:
        accounts = sorted({c["account_id"] for c in state.get("cases", {}).values()
                           if (c.get("contour") == "production") == (contour == "production")})
        tariff = "sandbox" if contour == "sandbox" else (api.call("UsersService/GetInfo", {}).get("tariff") or "unknown")
        trades = []
        rows = commission_rows(tariff, measure_commissions(api, accounts, contour == "sandbox", since, trades=trades))
        values = ",".join("(" + ",".join([literal(contour), literal(r["kind"]), literal(f"{r['rate']:.5f}"), literal(r["source"]),
                                            literal(tariff), str(int(r["trades"])), literal(f"{r['volume']:.2f}"),
                                            literal(f"{r['commission']:.2f}")]) + ",now())" for r in rows)
        campaign.run_sql(state, "BEGIN; DELETE FROM commission_rates WHERE contour=" + literal(contour) + "; "
                         "INSERT INTO commission_rates(contour,kind,rate_percent,source,tariff,trades,volume_rub,commission_rub,updated_at) VALUES "
                         + values + "; COMMIT;")
        store_trade_commissions(campaign, state, trades)


def store_trade_commissions(campaign, state, trades, chunk=300):
    """What each trade cost: the dashboard sums it over a portfolio's run. A psql
    argument has a size limit, so the rows go in chunks."""
    for start in range(0, len(trades), chunk):
        values = ",".join("(" + ",".join([literal(t["id"]), literal(t["account"]), literal(t["at"]) + "::timestamptz",
                                          literal(t["kind"]), literal(f"{t['volume']:.2f}"), literal(f"{t['commission']:.2f}")]) + ")"
                          for t in trades[start:start + chunk])
        campaign.run_sql(state, "INSERT INTO trade_commissions(operation_id,account_id,executed_at,kind,volume_rub,commission_rub) VALUES "
                         + values + " ON CONFLICT (operation_id) DO UPDATE SET volume_rub=EXCLUDED.volume_rub,"
                         "commission_rub=EXCLUDED.commission_rub")


# USD/RUB of the Moscow Exchange (USD000UTSTOM): AI model prices in dollars become rubles.
USD_RUB_UID = "a22a1263-8e1b-4546-a1aa-416463f104d3"


def refresh_usd_rub(campaign, state, api):
    """The ruble rate the bot uses to price AI work and the daily AI budget."""
    prices = api.call("MarketDataService/GetLastPrices", {"instrumentId": [USD_RUB_UID]}).get("lastPrices", [])
    rate = money_value(prices[0].get("price")) if prices else 0
    if not 10 < rate < 1000:
        raise ValueError(f"implausible USD/RUB rate {rate}")
    campaign.run_sql(state, "INSERT INTO fx_rates(currency,rub,source,observed_at) VALUES('usd'," + literal(f"{rate:.6f}")
                     + ",'T-Invest USD000UTSTOM',now()) ON CONFLICT (currency) DO UPDATE SET rub=EXCLUDED.rub,"
                     "source=EXCLUDED.source,observed_at=EXCLUDED.observed_at")
    return rate


def live_settings(campaign, state):
    return json.loads(campaign.run_sql(state, "SELECT row_to_json(l) FROM live_settings l WHERE id"))


def survey_account(campaign, api, account, used):
    """Read-only check that a real account is empty and dedicated: open, full
    token access, no securities, no active orders, no blocked money."""
    row = {"id": account.get("id"), "name": account.get("name") or "", "type": account.get("type"),
           "status": account.get("status"), "access_level": account.get("accessLevel"),
           "opened": account.get("openedDate"), "cash_rub": None, "holdings": None, "orders": None}
    problems = []
    if row["type"] not in LIVE_ACCOUNT_TYPES:
        problems.append("не брокерский счёт и не ИИС")
    if row["status"] != "ACCOUNT_STATUS_OPEN":
        problems.append("счёт не открыт")
    if row["access_level"] != "ACCOUNT_ACCESS_LEVEL_FULL_ACCESS":
        problems.append("токен только для чтения" if row["access_level"] == "ACCOUNT_ACCESS_LEVEL_READ_ONLY" else "нет доступа токена")
    if row["id"] in used:
        problems.append("уже подключён к стенду: " + used[row["id"]])
    if row["status"] == "ACCOUNT_STATUS_OPEN" and row["access_level"] in {"ACCOUNT_ACCESS_LEVEL_FULL_ACCESS", "ACCOUNT_ACCESS_LEVEL_READ_ONLY"}:
        positions = api.call("OperationsService/GetPositions", {"accountId": row["id"]})
        holdings = sum(1 for group in ("securities", "futures", "options") for item in positions.get(group, [])
                       if int(item.get("balance", 0) or 0) or int(item.get("blocked", 0) or 0))
        cash = sum(campaign.rub_value(m) for m in positions.get("money", []) if str(m.get("currency", "")).lower() == "rub")
        blocked = any(campaign.rub_value(m) for m in positions.get("blocked", []))
        orders = len(api.call("OrdersService/GetOrders", {"accountId": row["id"]}).get("orders", []))
        row.update(cash_rub=round(cash, 2), holdings=holdings, orders=orders)
        if holdings:
            problems.append(f"есть бумаги или деривативы ({holdings})")
        if orders:
            problems.append(f"есть активные заявки ({orders})")
        if blocked:
            problems.append("есть заблокированные средства")
        if cash <= 0:
            problems.append("нет свободных рублей")
    row.update(eligible=not problems, problems=problems)
    return row


def live_error(error):
    text = str(error)
    if "not connected" in text or "unreadable" in text:
        return "Токен TINKOFF_TOKEN_PRODUCTION не подключён: см. docs/LIVE-TRADING.ru.md"
    if "HTTP 401" in text or "HTTP 403" in text:
        return "Брокер отклонил токен реального счёта"
    return "Брокер не ответил; повторите позже"


def live_accounts(campaign, state, command):
    """Refresh the read-only survey of the token's real accounts."""
    try:
        api = campaign.API(production=True)
        accounts = api.call("UsersService/GetAccounts").get("accounts", [])
        used = {c["account_id"]: c["kind"] for c in state["cases"].values()}
        rows = [survey_account(campaign, api, account, used) for account in accounts]
    except RuntimeError as error:
        message = live_error(error)
        campaign.run_sql(state, "UPDATE live_settings SET accounts_error=" + literal(message) + ",updated_at=now() WHERE id")
        raise ValueError(message) from None
    campaign.run_sql(state, "UPDATE live_settings SET accounts=" + literal(json.dumps(rows, ensure_ascii=False)) + "::jsonb,"
                     "accounts_observed_at=now(),accounts_error=NULL,updated_at=now() WHERE id")
    finish(campaign, state, command, "succeeded", {"message": f"Счетов: {len(rows)}, пригодных для подключения: {sum(r['eligible'] for r in rows)}"})


def live_source(campaign, source, account, strategy_id, payload):
    """The sandbox-tested composition, unchanged, on a real account with mandatory stops."""
    text = (campaign.WORK / source["config"]).read_text()
    app = re.search(r'^app_name\s*=.*$', text, flags=re.M)
    header = ['contour = "production"', "account_id = " + json.dumps(account), "strategy_id = " + json.dumps(strategy_id),
              app.group(0) if app else 'app_name = "ObraztsovOleg.tinkoff-rs.campaign"', "database_connections = 5", "",
              "[risk]", "budget_rub = " + json.dumps(payload["budget"]),
              "daily_loss_percent = " + json.dumps(payload["daily_loss_percent"]),
              "max_drawdown_percent = " + json.dumps(payload["max_drawdown_percent"])]
    return "\n".join(header) + "\n\n" + text[text.index("[[universe]]"):]


def live_promote(campaign, state, command):
    """Copy a sandbox portfolio that passed the gate onto an empty real account.

    The new case starts switched off, confirms every plan and is warmed up with
    a read-only startup check; the operator enables it explicitly.
    """
    payload = command["payload"]
    settings = live_settings(campaign, state)
    if settings["halted"]:
        raise ValueError("Включена аварийная остановка реальной торговли")
    source = next((c for c in state["cases"].values() if c["strategy_id"] == payload["source_strategy_id"]), None)
    if not source or source.get("contour") == "production":
        raise ValueError("Источник — портфель Sandbox из списка стенда")
    kind = payload["kind"]
    if not PORTFOLIO_NAME.fullmatch(kind):
        raise ValueError("Имя портфеля: латиница, цифры и дефис, 3–41 символ")
    existing = state["cases"].get(kind)
    if existing and existing.get("created_by") != command["id"]:
        raise ValueError(f"Портфель {kind} уже есть в списке")
    if not existing:
        stats = gate_stats(campaign, state, source["strategy_id"])
        failures = gate_failures(stats, settings, uses_ai(campaign, source))
        if failures:
            raise ValueError("Порог допуска не пройден: " + "; ".join(failures))
        account = payload["account_id"]
        used = {c["account_id"]: c["kind"] for c in state["cases"].values()}
        if account in used:
            raise ValueError("Счёт уже подключён к стенду: " + used[account])
        try:
            api = campaign.API(production=True)
            info = next((a for a in api.call("UsersService/GetAccounts").get("accounts", []) if a.get("id") == account), None)
            if not info:
                raise ValueError("Токен не видит этот счёт")
            row = survey_account(campaign, api, info, used)
        except RuntimeError as error:
            raise ValueError(live_error(error)) from None
        if not row["eligible"]:
            raise ValueError("Счёт не подходит: " + "; ".join(row["problems"]))
        if Decimal(payload["budget"]) > Decimal(str(row["cash_rub"])):
            raise ValueError(f"Бюджет больше свободных рублей на счёте ({row['cash_rub']:.2f} ₽)")
        strategy_id = state["id"] + "-live-" + kind
        relative = "configs/" + kind + ".toml"
        path = campaign.WORK / relative
        if not path.exists():
            path.write_text(live_source(campaign, source, account, strategy_id, payload))
            path.chmod(0o600)
        case = {"kind": kind, "title": str(payload.get("title") or kind)[:60], "blocks": source.get("blocks", []),
                "strategy_id": strategy_id, "account_id": account, "config": relative, "state": "paused", "attempts": 0,
                "funded": True, "contour": "production", "created_by": command["id"], "source": source["kind"],
                "source_strategy_id": source["strategy_id"], "promoted_at": campaign.now().isoformat(), "gate": stats,
                "risk_limits": {key: payload[key] for key in ("budget", "daily_loss_percent", "max_drawdown_percent")}}
        checked = campaign.bot(state, case, "check-config")
        if checked.returncode:
            detail = ((getattr(checked, "stdout", "") or "") + (checked.stderr or "")).strip().splitlines()
            raise ValueError("Конфигурация реального портфеля не прошла проверку" + (": " + detail[-1][:300] if detail else ""))
        finish(campaign, state, command, "running", {"message": "Проверка счёта и прогрев (только чтение)", "kind": kind})
        campaign.run_sql(state, "BEGIN; INSERT INTO strategy_controls(strategy_id,account_id,enabled,execution_mode,execution_budget,run_mode,"
                         "trading_days,close_after,schedule_initialized,contour) VALUES(" + literal(strategy_id) + "," + literal(account)
                         + ",false,'confirm'," + literal(payload["budget"]) + ",'always','[]',false,true,'production') ON CONFLICT DO NOTHING; "
                         + inherit_memory(source["strategy_id"], strategy_id, "promotion") + "COMMIT;")
        state["cases"][kind] = case
        campaign.persist(state)
        existing = case
    if existing.get("preflight_result") != "passed":
        warmup = campaign.bot(state, existing, "run", "--startup-only", timeout=900)
        log = campaign.WORK / "logs" / (kind + ".preflight.log")
        log.parent.mkdir(exist_ok=True)
        with log.open("a") as output:
            output.write((warmup.stdout or "") + (warmup.stderr or ""))
        if warmup.returncode:
            raise ValueError("Портфель создан выключенным; проверка реального счёта и прогрев не завершены — см. журнал «Прогрев»")
        existing["preflight_result"] = "passed"
        campaign.persist(state)
    finish(campaign, state, command, "succeeded", {"message": f"Реальный портфель {kind} создан и прогрет; он выключен до «Включить»",
                                                   "kind": kind, "strategy_id": existing["strategy_id"]})


AI_JOB_SECONDS = 900


def ai_job(campaign, state, command):
    """Model check, dry-run analysis or a backtest in a background process: slow
    work never blocks the supervisor. The bot records previews in ai_decisions itself."""
    jobs = state.setdefault("ai_jobs", {})
    job = jobs.get(command["id"])
    payload = command["payload"]
    env_case = None
    if command["kind"] == "ai_preview":
        case = next((c for c in state["cases"].values() if c["strategy_id"] == payload.get("strategy_id")), None)
        if not case or 'kind = "ai-model"' not in (campaign.WORK / case["config"]).read_text():
            raise ValueError("В портфеле нет стратегии ИИ-модели")
        args, env_case = ["ai-preview"], case
    elif command["kind"] == "backtest":
        case = next((c for c in state["cases"].values() if c["strategy_id"] == payload.get("strategy_id")), None)
        if not case:
            raise ValueError("Портфель не найден")
        # History is read through the Sandbox contour even for a real-account portfolio.
        args = ["backtest", "--from", payload["from"], "--to", payload["to"], "--slippage-bps", str(payload["slippage_bps"])]
        if (state.get("benchmark") or {}).get("uid"):
            args += ["--benchmark", state["benchmark"]["uid"]]
    else:
        case = next(iter(state["cases"].values()))
        args, env_case = ["ai-check"] + (["--profile", payload["profile"]] if payload.get("profile") else []), case
    if not job:
        output = campaign.WORK / "logs" / ("ai-" + command["id"] + ".out")
        output.parent.mkdir(exist_ok=True)
        with output.open("w") as out:
            child = campaign.subprocess.Popen([str(campaign.BINARY), "--config", str(campaign.WORK / case["config"]), *args],
                                              env=campaign.environment(state, env_case), stdout=out, stderr=campaign.subprocess.STDOUT, start_new_session=True)
        jobs[command["id"]] = {"pid": child.pid, "output": str(output), "started": campaign.now().isoformat()}
        campaign.persist(state)
        finish(campaign, state, command, "running", {"message": {"ai_preview": "Модель анализирует…", "backtest": "Считаем на истории…"}.get(
            command["kind"], "Проверяем подключение…")})
        return
    output = Path(job["output"])
    if campaign.process_alive(job["pid"], str(campaign.BINARY)):
        if (campaign.now() - campaign.timestamp(job["started"])).total_seconds() > AI_JOB_SECONDS:
            campaign.os.kill(job["pid"], campaign.signal.SIGTERM)
            jobs.pop(command["id"])
            campaign.persist(state)
            raise ValueError("Проверка на истории не закончилась за 15 минут" if command["kind"] == "backtest" else "Модель не ответила за 15 минут")
        return
    jobs.pop(command["id"])
    campaign.persist(state)
    text = output.read_text(errors="replace") if output.exists() else ""
    output.unlink(missing_ok=True)
    lines = [line for line in text.splitlines() if line.strip()]
    result = next((json.loads(line) for line in reversed(lines) if line.startswith("{")), None)
    error = next((line.split("error=", 1)[1] for line in reversed(lines) if "error=" in line), None)
    if command["kind"] == "backtest":
        if not result or "final_equity" not in result:
            raise ValueError("Проверка на истории не удалась: " + (error or "нет результата")[:500])
        finish(campaign, state, command, "succeeded", {"message": backtest_summary(result), "report": result})
        return
    if command["kind"] == "ai_check":
        if not result:
            raise ValueError("Модель недоступна: " + (error or "нет ответа")[:500])
        finish(campaign, state, command, "succeeded", {"message": f"Модель ответила за {result['latency_ms']} мс: {result['reply']}", **result})
        return
    previews = (result or {}).get("previews", [])
    good = [p for p in previews if "error" not in p]
    if not good:
        raise ValueError("Пробный анализ не удался: " + ((previews[0]["error"] if previews else error) or "нет ответа")[:500])
    weights = ", ".join(f"{w['ticker']} {w['weight_percent']}%" for p in good for w in p["weights"]) or "без позиций"
    finish(campaign, state, command, "succeeded", {"message": "Пробный анализ: " + weights + " — подробности в «Решениях модели»"})


def backtest_summary(report):
    """One line for the command journal."""
    def number(value, digits=2):
        return "—" if value is None else f"{value:+.{digits}f}"
    benchmark = report.get("benchmark")
    line = (f"На истории {report['from']} — {report['to']}: {number(report['return_percent'])}% "
            f"(просадка {report['max_drawdown_percent']:.2f}%, Шарп {number(report.get('sharpe'))}, "
            f"сделок {report['trades']}, комиссии {float(report['commission_rub']):,.0f} ₽)").replace(",", " ")
    if benchmark:
        line += f"; ориентир {number(benchmark['return_percent'])}%"
    return line


def process(campaign, state):
    controls = read(campaign, state)
    if convert_schedules(campaign, state, controls):
        controls = read(campaign, state)
    state["max_runtimes"] = controls.get("max_runtimes") or 4
    for command in controls["commands"]:
        try:
            if command["kind"] == "mode":
                mode(campaign, state, command)
            elif command["kind"] == "portfolio":
                portfolio(campaign, state, command)
            elif command["kind"] == "portfolio_create":
                create_portfolio(campaign, state, command)
            elif command["kind"] == "live_accounts":
                live_accounts(campaign, state, command)
            elif command["kind"] == "live_promote":
                live_promote(campaign, state, command)
            elif command["kind"] in {"ai_check", "ai_preview", "backtest"}:
                ai_job(campaign, state, command)
            else:
                universe(campaign, state, command)
        except Exception as error:
            message = str(error)[:1500] if isinstance(error, ValueError) else "Не удалось выполнить команду; состояние сохранено, требуется проверка стенда"
            finish(campaign, state, command, "failed", {"message": message})
    return read(campaign, state) if controls["commands"] else controls
