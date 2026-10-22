//! Разбор итогов раз в неделю и сжатие памяти.
//!
//! Перед первым решением недели модель смотрит закрытые тезисы и итоги своих
//! решений против индекса и записывает выводы — короткие уроки, которые видит во
//! всех следующих решениях. Когда заметок становится много, старые сжимаются в
//! сводки.
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use tinkoff_rs::{Result, storage::PostgresStorage};
use uuid::Uuid;

use super::{
    AiJob,
    client::Chat,
    memory::past_decisions,
    profiles::Profiles,
    theses, truncate,
    usage::{self, Meter},
};

/// Разбор не чаще раза в неделю.
const REVIEW_EVERY: Duration = Duration::days(7);
/// Активных выводов не больше; лишние закрываются, старые первыми.
const MAX_LESSONS: i64 = 20;
/// Сжимаются заметки, когда их больше.
const COMPACT_ABOVE: i64 = 40;
const COMPACT_BATCH: i64 = 20;

fn review_tool() -> Value {
    json!({"name": "submit_review", "description": "Итог разбора: новые выводы и устаревшие выводы для закрытия.",
        "parameters": {"type": "object", "properties": {
            "lessons": {"type": "array", "items": {"type": "object", "properties": {
                "text": {"type": "string", "description": "Вывод: что работало или не работало и как поступать дальше, до 300 символов"},
                "tickers": {"type": "array", "items": {"type": "string"}}}, "required": ["text"]}},
            "retire": {"type": "array", "items": {"type": "string"}, "description": "id выводов, которые больше не верны"}},
            "required": ["lessons"]}})
}

fn summary_tool() -> Value {
    json!({"name": "submit_summaries", "description": "Сводки старых заметок: не больше 5, только то, что ещё важно.",
        "parameters": {"type": "object", "properties": {"summaries": {"type": "array", "items": {"type": "object", "properties": {
            "text": {"type": "string"}, "tickers": {"type": "array", "items": {"type": "string"}}}, "required": ["text"]}}},
            "required": ["summaries"]}})
}

async fn active(
    storage: &PostgresStorage,
    strategy_id: &str,
    kind: &str,
) -> Result<Vec<(Uuid, String)>> {
    Ok(sqlx::query_as(
        "SELECT id, note FROM ai_memory WHERE strategy_id = $1 AND kind = $2 AND closed_at IS NULL \
         AND (expires_at IS NULL OR expires_at > now()) ORDER BY created_at",
    )
    .bind(strategy_id)
    .bind(kind)
    .fetch_all(storage.pool())
    .await?)
}

async fn insert(
    storage: &PostgresStorage,
    strategy_id: &str,
    kind: &str,
    item: &Value,
) -> Result<()> {
    let text = truncate(item["text"].as_str().unwrap_or_default().trim(), 600);
    if text.is_empty() {
        return Ok(());
    }
    let tickers: Vec<String> = item["tickers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_uppercase)
        .take(10)
        .collect();
    sqlx::query(
        "INSERT INTO ai_memory(id, strategy_id, note, tickers, kind) VALUES($1, $2, $3, $4, $5)",
    )
    .bind(Uuid::new_v4())
    .bind(strategy_id)
    .bind(text)
    .bind(json!(tickers))
    .bind(kind)
    .execute(storage.pool())
    .await?;
    Ok(())
}

async fn close(storage: &PostgresStorage, ids: &[Uuid], reason: &str) -> Result<()> {
    sqlx::query("UPDATE ai_memory SET closed_at = now(), closed_reason = $2 WHERE id = ANY($1) AND closed_at IS NULL")
        .bind(ids)
        .bind(reason)
        .execute(storage.pool())
        .await?;
    Ok(())
}

/// Пора ли разбор: неделя с прошлого и есть что разбирать.
async fn review_due(storage: &PostgresStorage, strategy_id: &str) -> Result<bool> {
    let last: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("SELECT reviewed_at FROM ai_reviews WHERE strategy_id = $1")
            .bind(strategy_id)
            .fetch_optional(storage.pool())
            .await?;
    if last.is_some_and(|at| Utc::now() - at < REVIEW_EVERY) {
        return Ok(false);
    }
    let since = last.unwrap_or_else(|| Utc::now() - Duration::days(30));
    let material: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM ai_theses WHERE strategy_id = $1 AND closed_at >= $2) \
           OR (SELECT count(*) FROM ai_outcomes o JOIN ai_decisions d ON d.id = o.decision_id \
               WHERE d.strategy_id = $1 AND o.computed_at >= $2) >= 5",
    )
    .bind(strategy_id)
    .bind(since)
    .fetch_one(storage.pool())
    .await?;
    Ok(material)
}

