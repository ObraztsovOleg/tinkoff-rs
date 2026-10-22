//! Поиск по документам: полнотекстовый (русская морфология) по фрагментам и,
//! если настроен сервис векторов, по смыслу; результаты объединяются (RRF) и
//! группируются по событию (кластеру). Чтение документа по страницам и сводка
//! событий для начала решения.
// Оценки ранжирования приблизительны: точности f64 хватает с запасом.
#![allow(clippy::cast_precision_loss)]
use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use tinkoff_rs::{Result, storage::PostgresStorage};

use super::{
    Kind, aliases,
    embed::Vectors,
    exposures::{Exposures, Impact},
};

/// Бумага, о которой ищут документы.
#[derive(Clone, Debug)]
pub(crate) struct Subject {
    pub uid: String,
    pub ticker: String,
    pub name: String,
}

/// Условия поиска.
#[derive(Clone, Debug)]
pub(crate) struct Filter {
    pub text: String,
    pub subjects: Vec<Subject>,
    pub kinds: Vec<Kind>,
    pub sources: Vec<String>,
    /// Темы разметки (любая из них).
    pub topics: Vec<String>,
    /// Не ниже этой важности разметки.
    pub min_importance: Option<i16>,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub limit: usize,
}

/// Условие на темы и важность: `$topics` — массив тем, `$importance` — порог.
fn markup_condition(topics: usize, importance: usize) -> String {
    format!(
        "AND (cardinality(${topics}::text[]) = 0 OR EXISTS (SELECT 1 FROM jsonb_array_elements(d.topics) t WHERE t->>'topic' = ANY(${topics}))) \
         AND (${importance}::smallint IS NULL OR d.importance >= ${importance})"
    )
}

/// Найденное событие: лучший документ кластера и все его источники.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Hit {
    pub id: String,
    pub cluster: String,
    pub time: DateTime<Utc>,
    pub source: String,
    pub sources: Vec<String>,
    pub kind: String,
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tickers: Vec<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub priority: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub importance: Option<i16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_type: Option<String>,
    /// Темы разметки: `[{topic, direction, strength}]`.
    #[serde(skip_serializing_if = "is_empty_array")]
    pub topics: Value,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub snippet: String,
    /// Документ о бумагах запроса (отметка источника или упоминание).
    #[serde(skip)]
    pub about: bool,
    /// Ожидаемое влияние на бумаги запроса через темы.
    #[serde(skip)]
    pub impacts: Vec<Impact>,
    #[serde(skip)]
    pub score: f64,
}

/// Вес вида документа: раскрытие информации и регулятор важнее перепечаток.
fn kind_weight(kind: &str) -> f64 {
    match kind {
        "disclosure" | "report" => 1.4,
        "regulator" | "press_release" => 1.3,
        "news" => 1.0,
        "exchange" => 0.8,
        _ => 0.9,
    }
}

/// Свежесть: событие суточной давности весит 0.75, недельной — около 0.5.
fn recency(time: DateTime<Utc>, now: DateTime<Utc>) -> f64 {
    let hours = (now - time).num_minutes().max(0) as f64 / 60.0;
    0.5 + 0.5 * (-hours / 48.0).exp()
}

fn boost(hit: &Hit, now: DateTime<Utc>) -> f64 {
    recency(hit.time, now)
        * kind_weight(&hit.kind)
        * if hit.priority { 1.2 } else { 1.0 }
        * if hit.about { 1.5 } else { 1.0 }
        * (1.0 + 0.25 * (hit.sources.len().max(1) as f64).ln())
        * hit.importance.map_or(1.0, |i| 0.6 + 0.2 * f64::from(i))
}

fn is_empty_array(value: &Value) -> bool {
    value.as_array().is_none_or(Vec::is_empty)
}

