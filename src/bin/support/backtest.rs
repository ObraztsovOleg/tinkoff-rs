//! Проверка ценовых стратегий на истории (`tinkoff-bot backtest`).
//!
//! Используются те же стратегии конфига, тот же planner лотов (порог сделки,
//! резерв комиссии) и тот же risk manager (дневной стоп, стоп по просадке), что и
//! в торговле. Решение исполняется по цене открытия ближайшей внутридневной свечи,
//! а при одних дневных свечах — по цене открытия дня (решение в первые полчаса
//! сессии) или закрытия; цена ухудшается на проскальзывание, с объёма сделки
//! берётся комиссия контура. Фьючерсы учитываются вариационной маржой: переоценка
//! сразу переходит в деньги, при сделке платится только комиссия.
//!
//! Брокер не отдаёт расписание прошедших дней, поэтому торговые дни — те, за
//! которые есть дневные свечи инструментов, а время сессии фиксировано (UTC):
//! открытие 07:00, аукцион закрытия 15:40, конец основной сессии 15:50. Стратегии
//! на отчётности, дивидендах и решениях ИИ требуют исторических данных, которых у
//! брокера нет, и в бэктест не допускаются.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::Serialize;
use tinkoff_rs::{
    Error, Result,
    domain::{
        AccountId, Candle, CandleHistoryRequest, CandleInterval, InstrumentId, Money, OrderSide,
        PortfolioPosition, PortfolioSnapshot, Price, StrategyId,
    },
    engine::{
        EventPhase, InstrumentInfo, InstrumentKind, PlanningContext, PortfolioStrategy, RiskAnchor,
        RiskConfig, RiskManager, RiskMode, RiskObservation, ScheduleAnchor, ScheduleEvent,
        StrategyEvent, StrategyEventKind, TargetPlanner,
    },
    strategies::WeightedEnsembleStrategy,
};

use crate::{
    BotConfig, RuntimeIds, history, inspect_instruments,
    instruments::ExecutionCosts,
    share_only_instruments, strategy_kinds, strategy_requires_short,
    venue::{self, Contour},
    warmup::{candle_duration, candle_interval_key, warmup_span},
};

/// Торговых сессий в году для коротких прогонов; на периоде от месяца берётся
/// фактическое число сессий (MOEX торгует и в часть выходных).
const SESSIONS_PER_YEAR: f64 = 252.0;
/// Решение в первые столько минут сессии исполняется по цене открытия дня.
const OPENING_MINUTES: i64 = 30;
/// Комиссия по умолчанию без базы и без `--commission-percent` (тариф «Трейдер»).
const DEFAULT_COMMISSION: Decimal = Decimal::from_parts(5, 0, 0, false, 4);

/// Время сессии MOEX в UTC для дней истории.
fn session_time(anchor: ScheduleAnchor) -> NaiveTime {
    let (hour, minute) = match anchor {
        ScheduleAnchor::SessionOpen => (7, 0),
        ScheduleAnchor::ClosingAuctionStart => (15, 40),
        ScheduleAnchor::SessionClose => (15, 50),
    };
    NaiveTime::from_hms_opt(hour, minute, 0).unwrap_or(NaiveTime::MIN)
}

fn at(day: NaiveDate, anchor: ScheduleAnchor) -> DateTime<Utc> {
    day.and_time(session_time(anchor)).and_utc()
}

/// Параметры прогона.
#[derive(Clone, Debug)]
pub(crate) struct Settings {
    /// Первый и последний день торговли (дни раньше — только прогрев).
    pub from: NaiveDate,
    pub to: NaiveDate,
    /// Деньги на счёте в начале.
    pub capital: Decimal,
    /// Ухудшение цены исполнения, доля (0.0005 = 5 б.п.).
    pub slippage: Decimal,
    /// Порог сделки, доля капитала стратегии.
    pub min_trade_fraction: Decimal,
    /// Бюджет и стопы из конфига.
    pub risk: RiskConfig,
}

/// Итог прогона.
#[derive(Debug, Serialize)]
pub(crate) struct Report {
    pub kinds: Vec<&'static str>,
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub sessions: usize,
    #[serde(with = "rust_decimal::serde::str")]
    pub capital: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub final_equity: Decimal,
    pub return_percent: f64,
    pub annual_return_percent: Option<f64>,
    pub volatility_percent: Option<f64>,
    pub sharpe: Option<f64>,
    pub sortino: Option<f64>,
    pub max_drawdown_percent: f64,
    pub worst_day_percent: Option<f64>,
    /// Решения стратегии и сколько из них не исполнено (нет цены, отказ planner-а).
    pub decisions: usize,
    pub skipped_decisions: usize,
    pub skip_reasons: BTreeMap<String, usize>,
    pub trades: usize,
    #[serde(with = "rust_decimal::serde::str")]
    pub turnover_rub: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub commission_rub: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub slippage_rub: Decimal,
    /// Сессии, закрытые risk manager-ом в режиме «только закрытие».
    pub risk_blocked_sessions: usize,
    pub benchmark: Option<Benchmark>,
    /// Капитал на закрытии каждой сессии (CSV `--curve`).
    #[serde(skip)]
    pub curve: Vec<(NaiveDate, Decimal)>,
    /// Не больше 250 точек кривой капитала для графика дашборда.
    pub curve_sample: Vec<(NaiveDate, f64)>,
}

