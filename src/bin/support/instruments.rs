//! Инструменты любого типа и непрерывные фьючерсы.
//!
//! Стратегии и engine работают с идентификаторами из конфигурации: UID
//! инструмента T-Invest или серия фьючерсов `futures:BR`. Серию процесс при
//! запуске привязывает к текущему контракту — ближайшему, до перехода по
//! которому ещё есть время (переход за [`ROLL_DAYS`] дней до последнего дня
//! торгов, в начале основной сессии). Обёртки контура ([`super::venue`])
//! подставляют этот контракт в запросы к брокеру и возвращают серию в ответах,
//! а историю серии склеивают из контрактов с поправкой цен на разрыв при
//! переходе. Когда наступает переход, runtime переносит позицию в следующий
//! контракт и перезапускается, чтобы привязаться к нему.
//!
//! Рублёвая стоимость единицы цены зависит от типа: у акций, фондов и валюты
//! это рубль, у облигаций — процент номинала (плюс НКД), у фьючерсов — пункт со
//! своей стоимостью шага цены.
use std::{
    collections::HashMap,
    sync::{Mutex, RwLock},
};

use chrono::{DateTime, Duration, NaiveTime, Utc};
use rust_decimal::Decimal;
use tinkoff_rs::{
    Error, Result,
    api::InstrumentsApiClient,
    broker::MarketData,
    domain::{
        Candle, CandleHistoryRequest, CandleInterval, InstrumentId, Money, Price,
        timestamp_to_datetime,
    },
    engine::{InstrumentInfo, InstrumentKind},
    proto::{
        GetFuturesMarginRequest, InstrumentIdType, InstrumentRequest, InstrumentStatus,
        InstrumentsRequest, RealExchange,
    },
};

use super::venue::{VenueApi, VenueBroker};

/// Префикс идентификатора непрерывной серии фьючерсов.
pub(super) const SERIES_PREFIX: &str = "futures:";
/// За сколько дней до последнего дня торгов серия переходит на следующий контракт.
pub(super) const ROLL_DAYS: i64 = 5;
/// Переход — к началу основной сессии MOEX (10:00 МСК).
const ROLL_TIME_UTC: (u32, u32) = (7, 0);
/// Буквы месяцев в тикерах фьючерсов MOEX: BRX6 — ноябрь 2026.
const MONTHS: &str = "FGHJKMNQUVXZ";

/// Код серии в идентификаторе `futures:BR`.
pub(super) fn series_code(id: &InstrumentId) -> Option<&str> {
    id.as_str()
        .strip_prefix(SERIES_PREFIX)
        .filter(|code| !code.is_empty())
}

/// Код серии срочного контракта: `BRX6` → `BR`, `SiZ6` → `Si`. У вечных фьючерсов
/// (USDRUBF) и прочих инструментов серии нет.
pub(super) fn dated_code(ticker: &str) -> Option<&str> {
    let bytes = ticker.as_bytes();
    (bytes.len() == 4
        && bytes[..2].iter().all(u8::is_ascii_alphanumeric)
        && MONTHS.as_bytes().contains(&bytes[2])
        && bytes[3].is_ascii_digit())
    .then(|| &ticker[..2])
}

/// Контракт серии.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Contract {
    pub uid: InstrumentId,
    pub ticker: String,
    pub last_trade: DateTime<Utc>,
}

impl Contract {
    /// Момент перехода с этого контракта на следующий.
    pub(super) fn roll_at(&self) -> DateTime<Utc> {
        let day = self.last_trade.date_naive() - Duration::days(ROLL_DAYS);
        day.and_time(
            NaiveTime::from_hms_opt(ROLL_TIME_UTC.0, ROLL_TIME_UTC.1, 0).unwrap_or_default(),
        )
        .and_utc()
    }
}

/// Серия: контракты по возрастанию последнего дня торгов, включая истёкшие.
#[derive(Clone, Debug)]
pub(super) struct Series {
    pub code: String,
    pub contracts: Vec<Contract>,
}