/// Строка выдачи: документ, фрагмент, оценка и признак «о бумагах запроса».
#[derive(sqlx::FromRow)]
struct Row {
    id: String,
    cluster_id: String,
    source: String,
    kind: String,
    title: String,
    url: String,
    published_at: DateTime<Utc>,
    tickers: Vec<String>,
    priority: bool,
    importance: Option<i16>,
    summary: Option<String>,
    event_type: Option<String>,
    topics: Value,
    snippet: String,
    rank: f32,
    about: bool,
}

fn hit(row: Row) -> Hit {
    Hit {
        id: row.id,
        cluster: row.cluster_id,
        time: row.published_at,
        source: row.source,
        sources: Vec::new(),
        kind: row.kind,
        title: row.title,
        url: row.url,
        tickers: row.tickers,
        priority: row.priority,
        importance: row.importance,
        summary: row.summary,
        event_type: row.event_type,
        topics: row.topics,
        // Подсветка совпадений ts_headline модели не нужна.
        snippet: row
            .snippet
            .replace("<b>", "")
            .replace("</b>", "")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
        about: row.about,
        impacts: Vec::new(),
        score: f64::from(row.rank),
    }
}

/// Отметки бумаг: UID источника, тикеры разметки и полнотекстовый запрос названий.
fn subject_query(subjects: &[Subject]) -> (Vec<String>, Vec<String>, String) {
    (
        subjects
            .iter()
            .filter(|s| !s.uid.is_empty())
            .map(|s| s.uid.clone())
            .collect(),
        subjects.iter().map(|s| s.ticker.to_uppercase()).collect(),
        aliases::tsquery(
            subjects
                .iter()
                .map(|s| (s.ticker.as_str(), s.name.as_str())),
        ),
    )
}

const HIT_COLUMNS: &str = "d.id, d.cluster_id, d.source, d.kind, d.title, d.url, d.published_at, d.tickers, d.priority, d.importance, d.summary, d.event_type, d.topics";

/// Полнотекстовые кандидаты: фрагменты, совпавшие с запросом (любым из слов).
async fn text_candidates(storage: &PostgresStorage, filter: &Filter) -> Result<Vec<Hit>> {
    let (uids, tickers, about) = subject_query(&filter.subjects);
    let kinds: Vec<&str> = filter.kinds.iter().map(|k| k.as_str()).collect();
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "WITH q AS (SELECT \
           nullif(replace(plainto_tsquery('russian', $1)::text, '&', '|'), '')::tsquery AS text_q, \
           CASE WHEN $2 = '' THEN NULL ELSE to_tsquery('russian', $2) END AS about_q) \
         SELECT {HIT_COLUMNS}, \
           ts_headline('russian', c.text, coalesce(q.text_q, q.about_q, ''::tsquery), \
             'MaxFragments=2, MaxWords=35, MinWords=12, FragmentDelimiter= … ') AS snippet, \
           CASE WHEN q.text_q IS NULL THEN 1.0 ELSE ts_rank_cd(c.search, q.text_q, 32) END::real AS rank, \
           (d.instruments && $3 OR d.tickers && $8 OR (q.about_q IS NOT NULL AND c.search @@ q.about_q)) AS about \
         FROM document_chunks c JOIN documents d ON d.id = c.document_id, q \
         WHERE c.published_at BETWEEN $4 AND $5 \
           AND (q.text_q IS NULL OR c.search @@ q.text_q) \
           AND (q.about_q IS NULL OR d.instruments && $3 OR d.tickers && $8 OR c.search @@ q.about_q) \
           AND (cardinality($6::text[]) = 0 OR d.kind = ANY($6)) \
           AND (cardinality($7::text[]) = 0 OR d.source = ANY($7)) {markup} \
         ORDER BY rank DESC, c.published_at DESC LIMIT 300",
        markup = markup_condition(9, 10)
    ))
    .bind(&filter.text)
    .bind(&about)
    .bind(&uids)
    .bind(filter.from)
    .bind(filter.to)
    .bind(&kinds)
    .bind(&filter.sources)
    .bind(&tickers)
    .bind(&filter.topics)
    .bind(filter.min_importance)
    .fetch_all(storage.pool())
    .await?;
    Ok(rows.into_iter().map(hit).collect())
}

