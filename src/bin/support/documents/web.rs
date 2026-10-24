//! Загрузка страниц и выделение основного текста статьи.
//!
//! Правила вежливости: только открытые страницы, `robots.txt` сайта соблюдается,
//! между запросами к одному сайту — пауза, размер страницы ограничен. Платный
//! доступ, защита от ботов и авторизация не обходятся: такая страница просто
//! остаётся с анонсом из ленты.
use std::{
    collections::HashMap,
    time::{Duration as StdDuration, Instant},
};

use scraper::{ElementRef, Html, Selector};
use tinkoff_rs::{Error, Result};

use super::{clean, sources::Web};

const USER_AGENT: &str =
    "Mozilla/5.0 (compatible; tinkoff-rs-news/0.4; +https://github.com/ObraztsovOleg/tinkoff-rs)";
/// `robots.txt` перечитывается раз в сутки.
const ROBOTS_TTL: StdDuration = StdDuration::from_secs(86_400);

/// Правила `robots.txt` для нашего агента: запрещённые и разрешённые префиксы.
#[derive(Clone, Debug, Default)]
struct Robots {
    allow: Vec<String>,
    disallow: Vec<String>,
}

impl Robots {
    /// Правила группы `User-agent: *` (или нашей): действует самый длинный префикс.
    fn parse(text: &str) -> Self {
        let mut rules = Self::default();
        let (mut ours, mut in_group_header) = (false, false);
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or_default().trim();
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let (key, value) = (key.trim().to_lowercase(), value.trim());
            match key.as_str() {
                "user-agent" => {
                    let matches = value == "*" || value.to_lowercase().contains("tinkoff-rs");
                    ours = if in_group_header {
                        ours || matches
                    } else {
                        matches
                    };
                    in_group_header = true;
                }
                "allow" if ours => {
                    in_group_header = false;
                    rules.allow.push(value.to_owned());
                }
                "disallow" if ours => {
                    in_group_header = false;
                    if !value.is_empty() {
                        rules.disallow.push(value.to_owned());
                    }
                }
                _ => in_group_header = false,
            }
        }
        rules
    }

    fn allowed(&self, path: &str) -> bool {
        let longest = |rules: &[String]| {
            rules
                .iter()
                .filter(|rule| path.starts_with(rule.trim_end_matches('*').trim_end_matches('$')))
                .map(String::len)
                .max()
        };
        match (longest(&self.allow), longest(&self.disallow)) {
            (_, None) => true,
            (None, Some(_)) => false,
            (Some(allow), Some(disallow)) => allow >= disallow,
        }
    }
}

/// Страница статьи: описание и абзацы основного текста.
#[derive(Clone, Debug, Default)]
pub(crate) struct Page {
    pub description: String,
    pub body: String,
}

/// HTTP-клиент сборщика с паузами по сайтам и кэшем `robots.txt`.
pub(crate) struct Fetcher {
    client: reqwest::Client,
    last_request: HashMap<String, Instant>,
    robots: HashMap<String, (Instant, Robots)>,
    /// Сайты, которые отказали (401, 403, 429): не обращаемся к ним до срока.
    refused: HashMap<String, Instant>,
}

/// Отказавший сайт не запрашивается шесть часов.
const REFUSED_PAUSE: StdDuration = StdDuration::from_secs(6 * 3600);

pub(crate) fn host(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .unwrap_or(url)
        .trim_start_matches("www.")
        .to_lowercase()
}

fn path_of(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    rest.find('/')
        .map_or_else(|| "/".to_owned(), |i| rest[i..].to_owned())
}

fn origin(url: &str) -> String {
    let scheme = url.split("://").next().unwrap_or("https");
    let rest = url.split("://").nth(1).unwrap_or(url);
    format!("{scheme}://{}", rest.split('/').next().unwrap_or(rest))
}

/// Кодировка из заголовка ответа, иначе из `<meta charset>` в начале страницы.
fn decode(bytes: &[u8], content_type: Option<&str>) -> String {
    let label = content_type
        .and_then(|value| value.split("charset=").nth(1))
        .map(|value| value.trim_matches(['"', '\'', ' ', ';']).to_owned())
        .or_else(|| {
            let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]).to_lowercase();
            let at = head.find("charset=")?;
            Some(
                head[at + 8..]
                    .trim_start_matches(['"', '\''])
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                    .collect(),
            )
        });
    let encoding = label
        .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    encoding.decode(bytes).0.into_owned()
}