impl Series {
    /// Индекс текущего контракта на момент `at`.
    pub(super) fn active(&self, at: DateTime<Utc>) -> Option<usize> {
        self.contracts.iter().position(|c| c.roll_at() > at)
    }

    /// Куски окна `[from, to)` по контрактам до `last` включительно: контракт
    /// `i` отвечает за время от перехода с `i − 1` до своего перехода.
    pub(super) fn segments(
        &self,
        last: usize,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Vec<(usize, DateTime<Utc>, DateTime<Utc>)> {
        let mut out = Vec::new();
        for index in 0..=last.min(self.contracts.len().saturating_sub(1)) {
            let start = if index == 0 {
                DateTime::<Utc>::MIN_UTC
            } else {
                self.contracts[index - 1].roll_at()
            };
            let end = if index == last {
                DateTime::<Utc>::MAX_UTC
            } else {
                self.contracts[index].roll_at()
            };
            let (left, right) = (from.max(start), to.min(end));
            if left < right {
                out.push((index, left, right));
            }
        }
        out
    }
}

/// Переход серии, который runtime должен выполнить.
#[derive(Clone, Debug)]
pub(super) struct Roll {
    pub series: InstrumentId,
    pub from: Contract,
    pub to: Contract,
}

/// Серии процесса и их привязка к контрактам.
#[derive(Default)]
pub(super) struct SeriesBook {
    series: RwLock<HashMap<String, Series>>,
    /// Индекс контракта, к которому серия привязана в этом процессе.
    bound: RwLock<HashMap<String, usize>>,
    /// Отношение цен нового контракта к старому в момент перехода `(код, индекс старого)`.
    factors: Mutex<HashMap<(String, usize), Decimal>>,
}

impl SeriesBook {
    /// Загружает серии из списка и привязывает каждую к текущему контракту.
    pub(super) async fn load(
        &self,
        mut client: InstrumentsApiClient,
        ids: &[InstrumentId],
        now: DateTime<Utc>,
    ) -> Result<()> {
        let mut codes: Vec<&str> = ids.iter().filter_map(series_code).collect();
        codes.retain(|code| !self.series.read().is_ok_and(|s| s.contains_key(*code)));
        if codes.is_empty() {
            return Ok(());
        }
        let futures = client
            .futures(InstrumentsRequest {
                instrument_status: Some(InstrumentStatus::All as i32),
                instrument_exchange: None,
            })
            .await?
            .into_inner()
            .instruments;
        for code in codes {
            let mut contracts = Vec::new();
            for future in futures.iter().filter(|f| {
                dated_code(&f.ticker) == Some(code)
                    && f.real_exchange == RealExchange::Moex as i32
                    && f.currency.eq_ignore_ascii_case("rub")
            }) {
                let Some(last) = future.last_trade_date else {
                    continue;
                };
                contracts.push(Contract {
                    uid: InstrumentId::new(future.uid.clone())?,
                    ticker: future.ticker.clone(),
                    last_trade: timestamp_to_datetime(last)?,
                });
            }
            contracts.sort_by_key(|c| c.last_trade);
            contracts.dedup_by_key(|c| c.last_trade);
            let series = Series {
                code: code.to_owned(),
                contracts,
            };
            let active = series.active(now).ok_or_else(|| {
                Error::Configuration(format!(
                    "futures series `{code}` has no contract to trade on MOEX in rubles"
                ))
            })?;
            self.bound
                .write()
                .map_err(|_| poisoned())?
                .insert(code.to_owned(), active);
            self.series
                .write()
                .map_err(|_| poisoned())?
                .insert(code.to_owned(), series);
        }
        Ok(())
    }

    /// Контракт, который стоит за идентификатором в этом процессе.
    pub(super) fn contract(&self, id: &InstrumentId) -> InstrumentId {
        series_code(id)
            .and_then(|code| self.bound_contract(code))
            .map_or_else(|| id.clone(), |c| c.uid)
    }

