//! Настройки сбора документов (`SOURCES_FILE`, TOML). Файл необязателен: без
//! него действуют значения по умолчанию. Секретов здесь нет, кроме токена
//! сервиса векторов, который лучше передавать через `token_env`.
use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};
use tinkoff_rs::{Error, Result};

fn default_true() -> bool {
    true
}

fn default_tinvest_interval() -> u64 {
    120
}

fn default_backfill_hours() -> u32 {
    72
}

fn default_rss_interval() -> u64 {
    300
}

fn default_cbr_interval() -> u64 {
    900
}

/// Деловые ленты с полными статьями. Интерфакс, РИА и ТАСС приходят полными
/// текстами из T-Invest, их RSS здесь не нужны.
pub(crate) fn default_feeds() -> Vec<String> {
    [
        "https://www.kommersant.ru/RSS/section-business.xml",
        "https://www.kommersant.ru/RSS/section-finance.xml",
        "https://www.vedomosti.ru/rss/rubric/finance",
        "https://www.vedomosti.ru/rss/rubric/business",
        "https://www.finam.ru/analysis/nslent/rsspoint/",
        "https://www.finam.ru/analysis/conews/rsspoint/",
    ]
    .map(str::to_owned)
    .to_vec()
}

fn default_cbr_feeds() -> Vec<String> {
    [
        "https://www.cbr.ru/rss/RssPress",
        "https://www.cbr.ru/rss/eventrss",
    ]
    .map(str::to_owned)
    .to_vec()
}

/// Новости T-Invest: Интерфакс, РИА, ТАСС и раскрытие информации эмитентов
/// (`eDisclosure`) с полными текстами и отметками инструментов.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Tinvest {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_tinvest_interval")]
    pub interval_seconds: u64,
    /// Глубина первой загрузки.
    #[serde(default = "default_backfill_hours")]
    pub backfill_hours: u32,
}

impl Default for Tinvest {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_seconds: default_tinvest_interval(),
            backfill_hours: default_backfill_hours(),
        }
    }
}

/// Деловые RSS-ленты; полный текст статьи загружается по ссылке.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Rss {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_rss_interval")]
    pub interval_seconds: u64,
    #[serde(default = "default_feeds")]
    pub feeds: Vec<String>,
    #[serde(default = "default_true")]
    pub full_text: bool,
}

impl Default for Rss {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_seconds: default_rss_interval(),
            feeds: default_feeds(),
            full_text: true,
        }
    }
}

/// Сообщения Московской биржи (ISS).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Moex {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_rss_interval")]
    pub interval_seconds: u64,
}

impl Default for Moex {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_seconds: default_rss_interval(),
        }
    }
}

/// Пресс-релизы и события Банка России.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Cbr {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_cbr_interval")]
    pub interval_seconds: u64,
    #[serde(default = "default_cbr_feeds")]
    pub feeds: Vec<String>,
}

impl Default for Cbr {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_seconds: default_cbr_interval(),
            feeds: default_cbr_feeds(),
        }
    }
}

const fn default_host_pause() -> u64 {
    3
}

const fn default_page_bytes() -> usize {
    2_000_000
}

/// Загрузка страниц: вежливая пауза на сайт, предел размера, CSS-селекторы
/// основного текста для сайтов, где общий разбор ошибается.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Web {
    #[serde(default = "default_host_pause")]
    pub seconds_between_requests: u64,
    #[serde(default = "default_page_bytes")]
    pub max_page_bytes: usize,
    #[serde(default)]
    pub selectors: HashMap<String, String>,
}

impl Default for Web {
    fn default() -> Self {
        Self {
            seconds_between_requests: default_host_pause(),
            max_page_bytes: default_page_bytes(),
            selectors: HashMap::new(),
        }
    }
}

const fn default_news_days() -> u32 {
    365
}

const fn default_irrelevant_days() -> u32 {
    7
}

const fn default_embeddings_days() -> u32 {
    180
}

