//! Стратегия `ai-model`: профили моделей, новости, инструменты и цикл решения.
//!
//! Подключение к модели задаётся в TOML-файле профилей (`AI_MODELS_FILE`):
//! провайдер, URL, модель и токен. Поддерживаются два протокола: Anthropic
//! Messages и OpenAI-совместимый Chat Completions (`OpenAI`, `OpenRouter`, `DeepSeek`,
//! Ollama, LM Studio, vLLM и шлюзы к другим моделям).
//!
//! Модель получает инструменты блока, текущий портфель и лимиты, сама вызывает
//! инструменты (новости, свечи, индикаторы, мультипликаторы) и завершает работу
//! `submit_decision`. Тексты новостей — данные, а не инструкции; лимиты весов
//! применяет стратегия, а заявки проходят обычный risk manager.
// Индикаторы — аналитика для модели, а не торговые суммы: точности f64 достаточно,
// окна не длиннее 250 свечей.
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
use std::time::Duration as StdDuration;

use serde::Serialize;
use serde_json::Value;
use tinkoff_rs::{
    Error, Result,
    domain::{AccountId, InstrumentId},
    engine::ModelAdvice,
    proto::{GetFuturesMarginRequest, InstrumentIdType, InstrumentRequest, MoneyValue, Quotation},
    storage::PostgresStorage,
    strategies::AiModelConfig,
};

use crate::{VenueApi, VenueBroker, documents::rss::NewsItem};

mod calibration;
pub(crate) mod client;
mod decision;
mod indicators;
pub(crate) mod market;
mod memory;
mod outcomes;
pub(crate) mod profiles;
mod reflection;
#[cfg(test)]
pub(crate) mod testkit;
mod theses;
mod tools;
pub(crate) mod usage;
mod watch;

pub(crate) use calibration::calibrate;
pub(crate) use decision::{Entry, advise, check, record};
pub(crate) use outcomes::compute as compute_outcomes;
pub(crate) use profiles::{Prices, Profiles, Provider};
pub(crate) use reflection::review_if_due;
pub(crate) use watch::Watch;

/// Общий лимит времени на одно решение модели вместе с вызовами инструментов.
pub(crate) const DECISION_DEADLINE: StdDuration = StdDuration::from_secs(600);
/// Сколько свежих заголовков модель видит сразу, без вызова `get_news`.
pub(crate) const HEADLINES_IN_CONTEXT: usize = 30;
/// Ответ инструмента обрезается, чтобы не раздувать контекст модели.
pub(crate) const TOOL_RESULT_LIMIT: usize = 20_000;

pub(crate) fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_owned()
    } else {
        text.chars().take(limit).collect::<String>() + "…"
    }
}

/// Инструмент блока, как его видит модель.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Instrument {
    pub(crate) uid: InstrumentId,
    pub(crate) ticker: String,
    pub(crate) name: String,
    pub(crate) lot: i32,
    /// Тип: share, etf, bond, futures, currency.
    pub(crate) kind: String,
    /// Что важно о типе: у фьючерса базовый актив, контракт, стоимость пункта и
    /// ГО, у облигации номинал, погашение, купон и НКД.
    pub(crate) details: Value,
}

impl Instrument {
    /// Бумага как предмет поиска документов.
    pub(crate) fn subject(&self) -> crate::documents::search::Subject {
        crate::documents::search::Subject {
            uid: self.uid.to_string(),
            ticker: self.ticker.clone(),
            name: self.name.clone(),
        }
    }
}

/// Одна задача решения: компонента `ai-model` и её инструменты.
#[derive(Clone, Debug)]
pub(crate) struct AiJob {
    pub universe: Vec<InstrumentId>,
    pub config: AiModelConfig,
}

/// Почему модель принимает решение сейчас.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Trigger {
    /// `session_open`, `scheduled`, `news_burst`, `price_jump` или `preview`.
    pub kind: &'static str,
    pub details: String,
}

impl Trigger {
    pub(crate) fn new(kind: &'static str, details: impl Into<String>) -> Self {
        Self {
            kind,
            details: details.into(),
        }
    }

