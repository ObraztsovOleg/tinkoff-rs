"""Durable sandbox commands. The HTTP process never submits broker mutations."""
from __future__ import annotations

import datetime as dt
from decimal import Decimal, InvalidOperation
import importlib.util
import json
import math
import os
import re
from pathlib import Path
import subprocess
import threading
import uuid

SPEC = importlib.util.spec_from_file_location("dashboard_campaign", Path(__file__).resolve().parents[1] / "sandbox-campaign.py")
campaign = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(campaign)

MSK = dt.timezone(dt.timedelta(hours=3))


class ClientError(ValueError):
    def __init__(self, message, status=400):
        super().__init__(message)
        self.status = status


DOCUMENT_KINDS = {"news", "article", "disclosure", "press_release", "report", "exchange", "regulator"}
# Rules shared with the bot and the supervisor: contracts/stand-rules.json.
stand_rules = campaign.control_plane
TOPICS = stand_rules.TOPICS


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


SERIES_ID, DATED_FUTURES, ROLL_DAYS = stand_rules.SERIES_ID, stand_rules.DATED_FUTURES, stand_rules.ROLL_DAYS
roll_at, series_code = stand_rules.roll_at, stand_rules.series_code
# Instruments the stand trades: settled in rubles on MOEX, with a known value of a price unit.
TRADABLE_KINDS = {"share", "etf", "bond", "futures", "currency"}
INSTRUMENT_TYPES = {"share": "INSTRUMENT_TYPE_SHARE", "etf": "INSTRUMENT_TYPE_ETF", "bond": "INSTRUMENT_TYPE_BOND",
                    "futures": "INSTRUMENT_TYPE_FUTURES", "currency": "INSTRUMENT_TYPE_CURRENCY",
                    "option": "INSTRUMENT_TYPE_OPTION", "sp": "INSTRUMENT_TYPE_SP"}


def instrument_id(value):
    """A T-Invest instrument UID or a continuous futures series."""
    if isinstance(value, str) and SERIES_ID.fullmatch(value):
        return value
    return identifier(value)


def quotation(value):
    """Quotation/MoneyValue of the REST gateway as a float; None when absent."""
    if not isinstance(value, dict):
        return None
    return int(value.get("units") or 0) + int(value.get("nano") or 0) / 1e9



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
    fields = {"pause": set(), "resume": {"confirm"}, "cleanup": {"confirm"}, "new_run": set(),
              "manual_order": {"instrument_id", "side", "lots", "order_type", "price"},
              "cancel_batch": {"batch_id"}, "approve_batch": {"batch_id"},
              "execution_mode": {"mode"}, "budget": {"operation", "amount"}, "pay_in": {"amount"},
              "mode": {"mode"}, "catalog_add": {"uid"}, "universe": {"instruments"},
              "schedule": {"run_mode", "days", "close_after"}, "runtime_limit": {"value"}, "trade_band": {"percent"},
              "ai_budget": {"rub"},
              "portfolio": {"title", "blocks"}, "portfolio_create": {"account_id", "kind", "title", "blocks", "run_mode"},
              "live_accounts": set(), "live_halt": {"reason"}, "live_resume": set(),
              "live_gate": {"min_sessions", "min_fills", "min_ai_decisions", "require_no_risk_block"},
              "live_promote": {"source_strategy_id", "account_id", "kind", "title", "budget", "daily_loss_percent",
                               "max_drawdown_percent", "confirm"},
              "ai_check": {"profile"}, "ai_preview": {"strategy_id"}, "backtest": {"strategy_id", "from", "to", "slippage_bps"},
              "exposure_set": {"ticker", "topic", "sensitivity", "note"}, "exposure_confirm": {"ticker"}, "exposure_redraft": {"ticker"}}
    if not isinstance(kind, str) or kind not in fields or not isinstance(payload, dict) or set(payload) - fields[kind]:
        raise ClientError("Неизвестная команда или параметры")
    if kind in GLOBAL_COMMANDS:
        if command["strategy_id"] is not None or command["account_id"] is not None:
            raise ClientError("Глобальная команда не принимает счёт стратегии")
    else:
        case = next((c for c in state.get("cases", {}).values() if c["strategy_id"] == command["strategy_id"]), None)
        if not case or case["account_id"] != command["account_id"]:
            raise ClientError("Стратегия и счёт не соответствуют текущей кампании")
        if case.get("contour") == "production":
            check_live_command(case, kind, payload)
    if kind == "manual_order":
        payload["instrument_id"] = instrument_id(payload.get("instrument_id"))
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
    if kind == "budget":
        # Only the target value is entered; the account process computes the difference.
        if payload.setdefault("operation", "set") != "set":
            raise ClientError("Бюджет задаётся новой суммой; разницу стенд вычисляет сам")
    if kind == "catalog_add":
        payload["uid"] = instrument_id(payload.get("uid"))
    if kind == "schedule":
        days = payload.get("days", [])
        if payload.get("run_mode") not in ("schedule", "always") or not isinstance(payload.get("close_after"), bool):
            raise ClientError("Укажите режим торговли и закрытие позиций после окна")
        if not isinstance(days, list) or len(days) > 60 or any(not isinstance(d, str) or not re.fullmatch(r"\d{4}-\d\d-\d\d", d) for d in days):
            raise ClientError("Дни торговли — до 60 дат в формате ГГГГ-ММ-ДД")
        if len(set(days)) != len(days):
            raise ClientError("Дни не должны повторяться")
        payload["days"] = sorted(days)
    if kind == "runtime_limit" and (type(payload.get("value")) is not int or not 1 <= payload["value"] <= 10):
        raise ClientError("Лимит процессов — целое число от 1 до 10")
    if kind == "ai_budget":
        try:
            rub = Decimal(str(payload.get("rub")))
        except InvalidOperation:
            raise ClientError("Лимит расходов на ИИ — сумма в рублях") from None
        if not rub.is_finite() or not 0 < rub <= 1_000_000 or rub.as_tuple().exponent < -2:
            raise ClientError("Лимит расходов на ИИ — от 0,01 до 1 000 000 ₽ в сутки")
        payload["rub"] = str(rub)
    if kind == "trade_band":
        try:
            percent = Decimal(str(payload.get("percent")))
        except InvalidOperation:
            raise ClientError("Порог сделки — число процентов") from None
        if not percent.is_finite() or not 0 <= percent <= 20 or percent.as_tuple().exponent < -3:
            raise ClientError("Порог сделки — от 0 до 20% капитала, до трёх знаков после запятой")
        payload["percent"] = str(percent)
    if kind == "portfolio_create":
        if payload.get("account_id") not in {c["account_id"] for c in state.get("cases", {}).values()}:
            raise ClientError("Счёт не принадлежит стенду")
        if "kind" in payload and not campaign.control_plane.PORTFOLIO_NAME.fullmatch(str(payload["kind"])):
            raise ClientError("Имя портфеля: латиница, цифры и дефис, 3–41 символ")
        if payload.get("kind") in state.get("cases", {}):
            raise ClientError("Портфель с таким именем уже есть")
        if payload.get("run_mode", "always") not in ("schedule", "always"):
            raise ClientError("Режим торговли: schedule или always")
    if kind in {"portfolio", "portfolio_create"}:
        title, blocks = payload.get("title"), payload.get("blocks")
        if not isinstance(title, str) or not 1 <= len(title.strip()) <= 60:
            raise ClientError("Название портфеля — от 1 до 60 символов")
        payload["title"] = title.strip()
        if isinstance(blocks, list):
            for block in blocks:
                if isinstance(block, dict) and isinstance(block.get("instruments"), list):
                    block["instruments"] = [instrument_id(value) for value in block["instruments"]]
        try:
            campaign.control_plane.validate_blocks(blocks, (state.get("benchmark") or {}).get("uid"))
        except ValueError as error:
            raise ClientError(str(error)) from None
    if kind == "live_halt":
        reason = payload.get("reason", "Аварийная остановка из панели")
        if not isinstance(reason, str) or not 1 <= len(reason.strip()) <= 200:
            raise ClientError("Причина остановки — до 200 символов")
        payload["reason"] = reason.strip()
    if kind == "live_gate":
        if type(payload.get("min_sessions")) is not int or not 1 <= payload["min_sessions"] <= 60:
            raise ClientError("Минимум сессий — целое число от 1 до 60")
        if type(payload.get("min_fills")) is not int or not 0 <= payload["min_fills"] <= 1000:
            raise ClientError("Минимум сделок — целое число от 0 до 1000")
        if type(payload.setdefault("min_ai_decisions", 5)) is not int or not 0 <= payload["min_ai_decisions"] <= 1000:
            raise ClientError("Минимум решений модели — целое число от 0 до 1000")
        if not isinstance(payload.get("require_no_risk_block"), bool):
            raise ClientError("Укажите, допускаются ли риск-блокировки")
    if kind == "live_promote":
        validate_promotion(payload, state)
    if kind == "ai_check" and "profile" in payload and not (isinstance(payload["profile"], str) and campaign.control_plane.PROFILE_NAME.fullmatch(payload["profile"])):
        raise ClientError("Профиль модели — латиница, цифры, - и _")
    if kind in {"ai_preview", "backtest"} and payload.get("strategy_id") not in {c["strategy_id"] for c in state.get("cases", {}).values()}:
        raise ClientError("Выберите портфель стенда")
    if kind == "backtest":
        validate_backtest(payload)
    if kind in {"exposure_set", "exposure_confirm", "exposure_redraft"}:
        if not isinstance(payload.get("ticker"), str) or not re.fullmatch(r"[A-Z0-9]{1,12}", payload["ticker"]):
            raise ClientError("Тикер — латиница и цифры")
    if kind == "exposure_set":
        if payload.get("topic") not in TOPICS:
            raise ClientError("Неизвестная тема")
        value = payload.get("sensitivity")
        if value is not None:
            try:
                number = Decimal(str(value))
            except InvalidOperation:
                raise ClientError("Чувствительность — число от -2 до 2") from None
            if not number.is_finite() or not -2 <= number <= 2 or number.as_tuple().exponent < -1:
                raise ClientError("Чувствительность — число от -2 до 2 с шагом 0,1")
            payload["sensitivity"] = str(number)
        note = payload.get("note", "")
        if not isinstance(note, str) or len(note) > 300:
            raise ClientError("Пояснение — до 300 символов")
        payload["note"] = note.strip()
    if kind == "universe":
        values = payload.get("instruments")
        if not isinstance(values, list) or not 1 <= len(values) <= 50:
            raise ClientError("Выберите от 1 до 50 инструментов")
        payload["instruments"] = [instrument_id(value) for value in values]
        if len(set(payload["instruments"])) != len(values):
            raise ClientError("Инструменты не должны повторяться")
    return command


