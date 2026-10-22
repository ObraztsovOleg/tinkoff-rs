//! Итоги решений модели: доходность целевых долей решения и индекса Мосбиржи
//! через 1, 5 и 20 торговых дней. Решение «держать» оценивается по долям
//! предыдущего решения с весами. Доходность решения — после комиссии за
//! перекладку: оборот (сумма модулей изменений долей) × ставка контура.
use std::collections::HashMap;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde_json::Value;
use tinkoff_rs::{
    Result,
    domain::{CandleHistoryRequest, CandleInterval, InstrumentId},
    storage::PostgresStorage,
};
use uuid::Uuid;

use super::{indicators::number, market};
use crate::{VenueBroker, history};

pub(crate) const HORIZONS: [i64; 3] = [1, 5, 20];
/// Оцениваются решения не старше.
const WINDOW_DAYS: i64 = 60;

/// Доли решения: UID → доля капитала (0.2 = 20%).
pub(crate) fn weights_of(value: &Value) -> Vec<(String, f64)> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|w| {
            Some((
                w["instrument_id"].as_str()?.to_owned(),
                w["weight_percent"].as_f64()? / 100.0,
            ))
        })
        .collect()
}

/// Доходность долей между двумя наборами цен, %; без всех цен — нет оценки.
pub(crate) fn weighted_return(
    weights: &[(String, f64)],
    start: &HashMap<String, f64>,
    end: &HashMap<String, f64>,
) -> Option<f64> {
    let mut total = 0.0;
    for (uid, weight) in weights.iter().filter(|(_, w)| *w != 0.0) {
        let (a, b) = (start.get(uid)?, end.get(uid)?);
        if *a <= 0.0 {
            return None;
        }
        total += weight * (b / a - 1.0);
    }
    Some((total * 100.0 * 10_000.0).round() / 10_000.0)
}

/// Оборот перекладки: сумма модулей изменений долей (переложить 20% из одной
/// бумаги в другую — 0.4: продажа и покупка).
pub(crate) fn turnover(previous: &[(String, f64)], next: &[(String, f64)]) -> f64 {
    let mut changes: HashMap<&str, f64> = HashMap::new();
    for (uid, weight) in previous {
        *changes.entry(uid).or_default() -= weight;
    }
    for (uid, weight) in next {
        *changes.entry(uid).or_default() += weight;
    }
    changes.values().map(|change| change.abs()).sum()
}

/// Закрытие торгового дня `horizon` после дня решения; `None` — день ещё не наступил.
pub(crate) fn close_after(daily: &[(NaiveDate, f64)], day: NaiveDate, horizon: i64) -> Option<f64> {
    // День решения или последний торговый день до него.
    let start = daily.iter().rposition(|(date, _)| *date <= day)?;
    let target = start + usize::try_from(horizon).ok()?;
    daily.get(target).map(|(_, close)| *close)
}

type Pending = (Uuid, DateTime<Utc>, bool, Value, Value);
/// Итог к расчёту: решение, его день, доли, цены, горизонт и комиссия перекладки, %.
type Due = (
    Uuid,
    NaiveDate,
    Vec<(String, f64)>,
    HashMap<String, f64>,
    i64,
    f64,
);