impl Fetcher {
    pub(crate) fn new() -> Result<Self> {
        let client = crate::tls::http_client()
            .timeout(StdDuration::from_secs(20))
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .map_err(|error| Error::Configuration(format!("http client: {error}")))?;
        Ok(Self {
            client,
            last_request: HashMap::new(),
            robots: HashMap::new(),
            refused: HashMap::new(),
        })
    }

    /// Пауза перед запросом к сайту, чтобы не нагружать его.
    async fn pace(&mut self, host: &str, settings: &Web) {
        let pause = StdDuration::from_secs(settings.seconds_between_requests);
        if let Some(last) = self.last_request.get(host) {
            let elapsed = last.elapsed();
            if let Some(rest) = pause.checked_sub(elapsed) {
                tokio::time::sleep(rest).await;
            }
        }
        self.last_request.insert(host.to_owned(), Instant::now());
    }

    async fn robots_allow(&mut self, url: &str, settings: &Web) -> bool {
        let host = host(url);
        let fresh = self
            .robots
            .get(&host)
            .is_some_and(|(at, _)| at.elapsed() < ROBOTS_TTL);
        if !fresh {
            self.pace(&host, settings).await;
            let rules = match self
                .client
                .get(format!("{}/robots.txt", origin(url)))
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => {
                    Robots::parse(&response.text().await.unwrap_or_default())
                }
                // Нет файла — нет ограничений; сбой сети — попробуем страницу.
                _ => Robots::default(),
            };
            self.robots.insert(host.clone(), (Instant::now(), rules));
        }
        self.robots[&host].1.allowed(&path_of(url))
    }

    /// Текст ответа не больше `max_page_bytes`; `None` — страница запрещена
    /// `robots.txt` или недоступна.
    pub(crate) async fn get_text(&mut self, url: &str, settings: &Web) -> Result<Option<String>> {
        Ok(self
            .download(url, settings, settings.max_page_bytes)
            .await?
            .map(|(bytes, content_type)| decode(&bytes, content_type.as_deref())))
    }

    /// Файл целиком (PDF отчёта) не больше `limit` байт; больше — не скачивается.
    pub(crate) async fn get_bytes(
        &mut self,
        url: &str,
        settings: &Web,
        limit: usize,
    ) -> Result<Option<Vec<u8>>> {
        Ok(self
            .download(url, settings, limit + 1)
            .await?
            .map(|(bytes, _)| bytes)
            .filter(|bytes| bytes.len() <= limit))
    }

    async fn download(
        &mut self,
        url: &str,
        settings: &Web,
        limit: usize,
    ) -> Result<Option<(Vec<u8>, Option<String>)>> {
        let site = host(url);
        if self
            .refused
            .get(&site)
            .is_some_and(|at| at.elapsed() < REFUSED_PAUSE)
        {
            return Ok(None);
        }
        if !self.robots_allow(url, settings).await {
            return Ok(None);
        }
        self.pace(&site, settings).await;
        let mut response = match self.client.get(url).send().await {
            Ok(response) => response,
            Err(error) => {
                return Err(Error::Configuration(format!(
                    "{}: {}",
                    host(url),
                    error.without_url()
                )));
            }
        };
        let status = response.status();
        if matches!(status.as_u16(), 401 | 403 | 429) {
            // Защита от ботов или лимит: страницу не обходим, сайт оставляем в покое.
            self.refused.insert(site, Instant::now());
            return Ok(None);
        }
        if !status.is_success() {
            return Ok(None);
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            Error::Configuration(format!("{}: {}", host(url), error.without_url()))
        })? {
            bytes.extend_from_slice(&chunk);
            if bytes.len() >= limit {
                break;
            }
        }
        Ok(Some((bytes, content_type)))
    }

    /// Основной текст статьи по ссылке.
    pub(crate) async fn article(&mut self, url: &str, settings: &Web) -> Result<Option<Page>> {
        let Some(html) = self.get_text(url, settings).await? else {
            return Ok(None);
        };
        let selector = settings.selectors.get(&host(url)).map(String::as_str);
        Ok(Some(extract(&html, selector)))
    }
}

fn selector(text: &str) -> Selector {
    Selector::parse(text).expect("static selector is valid")
}

fn element_text(element: ElementRef<'_>) -> String {
    clean(&element.text().collect::<Vec<_>>().join(" "), 20_000)
}

/// Элемент внутри служебной части страницы: меню, подвал, врезки, формы.
fn in_boilerplate(element: ElementRef<'_>) -> bool {
    element
        .ancestors()
        .filter_map(ElementRef::wrap)
        .any(|parent| {
            let name = parent.value().name();
            let class = parent
                .value()
                .attr("class")
                .unwrap_or_default()
                .to_lowercase();
            matches!(
                name,
                "nav"
                    | "aside"
                    | "footer"
                    | "header"
                    | "form"
                    | "figure"
                    | "script"
                    | "style"
                    | "noscript"
            ) || [
                "related",
                "subscribe",
                "share",
                "banner",
                "advert",
                "promo",
                "comment",
                "footer",
                "menu",
            ]
            .iter()
            .any(|word| class.contains(word))
        })
}

