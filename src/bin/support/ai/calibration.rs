//! Калибровка чувствительности бумаг к темам по фактическим реакциям.
//!
//! Для каждой темы день с размеченными событиями получает сигнал — сумму
//! «знак × сила» событий (важность от 3). Реакция бумаги в этот день — её
//! доходность сверх индекса Мосбиржи, а для общерыночных тем (геополитика,
//! санкции, ставка, мировые рынки, бюджет) — доходность целиком. Наклон
//! «реакция на единицу сигнала» с поправкой на малую выборку переводится в
//! шкалу −2…+2 и записывается предложением для оператора.
use std::collections::HashMap;

use chrono::{DateTime, Duration, NaiveDate, Timelike, Utc};
use serde_json::Value;
use tinkoff_rs::{
    Result,
    domain::{CandleHistoryRequest, CandleInterval, InstrumentId},
    storage::PostgresStorage,
};

use super::{indicators::number, market};
use crate::{VenueBroker, history};

/// Глубина измерения.
const WINDOW_DAYS: i64 = 120;
/// Меньше дней с событиями темы — предложения нет.
const MIN_OBSERVATIONS: usize = 8;
/// Пересчёт не чаще.
const EVERY: Duration = Duration::hours(20);
/// Темы, которые двигают весь рынок: реакция без вычета индекса.
const MARKET_WIDE: &[&str] = &[
    "geopolitics",
    "sanctions",
    "key_rate",
    "world_markets",
    "budget",
    "inflation",
];

/// Дневные доходности в процентах по закрытиям.
pub(crate) fn returns(daily: &[(NaiveDate, f64)]) -> HashMap<NaiveDate, f64> {
    daily
        .windows(2)
        .filter(|w| w[0].1 > 0.0)
        .map(|w| (w[1].0, (w[1].1 / w[0].1 - 1.0) * 100.0))
        .collect()
}

/// Торговый день события: опубликованное после закрытия основной сессии
/// (18:40 МСК) относится к следующему торговому дню.
pub(crate) fn event_day(published: DateTime<Utc>, days: &[NaiveDate]) -> Option<NaiveDate> {
    let moscow = published + Duration::hours(3);
    let late = moscow.hour() > 18 || (moscow.hour() == 18 && moscow.minute() >= 40);
    let date = moscow.date_naive();
    days.iter()
        .copied()
        .find(|day| if late { *day > date } else { *day >= date })
}

/// Наклон регрессии через ноль и корреляция.
pub(crate) fn fit(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    let sxx: f64 = points.iter().map(|(x, _)| x * x).sum();
    let syy: f64 = points.iter().map(|(_, y)| y * y).sum();
    let sxy: f64 = points.iter().map(|(x, y)| x * y).sum();
    (sxx > 0.0 && syy > 0.0).then(|| (sxy / sxx, sxy / (sxx.sqrt() * syy.sqrt())))
}

/// Эффект (% на единицу сигнала) в шкалу −2…+2 со сжатием к нулю при малой выборке.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn suggestion(slope: f64, observations: usize) -> f64 {
    let n = observations as f64;
    let shrunk = slope * n / (n + 10.0);
    let level = match shrunk.abs() {
        a if a < 0.15 => 0.0,
        a if a < 0.4 => 0.5,
        a if a < 0.8 => 1.0,
        a if a < 1.5 => 1.5,
        _ => 2.0,
    };
    level * shrunk.signum()
}

async fn daily(broker: &VenueBroker, uid: &str) -> Result<Vec<(NaiveDate, f64)>> {
    let now = Utc::now();
    let candles = history::load(
        broker,
        CandleHistoryRequest {
            instrument_id: InstrumentId::new(uid.to_owned())?,
            interval: CandleInterval::OneDay,
            from: now - Duration::days(WINDOW_DAYS + 10),
            to: now,
            limit: None,
        },
        None,
    )
    .await?;
    Ok(candles
        .iter()
        .filter(|c| c.complete)
        .map(|c| {
            (
                (c.time + Duration::hours(3)).date_naive(),
                number(c.close.0),
            )
        })
        .collect())
}