    pub(crate) fn label(&self) -> String {
        let kind = match self.kind {
            "session_open" => "открытие торговой сессии",
            "scheduled" => "плановый пересмотр",
            "news_burst" => "внеочередное решение: всплеск новостей",
            "price_jump" => "внеочередное решение: резкое движение цены",
            "material_event" => "внеочередное решение: важное событие эмитента",
            "macro_shock" => "внеочередное решение: резкое движение рынка",
            "thesis_condition" => "внеочередное решение: сработало условие тезиса",
            _ => "пробный анализ без заявок",
        };
        if self.details.is_empty() {
            kind.to_owned()
        } else {
            format!("{kind} — {}", self.details)
        }
    }
}

/// Зависимости решения: рынок, счёт, брокер и журнал (для истории и памяти).
pub(crate) struct Context<'a> {
    pub api: &'a VenueApi,
    pub broker: &'a VenueBroker,
    pub account_id: &'a AccountId,
    pub storage: Option<&'a PostgresStorage>,
    pub strategy_id: &'a str,
    /// Пробный анализ читает память, но не меняет её.
    pub persist_memory: bool,
}

/// Результат решения вместе с аудитом: вызовы инструментов, источники новостей.
#[derive(Debug, Serialize)]
pub(crate) struct AdviceRun {
    pub advice: ModelAdvice,
    pub profile: String,
    pub provider: Provider,
    pub trigger: Trigger,
    /// Модель решила ничего не менять.
    pub hold: bool,
    pub weights: Vec<Value>,
    pub sources: Vec<Value>,
    pub tool_calls: Vec<Value>,
    pub feed_errors: Vec<String>,
    pub confidence_percent: Option<f64>,
    /// Цены бумаг блока и индекса в момент решения: от них считаются итоги.
    pub prices: std::collections::HashMap<String, f64>,
}

/// Слова, по которым новость относится к инструменту: тикер и основа названия.
///
/// Первое слово названия без падежного окончания: «Сбербанк» находит «Сбербанка».
pub(crate) fn keywords(instrument: &Instrument) -> Vec<String> {
    let mut words = vec![instrument.ticker.to_lowercase()];
    if let Some(word) = instrument
        .name
        .split_whitespace()
        .find(|w| w.chars().count() >= 4)
    {
        let stem: String = word
            .to_lowercase()
            .chars()
            .take(word.chars().count().saturating_sub(1).max(4))
            .collect();
        words.push(stem.trim_matches(|c: char| !c.is_alphanumeric()).to_owned());
    }
    words.retain(|w| w.chars().count() >= 3);
    words
}

pub(crate) fn mentions(item: &NewsItem, words: &[String]) -> bool {
    let text = format!("{} {}", item.title, item.summary).to_lowercase();
    words.iter().any(|w| text.contains(w.as_str()))
}

fn number(units: i64, nano: i32) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let value = units as f64 + f64::from(nano) / 1e9;
    value
}

fn quotation(value: Option<&Quotation>) -> Option<f64> {
    value.map(|q| number(q.units, q.nano))
}

fn money(value: Option<&MoneyValue>) -> Option<f64> {
    value.map(|m| number(m.units, m.nano))
}

/// Отметка для мониторинга в панели: модель портфеля начала думать. Сбой
/// записи не мешает решению.
pub(crate) async fn activity_started(
    storage: &PostgresStorage,
    strategy_id: &str,
    job: &AiJob,
    trigger: &Trigger,
) {
    let result = sqlx::query(
        "INSERT INTO ai_activity(strategy_id, state, trigger, details, profile, started_at) VALUES($1, 'thinking', $2, $3, $4, now()) \
         ON CONFLICT (strategy_id) DO UPDATE SET state = 'thinking', trigger = EXCLUDED.trigger, details = EXCLUDED.details, \
         profile = EXCLUDED.profile, started_at = now(), finished_at = NULL, status = NULL",
    )
    .bind(strategy_id)
    .bind(trigger.kind)
    .bind(truncate(&trigger.details, 300))
    .bind(job.config.model_profile.as_deref())
    .execute(storage.pool())
    .await;
    if let Err(error) = result {
        tracing::warn!(%error, "ai activity was not recorded");
    }
}