/// Кандидаты по смыслу: ближайшие векторы фрагментов.
async fn vector_candidates(
    storage: &PostgresStorage,
    filter: &Filter,
    vectors: &Vectors,
) -> Result<Vec<Hit>> {
    let Some(query) = vectors.embed_query(&filter.text).await? else {
        return Ok(Vec::new());
    };
    let (uids, tickers, about) = subject_query(&filter.subjects);
    let kinds: Vec<&str> = filter.kinds.iter().map(|k| k.as_str()).collect();
    let dimensions = vectors.dimensions();
    let mut transaction = storage.pool().begin().await?;
    sqlx::query("SET LOCAL hnsw.ef_search = 200")
        .execute(&mut *transaction)
        .await?;
    // Окно времени и виды документов отсекают ближайших после обхода индекса;
    // с pgvector 0.8 обход продолжается, пока не наберётся нужное число строк.
    let iterative: bool = sqlx::query_scalar(
        "SELECT coalesce((SELECT string_to_array(extversion, '.')::int[] >= '{0,8}' FROM pg_extension WHERE extname = 'vector'), false)",
    )
    .fetch_one(&mut *transaction)
    .await?;
    if iterative {
        sqlx::query("SET LOCAL hnsw.iterative_scan = relaxed_order")
            .execute(&mut *transaction)
            .await?;
    }
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "SELECT {HIT_COLUMNS}, left(c.text, 400) AS snippet, \
           (1 - ((e.embedding::halfvec({dimensions})) <=> $1::text::halfvec({dimensions})))::real AS rank, \
           (d.instruments && $2 OR d.tickers && $8 OR ($3 <> '' AND c.search @@ to_tsquery('russian', $3))) AS about \
         FROM document_embeddings e \
         JOIN document_chunks c ON c.document_id = e.document_id AND c.ord = e.ord \
         JOIN documents d ON d.id = e.document_id \
         WHERE e.model = {model} AND c.published_at BETWEEN $4 AND $5 \
           AND (cardinality($6::text[]) = 0 OR d.kind = ANY($6)) \
           AND (cardinality($7::text[]) = 0 OR d.source = ANY($7)) \
           AND ($3 = '' OR d.instruments && $2 OR d.tickers && $8 OR c.search @@ to_tsquery('russian', $3)) {markup} \
         ORDER BY (e.embedding::halfvec({dimensions})) <=> $1::text::halfvec({dimensions}) LIMIT 100",
        markup = markup_condition(9, 10),
        model = vectors.model_sql()?
    ))
    .bind(query)
    .bind(&uids)
    .bind(&about)
    .bind(filter.from)
    .bind(filter.to)
    .bind(&kinds)
    .bind(&filter.sources)
    .bind(&tickers)
    .bind(&filter.topics)
    .bind(filter.min_importance)
    .fetch_all(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(rows.into_iter().map(hit).collect())
}

/// Лучший фрагмент каждого кластера в порядке списка.
fn by_cluster(hits: Vec<Hit>) -> Vec<Hit> {
    let mut seen = HashMap::new();
    let mut out: Vec<Hit> = Vec::new();
    for hit in hits {
        if let std::collections::hash_map::Entry::Vacant(entry) = seen.entry(hit.cluster.clone()) {
            entry.insert(out.len());
            out.push(hit);
        }
    }
    out
}

/// Объединение двух ранжированных списков (Reciprocal Rank Fusion, k = 60).
pub(crate) fn fuse(lists: Vec<Vec<Hit>>) -> Vec<Hit> {
    let mut merged: HashMap<String, Hit> = HashMap::new();
    for list in lists {
        for (position, hit) in by_cluster(list).into_iter().enumerate() {
            let score = 1.0 / (60.0 + position as f64 + 1.0);
            merged
                .entry(hit.cluster.clone())
                .and_modify(|known| {
                    known.score += score;
                    known.about |= hit.about;
                })
                .or_insert(Hit { score, ..hit });
        }
    }
    let mut out: Vec<Hit> = merged.into_values().collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    out
}