/// Купить и держать ориентир за тот же период.
#[derive(Debug, Serialize)]
pub(crate) struct Benchmark {
    pub instrument_id: String,
    pub return_percent: f64,
    pub excess_percent: f64,
}

/// Свечи, по которым определяются цены исполнения и оценки.
struct Prices {
    daily: HashMap<InstrumentId, BTreeMap<NaiveDate, Candle>>,
    intraday: HashMap<InstrumentId, BTreeMap<DateTime<Utc>, Candle>>,
}

const fn is_intraday(interval: CandleInterval) -> bool {
    !matches!(
        interval,
        CandleInterval::OneDay | CandleInterval::OneWeek | CandleInterval::OneMonth
    )
}

impl Prices {
    fn new(candles: &[Candle]) -> Self {
        let mut prices = Self {
            daily: HashMap::new(),
            intraday: HashMap::new(),
        };
        for candle in candles.iter().filter(|c| c.complete) {
            if candle.interval == CandleInterval::OneDay {
                prices
                    .daily
                    .entry(candle.instrument_id.clone())
                    .or_default()
                    .insert(candle.time.date_naive(), candle.clone());
            } else if is_intraday(candle.interval) {
                prices
                    .intraday
                    .entry(candle.instrument_id.clone())
                    .or_default()
                    .insert(candle.time, candle.clone());
            }
        }
        prices
    }

    /// Дни, за которые есть дневные свечи хотя бы одного инструмента.
    fn trading_days(&self, from: NaiveDate, to: NaiveDate) -> BTreeSet<NaiveDate> {
        self.daily
            .values()
            .flat_map(|days| days.range(from..=to).map(|(day, _)| *day))
            .collect()
    }

    /// Цена исполнения решения в момент `moment`.
    fn execution(
        &self,
        id: &InstrumentId,
        moment: DateTime<Utc>,
        opening: bool,
    ) -> Option<Decimal> {
        let intraday = self.intraday.get(id).and_then(|bars| {
            bars.range(moment..)
                .next()
                .filter(|(time, _)| time.date_naive() == moment.date_naive())
                .map(|(_, candle)| candle.open.0)
        });
        let daily = || {
            self.daily
                .get(id)
                .and_then(|days| days.get(&moment.date_naive()))
                .map(|candle| {
                    if opening {
                        candle.open.0
                    } else {
                        candle.close.0
                    }
                })
        };
        intraday.or_else(daily).or_else(|| self.mark(id, moment))
    }

    /// Последняя известная к `moment` цена закрытия.
    fn mark(&self, id: &InstrumentId, moment: DateTime<Utc>) -> Option<Decimal> {
        let intraday = self.intraday.get(id).and_then(|bars| {
            bars.range(..=moment)
                .rev()
                .find(|(time, candle)| **time + candle_duration(candle.interval) <= moment)
                .map(|(time, candle)| (*time + candle_duration(candle.interval), candle.close.0))
        });
        let daily = self.daily.get(id).and_then(|days| {
            days.range(..=moment.date_naive())
                .rev()
                .find(|(day, _)| at(**day, ScheduleAnchor::SessionClose) <= moment)
                .map(|(day, candle)| (at(*day, ScheduleAnchor::SessionClose), candle.close.0))
        });
        match (intraday, daily) {
            (Some(a), Some(b)) => Some(if a.0 >= b.0 { a.1 } else { b.1 }),
            (a, b) => a.or(b).map(|(_, price)| price),
        }
    }
}

/// Позиция в бумагах (контрактах); у фьючерса `settled` — цена последнего расчёта маржи.
#[derive(Clone, Copy, Debug, Default)]
struct Holding {
    quantity: Decimal,
    settled: Decimal,
}

/// Издержки исполнения за прогон.
#[derive(Debug, Default)]
struct Costs {
    trades: usize,
    turnover: Decimal,
    commission: Decimal,
    slippage: Decimal,
}

/// Счёт прогона: деньги и позиции.
struct Book {
    cash: Decimal,
    holdings: HashMap<InstrumentId, Holding>,
}

impl Book {
    fn value(info: &InstrumentInfo, holding: Holding, price: Decimal) -> Decimal {
        if info.kind == InstrumentKind::Futures {
            holding.quantity * (price - holding.settled) * info.price_multiplier
        } else {
            holding.quantity * info.piece_value(price)
        }
    }

    fn equity(
        &self,
        infos: &HashMap<InstrumentId, InstrumentInfo>,
        prices: &HashMap<InstrumentId, Decimal>,
    ) -> Option<Decimal> {
        let mut total = self.cash;
        for (id, holding) in &self.holdings {
            if holding.quantity.is_zero() {
                continue;
            }
            total += Self::value(infos.get(id)?, *holding, *prices.get(id)?);
        }
        Some(total)
    }

    fn is_flat(&self) -> bool {
        self.holdings.values().all(|h| h.quantity.is_zero())
    }

