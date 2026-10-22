//! Тезисы модели: бумага, направление, обоснование, горизонт и условия выхода.
//! Условия по цене и дате проверяет сторож, а при закрытии считается результат
//! тезиса от цены открытия.
use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use tinkoff_rs::{Result, storage::PostgresStorage};
use uuid::Uuid;

use super::truncate;

/// Активных тезисов на стратегию не больше.
pub(crate) const MAX_OPEN: i64 = 30;

/// Условие выхода тезиса.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) enum Condition {
    PriceBelow(f64),
    PriceAbove(f64),
    Date(NaiveDate),
}

impl Condition {
    fn parse(value: &Value) -> Option<Self> {
        let level = || {
            value["value"]
                .as_f64()
                .filter(|v| v.is_finite() && *v > 0.0)
        };
        match value["kind"].as_str()? {
            "price_below" => level().map(Self::PriceBelow),
            "price_above" => level().map(Self::PriceAbove),
            "date" => value["value"]
                .as_str()
                .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
                .map(Self::Date),
            _ => None,
        }
    }

    /// Наступило ли условие при этой цене и дате.
    pub(crate) fn met(&self, price: Option<f64>, today: NaiveDate) -> bool {
        match (self, price) {
            (Self::PriceBelow(level), Some(price)) => price <= *level,
            (Self::PriceAbove(level), Some(price)) => price >= *level,
            (Self::Date(date), _) => today >= *date,
            _ => false,
        }
    }

    pub(crate) fn describe(&self) -> String {
        match self {
            Self::PriceBelow(level) => format!("цена не выше {level}"),
            Self::PriceAbove(level) => format!("цена не ниже {level}"),
            Self::Date(date) => format!("наступила дата {date}"),
        }
    }
}

/// Условия из записи: `[{kind, value, note}]`.
pub(crate) fn conditions(value: &Value) -> Vec<Condition> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Condition::parse)
        .collect()
}

/// Активный тезис для сторожа и контекста.
#[derive(Clone, Debug)]
#[allow(clippy::struct_field_names)] // «тезис» и его текст
pub(crate) struct Thesis {
    pub id: Uuid,
    pub ticker: String,
    pub direction: String,
    pub thesis: String,
    pub conditions: Vec<Condition>,
    pub entry_price: Option<f64>,
}

type Row = (
    Uuid,
    String,
    String,
    String,
    Value,
    Option<String>,
    DateTime<Utc>,
    Option<i32>,
    Value,
    Option<String>,
);

/// Открытые тезисы стратегии.
pub(crate) async fn open_theses(
    storage: &PostgresStorage,
    strategy_id: &str,
) -> Result<Vec<(Thesis, Value)>> {
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, ticker, direction, thesis, conditions, entry_price::text, opened_at, horizon_days, sources, inherited_from \
         FROM ai_theses WHERE strategy_id = $1 AND closed_at IS NULL ORDER BY opened_at",
    )
    .bind(strategy_id)
    .fetch_all(storage.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, ticker, direction, thesis, raw, entry, opened_at, horizon, sources, inherited)| {
            let entry_price = entry.and_then(|e| e.parse().ok());
            let view = json!({"id": id, "ticker": ticker, "direction": direction, "thesis": thesis,
                              "entry_price": entry_price, "opened": opened_at, "horizon_days": horizon,
                              "conditions": raw, "sources": sources, "inherited": inherited.is_some()});
            (
                Thesis {
                    id,
                    conditions: conditions(&raw),
                    ticker,
                    direction,
                    thesis,
                    entry_price,
                },
                view,
            )
        })
        .collect())
}

/// Результат тезиса в процентах от цены открытия с учётом направления.
pub(crate) fn result_percent(direction: &str, entry: f64, close: f64) -> Option<f64> {
    if entry <= 0.0 || !close.is_finite() {
        return None;
    }
    let change = (close / entry - 1.0) * 100.0;
    // Short и «избегать» верны, если бумага не выросла.
    let signed = if direction == "long" { change } else { -change };
    Some((signed * 1000.0).round() / 1000.0)
}

fn condition_list(input: &Value) -> Value {
    json!(input
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| Condition::parse(c).is_some())
        .take(5)
        .map(|c| json!({"kind": c["kind"], "value": c["value"], "note": truncate(c["note"].as_str().unwrap_or_default(), 200)}))
        .collect::<Vec<_>>())
}

/// Новый тезис. `price` — текущая цена бумаги.
pub(crate) async fn open(
    storage: &PostgresStorage,
    strategy_id: &str,
    ticker: &str,
    input: &Value,
    price: Option<f64>,
) -> Result<Value> {
    let direction = input["direction"].as_str().unwrap_or("long");
    if !matches!(direction, "long" | "short" | "avoid") {
        return Ok(json!({"error": "direction: long, short или avoid"}));
    }
    let text = input["thesis"].as_str().unwrap_or_default().trim();
    if text.is_empty() {
        return Ok(json!({"error": "опиши тезис"}));
    }
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM ai_theses WHERE strategy_id = $1 AND closed_at IS NULL",
    )
    .bind(strategy_id)
    .fetch_one(storage.pool())
    .await?;
    if open >= MAX_OPEN {
        return Ok(
            json!({"error": format!("открыто уже {MAX_OPEN} тезисов: закрой отработанные через close_thesis")}),
        );
    }
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO ai_theses(id, strategy_id, ticker, direction, thesis, horizon_days, entry_price, conditions, sources) \
         VALUES($1, $2, $3, $4, $5, $6, $7::numeric, $8, $9)",
    )
    .bind(id)
    .bind(strategy_id)
    .bind(ticker)
    .bind(direction)
    .bind(truncate(text, 1500))
    .bind(input["horizon_days"].as_i64().and_then(|d| i32::try_from(d.clamp(1, 730)).ok()))
    .bind(price.map(|p| p.to_string()))
    .bind(condition_list(&input["exit_conditions"]))
    .bind(json!(input["sources"].as_array().into_iter().flatten().filter_map(Value::as_str).take(10).collect::<Vec<_>>()))
    .execute(storage.pool())
    .await?;
    Ok(json!({"opened": id, "entry_price": price}))
}

