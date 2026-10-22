//! Профили подключения к моделям (`AI_MODELS_FILE`).
use std::{collections::HashMap, path::PathBuf};

use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tinkoff_rs::{Error, Result};

/// Протокол API модели.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Provider {
    /// Anthropic Messages API (Claude).
    Anthropic,
    /// OpenAI-совместимый Chat Completions.
    Openai,
}

pub(crate) const fn default_max_output_tokens() -> u32 {
    4096
}

pub(crate) const fn default_request_timeout() -> u64 {
    120
}

const fn default_true() -> bool {
    true
}

/// Цены модели в долларах США: токены — за миллион, веб-поиск — за тысячу запросов.
/// Без цен стоимость не считается и дневной лимит к профилю не применяется.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Prices {
    /// Валюта цен: `usd` (встроенные цены Anthropic) или `rub` (например, Cloud.ru).
    #[serde(default)]
    pub currency: Currency,
    pub input: f64,
    pub output: f64,
    /// Запись в кэш промпта; по умолчанию как обычный вход.
    #[serde(default)]
    pub cache_write: Option<f64>,
    /// Чтение из кэша промпта; по умолчанию как обычный вход.
    #[serde(default)]
    pub cache_read: Option<f64>,
    #[serde(default)]
    pub web_search_per_1000: Option<f64>,
}

/// Валюта цен модели.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Currency {
    #[default]
    Usd,
    Rub,
}

impl Prices {
    fn validate(&self) -> bool {
        [
            Some(self.input),
            Some(self.output),
            self.cache_write,
            self.cache_read,
            self.web_search_per_1000,
        ]
        .into_iter()
        .flatten()
        .all(|price| price.is_finite() && (0.0..=1000.0).contains(&price))
    }
}

/// Цены моделей Anthropic на октябрь 2026 года (platform.claude.com/docs/en/about-claude/pricing):
/// вход, выход, запись и чтение кэша за миллион токенов. Профиль может задать свои.
/// Haiku 5.5 дороже для запросов длиннее 100 тыс. токенов — разметка их не делает.
const KNOWN_PRICES: &[(&str, f64, f64, f64, f64)] = &[
    ("claude-sonnet-4-6", 3.0, 15.0, 3.75, 0.30),
    ("claude-sonnet-5-5", 2.0, 10.0, 2.5, 0.10),
    ("claude-opus-5-5", 4.0, 20.0, 5.0, 0.20),
    ("claude-haiku-5-5", 0.10, 0.50, 0.125, 0.01),
    ("claude-haiku-4-5", 1.0, 5.0, 1.25, 0.10),
];
/// Веб-поиск Anthropic: $10 за тысячу запросов.
const ANTHROPIC_WEB_SEARCH_PER_1000: f64 = 10.0;

fn known_prices(provider: Provider, model: &str) -> Option<Prices> {
    if provider != Provider::Anthropic {
        return None;
    }
    KNOWN_PRICES
        .iter()
        .find(|(name, ..)| model.starts_with(name))
        .map(|&(_, input, output, cache_write, cache_read)| Prices {
            currency: Currency::Usd,
            input,
            output,
            cache_write: Some(cache_write),
            cache_read: Some(cache_read),
            web_search_per_1000: Some(ANTHROPIC_WEB_SEARCH_PER_1000),
        })
}

/// Устаревший раздел `[budget]` файла профилей: дневной лимит расходов теперь
/// задаётся в панели в рублях (`dashboard_settings.ai_budget_rub`). Значение
/// файла только показывается в панели, чтобы его можно было убрать.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Budget {
    #[serde(default)]
    pub max_usd_per_day: Option<f64>,
}

/// Подключение к одной модели. Токен не попадает в `Debug`, журналы и ответы панели.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Profile {
    pub provider: Provider,
    /// Базовый URL API; по умолчанию официальный адрес провайдера.
    #[serde(default)]
    pub url: Option<String>,
    pub model: String,
    /// Токен прямо в файле профилей.
    #[serde(default)]
    pub(crate) token: Option<String>,
    /// Или имя переменной окружения с токеном.
    #[serde(default)]
    pub(crate) token_env: Option<String>,
    /// Веб-поиск на стороне провайдера (Anthropic).
    #[serde(default)]
    pub web_search: bool,
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u32,
    #[serde(default = "default_request_timeout")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Дополнительные HTTP-заголовки, которых требует шлюз провайдера.
    #[serde(default)]
    pub(crate) headers: HashMap<String, String>,
    /// Цены для учёта расходов и дневного лимита.
    #[serde(default)]
    pub prices: Option<Prices>,
    /// Кэш промпта Anthropic: повторные шаги одного решения в разы дешевле.
    #[serde(default = "default_true")]
    pub prompt_cache: bool,
}

