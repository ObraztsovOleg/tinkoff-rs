//! Учёт токенов и расходов на модели, дневной лимит.
//!
//! Каждый запрос к модели добавляет токены в счётчик (`Meter`) своей работы:
//! решения, пробного анализа, разметки документов. Счётчик общий с диалогом и
//! переживает прерванное по таймауту решение: потраченные токены учитываются
//! всегда. Итог пишется в `ai_usage`, дневной лимит — сумма по московским суткам.
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, FixedOffset, Utc};
use rust_decimal::{Decimal, prelude::FromPrimitive};
use serde::Serialize;
use serde_json::Value;
use tinkoff_rs::{Result, storage::PostgresStorage};

use super::profiles::{Currency, Prices};

/// Токены и платные вызовы инструментов провайдера.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Usage {
    pub requests: u32,
    /// Входные токены без кэша промпта.
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub web_searches: u32,
}

fn tokens(value: &Value) -> u64 {
    value.as_u64().unwrap_or_default()
}

impl Usage {
    /// `usage` ответа Anthropic Messages.
    pub(crate) fn add_anthropic(&mut self, usage: &Value) {
        self.requests += 1;
        self.input_tokens += tokens(&usage["input_tokens"]);
        self.output_tokens += tokens(&usage["output_tokens"]);
        self.cache_read_tokens += tokens(&usage["cache_read_input_tokens"]);
        self.cache_write_tokens += tokens(&usage["cache_creation_input_tokens"]);
        self.web_searches +=
            u32::try_from(tokens(&usage["server_tool_use"]["web_search_requests"]))
                .unwrap_or(u32::MAX);
    }

    /// `usage` ответа Chat Completions: `prompt_tokens` уже включает кэш.
    pub(crate) fn add_openai(&mut self, usage: &Value) {
        self.requests += 1;
        let cached = tokens(&usage["prompt_tokens_details"]["cached_tokens"]);
        self.input_tokens += tokens(&usage["prompt_tokens"]).saturating_sub(cached);
        self.cache_read_tokens += cached;
        self.output_tokens += tokens(&usage["completion_tokens"]);
    }

    /// Токены сервиса векторов: только вход.
    pub(crate) fn add_embedding(&mut self, input_tokens: u64) {
        self.requests += 1;
        self.input_tokens += input_tokens;
    }

    /// Стоимость в валюте цен профиля; без цен — неизвестна.
    pub(crate) fn cost(&self, prices: Option<&Prices>) -> Option<f64> {
        let prices = prices?;
        let million = |count: u64, price: f64| count as f64 * price / 1_000_000.0;
        Some(
            million(self.input_tokens, prices.input)
                + million(self.output_tokens, prices.output)
                + million(
                    self.cache_read_tokens,
                    prices.cache_read.unwrap_or(prices.input),
                )
                + million(
                    self.cache_write_tokens,
                    prices.cache_write.unwrap_or(prices.input),
                )
                + f64::from(self.web_searches) * prices.web_search_per_1000.unwrap_or(0.0) / 1000.0,
        )
    }
}

/// Расход одной работы: чья модель и сколько потрачено.
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct Spend {
    pub profile: Option<String>,
    pub model: Option<String>,
    pub usage: Usage,
    #[serde(skip)]
    pub prices: Option<Prices>,
}

impl Spend {
    /// Стоимость в валюте цен профиля.
    pub(crate) fn cost(&self) -> Option<f64> {
        self.usage.cost(self.prices.as_ref())
    }

    fn currency(&self) -> Currency {
        self.prices.as_ref().map_or(Currency::Usd, |p| p.currency)
    }

    /// Стоимость в рублях по курсу доллара `usd_rub`.
    pub(crate) fn cost_rub(&self, usd_rub: f64) -> Option<f64> {
        let cost = self.cost()?;
        Some(match self.currency() {
            Currency::Rub => cost,
            Currency::Usd => cost * usd_rub,
        })
    }