/// Уточнение тезиса: текст, условия, горизонт.
pub(crate) async fn update(
    storage: &PostgresStorage,
    strategy_id: &str,
    input: &Value,
) -> Result<Value> {
    let Some(id) = input["id"].as_str().and_then(|id| Uuid::parse_str(id).ok()) else {
        return Ok(json!({"error": "укажи id тезиса"}));
    };
    let updated = sqlx::query(
        "UPDATE ai_theses SET thesis = coalesce($3, thesis), conditions = coalesce($4, conditions), \
           horizon_days = coalesce($5, horizon_days), updated_at = now() \
         WHERE id = $1 AND strategy_id = $2 AND closed_at IS NULL",
    )
    .bind(id)
    .bind(strategy_id)
    .bind(input["thesis"].as_str().map(|t| truncate(t.trim(), 1500)))
    .bind(input["exit_conditions"].is_array().then(|| condition_list(&input["exit_conditions"])))
    .bind(input["horizon_days"].as_i64().and_then(|d| i32::try_from(d.clamp(1, 730)).ok()))
    .execute(storage.pool())
    .await?
    .rows_affected();
    Ok(json!({"updated": updated == 1}))
}

/// Закрытие тезиса с результатом от цены открытия.
pub(crate) async fn close(
    storage: &PostgresStorage,
    strategy_id: &str,
    input: &Value,
    price_of: impl Fn(&str) -> Option<f64>,
) -> Result<Value> {
    let Some(id) = input["id"].as_str().and_then(|id| Uuid::parse_str(id).ok()) else {
        return Ok(json!({"error": "укажи id тезиса"}));
    };
    let row: Option<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT ticker, direction, entry_price::text FROM ai_theses WHERE id = $1 AND strategy_id = $2 AND closed_at IS NULL",
    )
    .bind(id)
    .bind(strategy_id)
    .fetch_optional(storage.pool())
    .await?;
    let Some((ticker, direction, entry)) = row else {
        return Ok(json!({"error": "открытого тезиса с таким id нет"}));
    };
    let price = price_of(&ticker);
    let result = match (entry.and_then(|e| e.parse::<f64>().ok()), price) {
        (Some(entry), Some(price)) => result_percent(&direction, entry, price),
        _ => None,
    };
    sqlx::query(
        "UPDATE ai_theses SET closed_at = now(), close_reason = $3, close_price = $4::numeric, result_percent = $5::numeric, updated_at = now() \
         WHERE id = $1 AND strategy_id = $2",
    )
    .bind(id)
    .bind(strategy_id)
    .bind(truncate(input["reason"].as_str().unwrap_or("без причины"), 500))
    .bind(price.map(|p| p.to_string()))
    .bind(result.map(|r| r.to_string()))
    .execute(storage.pool())
    .await?;
    Ok(json!({"closed": true, "result_percent": result}))
}

/// Закрытые тезисы за период для разбора.
pub(crate) async fn closed_since(
    storage: &PostgresStorage,
    strategy_id: &str,
    since: DateTime<Utc>,
) -> Result<Value> {
    Ok(sqlx::query_scalar(
        "SELECT coalesce(json_agg(t ORDER BY t.closed_at), '[]') FROM (SELECT ticker, direction, thesis, opened_at, closed_at, \
           close_reason, result_percent FROM ai_theses WHERE strategy_id = $1 AND closed_at >= $2) t",
    )
    .bind(strategy_id)
    .bind(since)
    .fetch_one(storage.pool())
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditions_and_results_follow_the_direction() {
        let list = conditions(&json!([
            {"kind": "price_below", "value": 250.0}, {"kind": "price_above", "value": 320},
            {"kind": "date", "value": "2026-12-01"}, {"kind": "price_below", "value": -1}, {"kind": "mood", "value": 1}]));
        assert_eq!(
            list,
            vec![
                Condition::PriceBelow(250.0),
                Condition::PriceAbove(320.0),
                Condition::Date(NaiveDate::from_ymd_opt(2026, 12, 1).unwrap())
            ]
        );
        let today = NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();
        assert!(list[0].met(Some(249.9), today) && !list[0].met(Some(260.0), today));
        assert!(list[1].met(Some(320.0), today) && !list[1].met(None, today));
        assert!(
            !list[2].met(None, today)
                && list[2].met(None, NaiveDate::from_ymd_opt(2026, 12, 1).unwrap())
        );
        assert_eq!(result_percent("long", 100.0, 110.0), Some(10.0));
        assert_eq!(result_percent("short", 100.0, 110.0), Some(-10.0));
        assert_eq!(
            result_percent("avoid", 100.0, 90.0),
            Some(10.0),
            "avoiding a falling stock was right"
        );
        assert_eq!(result_percent("long", 0.0, 110.0), None);
    }
}
