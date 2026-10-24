//! Сборщик документов (`tinkoff-bot collect`): один процесс на стенд.
//!
//! Раз в полминуты проверяет, каким источникам пора: новости T-Invest (каждые
//! 2 минуты), деловые RSS с полными статьями, сообщения биржи и Банка России.
//! Каждый источник независим: сбой одного записывается в `collector_state` и
//! не мешает остальным. Если настроен сервис векторов, сборщик дописывает
//! векторы новых фрагментов в пределах дневного лимита расходов.
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};
use tinkoff_rs::{Error, Result, storage::PostgresStorage};
use tokio::sync::watch;
use tracing::{info, warn};

use super::{
    Kind, cbr,
    embed::Vectors,
    exposures, facts, issuers, moex,
    rss::{NewsItem, parse_feed, to_document},
    sources::Sources,
    store, tagging, tinvest,
    web::Fetcher,
};
use crate::{VenueApi, ai::usage, resilience};

/// Как часто сборщик просыпается.
const TICK: StdDuration = StdDuration::from_secs(30);
/// Полных статей за один проход: остальные — на следующих.
const ARTICLES_PER_TICK: usize = 30;
/// Векторов за один проход.
const EMBEDDINGS_PER_TICK: usize = 256;
/// Ключ advisory lock: на стенде работает один сборщик.
const COLLECTOR_LOCK: i64 = 0x7469_6e6b_646f_6373;

/// Результат прохода одного источника.
struct Pass {
    added: usize,
    cursor: Option<String>,
    errors: Vec<String>,
}

async fn state(
    storage: &PostgresStorage,
    source: &str,
) -> Result<(Option<DateTime<Utc>>, Option<String>)> {
    let row: Option<(Option<DateTime<Utc>>, Option<String>)> =
        sqlx::query_as("SELECT last_run, cursor FROM collector_state WHERE source = $1")
            .bind(source)
            .fetch_optional(storage.pool())
            .await?;
    Ok(row.unwrap_or((None, None)))
}

async fn record(storage: &PostgresStorage, source: &str, result: &Result<Pass>) -> Result<()> {
    let (added, cursor, error) = match result {
        Ok(pass) => (
            pass.added,
            pass.cursor.clone(),
            (!pass.errors.is_empty()).then(|| pass.errors.join("; ")),
        ),
        Err(error) => (0, None, Some(error.to_string())),
    };
    let succeeded = result.is_ok();
    sqlx::query(
        "INSERT INTO collector_state(source,cursor,last_run,last_success,last_error,added_total,added_last) \
         VALUES($1,$2,now(),CASE WHEN $3 THEN now() END,$4,$5,$6) \
         ON CONFLICT (source) DO UPDATE SET cursor=coalesce(EXCLUDED.cursor,collector_state.cursor),last_run=now(), \
           last_success=CASE WHEN $3 THEN now() ELSE collector_state.last_success END,last_error=EXCLUDED.last_error, \
           added_total=collector_state.added_total+EXCLUDED.added_last,added_last=EXCLUDED.added_last",
    )
    .bind(source)
    .bind(cursor)
    .bind(succeeded)
    .bind(error.map(|e| e.chars().take(1000).collect::<String>()))
    .bind(i64::try_from(added).unwrap_or(i64::MAX))
    .bind(i32::try_from(added).unwrap_or(i32::MAX))
    .execute(storage.pool())
    .await?;
    Ok(())
}

