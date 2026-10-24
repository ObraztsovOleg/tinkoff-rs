//! Прогрев стратегии историей: закрытые свечи до начала торговли и свечи,
//! закрывшиеся во время запуска. Свечи подаются в фазе `Warmup`, поэтому
//! стратегия наполняет состояние, но не может выставить заявку.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use tinkoff_rs::{
    Error, Result,
    domain::{Candle, CandleHistoryRequest, CandleInterval, InstrumentId, Price, StrategyId},
    engine::{DataRequirement, EventPhase, PortfolioEngine, StrategyEvent, StrategyEventKind},
    storage::PostgresStorage,
};
use tracing::info;

use crate::{history, venue::VenueBroker};

/// Загружает минимальную историю закрытых свечей и прогревает состояние стратегии.
///
/// Свечи сортируются глобально по времени, чтобы многомерные стратегии увидели их
/// в причинном порядке. `PlanningContext = None` и `EventPhase::Warmup` запрещают
/// планирование/исполнение заявок во время воспроизведения истории.
pub(super) async fn warm_up(
    engine: &PortfolioEngine<PostgresStorage>,
    broker: &VenueBroker,
    requirements: &[DataRequirement],
    cutoff: DateTime<Utc>,
    storage: &PostgresStorage,
    strategy_id: &StrategyId,
) -> Result<HashMap<InstrumentId, Price>> {
    // Each event marker and strategy state were committed atomically. Replaying
    // known warmup events after restore only serializes the full state repeatedly.
    let processed: HashSet<String> = sqlx::query_scalar::<_, String>(
        "SELECT event_id FROM processed_events WHERE strategy_id=$1 AND event_id LIKE 'candle:%'",
    )
    .bind(strategy_id.as_str())
    .fetch_all(storage.pool())
    .await?
    .into_iter()
    .collect();
    let mut candles = Vec::new();
    for requirement in requirements {
        if requirement.warmup_bars == 0 || requirement.warmup_bars > 2_500 {
            return Err(Error::Configuration(format!(
                "warmup for `{}` must contain 1..=2500 bars",
                requirement.instrument_id
            )));
        }
        let mut received = history::load(
            broker,
            CandleHistoryRequest {
                instrument_id: requirement.instrument_id.clone(),
                interval: requirement.interval,
                from: cutoff - warmup_span(requirement.interval, requirement.warmup_bars)?,
                to: cutoff,
                limit: None,
            },
            Some(requirement.warmup_bars),
        )
        .await?;
        received.retain(|candle| candle.complete);
        received.sort_by_key(|candle| candle.time);
        if received.len() < requirement.warmup_bars {
            return Err(Error::Configuration(format!(
                "instrument `{}` returned {} complete {:?} candles, but {} are required",
                requirement.instrument_id,
                received.len(),
                requirement.interval,
                requirement.warmup_bars
            )));
        }
        // API может вернуть небольшой запас. Стратегии нужны ровно последние N
        // завершённых баров, поэтому более старую часть отбрасываем.
        let keep_from = received.len() - requirement.warmup_bars;
        candles.extend(received.drain(keep_from..));
    }
    // При одинаковом timestamp UID задаёт стабильный порядок воспроизведения.
    candles.sort_by(|left, right| {
        left.time.cmp(&right.time).then_with(|| {
            left.instrument_id
                .as_str()
                .cmp(right.instrument_id.as_str())
        })
    });
    let mut latest_prices = HashMap::new();
    let mut events = Vec::new();
    for candle in candles {
        latest_prices.insert(candle.instrument_id.clone(), candle.close);
        let event = candle_event(candle, EventPhase::Warmup);
        if !processed.contains(&event.id) {
            events.push(event);
        }
    }
    let total = events.len();
    for (index, chunk) in events.chunks(250).enumerate() {
        if !engine.process_warmup_batch(chunk).await? {
            return Err(Error::Storage(
                "warmup checkpoint conflicted with another event writer".to_owned(),
            ));
        }
        info!(
            completed = ((index + 1) * 250).min(total),
            total, "warmup progress"
        );
    }
    Ok(latest_prices)
}

/// Дополняет warmup свечами, закрывшимися во время startup-последовательности.
///
/// Нижняя граница сдвинута на один интервал назад, потому что API индексирует
/// свечу временем открытия: бар мог открыться до `from`, но закрыться после него.
/// Повторы безопасны благодаря event ID, а `Warmup` не разрешает торговый target.
pub(super) async fn catch_up_warmup(
    engine: &PortfolioEngine<PostgresStorage>,
    broker: &VenueBroker,
    requirements: &[DataRequirement],
    from: DateTime<Utc>,
    latest_prices: &mut HashMap<InstrumentId, Price>,
) -> Result<()> {
    let to = Utc::now();
    let mut candles = Vec::new();
    for requirement in requirements {
        let mut received = history::load(
            broker,
            CandleHistoryRequest {
                instrument_id: requirement.instrument_id.clone(),
                interval: requirement.interval,
                from: from - candle_duration(requirement.interval),
                to,
                limit: None,
            },
            None,
        )
        .await?;
        received.retain(|candle| {
            candle.complete
                // `time` — начало бара. Для daily/monthly номинальная длина
                // интервала не совпадает с фактическим закрытием биржи, поэтому
                // authoritative флаг `complete` определяет верхнюю границу.
                // Условие ниже нужно лишь для короткого пересечения с cutoff;
                // лишний старый бар безвреден благодаря event-id дедупликации.
                && candle.time + candle_duration(candle.interval) > from
                && candle.time <= to
        });
        candles.extend(received);
    }
    candles.sort_by(|left, right| {
        left.time.cmp(&right.time).then_with(|| {
            left.instrument_id
                .as_str()
                .cmp(right.instrument_id.as_str())
        })
    });
    candles.dedup_by(|left, right| {
        left.time == right.time
            && left.interval == right.interval
            && left.instrument_id == right.instrument_id
    });
    for candle in candles {
        latest_prices.insert(candle.instrument_id.clone(), candle.close);
        engine
            .process_event(&candle_event(candle, EventPhase::Warmup), None)
            .await?;
    }
    Ok(())
}