GLOBAL_COMMANDS = {"mode", "catalog_add", "runtime_limit", "trade_band", "ai_budget", "portfolio_create",
                   "live_accounts", "live_promote", "live_halt", "live_resume", "live_gate", "ai_check", "ai_preview", "backtest",
                   "exposure_set", "exposure_confirm", "exposure_redraft"}
# Applied by the dashboard in one transaction: they need neither the broker nor the account lease.
IMMEDIATE_COMMANDS = {"schedule", "runtime_limit", "trade_band", "ai_budget", "live_halt", "live_resume", "live_gate",
                      "exposure_set", "exposure_confirm", "exposure_redraft"}
# With an operator PIN set, these need it: moving a portfolio to a real account,
# turning real accounts back on, loosening the gate, and every command for a
# real-account portfolio. Stopping never waits for the PIN.
PIN_COMMANDS = {"live_promote", "live_resume", "live_gate"}
PIN_FREE_COMMANDS = {"pause", "live_halt"}


def pin_required(body, state):
    """Whether a command acts on real money and so needs the operator PIN."""
    kind = body.get("kind") if isinstance(body, dict) else None
    if kind in PIN_COMMANDS:
        return True
    if kind in PIN_FREE_COMMANDS or kind in GLOBAL_COMMANDS:
        return False
    case = next((c for c in state.get("cases", {}).values() if c.get("strategy_id") == body.get("strategy_id")), None)
    return bool(case and case.get("contour") == "production")


# A real account: no virtual money, no manual or automatic orders, no composition edits.
LIVE_FORBIDDEN = {"pay_in": "Пополнение реального счёта — только в приложении брокера",
                  "manual_order": "Ручные заявки на реальном счёте — в приложении брокера",
                  "universe": "Состав реального портфеля не меняется: проверьте новый состав в Sandbox и переведите его заново",
                  "portfolio": "Состав реального портфеля не меняется: проверьте новый состав в Sandbox и переведите его заново"}


def confirm_code(account_id):
    return str(account_id)[-4:]


def check_live_command(case, kind, payload):
    """Extra rules for a real-account case; the runtime enforces the same ones."""
    if kind in LIVE_FORBIDDEN:
        raise ClientError(LIVE_FORBIDDEN[kind])
    if kind == "execution_mode" and payload.get("mode") != "confirm":
        raise ClientError("На реальном счёте каждый план подтверждается вручную")
    if kind == "schedule" and payload.get("close_after"):
        raise ClientError("Реальный счёт не закрывает позиции по расписанию: используйте «Закрыть позиции»")
    if kind in {"resume", "cleanup"} and payload.get("confirm") != confirm_code(case["account_id"]):
        raise ClientError("Подтвердите действие на реальном счёте: введите последние 4 цифры номера счёта")


def percent(value, label):
    if not isinstance(value, str) or len(value) > 10:
        raise ClientError(label + " — десятичная строка")
    try:
        number = Decimal(value)
    except InvalidOperation:
        raise ClientError(label + " — десятичная строка") from None
    if not number.is_finite() or not 0 < number <= 100 or number.as_tuple().exponent < -2:
        raise ClientError(label + ": больше 0 и не больше 100, до 2 знаков после запятой")
    return number