/// Новые статьи деловых лент: анонс из ленты, текст — со страницы статьи.
async fn rss(fetcher: &mut Fetcher, storage: &PostgresStorage, sources: &Sources) -> Result<Pass> {
    let since = Utc::now() - Duration::hours(i64::from(sources.tinvest.backfill_hours));
    let mut items: Vec<NewsItem> = Vec::new();
    let mut errors = Vec::new();
    for feed in &sources.rss.feeds {
        match fetcher.get_text(feed, &sources.web).await {
            Ok(Some(xml)) => items.extend(
                parse_feed(&xml, &super::web::host(feed))
                    .into_iter()
                    .filter(|item| item.published >= since),
            ),
            Ok(None) => errors.push(format!("{}: unavailable", super::web::host(feed))),
            Err(error) => errors.push(error.to_string()),
        }
    }
    items.sort_by_key(|item| std::cmp::Reverse(item.published));
    let ids: Vec<String> = items.iter().map(super::rss::news_id).collect();
    let known: Vec<String> = sqlx::query_scalar("SELECT id FROM documents WHERE id = ANY($1)")
        .bind(&ids)
        .fetch_all(storage.pool())
        .await?;
    let mut added = 0;
    let mut pages = 0;
    for item in items
        .iter()
        .filter(|item| !known.contains(&super::rss::news_id(item)))
    {
        let mut document = to_document(item, Kind::Article);
        if sources.rss.full_text && !item.link.is_empty() {
            if pages >= ARTICLES_PER_TICK {
                break;
            }
            pages += 1;
            match fetcher.article(&item.link, &sources.web).await {
                Ok(Some(page)) => {
                    document.body = page.body;
                    if document.lead.is_empty() {
                        document.lead = page.description;
                    }
                }
                // Страница закрыта robots.txt, платная или недоступна — остаётся анонс.
                Ok(None) => {}
                Err(error) => errors.push(error.to_string()),
            }
        }
        added += store(storage, std::slice::from_ref(&document)).await?;
    }
    errors.dedup();
    Ok(Pass {
        added,
        cursor: None,
        errors,
    })
}

/// Один источник, если пришло его время.
async fn due(
    storage: &PostgresStorage,
    source: &str,
    interval: u64,
) -> Result<(bool, Option<String>)> {
    let (last_run, cursor) = state(storage, source).await?;
    let interval = Duration::seconds(i64::try_from(interval).unwrap_or(i64::MAX));
    Ok((
        last_run.is_none_or(|last| Utc::now() - last >= interval),
        cursor,
    ))
}

/// Один проход по всем источникам, которым пора.
async fn pass(
    api: Option<&VenueApi>,
    storage: &PostgresStorage,
    fetcher: &mut Fetcher,
    sources: &Sources,
) -> Result<()> {
    if let Some(api) = api.filter(|_| sources.tinvest.enabled) {
        let (ready, cursor) = due(storage, "tinvest", sources.tinvest.interval_seconds).await?;
        if ready {
            let saved = cursor.and_then(|c| c.parse::<i64>().ok());
            let result = tinvest::collect(api, storage, &sources.tinvest, saved)
                .await
                .map(|(added, backfill)| Pass {
                    added,
                    // Пустая строка — история догружена.
                    cursor: Some(backfill.map(|c| c.to_string()).unwrap_or_default()),
                    errors: Vec::new(),
                });
            finish(storage, "tinvest", result).await?;
        }
    }
    if sources.rss.enabled && due(storage, "rss", sources.rss.interval_seconds).await?.0 {
        let result = rss(fetcher, storage, sources).await;
        finish(storage, "rss", result).await?;
    }
    if sources.moex.enabled && due(storage, "moex", sources.moex.interval_seconds).await?.0 {
        let result = moex::collect(
            fetcher,
            storage,
            &sources.web,
            sources.tinvest.backfill_hours,
        )
        .await
        .map(|added| Pass {
            added,
            cursor: None,
            errors: Vec::new(),
        });
        finish(storage, "moex", result).await?;
    }
    if sources.issuers.enabled
        && due(storage, "issuers", sources.issuers.interval_seconds)
            .await?
            .0
    {
        let result = issuers::collect(fetcher, storage, &sources.issuers, &sources.web)
            .await
            .map(|(added, errors)| Pass {
                added,
                cursor: None,
                errors,
            });
        finish(storage, "issuers", result).await?;
    }
    if due(storage, "retention", 86_400).await?.0 {
        let result = retention(storage, sources).await;
        finish(storage, "retention", result).await?;
    }
    if sources.cbr.enabled && due(storage, "cbr", sources.cbr.interval_seconds).await?.0 {
        let result = cbr::collect(
            fetcher,
            storage,
            &sources.cbr,
            &sources.web,
            sources.tinvest.backfill_hours.max(24 * 14),
        )
        .await
        .map(|(added, errors)| Pass {
            added,
            cursor: None,
            errors,
        });
        finish(storage, "cbr", result).await?;
    }
    Ok(())
}