    /// Привязанный контракт серии.
    pub(super) fn bound_contract(&self, code: &str) -> Option<Contract> {
        let index = *self.bound.read().ok()?.get(code)?;
        self.series
            .read()
            .ok()?
            .get(code)?
            .contracts
            .get(index)
            .cloned()
    }

    /// Идентификатор в терминах процесса для UID из ответа брокера: привязанный
    /// контракт становится серией, остальное не меняется.
    pub(super) fn of_broker(&self, uid: InstrumentId) -> InstrumentId {
        let Ok(bound) = self.bound.read() else {
            return uid;
        };
        let Ok(series) = self.series.read() else {
            return uid;
        };
        for (code, index) in bound.iter() {
            if series
                .get(code)
                .and_then(|s| s.contracts.get(*index))
                .is_some_and(|c| c.uid == uid)
            {
                return InstrumentId::new(format!("{SERIES_PREFIX}{code}")).unwrap_or(uid);
            }
        }
        uid
    }

    /// Серия, к которой относится непривязанный контракт (остаток позиции после
    /// перехода или прерванного переноса).
    pub(super) fn other_contract_series(&self, uid: &InstrumentId) -> Option<InstrumentId> {
        let series = self.series.read().ok()?;
        let bound = self.bound.read().ok()?;
        series.iter().find_map(|(code, s)| {
            s.contracts
                .iter()
                .enumerate()
                .any(|(index, c)| &c.uid == uid && bound.get(code) != Some(&index))
                .then(|| InstrumentId::new(format!("{SERIES_PREFIX}{code}")).ok())
                .flatten()
        })
    }

    /// Серии, у которых наступил переход на следующий контракт.
    pub(super) fn due_rolls(&self, now: DateTime<Utc>) -> Vec<Roll> {
        let (Ok(series), Ok(bound)) = (self.series.read(), self.bound.read()) else {
            return Vec::new();
        };
        let mut rolls = Vec::new();
        for (code, s) in series.iter() {
            let (Some(&index), Some(active)) = (bound.get(code), s.active(now)) else {
                continue;
            };
            if active != index {
                if let (Some(from), Some(to)) = (s.contracts.get(index), s.contracts.get(active)) {
                    rolls.push(Roll {
                        series: InstrumentId::new(format!("{SERIES_PREFIX}{code}"))
                            .unwrap_or_else(|_| from.uid.clone()),
                        from: from.clone(),
                        to: to.clone(),
                    });
                }
            }
        }
        rolls
    }

    /// История серии: куски контрактов до привязанного включительно, цены
    /// старых контрактов умножены на отношения цен при каждом следующем переходе
    /// (доходности сохраняются, разрыв при переходе исчезает).
    pub(super) async fn candles(
        &self,
        broker: &impl MarketData,
        request: CandleHistoryRequest,
    ) -> Result<Vec<Candle>> {
        let code = series_code(&request.instrument_id)
            .ok_or_else(|| Error::InvalidArgument("not a futures series".to_owned()))?
            .to_owned();
        let series = self
            .series
            .read()
            .map_err(|_| poisoned())?
            .get(&code)
            .cloned()
            .ok_or_else(|| {
                Error::Configuration(format!("futures series `{code}` is not loaded"))
            })?;
        let bound = *self
            .bound
            .read()
            .map_err(|_| poisoned())?
            .get(&code)
            .ok_or_else(|| Error::Configuration(format!("futures series `{code}` is not bound")))?;
        let mut out = Vec::new();
        for (index, from, to) in series.segments(bound, request.from, request.to) {
            let contract = &series.contracts[index];
            let piece = broker
                .candles(CandleHistoryRequest {
                    instrument_id: contract.uid.clone(),
                    interval: request.interval,
                    from,
                    to,
                    limit: request.limit,
                })
                .await?
                .data;
            let mut factor = Decimal::ONE;
            for older in index..bound {
                factor *= self.factor(broker, &series, older).await?;
            }
            for mut candle in piece.into_iter().filter(|c| c.time >= from && c.time < to) {
                candle.instrument_id = request.instrument_id.clone();
                if factor != Decimal::ONE {
                    for price in [
                        &mut candle.open,
                        &mut candle.high,
                        &mut candle.low,
                        &mut candle.close,
                    ] {
                        price.0 = (price.0 * factor).round_dp(6);
                    }
                }
                out.push(candle);
            }
        }
        out.sort_by_key(|c| c.time);
        Ok(out)
    }