    /// Вариационная маржа фьючерсов: переоценка переходит в деньги.
    fn settle(
        &mut self,
        infos: &HashMap<InstrumentId, InstrumentInfo>,
        prices: &HashMap<InstrumentId, Decimal>,
    ) {
        for (id, holding) in &mut self.holdings {
            if let (Some(info), Some(price)) = (infos.get(id), prices.get(id)) {
                if info.kind == InstrumentKind::Futures {
                    self.cash +=
                        holding.quantity * (price - holding.settled) * info.price_multiplier;
                    holding.settled = *price;
                }
            }
        }
    }

    /// Сделка `pieces` бумаг по цене `price`, хуже на `slippage`.
    fn trade(
        &mut self,
        info: &InstrumentInfo,
        side: OrderSide,
        pieces: Decimal,
        price: Decimal,
        slippage: Decimal,
        costs: &mut Costs,
    ) {
        let (signed, executed) = match side {
            OrderSide::Buy => (pieces, price * (Decimal::ONE + slippage)),
            OrderSide::Sell => (-pieces, price * (Decimal::ONE - slippage)),
        };
        let holding = self.holdings.entry(info.instrument_id.clone()).or_default();
        let notional = if info.kind == InstrumentKind::Futures {
            // Сначала маржа до цены сделки, затем новая позиция от неё.
            self.cash += holding.quantity * (executed - holding.settled) * info.price_multiplier;
            holding.settled = executed;
            pieces * executed * info.price_multiplier
        } else {
            self.cash -= signed * info.piece_value(executed);
            pieces * info.piece_value(executed)
        };
        holding.quantity += signed;
        let commission = notional * info.commission_rate;
        self.cash -= commission;
        costs.trades += 1;
        costs.turnover += notional;
        costs.commission += commission;
        costs.slippage += pieces * (executed - price).abs() * info.price_multiplier;
    }

    fn snapshot(
        &self,
        account_id: &AccountId,
        equity: Decimal,
        prices: &HashMap<InstrumentId, Decimal>,
        moment: DateTime<Utc>,
    ) -> PortfolioSnapshot {
        let rub = |value: Decimal| Money {
            currency: "rub".to_owned(),
            amount: Price(value),
        };
        PortfolioSnapshot {
            account_id: account_id.clone(),
            total: Some(rub(equity)),
            positions: self
                .holdings
                .iter()
                .filter(|(_, h)| !h.quantity.is_zero())
                .map(|(id, h)| PortfolioPosition {
                    instrument_id: id.clone(),
                    quantity: h.quantity,
                    current_price: prices.get(id).copied().map(rub),
                    blocked: false,
                })
                .collect(),
            observed_at: moment,
        }
    }
}

/// Шаг прогона: событие стратегии либо оценка счёта на закрытии сессии.
enum Step {
    Event(StrategyEvent),
    Close(NaiveDate),
}

/// Момент исполнения решения, принятого в `moment`: внутри сессии — сразу, иначе
/// на открытии ближайшего торгового дня. `true` — исполнять по цене открытия.
fn execution_moment(
    moment: DateTime<Utc>,
    days: &BTreeSet<NaiveDate>,
) -> Option<(DateTime<Utc>, NaiveDate, bool)> {
    let day = moment.date_naive();
    if days.contains(&day) {
        let open = at(day, ScheduleAnchor::SessionOpen);
        if moment <= open {
            return Some((open, day, true));
        }
        if moment <= at(day, ScheduleAnchor::SessionClose) {
            return Some((
                moment,
                day,
                moment < open + Duration::minutes(OPENING_MINUTES),
            ));
        }
    }
    let next = *days.range(day.succ_opt()?..).next()?;
    Some((at(next, ScheduleAnchor::SessionOpen), next, true))
}

fn candle_step(candle: Candle, start: DateTime<Utc>) -> (DateTime<Utc>, u8, Step) {
    let occurred_at = candle.time + candle_duration(candle.interval);
    let event = StrategyEvent {
        id: format!(
            "candle:{}:{}:{}",
            candle.instrument_id,
            candle_interval_key(candle.interval),
            candle.time.to_rfc3339()
        ),
        phase: if occurred_at < start {
            EventPhase::Warmup
        } else {
            EventPhase::Live
        },
        occurred_at,
        payload: StrategyEventKind::Candle(candle),
    };
    (occurred_at, 0, Step::Event(event))
}

