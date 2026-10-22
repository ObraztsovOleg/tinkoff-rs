//! RSS 2.0 и Atom: заголовки и анонсы лент новостей.
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};
use quick_xml::{Reader, events::Event as XmlEvent};
use serde::Serialize;

use super::{Document, Kind};
use crate::ai::truncate;

/// Новость из RSS/Atom-ленты.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct NewsItem {
    pub title: String,
    pub link: String,
    pub published: DateTime<Utc>,
    pub source: String,
    pub summary: String,
    /// Полный HTML описания или `content:encoded`, если лента его даёт.
    #[serde(skip)]
    pub html: String,
}

pub(crate) fn strip_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut tag = false;
    for c in text.chars() {
        match c {
            '<' => tag = true,
            '>' if tag => {
                tag = false;
                out.push(' ');
            }
            _ if !tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn parse_date(text: &str) -> Option<DateTime<Utc>> {
    let text = text.trim();
    DateTime::parse_from_rfc2822(text)
        .or_else(|_| DateTime::parse_from_rfc3339(text))
        .ok()
        .map(|date| date.with_timezone(&Utc))
}

/// Поля одного `<item>`/`<entry>` по мере чтения.
#[derive(Default)]
struct Fields {
    title: String,
    link: String,
    date: String,
    summary: String,
    content: String,
}

/// Анонс и полный текст ленты ограничены: лента не должна раздуть память.
const SUMMARY_LIMIT: usize = 100_000;
const CONTENT_LIMIT: usize = 200_000;

impl Fields {
    fn append(&mut self, field: &str, value: &str) {
        match field {
            "title" => self.title.push_str(value),
            "link" | "guid" if self.link.is_empty() && value.starts_with("http") => {
                self.link.push_str(value);
            }
            "pubdate" | "published" | "updated" | "date" if self.date.is_empty() => {
                self.date.push_str(value);
            }
            "description" | "summary" if self.summary.len() < SUMMARY_LIMIT => {
                self.summary.push_str(value);
            }
            "content" | "encoded" if self.content.len() < CONTENT_LIMIT => {
                self.content.push_str(value);
            }
            _ => {}
        }
    }
}

/// Разбирает RSS 2.0 (`<item>`) и Atom (`<entry>`).
pub(crate) fn parse_feed(xml: &str, source: &str) -> Vec<NewsItem> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut items = Vec::new();
    let (mut inside, mut field) = (false, String::new());
    // Только прямые потомки <item>/<entry>: вложенные элементы расширений лент
    // (например, связанные новости РБК) тоже бывают <title>.
    let (mut depth, mut item_depth) = (0_usize, 0_usize);
    let mut fields = Fields::default();
    loop {
        match reader.read_event() {
            Ok(event @ (XmlEvent::Start(_) | XmlEvent::Empty(_))) => {
                let (XmlEvent::Start(element) | XmlEvent::Empty(element)) = &event else {
                    unreachable!("matched above")
                };
                let level = depth + 1;
                if matches!(event, XmlEvent::Start(_)) {
                    depth = level;
                }
                let name = String::from_utf8_lossy(element.local_name().as_ref()).to_lowercase();
                if !inside && (name == "item" || name == "entry") {
                    inside = true;
                    item_depth = level;
                    fields = Fields::default();
                } else if !inside || level != item_depth + 1 {
                    field.clear();
                    continue;
                } else if name == "link" && fields.link.is_empty() {
                    // Atom хранит ссылку в атрибуте href.
                    if let Some(href) = element
                        .attributes()
                        .flatten()
                        .find(|attribute| attribute.key.local_name().as_ref() == b"href")
                    {
                        fields.link = String::from_utf8_lossy(&href.value).into_owned();
                    }
                }
                field = name;
            }
            Ok(XmlEvent::Text(text)) if inside => {
                let value = text
                    .unescape()
                    .map(std::borrow::Cow::into_owned)
                    .unwrap_or_default();
                fields.append(&field, &value);
            }
            Ok(XmlEvent::CData(text)) if inside => {
                fields.append(&field, &String::from_utf8_lossy(&text));
            }
            Ok(XmlEvent::End(element)) => {
                let name = String::from_utf8_lossy(element.local_name().as_ref()).to_lowercase();
                let level = depth;
                depth = depth.saturating_sub(1);
                if (name == "item" || name == "entry") && inside && level == item_depth {
                    inside = false;
                    let item = std::mem::take(&mut fields);
                    if let Some(published) = parse_date(&item.date) {
                        let title = strip_html(&item.title);
                        if !title.is_empty() {
                            items.push(NewsItem {
                                title: truncate(&title, 300),
                                link: item.link.trim().to_owned(),
                                published,
                                source: source.to_owned(),
                                summary: truncate(&strip_html(&item.summary), 400),
                                html: if item.content.trim().is_empty() {
                                    item.summary
                                } else {
                                    item.content
                                },
                            });
                        }
                    }
                }
                field.clear();
            }
            Ok(XmlEvent::Eof) | Err(_) => break,
            _ => {}
        }
    }
    items
}

pub(crate) fn host(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(url)
        .trim_start_matches("www.")
        .to_owned()
}

/// Загружает все ленты параллельно; недоступная лента не мешает остальным.
pub(crate) async fn fetch_news(feeds: &[String], hours: u32) -> (Vec<NewsItem>, Vec<String>) {
    let client = match reqwest::Client::builder()
        .timeout(StdDuration::from_secs(20))
        .user_agent("Mozilla/5.0 (compatible; tinkoff-rs-news/0.3)")
        .build()
    {
        Ok(client) => client,
        Err(error) => return (Vec::new(), vec![format!("http client: {error}")]),
    };
    let since = Utc::now() - Duration::hours(i64::from(hours));
    let requests = feeds.iter().map(|url| {
        let client = client.clone();
        async move {
            // Одна повторная попытка: защита сайтов от ботов иногда рвёт соединение.
            let mut result = Err(None);
            for attempt in 0..2 {
                if attempt > 0 {
                    tokio::time::sleep(StdDuration::from_secs(2)).await;
                }
                result = async {
                    let response = client.get(url).send().await?.error_for_status()?;
                    response.text().await
                }
                .await
                .map_err(Some);
                if result.is_ok() {
                    break;
                }
            }
            (url, result)
        }
    });
    let (mut items, mut errors) = (Vec::new(), Vec::new());
    for (url, result) in futures::future::join_all(requests).await {
        match result {
            Ok(xml) => items.extend(
                parse_feed(&xml, &host(url))
                    .into_iter()
                    .filter(|item| item.published >= since),
            ),
            Err(error) => errors.push(format!(
                "{}: {}",
                host(url),
                error.map_or_else(|| "no response".to_owned(), |e| e.without_url().to_string())
            )),
        }
    }
    items.sort_by(|a, b| b.published.cmp(&a.published));
    items.dedup_by(|a, b| a.title == b.title);
    items.truncate(400);
    (items, errors)
}

/// Стабильный ID новости ленты по источнику и заголовку (совпадает с историей
/// версий до появления документов).
pub(crate) fn news_id(item: &NewsItem) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in item.source.bytes().chain([0]).chain(item.title.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("rss:{hash:016x}")
}

/// Новость ленты как документ; полный текст статьи добавляет сборщик.
pub(crate) fn to_document(item: &NewsItem, kind: Kind) -> Document {
    let mut document = Document::new(
        news_id(item),
        &item.source,
        kind,
        &item.title,
        item.published,
    );
    document.url.clone_from(&item.link);
    document.lead.clone_from(&item.summary);
    document
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rss_and_atom_items_become_news() {
        let rss = r#"<?xml version="1.0"?><rss><channel><title>Feed</title>
            <item><title><![CDATA[Сбербанк повысил <b>прогноз</b>]]></title><link>https://example.ru/1</link>
            <pubDate>Mon, 19 Oct 2026 19:52:00 +0300</pubDate><description>&lt;p&gt;Банк ждёт роста&lt;/p&gt;</description></item>
            <item><title>Без даты</title><link>https://example.ru/2</link></item>
            <item><title>Главная</title><pubDate>Mon, 19 Oct 2026 19:53:00 +0300</pubDate>
            <rbc_news:related><rbc_news:item><title>Связанная</title></rbc_news:item></rbc_news:related></item></channel></rss>"#;
        let items = parse_feed(rss, "example.ru");
        assert_eq!(items.len(), 2, "an item without a date is skipped");
        assert_eq!(
            items[1].title, "Главная",
            "nested titles of feed extensions are ignored"
        );
        assert_eq!(items[0].title, "Сбербанк повысил прогноз");
        assert_eq!(items[0].summary, "Банк ждёт роста");
        assert_eq!(items[0].published.to_rfc3339(), "2026-10-19T16:52:00+00:00");
        let atom = r#"<feed xmlns="http://www.w3.org/2005/Atom"><entry><title>Газпром</title>
            <link href="https://example.ru/a"/><updated>2026-10-19T10:00:00Z</updated><summary>Итоги</summary></entry></feed>"#;
        let items = parse_feed(atom, "atom");
        assert_eq!(
            (items[0].link.as_str(), items[0].summary.as_str()),
            ("https://example.ru/a", "Итоги")
        );
    }
}