def validate_backtest(payload):
    """Period of a backtest: whole days, the first not after the last, the last
    before today, at most ten years; slippage 0–100 basis points."""
    try:
        first, last = (dt.date.fromisoformat(str(payload.get(key))) for key in ("from", "to"))
    except ValueError:
        raise ClientError("Период — даты в формате ГГГГ-ММ-ДД") from None
    if not first <= last < dt.date.today() or (last - first).days > 3660:
        raise ClientError("Период: начало не позже конца, конец раньше сегодняшнего дня, не длиннее 10 лет")
    if type(payload.get("slippage_bps")) not in (int, float) or not 0 <= payload["slippage_bps"] <= 100:
        raise ClientError("Проскальзывание — от 0 до 100 базисных пунктов")
    payload["from"], payload["to"] = first.isoformat(), last.isoformat()


def validate_promotion(payload, state):
    source = next((c for c in state.get("cases", {}).values() if c["strategy_id"] == payload.get("source_strategy_id")), None)
    if not source or source.get("contour") == "production":
        raise ClientError("Выберите портфель Sandbox")
    account = payload.get("account_id")
    if not isinstance(account, str) or not re.fullmatch(r"[0-9A-Za-z-]{4,64}", account):
        raise ClientError("Выберите реальный счёт из списка")
    if account in {c["account_id"] for c in state.get("cases", {}).values()}:
        raise ClientError("Счёт уже подключён к стенду")
    if not campaign.control_plane.PORTFOLIO_NAME.fullmatch(str(payload.get("kind", ""))):
        raise ClientError("Имя портфеля: латиница, цифры и дефис, 3–41 символ")
    if payload["kind"] in state.get("cases", {}):
        raise ClientError("Портфель с таким именем уже есть")
    title = payload.get("title")
    if not isinstance(title, str) or not 1 <= len(title.strip()) <= 60:
        raise ClientError("Название портфеля — от 1 до 60 символов")
    payload["title"] = title.strip()
    payload["budget"] = amount(payload.get("budget"))
    daily = percent(payload.get("daily_loss_percent"), "Дневной стоп")
    drawdown = percent(payload.get("max_drawdown_percent"), "Стоп по просадке")
    if daily > drawdown:
        raise ClientError("Дневной стоп не может быть больше стопа по просадке")
    if payload.get("confirm") != confirm_code(account):
        raise ClientError("Подтвердите перевод: введите последние 4 цифры номера реального счёта")


def runtime_cap(state):
    """Every runtime opens account streams; the tightest tariff stream limit bounds the count."""
    limits = [entry.get("limit", 10) for entry in (state.get("limits") or {}).get("streamLimits", [])]
    return max(1, min([10] + [limit - 1 for limit in limits if isinstance(limit, int)]))


def check_schedule(state, payload, before, others):
    """Days must be known MOEX trading days; new days must not be over; no account overlap."""
    known = {d["date"]: d for d in campaign.calendar(state)}
    now = dt.datetime.now(dt.timezone.utc)
    previous = set(before.get("trading_days") or [])
    for day in payload["days"]:
        entry = known.get(day)
        if not entry or not entry.get("isTradingDay") or not entry.get("endTime"):
            raise ClientError(f"{day}: не торговый день MOEX или вне известного календаря")
        if day not in previous and campaign.timestamp(entry["endTime"]) <= now:
            raise ClientError(f"{day}: торговая сессия уже завершилась")
    for other in others:
        common = sorted(set(payload["days"]) & set(other.get("trading_days") or []))
        if common and payload["run_mode"] == "schedule" and other.get("run_mode") == "schedule":
            name = next((c["kind"] for c in state["cases"].values() if c["strategy_id"] == other["strategy_id"]), other["strategy_id"])
            raise ClientError(f"Счёт в {', '.join(common)} уже занят стратегией {name}: на одном счёте торгует один портфель")


def instrument_metadata(raw, details=None):
    """Catalog entry of any T-Invest instrument; `tradable` says whether the stand
    can trade it and `reason` why not. `details` are the type-specific fields
    (FutureBy + GetFuturesMargin, BondBy)."""
    kind = raw.get("instrumentType") or ""
    details = details or {}
    problems = []
    if kind not in TRADABLE_KINDS:
        problems.append("этот тип инструментов стенд пока не торгует")
    if (raw.get("currency") or "").lower() != "rub":
        problems.append("расчёты не в рублях")
    if raw.get("realExchange") != "REAL_EXCHANGE_MOEX":
        problems.append("торгуется не на Мосбирже")
    if not raw.get("apiTradeAvailableFlag"):
        problems.append("торговля через API недоступна")
    meta = {"uid": identifier(raw.get("uid")), "ticker": raw.get("ticker"), "name": raw.get("name"), "kind": kind,
            "lot": raw.get("lot"), "currency": (raw.get("currency") or "").lower(), "exchange": raw.get("exchange"),
            "class_code": raw.get("classCode"), "figi": raw.get("figi"),
            # A futures position is opened by selling; its short flag is not reported.
            "short_enabled": bool(raw.get("shortEnabledFlag") or (kind == "futures" and raw.get("sellAvailableFlag", True))),
            "qualified_only": bool(raw.get("forQualInvestorFlag")),
            "first_1min_candle_date": raw.get("first1minCandleDate"),
            "first_1day_candle_date": raw.get("first1dayCandleDate")}
    if kind == "futures":
        increment, amount = quotation(details.get("minPriceIncrement")), quotation(details.get("minPriceIncrementAmount"))
        meta.update(series=series_code(raw.get("ticker")), basic_asset=details.get("basicAsset"),
                    last_trade_date=details.get("lastTradeDate"), expiration_date=details.get("expirationDate"),
                    point_value=amount / increment if increment and amount else None,
                    margin_buy=quotation(details.get("initialMarginOnBuy")), margin_sell=quotation(details.get("initialMarginOnSell")))
        if not meta["point_value"]:
            problems.append("биржа не сообщила стоимость шага цены")
    if kind == "bond":
        meta.update(nominal=quotation(details.get("nominal")), maturity_date=details.get("maturityDate"),
                    coupons_per_year=details.get("couponQuantityPerYear"), aci=quotation(details.get("aciValue")),
                    floating_coupon=bool(details.get("floatingCouponFlag")), amortization=bool(details.get("amortizationFlag")))
        if (details.get("nominal") or {}).get("currency", "rub").lower() != "rub":
            problems.append("номинал не в рублях")
    meta["tradable"] = not problems
    meta["reason"] = "; ".join(problems)
    return meta


