//! Долгосрочная память модели: заметки и прошлые решения.
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use tinkoff_rs::{Result, storage::PostgresStorage};
use uuid::Uuid;

use super::truncate;

/// Заметка памяти: id, текст, тикеры, создана, истекает.
pub(crate) type MemoryRow = (
    Uuid,
    String,
    Value,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
    String,
);
/// Прошлое решение: время, причина, «держать», доли, обоснование.
pub(crate) type DecisionRow = (
    DateTime<Utc>,
    Option<String>,
    bool,
    Value,
    Option<String>,
    Option<Value>,
);

pub(crate) async fn memory_notes(
    storage: &PostgresStorage,
    strategy_id: &str,
) -> Result<Vec<Value>> {
    let rows: Vec<MemoryRow> = sqlx::query_as(
        "SELECT id,note,tickers,created_at,expires_at,kind FROM ai_memory WHERE strategy_id=$1 AND closed_at IS NULL AND (expires_at IS NULL OR expires_at>now()) ORDER BY kind, created_at DESC LIMIT 80",
    )
    .bind(strategy_id)
    .fetch_all(storage.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, note, tickers, created, expires, kind)| json!({"id": id, "kind": kind, "created": created, "expires": expires, "tickers": tickers, "note": note}))
        .collect())
}

pub(crate) async fn past_decisions(
    storage: &PostgresStorage,
    strategy_id: &str,
    limit: i64,
) -> Result<Vec<Value>> {
    let rows: Vec<DecisionRow> = sqlx::query_as(
        // Ревизия и реальная копия продолжают историю предка до момента ответвления.
        "WITH RECURSIVE line(id, until, depth) AS (SELECT $1::text, 'infinity'::timestamptz, 0 \
         UNION ALL SELECT l.parent_id, least(line.until, l.forked_at), line.depth + 1 \
         FROM strategy_lineage l JOIN line ON l.strategy_id = line.id WHERE line.depth < 20) \
         SELECT d.created_at,d.trigger,d.hold,d.weights,d.rationale, \
           (SELECT json_object_agg(o.horizon_days || 'd', json_build_object('portfolio', o.portfolio_return, 'imoex', o.benchmark_return)) \
              FROM ai_outcomes o WHERE o.decision_id = d.id) FROM ai_decisions d JOIN line \
         ON d.strategy_id = line.id AND d.created_at <= line.until \
         WHERE d.kind='decision' AND d.status='ok' ORDER BY d.created_at DESC LIMIT $2",
    )
    .bind(strategy_id)
    .bind(limit)
    .fetch_all(storage.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|(time, trigger, hold, weights, rationale, outcome)| {
            let weights: Vec<Value> = weights
                .as_array()
                .into_iter()
                .flatten()
                .map(|w| json!({"ticker": w["ticker"], "weight_percent": w["weight_percent"]}))
                .collect();
            json!({"time": time, "trigger": trigger, "hold": hold, "weights": weights,
                   "rationale": truncate(rationale.as_deref().unwrap_or_default(), 600),
                   "outcome": outcome.unwrap_or_else(|| json!({}))})
        })
        .collect())
}
