//! Сайты эмитентов: пресс-релизы из RSS или со страницы списка и отчётность в
//! PDF. Первый обход сайта только запоминает ссылки; документами становятся
//! публикации, появившиеся позже, — старый релиз не выглядит свежим событием.
use chrono::{DateTime, Utc};
use scraper::{Html, Selector};
use tinkoff_rs::{Error, Result, storage::PostgresStorage};

use super::{
    Document, Kind, clean, fnv,
    rss::parse_feed,
    sources::{Issuer, Issuers, Web, regex_ok},
    store,
    web::{Fetcher, host, html_text},
};

/// Новых публикаций одного сайта за проход.
const NEW_PER_PASS: usize = 10;
/// Ссылок со страницы списка.
const LINKS_PER_PAGE: usize = 60;
/// Предел PDF отчёта.
const PDF_BYTES: usize = 25_000_000;

/// Публикация на сайте: адрес, заголовок, время и текст из ленты (если есть).
#[derive(Clone, Debug)]
pub(crate) struct Link {
    pub url: String,
    pub title: String,
    pub published: Option<DateTime<Utc>>,
    pub html: String,
}

/// Ссылки на публикации со страницы списка: тот же сайт, путь по шаблону.
pub(crate) fn page_links(html: &str, base: &str, pattern: &regex::Regex) -> Vec<Link> {
    let Ok(base_url) = reqwest::Url::parse(base) else {
        return Vec::new();
    };
    let site = host(base);
    let document = Html::parse_document(html);
    let anchors = Selector::parse("a[href]").expect("static selector is valid");
    let mut out: Vec<Link> = Vec::new();
    for anchor in document.select(&anchors) {
        let Some(href) = anchor.value().attr("href") else {
            continue;
        };
        let Ok(mut url) = base_url.join(href.trim()) else {
            continue;
        };
        url.set_fragment(None);
        if host(url.as_str()) != site || !pattern.is_match(url.path()) {
            continue;
        }
        let title = clean(&anchor.text().collect::<Vec<_>>().join(" "), 300);
        let url = url.to_string();
        match out.iter_mut().find(|link| link.url == url) {
            // Одна публикация — несколько ссылок (картинка и заголовок): берём текст длиннее.
            Some(link) if link.title.chars().count() < title.chars().count() => link.title = title,
            Some(_) => {}
            None => out.push(Link {
                url,
                title,
                published: None,
                html: String::new(),
            }),
        }
        if out.len() >= LINKS_PER_PAGE {
            break;
        }
    }
    out
}

/// Текст PDF через `pdftotext` (poppler).
async fn pdf_text(bytes: Vec<u8>) -> Result<String> {
    tokio::task::spawn_blocking(move || {
        let path = std::env::temp_dir().join(format!("tinkoff-rs-{}.pdf", uuid::Uuid::new_v4()));
        std::fs::write(&path, &bytes).map_err(|e| Error::Configuration(format!("pdf: {e}")))?;
        let output = std::process::Command::new("pdftotext")
            .args(["-layout", "-q", "-enc", "UTF-8"])
            .arg(&path)
            .arg("-")
            .output();
        let _ = std::fs::remove_file(&path);
        let output =
            output.map_err(|e| Error::Configuration(format!("pdftotext is not available: {e}")))?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| clean(line, 2000))
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join("\n"))
    })
    .await
    .map_err(|e| Error::Configuration(format!("pdf: {e}")))?
}

/// Публикации сайта сейчас: из ленты или со страницы.
async fn links(fetcher: &mut Fetcher, site: &Issuer, web: &Web) -> Result<Vec<Link>> {
    if let Some(feed) = &site.feed {
        let xml = fetcher
            .get_text(feed, web)
            .await?
            .ok_or_else(|| Error::Configuration(format!("{}: feed unavailable", host(feed))))?;
        return Ok(parse_feed(&xml, &host(feed))
            .into_iter()
            .filter(|item| !item.link.is_empty())
            .map(|item| Link {
                url: item.link,
                title: item.title,
                published: Some(item.published),
                html: item.html,
            })
            .collect());
    }
    let (Some(page), Some(pattern)) = (&site.page, &site.link_pattern) else {
        return Ok(Vec::new());
    };
    let pattern =
        regex_ok(pattern).map_err(|e| Error::Configuration(format!("link_pattern: {e}")))?;
    let html = fetcher
        .get_text(page, web)
        .await?
        .ok_or_else(|| Error::Configuration(format!("{}: page unavailable", host(page))))?;
    Ok(page_links(&html, page, &pattern))
}

