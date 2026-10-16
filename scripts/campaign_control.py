"""Supervisor-side durable mode and immutable-universe transitions."""
import json
import re
import uuid


def literal(value):
    return "'" + str(value).replace("'", "''") + "'"


def read(campaign, state):
    return json.loads(campaign.run_sql(state, """SELECT json_build_object(
      'mode',(SELECT mode FROM dashboard_settings WHERE id),
      'cases',coalesce((SELECT json_agg(c) FROM strategy_controls c),'[]'),
      'owners',coalesce((SELECT json_agg(json_build_object('account_id',account_id,'strategy_id',strategy_id)) FROM account_risk_state),'[]'),
      'pending',coalesce((SELECT json_agg(q) FROM (SELECT id,COALESCE(execution_strategy_id,strategy_id) AS strategy_id,created_at FROM control_commands WHERE status IN ('queued','running') ORDER BY created_at) q),'[]'),
      'closed',coalesce((SELECT json_agg(c.strategy_id) FROM strategy_controls c WHERE NOT c.enabled AND NOT c.needs_service AND NOT c.cleanup_requested AND (SELECT q.kind FROM control_commands q WHERE COALESCE(q.execution_strategy_id,q.strategy_id)=c.strategy_id AND q.status='succeeded' AND q.kind IN ('cleanup','manual_order','resume','universe') ORDER BY q.created_at DESC LIMIT 1)='cleanup'),'[]'),
      'commands',coalesce((SELECT json_agg(q) FROM (SELECT * FROM control_commands WHERE kind IN ('mode','universe') AND status IN ('queued','running') ORDER BY created_at) q),'[]'))"""))


def initialize(campaign, state):
    values = ",".join("(" + literal(c["strategy_id"]) + "," + literal(c["account_id"]) + ")" for c in state["cases"].values())
    if values:
        campaign.run_sql(state, "INSERT INTO strategy_controls(strategy_id,account_id,enabled) SELECT v.strategy_id,v.account_id,(SELECT mode='campaign' FROM dashboard_settings WHERE id) FROM (VALUES " + values + ") AS v(strategy_id,account_id) ON CONFLICT DO NOTHING")


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
        "INSERT INTO strategy_controls(strategy_id,account_id,enabled,execution_mode,execution_budget,needs_service) SELECT " + literal(new_id) + ",account_id,enabled,execution_mode,execution_budget,needs_service FROM strategy_controls WHERE strategy_id=" + literal(old_id) + " ON CONFLICT DO NOTHING; "
        "UPDATE control_commands SET execution_strategy_id=" + literal(new_id) + " WHERE COALESCE(execution_strategy_id,strategy_id)=" + literal(old_id) + " AND status IN ('queued','running') AND kind NOT IN ('mode','universe'); "
        "UPDATE strategy_controls SET enabled=false,needs_service=false,revision=revision+1,superseded_by=" + literal(new_id) + " WHERE strategy_id=" + literal(old_id) + "; COMMIT;")
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


