//! Сообщения Московской биржи из RSS сайта (полный текст в `content:encoded`):
//! начало и приостановка торгов, риск-параметры, индексы, листинг, аукционы.
use chrono::{Duration, Utc};
use tinkoff_rs::{Error, Result, storage::PostgresStorage};

use super::{
    Document, Kind,
    rss::{NewsItem, parse_feed},
    sources::Web,
    store,
    web::{Fetcher, html_text},
};

const FEED: &str = "https://www.moex.com/export/news.aspx?cat=100";

/// Номер сообщения из ссылки `https://www.moex.com/n105254`.
fn number(link: &str) -> Option<&str> {
    let tail = link.rsplit('/').next()?.strip_prefix('n')?;
    tail.chars()
        .all(|c| c.is_ascii_digit())
        .then_some(tail)
        .filter(|t| !t.is_empty())
}

pub(crate) fn to_document(item: &NewsItem) -> Document {
    let id = number(&item.link).map_or_else(
        || format!("moex:{:016x}", super::fnv(&[&item.link, &item.title])),
        |number| format!("moex:{number}"),
    );
    let mut document = Document::new(id, "moex", Kind::Exchange, &item.title, item.published);
    document.url.clone_from(&item.link);
    document.body = html_text(&item.html);
    document
}

/// Сообщения биржи не старше `hours`.
pub(crate) async fn collect(
    fetcher: &mut Fetcher,
    storage: &PostgresStorage,
    settings: &Web,
    hours: u32,
) -> Result<usize> {
    let xml = fetcher
        .get_text(FEED, settings)
        .await?
        .ok_or_else(|| Error::Configuration("moex: news feed is unavailable".to_owned()))?;
    let since = Utc::now() - Duration::hours(i64::from(hours));
    let documents: Vec<Document> = parse_feed(&xml, "moex")
        .iter()
        .filter(|item| item.published >= since)
        .map(to_document)
        .collect();
    store(storage, &documents).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exchange_notices_keep_their_number_and_table_text() {
        let xml = r"<rss><channel><item><title>Аукцион</title><link>https://www.moex.com/n105254</link>
            <pubDate>Tue, 20 Oct 2026 10:56:43 +0300</pubDate><description>кратко</description>
            <content:encoded>&lt;table&gt;&lt;tr&gt;&lt;td&gt;Срок, дней&lt;/td&gt;&lt;td&gt;181&lt;/td&gt;&lt;/tr&gt;&lt;/table&gt;</content:encoded></item></channel></rss>";
        let items = parse_feed(xml, "moex");
        let document = to_document(&items[0]);
        assert_eq!(
            (document.id.as_str(), document.kind),
            ("moex:105254", Kind::Exchange)
        );
        assert_eq!(document.body, "Срок, дней | 181");
        assert_eq!(number("https://www.moex.com/news/x"), None);
    }
}