async fn finish(storage: &PostgresStorage, source: &str, result: Result<Pass>) -> Result<()> {
    match &result {
        Ok(pass) if pass.added > 0 || !pass.errors.is_empty() => {
            info!(source, added = pass.added, errors = %pass.errors.join("; "), "documents collected");
        }
        Ok(_) => {}
        Err(error) => warn!(source, %error, "document source failed; retrying on schedule"),
    }
    record(storage, source, &result).await
}

/// Сроки хранения раз в сутки: старые новости и статьи удаляются целиком, у
/// документов, признанных не относящимися к рынку, остаётся только заголовок,
/// векторы старше срока удаляются (поиск по ним остаётся полнотекстовым).
async fn retention(storage: &PostgresStorage, sources: &Sources) -> Result<Pass> {
    let days = |value: u32| i32::try_from(value).unwrap_or(i32::MAX);
    let removed: i64 = sqlx::query_scalar(
        "WITH gone AS (DELETE FROM documents WHERE kind IN ('news','article','exchange') \
           AND published_at < now() - make_interval(days => $1) RETURNING 1) SELECT count(*) FROM gone",
    )
    .bind(days(sources.retention.news_days))
    .fetch_one(storage.pool())
    .await?;
    let dropped: i64 = sqlx::query_scalar(
        "WITH dropped AS (UPDATE documents SET body = '', tables = '[]', body_dropped_at = now() \
           WHERE status = 'skipped' AND body_dropped_at IS NULL AND published_at < now() - make_interval(days => $1) RETURNING id), \
         chunks AS (DELETE FROM document_chunks c USING dropped d WHERE c.document_id = d.id AND c.ord > 0) \
         SELECT count(*) FROM dropped",
    )
    .bind(days(sources.retention.irrelevant_days))
    .fetch_one(storage.pool())
    .await?;
    let has_vectors: bool =
        sqlx::query_scalar("SELECT to_regclass('document_embeddings') IS NOT NULL")
            .fetch_one(storage.pool())
            .await?;
    let vectors = if has_vectors {
        sqlx::query(
            "DELETE FROM document_embeddings WHERE created_at < now() - make_interval(days => $1)",
        )
        .bind(days(sources.retention.embeddings_days))
        .execute(storage.pool())
        .await?
        .rows_affected()
    } else {
        0
    };
    info!(removed, dropped, vectors, "document retention applied");
    Ok(Pass {
        added: 0,
        cursor: None,
        errors: Vec::new(),
    })
}

/// Разметка новых событий и черновики профилей бумаг. Без подключения к
/// модели (нет токена) ошибка записывается раз в проход и не мешает сбору.
/// Возвращает `true`, если модель недоступна (нет профиля или токена, кончились
/// средства, сбой провайдера): тогда разметка ждёт, а не повторяет ошибку каждые
/// полминуты.
async fn markup(storage: &PostgresStorage, sources: &Sources) -> Result<bool> {
    if !sources.tagging.enabled {
        return Ok(false);
    }
    let tagged = tagging::tag(storage, &sources.tagging)
        .await
        .map(|(added, errors)| Pass {
            added,
            cursor: None,
            errors,
        });
    // Ни ключа, ни денег на счёте провайдера, ни ответа: повтор через паузу.
    let unconfigured = tagged.is_err();
    if !matches!(&tagged, Ok(pass) if pass.added == 0 && pass.errors.is_empty()) {
        finish(storage, "tagging", tagged).await?;
    }
    if unconfigured {
        return Ok(true);
    }
    let figures = facts::extract(storage, &sources.tagging)
        .await
        .map(|added| Pass {
            added,
            cursor: None,
            errors: Vec::new(),
        });
    let failed = figures.is_err();
    if !matches!(&figures, Ok(pass) if pass.added == 0) {
        finish(storage, "facts", figures).await?;
    }
    if failed {
        return Ok(true);
    }
    let drafted = exposures::draft(storage, &sources.tagging)
        .await
        .map(|(added, errors)| Pass {
            added,
            cursor: None,
            errors,
        });
    let unconfigured = drafted.is_err();
    if !matches!(&drafted, Ok(pass) if pass.added == 0 && pass.errors.is_empty()) {
        finish(storage, "exposures", drafted).await?;
    }
    Ok(unconfigured)
}