def benchmark_closes(work, benchmark):
    """Daily closes of the stand's benchmark (a fund on the Moscow Exchange index)
    from the chart cache the market feed keeps: {Moscow date: close}."""
    if not benchmark:
        return None
    try:
        cache = json.loads((Path(work) / "reports" / "market-chart.json").read_text())
        candles = cache["assets"][benchmark["uid"]]["series"]["day"]["candles"]
    except (OSError, ValueError, KeyError, TypeError):
        return None
    closes = {}
    for candle in candles:
        moment = dt.datetime.fromisoformat(str(candle["time"]).replace("Z", "+00:00"))
        closes[moment.astimezone(MSK).date().isoformat()] = float(candle["close"])
    return {"ticker": benchmark.get("ticker"), "closes": closes} if closes else None


def risk_metrics(anchor, anchor_flows, days, benchmark=None, started=None):
    """Daily returns of a run net of deposits and withdrawals (the last sample of
    each Moscow day): annualized volatility, Sharpe and Sortino ratios without a
    risk-free rate, the worst day, the cumulative curve and the benchmark's return
    over the same days. Ratios need at least five days."""
    returns, curve, index = [], [], 1.0
    previous, previous_flows = anchor, anchor_flows
    for day, equity, flows in days or []:
        equity, flows = float(equity), float(flows)
        if previous and previous > 0:
            value = (equity - previous - (flows - previous_flows)) / previous
            returns.append(value)
            index *= 1 + value
            curve.append([day, round((index - 1) * 100, 3)])
        previous, previous_flows = equity, flows
    result = {"days": len(returns), "curve": curve[-120:], "volatility_percent": None, "sharpe": None,
              "sortino": None, "worst_day_percent": None, "benchmark_percent": None, "benchmark": None}
    if len(returns) >= 5:
        mean = sum(returns) / len(returns)
        deviation = math.sqrt(sum((r - mean) ** 2 for r in returns) / (len(returns) - 1))
        downside = math.sqrt(sum(min(r, 0) ** 2 for r in returns) / len(returns))
        year = math.sqrt(365)
        result.update(volatility_percent=deviation * year * 100,
                      sharpe=mean / deviation * year if deviation > 0 else None,
                      sortino=mean / downside * year if downside > 0 else None,
                      worst_day_percent=min(returns) * 100)
    if benchmark and curve and started:
        closes = benchmark["closes"]
        first = str(started)[:10]
        before = [d for d in closes if d < first]
        upto = [d for d in closes if d <= curve[-1][0]]
        if before and upto and upto[-1] > before[-1]:
            result.update(benchmark=benchmark["ticker"],
                          benchmark_percent=(closes[upto[-1]] / closes[before[-1]] - 1) * 100)
    return result


def ranking(runs, names=None, benchmark=None):
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
        costs = run.get("costs") or {}
        row.update(commission_rub=float(costs.get("commission_rub") or 0), trades=int(costs.get("trades") or 0),
                   volume_rub=float(costs.get("volume_rub") or 0),
                   metrics=risk_metrics(float(anchor) if anchor is not None else None,
                                        float(anchor_flows) if anchor_flows is not None else 0.0,
                                        run.get("days"), benchmark, run.get("started_at")))
        if row["metrics"]["benchmark_percent"] is not None and row["return_percent"] is not None:
            row["metrics"]["excess_percent"] = row["return_percent"] - row["metrics"]["benchmark_percent"]
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


def ai_models_path():
    return Path(os.environ.get("AI_MODELS_FILE", "ai-models.toml"))