    /// Стоимость в долларах по курсу `usd_rub`.
    pub(crate) fn cost_usd(&self, usd_rub: f64) -> Option<f64> {
        let cost = self.cost()?;
        Some(match self.currency() {
            Currency::Usd => cost,
            Currency::Rub => cost / usd_rub,
        })
    }

    /// Краткая сводка для журнала решений.
    pub(crate) fn summary(&self, usd_rub: f64) -> Value {
        serde_json::json!({
            "requests": self.usage.requests, "input_tokens": self.usage.input_tokens,
            "output_tokens": self.usage.output_tokens, "cache_read_tokens": self.usage.cache_read_tokens,
            "cache_write_tokens": self.usage.cache_write_tokens, "web_searches": self.usage.web_searches,
            "cost_rub": self.cost_rub(usd_rub), "cost_usd": self.cost_usd(usd_rub),
        })
    }
}

/// Курс доллара без данных в базе: с запасом, чтобы лимит не превышался.
pub(crate) const FALLBACK_USD_RUB: f64 = 100.0;

/// Курс доллара к рублю, который наблюдатель берёт из T-Invest.
pub(crate) async fn usd_rub(storage: &PostgresStorage) -> f64 {
    sqlx::query_scalar::<_, String>("SELECT rub::text FROM fx_rates WHERE currency = 'usd'")
        .fetch_optional(storage.pool())
        .await
        .ok()
        .flatten()
        .and_then(|text| text.parse::<f64>().ok())
        .filter(|rate| rate.is_finite() && *rate > 0.0)
        .unwrap_or(FALLBACK_USD_RUB)
}

/// Общий счётчик работы: его пополняет диалог, а читает тот, кто пишет журнал.
pub(crate) type Meter = Arc<Mutex<Spend>>;

pub(crate) fn meter() -> Meter {
    Arc::new(Mutex::new(Spend::default()))
}

/// Снимок счётчика (замок не держится дольше копирования).
pub(crate) fn snapshot(meter: &Meter) -> Spend {
    meter.lock().map_or_else(
        |poisoned| poisoned.into_inner().clone(),
        |spend| spend.clone(),
    )
}

fn decimal(cost: Option<f64>) -> Option<Decimal> {
    cost.and_then(Decimal::from_f64)
        .map(|cost| cost.round_dp(6))
}

/// Записывает расход работы; пустой (ни одного запроса) не пишется.
pub(crate) async fn store(
    storage: &PostgresStorage,
    purpose: &str,
    strategy_id: Option<&str>,
    spend: &Spend,
) -> Result<()> {
    if spend.usage.requests == 0 {
        return Ok(());
    }
    let count = |value: u64| i64::try_from(value).unwrap_or(i64::MAX);
    let rate = usd_rub(storage).await;
    sqlx::query(
        "INSERT INTO ai_usage(purpose,strategy_id,profile,model,requests,input_tokens,output_tokens,\
         cache_read_tokens,cache_write_tokens,web_searches,cost_usd,cost_rub) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)",
    )
    .bind(purpose)
    .bind(strategy_id)
    .bind(spend.profile.as_deref())
    .bind(spend.model.as_deref())
    .bind(i32::try_from(spend.usage.requests).unwrap_or(i32::MAX))
    .bind(count(spend.usage.input_tokens))
    .bind(count(spend.usage.output_tokens))
    .bind(count(spend.usage.cache_read_tokens))
    .bind(count(spend.usage.cache_write_tokens))
    .bind(i32::try_from(spend.usage.web_searches).unwrap_or(i32::MAX))
    .bind(decimal(spend.cost_usd(rate)))
    .bind(decimal(spend.cost_rub(rate)))
    .execute(storage.pool())
    .await?;
    Ok(())
}