/// Сроки хранения. Раскрытие информации, пресс-релизы, отчёты и сообщения
/// регулятора хранятся бессрочно: их немного.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_field_names)] // имена полей — ключи файла настроек
pub(crate) struct Retention {
    /// Новости, статьи и сообщения биржи.
    #[serde(default = "default_news_days")]
    pub news_days: u32,
    /// Текст документов, которые разметка признала не относящимися к рынку.
    #[serde(default = "default_irrelevant_days")]
    pub irrelevant_days: u32,
    #[serde(default = "default_embeddings_days")]
    pub embeddings_days: u32,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            news_days: default_news_days(),
            irrelevant_days: default_irrelevant_days(),
            embeddings_days: default_embeddings_days(),
        }
    }
}

const fn default_dimensions() -> u32 {
    1024
}

const fn default_batch() -> usize {
    32
}

const fn default_same_event_similarity() -> f64 {
    0.92
}

/// Сервис векторов для поиска по смыслу: OpenAI-совместимый `/embeddings`
/// (Ollama, text-embeddings-inference, vLLM, `OpenAI`, Voyage). Без раздела поиск
/// только полнотекстовый.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Embeddings {
    /// Адрес OpenAI-совместимого API; без него — адрес профиля `profile`.
    #[serde(default)]
    pub url: String,
    /// Профиль из файла моделей, чьи адрес и токен использовать: ключ остаётся
    /// в одном файле с правами 0600.
    #[serde(default)]
    pub profile: Option<String>,
    pub model: String,
    #[serde(default = "default_dimensions")]
    pub dimensions: u32,
    #[serde(default, skip_serializing)]
    pub token: Option<String>,
    #[serde(default)]
    pub token_env: Option<String>,
    /// Префиксы моделей семейства e5: `query: ` и `passage: `.
    #[serde(default)]
    pub query_prefix: String,
    #[serde(default)]
    pub document_prefix: String,
    /// Voyage различает запросы и документы полем `input_type`.
    #[serde(default)]
    pub input_type: bool,
    #[serde(default = "default_batch")]
    pub batch: usize,
    /// Цена за миллион токенов для учёта расходов; локальная модель — 0.
    #[serde(default)]
    pub price_per_million: f64,
    /// Валюта цены: `usd` (по умолчанию) или `rub` (например, векторы Cloud.ru).
    #[serde(default)]
    pub price_currency: crate::ai::profiles::Currency,
    /// Близость первого фрагмента, с которой сообщение другого агентства
    /// считается тем же событием. У моделей e5 близость разных текстов на одну
    /// тему высокая — порог выше (0.95); 1 отключает объединение.
    #[serde(default = "default_same_event_similarity")]
    pub same_event_similarity: f64,
}

impl std::fmt::Debug for Embeddings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Embeddings")
            .field("url", &self.url)
            .field("model", &self.model)
            .field("dimensions", &self.dimensions)
            .finish_non_exhaustive()
    }
}

const fn default_tag_batch() -> usize {
    15
}

const fn default_tag_per_tick() -> usize {
    60
}

/// Разметка документов дешёвой моделью: важность, вид события, краткое
/// содержание, эмитенты и темы. Подключение и токен — из файла профилей моделей.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Tagging {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Профиль подключения из файла моделей; без него — профиль по умолчанию.
    #[serde(default)]
    pub profile: Option<String>,
    /// Модель вместо модели профиля; для Anthropic по умолчанию `claude-haiku-5-5`.
    #[serde(default)]
    pub model: Option<String>,
    /// Профиль для черновиков чувствительности бумаг; без него — профиль по умолчанию.
    #[serde(default)]
    pub exposure_profile: Option<String>,
    #[serde(default = "default_tag_batch")]
    pub batch: usize,
    #[serde(default = "default_tag_per_tick")]
    pub per_tick: usize,
}

impl Default for Tagging {
    fn default() -> Self {
        Self {
            enabled: true,
            profile: None,
            model: None,
            exposure_profile: None,
            batch: default_tag_batch(),
            per_tick: default_tag_per_tick(),
        }
    }
}

/// Шаблон ссылок сайта эмитента.
pub(crate) fn regex_ok(pattern: &str) -> std::result::Result<regex::Regex, regex::Error> {
    regex::RegexBuilder::new(pattern)
        .size_limit(1 << 20)
        .build()
}

