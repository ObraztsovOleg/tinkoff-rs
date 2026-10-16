"""Durable sandbox commands. The HTTP process never submits broker mutations."""
from __future__ import annotations

import datetime as dt
from decimal import Decimal, InvalidOperation
import importlib.util
import json
import re
from pathlib import Path
import threading
import uuid

SPEC = importlib.util.spec_from_file_location("dashboard_campaign", Path(__file__).resolve().parents[1] / "sandbox-campaign.py")
campaign = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(campaign)


class ClientError(ValueError):
    def __init__(self, message, status=400):
        super().__init__(message)
        self.status = status


def literal(value):
    if value is None:
        return "NULL"
    # E strings remain safe independently of standard_conforming_strings.
    return "E'" + str(value).replace("\\", "\\\\").replace("'", "''") + "'"


def identifier(value):
    try:
        return str(uuid.UUID(str(value)))
    except (ValueError, TypeError, AttributeError):
        raise ClientError("Ожидается UUID идентификатор") from None


def amount(value, *, price=False):
    if not isinstance(value, str) or len(value) > 40:
        raise ClientError("Сумма должна быть десятичной строкой")
    try:
        number = Decimal(value)
        if not number.is_finite() or number <= 0 or number > Decimal("1000000000000"):
            raise InvalidOperation
        if number.as_tuple().exponent < (-9 if price else -2):
            raise InvalidOperation
        return format(number, "f")
    except InvalidOperation:
        raise ClientError("Сумма должна быть положительной, до 2 знаков после запятой (цена — до 9)") from None


def validate(body, state):
    if not isinstance(body, dict) or set(body) - {"id", "kind", "strategy_id", "account_id", "payload"}:
        raise ClientError("Некорректная команда")
    command = {"id": identifier(body.get("id")), "kind": body.get("kind"),
               "strategy_id": body.get("strategy_id"), "account_id": body.get("account_id"),
               "payload": body.get("payload", {})}
    kind, payload = command["kind"], command["payload"]
    fields = {"pause": set(), "resume": set(), "cleanup": set(), "new_run": set(),
              "manual_order": {"instrument_id", "side", "lots", "order_type", "price"},
              "cancel_batch": {"batch_id"}, "approve_batch": {"batch_id"},
              "execution_mode": {"mode"}, "budget": {"operation", "amount"}, "pay_in": {"amount"},
              "mode": {"mode"}, "catalog_add": {"uid"}, "universe": {"instruments"}}
    if not isinstance(kind, str) or kind not in fields or not isinstance(payload, dict) or set(payload) - fields[kind]:
        raise ClientError("Неизвестная команда или параметры")
    if kind in {"mode", "catalog_add"}:
        if command["strategy_id"] is not None or command["account_id"] is not None:
            raise ClientError("Глобальная команда не принимает счёт стратегии")
    else:
        case = next((c for c in state.get("cases", {}).values() if c["strategy_id"] == command["strategy_id"]), None)
        if not case or case["account_id"] != command["account_id"]:
            raise ClientError("Стратегия и счёт не соответствуют текущей кампании")
    if kind == "manual_order":
        payload["instrument_id"] = identifier(payload.get("instrument_id"))
        if payload.get("side") not in ("buy", "sell") or payload.get("order_type") not in ("market", "limit"):
            raise ClientError("Укажите направление и тип заявки")
        if type(payload.get("lots")) is not int or not 0 < payload["lots"] <= 1000000000:
            raise ClientError("Количество лотов должно быть положительным целым")
        if payload["order_type"] == "limit":
            payload["price"] = amount(payload.get("price"), price=True)
        elif "price" in payload:
            raise ClientError("Рыночная заявка не принимает лимитную цену")
    if kind in {"cancel_batch", "approve_batch"}:
        payload["batch_id"] = identifier(payload.get("batch_id"))
    if kind == "execution_mode" and payload.get("mode") not in ("auto", "confirm"):
        raise ClientError("Режим исполнения: auto или confirm")
    if kind == "mode" and payload.get("mode") not in ("campaign", "free"):
        raise ClientError("Режим запуска: campaign или free")
    if kind in {"budget", "pay_in"}:
        payload["amount"] = amount(payload.get("amount"))
    if kind == "budget" and payload.get("operation") not in ("set", "add", "subtract"):
        raise ClientError("Действие с бюджетом: set, add или subtract")
    if kind == "catalog_add":
        payload["uid"] = identifier(payload.get("uid"))
    if kind == "universe":
        values = payload.get("instruments")
        if not isinstance(values, list) or not 1 <= len(values) <= 50:
            raise ClientError("Выберите от 1 до 50 инструментов")
        payload["instruments"] = [identifier(value) for value in values]
        if len(set(payload["instruments"])) != len(values):
            raise ClientError("Инструменты не должны повторяться")
    return command


