//! Векторы фрагментов для поиска по смыслу (pgvector, `halfvec`).
//!
//! Сервис — любой OpenAI-совместимый `/embeddings` (Ollama, text-embeddings-
//! inference, vLLM, `OpenAI`, Voyage), задаётся разделом `[embeddings]` файла
//! источников. Без раздела или без расширения `vector` в `PostgreSQL` поиск
//! остаётся полнотекстовым: таблица и индекс создаются только когда всё готово.
use std::{fmt::Write as _, time::Duration as StdDuration};

use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use tinkoff_rs::{Error, Result, storage::PostgresStorage};
use tracing::warn;

use super::{
    fnv,
    sources::{Embeddings, Sources},
};
use crate::ai::usage::{Spend, Usage};

/// Сообщения одного события от разных агентств выходят в пределах часов.
const SAME_EVENT_WINDOW: Duration = Duration::hours(12);
/// Текст фрагмента для вектора ограничен: модели с окном 512 токенов.
const EMBED_CHARS: usize = 2000;

/// Подключение к сервису векторов.
pub(crate) struct Vectors {
    client: reqwest::Client,
    settings: Embeddings,
    token: Option<String>,
}

fn model_error(message: impl Into<String>) -> Error {
    Error::Configuration(format!("embeddings: {}", message.into()))
}

/// Литерал pgvector `[0.1,0.2]`.
fn literal(vector: &[f32]) -> String {
    let mut out = String::with_capacity(vector.len() * 8);
    out.push('[');
    for (index, value) in vector.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(out, "{value:.6}");
    }
    out.push(']');
    out
}

impl Vectors {
    /// Подключение из настроек; `None` — векторы не настроены.
    pub(crate) fn from_sources(sources: &Sources) -> Option<Self> {
        let settings = sources.embeddings.clone()?;
        let client = reqwest::Client::builder()
            .timeout(StdDuration::from_secs(120))
            .build()
            .ok()?;
        Some(Self {
            client,
            token: sources.embeddings_token(),
            settings,
        })
    }

    pub(crate) const fn dimensions(&self) -> u32 {
        self.settings.dimensions
    }

    /// Модель SQL-литералом. Индекс модели частичный (`WHERE model = '…'`):
    /// с параметром вместо литерала подготовленный запрос его не использует.
    pub(crate) fn model_sql(&self) -> Result<String> {
        let model = &self.settings.model;
        if model.is_empty()
            || !model
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._:/-".contains(c))
        {
            return Err(model_error(
                "model name may contain only letters, digits and ._:/-",
            ));
        }
        Ok(format!("'{model}'"))
    }

    /// Имя индекса модели: модель и размерность в хэше.
    fn index_name(&self) -> String {
        format!(
            "document_embeddings_{:012x}_idx",
            fnv(&[&self.settings.model, &self.settings.dimensions.to_string()]) & 0xffff_ffff_ffff
        )
    }