/// Сайт эмитента: RSS пресс-релизов или страница со списком и шаблоном ссылок.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Issuer {
    pub ticker: String,
    /// RSS/Atom пресс-релизов.
    #[serde(default)]
    pub feed: Option<String>,
    /// Или страница со списком публикаций…
    #[serde(default)]
    pub page: Option<String>,
    /// …и регулярное выражение пути ссылки на публикацию (PDF — отчётность).
    #[serde(default)]
    pub link_pattern: Option<String>,
}

const fn default_issuer_interval() -> u64 {
    3600
}

/// Сайты эмитентов, проверенные на доступность и разрешение `robots.txt`.
pub(crate) fn default_issuers() -> Vec<Issuer> {
    let feed = |ticker: &str, url: &str| Issuer {
        ticker: ticker.to_owned(),
        feed: Some(url.to_owned()),
        page: None,
        link_pattern: None,
    };
    let page = |ticker: &str, url: &str, pattern: &str| Issuer {
        ticker: ticker.to_owned(),
        feed: None,
        page: Some(url.to_owned()),
        link_pattern: Some(pattern.to_owned()),
    };
    vec![
        feed("GAZP", "https://www.gazprom.ru/rss/"),
        feed("ROSN", "https://www.rosneft.ru/press/releases/rss"),
        page(
            "CHMF",
            "https://severstal.com/rus/media/archive/",
            r"^/media/archive/[a-z0-9-]+/?$",
        ),
        page(
            "MTSS",
            "https://ir.mts.ru/news_and_events/corporate_releases",
            r"^/news_and_events/corporate_releases/details/\d+$",
        ),
        page(
            "MAGN",
            "https://mmk.ru/ru/press-center/news/",
            r"^/ru/press-center/news/[a-z0-9-]+/$",
        ),
    ]
}

/// Сайты эмитентов: пресс-релизы и отчётность.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Issuers {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_issuer_interval")]
    pub interval_seconds: u64,
    #[serde(default = "default_issuers")]
    pub sites: Vec<Issuer>,
}

impl Default for Issuers {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_seconds: default_issuer_interval(),
            sites: default_issuers(),
        }
    }
}

/// Настройки сбора целиком.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Sources {
    #[serde(default)]
    pub tinvest: Tinvest,
    #[serde(default)]
    pub rss: Rss,
    #[serde(default)]
    pub moex: Moex,
    #[serde(default)]
    pub cbr: Cbr,
    #[serde(default)]
    pub web: Web,
    #[serde(default)]
    pub retention: Retention,
    #[serde(default)]
    pub embeddings: Option<Embeddings>,
    #[serde(default)]
    pub tagging: Tagging,
    #[serde(default)]
    pub issuers: Issuers,
}

impl Sources {
    pub(crate) fn path() -> Option<PathBuf> {
        std::env::var_os("SOURCES_FILE").map(PathBuf::from)
    }