/// Все источники каждого кластера.
async fn attach_sources(storage: &PostgresStorage, hits: &mut [Hit]) -> Result<()> {
    let clusters: Vec<&str> = hits.iter().map(|h| h.cluster.as_str()).collect();
    let rows: Vec<(String, Vec<String>)> = sqlx::query_as(
        "SELECT cluster_id, array_agg(DISTINCT source ORDER BY source) FROM documents WHERE cluster_id = ANY($1) GROUP BY 1",
    )
    .bind(&clusters)
    .fetch_all(storage.pool())
    .await?;
    let sources: HashMap<String, Vec<String>> = rows.into_iter().collect();
    for hit in hits {
        hit.sources = sources
            .get(&hit.cluster)
            .cloned()
            .unwrap_or_else(|| vec![hit.source.clone()]);
    }
    Ok(())
}

/// События по запросу, лучшие сверху.
pub(crate) async fn search(
    storage: &PostgresStorage,
    filter: &Filter,
    vectors: Option<&Vectors>,
) -> Result<Vec<Hit>> {
    let now = Utc::now();
    let text = text_candidates(storage, filter).await?;
    let mut hits = match vectors.filter(|_| !filter.text.trim().is_empty()) {
        Some(vectors) => {
            // Сервис векторов недоступен — поиск остаётся полнотекстовым.
            let semantic = vector_candidates(storage, filter, vectors)
                .await
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, "semantic search failed; full-text results only");
                    Vec::new()
                });
            fuse(vec![text, semantic])
        }
        None => fuse(vec![text]),
    };
    attach_sources(storage, &mut hits).await?;
    for hit in &mut hits {
        hit.score *= boost(hit, now);
    }
    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    hits.truncate(filter.limit);
    Ok(hits)
}

/// Страница текста документа для модели.
pub(crate) const PAGE_CHARS: usize = 8000;

/// Документ целиком по страницам; таблицы — на первой странице.
pub(crate) async fn read(
    storage: &PostgresStorage,
    id: &str,
    page: usize,
) -> Result<Option<Value>> {
    type DocumentRow = (
        String,
        String,
        String,
        String,
        String,
        String,
        Value,
        DateTime<Utc>,
        Vec<String>,
        Option<String>,
        String,
    );
    let row: Option<DocumentRow> = sqlx::query_as(
        "SELECT id, source, kind, title, lead, body, tables, published_at, tickers, summary, cluster_id FROM documents WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(storage.pool())
    .await?;
    let Some((id, source, kind, title, lead, body, tables, published, tickers, summary, cluster)) =
        row
    else {
        return Ok(None);
    };
    let chars: Vec<char> = body.chars().collect();
    let pages = chars.len().div_ceil(PAGE_CHARS).max(1);
    let page = page.clamp(1, pages);
    let text: String = chars
        .iter()
        .skip((page - 1) * PAGE_CHARS)
        .take(PAGE_CHARS)
        .collect();
    let others: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, source FROM documents WHERE cluster_id = $1 AND id <> $2 ORDER BY published_at LIMIT 10",
    )
    .bind(&cluster)
    .bind(&id)
    .fetch_all(storage.pool())
    .await?;
    let mut value = json!({
        "id": id, "source": source, "kind": kind, "title": title, "time": published,
        "tickers": tickers, "page": page, "pages": pages, "text": text,
        "same_event": others.into_iter().map(|(id, source)| json!({"id": id, "source": source})).collect::<Vec<_>>(),
    });
    if page == 1 {
        if !lead.is_empty() {
            value["lead"] = json!(lead);
        }
        if tables.as_array().is_some_and(|t| !t.is_empty()) {
            value["tables"] = tables;
        }
        if let Some(summary) = summary {
            value["summary"] = json!(summary);
        }
    }
    Ok(Some(value))
}

