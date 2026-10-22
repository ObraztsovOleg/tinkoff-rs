//! Банк России: пресс-релизы и события. Лента даёт полный текст в описании.
use chrono::{Duration, Utc};
use tinkoff_rs::{Result, storage::PostgresStorage};

use super::{
    Document, Kind, fnv,
    rss::{NewsItem, parse_feed},
    sources::{Cbr, Web},
    store,
    web::{Fetcher, html_text},
};

pub(crate) fn to_document(item: &NewsItem) -> Document {
    let mut document = Document::new(
        format!("cbr:{:016x}", fnv(&[&item.link, &item.title])),
        "cbr",
        Kind::Regulator,
        &item.title,
        item.published,
    );
    document.url.clone_from(&item.link);
    document.body = html_text(&item.html);
    document
}

/// Новые сообщения всех лент Банка России. Недоступная лента не мешает
/// остальным; её ошибка возвращается для журнала.
pub(crate) async fn collect(
    fetcher: &mut Fetcher,
    storage: &PostgresStorage,
    settings: &Cbr,
    web: &Web,
    hours: u32,
) -> Result<(usize, Vec<String>)> {
    let since = Utc::now() - Duration::hours(i64::from(hours));
    let (mut added, mut errors) = (0, Vec::new());
    for feed in &settings.feeds {
        match fetcher.get_text(feed, web).await {
            Ok(Some(xml)) => {
                let documents: Vec<Document> = parse_feed(&xml, "cbr")
                    .iter()
                    .filter(|item| item.published >= since)
                    .map(to_document)
                    .collect();
                added += store(storage, &documents).await?;
            }
            Ok(None) => errors.push(format!("{feed}: unavailable")),
            Err(error) => errors.push(error.to_string()),
        }
    }
    Ok((added, errors))
}