/// Модель ответила (`ok`) или нет (`failed`).
pub(crate) async fn activity_finished(storage: &PostgresStorage, strategy_id: &str, status: &str) {
    let result = sqlx::query("UPDATE ai_activity SET state = 'idle', finished_at = now(), status = $2 WHERE strategy_id = $1")
        .bind(strategy_id)
        .bind(status)
        .execute(storage.pool())
        .await;
    if let Err(error) = result {
        tracing::warn!(%error, "ai activity was not recorded");
    }
}

/// Инструменты блока для модели. Серия фьючерсов описывается текущим контрактом:
/// тикер — код серии (BR), название — базовый актив.
pub(crate) async fn instruments(
    api: &VenueApi,
    universe: &[InstrumentId],
) -> Result<Vec<Instrument>> {
    let mut out = Vec::with_capacity(universe.len());
    for uid in universe {
        let contract = api.series().contract(uid);
        let request = || InstrumentRequest {
            id_type: InstrumentIdType::Uid as i32,
            class_code: None,
            id: contract.to_string(),
        };
        let instrument = api
            .instruments()
            .get_instrument_by(request())
            .await?
            .into_inner()
            .instrument
            .ok_or_else(|| Error::Configuration(format!("instrument {uid} is unavailable")))?;
        let series = crate::instruments::series_code(uid);
        let (mut ticker, mut name) = (instrument.ticker.clone(), instrument.name.clone());
        let details = match instrument.instrument_type.as_str() {
            "futures" => {
                let future = api
                    .instruments()
                    .future_by(request())
                    .await?
                    .into_inner()
                    .instrument
                    .unwrap_or_default();
                let margin = api
                    .instruments()
                    .get_futures_margin(GetFuturesMarginRequest {
                        instrument_id: contract.to_string(),
                        ..Default::default()
                    })
                    .await?
                    .into_inner();
                let point = match (
                    quotation(margin.min_price_increment_amount.as_ref()),
                    quotation(margin.min_price_increment.as_ref()),
                ) {
                    (Some(amount), Some(step)) if step > 0.0 => Some(amount / step),
                    _ => None,
                };
                if let Some(code) = series {
                    ticker = code.to_owned();
                    name = future.basic_asset.clone();
                }
                serde_json::json!({
                    "basic_asset": future.basic_asset, "contract": instrument.ticker,
                    "continuous": series.is_some(),
                    "roll_at": series.and_then(|code| api.series().bound_contract(code)).map(|c| c.roll_at()),
                    "last_trade_date": future.last_trade_date.and_then(|t| tinkoff_rs::domain::timestamp_to_datetime(t).ok()),
                    "point_value_rub": point,
                    "margin_buy_rub": money(margin.initial_margin_on_buy.as_ref()).filter(|v| *v > 0.0),
                    "margin_sell_rub": money(margin.initial_margin_on_sell.as_ref()).filter(|v| *v > 0.0),
                    "price_note": "цена в пунктах; рублёвая стоимость контракта = цена × point_value_rub",
                })
            }
            "bond" => {
                let bond = api
                    .instruments()
                    .bond_by(request())
                    .await?
                    .into_inner()
                    .instrument
                    .unwrap_or_default();
                serde_json::json!({
                    "nominal_rub": money(bond.nominal.as_ref()),
                    "maturity_date": bond.maturity_date.and_then(|t| tinkoff_rs::domain::timestamp_to_datetime(t).ok()),
                    "coupons_per_year": bond.coupon_quantity_per_year,
                    "accrued_interest_rub": money(bond.aci_value.as_ref()),
                    "floating_coupon": bond.floating_coupon_flag,
                    "amortization": bond.amortization_flag,
                    "price_note": "цена в процентах номинала, сверху платится НКД",
                })
            }
            _ => serde_json::json!({}),
        };
        out.push(Instrument {
            uid: uid.clone(),
            ticker,
            name,
            lot: instrument.lot,
            kind: instrument.instrument_type,
            details,
        });
    }
    Ok(out)
}