def universe(campaign, state, command):
    candidates = [c for c in state["cases"].values() if c["strategy_id"] in {command["strategy_id"], command.get("result", {}).get("new_strategy_id")}]
    if len(candidates) != 1:
        raise ValueError("Стратегия уже сменила ревизию")
    case = candidates[0]
    control = json.loads(campaign.run_sql(state, "SELECT row_to_json(c) FROM strategy_controls c WHERE strategy_id=" + literal(case["strategy_id"])))
    if control["enabled"]:
        raise ValueError("Сначала выключите стратегию; набор меняется после закрытия позиций")
    ids = command["payload"]["instruments"]
    if not ids or len(set(ids)) != len(ids):
        raise ValueError("Нужен непустой набор разных инструментов")
    catalog = json.loads(campaign.run_sql(state, "SELECT coalesce(json_agg(json_build_object('uid',uid,'metadata',metadata)),'[]') FROM instrument_catalog"))
    known = {x["uid"]: x["metadata"] for x in catalog}
    known.update({i["uid"]: i for i in state["instruments"]})
    if any(uid not in known for uid in ids):
        raise ValueError("Сначала добавьте инструменты в каталог")
    busy = campaign.run_sql(state, "SELECT EXISTS(SELECT 1 FROM control_commands WHERE COALESCE(execution_strategy_id,strategy_id)=" + literal(case["strategy_id"]) + " AND id<>" + literal(command["id"]) + "::uuid AND status IN ('queued','running') AND kind NOT IN ('mode','universe'))")
    if busy == "t":
        finish(campaign, state, command, "running", {**command.get("result", {}), "message": "Ожидание завершения предыдущих команд счёта"})
        return
    other_running = sum(campaign.process_alive(c.get("pid"), str(campaign.WORK / c["config"]))
                        for c in state["cases"].values() if c is not case)
    if other_running >= 4:
        finish(campaign, state, command, "running", {**command.get("result", {}), "message": "Ожидание свободного места для прогрева"})
        return
    old_id = command["strategy_id"]
    new_id = command.get("result", {}).get("new_strategy_id") or old_id + "-u" + command["id"].replace("-", "")[:12]
    relative = "configs/" + case["kind"] + "-" + command["id"] + ".toml"
    path = campaign.WORK / relative
    if not path.exists():
        source = (campaign.WORK / case["config"]).read_text()
        source = re.sub(r'^strategy_id\s*=.*$', 'strategy_id = ' + json.dumps(new_id), source, flags=re.M)
        start, end = source.index("[[universe]]"), source.index("[[strategies]]")
        source = source[:start] + "\n".join("[[universe]]\nuid = " + json.dumps(uid) + "\n" for uid in ids) + "\n" + source[end:]
        path.write_text(source)
        path.chmod(0o600)
    replacement = dict(case, strategy_id=new_id, config=relative)
    checked = campaign.bot(state, replacement, "check-config")
    if checked.returncode:
        raise ValueError("Набор не соответствует требованиям стратегии к числу инструментов или benchmark")
    finish(campaign, state, command, "running", {"new_strategy_id": new_id, "message": "Проверка передачи счёта"})
    campaign.stop_process(case)
    # risk-reset itself holds the lease and refuses non-flat portfolios or unresolved orders.
    current = campaign.bot(state, replacement, "risk-status")
    if current.returncode:
        reset = campaign.bot(state, replacement, "risk-reset", "--reason", "operator universe revision " + command["id"])
        if reset.returncode:
            raise ValueError("Смена набора заблокирована: закройте позиции, завершите заявки и проверьте владение счётом")
    campaign.run_sql(state, "BEGIN; UPDATE strategy_runs SET ended_at=now() WHERE account_id=" + literal(case["account_id"]) + " AND ended_at IS NULL AND strategy_id<>" + literal(new_id) + "; INSERT INTO strategy_controls(strategy_id,account_id,enabled,execution_mode,execution_budget) SELECT " + literal(new_id) + ",account_id,false,execution_mode,execution_budget FROM strategy_controls WHERE strategy_id=" + literal(old_id) + " ON CONFLICT DO NOTHING; UPDATE strategy_controls SET superseded_by=" + literal(new_id) + ",needs_service=false WHERE strategy_id=" + literal(old_id) + "; COMMIT;")
    if case["strategy_id"] != new_id:
        case.setdefault("revisions", []).append({"strategy_id": case["strategy_id"], "config": case["config"]})
        log = campaign.WORK / "logs" / (case["kind"] + ".log")
        case["revision_log_offset"] = log.stat().st_size if log.exists() else 0
    case.update(strategy_id=new_id, config=relative, state="paused", attempts=0, ownership_transferred=True)
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
    finish(campaign, state, command, "succeeded", {"message": "Новый набор прогрет; стратегия остаётся выключенной", "strategy_id": new_id})


def process(campaign, state):
    controls = read(campaign, state)
    for command in controls["commands"]:
        try:
            if command["kind"] == "mode":
                mode(campaign, state, command)
            else:
                universe(campaign, state, command)
        except Exception as error:
            message = str(error)[:1500] if isinstance(error, ValueError) else "Не удалось выполнить команду; состояние сохранено, требуется проверка стенда"
            finish(campaign, state, command, "failed", {"message": message})
    return read(campaign, state) if controls["commands"] else controls