/// Документ публикации: текст из ленты, со страницы или из PDF.
async fn document(
    fetcher: &mut Fetcher,
    site: &Issuer,
    link: &Link,
    web: &Web,
) -> Result<Document> {
    let path = link.url.split(['?', '#']).next().unwrap_or_default();
    let pdf = std::path::Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"));
    let title = if link.title.is_empty() {
        link.url
            .rsplit('/')
            .find(|part| !part.is_empty())
            .unwrap_or("Публикация")
            .to_owned()
    } else {
        link.title.clone()
    };
    let mut document = Document::new(
        format!("ir:{:016x}", fnv(&[&link.url])),
        &host(&link.url),
        if pdf {
            Kind::Report
        } else {
            Kind::PressRelease
        },
        &title,
        link.published.unwrap_or_else(Utc::now),
    );
    document.url.clone_from(&link.url);
    document.tickers = vec![site.ticker.clone()];
    if pdf {
        if let Some(bytes) = fetcher.get_bytes(&link.url, web, PDF_BYTES).await? {
            document.body = pdf_text(bytes).await?;
        }
    } else if !link.html.trim().is_empty() && html_text(&link.html).chars().count() > 300 {
        document.body = html_text(&link.html);
    } else if let Some(page) = fetcher.article(&link.url, web).await? {
        document.body = page.body;
        document.lead = page.description;
    }
    Ok(document)
}

async fn crawl(
    fetcher: &mut Fetcher,
    storage: &PostgresStorage,
    site: &Issuer,
    web: &Web,
) -> Result<usize> {
    let found = links(fetcher, site, web).await?;
    if found.is_empty() {
        return Ok(0);
    }
    let urls: Vec<&str> = found.iter().map(|l| l.url.as_str()).collect();
    let known: Vec<String> = sqlx::query_scalar("SELECT url FROM issuer_links WHERE url = ANY($1)")
        .bind(&urls)
        .fetch_all(storage.pool())
        .await?;
    let first_visit: bool =
        sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM issuer_links WHERE ticker = $1)")
            .bind(&site.ticker)
            .fetch_one(storage.pool())
            .await?;
    let fresh: Vec<&Link> = found.iter().filter(|l| !known.contains(&l.url)).collect();
    if first_visit {
        let all: Vec<&str> = fresh.iter().map(|l| l.url.as_str()).collect();
        sqlx::query("INSERT INTO issuer_links(url, ticker) SELECT unnest($1::text[]), $2 ON CONFLICT DO NOTHING")
            .bind(&all)
            .bind(&site.ticker)
            .execute(storage.pool())
            .await?;
        return Ok(0);
    }
    let mut added = 0;
    for link in fresh.into_iter().take(NEW_PER_PASS) {
        let document = document(fetcher, site, link, web).await?;
        added += store(storage, std::slice::from_ref(&document)).await?;
        sqlx::query("INSERT INTO issuer_links(url, ticker, document_id) VALUES($1, $2, $3) ON CONFLICT DO NOTHING")
            .bind(&link.url)
            .bind(&site.ticker)
            .bind(&document.id)
            .execute(storage.pool())
            .await?;
    }
    Ok(added)
}

/// Все сайты эмитентов; ошибка сайта не мешает остальным.
pub(crate) async fn collect(
    fetcher: &mut Fetcher,
    storage: &PostgresStorage,
    settings: &Issuers,
    web: &Web,
) -> Result<(usize, Vec<String>)> {
    let (mut added, mut errors) = (0, Vec::new());
    for site in &settings.sites {
        match crawl(fetcher, storage, site, web).await {
            Ok(count) => added += count,
            Err(error) => errors.push(format!("{}: {error}", site.ticker)),
        }
    }
    Ok((added, errors))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_links_keep_publications_of_the_same_site() {
        let html = r#"<html><body><nav><a href="/rus/about/">О компании</a></nav>
            <a href="/media/archive/operatsionnye-rezultaty-za-3-kvartal/"><img src="x.png"></a>
            <a href="/media/archive/operatsionnye-rezultaty-za-3-kvartal/">Операционные и финансовые результаты за 3 квартал</a>
            <a href="https://other.ru/media/archive/x/">Чужая ссылка</a>
            <a href="/media/archive/?PAGEN_1=2">Следующая страница</a>
            <a href="/media/archive/novaya-domna#comments">Новая домна</a></body></html>"#;
        let pattern = regex_ok(r"^/media/archive/[a-z0-9-]+/?$").unwrap();
        let links = page_links(html, "https://severstal.com/rus/media/archive/", &pattern);
        assert_eq!(
            links
                .iter()
                .map(|l| (l.url.as_str(), l.title.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (
                    "https://severstal.com/media/archive/operatsionnye-rezultaty-za-3-kvartal/",
                    "Операционные и финансовые результаты за 3 квартал"
                ),
                (
                    "https://severstal.com/media/archive/novaya-domna",
                    "Новая домна"
                ),
            ]
        );
    }
}