/// Оценивает календарное окно, достаточное для получения `bars` торговых свечей.
///
/// Множители больше длины самого интервала оставляют запас на ночь, выходные и
/// праздники MOEX. Это только границы запроса; после ответа берутся ровно последние
/// завершённые бары, поэтому запас не меняет вычисляемый сигнал.
pub(super) fn warmup_span(interval: CandleInterval, bars: usize) -> Result<Duration> {
    let bars = i64::try_from(bars)
        .map_err(|_| Error::Configuration("warmup bar count is too large".to_owned()))?;
    let duration = match interval {
        CandleInterval::OneMonth => Duration::days(bars.saturating_mul(35)),
        CandleInterval::OneWeek => Duration::days(bars.saturating_mul(10)),
        CandleInterval::OneDay => Duration::days(bars.saturating_mul(3).saturating_add(14)),
        CandleInterval::FourHours => Duration::hours(bars.saturating_mul(16)),
        CandleInterval::TwoHours => Duration::hours(bars.saturating_mul(8)),
        CandleInterval::OneHour => Duration::hours(bars.saturating_mul(4)),
        CandleInterval::ThirtyMinutes => Duration::minutes(bars.saturating_mul(30 * 8)),
        CandleInterval::FifteenMinutes => Duration::minutes(bars.saturating_mul(15 * 8)),
        CandleInterval::TenMinutes => Duration::minutes(bars.saturating_mul(10 * 8)),
        CandleInterval::FiveMinutes => Duration::minutes(bars.saturating_mul(5 * 8)),
        CandleInterval::ThreeMinutes => Duration::minutes(bars.saturating_mul(3 * 8)),
        CandleInterval::TwoMinutes => Duration::minutes(bars.saturating_mul(2 * 8)),
        CandleInterval::OneMinute => Duration::minutes(bars.saturating_mul(8)),
        CandleInterval::ThirtySeconds => Duration::seconds(bars.saturating_mul(30 * 8)),
        CandleInterval::TenSeconds => Duration::seconds(bars.saturating_mul(10 * 8)),
        CandleInterval::FiveSeconds => Duration::seconds(bars.saturating_mul(5 * 8)),
    };
    Ok(duration)
}

/// Оборачивает свечу в идемпотентное событие стратегии.
///
/// `candle.time` обозначает начало интервала, поэтому `occurred_at` сдвигается к
/// номинальному закрытию. Дневная свеча может стать complete до `open + 24h`,
/// поэтому будущее номинальное время ограничивается фактом её получения.
pub(super) fn candle_event(candle: Candle, phase: EventPhase) -> StrategyEvent {
    let interval = candle_interval_key(candle.interval);
    let occurred_at = (candle.time + candle_duration(candle.interval)).min(Utc::now());
    StrategyEvent {
        id: format!(
            "candle:{}:{interval}:{}",
            candle.instrument_id,
            candle.time.to_rfc3339()
        ),
        phase,
        occurred_at,
        payload: StrategyEventKind::Candle(candle),
    }
}

/// Возвращает номинальную длительность свечи для вычисления времени её закрытия.
pub(super) const fn candle_duration(interval: CandleInterval) -> Duration {
    match interval {
        CandleInterval::FiveSeconds => Duration::seconds(5),
        CandleInterval::TenSeconds => Duration::seconds(10),
        CandleInterval::ThirtySeconds => Duration::seconds(30),
        CandleInterval::OneMinute => Duration::minutes(1),
        CandleInterval::TwoMinutes => Duration::minutes(2),
        CandleInterval::ThreeMinutes => Duration::minutes(3),
        CandleInterval::FiveMinutes => Duration::minutes(5),
        CandleInterval::TenMinutes => Duration::minutes(10),
        CandleInterval::FifteenMinutes => Duration::minutes(15),
        CandleInterval::ThirtyMinutes => Duration::minutes(30),
        CandleInterval::OneHour => Duration::hours(1),
        CandleInterval::TwoHours => Duration::hours(2),
        CandleInterval::FourHours => Duration::hours(4),
        CandleInterval::OneDay => Duration::days(1),
        CandleInterval::OneWeek => Duration::weeks(1),
        CandleInterval::OneMonth => Duration::days(31),
    }
}

/// Стабильное короткое имя интервала, входящее в идемпотентный event ID.
pub(super) const fn candle_interval_key(interval: CandleInterval) -> &'static str {
    match interval {
        CandleInterval::OneMinute => "1m",
        CandleInterval::FiveMinutes => "5m",
        CandleInterval::FifteenMinutes => "15m",
        CandleInterval::OneHour => "1h",
        CandleInterval::OneDay => "1d",
        CandleInterval::TwoMinutes => "2m",
        CandleInterval::ThreeMinutes => "3m",
        CandleInterval::TenMinutes => "10m",
        CandleInterval::ThirtyMinutes => "30m",
        CandleInterval::TwoHours => "2h",
        CandleInterval::FourHours => "4h",
        CandleInterval::OneWeek => "1w",
        CandleInterval::OneMonth => "1mo",
        CandleInterval::FiveSeconds => "5s",
        CandleInterval::TenSeconds => "10s",
        CandleInterval::ThirtySeconds => "30s",
    }
}