/// Начало текущих московских суток.
pub(crate) fn moscow_day_start(now: DateTime<Utc>) -> DateTime<Utc> {
    let moscow = FixedOffset::east_opt(3 * 3600).expect("valid offset");
    let local = now.with_timezone(&moscow);
    let midnight = local
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("valid midnight");
    (midnight - Duration::hours(3)).and_utc()
}

/// Дневной лимит расходов на модели по умолчанию, ₽ (около $5–6).
pub(crate) const DEFAULT_BUDGET_RUB: f64 = 500.0;

/// Лимит и расход за московские сутки, в рублях.
#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct BudgetState {
    pub limit_rub: Option<f64>,
    pub spent_rub: f64,
}

impl BudgetState {
    /// Лимит исчерпан: фоновая работа и внеочередные решения ждут следующих суток.
    pub(crate) fn exhausted(&self) -> bool {
        self.limit_rub.is_some_and(|limit| self.spent_rub >= limit)
    }
}

/// Лимит из настроек панели и расход за сегодня; старые записи без рублёвой
/// стоимости пересчитываются по текущему курсу.
pub(crate) async fn budget(storage: &PostgresStorage) -> Result<BudgetState> {
    let limit: Option<String> =
        sqlx::query_scalar("SELECT ai_budget_rub::text FROM dashboard_settings WHERE id")
            .fetch_optional(storage.pool())
            .await
            .ok()
            .flatten();
    let rate = usd_rub(storage).await;
    let spent: Option<Decimal> = sqlx::query_scalar(
        "SELECT sum(coalesce(cost_rub, cost_usd * $2::numeric)) FROM ai_usage WHERE at >= $1",
    )
    .bind(moscow_day_start(Utc::now()))
    .bind(rate.to_string())
    .fetch_one(storage.pool())
    .await?;
    Ok(BudgetState {
        limit_rub: Some(
            limit
                .and_then(|text| text.parse::<f64>().ok())
                .unwrap_or(DEFAULT_BUDGET_RUB),
        ),
        spent_rub: spent
            .and_then(|value| value.to_string().parse().ok())
            .unwrap_or(0.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_of_both_protocols_is_priced_with_cache_and_search() {
        let mut usage = Usage::default();
        usage.add_anthropic(&json!({"input_tokens": 1000, "output_tokens": 500,
            "cache_read_input_tokens": 20_000, "cache_creation_input_tokens": 4000,
            "server_tool_use": {"web_search_requests": 2}}));
        usage.add_openai(&json!({"prompt_tokens": 3000, "completion_tokens": 100,
            "prompt_tokens_details": {"cached_tokens": 1000}}));
        assert_eq!(
            usage,
            Usage {
                requests: 2,
                input_tokens: 3000,
                output_tokens: 600,
                cache_read_tokens: 21_000,
                cache_write_tokens: 4000,
                web_searches: 2,
            }
        );
        let prices: Prices = toml::from_str(
            "input = 3\noutput = 15\ncache_write = 3.75\ncache_read = 0.3\nweb_search_per_1000 = 10",
        )
        .unwrap();
        // 3000×3 + 600×15 + 21000×0.3 + 4000×3.75 = 39300 per million, plus two searches.
        let cost = usage.cost(Some(&prices)).unwrap();
        assert!((cost - (0.0393 + 0.02)).abs() < 1e-9, "{cost}");
        assert_eq!(usage.cost(None), None, "no prices, no cost");
    }

    #[test]
    fn budget_days_follow_moscow_midnight() {
        let late_evening = DateTime::parse_from_rfc3339("2026-10-19T22:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            moscow_day_start(late_evening).to_rfc3339(),
            "2026-10-19T21:00:00+00:00",
            "01:30 in Moscow belongs to the new day"
        );
        let state = BudgetState {
            limit_rub: Some(500.0),
            spent_rub: 500.0,
        };
        assert!(state.exhausted());
        assert!(
            !BudgetState {
                limit_rub: None,
                spent_rub: 10_000.0
            }
            .exhausted()
        );
    }
}