    /// Расширение, таблица и индекс модели. `false` — pgvector недоступен.
    pub(crate) async fn ensure_schema(&self, storage: &PostgresStorage) -> Result<bool> {
        if sqlx::query("CREATE EXTENSION IF NOT EXISTS vector")
            .execute(storage.pool())
            .await
            .is_err()
        {
            return Ok(false);
        }
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS document_embeddings (\
               document_id TEXT NOT NULL, ord INTEGER NOT NULL, model TEXT NOT NULL, \
               embedding halfvec NOT NULL, created_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
               PRIMARY KEY (document_id, ord, model), \
               FOREIGN KEY (document_id, ord) REFERENCES document_chunks(document_id, ord) ON DELETE CASCADE)",
        )
        .execute(storage.pool())
        .await?;
        let model = self.model_sql()?;
        let dimensions = self.settings.dimensions;
        sqlx::query(&format!(
            "CREATE INDEX IF NOT EXISTS {} ON document_embeddings USING hnsw \
             ((embedding::halfvec({dimensions})) halfvec_cosine_ops) WHERE model = {model}",
            self.index_name()
        ))
        .execute(storage.pool())
        .await?;
        Ok(true)
    }

    /// Векторы текстов; `query` — запрос пользователя, иначе документы.
    pub(crate) async fn embed(
        &self,
        texts: &[String],
        query: bool,
    ) -> Result<(Vec<Vec<f32>>, u64)> {
        let prefix = if query {
            &self.settings.query_prefix
        } else {
            &self.settings.document_prefix
        };
        let input: Vec<String> = texts
            .iter()
            .map(|text| {
                format!(
                    "{prefix}{}",
                    text.chars().take(EMBED_CHARS).collect::<String>()
                )
            })
            .collect();
        let mut body = json!({"model": self.settings.model, "input": input});
        if self.settings.input_type {
            body["input_type"] = json!(if query { "query" } else { "document" });
        }
        let mut request = self
            .client
            .post(format!(
                "{}/embeddings",
                self.settings.url.trim_end_matches('/')
            ))
            .json(&body);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .map_err(|error| model_error(error.without_url().to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| model_error(error.without_url().to_string()))?;
        if !status.is_success() {
            return Err(model_error(format!(
                "HTTP {status}: {}",
                text.chars().take(300).collect::<String>()
            )));
        }
        let value: Value = serde_json::from_str(&text)
            .map_err(|error| model_error(format!("invalid JSON: {error}")))?;
        let mut data: Vec<&Value> = value["data"].as_array().into_iter().flatten().collect();
        data.sort_by_key(|item| item["index"].as_u64().unwrap_or_default());
        let vectors: Vec<Vec<f32>> = data
            .iter()
            .map(|item| {
                item["embedding"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_f64)
                    // Векторы хранятся в половинной точности: f32 более чем достаточно.
                    .map(|v| {
                        #[allow(clippy::cast_possible_truncation)]
                        let value = v as f32;
                        value
                    })
                    .collect()
            })
            .collect();
        if vectors.len() != texts.len()
            || vectors
                .iter()
                .any(|v| v.len() != self.settings.dimensions as usize)
        {
            return Err(model_error(format!(
                "expected {} vectors of {} numbers",
                texts.len(),
                self.settings.dimensions
            )));
        }
        let tokens = value["usage"]["prompt_tokens"]
            .as_u64()
            .or_else(|| value["usage"]["total_tokens"].as_u64())
            .unwrap_or_else(|| texts.iter().map(|t| t.chars().count() as u64 / 4).sum());
        Ok((vectors, tokens))
    }

    /// Вектор запроса как литерал pgvector.
    pub(crate) async fn embed_query(&self, text: &str) -> Result<Option<String>> {
        if text.trim().is_empty() {
            return Ok(None);
        }
        let (vectors, _) = self.embed(&[text.to_owned()], true).await?;
        Ok(vectors.first().map(|v| literal(v)))
    }

    /// Расход сервиса векторов для учёта.
    pub(crate) fn spend(&self, usage: Usage) -> Spend {
        Spend {
            profile: Some("embeddings".to_owned()),
            model: Some(self.settings.model.clone()),
            usage,
            prices: Some(crate::ai::Prices {
                currency: self.settings.price_currency,
                input: self.settings.price_per_million,
                output: 0.0,
                cache_write: None,
                cache_read: None,
                web_search_per_1000: None,
            }),
        }
    }

    /// Векторы для новых фрагментов, свежие сначала. Возвращает число векторов
    /// и токены.
    pub(crate) async fn backfill(
        &self,
        storage: &PostgresStorage,
        keep_days: u32,
        limit: usize,
    ) -> Result<(usize, Usage)> {
        type Pending = (String, i32, String, String, DateTime<Utc>, String);
        let rows: Vec<Pending> = sqlx::query_as(
            "SELECT c.document_id, c.ord, c.context, c.text, c.published_at, d.cluster_id \
             FROM document_chunks c JOIN documents d ON d.id = c.document_id \
             WHERE c.published_at >= now() - make_interval(days => $2) AND d.status <> 'skipped' \
               AND d.body_dropped_at IS NULL \
               AND NOT EXISTS (SELECT 1 FROM document_embeddings e WHERE e.document_id = c.document_id AND e.ord = c.ord AND e.model = $1) \
             ORDER BY c.published_at DESC LIMIT $3",
        )
        .bind(&self.settings.model)
        .bind(i32::try_from(keep_days).unwrap_or(i32::MAX))
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(storage.pool())
        .await?;
        let mut usage = Usage::default();
        let mut stored = 0;
        for batch in rows.chunks(self.settings.batch.max(1)) {
            let texts: Vec<String> = batch
                .iter()
                .map(|(_, _, context, text, _, _)| format!("{context}\n{text}"))
                .collect();
            let (vectors, tokens) = self.embed(&texts, false).await?;
            usage.add_embedding(tokens);
            let mut transaction = storage.pool().begin().await?;
            for ((document, ord, ..), vector) in batch.iter().zip(&vectors) {
                sqlx::query(
                    "INSERT INTO document_embeddings(document_id, ord, model, embedding) \
                     VALUES($1, $2, $3, $4::text::halfvec) ON CONFLICT DO NOTHING",
                )
                .bind(document)
                .bind(ord)
                .bind(&self.settings.model)
                .bind(literal(vector))
                .execute(&mut *transaction)
                .await?;
            }
            transaction.commit().await?;
            stored += batch.len();
            for (document, ord, _, _, published, cluster) in batch {
                if *ord == 0 {
                    if let Err(error) = self
                        .merge_event(storage, document, cluster, *published)
                        .await
                    {
                        warn!(%error, "could not compare a document with similar events");
                    }
                }
            }
        }
        Ok((stored, usage))
    }

    /// Одно событие от разных агентств: новый одиночный документ присоединяется
    /// к кластеру с близким по смыслу первым фрагментом другого источника рядом
    /// по времени. Готовые кластеры между собой не сливаются: иначе соседние
    /// темы собирались бы цепочкой в одно «событие».
    async fn merge_event(
        &self,
        storage: &PostgresStorage,
        document: &str,
        cluster: &str,
        published: DateTime<Utc>,
    ) -> Result<()> {
        if self.settings.same_event_similarity >= 1.0 {
            return Ok(());
        }
        let alone: bool =
            sqlx::query_scalar("SELECT count(*) = 1 FROM documents WHERE cluster_id = $1")
                .bind(cluster)
                .fetch_one(storage.pool())
                .await?;
        if !alone {
            return Ok(());
        }
        let dimensions = self.settings.dimensions;
        let similar: Option<(String, f64)> = sqlx::query_as(&format!(
            "SELECT d.cluster_id, (1 - ((e.embedding::halfvec({dimensions})) <=> (me.embedding::halfvec({dimensions}))))::float8 \
             FROM document_embeddings me JOIN documents mine ON mine.id = me.document_id, \
                  document_embeddings e JOIN documents d ON d.id = e.document_id \
             WHERE me.document_id = $1 AND me.ord = 0 AND me.model = $2 AND e.model = $2 AND e.ord = 0 \
               AND d.cluster_id <> $3 AND d.source <> mine.source AND d.published_at BETWEEN $4 AND $5 \
             ORDER BY (e.embedding::halfvec({dimensions})) <=> (me.embedding::halfvec({dimensions})) LIMIT 1"
        ))
        .bind(document)
        .bind(&self.settings.model)
        .bind(cluster)
        .bind(published - SAME_EVENT_WINDOW)
        .bind(published + SAME_EVENT_WINDOW)
        .fetch_optional(storage.pool())
        .await?;
        if let Some((target, similarity)) =
            similar.filter(|(_, s)| *s >= self.settings.same_event_similarity)
        {
            sqlx::query("UPDATE documents SET cluster_id = $1 WHERE id = $2")
                .bind(&target)
                .bind(document)
                .execute(storage.pool())
                .await?;
            tracing::debug!(document, %target, similarity, "same event from another source");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vectors_are_written_as_pgvector_literals() {
        assert_eq!(literal(&[0.5, -0.25, 1.0]), "[0.500000,-0.250000,1.000000]");
        assert_eq!(literal(&[]), "[]");
    }

    #[tokio::test]
    async fn the_service_returns_vectors_in_input_order_with_token_usage() {
        let (url, seen) = crate::ai::testkit::provider(vec![json!({
            "data": [{"index": 1, "embedding": [0.0, 1.0]}, {"index": 0, "embedding": [1.0, 0.0]}],
            "usage": {"prompt_tokens": 12}
        })])
        .await;
        let sources: Sources = toml::from_str(&format!(
            "[embeddings]\nurl = \"{url}\"\nmodel = \"multilingual-e5-small\"\ndimensions = 2\ndocument_prefix = \"passage: \"\ninput_type = true\ntoken = \"key\""
        ))
        .unwrap();
        let vectors = Vectors::from_sources(&sources).unwrap();
        let (result, tokens) = vectors
            .embed(&["первый".to_owned(), "второй".to_owned()], false)
            .await
            .unwrap();
        assert_eq!((result, tokens), (vec![vec![1.0, 0.0], vec![0.0, 1.0]], 12));
        let seen = seen.lock().unwrap();
        let (head, body) = &seen[0];
        assert!(
            head.starts_with("post /v1/embeddings") && head.contains("authorization: bearer key")
        );
        assert_eq!(body["input"][0], "passage: первый");
        assert_eq!(body["input_type"], "document");
    }
}