impl std::fmt::Debug for Profile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Profile")
            .field("provider", &self.provider)
            .field("url", &self.base_url())
            .field("model", &self.model)
            .field("token", &self.token_source())
            .finish_non_exhaustive()
    }
}

impl Profile {
    pub(crate) fn base_url(&self) -> String {
        self.url.as_deref().map_or_else(
            || match self.provider {
                Provider::Anthropic => "https://api.anthropic.com/v1".to_owned(),
                Provider::Openai => "https://api.openai.com/v1".to_owned(),
            },
            |url| url.trim_end_matches('/').to_owned(),
        )
    }

    /// Цены профиля, иначе известные цены модели Anthropic.
    pub(crate) fn effective_prices(&self) -> Option<Prices> {
        self.prices
            .clone()
            .or_else(|| known_prices(self.provider, &self.model))
    }

    /// Откуда берётся токен — без самого значения.
    pub(crate) fn token_source(&self) -> String {
        if self.token.as_deref().is_some_and(|t| !t.trim().is_empty()) {
            "file".to_owned()
        } else if let Some(name) = &self.token_env {
            format!("env:{name}")
        } else {
            "none".to_owned()
        }
    }

    pub(crate) fn token(&self) -> Result<Option<SecretString>> {
        if let Some(token) = self.token.as_deref().filter(|t| !t.trim().is_empty()) {
            return Ok(Some(SecretString::new(token.trim().to_owned())));
        }
        if let Some(name) = &self.token_env {
            let value = std::env::var(name).unwrap_or_default();
            if value.trim().is_empty() {
                return Err(Error::Configuration(format!(
                    "AI model token variable {name} is empty"
                )));
            }
            return Ok(Some(SecretString::new(value.trim().to_owned())));
        }
        // Локальные модели (Ollama, LM Studio) работают без токена.
        Ok(None)
    }

    pub(crate) fn validate(&self, name: &str) -> Result<()> {
        let fail = |message: &str| {
            Err(Error::Configuration(format!(
                "AI profile {name}: {message}"
            )))
        };
        if self.model.trim().is_empty() || self.model.len() > 200 {
            return fail("model must be 1..=200 characters");
        }
        let url = self.base_url();
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return fail("url must start with http:// or https://");
        }
        if !(256..=64_000).contains(&self.max_output_tokens) {
            return fail("max_output_tokens must be in 256..=64000");
        }
        if !(10..=600).contains(&self.timeout_seconds) {
            return fail("timeout_seconds must be in 10..=600");
        }
        if self.temperature.is_some_and(|t| !(0.0..=2.0).contains(&t)) {
            return fail("temperature must be in 0..=2");
        }
        if self
            .prices
            .as_ref()
            .is_some_and(|prices| !prices.validate())
        {
            return fail("prices must be numbers from 0 to 1000");
        }
        Ok(())
    }
}

/// Файл профилей моделей.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Profiles {
    #[serde(default)]
    pub(crate) default: Option<String>,
    #[serde(default)]
    pub(crate) models: HashMap<String, Profile>,
    #[serde(default)]
    pub budget: Budget,
}

impl Profiles {
    pub(crate) fn path() -> PathBuf {
        std::env::var_os("AI_MODELS_FILE")
            .map_or_else(|| PathBuf::from("ai-models.toml"), PathBuf::from)
    }

    /// Читает файл при каждом решении: правка профиля применяется без перезапуска.
    pub(crate) fn load() -> Result<Self> {
        let path = Self::path();
        let text = std::fs::read_to_string(&path).map_err(|error| {
            Error::Configuration(format!(
                "cannot read AI model profiles {}: {error}",
                path.display()
            ))
        })?;
        let profiles: Self = toml::from_str(&text).map_err(|error| {
            // Сообщение toml не содержит значений полей, только позицию и тип ошибки.
            Error::Configuration(format!(
                "invalid AI model profiles {}: {}",
                path.display(),
                error.message()
            ))
        })?;
        for (name, profile) in &profiles.models {
            profile.validate(name)?;
        }
        if profiles
            .budget
            .max_usd_per_day
            .is_some_and(|limit| !(limit.is_finite() && limit > 0.0 && limit <= 10_000.0))
        {
            return Err(Error::Configuration(
                "budget.max_usd_per_day must be in (0, 10000]".to_owned(),
            ));
        }
        Ok(profiles)
    }