/// Пересчитывает предложения, если прошлый расчёт устарел. Возвращает число записей.
pub(crate) async fn calibrate(storage: &PostgresStorage, broker: &VenueBroker) -> Result<usize> {
    let last: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT max(computed_at) FROM exposure_suggestions")
            .fetch_one(storage.pool())
            .await?;
    if last.is_some_and(|at| Utc::now() - at < EVERY) {
        return Ok(0);
    }
    let instruments: Vec<(String, String)> = sqlx::query_as(
        "SELECT ticker, uid FROM instrument_profiles WHERE uid IS NOT NULL ORDER BY ticker",
    )
    .fetch_all(storage.pool())
    .await?;
    let events: Vec<(DateTime<Utc>, Value)> = sqlx::query_as(
        "SELECT published_at, topics FROM documents WHERE status = 'tagged' AND importance >= 3 \
           AND topics <> '[]' AND published_at >= now() - make_interval(days => $1)",
    )
    .bind(i32::try_from(WINDOW_DAYS).unwrap_or(i32::MAX))
    .fetch_all(storage.pool())
    .await?;
    if instruments.is_empty() || events.is_empty() {
        return Ok(0);
    }
    let index = daily(broker, market::IMOEX).await?;
    let days: Vec<NaiveDate> = index.iter().map(|(day, _)| *day).collect();
    let index_returns = returns(&index);
    // Сигнал темы по торговым дням.
    let mut signals: HashMap<String, HashMap<NaiveDate, f64>> = HashMap::new();
    for (published, topics) in &events {
        let Some(day) = event_day(*published, &days) else {
            continue;
        };
        for tag in topics.as_array().into_iter().flatten() {
            let (Some(topic), Some(direction)) = (tag["topic"].as_str(), tag["direction"].as_f64())
            else {
                continue;
            };
            let strength = tag["strength"].as_f64().unwrap_or(1.0);
            *signals
                .entry(topic.to_owned())
                .or_default()
                .entry(day)
                .or_default() += direction * strength;
        }
    }
    let mut written = 0;
    for (ticker, uid) in &instruments {
        let Ok(series) = daily(broker, uid).await else {
            continue;
        };
        let own = returns(&series);
        for (topic, by_day) in &signals {
            let wide = MARKET_WIDE.contains(&topic.as_str());
            let points: Vec<(f64, f64)> = by_day
                .iter()
                .filter(|(_, signal)| **signal != 0.0)
                .filter_map(|(day, signal)| {
                    let reaction =
                        own.get(day)? - if wide { 0.0 } else { *index_returns.get(day)? };
                    Some((*signal, reaction))
                })
                .collect();
            if points.len() < MIN_OBSERVATIONS {
                continue;
            }
            let Some((slope, correlation)) = fit(&points) else {
                continue;
            };
            sqlx::query(
                "INSERT INTO exposure_suggestions(ticker, topic, observations, effect_percent, correlation, suggested, computed_at) \
                 VALUES($1, $2, $3, $4::numeric, $5::numeric, $6::numeric, now()) ON CONFLICT (ticker, topic) DO UPDATE SET \
                 observations = EXCLUDED.observations, effect_percent = EXCLUDED.effect_percent, correlation = EXCLUDED.correlation, \
                 suggested = EXCLUDED.suggested, computed_at = now()",
            )
            .bind(ticker)
            .bind(topic)
            .bind(i32::try_from(points.len()).unwrap_or(i32::MAX))
            .bind(format!("{slope:.4}"))
            .bind(format!("{correlation:.4}"))
            .bind(format!("{:.1}", suggestion(slope, points.len())))
            .execute(storage.pool())
            .await?;
            written += 1;
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reactions_on_event_days_become_a_shrunk_sensitivity() {
        let day = |d: u32| NaiveDate::from_ymd_opt(2026, 10, d).unwrap();
        let days = vec![day(15), day(16), day(19), day(20)];
        let at = |d: u32, h: u32, m: u32| {
            DateTime::parse_from_rfc3339(&format!("2026-10-{d:02}T{h:02}:{m:02}:00+03:00"))
                .unwrap()
                .with_timezone(&Utc)
        };
        assert_eq!(event_day(at(16, 12, 0), &days), Some(day(16)));
        assert_eq!(
            event_day(at(16, 19, 0), &days),
            Some(day(19)),
            "after the close: the next trading day"
        );
        assert_eq!(
            event_day(at(17, 12, 0), &days),
            Some(day(19)),
            "a weekend event: Monday"
        );
        assert!((returns(&[(day(15), 100.0), (day(16), 102.0)])[&day(16)] - 2.0).abs() < 1e-9);
        // An oil producer gains ~1% above the index per unit of the oil signal.
        let points: Vec<(f64, f64)> = (0..12)
            .map(|i| {
                let x = if i % 2 == 0 { 2.0 } else { -1.0 };
                (x, x * 1.0 + if i % 3 == 0 { 0.2 } else { -0.1 })
            })
            .collect();
        let (slope, correlation) = fit(&points).unwrap();
        assert!(
            (slope - 1.0).abs() < 0.1 && correlation > 0.9,
            "{slope} {correlation}"
        );
        assert!(
            (suggestion(slope, points.len()) - 1.0).abs() < 1e-9,
            "12 days shrink 1% to 0.55: level 1"
        );
        assert!((suggestion(-3.0, 40) + 2.0).abs() < 1e-9);
        assert!(
            suggestion(0.1, 8).abs() < 1e-9,
            "a tiny effect suggests nothing"
        );
        assert!(fit(&[(0.0, 1.0)]).is_none());
    }
}