    /// Отношение цены контракта `older + 1` к цене `older` по последним дневным
    /// закрытиям обоих до перехода; без данных — 1 (склейка без поправки).
    async fn factor(
        &self,
        broker: &impl MarketData,
        series: &Series,
        older: usize,
    ) -> Result<Decimal> {
        let key = (series.code.clone(), older);
        if let Some(value) = self.factors.lock().map_err(|_| poisoned())?.get(&key) {
            return Ok(*value);
        }
        let (Some(old), Some(new)) = (series.contracts.get(older), series.contracts.get(older + 1))
        else {
            return Ok(Decimal::ONE);
        };
        let at = old.roll_at();
        let mut closes = Vec::new();
        for contract in [old, new] {
            let candles = broker
                .candles(CandleHistoryRequest {
                    instrument_id: contract.uid.clone(),
                    interval: CandleInterval::OneDay,
                    from: at - Duration::days(10),
                    to: at,
                    limit: None,
                })
                .await?
                .data;
            closes.push(
                candles
                    .iter()
                    .filter(|c| c.complete && c.time < at)
                    .max_by_key(|c| c.time)
                    .map(|c| c.close.0),
            );
        }
        let value = match (closes[0], closes[1]) {
            (Some(old), Some(new)) if old > Decimal::ZERO && new > Decimal::ZERO => new / old,
            _ => Decimal::ONE,
        };
        self.factors
            .lock()
            .map_err(|_| poisoned())?
            .insert(key, value);
        Ok(value)
    }
}

/// Издержки исполнения контура: комиссия по типам инструментов (тариф или
/// измеренная по фактическим списаниям — см. наблюдатель стенда) и порог
/// минимальной сделки. Без таблиц (тесты, старая база) — нули.
#[derive(Clone, Debug, Default)]
pub(super) struct ExecutionCosts {
    /// Тип инструмента (`share`, `futures`, …, `default`) → доля объёма сделки.
    rates: HashMap<String, Decimal>,
    /// Доля `sizing_capital`, меньше которой изменение позиции не исполняется.
    pub min_trade_fraction: Decimal,
}

impl ExecutionCosts {
    pub(super) async fn load(
        storage: &tinkoff_rs::storage::PostgresStorage,
        contour: &str,
    ) -> Self {
        let rates: Vec<(String, String)> = sqlx::query_as(
            "SELECT kind, rate_percent::text FROM commission_rates WHERE contour = $1",
        )
        .bind(contour)
        .fetch_all(storage.pool())
        .await
        .unwrap_or_default();
        let band: Option<String> =
            sqlx::query_scalar("SELECT min_trade_percent::text FROM dashboard_settings WHERE id")
                .fetch_optional(storage.pool())
                .await
                .ok()
                .flatten();
        let percent = |text: &str| {
            text.parse::<Decimal>()
                .ok()
                .map(|value| value / Decimal::ONE_HUNDRED)
        };
        Self {
            rates: rates
                .into_iter()
                .filter_map(|(kind, rate)| Some((kind, percent(&rate)?)))
                .collect(),
            min_trade_fraction: band.as_deref().and_then(percent).unwrap_or_default(),
        }
    }