/// Абзацы основного текста внутри корня.
fn paragraphs(root: ElementRef<'_>) -> Vec<String> {
    let blocks = selector("p, li, h2, h3, blockquote");
    let mut out: Vec<String> = Vec::new();
    for block in root.select(&blocks) {
        if in_boilerplate(block) {
            continue;
        }
        // Абзац внутри другого абзаца или пункта уже учтён родителем.
        if block
            .ancestors()
            .filter_map(ElementRef::wrap)
            .take_while(|parent| parent.id() != root.id())
            .any(|parent| matches!(parent.value().name(), "p" | "li" | "blockquote"))
        {
            continue;
        }
        let text = element_text(block);
        let heading = matches!(block.value().name(), "h2" | "h3");
        if text.chars().count() < if heading { 10 } else { 30 } || out.last() == Some(&text) {
            continue;
        }
        out.push(text);
    }
    out
}

fn meta(document: &Html, query: &str) -> Option<String> {
    document
        .select(&selector(query))
        .next()
        .and_then(|element| element.value().attr("content"))
        .map(|value| clean(value, 1000))
        .filter(|value| !value.is_empty())
}

/// Выделяет основной текст: селектор сайта, разметка статьи или элемент с
/// наибольшим объёмом абзацев.
pub(crate) fn extract(html: &str, site_selector: Option<&str>) -> Page {
    let document = Html::parse_document(html);
    let description = meta(&document, r#"meta[property="og:description"]"#)
        .or_else(|| meta(&document, r#"meta[name="description"]"#))
        .unwrap_or_default();
    let long = |text: &String| text.chars().count() > 200;
    let body = site_selector
        .and_then(|query| Selector::parse(query).ok())
        .map(|query| {
            document
                .select(&query)
                .map(|root| paragraphs(root).join("\n"))
                .collect::<Vec<_>>()
                .join("\n")
        })
        // Селектор сайта задан оператором: короткий текст тоже принимается.
        .filter(|text| !text.trim().is_empty())
        .or_else(|| json_ld_body(&document).filter(long))
        .or_else(|| {
            document
                .select(&selector(r#"[itemprop="articleBody"]"#))
                .map(|root| paragraphs(root).join("\n"))
                .max_by_key(|text| text.chars().count())
                .filter(long)
        })
        .or_else(|| paragraph_cluster(&document).filter(long))
        .or_else(|| text_block(&document).filter(long))
        .unwrap_or_default();
    Page { description, body }
}

/// Текст статьи из разметки schema.org (`articleBody` в JSON-LD): так его
/// публикуют многие издания, в том числе с отрисовкой страницы скриптами.
fn json_ld_body(document: &Html) -> Option<String> {
    fn find(value: &serde_json::Value) -> Option<String> {
        match value {
            serde_json::Value::Object(map) => map
                .get("articleBody")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .or_else(|| map.values().find_map(find)),
            serde_json::Value::Array(items) => items.iter().find_map(find),
            _ => None,
        }
    }
    document
        .select(&selector(r#"script[type="application/ld+json"]"#))
        .filter_map(|script| {
            serde_json::from_str::<serde_json::Value>(&script.text().collect::<String>()).ok()
        })
        .find_map(|value| find(&value))
        .map(|body| {
            body.lines()
                .map(|line| clean(line, 20_000))
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        })
}

/// Без разметки статьи: абзацы (`<p>` или блоки с классом «paragraph»),
/// сгруппированные по общему родителю; побеждает группа с наибольшим текстом.
fn paragraph_cluster(document: &Html) -> Option<String> {
    let mut groups: Vec<(ego_tree::NodeId, Vec<String>, usize)> = Vec::new();
    for block in document.select(&selector(r#"p, [class*="paragraph"]"#)) {
        if in_boilerplate(block)
            || block
                .ancestors()
                .filter_map(ElementRef::wrap)
                .any(|parent| {
                    parent.value().name() == "p"
                        || parent
                            .value()
                            .attr("class")
                            .is_some_and(|c| c.contains("paragraph"))
                })
        {
            continue;
        }
        let text = element_text(block);
        if text.chars().count() < 30 {
            continue;
        }
        let Some(parent) = block.parent().map(|p| p.id()) else {
            continue;
        };
        let size = text.chars().count();
        match groups.iter_mut().find(|(id, ..)| *id == parent) {
            Some(group) => {
                if group.1.last() != Some(&text) {
                    group.1.push(text);
                    group.2 += size;
                }
            }
            None => groups.push((parent, vec![text], size)),
        }
    }
    groups
        .into_iter()
        .max_by_key(|(_, _, size)| *size)
        .map(|(_, texts, _)| texts.join("\n"))
}

/// Без абзацев (текст через `<br>`): блок с наибольшим собственным текстом —
/// текстом прямо в нём и во вложенных строчных элементах.
fn text_block(document: &Html) -> Option<String> {
    const INLINE: &[&str] = &[
        "strong", "b", "em", "i", "span", "a", "u", "sup", "sub", "br",
    ];
    document
        .select(&selector("div, section, td, article"))
        .filter(|root| !in_boilerplate(*root))
        .map(|root| {
            let own: usize = root
                .children()
                .map(|child| match child.value() {
                    scraper::Node::Text(text) => text.trim().chars().count(),
                    scraper::Node::Element(element) if INLINE.contains(&element.name()) => {
                        ElementRef::wrap(child).map_or(0, |e| element_text(e).chars().count())
                    }
                    _ => 0,
                })
                .sum();
            (own, root)
        })
        .filter(|(own, _)| *own > 300)
        .max_by_key(|(own, _)| *own)
        .map(|(_, root)| html_text(&root.inner_html()))
}

/// Текст HTML-фрагмента (сообщения биржи, пресс-релиза) по абзацам; ячейки
/// таблиц разделены `|`.
pub(crate) fn html_text(fragment: &str) -> String {
    use ego_tree::iter::Edge;
    use scraper::Node;
    let html = Html::parse_fragment(fragment);
    let mut out = String::new();
    let mut skip = 0_usize;
    for edge in html.root_element().traverse() {
        match edge {
            Edge::Open(node) => match node.value() {
                Node::Element(element) if matches!(element.name(), "script" | "style") => skip += 1,
                Node::Element(element)
                    if matches!(
                        element.name(),
                        "p" | "div" | "li" | "tr" | "br" | "table" | "ul" | "ol" | "blockquote"
                    ) || element.name().starts_with('h') && element.name().len() == 2 =>
                {
                    out.push('\n');
                }
                Node::Text(text) if skip == 0 => out.push_str(text),
                _ => {}
            },
            Edge::Close(node) => {
                if let Node::Element(element) = node.value() {
                    match element.name() {
                        "script" | "style" => skip = skip.saturating_sub(1),
                        "td" | "th" => out.push_str(" | "),
                        "p" | "div" | "li" | "tr" => out.push('\n'),
                        _ => {}
                    }
                }
            }
        }
    }
    out.lines()
        .map(|line| clean(line.trim().trim_end_matches('|').trim(), 5000))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// После обновления HTTP-стека: клиент строится с единственным
    /// криптопровайдером rustls и проверяет сертификаты по системным корням.
    #[tokio::test]
    #[ignore = "network: reaches a public HTTPS site"]
    async fn https_client_verifies_a_public_site() {
        let fetcher = Fetcher::new().unwrap();
        let response = fetcher
            .client
            .get("https://www.moex.com/")
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success() || response.status().is_redirection());
        assert!(
            fetcher
                .client
                .get("https://self-signed.badssl.com/")
                .send()
                .await
                .is_err()
        );
    }

    #[test]
    fn html_fragments_become_paragraphs_and_table_rows() {
        let text = html_text(
            "<p>Решение&nbsp;<!-- -->принято:</p><ul><li>дата начала торгов</li></ul>\
             <table><tr><th>Код</th><th>Цена</th></tr><tr><td>SBER</td><td>300</td></tr></table><script>x()</script>",
        );
        assert_eq!(
            text,
            "Решение принято:\nдата начала торгов\nКод | Цена\nSBER | 300"
        );
    }

    #[test]
    fn robots_rules_use_the_longest_matching_prefix() {
        let robots = Robots::parse(
            "User-agent: Googlebot\nDisallow: /\n\nUser-agent: *\nDisallow: /search\nDisallow: /private/\nAllow: /private/public\n",
        );
        assert!(robots.allowed("/news/2026/10/20/oil"));
        assert!(!robots.allowed("/search?q=oil"));
        assert!(!robots.allowed("/private/a"));
        assert!(robots.allowed("/private/public/b"));
        assert!(Robots::parse("").allowed("/anything"));
        assert!(!Robots::parse("User-agent: *\nDisallow: /").allowed("/x"));
    }

    #[test]
    fn article_text_skips_menus_related_links_and_short_lines() {
        let html = r#"<html><head><meta charset="utf-8"><title>Сайт</title>
            <meta property="og:title" content="Норникель снизил прогноз">
            <meta property="og:description" content="Компания пересмотрела планы">
            <meta property="article:published_time" content="2026-10-20T09:30:00+03:00"></head>
            <body><nav><p>Главная · Новости · Экономика · Политика · Подписка на рассылку</p></nav>
            <article><h1>Норникель снизил прогноз</h1>
            <p>«Норильский никель» снизил прогноз производства никеля на 2026 год из-за ремонта на Надеждинском заводе.</p>
            <p>Короткая.</p>
            <div class="related"><p>Читайте также: десять новостей о металлах, которые нельзя пропустить сегодня</p></div>
            <p>Компания ожидает выпуск 190–200 тыс. тонн никеля против прежних 200–210 тыс. тонн, сообщила пресс-служба.</p>
            </article><footer><p>© Издание, все права защищены, перепечатка запрещена без разрешения</p></footer></body></html>"#;
        let page = extract(html, None);
        assert_eq!(page.description, "Компания пересмотрела планы");
        let lines: Vec<&str> = page.body.lines().collect();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].starts_with("«Норильский никель» снизил") && lines[1].contains("190–200"));
    }

    #[test]
    fn pages_without_markup_use_the_densest_block_and_any_encoding() {
        let text = "Банк России сохранил ключевую ставку на уровне 16% годовых и сигнализирует о возможном снижении. ";
        let html = format!(
            "<html><head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=windows-1251\"></head><body>\
             <div><p>Меню сайта и ссылки на разделы, которые повторяются на всех страницах</p></div>\
             <div class=\"content\"><p>{text}</p><p>Второй абзац: {text}</p><p>Третий абзац: {text}</p></div></body></html>"
        );
        let (bytes, _, _) = encoding_rs::WINDOWS_1251.encode(&html);
        let decoded = decode(&bytes, None);
        let page = extract(&decoded, None);
        assert!(
            page.body.starts_with("Банк России сохранил"),
            "{}",
            page.body
        );
        assert_eq!(page.body.lines().count(), 3);
        let selected = extract(
            "<html><body><div class=\"x\"><p>Текст по селектору сайта достаточно длинный для абзаца.</p></div></body></html>",
            Some("div.x"),
        );
        assert!(selected.body.starts_with("Текст по селектору"));
        assert_eq!(host("https://www.Kommersant.ru/doc/1?x=1"), "kommersant.ru");
        let fact = "Чистая прибыль компании за девять месяцев снизилась на 61%, свободный денежный поток стал отрицательным.";
        // Text published only as schema.org JSON-LD (the page is drawn by scripts).
        let scripted = extract(
            &format!(
                "<html><head><script type=\"application/ld+json\">{{\"@graph\": [{{\"@type\": \"NewsArticle\", \"articleBody\": \"{fact}\\n{fact}\\n{fact}\"}}]}}</script></head><body><div id=\"app\"></div></body></html>"
            ),
            None,
        );
        assert_eq!(scripted.body.lines().count(), 3, "{}", scripted.body);
        // Paragraph blocks without <p>, next to a list of other news.
        let blocks = extract(
            &format!(
                "<html><body><section><article class=\"recent-news-item\"><p>Другая новость дня с длинным заголовком для проверки</p></article></section>\
                 <div class=\"doc__body\"><div class=\"doc-text-block__paragraph\">{fact}</div><div class=\"doc-text-block__paragraph\">Выручка упала на 10%. {fact}</div>\
                 <div class=\"doc-text-block__paragraph\">Дивиденды не выплачиваются. {fact}</div></div></body></html>"
            ),
            None,
        );
        assert!(
            blocks.body.starts_with("Чистая прибыль") && blocks.body.lines().count() == 3,
            "{}",
            blocks.body
        );
        // Text separated by <br> in one block, next to a short navigation block.
        let release = extract(
            &format!(
                "<html><body><div class=\"menu\">Главная · Новости</div><div class=\"news-detail\"><strong>Москва — {fact}</strong>\
                 <br />&nbsp;<br />Вторая часть. {fact}<br />Третья часть. {fact}</div></body></html>"
            ),
            None,
        );
        assert!(
            release.body.starts_with("Москва — Чистая прибыль")
                && release.body.lines().count() == 3,
            "{}",
            release.body
        );
        assert_eq!(path_of("https://cbr.ru/press/pr/?id=1"), "/press/pr/?id=1");
    }
}