def instrument_metadata(raw):
    if (raw.get("instrumentType") != "share" or raw.get("currency", "").lower() != "rub"
            or raw.get("realExchange") != "REAL_EXCHANGE_MOEX" or not raw.get("apiTradeAvailableFlag")):
        raise ClientError("Поддерживаются доступные через API акции MOEX в рублях")
    return {"uid": identifier(raw.get("uid")), "ticker": raw.get("ticker"), "name": raw.get("name"),
            "lot": raw.get("lot"), "currency": "rub", "exchange": raw.get("exchange"),
            "short_enabled": bool(raw.get("shortEnabledFlag")), "figi": raw.get("figi"),
            "first_1min_candle_date": raw.get("first1minCandleDate"),
            "first_1day_candle_date": raw.get("first1dayCandleDate")}


def ranking(runs, names=None):
    """Account equity already contains commission; exclude all external flows."""
    names = names or {}
    history = []
    for run in runs:
        def decimal(key):
            value = run.get(key)
            return Decimal(str(value)) if value is not None else None
        principal = decimal("principal")
        equity, anchor = decimal("last_equity"), decimal("anchor_equity")
        flows, anchor_flows = decimal("last_external_cash_flows"), decimal("anchor_external_cash_flows")
        ready = all(v is not None and v.is_finite() for v in [principal, equity, anchor, flows, anchor_flows]) and principal > 0
        pnl = equity - anchor - (flows - anchor_flows) if ready else None
        row = {key: run.get(key) for key in ["id", "strategy_id", "account_id", "classification", "started_at", "ended_at", "interventions", "updated_at"]}
        row.update(strategy=names.get(run.get("strategy_id"), run.get("strategy_id")),
                   pnl_rub=float(pnl) if pnl is not None else None,
                   return_percent=float(pnl / principal * 100) if ready else None,
                   principal=float(principal) if principal is not None else None,
                   max_drawdown_percent=float(decimal("max_drawdown_percent") or 0),
                   fills=int(run.get("fills") or 0), ranked=bool(ready and int(run.get("fills") or 0) > 0))
        history.append(row)
    history.sort(key=lambda r: r.get("started_at") or "", reverse=True)
    latest, seen = [], set()
    for row in history:
        key = (row["strategy"], row["classification"])
        if key not in seen:
            latest.append(row)
            seen.add(key)
    result = {"history": history, "clean": [], "mixed": [], "unranked": []}
    for row in latest:
        result[row["classification"] if row["ranked"] else "unranked"].append(dict(row))
    for group in ["clean", "mixed"]:
        result[group].sort(key=lambda r: (-r["return_percent"], r["strategy_id"]))
        for index, row in enumerate(result[group], 1):
            row["rank"] = index
    return result