    /// Комиссия сделки с инструментом этого типа, доля объёма.
    pub(super) fn rate(&self, kind: InstrumentKind) -> Decimal {
        let name = serde_json::to_value(kind)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        self.rates
            .get(&name)
            .or_else(|| self.rates.get("default"))
            .copied()
            .unwrap_or_default()
    }

    /// Проставляет комиссию инструментам.
    pub(super) fn apply(&self, instruments: &mut [InstrumentInfo]) {
        for info in instruments {
            info.commission_rate = self.rate(info.kind);
        }
    }

    /// Ставки в процентах для модели и журнала.
    pub(super) fn percents(&self) -> serde_json::Value {
        serde_json::Value::Object(
            self.rates
                .iter()
                .map(|(kind, rate)| {
                    (
                        kind.clone(),
                        serde_json::json!((rate * Decimal::ONE_HUNDRED).normalize().to_string()),
                    )
                })
                .collect(),
        )
    }
}

fn poisoned() -> Error {
    Error::Configuration("futures series registry is unavailable".to_owned())
}

/// Тип инструмента по полю `instrument_type` T-Invest.
pub(super) fn kind_of(instrument_type: &str) -> Result<InstrumentKind> {
    Ok(match instrument_type {
        "share" => InstrumentKind::Share,
        "etf" => InstrumentKind::Etf,
        "bond" => InstrumentKind::Bond,
        "futures" => InstrumentKind::Futures,
        "currency" => InstrumentKind::Currency,
        other => {
            return Err(Error::Configuration(format!(
                "instrument type `{other}` is not supported"
            )));
        }
    })
}

/// Свойства инструмента для planner-а и шаг цены. Серия описывается
/// привязанным контрактом, но сохраняет свой идентификатор.
pub(super) async fn describe(
    api: &VenueApi,
    broker: &VenueBroker,
    id: &InstrumentId,
) -> Result<(InstrumentInfo, Option<Decimal>)> {
    let contract = api.series().contract(id);
    let request = |kind: InstrumentIdType| InstrumentRequest {
        id_type: kind as i32,
        class_code: None,
        id: contract.to_string(),
    };
    let instrument = api
        .instruments()
        .get_instrument_by(request(InstrumentIdType::Uid))
        .await?
        .into_inner()
        .instrument
        .ok_or_else(|| {
            Error::Configuration(format!("instrument `{id}` was not returned by T-Invest"))
        })?;
    if instrument.uid != contract.as_str()
        || instrument.real_exchange != RealExchange::Moex as i32
        || !instrument.currency.eq_ignore_ascii_case("rub")
    {
        return Err(Error::Configuration(format!(
            "instrument `{id}` must be a RUB instrument executed on MOEX"
        )));
    }
    if instrument.lot <= 0 || !instrument.api_trade_available_flag {
        return Err(Error::Configuration(format!(
            "instrument `{id}` lacks a lot size or API trading"
        )));
    }
    let kind = kind_of(&instrument.instrument_type)?;
    let (multiplier, accrued) = match kind {
        InstrumentKind::Futures => {
            let margin = api
                .instruments()
                .get_futures_margin(GetFuturesMarginRequest {
                    instrument_id: contract.to_string(),
                    ..Default::default()
                })
                .await?
                .into_inner();
            let step = margin
                .min_price_increment
                .map(Price::from_quotation)
                .transpose()?;
            let amount = margin
                .min_price_increment_amount
                .map(Price::from_quotation)
                .transpose()?;
            match (step, amount) {
                (Some(step), Some(amount))
                    if step.0 > Decimal::ZERO && amount.0 > Decimal::ZERO =>
                {
                    (amount.0 / step.0, Decimal::ZERO)
                }
                _ => {
                    return Err(Error::Configuration(format!(
                        "futures `{id}`: the exchange did not report the value of a price step"
                    )));
                }
            }
        }
        InstrumentKind::Bond => {
            let bond = api
                .instruments()
                .bond_by(request(InstrumentIdType::Uid))
                .await?
                .into_inner()
                .instrument
                .ok_or_else(|| Error::Configuration(format!("bond `{id}` not found")))?;
            let nominal = bond
                .nominal
                .map(Money::from_proto)
                .transpose()?
                .filter(|m| m.currency.eq_ignore_ascii_case("rub") && m.amount.0 > Decimal::ZERO)
                .ok_or_else(|| {
                    Error::Configuration(format!("bond `{id}` must have a ruble nominal"))
                })?;
            let accrued = bond
                .aci_value
                .map(Money::from_proto)
                .transpose()?
                .map_or(Decimal::ZERO, |m| m.amount.0);
            (nominal.amount.0 / Decimal::ONE_HUNDRED, accrued)
        }
        _ => (Decimal::ONE, Decimal::ZERO),
    };
    let availability = broker.trading_availability(id).await?.data;
    let tick = instrument
        .min_price_increment
        .map(Price::from_quotation)
        .transpose()?
        .map(|p| p.0)
        .filter(|p| *p > Decimal::ZERO);
    Ok((
        InstrumentInfo {
            instrument_id: id.clone(),
            lot_size: i64::from(instrument.lot),
            currency: instrument.currency,
            exchange: "moex".to_owned(),
            // Позиция по фьючерсу открывается продажей; флаг шорта у них не заполняется.
            short_enabled: instrument.short_enabled_flag
                || (kind == InstrumentKind::Futures && instrument.sell_available_flag),
            api_trade_available: instrument.api_trade_available_flag
                && availability.api_trade_available,
            market_order_available: availability.market_order_available,
            kind,
            price_multiplier: multiplier,
            accrued_interest: accrued,
            commission_rate: Decimal::ZERO,
        },
        tick,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as Map;
    use tinkoff_rs::{
        domain::{AccountId, StrategyId},
        engine::{
            PlanningContext, RiskAnchor, RiskConfig, RiskManager, RiskMode, RiskObservation,
            TargetPlanner, TargetPortfolio, TargetWeight,
        },
    };

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn contract(ticker: &str, last: &str) -> Contract {
        Contract {
            uid: InstrumentId::new(format!("uid-{ticker}")).unwrap(),
            ticker: ticker.to_owned(),
            last_trade: at(last),
        }
    }

    #[test]
    fn a_series_trades_the_nearest_contract_until_five_days_before_its_last_day() {
        assert_eq!(dated_code("BRX6"), Some("BR"));
        assert_eq!(dated_code("SiZ6"), Some("Si"));
        assert_eq!(
            dated_code("USDRUBF"),
            None,
            "a perpetual future has no roll"
        );
        assert_eq!(dated_code("SBER"), None);
        let series = Series {
            code: "BR".to_owned(),
            contracts: vec![
                contract("BRV6", "2026-10-01T16:00:00Z"),
                contract("BRX6", "2026-11-02T16:00:00Z"),
                contract("BRZ6", "2026-12-01T16:00:00Z"),
            ],
        };
        assert_eq!(series.contracts[1].roll_at(), at("2026-10-28T07:00:00Z"));
        assert_eq!(series.active(at("2026-10-21T12:00:00Z")), Some(1));
        assert_eq!(series.active(at("2026-10-28T06:59:00Z")), Some(1));
        assert_eq!(
            series.active(at("2026-10-28T07:00:00Z")),
            Some(2),
            "the roll happens at 10:00 MSK"
        );
        // History up to the bound contract is split at each roll.
        let pieces = series.segments(1, at("2026-09-01T00:00:00Z"), at("2026-10-21T00:00:00Z"));
        assert_eq!(
            pieces,
            vec![
                (0, at("2026-09-01T00:00:00Z"), at("2026-09-26T07:00:00Z")),
                (1, at("2026-09-26T07:00:00Z"), at("2026-10-21T00:00:00Z")),
            ]
        );
    }

    fn risk_decision(capital: Decimal) -> tinkoff_rs::engine::RiskDecision {
        let now = Utc::now();
        let config: RiskConfig = toml::from_str(&format!("budget_rub = \"{capital}\"")).unwrap();
        RiskManager::new(
            config,
            RiskAnchor {
                account_id: AccountId::new("a").unwrap(),
                strategy_id: StrategyId::new("s").unwrap(),
                account_equity: capital,
                session_date: now.date_naive(),
                observed_at: now,
            },
        )
        .unwrap()
        .observe(&RiskObservation {
            current_equity: capital,
            session_date: now.date_naive(),
            observed_at: now,
            external_cash_flows: Decimal::ZERO,
            external_cash_flows_recognized: true,
            portfolio_is_flat: true,
        })
        .unwrap()
    }

    /// Target lots of one 1 000 ₽ lot instrument on 1 000 000 ₽ of capital.
    fn probe(held: i64, weight: &str, commission: &str, band: &str) -> i64 {
        let id = InstrumentId::new("x").unwrap();
        let info = InstrumentInfo {
            instrument_id: id.clone(),
            lot_size: 1,
            currency: "rub".to_owned(),
            exchange: "moex".to_owned(),
            short_enabled: true,
            api_trade_available: true,
            market_order_available: true,
            kind: InstrumentKind::Share,
            price_multiplier: Decimal::ONE,
            accrued_interest: Decimal::ZERO,
            commission_rate: commission.parse().unwrap(),
        };
        let now = Utc::now();
        let target = TargetPortfolio {
            batch_id: uuid::Uuid::new_v4(),
            event_id: "test".to_owned(),
            strategy_id: StrategyId::new("s").unwrap(),
            account_id: AccountId::new("a").unwrap(),
            source_as_of: now,
            generated_at: now,
            valid_until: now + Duration::minutes(5),
            weights: vec![TargetWeight {
                instrument_id: id.clone(),
                weight: weight.parse().unwrap(),
            }],
        };
        let plan = TargetPlanner
            .plan(
                &target,
                &PlanningContext {
                    portfolio: tinkoff_rs::domain::PortfolioSnapshot {
                        account_id: AccountId::new("a").unwrap(),
                        total: None,
                        positions: vec![tinkoff_rs::domain::PortfolioPosition {
                            instrument_id: id.clone(),
                            quantity: Decimal::from(held),
                            current_price: None,
                            blocked: false,
                        }],
                        observed_at: now,
                    },
                    instruments: vec![info],
                    prices: Map::from([(id, Price(Decimal::from(1000)))]),
                    risk: risk_decision(Decimal::from(1_000_000)),
                    min_trade_fraction: band.parse().unwrap(),
                },
            )
            .unwrap();
        plan.intents
            .last()
            .map_or(held, |intent| intent.target_lots)
    }

    #[test]
    fn small_changes_are_not_traded_and_purchases_keep_a_commission_reserve() {
        // 1 000 000 ₽ capital, a 1 000 ₽ lot, commission 0.05%.
        // Target 50%: 500 lots without the reserve, 499 with it (500 × 1000.5 > 500 000).
        assert_eq!(probe(0, "0.5", "0.0005", "0"), 499);
        // Held 495 lots: the change of 4 lots (4 000 ₽) is below the 0.5% band (5 000 ₽).
        assert_eq!(probe(495, "0.5", "0.0005", "0.005"), 495);
        // Held 400 lots: the change of 99 lots is traded.
        assert_eq!(probe(400, "0.5", "0.0005", "0.005"), 499);
        // Closing and reversing are always traded; a dust position is not opened.
        assert_eq!(probe(3, "0", "0.0005", "0.005"), 0);
        assert_eq!(probe(3, "-0.002", "0", "0.005"), -2);
        assert_eq!(probe(0, "0.003", "0", "0.005"), 0);
    }

    #[test]
    fn the_planner_sizes_futures_by_point_value_and_bonds_by_nominal() {
        let info = |id: &str, kind, multiplier: Decimal, accrued: Decimal| InstrumentInfo {
            instrument_id: InstrumentId::new(id).unwrap(),
            lot_size: 1,
            currency: "rub".to_owned(),
            exchange: "moex".to_owned(),
            short_enabled: true,
            api_trade_available: true,
            market_order_available: true,
            kind,
            price_multiplier: multiplier,
            accrued_interest: accrued,
            commission_rate: Decimal::ZERO,
        };
        // Brent at 65.00 USD with a 0.01 step worth 7.50 ₽: one contract is 48 750 ₽.
        let brent = info(
            "futures:BR",
            InstrumentKind::Futures,
            Decimal::new(750, 0),
            Decimal::ZERO,
        );
        // An OFZ at 98.5% of 1000 ₽ with 12.34 ₽ accrued interest: 997.34 ₽ a bond.
        let ofz = info(
            "ofz",
            InstrumentKind::Bond,
            Decimal::TEN,
            Decimal::new(1234, 2),
        );
        assert_eq!(
            brent.piece_value(Decimal::new(65, 0)),
            Decimal::new(48_750, 0)
        );
        assert_eq!(
            ofz.piece_value(Decimal::new(985, 1)),
            Decimal::new(99_734, 2)
        );
        let now = Utc::now();
        let target = TargetPortfolio {
            batch_id: uuid::Uuid::new_v4(),
            event_id: "test".to_owned(),
            strategy_id: StrategyId::new("s").unwrap(),
            account_id: AccountId::new("a").unwrap(),
            source_as_of: now,
            generated_at: now,
            valid_until: now + Duration::minutes(5),
            weights: vec![
                TargetWeight {
                    instrument_id: brent.instrument_id.clone(),
                    weight: Decimal::new(5, 1),
                },
                TargetWeight {
                    instrument_id: ofz.instrument_id.clone(),
                    weight: Decimal::new(4, 1),
                },
            ],
        };
        let capital = Decimal::new(1_000_000, 0);
        let config: RiskConfig = toml::from_str("budget_rub = \"1000000\"").unwrap();
        let risk = RiskManager::new(
            config,
            RiskAnchor {
                account_id: AccountId::new("a").unwrap(),
                strategy_id: StrategyId::new("s").unwrap(),
                account_equity: capital,
                session_date: now.date_naive(),
                observed_at: now,
            },
        )
        .unwrap()
        .observe(&RiskObservation {
            current_equity: capital,
            session_date: now.date_naive(),
            observed_at: now,
            external_cash_flows: Decimal::ZERO,
            external_cash_flows_recognized: true,
            portfolio_is_flat: true,
        })
        .unwrap();
        assert_eq!(risk.mode, RiskMode::Normal);
        let plan = TargetPlanner
            .plan(
                &target,
                &PlanningContext {
                    portfolio: tinkoff_rs::domain::PortfolioSnapshot {
                        account_id: AccountId::new("a").unwrap(),
                        total: None,
                        positions: Vec::new(),
                        observed_at: now,
                    },
                    instruments: vec![brent, ofz],
                    prices: Map::from([
                        (
                            InstrumentId::new("futures:BR").unwrap(),
                            Price(Decimal::new(65, 0)),
                        ),
                        (
                            InstrumentId::new("ofz").unwrap(),
                            Price(Decimal::new(985, 1)),
                        ),
                    ]),
                    risk,
                    min_trade_fraction: Decimal::ZERO,
                },
            )
            .unwrap();
        let lots: Map<_, _> = plan
            .intents
            .iter()
            .map(|i| (i.instrument_id.to_string(), i.lots))
            .collect();
        // 500 000 ₽ / 48 750 ₽ → 10 contracts; 400 000 ₽ / 997.34 ₽ → 401 bonds.
        assert_eq!((lots["futures:BR"], lots["ofz"]), (10, 401));
    }
}