/// Дописывает итоги решений стратегии, для которых прошло нужное число дней.
pub(crate) async fn compute(
    storage: &PostgresStorage,
    broker: &VenueBroker,
    strategy_id: &str,
) -> Result<usize> {
    let rows: Vec<Pending> = sqlx::query_as(
        "SELECT d.id, d.created_at, d.hold, d.weights, d.prices FROM ai_decisions d \
         WHERE d.strategy_id = $1 AND d.kind = 'decision' AND d.status = 'ok' AND d.prices IS NOT NULL \
           AND d.created_at >= now() - make_interval(days => $2) ORDER BY d.created_at",
    )
    .bind(strategy_id)
    .bind(i32::try_from(WINDOW_DAYS).unwrap_or(i32::MAX))
    .fetch_all(storage.pool())
    .await?;
    let done: Vec<(Uuid, i32)> = sqlx::query_as(
        "SELECT o.decision_id, o.horizon_days FROM ai_outcomes o JOIN ai_decisions d ON d.id = o.decision_id WHERE d.strategy_id = $1",
    )
    .bind(strategy_id)
    .fetch_all(storage.pool())
    .await?;
    let Some(first) = rows.first().map(|r| r.1) else {
        return Ok(0);
    };
    // Ставка комиссии контура, % объёма сделки.
    let rate = crate::instruments::ExecutionCosts::load(storage, broker.contour().label())
        .await
        .rate(tinkoff_rs::engine::InstrumentKind::Share);
    let rate = number(rate * rust_decimal::Decimal::ONE_HUNDRED);
    // Доли «держать» — последние доли с весами до этого решения.
    let mut last_weights: Vec<(String, f64)> = Vec::new();
    let mut pending: Vec<Due> = Vec::new();
    for (id, created, hold, weights, prices) in rows {
        let weights = if hold {
            last_weights.clone()
        } else {
            weights_of(&weights)
        };
        let costs = if hold {
            0.0
        } else {
            ((turnover(&last_weights, &weights) * rate) * 10_000.0).round() / 10_000.0
        };
        if !hold {
            last_weights.clone_from(&weights);
        }
        let prices: HashMap<String, f64> = prices
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(uid, p)| Some((uid.clone(), p.as_f64()?)))
            .collect();
        let day = (created + Duration::hours(3)).date_naive();
        for horizon in HORIZONS {
            if !done.contains(&(id, i32::try_from(horizon).unwrap_or(i32::MAX))) {
                pending.push((id, day, weights.clone(), prices.clone(), horizon, costs));
            }
        }
    }
    if pending.is_empty() {
        return Ok(0);
    }
    let mut uids: Vec<String> = pending
        .iter()
        .flat_map(|(_, _, weights, ..)| weights.iter().map(|(uid, _)| uid.clone()))
        .collect();
    uids.push(market::IMOEX.to_owned());
    uids.sort();
    uids.dedup();
    let mut daily: HashMap<String, Vec<(NaiveDate, f64)>> = HashMap::new();
    for uid in &uids {
        let candles = history::load(
            broker,
            CandleHistoryRequest {
                instrument_id: InstrumentId::new(uid.clone())?,
                interval: CandleInterval::OneDay,
                from: first - Duration::days(7),
                to: Utc::now(),
                limit: None,
            },
            None,
        )
        .await?;
        daily.insert(
            uid.clone(),
            candles
                .iter()
                .filter(|c| c.complete)
                .map(|c| {
                    (
                        (c.time + Duration::hours(3)).date_naive(),
                        number(c.close.0),
                    )
                })
                .collect(),
        );
    }
    let mut written = 0;
    for (id, day, weights, start, horizon, costs) in pending {
        let end: HashMap<String, f64> = daily
            .iter()
            .filter_map(|(uid, series)| Some((uid.clone(), close_after(series, day, horizon)?)))
            .collect();
        // Индекс ещё без нужного дня — итог ещё не наступил.
        let Some(benchmark) = weighted_return(&[(market::IMOEX.to_owned(), 1.0)], &start, &end)
        else {
            continue;
        };
        // Доходность решения после комиссии за его перекладку.
        let portfolio = weighted_return(&weights, &start, &end)
            .map(|gross| ((gross - costs) * 10_000.0).round() / 10_000.0);
        sqlx::query(
            "INSERT INTO ai_outcomes(decision_id, horizon_days, portfolio_return, benchmark_return, costs_percent) \
             VALUES($1, $2, $3::numeric, $4::numeric, $5::numeric) ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(i32::try_from(horizon).unwrap_or(i32::MAX))
        .bind(portfolio.map(|p| p.to_string()))
        .bind(benchmark.to_string())
        .bind(costs.to_string())
        .execute(storage.pool())
        .await?;
        written += 1;
    }
    Ok(written)
}