/// Сводка событий для начала решения: о бумагах блока, важные и раскрытия
/// эмитентов с момента `since`; сначала о бумагах блока.
pub(crate) async fn digest(
    storage: &PostgresStorage,
    subjects: &[Subject],
    since: DateTime<Utc>,
    limit: usize,
) -> Result<(Vec<Hit>, i64)> {
    let now = Utc::now();
    let (uids, tickers, about) = subject_query(subjects);
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "WITH q AS (SELECT CASE WHEN $2 = '' THEN NULL ELSE to_tsquery('russian', $2) END AS about_q) \
         SELECT {HIT_COLUMNS}, left(c.text, 300) AS snippet, 1.0::real AS rank, \
           (d.instruments && $3 OR d.tickers && $4 OR (q.about_q IS NOT NULL AND c.search @@ q.about_q)) AS about \
         FROM documents d JOIN document_chunks c ON c.document_id = d.id AND c.ord = 0, q \
         WHERE d.published_at >= $1 AND d.status <> 'skipped' \
         ORDER BY d.published_at DESC LIMIT 3000"
    ))
    .bind(since)
    .bind(&about)
    .bind(&uids)
    .bind(&tickers)
    .fetch_all(storage.pool())
    .await?;
    let total = i64::try_from(rows.len()).unwrap_or(i64::MAX);
    let mut hits = by_cluster(rows.into_iter().map(hit).collect());
    attach_sources(storage, &mut hits).await?;
    let exposures = Exposures::load(storage, &tickers).await?;
    for hit in &mut hits {
        hit.impacts = exposures.impacts(&hit.topics);
    }
    hits.retain(keep_in_digest);
    for hit in &mut hits {
        let reach: f64 = hit.impacts.iter().map(|i| i.value.abs()).sum();
        hit.score = boost(hit, now) * (1.0 + reach.min(3.0));
    }
    hits.sort_by(|a, b| b.about.cmp(&a.about).then(b.score.total_cmp(&a.score)));
    hits.truncate(limit);
    Ok((hits, total))
}

/// Событие в сводке: о бумагах блока; заметно влияет на них через темы; важное
/// по разметке или отмечено источником; сообщение регулятора; три и более
/// источников. Мелочь по разметке (важность 1) — только если о бумагах блока.
fn keep_in_digest(hit: &Hit) -> bool {
    if hit.importance == Some(1) && !hit.about {
        return false;
    }
    hit.about
        || (!hit.impacts.is_empty() && hit.importance.is_none_or(|i| i >= 2))
        || hit.priority
        || hit.importance.is_some_and(|i| i >= 3)
        || hit.kind == "regulator"
        || hit.sources.len() >= 3
}

/// События о бумагах блока с момента `since` (для внеочередного решения).
pub(crate) async fn about(
    storage: &PostgresStorage,
    subjects: &[Subject],
    since: DateTime<Utc>,
) -> Result<Vec<Hit>> {
    let (mut hits, _) = digest(storage, subjects, since, 200).await?;
    hits.retain(|h| h.about);
    Ok(hits)
}

/// Свежесть последнего документа: сборщик работает, если новые документы
/// появлялись недавно.
pub(crate) async fn newest(storage: &PostgresStorage) -> Result<Option<DateTime<Utc>>> {
    Ok(sqlx::query_scalar("SELECT max(fetched_at) FROM documents")
        .fetch_one(storage.pool())
        .await?)
}