/// Пауза разметки, если модель не настроена: файл профилей перечитывается
/// после неё, и подключённый ключ начинает работать без перезапуска.
const MARKUP_PAUSE: StdDuration = StdDuration::from_secs(600);

/// Векторы новых фрагментов, пока не исчерпан дневной лимит расходов.
async fn embeddings(
    storage: &PostgresStorage,
    sources: &Sources,
    ready: &mut Option<bool>,
) -> Result<()> {
    let Some(vectors) = Vectors::from_sources(sources) else {
        return Ok(());
    };
    if ready.is_none() {
        let available = vectors.ensure_schema(storage).await?;
        if !available {
            warn!("pgvector is not available in PostgreSQL; search stays full-text");
        }
        *ready = Some(available);
    }
    if *ready != Some(true) || usage::budget(storage).await?.exhausted() {
        return Ok(());
    }
    let result = vectors
        .backfill(
            storage,
            sources.retention.embeddings_days,
            EMBEDDINGS_PER_TICK,
        )
        .await;
    let outcome = match result {
        Ok((count, spent)) => {
            usage::store(storage, "embeddings", None, &vectors.spend(spent)).await?;
            Ok(Pass {
                added: count,
                cursor: None,
                errors: Vec::new(),
            })
        }
        Err(error) => Err(error),
    };
    finish(storage, "embeddings", outcome).await
}

/// Сборщик работает до сигнала остановки.
pub(crate) async fn run(api: Option<VenueApi>, storage: &PostgresStorage) -> Result<()> {
    // Отдельное соединение держит блокировку всё время работы.
    let mut lock = storage.pool().acquire().await?;
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(COLLECTOR_LOCK)
        .fetch_one(&mut *lock)
        .await?;
    if !locked {
        return Err(Error::Configuration(
            "another document collector is already running for this database".to_owned(),
        ));
    }
    let mut fetcher = Fetcher::new()?;
    let mut shutdown: watch::Receiver<bool> = resilience::shutdown_signal();
    let mut sources = Sources::default();
    let mut vectors_ready: Option<bool> = None;
    let mut markup_paused_until: Option<std::time::Instant> = None;
    info!("document collector is running");
    loop {
        match Sources::load() {
            Ok(fresh) => {
                if fresh.embeddings.as_ref().map(|e| (&e.model, e.dimensions))
                    != sources
                        .embeddings
                        .as_ref()
                        .map(|e| (&e.model, e.dimensions))
                {
                    vectors_ready = None;
                }
                sources = fresh;
            }
            Err(error) => warn!(%error, "sources file is invalid; the previous settings stay"),
        }
        if let Err(error) = pass(api.as_ref(), storage, &mut fetcher, &sources).await {
            warn!(%error, "document collection pass failed");
        }
        if markup_paused_until.is_none_or(|until| std::time::Instant::now() >= until) {
            match markup(storage, &sources).await {
                Ok(true) => markup_paused_until = Some(std::time::Instant::now() + MARKUP_PAUSE),
                Ok(false) => markup_paused_until = None,
                Err(error) => warn!(%error, "document markup pass failed"),
            }
        }
        if let Err(error) = embeddings(storage, &sources, &mut vectors_ready).await {
            warn!(%error, "embedding pass failed");
            vectors_ready = None;
        }
        tokio::select! {
            () = tokio::time::sleep(TICK) => {}
            () = resilience::shutdown_requested(&mut shutdown) => {
                info!("document collector stopped");
                return Ok(());
            }
        }
    }
}