/// Разбор итогов и сжатие памяти, если пора. Возвращает, что сделано.
pub(crate) async fn review_if_due(
    storage: &PostgresStorage,
    strategy_id: &str,
    job: &AiJob,
    meter: &Meter,
) -> Result<Option<String>> {
    if !job.config.memory || usage::budget(storage).await?.exhausted() {
        return Ok(None);
    }
    let mut done = Vec::new();
    let profiles = Profiles::load()?;
    let (name, profile) = profiles.get(job.config.model_profile.as_deref())?;
    let mut profile = profile.clone();
    profile.web_search = false;
    if review_due(storage, strategy_id).await? {
        let since = Utc::now() - Duration::days(30);
        let closed = theses::closed_since(storage, strategy_id, since).await?;
        let decisions = past_decisions(storage, strategy_id, 30).await?;
        let lessons = active(storage, strategy_id, "lesson").await?;
        let mut chat = Chat::new(
            name.clone(),
            &profile,
            "Ты разбираешь итоги своей торговли на Московской бирже, чтобы торговать лучше. Отвечай только вызовом \
             submit_review: короткие конкретные выводы (что работало, что нет, какие поводы были ложными) и id \
             выводов, которые больше не верны."
                .to_owned(),
            meter.clone(),
        )?;
        chat.user(&format!(
            "Закрытые тезисы за 30 дней (результат в % с учётом направления): {closed}\n\
             Решения и их итоги (outcome: доходность долей и индекса Мосбиржи через 1, 5, 20 торговых дней): {decisions}\n\
             Действующие выводы: {lessons}",
            decisions = truncate(&json!(decisions).to_string(), 30_000),
            lessons = json!(lessons.iter().map(|(id, text)| json!({"id": id, "text": text})).collect::<Vec<_>>()),
        ));
        let turn = chat.step(&[review_tool()], Some("submit_review")).await?;
        if let Some(call) = turn.calls.iter().find(|c| c.name == "submit_review") {
            let retire: Vec<Uuid> = call.input["retire"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|id| id.as_str().and_then(|id| Uuid::parse_str(id).ok()))
                .collect();
            close(storage, &retire, "устарел по итогам разбора").await?;
            let new = call.input["lessons"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            for lesson in new.iter().take(5) {
                insert(storage, strategy_id, "lesson", lesson).await?;
            }
            // Лишние выводы закрываются, старые первыми.
            let all = active(storage, strategy_id, "lesson").await?;
            let excess =
                usize::try_from((i64::try_from(all.len()).unwrap_or(0) - MAX_LESSONS).max(0))
                    .unwrap_or(0);
            let oldest: Vec<Uuid> = all.iter().take(excess).map(|(id, _)| *id).collect();
            close(storage, &oldest, "вытеснен новыми выводами").await?;
            done.push(format!(
                "выводов: +{}, закрыто {}",
                new.len().min(5),
                retire.len() + oldest.len()
            ));
        }
        sqlx::query(
            "INSERT INTO ai_reviews(strategy_id, reviewed_at) VALUES($1, now()) \
             ON CONFLICT (strategy_id) DO UPDATE SET reviewed_at = now()",
        )
        .bind(strategy_id)
        .execute(storage.pool())
        .await?;
    }
    let notes = active(storage, strategy_id, "note").await?;
    if i64::try_from(notes.len()).unwrap_or(0) > COMPACT_ABOVE {
        let batch: Vec<(Uuid, String)> = notes
            .into_iter()
            .take(usize::try_from(COMPACT_BATCH).unwrap_or(20))
            .collect();
        let mut chat = Chat::new(
            name,
            &profile,
            "Ты сжимаешь свои старые заметки о торговле: оставь только то, что ещё важно, не больше 5 сводок. \
             Отвечай только вызовом submit_summaries."
                .to_owned(),
            meter.clone(),
        )?;
        chat.user(&format!(
            "Заметки: {}",
            json!(batch.iter().map(|(_, text)| text).collect::<Vec<_>>())
        ));
        let turn = chat
            .step(&[summary_tool()], Some("submit_summaries"))
            .await?;
        if let Some(call) = turn.calls.iter().find(|c| c.name == "submit_summaries") {
            let summaries = call.input["summaries"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            for summary in summaries.iter().take(5) {
                insert(storage, strategy_id, "summary", summary).await?;
            }
            let ids: Vec<Uuid> = batch.iter().map(|(id, _)| *id).collect();
            close(storage, &ids, "сжато в сводку").await?;
            done.push(format!(
                "{} заметок сжато в {}",
                ids.len(),
                summaries.len().min(5)
            ));
        }
    }
    Ok((!done.is_empty()).then(|| done.join("; ")))
}