class Control:
    def __init__(self, work):
        self.work = work
        self.api = None
        self.api_lock = threading.Lock()
        self.instrument_cache = {}

    def state(self):
        return json.loads((self.work / "campaign.json").read_text())

    def sql(self, query):
        # SQL contains no credentials. Failures are translated by the HTTP boundary.
        output = campaign.run_sql(self.state(), query)
        # json_agg(record) inserts newlines between records. Transactions also
        # emit BEGIN / row counts / COMMIT, so decode the complete JSON value.
        decoder = json.JSONDecoder()
        for match in re.finditer(r"(?m)^[ \t]*(?=[{\[])", output):
            try:
                return decoder.raw_decode(output[match.end():])[0]
            except json.JSONDecodeError:
                continue
        raise RuntimeError("Database did not acknowledge the command")

    def metadata(self, uid):
        if uid not in self.instrument_cache:
            with self.api_lock:
                if self.api is None:
                    self.api = campaign.API()
                raw = self.api.call("InstrumentsService/GetInstrumentBy", {"idType": "INSTRUMENT_ID_TYPE_UID", "id": uid})["instrument"]
            self.instrument_cache[uid] = instrument_metadata(raw)
        return self.instrument_cache[uid]

    def search(self, query):
        if not isinstance(query, str) or not 1 <= len(query.strip()) <= 80:
            raise ClientError("Введите тикер или название, до 80 символов")
        with self.api_lock:
            if self.api is None:
                self.api = campaign.API()
            response = self.api.call("InstrumentsService/FindInstrument", {"query": query.strip(), "instrumentKind": "INSTRUMENT_TYPE_SHARE", "apiTradeAvailableFlag": True})
        values = []
        for raw in response.get("instruments", [])[:15]:
            try:
                values.append(self.metadata(identifier(raw.get("uid"))))
            except ClientError:
                continue
        return {"instruments": values}

    def data(self):
        data = self.sql("""SELECT json_build_object(
          'available',true,'mode',(SELECT mode FROM dashboard_settings WHERE id=true),
          'cases',(SELECT coalesce(json_agg(t),'[]'::json) FROM strategy_controls t),
          'commands',(SELECT coalesce(json_agg(t),'[]'::json) FROM (SELECT * FROM control_commands ORDER BY created_at DESC LIMIT 100) t),
          'batches',(SELECT coalesce(json_agg(t),'[]'::json) FROM batch_controls t),
          'catalog',(SELECT coalesce(json_agg(metadata),'[]'::json) FROM instrument_catalog),
          'runs',(SELECT coalesce(json_agg(t),'[]'::json) FROM (SELECT r.*,
             (SELECT count(*) FROM fills f JOIN trade_intents i ON i.request_id=f.request_id
              WHERE i.strategy_id=r.strategy_id AND i.account_id=r.account_id AND f.executed_at>=r.started_at
                AND (r.ended_at IS NULL OR f.executed_at<r.ended_at)) AS fills FROM strategy_runs r) t));""")
        state = self.state()
        names = {c["strategy_id"]: c["kind"] for c in state.get("cases", {}).values()}
        for case in state.get("cases", {}).values():
            for revision in case.get("revisions", []):
                names[revision["strategy_id"]] = case["kind"]
        for case in state.get("archived_cases", []):
            names[case["strategy_id"]] = case["kind"]
        data["ranking"] = ranking(data.pop("runs"), names)
        data["mode"] = data["mode"] or "campaign"
        data["observed_at"] = dt.datetime.now(dt.timezone.utc).isoformat()
        return data

    def get_command(self, command_id):
        row = self.sql(f"SELECT coalesce((SELECT row_to_json(c) FROM control_commands c WHERE id={literal(identifier(command_id))}::uuid),'{{}}'::json)")
        if not row:
            raise ClientError("Команда не найдена", 404)
        return {"command": row}

    def enqueue(self, body):
        state = self.state()
        # An in-flight HTTP retry can name a revision archived during account
        # handoff. Return only its existing immutable request, never enqueue new
        # work for an archived owner.
        if isinstance(body, dict) and isinstance(body.get("strategy_id"), str) and body["strategy_id"] not in {c["strategy_id"] for c in state["cases"].values()}:
            previous = next((dict(c, strategy_id=r["strategy_id"]) for c in state["cases"].values()
                             for r in c.get("revisions", []) if r["strategy_id"] == body.get("strategy_id")), None)
            if previous:
                command = validate(body, {"cases": {"previous": previous}})
                row = self.get_command(command["id"])["command"]
                if any(row.get(key) != command[key] for key in ["id", "kind", "strategy_id", "account_id", "payload"]):
                    raise ClientError("Этот идентификатор уже использован для другой команды", 409)
                return {"command": row}
        command = validate(body, state)
        kind, payload = command["kind"], command["payload"]
        metadata = self.metadata(payload["uid"]) if kind == "catalog_add" else None
        if kind == "manual_order":
            self.metadata(payload["instrument_id"])
        if kind == "universe":
            for uid in payload["instruments"]:
                self.metadata(uid)
        sid, aid, cid = (literal(command[key]) for key in ["strategy_id", "account_id", "id"])
        statements = ["BEGIN;"]
        if command["strategy_id"]:
            statements += [f"INSERT INTO strategy_controls(strategy_id,account_id,enabled) SELECT {sid},{aid},coalesce((SELECT mode='campaign' FROM dashboard_settings WHERE id=true),true) ON CONFLICT DO NOTHING;",
                           f"SELECT strategy_id FROM strategy_controls WHERE strategy_id={sid} FOR UPDATE;"]
        guard = "TRUE"
        if kind in {"cancel_batch", "approve_batch"}:
            guard = (f"EXISTS(SELECT 1 FROM rebalance_batches WHERE id={literal(payload['batch_id'])}::uuid "
                     f"AND strategy_id={sid} AND account_id={aid} AND status NOT IN ('completed','failed') "
                     "AND event_id NOT LIKE 'cleanup:%' AND event_id NOT LIKE 'risk:%' "
                     "AND NOT EXISTS(SELECT 1 FROM batch_controls bc WHERE bc.batch_id=rebalance_batches.id AND bc.revision=-1))")
        if command["strategy_id"]:
            guard += f" AND EXISTS(SELECT 1 FROM strategy_controls WHERE strategy_id={sid} AND superseded_by IS NULL)"
            guard += f" AND NOT EXISTS(SELECT 1 FROM control_commands WHERE strategy_id={sid} AND kind='universe' AND status IN ('queued','running'))"
        insert = (f"INSERT INTO control_commands(id,strategy_id,account_id,kind,payload) SELECT {cid}::uuid,{sid},{aid},"
                  f"{literal(kind)},{literal(json.dumps(payload))}::jsonb WHERE {guard} ON CONFLICT DO NOTHING RETURNING id")
        ctes = ["inserted AS (" + insert + ")"]
        if command["strategy_id"]:
            pause = "enabled=false," if kind in {"pause", "manual_order", "cleanup", "universe"} else ""
            revision = "revision=revision+1," if pause else ""
            ctes.append(f"gated AS (UPDATE strategy_controls SET {pause}{revision}needs_service=true,updated_at=now() "
                        f"WHERE strategy_id={sid} AND EXISTS(SELECT 1 FROM inserted) RETURNING strategy_id)")
        if kind == "cancel_batch":
            bid = literal(payload["batch_id"])
            ctes.append("rejected AS (INSERT INTO batch_controls(batch_id,status,revision) "
                        f"SELECT {bid}::uuid,'rejected',0 WHERE EXISTS(SELECT 1 FROM inserted) "
                        "ON CONFLICT(batch_id) DO UPDATE SET status='rejected',revision=batch_controls.revision+1,updated_at=now() RETURNING batch_id)")
        if metadata:
            ctes.append(f"catalogued AS (INSERT INTO instrument_catalog(uid,metadata) SELECT {literal(metadata['uid'])},{literal(json.dumps(metadata))}::jsonb "
                        "WHERE EXISTS(SELECT 1 FROM inserted) ON CONFLICT(uid) DO UPDATE SET metadata=excluded.metadata RETURNING uid)")
        statements.append("WITH " + ",".join(ctes) + " SELECT count(*) FROM inserted;")
        if metadata:
            statements.append(f"UPDATE control_commands SET status='succeeded',result='{{}}'::jsonb,updated_at=now() WHERE id={cid}::uuid AND kind='catalog_add' AND payload={literal(json.dumps(payload))}::jsonb;")
        statements.append(f"SELECT coalesce((SELECT row_to_json(c) FROM control_commands c WHERE id={cid}::uuid),'{{}}'::json);")
        statements.append("COMMIT;")
        row = self.sql("\n".join(statements))
        if not row:
            raise ClientError("Команда не принята: стратегия меняет ревизию либо план завершён, служебный или принадлежит другой стратегии", 409)
        if any(row.get(key) != command[key] for key in ["id", "kind", "strategy_id", "account_id", "payload"]):
            raise ClientError("Этот идентификатор уже использован для другой команды", 409)
        return {"command": row}