/// Окно поиска по умолчанию.
pub(crate) fn default_window(hours: u32) -> (DateTime<Utc>, DateTime<Utc>) {
    let now = Utc::now();
    (
        now - Duration::hours(i64::from(hours)),
        now + Duration::minutes(5),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(cluster: &str, kind: &str, hours: i64, about: bool, sources: usize) -> Hit {
        let now = DateTime::from_timestamp(1_792_390_000, 0).unwrap();
        Hit {
            id: format!("{cluster}-doc"),
            cluster: cluster.to_owned(),
            time: now - Duration::hours(hours),
            source: "interfax".to_owned(),
            sources: (0..sources).map(|i| format!("s{i}")).collect(),
            kind: kind.to_owned(),
            title: cluster.to_owned(),
            url: String::new(),
            tickers: Vec::new(),
            priority: false,
            importance: None,
            summary: None,
            event_type: None,
            topics: Value::Null,
            snippet: String::new(),
            about,
            impacts: Vec::new(),
            score: 1.0,
        }
    }

    #[test]
    fn fusion_rewards_agreement_and_one_event_appears_once() {
        let text = vec![
            sample("a", "news", 1, false, 1),
            sample("b", "news", 1, false, 1),
            sample("a", "news", 2, false, 1),
        ];
        let semantic = vec![
            sample("b", "news", 1, true, 1),
            sample("c", "news", 1, false, 1),
        ];
        let fused = fuse(vec![text, semantic]);
        assert_eq!(
            fused.iter().map(|h| h.cluster.as_str()).collect::<Vec<_>>(),
            vec!["b", "a", "c"]
        );
        assert!(
            fused[0].about,
            "a match on either side marks the event as about the block"
        );
    }

    #[test]
    fn disclosures_fresh_events_and_wide_coverage_rank_higher() {
        let now = DateTime::from_timestamp(1_792_390_000, 0).unwrap();
        let fresh = boost(&sample("x", "news", 1, false, 1), now);
        let old = boost(&sample("x", "news", 24 * 7, false, 1), now);
        assert!(fresh > old && old > 0.5);
        assert!(boost(&sample("x", "disclosure", 1, false, 1), now) > fresh);
        assert!(
            boost(&sample("x", "news", 1, false, 3), now) > fresh,
            "three agencies report it"
        );
        assert!(boost(&sample("x", "news", 1, true, 1), now) > fresh);
    }

    /// Сервис векторов с признаками «сырьё», «банки», «ставка»: отвечает на
    /// любое число запросов. Возвращает адрес.
    async fn feature_vectors() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = Vec::new();
                let mut chunk = [0_u8; 8192];
                let body: Value = loop {
                    let n = socket.read(&mut chunk).await.unwrap();
                    buffer.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buffer).into_owned();
                    if let Some(split) = text.find("\r\n\r\n") {
                        let length: usize = text[..split]
                            .to_lowercase()
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .map_or(0, |v| v.trim().parse().unwrap());
                        if buffer.len() >= split + 4 + length {
                            break serde_json::from_slice(&buffer[split + 4..split + 4 + length])
                                .unwrap();
                        }
                    }
                };
                let data: Vec<Value> = body["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .enumerate()
                    .map(|(index, text)| {
                        let text = text.as_str().unwrap().to_lowercase();
                        let has = |words: &[&str]| {
                            f64::from(u8::from(words.iter().any(|w| text.contains(w))))
                        };
                        json!({"index": index, "embedding": [
                            has(&["нефт", "добыч", "углеводород", "опек"]),
                            has(&["банк", "кредит"]),
                            has(&["ставк"]),
                            0.05
                        ]})
                    })
                    .collect();
                let payload = json!({"data": data, "usage": {"prompt_tokens": 10}}).to_string();
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        url
    }

    /// Векторная часть поиска на настоящем `PostgreSQL` с pgvector: схема и
    /// индекс модели, векторы новых фрагментов, объединение одного события от
    /// разных агентств и находка по смыслу без общих слов с запросом.
    #[tokio::test]
    #[ignore = "requires DOCUMENTS_TEST_DATABASE_URL: an isolated PostgreSQL with the vector extension"]
    async fn semantic_search_finds_events_without_shared_words() {
        let storage =
            PostgresStorage::connect(&std::env::var("DOCUMENTS_TEST_DATABASE_URL").unwrap(), 2)
                .await
                .unwrap();
        storage.migrate().await.unwrap();
        let sources: super::super::sources::Sources = toml::from_str(&format!(
            "[embeddings]\nurl = \"{}\"\nmodel = \"test-features\"\ndimensions = 4",
            feature_vectors().await
        ))
        .unwrap();
        let vectors = Vectors::from_sources(&sources).unwrap();
        assert!(
            vectors.ensure_schema(&storage).await.unwrap(),
            "pgvector is not installed"
        );
        // The database may hold other documents: only these three, the newest
        // chunks, get vectors of the test model.
        let cleanup = "DELETE FROM documents WHERE id LIKE 'test:semantic:%'; \
                       DELETE FROM document_embeddings WHERE model = 'test-features'";
        sqlx::raw_sql(cleanup)
            .execute(storage.pool())
            .await
            .unwrap();
        let now = Utc::now();
        let document = |id: &str, source: &str, title: &str, body: &str, seconds: i64| {
            let mut document = super::super::Document::new(
                format!("test:semantic:{id}"),
                source,
                Kind::News,
                title,
                now - Duration::seconds(seconds),
            );
            document.body = body.to_owned();
            document
        };
        super::super::store(
            &storage,
            &[
                document(
                    "tax",
                    "interfax",
                    "Минфин изменит налог для нефтяных компаний",
                    "Правительство обсуждает НДПИ для нефтяников.",
                    30,
                ),
                document(
                    "opec",
                    "tass",
                    "ОПЕК+ договорилась о сокращении добычи",
                    "Страны альянса сократят добычу с декабря.",
                    20,
                ),
                document(
                    "loans",
                    "interfax",
                    "Сбербанк увеличил выдачу кредитов",
                    "Банк отчитался о росте кредитного портфеля.",
                    10,
                ),
            ],
        )
        .await
        .unwrap();
        let (stored, usage) = vectors.backfill(&storage, 7, 3).await.unwrap();
        assert!(stored == 3 && usage.input_tokens > 0, "{stored} vectors");
        let clusters: HashMap<String, String> =
            sqlx::query_as("SELECT id, cluster_id FROM documents WHERE id LIKE 'test:semantic:%'")
                .fetch_all(storage.pool())
                .await
                .unwrap()
                .into_iter()
                .collect();
        assert_eq!(
            clusters["test:semantic:tax"], clusters["test:semantic:opec"],
            "close meaning from another agency within hours joins one event"
        );
        assert_ne!(
            clusters["test:semantic:tax"],
            clusters["test:semantic:loans"]
        );
        let filter = Filter {
            text: "углеводороды".to_owned(),
            subjects: Vec::new(),
            kinds: Vec::new(),
            sources: Vec::new(),
            topics: Vec::new(),
            min_importance: None,
            from: now - Duration::days(2),
            to: now + Duration::hours(1),
            limit: 10,
        };
        assert!(
            text_candidates(&storage, &filter)
                .await
                .unwrap()
                .iter()
                .all(|hit| !hit.id.starts_with("test:semantic:")),
            "no shared words with the query"
        );
        let semantic = vector_candidates(&storage, &filter, &vectors)
            .await
            .unwrap();
        assert_eq!(
            semantic[0].cluster,
            clusters["test:semantic:tax"],
            "{:?}",
            semantic.iter().map(|h| &h.id).collect::<Vec<_>>()
        );
        let hits = search(&storage, &filter, Some(&vectors)).await.unwrap();
        assert_eq!(hits[0].cluster, clusters["test:semantic:tax"]);
        assert_eq!(
            hits[0].sources,
            ["interfax", "tass"],
            "one event, both agencies"
        );
        sqlx::raw_sql(cleanup)
            .execute(storage.pool())
            .await
            .unwrap();
    }
}