    /// Профиль по имени; без имени — `default`, а при единственном профиле — он.
    pub(crate) fn get(&self, name: Option<&str>) -> Result<(String, &Profile)> {
        let name = name
            .map(str::to_owned)
            .or_else(|| self.default.clone())
            .or_else(|| {
                (self.models.len() == 1)
                    .then(|| self.models.keys().next().cloned())
                    .flatten()
            })
            .ok_or_else(|| {
                Error::Configuration("no AI model profile selected and no default".to_owned())
            })?;
        let profile = self.models.get(&name).ok_or_else(|| {
            Error::Configuration(format!("AI model profile `{name}` is not defined"))
        })?;
        Ok((name, profile))
    }

    /// Сводка для панели и CLI: без токенов.
    pub(crate) fn summary(&self) -> Value {
        let mut names: Vec<&String> = self.models.keys().collect();
        names.sort();
        json!({
            "default": self.default,
            "models": names.into_iter().map(|name| {
                let profile = &self.models[name];
                json!({
                    "name": name, "provider": profile.provider, "model": profile.model,
                    "url": profile.base_url(), "token": profile.token_source(),
                    "web_search": profile.web_search, "prices": profile.effective_prices(),
                    "prices_source": if profile.prices.is_some() { Some("file") } else { profile.effective_prices().map(|_| "built-in") },
                    "default": self.default.as_deref() == Some(name.as_str()),
                })
            }).collect::<Vec<_>>(),
            // Дневной лимит задаётся в панели в рублях; раздел [budget] файла не действует.
            "budget_in_file": self.budget.max_usd_per_day,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_hide_tokens_and_pick_the_default() {
        let profiles: Profiles = toml::from_str(
            r#"
            default = "claude"
            [models.claude]
            provider = "anthropic"
            model = "claude-opus-5-5"
            token = "secret-value"
            [models.local]
            provider = "openai"
            url = "http://localhost:11434/v1/"
            model = "qwen2.5:14b"
        "#,
        )
        .unwrap();
        let (name, profile) = profiles.get(None).unwrap();
        assert_eq!(
            (name.as_str(), profile.base_url().as_str()),
            ("claude", "https://api.anthropic.com/v1")
        );
        assert_eq!(
            profiles.get(Some("local")).unwrap().1.base_url(),
            "http://localhost:11434/v1"
        );
        let summary = profiles.summary().to_string();
        assert!(!summary.contains("secret-value") && summary.contains("\"token\":\"file\""));
        assert!(!format!("{profile:?}").contains("secret-value"));
        assert!(profiles.get(Some("missing")).is_err());
        assert!(
            toml::from_str::<Profiles>("[models.x]\nprovider = \"gemini\"\nmodel = \"m\"").is_err()
        );
    }

    #[test]
    fn known_anthropic_models_are_priced_and_the_budget_has_a_default() {
        let profiles: Profiles = toml::from_str(
            r#"
            [models.sonnet]
            provider = "anthropic"
            model = "claude-sonnet-4-6"
            [models.own]
            provider = "anthropic"
            model = "claude-haiku-5-5"
            prices = { input = 0.2, output = 1 }
            [models.local]
            provider = "openai"
            model = "qwen"
        "#,
        )
        .unwrap();
        let sonnet = profiles.models["sonnet"].effective_prices().unwrap();
        assert_eq!(
            (
                sonnet.input,
                sonnet.output,
                sonnet.cache_read,
                sonnet.web_search_per_1000
            ),
            (3.0, 15.0, Some(0.3), Some(10.0))
        );
        assert!(
            (profiles.models["own"].effective_prices().unwrap().input - 0.2).abs() < 1e-12,
            "the file wins"
        );
        assert!(profiles.models["local"].effective_prices().is_none());
        assert_eq!(
            profiles.budget.max_usd_per_day, None,
            "the daily limit is set in rubles in the dashboard"
        );
        let summary = profiles.summary();
        assert_eq!(summary["models"][2]["prices_source"], "built-in");
        assert_eq!(summary["models"][1]["prices_source"], "file");
        assert_eq!(
            summary["models"][0]["prices_source"],
            json!(null),
            "a local model has no price"
        );
        let custom: Profiles = toml::from_str("[budget]\nmax_usd_per_day = 12.5").unwrap();
        assert_eq!(custom.budget.max_usd_per_day, Some(12.5));
    }
}