/// Сводка итогов последних решений для контекста модели.
pub(crate) fn summary(decisions: &[Value]) -> Option<String> {
    let mut scored = 0;
    let mut better = 0;
    let mut excess = 0.0;
    for decision in decisions {
        let outcome = &decision["outcome"]["5d"];
        if let (Some(portfolio), Some(imoex)) =
            (outcome["portfolio"].as_f64(), outcome["imoex"].as_f64())
        {
            scored += 1;
            excess += portfolio - imoex;
            if portfolio > imoex {
                better += 1;
            }
        }
    }
    (scored > 0).then(|| {
        format!(
            "из {scored} решений, оценённых через 5 торговых дней, лучше индекса Мосбиржи {better}; в среднем {:+.2} п.п. к индексу",
            excess / f64::from(scored)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn moving_a_share_from_one_stock_to_another_costs_two_commissions() {
        let w = |pairs: &[(&str, f64)]| {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), *v))
                .collect::<Vec<_>>()
        };
        assert!(
            (turnover(&[], &w(&[("a", 0.2)])) - 0.2).abs() < 1e-12,
            "buying 20%"
        );
        assert!(
            (turnover(&w(&[("a", 0.2)]), &w(&[("b", 0.2)])) - 0.4).abs() < 1e-12,
            "sell a, buy b"
        );
        assert!((turnover(&w(&[("a", 0.3)]), &w(&[("a", 0.25)])) - 0.05).abs() < 1e-12);
        assert!(turnover(&w(&[("a", 0.2)]), &w(&[("a", 0.2)])).abs() < 1e-12);
    }

    #[test]
    fn decisions_are_scored_against_the_index_after_trading_days() {
        let day = |d: u32| NaiveDate::from_ymd_opt(2026, 10, d).unwrap();
        // Weekend 17–18 October: the horizon counts trading days.
        let series = vec![
            (day(15), 100.0),
            (day(16), 101.0),
            (day(19), 103.0),
            (day(20), 99.0),
        ];
        assert_eq!(close_after(&series, day(16), 1), Some(103.0));
        assert_eq!(
            close_after(&series, day(17), 1),
            Some(103.0),
            "a weekend decision counts from Friday"
        );
        assert_eq!(close_after(&series, day(16), 5), None, "not yet");
        let start = HashMap::from([
            ("a".to_owned(), 100.0),
            ("b".to_owned(), 50.0),
            ("ix".to_owned(), 2000.0),
        ]);
        let end = HashMap::from([
            ("a".to_owned(), 110.0),
            ("b".to_owned(), 45.0),
            ("ix".to_owned(), 2020.0),
        ]);
        // 30% in a (+10%), 20% in b (−10%), the rest in cash: +1%.
        assert_eq!(
            weighted_return(
                &[("a".to_owned(), 0.3), ("b".to_owned(), 0.2)],
                &start,
                &end
            ),
            Some(1.0)
        );
        assert_eq!(
            weighted_return(&[("ix".to_owned(), 1.0)], &start, &end),
            Some(1.0)
        );
        assert_eq!(
            weighted_return(&[("c".to_owned(), 0.5)], &start, &end),
            None
        );
        assert_eq!(weighted_return(&[], &start, &end), Some(0.0), "all in cash");
        let decisions = vec![
            json!({"outcome": {"5d": {"portfolio": 2.0, "imoex": 1.0}}}),
            json!({"outcome": {"5d": {"portfolio": -1.0, "imoex": 0.5}}}),
            json!({"outcome": {}}),
        ];
        assert_eq!(
            summary(&decisions).unwrap(),
            "из 2 решений, оценённых через 5 торговых дней, лучше индекса Мосбиржи 1; в среднем -0.25 п.п. к индексу"
        );
        assert_eq!(
            weights_of(&json!([{"instrument_id": "a", "weight_percent": 25}])),
            vec![("a".to_owned(), 0.25)]
        );
    }
}