class Control:
    def __init__(self, work):
        self.work = work
        self.api = None
        self.api_lock = threading.Lock()
        self.instrument_cache = {}
        self.ai_models_cache = (None, None)

    def ai_models(self):
        """Model profiles as the bot reads them; tokens are reported only as a source."""
        path = ai_models_path()
        try:
            key = (str(path), path.stat().st_mtime_ns)
        except OSError:
            return {"path": str(path), "exists": False, "models": [], "default": None}
        if self.ai_models_cache[0] != key:
            try:
                result = subprocess.run([str(campaign.BINARY), "ai-profiles"], capture_output=True, text=True, timeout=20)
                summary = json.loads(result.stdout.strip().splitlines()[-1])
                profiles = summary.get("profiles", {})
                value = {"path": str(path), "exists": True, "models": profiles.get("models", []), "default": profiles.get("default"),
                         "error": profiles.get("error"), "budget_in_file": profiles.get("budget_in_file")}
            except (OSError, ValueError, IndexError, subprocess.SubprocessError):
                value = {"path": str(path), "exists": True, "models": [], "default": None, "error": "Не удалось прочитать файл профилей"}
            self.ai_models_cache = (key, value)
        return self.ai_models_cache[1]

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

    def call(self, method, body):
        with self.api_lock:
            if self.api is None:
                self.api = campaign.API()
            return self.api.call(method, body)

    def describe(self, uid):
        """Catalog entry of an instrument or series, tradable or not; cached for an hour."""
        cached = self.instrument_cache.get(uid)
        if cached and cached[0] > dt.datetime.now(dt.timezone.utc) - dt.timedelta(hours=1):
            return cached[1]
        if SERIES_ID.fullmatch(uid):
            value = self.series(SERIES_ID.fullmatch(uid).group(1))
        else:
            raw = self.call("InstrumentsService/GetInstrumentBy", {"idType": "INSTRUMENT_ID_TYPE_UID", "id": uid})["instrument"]
            details = {}
            if raw.get("instrumentType") == "futures":
                details = self.call("InstrumentsService/FutureBy", {"idType": "INSTRUMENT_ID_TYPE_UID", "id": uid})["instrument"]
                margin = self.call("InstrumentsService/GetFuturesMargin", {"instrumentId": uid})
                details.update({k: v for k, v in margin.items() if v})
            elif raw.get("instrumentType") == "bond":
                details = self.call("InstrumentsService/BondBy", {"idType": "INSTRUMENT_ID_TYPE_UID", "id": uid})["instrument"]
            value = instrument_metadata(raw, details)
        self.instrument_cache[uid] = (dt.datetime.now(dt.timezone.utc), value)
        return value

    def metadata(self, uid):
        """A catalog entry the stand can trade; otherwise a client error with the reason."""
        value = self.describe(uid)
        if not value.get("tradable"):
            raise ClientError(f"{value.get('ticker') or uid}: {value.get('reason') or 'инструмент недоступен'}")
        return value

    def futures(self):
        """Futures that trade now, by contract; refreshed hourly."""
        cached = self.instrument_cache.get("futures-list")
        if not cached or cached[0] < dt.datetime.now(dt.timezone.utc) - dt.timedelta(hours=1):
            listed = self.call("InstrumentsService/Futures", {"instrumentStatus": "INSTRUMENT_STATUS_BASE"}).get("instruments", [])
            cached = (dt.datetime.now(dt.timezone.utc), listed)
            self.instrument_cache["futures-list"] = cached
        return cached[1]

    def series(self, code):
        """A continuous series is described by its current contract: the nearest one
        whose last trading day is more than ROLL_DAYS away."""
        now = dt.datetime.now(dt.timezone.utc)
        contracts = sorted((f for f in self.futures() if series_code(f.get("ticker")) == code and f.get("lastTradeDate")
                            and roll_at(f["lastTradeDate"]) > now),
                           key=lambda f: f["lastTradeDate"])
        if not contracts:
            raise ClientError(f"Нет торгуемых контрактов серии {code}")
        current = self.describe(contracts[0]["uid"])
        return dict(current, uid="futures:" + code, ticker=code, series=code, continuous=True,
                    name=f"{current.get('basic_asset') or code} · непрерывный фьючерс",
                    contract=current["ticker"], contract_uid=current["uid"], roll_at=roll_at(contracts[0]["lastTradeDate"]).isoformat(),
                    contracts=[f["ticker"] for f in contracts[:4]])

    def documents(self, params):
        """Document feed for the operator: full-text search, kind, source, ticker, period."""
        text = str(params.get("q", "")).strip()
        kind, source, ticker = (str(params.get(k, "")).strip() for k in ("kind", "source", "ticker"))
        try:
            days = min(max(int(params.get("days", 3)), 1), 400)
        except (TypeError, ValueError):
            raise ClientError("Период — число дней") from None
        if len(text) > 200 or kind not in DOCUMENT_KINDS | {""} or len(source) > 60 or not re.fullmatch(r"[A-Za-z0-9.]{0,12}", ticker):
            raise ClientError("Некорректный фильтр документов")
        return self.sql(f"""WITH q AS (SELECT nullif(replace(plainto_tsquery('russian', {literal(text)})::text, '&', '|'), '')::tsquery AS q)
          SELECT json_build_object('documents', coalesce((SELECT json_agg(t ORDER BY t.published_at DESC) FROM (
            SELECT d.id, d.source, d.kind, d.title, d.url, d.published_at, d.tickers, d.priority, d.importance, d.status,
              left(coalesce(d.summary, nullif(d.lead, ''), d.body), 280) AS preview,
              (SELECT count(DISTINCT x.source) FROM documents x WHERE x.cluster_id = d.cluster_id) AS sources
            FROM documents d, q
            WHERE d.published_at >= now() - make_interval(days => {days})
              AND (q.q IS NULL OR EXISTS (SELECT 1 FROM document_chunks c WHERE c.document_id = d.id AND c.search @@ q.q))
              AND ({literal(kind)} = '' OR d.kind = {literal(kind)})
              AND ({literal(source)} = '' OR d.source = {literal(source)})
              AND ({literal(ticker)} = '' OR upper({literal(ticker)}) = ANY(d.tickers)
                   OR EXISTS (SELECT 1 FROM document_chunks c WHERE c.document_id = d.id AND c.ord = 0 AND c.search @@ plainto_tsquery('simple', {literal(ticker)})))
            ORDER BY d.published_at DESC LIMIT 100) t), '[]'::json))""")

    def document(self, document_id):
        if not isinstance(document_id, str) or not re.fullmatch(r"[a-z]+:[A-Za-z0-9]{1,40}", document_id):
            raise ClientError("Некорректный идентификатор документа")
        found = self.sql(f"""SELECT coalesce((SELECT row_to_json(t) FROM (SELECT d.id, d.source, d.kind, d.title, d.url, d.lead, d.body, d.tables,
            d.published_at, d.fetched_at, d.tickers, d.priority, d.importance, d.event_type, d.summary, d.topics, d.status,
            (SELECT coalesce(json_agg(json_build_object('id', x.id, 'source', x.source, 'title', x.title, 'published_at', x.published_at) ORDER BY x.published_at), '[]')
               FROM documents x WHERE x.cluster_id = d.cluster_id AND x.id <> d.id) AS same_event
            FROM documents d WHERE d.id = {literal(document_id)}) t), '{{}}'::json)""")
        if not found:
            raise ClientError("Документ не найден", 404)
        return found

    def search(self, query, kind=""):
        """Any instrument of T-Invest by ticker or name; each result says whether the
        stand can trade it. A dated futures contract also brings its continuous series."""
        if not isinstance(query, str) or not 1 <= len(query.strip()) <= 80:
            raise ClientError("Введите тикер или название, до 80 символов")
        if kind not in INSTRUMENT_TYPES and kind != "":
            raise ClientError("Неизвестный тип инструмента")
        body = {"query": query.strip(), "apiTradeAvailableFlag": True}
        if kind:
            body["instrumentKind"] = INSTRUMENT_TYPES[kind]
        response = self.call("InstrumentsService/FindInstrument", body)
        values, seen = [], set()
        for raw in response.get("instruments", [])[:20]:
            code = series_code(raw.get("ticker")) if raw.get("instrumentType") == "futures" else None
            for uid in (["futures:" + code] if code else []) + [raw.get("uid")]:
                if uid in seen:
                    continue
                seen.add(uid)
                try:
                    values.append(self.describe(uid if uid.startswith("futures:") else identifier(uid)))
                except (ClientError, RuntimeError, KeyError):
                    continue
        # The exact ticker first, then what the stand trades, shares before bonds.
        order = ["share", "etf", "futures", "currency", "bond"]
        wanted = query.strip().upper()
        values.sort(key=lambda v: ((v.get("ticker") or "").upper() != wanted, not v.get("tradable"),
                                   order.index(v["kind"]) if v.get("kind") in order else len(order)))
        return {"instruments": values}

    def data(self):
        data = self.sql("""SELECT json_build_object(
          'available',true,'mode',(SELECT mode FROM dashboard_settings WHERE id=true),
          'max_runtimes',(SELECT max_runtimes FROM dashboard_settings WHERE id=true),
          'min_trade_percent',(SELECT min_trade_percent FROM dashboard_settings WHERE id=true),
          'ai_budget_rub',(SELECT ai_budget_rub FROM dashboard_settings WHERE id=true),
          'usd_rub',(SELECT row_to_json(f) FROM fx_rates f WHERE currency='usd'),
          'commissions',(SELECT coalesce(json_agg(c ORDER BY c.contour,c.kind),'[]'::json) FROM commission_rates c),
          'budget_history',(SELECT coalesce(json_agg(t),'[]'::json) FROM (SELECT id,strategy_id,account_id,status,payload,result,created_at,updated_at FROM control_commands WHERE kind='budget' ORDER BY created_at DESC LIMIT 200) t),
          'cases',(SELECT coalesce(json_agg(t),'[]'::json) FROM strategy_controls t),
          'commands',(SELECT coalesce(json_agg(t),'[]'::json) FROM (SELECT * FROM control_commands ORDER BY created_at DESC LIMIT 100) t),
          'batches',(SELECT coalesce(json_agg(t),'[]'::json) FROM batch_controls t),
          'catalog',(SELECT coalesce(json_agg(metadata),'[]'::json) FROM instrument_catalog),
          'ai_decisions',(SELECT coalesce(json_agg(t),'[]'::json) FROM (SELECT d.id,d.strategy_id,d.account_id,d.kind,d.status,d.session_date,d.profile,d.provider,d.model,d.weights,d.rationale,d.sources,d.tool_calls,d.error,d.duration_ms,d.created_at,d.trigger,d.hold,d.usage,d.cost_usd,coalesce(d.cost_rub,d.cost_usd*coalesce((SELECT rub FROM fx_rates WHERE currency='usd'),100)) AS cost_rub,(SELECT json_object_agg(o.horizon_days || 'd',json_build_object('portfolio',o.portfolio_return,'imoex',o.benchmark_return)) FROM ai_outcomes o WHERE o.decision_id=d.id) AS outcome FROM ai_decisions d ORDER BY d.created_at DESC LIMIT 300) t),
          'ai_memory',(SELECT coalesce(json_agg(t),'[]'::json) FROM (SELECT id,strategy_id,note,tickers,created_at,expires_at,inherited_from,kind FROM ai_memory WHERE closed_at IS NULL AND (expires_at IS NULL OR expires_at>now()) ORDER BY created_at DESC LIMIT 200) t),
          'ai_theses',(SELECT coalesce(json_agg(t ORDER BY t.closed_at IS NOT NULL, coalesce(t.closed_at,t.opened_at) DESC),'[]'::json) FROM (SELECT id,strategy_id,ticker,direction,thesis,horizon_days,entry_price,conditions,
             opened_at,closed_at,close_reason,close_price,result_percent,inherited_from IS NOT NULL AS inherited FROM ai_theses
             WHERE closed_at IS NULL OR closed_at >= now()-interval '30 days' ORDER BY opened_at DESC LIMIT 300) t),
          'collector',(SELECT coalesce(json_agg(t ORDER BY t.source),'[]'::json) FROM collector_state t),
          'documents',(SELECT json_build_object('total',count(*),'day',count(*) FILTER (WHERE published_at >= now()-interval '1 day'),
             'newest',max(fetched_at),'kinds',(SELECT coalesce(json_object_agg(kind,n),'{}') FROM (SELECT kind,count(*) n FROM documents
             WHERE published_at >= now()-interval '1 day' GROUP BY kind) k),
             'sources',(SELECT coalesce(json_agg(source ORDER BY source),'[]') FROM (SELECT DISTINCT source FROM documents) s)) FROM documents),
          'exposures',(SELECT coalesce(json_agg(json_build_object('ticker',p.ticker,'name',p.name,'sector',p.sector,'note',p.note,
             'drafted_at',p.drafted_at,'confirmed_at',p.confirmed_at,'draft_error',p.draft_error,
             'exposures',(SELECT coalesce(json_object_agg(e.topic,json_build_object('value',e.sensitivity,'note',e.note,'origin',e.origin)),'{}')
               FROM instrument_exposures e WHERE e.ticker=p.ticker),
             'measured',(SELECT coalesce(json_object_agg(m.topic,json_build_object('suggested',m.suggested,'effect',m.effect_percent,
               'correlation',m.correlation,'observations',m.observations,'at',m.computed_at)),'{}')
               FROM exposure_suggestions m WHERE m.ticker=p.ticker)) ORDER BY p.ticker),'[]'::json) FROM instrument_profiles p),
          'ai_now',(SELECT coalesce(json_agg(json_build_object('strategy_id',s.strategy_id,
             'activity',(SELECT row_to_json(a) FROM ai_activity a WHERE a.strategy_id=s.strategy_id),
             'last',(SELECT json_build_object('created_at',d.created_at,'status',d.status,'trigger',d.trigger,'hold',d.hold,'weights',d.weights,
                'error',left(d.error,300),'cost_rub',coalesce(d.cost_rub,d.cost_usd*coalesce((SELECT rub FROM fx_rates WHERE currency='usd'),100)),'model',d.model,'profile',d.profile,'duration_ms',d.duration_ms)
                FROM ai_decisions d WHERE d.strategy_id=s.strategy_id AND d.kind='decision' ORDER BY d.created_at DESC LIMIT 1),
             'today',(SELECT json_build_object('decisions',count(*) FILTER (WHERE d.status='ok'),'failures',count(*) FILTER (WHERE d.status<>'ok'),
                'cost_rub',sum(coalesce(d.cost_rub,d.cost_usd*coalesce((SELECT rub FROM fx_rates WHERE currency='usd'),100)))) FROM ai_decisions d WHERE d.strategy_id=s.strategy_id AND d.kind='decision'
                AND d.created_at >= (date_trunc('day', now() AT TIME ZONE 'Europe/Moscow') AT TIME ZONE 'Europe/Moscow')))),'[]'::json)
             FROM (SELECT strategy_id FROM ai_decisions WHERE kind='decision' UNION SELECT strategy_id FROM ai_activity) s),
          'ai_rating',(SELECT coalesce(json_agg(r),'[]'::json) FROM (SELECT d.model,max(d.provider) AS provider,
             string_agg(DISTINCT d.profile,', ') AS profiles,json_agg(DISTINCT d.strategy_id) AS strategies,
             count(*) FILTER (WHERE d.status='ok') AS decisions,count(*) FILTER (WHERE d.status<>'ok') AS failures,
             count(*) FILTER (WHERE d.status='ok' AND d.hold) AS holds,round(avg(d.duration_ms) FILTER (WHERE d.status='ok')) AS avg_ms,
             sum(coalesce(d.cost_rub,d.cost_usd*coalesce((SELECT rub FROM fx_rates WHERE currency='usd'),100))) AS cost_rub,min(d.created_at) AS first_at,max(d.created_at) AS last_at,
             (SELECT json_build_object('n',count(*),'beat',count(*) FILTER (WHERE o.portfolio_return>o.benchmark_return),
                'excess',avg(o.portfolio_return-o.benchmark_return)) FROM ai_outcomes o JOIN ai_decisions x ON x.id=o.decision_id
                WHERE x.model IS NOT DISTINCT FROM d.model AND o.horizon_days=5) AS h5,
             (SELECT json_build_object('n',count(*),'beat',count(*) FILTER (WHERE o.portfolio_return>o.benchmark_return),
                'excess',avg(o.portfolio_return-o.benchmark_return)) FROM ai_outcomes o JOIN ai_decisions x ON x.id=o.decision_id
                WHERE x.model IS NOT DISTINCT FROM d.model AND o.horizon_days=20) AS h20
             FROM ai_decisions d WHERE d.kind='decision' GROUP BY d.model) r),
          'ai_usage',(SELECT coalesce(json_agg(t ORDER BY t.day DESC,t.purpose),'[]'::json) FROM (SELECT (at AT TIME ZONE 'Europe/Moscow')::date AS day,purpose,
             sum(coalesce(cost_rub,cost_usd*coalesce((SELECT rub FROM fx_rates WHERE currency='usd'),100))) AS cost_rub,count(*) FILTER (WHERE cost_usd IS NULL AND cost_rub IS NULL) AS unpriced,sum(requests) AS requests,
             sum(input_tokens+cache_read_tokens+cache_write_tokens) AS input_tokens,sum(cache_read_tokens) AS cache_read_tokens,sum(output_tokens) AS output_tokens
             FROM ai_usage WHERE at>=now()-interval '8 days' GROUP BY 1,2) t),
          'runs',(SELECT coalesce(json_agg(t),'[]'::json) FROM (SELECT r.*,
             (SELECT count(*) FROM fills f JOIN trade_intents i ON i.request_id=f.request_id
              WHERE i.strategy_id=r.strategy_id AND i.account_id=r.account_id AND f.executed_at>=r.started_at
                AND (r.ended_at IS NULL OR f.executed_at<r.ended_at)) AS fills,
             (SELECT json_agg(json_build_array(d.day,d.equity,d.external_cash_flows) ORDER BY d.day) FROM (
                SELECT DISTINCT ON (1) (p.observed_at AT TIME ZONE 'Europe/Moscow')::date AS day,p.equity,p.external_cash_flows
                FROM performance_samples p WHERE p.run_id=r.id ORDER BY 1,p.observed_at DESC) d) AS days,
             (SELECT json_build_object('trades',count(*),'volume_rub',sum(c.volume_rub),'commission_rub',sum(c.commission_rub))
                FROM trade_commissions c WHERE c.account_id=r.account_id AND c.executed_at>=r.started_at
                AND (r.ended_at IS NULL OR c.executed_at<r.ended_at)) AS costs FROM strategy_runs r) t));""")
        state = self.state()
        names = {c["strategy_id"]: c["kind"] for c in state.get("cases", {}).values()}
        for case in state.get("cases", {}).values():
            for revision in case.get("revisions", []):
                names[revision["strategy_id"]] = case["kind"]
        for case in state.get("archived_cases", []):
            names[case["strategy_id"]] = case["kind"]
        data["ranking"] = ranking(data.pop("runs"), names, benchmark_closes(self.work, state.get("benchmark")))
        data["mode"] = data["mode"] or "campaign"
        data["runtime_cap"] = runtime_cap(state)
        data["live"] = self.live_data(state)
        data["ai_models"] = self.ai_models()
        data["topics"] = TOPICS
        data["rules"] = {"share_only_kinds": sorted(stand_rules.SHARE_ONLY_KINDS), "roll_days": ROLL_DAYS}
        data["operator_pin"] = (self.work / "operator-pin.json").exists()
        data["observed_at"] = dt.datetime.now(dt.timezone.utc).isoformat()
        return data

    def get_command(self, command_id):
        row = self.sql(f"SELECT coalesce((SELECT row_to_json(c) FROM control_commands c WHERE id={literal(identifier(command_id))}::uuid),'{{}}'::json)")
        if not row:
            raise ClientError("Команда не найдена", 404)
        return {"command": row}

    def apply_setting(self, command, state):
        """Schedule and runtime limit: validated, stored and journaled with before/after."""
        kind, payload, cid = command["kind"], command["payload"], literal(command["id"])
        existing = self.sql(f"SELECT coalesce((SELECT row_to_json(c) FROM control_commands c WHERE id={cid}::uuid),'{{}}'::json)")
        if existing:
            if any(existing.get(key) != command[key] for key in ["id", "kind", "strategy_id", "account_id", "payload"]):
                raise ClientError("Этот идентификатор уже использован для другой команды", 409)
            return {"command": existing}
        if kind in {"live_halt", "live_resume", "live_gate"}:
            change, result = self.live_setting(kind, payload)
        elif kind.startswith("exposure_"):
            change, result = self.exposure_change(kind, payload)
        elif kind == "ai_budget":
            before = self.sql("SELECT json_build_object('rub',ai_budget_rub) FROM dashboard_settings WHERE id=true")
            change = f"UPDATE dashboard_settings SET ai_budget_rub={literal(payload['rub'])}::numeric,updated_at=now() WHERE id=true;"
            result = {"before": before, "after": {"rub": payload["rub"]},
                      "message": f"Лимит расходов на ИИ: {before['rub']} ₽ → {payload['rub']} ₽ в сутки; действует сразу"}
        elif kind == "trade_band":
            before = self.sql("SELECT json_build_object('percent',min_trade_percent) FROM dashboard_settings WHERE id=true")
            change = f"UPDATE dashboard_settings SET min_trade_percent={literal(payload['percent'])}::numeric,updated_at=now() WHERE id=true;"
            result = {"before": before, "after": {"percent": payload["percent"]},
                      "message": f"Порог сделки: {before['percent']}% → {payload['percent']}% капитала; процессы применят его на ближайшем открытии сессии"}
        elif kind == "runtime_limit":
            cap = runtime_cap(state)
            if payload["value"] > cap:
                raise ClientError(f"Тариф T-Invest позволяет не больше {cap} процессов одновременно")
            before = self.sql("SELECT json_build_object('value',max_runtimes) FROM dashboard_settings WHERE id=true")
            change = f"UPDATE dashboard_settings SET max_runtimes={int(payload['value'])},updated_at=now() WHERE id=true;"
            result = {"before": before, "after": {"value": payload["value"]},
                      "message": f"Одновременно процессов: {before['value']} → {payload['value']}"}
        else:
            sid, aid = literal(command["strategy_id"]), literal(command["account_id"])
            before = self.sql(f"SELECT coalesce((SELECT json_build_object('run_mode',run_mode,'trading_days',trading_days,'close_after',close_after) FROM strategy_controls WHERE strategy_id={sid}),'{{}}'::json)")
            others = self.sql(f"SELECT coalesce(json_agg(json_build_object('strategy_id',strategy_id,'run_mode',run_mode,'trading_days',trading_days)),'[]'::json) FROM strategy_controls WHERE account_id={aid} AND strategy_id<>{sid} AND superseded_by IS NULL")
            check_schedule(state, payload, before, others)
            change = (f"INSERT INTO strategy_controls(strategy_id,account_id,enabled) VALUES({sid},{aid},true) ON CONFLICT DO NOTHING; "
                      f"UPDATE strategy_controls SET run_mode={literal(payload['run_mode'])},trading_days={literal(json.dumps(payload['days']))}::jsonb,"
                      f"close_after={'true' if payload['close_after'] else 'false'},schedule_initialized=true,updated_at=now() "
                      f"WHERE strategy_id={sid} AND superseded_by IS NULL;")
            after = {"run_mode": payload["run_mode"], "trading_days": payload["days"], "close_after": payload["close_after"]}
            result = {"before": before, "after": after, "message": "Постоянно, пока включена" if payload["run_mode"] == "always" else
                      ("Дни торговли: " + ", ".join(payload["days"]) if payload["days"] else "Дни торговли не выбраны")}
        row = self.sql("\n".join(["BEGIN;", change,
            f"INSERT INTO control_commands(id,strategy_id,account_id,kind,payload,status,result) VALUES({cid}::uuid,{literal(command['strategy_id'])},{literal(command['account_id'])},"
            f"{literal(kind)},{literal(json.dumps(payload))}::jsonb,'succeeded',{literal(json.dumps(result, ensure_ascii=False))}::jsonb);",
            f"SELECT row_to_json(c) FROM control_commands c WHERE id={cid}::uuid;", "COMMIT;"]))
        return {"command": row}

    def exposure_change(self, kind, payload):
        """Operator edits of an instrument profile: a value, confirmation, a fresh draft."""
        ticker = literal(payload["ticker"])
        if not self.sql(f"SELECT json_build_object('known',EXISTS(SELECT 1 FROM instrument_profiles WHERE ticker={ticker}))")["known"]:
            raise ClientError("Профиля этой бумаги нет: он появляется с первым решением модели")
        if kind == "exposure_confirm":
            return (f"UPDATE instrument_profiles SET confirmed_at=now(),updated_at=now() WHERE ticker={ticker};",
                    {"message": f"Профиль {payload['ticker']} подтверждён"})
        if kind == "exposure_redraft":
            return (f"UPDATE instrument_profiles SET drafted_at=NULL,draft_error=NULL,confirmed_at=NULL,updated_at=now() WHERE ticker={ticker};",
                    {"message": f"Черновик {payload['ticker']} будет пересоздан; ваши значения сохранятся"})
        topic = literal(payload["topic"])
        before = self.sql(f"SELECT coalesce((SELECT json_build_object('sensitivity',sensitivity,'note',note,'origin',origin) FROM instrument_exposures WHERE ticker={ticker} AND topic={topic}),'{{}}'::json)")
        name = TOPICS[payload["topic"]]
        if payload.get("sensitivity") is None:
            return (f"DELETE FROM instrument_exposures WHERE ticker={ticker} AND topic={topic};",
                    {"before": before, "message": f"{payload['ticker']}: тема «{name}» убрана"})
        change = (f"INSERT INTO instrument_exposures(ticker,topic,sensitivity,note,origin) VALUES({ticker},{topic},{literal(payload['sensitivity'])}::numeric,"
                  f"{literal(payload.get('note', ''))},'operator') ON CONFLICT (ticker,topic) DO UPDATE SET sensitivity=EXCLUDED.sensitivity,"
                  f"note=EXCLUDED.note,origin='operator',updated_at=now();")
        return change, {"before": before, "after": {"sensitivity": payload["sensitivity"], "note": payload.get("note", "")},
                        "message": f"{payload['ticker']}: «{name}» = {payload['sensitivity']}"}

    def live_setting(self, kind, payload):
        """Emergency stop, its release and the promotion gate thresholds."""
        if kind == "live_halt":
            # Disabling every real strategy makes each runtime stop new plans within a
            # second; the supervisor then stops the processes. Positions are kept.
            change = (f"UPDATE live_settings SET halted=true,halted_at=now(),halt_reason={literal(payload['reason'])},updated_at=now() WHERE id=true; "
                      "UPDATE strategy_controls SET enabled=false,revision=revision+1,updated_at=now() WHERE contour='production' AND enabled;")
            return change, {"message": "Аварийная остановка: реальные стратегии выключены, позиции сохранены", "reason": payload["reason"]}
        if kind == "live_resume":
            return ("UPDATE live_settings SET halted=false,halt_reason=NULL,updated_at=now() WHERE id=true;",
                    {"message": "Аварийная остановка снята; реальные стратегии остаются выключенными до «Включить»"})
        before = self.sql("SELECT json_build_object('min_sessions',min_sessions,'min_fills',min_fills,'min_ai_decisions',min_ai_decisions,'require_no_risk_block',require_no_risk_block) FROM live_settings WHERE id=true")
        change = (f"UPDATE live_settings SET min_sessions={int(payload['min_sessions'])},min_fills={int(payload['min_fills'])},"
                  f"min_ai_decisions={int(payload['min_ai_decisions'])},"
                  f"require_no_risk_block={'true' if payload['require_no_risk_block'] else 'false'},updated_at=now() WHERE id=true;")
        return change, {"before": before, "after": payload, "message": f"Порог допуска: {payload['min_sessions']} сессий, "
                        f"{payload['min_fills']} сделок (ИИ-портфели — {payload['min_ai_decisions']} решений модели), "
                        + ("без риск-блокировок" if payload["require_no_risk_block"] else "блокировки допускаются")}

    def live_data(self, state):
        """Real-account panel: connection, emergency stop, gate and the account survey."""
        settings = self.sql("SELECT row_to_json(l) FROM live_settings l WHERE id=true")
        gate = self.sql(campaign.control_plane.GATE_SQL.format(where=""))
        rules = {key: settings[key] for key in ["min_sessions", "min_fills", "min_ai_decisions", "require_no_risk_block"]}
        names = {c["strategy_id"]: c for c in state.get("cases", {}).values()}
        for row in gate:
            case = names.get(row["strategy_id"], {})
            ai = campaign.control_plane.uses_ai(campaign, case) if case else False
            row.update(kind=case.get("kind"), title=case.get("title") or case.get("kind"), ai=ai,
                       failures=campaign.control_plane.gate_failures(row, rules, ai))
            row["passed"] = not row["failures"]
        # Ready and running portfolios first, then the closest to the threshold.
        gate.sort(key=lambda r: (not r["passed"], not r.get("enabled"), -r["sessions"], -r["fills"], r.get("title") or ""))
        status = campaign.live_status({"live": settings})
        return {"status": status, "settings": rules, "halted": settings["halted"], "halted_at": settings["halted_at"],
                "halt_reason": settings["halt_reason"], "accounts": settings["accounts"],
                "accounts_observed_at": settings["accounts_observed_at"], "accounts_error": settings["accounts_error"],
                "gate": [row for row in gate if row.get("kind")]}

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
        if kind in {"portfolio", "portfolio_create"}:
            for block in payload["blocks"]:
                for uid in block["instruments"]:
                    self.metadata(uid)
        if kind == "resume":
            case = next(c for c in state["cases"].values() if c["strategy_id"] == command["strategy_id"])
            if case.get("contour") == "production":
                halted = self.sql("SELECT json_build_object('halted',halted) FROM live_settings WHERE id=true")["halted"]
                status = campaign.live_status({"live": {"halted": halted}})
                if not status["ready"]:
                    raise ClientError("Реальная торговля недоступна: " + ("включена аварийная остановка" if halted else
                                      "не подключён токен TINKOFF_TOKEN_PRODUCTION"), 409)
        if kind in IMMEDIATE_COMMANDS:
            return self.apply_setting(command, state)
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
            guard += f" AND NOT EXISTS(SELECT 1 FROM control_commands WHERE strategy_id={sid} AND kind IN ('universe','portfolio') AND status IN ('queued','running'))"
        insert = (f"INSERT INTO control_commands(id,strategy_id,account_id,kind,payload) SELECT {cid}::uuid,{sid},{aid},"
                  f"{literal(kind)},{literal(json.dumps(payload))}::jsonb WHERE {guard} ON CONFLICT DO NOTHING RETURNING id")
        ctes = ["inserted AS (" + insert + ")"]
        if command["strategy_id"]:
            pause = "enabled=false," if kind in {"pause", "manual_order", "cleanup", "universe", "portfolio"} else ""
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