/// Прогон стратегии по свечам: `strategy_candles` подаются стратегии, по
/// `price_candles` (включая дневные свечи всех инструментов) считаются цены.
pub(crate) async fn simulate(
    strategy: &mut dyn PortfolioStrategy,
    instruments: &[InstrumentInfo],
    strategy_candles: Vec<Candle>,
    price_candles: &[Candle],
    settings: &Settings,
) -> Result<Report> {
    let prices = Prices::new(price_candles);
    let days = prices.trading_days(settings.from, settings.to);
    let first = *days.first().ok_or_else(|| {
        Error::Configuration(format!(
            "no daily candles between {} and {}",
            settings.from, settings.to
        ))
    })?;
    let start = first.and_time(NaiveTime::MIN).and_utc();
    let account_id = strategy.account_id().clone();
    let infos: HashMap<_, _> = instruments
        .iter()
        .map(|info| (info.instrument_id.clone(), info.clone()))
        .collect();

    let mut steps: Vec<(DateTime<Utc>, u8, Step)> = strategy_candles
        .into_iter()
        .filter(|c| {
            c.complete
                && c.time + candle_duration(c.interval)
                    <= at(settings.to, ScheduleAnchor::SessionClose) + Duration::days(1)
        })
        .map(|c| candle_step(c, start))
        .collect();
    for day in &days {
        for requirement in strategy.schedule_requirements() {
            let scheduled_at =
                at(*day, requirement.anchor) + Duration::seconds(requirement.offset_seconds);
            let event = StrategyEvent {
                id: format!(
                    "schedule:{:?}:{}:{day}",
                    requirement.anchor, requirement.offset_seconds
                ),
                phase: EventPhase::Live,
                occurred_at: scheduled_at,
                payload: StrategyEventKind::Schedule(ScheduleEvent {
                    anchor: requirement.anchor,
                    session_date: *day,
                    scheduled_at,
                }),
            };
            steps.push((scheduled_at, 1, Step::Event(event)));
        }
        steps.push((
            at(*day, ScheduleAnchor::SessionClose) + Duration::seconds(1),
            2,
            Step::Close(*day),
        ));
    }
    steps.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    let mut risk = RiskManager::new(
        settings.risk.clone(),
        RiskAnchor {
            account_id: account_id.clone(),
            strategy_id: strategy.id().clone(),
            account_equity: settings.capital,
            session_date: first,
            observed_at: start,
        },
    )?;
    let mut book = Book {
        cash: settings.capital,
        holdings: HashMap::new(),
    };
    let mut costs = Costs::default();
    let (mut decisions, mut blocked) = (0, 0);
    // Причина неисполненного решения → сколько раз.
    let mut skipped: BTreeMap<String, usize> = BTreeMap::new();
    let mut curve = Vec::new();
    let universe: Vec<InstrumentId> = instruments
        .iter()
        .map(|i| i.instrument_id.clone())
        .collect();
    let quote = |moment: DateTime<Utc>, opening: bool| -> HashMap<InstrumentId, Decimal> {
        universe
            .iter()
            .filter_map(|id| Some((id.clone(), prices.execution(id, moment, opening)?)))
            .collect()
    };

    for (_, _, step) in steps {
        match step {
            Step::Close(day) => {
                let moment = at(day, ScheduleAnchor::SessionClose) + Duration::seconds(1);
                let marks: HashMap<_, _> = universe
                    .iter()
                    .filter_map(|id| Some((id.clone(), prices.mark(id, moment)?)))
                    .collect();
                let Some(equity) = book.equity(&infos, &marks) else {
                    continue;
                };
                book.settle(&infos, &marks);
                let decision = risk.observe(&RiskObservation {
                    current_equity: equity.max(Decimal::ZERO),
                    session_date: day,
                    observed_at: moment,
                    external_cash_flows: Decimal::ZERO,
                    external_cash_flows_recognized: true,
                    portfolio_is_flat: book.is_flat(),
                })?;
                if decision.mode == RiskMode::CloseOnly {
                    blocked += 1;
                }
                curve.push((day, equity));
            }
            Step::Event(event) => {
                let live = event.phase == EventPhase::Live;
                let occurred_at = event.occurred_at;
                let Some(mut target) = strategy.on_event(&event).await? else {
                    continue;
                };
                if !live {
                    continue;
                }
                decisions += 1;
                let Some((moment, day, opening)) = execution_moment(occurred_at, &days) else {
                    *skipped
                        .entry("решение после конца периода".to_owned())
                        .or_default() += 1;
                    continue;
                };
                let quotes = quote(moment, opening);
                let Some(equity) = book.equity(&infos, &quotes) else {
                    *skipped.entry("нет цены позиции".to_owned()).or_default() += 1;
                    continue;
                };
                let decision = risk.observe(&RiskObservation {
                    current_equity: equity.max(Decimal::ZERO),
                    session_date: day,
                    observed_at: moment,
                    external_cash_flows: Decimal::ZERO,
                    external_cash_flows_recognized: true,
                    portfolio_is_flat: book.is_flat(),
                })?;
                // Срок решения сверяется planner-ом с часами компьютера; время
                // истории уже проверено порядком событий.
                target.valid_until = Utc::now() + Duration::hours(1);
                let context = PlanningContext {
                    portfolio: book.snapshot(&account_id, equity, &quotes, moment),
                    instruments: instruments.to_vec(),
                    prices: quotes
                        .iter()
                        .map(|(id, p)| (id.clone(), Price(*p)))
                        .collect(),
                    risk: decision,
                    min_trade_fraction: settings.min_trade_fraction,
                };
                let planned = match TargetPlanner.plan(&target, &context) {
                    Ok(planned) => planned,
                    Err(error) => {
                        let reason: String = error.to_string().chars().take(160).collect();
                        *skipped.entry(reason).or_default() += 1;
                        continue;
                    }
                };
                for intent in planned.intents {
                    let info = &infos[&intent.instrument_id];
                    let pieces = Decimal::from(intent.lots) * Decimal::from(info.lot_size);
                    book.trade(
                        info,
                        intent.side,
                        pieces,
                        quotes[&intent.instrument_id],
                        settings.slippage,
                        &mut costs,
                    );
                }
            }
        }
    }
    let mut report = metrics(&curve, settings);
    report.sessions = curve.len();
    report.decisions = decisions;
    report.skipped_decisions = skipped.values().sum();
    report.skip_reasons = skipped;
    report.trades = costs.trades;
    report.turnover_rub = costs.turnover.round_dp(2);
    report.commission_rub = costs.commission.round_dp(2);
    report.slippage_rub = costs.slippage.round_dp(2);
    report.risk_blocked_sessions = blocked;
    let step = curve.len().div_ceil(250).max(1);
    report.curve_sample = curve
        .iter()
        .enumerate()
        .filter(|(index, _)| index % step == 0 || index + 1 == curve.len())
        .map(|(_, (day, equity))| (*day, equity.round_dp(2).to_f64().unwrap_or_default()))
        .collect();
    report.curve = curve;
    Ok(report)
}