    /// Файл перечитывается каждый цикл сборщика; его отсутствие — настройки по умолчанию.
    pub(crate) fn load() -> Result<Self> {
        let Some(path) = Self::path() else {
            return Ok(Self::default());
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(Error::Configuration(format!(
                    "cannot read sources {}: {error}",
                    path.display()
                )));
            }
        };
        let sources: Self = toml::from_str(&text).map_err(|error| {
            Error::Configuration(format!(
                "invalid sources {}: {}",
                path.display(),
                error.message()
            ))
        })?;
        sources.validate()?;
        Ok(sources)
    }

    fn validate(&self) -> Result<()> {
        let fail = |message: &str| Err(Error::Configuration(format!("sources: {message}")));
        for interval in [
            self.tinvest.interval_seconds,
            self.rss.interval_seconds,
            self.moex.interval_seconds,
            self.cbr.interval_seconds,
        ] {
            if !(30..=86_400).contains(&interval) {
                return fail("interval_seconds must be in 30..=86400");
            }
        }
        if self
            .rss
            .feeds
            .iter()
            .chain(&self.cbr.feeds)
            .any(|url| !(url.starts_with("https://") || url.starts_with("http://")))
        {
            return fail("feeds must be http(s) URLs");
        }
        for site in &self.issuers.sites {
            let url = site.feed.as_ref().or(site.page.as_ref());
            if site.ticker.is_empty()
                || site.ticker.len() > 12
                || !site.ticker.chars().all(|c| c.is_ascii_alphanumeric())
                || url.is_none_or(|u| !u.starts_with("https://") && !u.starts_with("http://"))
                || (site.page.is_some()
                    && site
                        .link_pattern
                        .as_deref()
                        .is_none_or(|p| regex_ok(p).is_err()))
            {
                return fail("issuers: ticker, feed or page with a valid link_pattern");
            }
        }
        if !(1..=40).contains(&self.tagging.batch) || !(1..=500).contains(&self.tagging.per_tick) {
            return fail("tagging batch must be 1..=40 and per_tick 1..=500");
        }
        if !(1..=60).contains(&self.web.seconds_between_requests)
            || !(10_000..=20_000_000).contains(&self.web.max_page_bytes)
        {
            return fail("web limits are out of range");
        }
        if let Some(embeddings) = &self.embeddings {
            if !(64..=4096).contains(&embeddings.dimensions)
                || !(1..=256).contains(&embeddings.batch)
                || embeddings.model.trim().is_empty()
                || (embeddings.url.trim().is_empty() && embeddings.profile.is_none())
                || !embeddings.price_per_million.is_finite()
                || embeddings.price_per_million < 0.0
            {
                return fail("embeddings settings are out of range");
            }
        }
        Ok(())
    }

    /// Токен сервиса векторов из файла или переменной окружения.
    pub(crate) fn embeddings_token(&self) -> Option<String> {
        use secrecy::ExposeSecret;
        let embeddings = self.embeddings.as_ref()?;
        embeddings
            .token
            .clone()
            .filter(|t| !t.trim().is_empty())
            .or_else(|| {
                embeddings
                    .token_env
                    .as_ref()
                    .and_then(|name| std::env::var(name).ok())
                    .filter(|t| !t.trim().is_empty())
            })
            .or_else(|| {
                let (_, profile) = self.embeddings_profile()?;
                profile
                    .token()
                    .ok()
                    .flatten()
                    .map(|token| token.expose_secret().clone())
            })
    }

    /// Профиль модели, на который ссылаются настройки векторов.
    pub(crate) fn embeddings_profile(&self) -> Option<(String, crate::ai::profiles::Profile)> {
        let name = self.embeddings.as_ref()?.profile.as_deref()?;
        let profiles = crate::ai::profiles::Profiles::load().ok()?;
        profiles
            .get(Some(name))
            .ok()
            .map(|(name, profile)| (name, profile.clone()))
    }

    /// Адрес сервиса векторов: свой или профиля.
    pub(crate) fn embeddings_url(&self) -> Option<String> {
        let embeddings = self.embeddings.as_ref()?;
        if embeddings.url.trim().is_empty() {
            self.embeddings_profile()
                .map(|(_, profile)| profile.base_url())
        } else {
            Some(embeddings.url.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_cover_every_source_and_files_are_validated() {
        let sources: Sources = toml::from_str("").unwrap();
        assert!(sources.tinvest.enabled && sources.rss.full_text && sources.embeddings.is_none());
        assert_eq!(sources.retention.news_days, 365);
        let custom: Sources = toml::from_str(
            "[rss]\nfeeds = [\"https://example.ru/rss\"]\n[embeddings]\nurl = \"http://embeddings:11434/v1\"\nmodel = \"bge-m3\"\ntoken = \"secret\"",
        )
        .unwrap();
        custom.validate().unwrap();
        assert_eq!(custom.rss.feeds.len(), 1);
        assert!(
            !format!("{custom:?}").contains("secret"),
            "the token stays out of logs"
        );
        assert!(!serde_json::to_string(&custom).unwrap().contains("secret"));
        let bad: Sources = toml::from_str("[tinvest]\ninterval_seconds = 5").unwrap();
        assert!(bad.validate().is_err());
        // Адрес и токен векторов можно взять из профиля модели.
        let borrowed: Sources = toml::from_str(
            "[embeddings]\nprofile = \"remote\"\nmodel = \"bge-m3\"\ndimensions = 1024",
        )
        .unwrap();
        borrowed.validate().unwrap();
        let nowhere: Sources = toml::from_str("[embeddings]\nmodel = \"bge-m3\"").unwrap();
        assert!(nowhere.validate().is_err(), "neither url nor profile");
        assert!(toml::from_str::<Sources>("[unknown]\nx = 1").is_err());
    }
}