/// Доходность, годовая волатильность, Шарп и Сортино (без безрисковой ставки),
/// максимальная просадка и худший день по капиталу на закрытии сессий.
fn metrics(curve: &[(NaiveDate, Decimal)], settings: &Settings) -> Report {
    let capital = settings.capital.to_f64().unwrap_or_default();
    let values: Vec<f64> = curve
        .iter()
        .map(|(_, e)| e.to_f64().unwrap_or_default())
        .collect();
    let final_equity = curve.last().map_or(settings.capital, |(_, e)| *e);
    let mut returns = Vec::new();
    let (mut previous, mut peak, mut drawdown) = (capital, capital, 0.0_f64);
    for value in &values {
        if previous > 0.0 {
            returns.push(value / previous - 1.0);
        }
        peak = peak.max(*value);
        if peak > 0.0 {
            drawdown = drawdown.max(1.0 - value / peak);
        }
        previous = *value;
    }
    let total = final_equity.to_f64().unwrap_or_default() / capital - 1.0;
    let calendar_days = curve
        .first()
        .zip(curve.last())
        .map(|((a, _), (b, _))| (*b - *a).num_days() + 1)
        .unwrap_or_default();
    let mut report = Report {
        kinds: Vec::new(),
        from: settings.from,
        to: settings.to,
        sessions: 0,
        capital: settings.capital,
        final_equity: final_equity.round_dp(2),
        return_percent: total * 100.0,
        annual_return_percent: (calendar_days >= 30).then(|| {
            ((1.0 + total)
                .powf(365.0 / f64::from(i32::try_from(calendar_days).unwrap_or(i32::MAX)))
                - 1.0)
                * 100.0
        }),
        volatility_percent: None,
        sharpe: None,
        sortino: None,
        max_drawdown_percent: drawdown * 100.0,
        worst_day_percent: returns.iter().copied().reduce(f64::min).map(|r| r * 100.0),
        decisions: 0,
        skipped_decisions: 0,
        skip_reasons: BTreeMap::new(),
        trades: 0,
        turnover_rub: Decimal::ZERO,
        commission_rub: Decimal::ZERO,
        slippage_rub: Decimal::ZERO,
        risk_blocked_sessions: 0,
        benchmark: None,
        curve: Vec::new(),
        curve_sample: Vec::new(),
    };
    if returns.len() >= 5 {
        let n = f64::from(u32::try_from(returns.len()).unwrap_or(u32::MAX));
        let mean = returns.iter().sum::<f64>() / n;
        let deviation =
            (returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt();
        let downside = (returns.iter().map(|r| r.min(0.0).powi(2)).sum::<f64>() / n).sqrt();
        let per_year = if calendar_days >= 30 {
            n * 365.0 / f64::from(i32::try_from(calendar_days).unwrap_or(i32::MAX))
        } else {
            SESSIONS_PER_YEAR
        };
        let year = per_year.sqrt();
        report.volatility_percent = Some(deviation * year * 100.0);
        report.sharpe = (deviation > 0.0).then(|| mean / deviation * year);
        report.sortino = (downside > 0.0).then(|| mean / downside * year);
    }
    report
}

/// Параметры командной строки.
pub(crate) struct Options {
    pub from: NaiveDate,
    pub to: Option<NaiveDate>,
    pub slippage_bps: Decimal,
    pub commission_percent: Option<Decimal>,
    pub capital: Option<Decimal>,
    pub benchmark: Option<String>,
}

async fn candles(
    broker: &venue::VenueBroker,
    instrument_id: &InstrumentId,
    interval: CandleInterval,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<Candle>> {
    let mut received = history::load(
        broker,
        CandleHistoryRequest {
            instrument_id: instrument_id.clone(),
            interval,
            from,
            to,
            limit: None,
        },
        None,
    )
    .await?;
    received.retain(|c| c.complete);
    Ok(received)
}

/// Загружает историю через контур Sandbox (только чтение; с `DATABASE_URL` — из
/// общего кэша свечей) и прогоняет стратегии конфига.
pub(crate) async fn run(config: &BotConfig, ids: &RuntimeIds, options: Options) -> Result<Report> {
    let kinds = strategy_kinds(config);
    if kinds.contains(&"ai-model") {
        return Err(Error::Configuration(
            "backtest covers price strategies; ai-model decisions are not reproducible from history".to_owned(),
        ));
    }
    let mut strategy = WeightedEnsembleStrategy::new(
        StrategyId::new("backtest")?,
        AccountId::new("backtest")?,
        ids.universe.clone(),
        config.strategies.clone(),
    )?;
    if !strategy.reference_requirements().is_empty() {
        return Err(Error::Configuration(
            "backtest covers price strategies; strategies on reports and dividends need history the broker does not provide".to_owned(),
        ));
    }
    let to = options
        .to
        .unwrap_or_else(|| Utc::now().date_naive() - Duration::days(1));
    if options.from > to {
        return Err(Error::Configuration(
            "--from must not be after --to".to_owned(),
        ));
    }
    // История и справочник — только чтение через Sandbox, даже для конфига реального счёта.
    let (api, broker) = venue::connect(Contour::Sandbox, &config.app_name).await?;
    api.series()
        .load(api.instruments(), &ids.universe, Utc::now())
        .await?;
    let storage = match std::env::var("DATABASE_URL") {
        Ok(url) => Some(std::sync::Arc::new(
            tinkoff_rs::storage::PostgresStorage::connect(&url, 2).await?,
        )),
        Err(_) => None,
    };
    if let Some(storage) = &storage {
        broker.use_candle_cache(storage.clone());
    }
    let requirements = strategy.data_requirements();
    let mut instruments = inspect_instruments(
        &api,
        &broker,
        &ids.universe,
        &requirements,
        config
            .strategies
            .iter()
            .any(|c| strategy_requires_short(&c.strategy)),
        &share_only_instruments(config, ids),
    )
    .await?;
    let costs = match &storage {
        Some(storage) => ExecutionCosts::load(storage, config.contour.label()).await,
        None => ExecutionCosts::default(),
    };
    costs.apply(&mut instruments);
    for info in &mut instruments {
        if let Some(percent) = options.commission_percent {
            info.commission_rate = percent / Decimal::ONE_HUNDRED;
        } else if info.commission_rate.is_zero() {
            info.commission_rate = DEFAULT_COMMISSION;
        }
    }

    let start = options.from.and_time(NaiveTime::MIN).and_utc();
    let end = (to + Duration::days(1))
        .and_time(NaiveTime::MIN)
        .and_utc()
        .min(Utc::now());
    let mut strategy_candles = Vec::new();
    let mut price_candles = Vec::new();
    for requirement in &requirements {
        let from = start - warmup_span(requirement.interval, requirement.warmup_bars)?;
        let loaded = candles(
            &broker,
            &requirement.instrument_id,
            requirement.interval,
            from,
            end,
        )
        .await?;
        price_candles.extend(
            loaded
                .iter()
                .filter(|c| c.time >= start - Duration::days(7))
                .cloned(),
        );
        strategy_candles.extend(loaded);
    }
    // Дневные свечи каждого инструмента: торговые дни и оценка счёта.
    for id in &ids.universe {
        if !requirements
            .iter()
            .any(|r| &r.instrument_id == id && r.interval == CandleInterval::OneDay)
        {
            price_candles.extend(
                candles(
                    &broker,
                    id,
                    CandleInterval::OneDay,
                    start - Duration::days(7),
                    end,
                )
                .await?,
            );
        }
    }
    strategy_candles.sort_by(|a, b| {
        a.time
            .cmp(&b.time)
            .then_with(|| a.instrument_id.as_str().cmp(b.instrument_id.as_str()))
    });

    let budget = config.risk.budget_rub;
    let settings = Settings {
        from: options.from,
        to,
        capital: options.capital.unwrap_or(budget),
        slippage: options.slippage_bps / Decimal::from(10_000),
        min_trade_fraction: costs.min_trade_fraction,
        risk: config.risk.clone(),
    };
    let mut report = simulate(
        &mut strategy,
        &instruments,
        strategy_candles,
        &price_candles,
        &settings,
    )
    .await?;
    report.kinds = kinds;
    if let Some(uid) = options.benchmark {
        let id = InstrumentId::new(uid)?;
        let bars = candles(&broker, &id, CandleInterval::OneDay, start, end).await?;
        let first = report.curve.first().map(|(day, _)| *day);
        let open = bars
            .iter()
            .find(|c| Some(c.time.date_naive()) >= first)
            .map(|c| c.open.0);
        let close = bars.last().map(|c| c.close.0);
        if let (Some(open), Some(close)) = (open, close) {
            let percent = ((close / open - Decimal::ONE) * Decimal::ONE_HUNDRED)
                .to_f64()
                .unwrap_or_default();
            report.benchmark = Some(Benchmark {
                instrument_id: id.into_inner(),
                return_percent: percent,
                excess_percent: report.return_percent - percent,
            });
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::Value;
    use tinkoff_rs::engine::{
        DataRequirement, DataRole, ScheduleRequirement, TargetPortfolio, TargetWeight,
    };
    use uuid::Uuid;

    use super::*;

    /// Держит заданные веса: решение на открытии сессии — только первой либо каждой.
    struct Fixed {
        id: StrategyId,
        account: AccountId,
        weights: Vec<(InstrumentId, Decimal)>,
        every_session: bool,
        decided: bool,
    }

    #[async_trait]
    impl PortfolioStrategy for Fixed {
        fn id(&self) -> &StrategyId {
            &self.id
        }
        fn account_id(&self) -> &AccountId {
            &self.account
        }
        fn kind(&self) -> &'static str {
            "fixed"
        }
        fn config(&self) -> Result<Value> {
            Ok(Value::Null)
        }
        fn data_requirements(&self) -> Vec<DataRequirement> {
            self.weights
                .iter()
                .map(|(id, _)| DataRequirement {
                    instrument_id: id.clone(),
                    role: DataRole::Tradable,
                    interval: CandleInterval::OneDay,
                    warmup_bars: 1,
                    waiting_close: true,
                })
                .collect()
        }
        fn schedule_requirements(&self) -> Vec<ScheduleRequirement> {
            vec![ScheduleRequirement {
                anchor: ScheduleAnchor::SessionOpen,
                offset_seconds: 0,
            }]
        }
        fn state(&self) -> Result<Value> {
            Ok(Value::Null)
        }
        fn restore_state(&mut self, _: Value) -> Result<()> {
            Ok(())
        }
        async fn on_event(&mut self, event: &StrategyEvent) -> Result<Option<TargetPortfolio>> {
            if !matches!(event.payload, StrategyEventKind::Schedule(_))
                || (self.decided && !self.every_session)
            {
                return Ok(None);
            }
            self.decided = true;
            Ok(Some(TargetPortfolio {
                batch_id: Uuid::new_v4(),
                event_id: event.id.clone(),
                strategy_id: self.id.clone(),
                account_id: self.account.clone(),
                source_as_of: event.occurred_at,
                generated_at: event.occurred_at,
                valid_until: event.occurred_at + Duration::hours(1),
                weights: self
                    .weights
                    .iter()
                    .map(|(id, weight)| TargetWeight {
                        instrument_id: id.clone(),
                        weight: *weight,
                    })
                    .collect(),
            }))
        }
    }

    fn day(text: &str) -> NaiveDate {
        text.parse().unwrap()
    }

    fn bar(id: &InstrumentId, date: &str, open: i64, close: i64) -> Candle {
        Candle {
            instrument_id: id.clone(),
            interval: CandleInterval::OneDay,
            open: Price(Decimal::from(open)),
            high: Price(Decimal::from(open.max(close))),
            low: Price(Decimal::from(open.min(close))),
            close: Price(Decimal::from(close)),
            volume: 1,
            time: day(date).and_time(NaiveTime::MIN).and_utc(),
            complete: true,
        }
    }

    fn info(
        id: &InstrumentId,
        kind: InstrumentKind,
        multiplier: i64,
        commission: Decimal,
    ) -> InstrumentInfo {
        InstrumentInfo {
            instrument_id: id.clone(),
            lot_size: 1,
            currency: "rub".to_owned(),
            exchange: "MOEX".to_owned(),
            short_enabled: true,
            api_trade_available: true,
            market_order_available: true,
            kind,
            price_multiplier: Decimal::from(multiplier),
            accrued_interest: Decimal::ZERO,
            commission_rate: commission,
        }
    }

    fn settings(risk: &str, slippage: Decimal) -> Settings {
        Settings {
            from: day("2026-10-19"),
            to: day("2026-10-21"),
            capital: Decimal::from(10_000),
            slippage,
            min_trade_fraction: Decimal::ZERO,
            risk: toml::from_str(risk).unwrap(),
        }
    }

    async fn backtest(
        weights: Vec<(InstrumentId, Decimal)>,
        every_session: bool,
        instruments: &[InstrumentInfo],
        bars: &[Candle],
        settings: &Settings,
    ) -> Report {
        let mut strategy = Fixed {
            id: StrategyId::new("s").unwrap(),
            account: AccountId::new("a").unwrap(),
            weights,
            every_session,
            decided: false,
        };
        simulate(&mut strategy, instruments, bars.to_vec(), bars, settings)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_share_is_bought_at_the_open_and_valued_at_each_close_after_costs() {
        let sber = InstrumentId::new("sber").unwrap();
        let bars = [
            bar(&sber, "2026-10-16", 95, 100),
            bar(&sber, "2026-10-19", 100, 110),
            bar(&sber, "2026-10-20", 110, 121),
            bar(&sber, "2026-10-21", 121, 121),
        ];
        let instruments = [info(&sber, InstrumentKind::Share, 1, Decimal::new(1, 3))];
        let settings = settings("budget_rub = \"10000\"", Decimal::ZERO);
        let report = backtest(
            vec![(sber.clone(), Decimal::new(5, 1))],
            false,
            &instruments,
            &bars,
            &settings,
        )
        .await;
        // 5000 / (100 × 1.001) = 49 лотов: 4900 + 4.9 комиссии, дальше переоценка.
        assert_eq!(
            report.curve[0],
            (day("2026-10-19"), Decimal::new(104_851, 1))
        );
        assert_eq!(report.final_equity, Decimal::new(110_241, 1));
        assert_eq!(
            (report.trades, report.commission_rub),
            (1, Decimal::new(49, 1))
        );
        assert_eq!((report.sessions, report.decisions), (3, 1));
        assert!((report.return_percent - 10.241).abs() < 1e-9);
        // Те же лимиты, что в торговле: больше 50% в одном инструменте planner не исполняет.
        let report = backtest(
            vec![(sber.clone(), Decimal::ONE)],
            false,
            &instruments,
            &bars,
            &settings,
        )
        .await;
        assert_eq!((report.trades, report.skipped_decisions), (0, 1));
        assert!(
            report
                .skip_reasons
                .keys()
                .any(|reason| reason.contains("per-instrument exposure limit"))
        );
    }

    #[tokio::test]
    async fn slippage_makes_the_price_worse_in_the_direction_of_the_trade() {
        let sber = InstrumentId::new("sber").unwrap();
        let bars = [
            bar(&sber, "2026-10-19", 100, 100),
            bar(&sber, "2026-10-20", 100, 100),
        ];
        let report = backtest(
            vec![(sber.clone(), Decimal::new(5, 1))],
            false,
            &[info(&sber, InstrumentKind::Share, 1, Decimal::ZERO)],
            &bars,
            &settings("budget_rub = \"10000\"", Decimal::new(1, 2)),
        )
        .await;
        // 50 бумаг по 101 вместо 100: 50 ₽ проскальзывания.
        assert_eq!(report.slippage_rub, Decimal::from(50));
        assert_eq!(report.final_equity, Decimal::from(9_950));
    }

    #[tokio::test]
    async fn futures_pay_only_variation_margin_and_commission() {
        let br = InstrumentId::new("futures:BR").unwrap();
        let bars = [
            bar(&br, "2026-10-19", 60, 62),
            bar(&br, "2026-10-20", 62, 59),
            bar(&br, "2026-10-21", 59, 59),
        ];
        let report = backtest(
            vec![(br.clone(), Decimal::new(3, 1))],
            false,
            &[info(&br, InstrumentKind::Futures, 10, Decimal::ZERO)],
            &bars,
            &settings("budget_rub = \"10000\"", Decimal::ZERO),
        )
        .await;
        // 3000 / (60 × 10) = 5 контрактов: +2 пункта × 10 × 5, затем −3 × 10 × 5.
        assert_eq!(report.curve[0].1, Decimal::from(10_100));
        assert_eq!(report.curve[1].1, Decimal::from(9_950));
        assert_eq!(
            report.turnover_rub,
            Decimal::from(3_000),
            "the position is kept as is"
        );
    }

    #[tokio::test]
    async fn the_daily_loss_stop_closes_the_position_at_the_next_open() {
        let sber = InstrumentId::new("sber").unwrap();
        let bars = [
            bar(&sber, "2026-10-19", 100, 80),
            bar(&sber, "2026-10-20", 80, 90),
            bar(&sber, "2026-10-21", 90, 90),
        ];
        let report = backtest(
            vec![(sber.clone(), Decimal::new(5, 1))],
            true,
            &[info(&sber, InstrumentKind::Share, 1, Decimal::ZERO)],
            &bars,
            &settings(
                "budget_rub = \"10000\"\ndaily_loss_percent = \"5\"",
                Decimal::ZERO,
            ),
        )
        .await;
        // Убыток 10% за 19-е: стоп, продажа по открытию 20-го (80); 21-го счёт
        // пуст, блокировка снята — покупка заново по 90.
        assert_eq!(report.risk_blocked_sessions, 1);
        assert_eq!(report.curve[1].1, Decimal::from(9_000));
        assert_eq!(report.final_equity, Decimal::from(9_000));
        assert_eq!(report.trades, 3);
    }

    #[test]
    fn ratios_are_annualized_by_the_sessions_actually_traded() {
        // 60 сессий за 30 календарных дней: в году 730 сессий, а не 252.
        let start = day("2026-01-01");
        let curve: Vec<_> = (0..60)
            .map(|i| {
                let equity = if i % 2 == 0 { 10_100 } else { 10_000 };
                (start + Duration::days(i / 2), Decimal::from(equity))
            })
            .collect();
        let report = metrics(&curve, &settings("budget_rub = \"10000\"", Decimal::ZERO));
        let returns: Vec<f64> = std::iter::once(10_000.0)
            .chain(curve.iter().map(|(_, e)| e.to_f64().unwrap()))
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| w[1] / w[0] - 1.0)
            .collect();
        let n = 60.0;
        let mean = returns.iter().sum::<f64>() / n;
        let deviation =
            (returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt();
        let per_year: f64 = 60.0 * 365.0 / 30.0;
        assert!(
            (report.volatility_percent.unwrap() - deviation * per_year.sqrt() * 100.0).abs() < 1e-9
        );
    }

    #[test]
    fn decisions_outside_the_session_wait_for_the_next_open() {
        let days: BTreeSet<_> = [day("2026-10-23"), day("2026-10-26")].into();
        let moment = |text: &str| chrono::DateTime::parse_from_rfc3339(text).unwrap().to_utc();
        // Дневная свеча пятницы закрывается в полночь — исполнение в понедельник.
        assert_eq!(
            execution_moment(moment("2026-10-24T00:00:00Z"), &days),
            Some((moment("2026-10-26T07:00:00Z"), day("2026-10-26"), true))
        );
        assert_eq!(
            execution_moment(moment("2026-10-23T12:00:00Z"), &days),
            Some((moment("2026-10-23T12:00:00Z"), day("2026-10-23"), false))
        );
        assert_eq!(
            execution_moment(moment("2026-10-27T00:00:00Z"), &days),
            None
        );
    }
}
