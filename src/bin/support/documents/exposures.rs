//! Чувствительность бумаг к темам рынка и ожидаемое влияние событий.
//!
//! Черновик профиля бумаги (отрасль, чем зарабатывает, к каким темам и как
//! чувствительна) один раз пишет модель; оператор правит значения в панели, и
//! модель их больше не трогает. Влияние события на бумагу — сумма по темам
//! события: знак темы × сила × чувствительность бумаги, делённая на 3.
use std::collections::HashMap;

use serde_json::{Value, json};
use tinkoff_rs::{Result, storage::PostgresStorage};

use super::{clean, sources::Tagging, topics};
use crate::ai::{client::Chat, profiles::Profiles, usage};

/// Черновиков за один проход сборщика.
const DRAFTS_PER_TICK: i64 = 3;

/// Бумага стратегии: появляется в списке на черновик профиля.
pub(crate) async fn register(
    storage: &PostgresStorage,
    uid: &str,
    ticker: &str,
    name: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO instrument_profiles(ticker, uid, name) VALUES($1, $2, $3) \
         ON CONFLICT (ticker) DO UPDATE SET uid = EXCLUDED.uid, name = EXCLUDED.name \
         WHERE instrument_profiles.uid IS DISTINCT FROM EXCLUDED.uid OR instrument_profiles.name <> EXCLUDED.name",
    )
    .bind(ticker)
    .bind(uid)
    .bind(name)
    .execute(storage.pool())
    .await?;
    Ok(())
}

fn tool() -> Value {
    json!({"name": "submit_profile", "description": "Профиль бумаги: отрасль, бизнес и чувствительность к темам.",
        "parameters": {"type": "object", "properties": {
            "sector": {"type": "string", "description": "Отрасль, коротко"},
            "note": {"type": "string", "description": "1–2 предложения: чем зарабатывает компания и что сильнее всего влияет на её акции"},
            "exposures": {"type": "array", "items": {"type": "object", "properties": {
                "topic": {"type": "string", "enum": topics::ids()},
                "sensitivity": {"type": "number", "minimum": -2, "maximum": 2},
                "note": {"type": "string"}}, "required": ["topic", "sensitivity"]}}},
            "required": ["sector", "note", "exposures"]}})
}

fn prompt(ticker: &str, name: &str) -> String {
    format!(
        "Опиши акции {name} ({ticker}, Московская биржа) для торговой модели: отрасль, бизнес и чувствительность к \
         темам рынка. Чувствительность от -2 до +2 — насколько акциям хорошо движение темы вверх (+1 по определению \
         темы ниже): +2 — сильно выигрывают, -2 — сильно проигрывают. Указывай только значимые темы (обычно 4–10), \
         включая общерыночные (геополитика, санкции, ключевая ставка), и коротко объясняй почему. Темы:\n{}",
        topics::describe()
    )
}

/// Черновики профилей для новых бумаг. Возвращает число готовых и ошибки.
pub(crate) async fn draft(
    storage: &PostgresStorage,
    settings: &Tagging,
) -> Result<(usize, Vec<String>)> {
    let pending: Vec<(String, String)> = sqlx::query_as(
        "SELECT ticker, name FROM instrument_profiles WHERE drafted_at IS NULL \
           AND (draft_error IS NULL OR updated_at < now() - interval '6 hours') ORDER BY ticker LIMIT $1",
    )
    .bind(DRAFTS_PER_TICK)
    .fetch_all(storage.pool())
    .await?;
    if pending.is_empty() || usage::budget(storage).await?.exhausted() {
        return Ok((0, Vec::new()));
    }
    let profiles = Profiles::load()?;
    let (name, profile) = profiles.get(settings.exposure_profile.as_deref())?;
    let mut profile = profile.clone();
    profile.web_search = false;
    let (mut done, mut errors) = (0, Vec::new());
    for (ticker, title) in pending {
        let meter = usage::meter();
        let result = async {
            let mut chat = Chat::new(
                name.clone(),
                &profile,
                "Ты аналитик российского фондового рынка. Отвечай только вызовом submit_profile."
                    .to_owned(),
                meter.clone(),
            )?;
            chat.user(&prompt(&ticker, &title));
            chat.step(&[tool()], Some("submit_profile")).await
        }
        .await;
        usage::store(storage, "exposures", None, &usage::snapshot(&meter)).await?;
        let input = match result {
            Ok(turn) => turn
                .calls
                .into_iter()
                .find(|c| c.name == "submit_profile")
                .map(|c| c.input),
            Err(error) => {
                errors.push(format!("{ticker}: {error}"));
                sqlx::query("UPDATE instrument_profiles SET draft_error = $2, updated_at = now() WHERE ticker = $1")
                    .bind(&ticker)
                    .bind(error.to_string().chars().take(500).collect::<String>())
                    .execute(storage.pool())
                    .await?;
                continue;
            }
        };
        let Some(input) = input else {
            continue;
        };
        save_draft(storage, &ticker, &input).await?;
        done += 1;
    }
    Ok((done, errors))
}

/// Черновик модели; значения оператора не перезаписываются.
pub(crate) async fn save_draft(
    storage: &PostgresStorage,
    ticker: &str,
    input: &Value,
) -> Result<()> {
    let mut transaction = storage.pool().begin().await?;
    sqlx::query(
        "UPDATE instrument_profiles SET sector = $2, note = $3, drafted_at = now(), draft_error = NULL, updated_at = now() WHERE ticker = $1",
    )
    .bind(ticker)
    .bind(clean(input["sector"].as_str().unwrap_or_default(), 100))
    .bind(clean(input["note"].as_str().unwrap_or_default(), 600))
    .execute(&mut *transaction)
    .await?;
    for exposure in input["exposures"].as_array().into_iter().flatten() {
        let (Some(topic), Some(value)) = (
            exposure["topic"].as_str().and_then(topics::find),
            exposure["sensitivity"].as_f64().filter(|v| v.is_finite()),
        ) else {
            continue;
        };
        let value = (value.clamp(-2.0, 2.0) * 10.0).round() / 10.0;
        sqlx::query(
            "INSERT INTO instrument_exposures(ticker, topic, sensitivity, note) VALUES($1, $2, $3::numeric, $4) \
             ON CONFLICT (ticker, topic) DO UPDATE SET sensitivity = EXCLUDED.sensitivity, note = EXCLUDED.note, updated_at = now() \
             WHERE instrument_exposures.origin = 'model'",
        )
        .bind(ticker)
        .bind(topic.id)
        .bind(value.to_string())
        .bind(clean(exposure["note"].as_str().unwrap_or_default(), 300))
        .execute(&mut *transaction)
        .await?;
    }
    transaction.commit().await?;
    Ok(())
}

/// Чувствительность бумаг: тикер → тема → значение.
#[derive(Clone, Debug, Default)]
pub(crate) struct Exposures(pub HashMap<String, HashMap<String, f64>>);

/// Вклад события в бумагу.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Impact {
    pub ticker: String,
    /// Больше нуля — хорошо для бумаги.
    pub value: f64,
    /// Тема с наибольшим вкладом.
    pub topic: &'static str,
}

impl Exposures {
    pub(crate) async fn load(storage: &PostgresStorage, tickers: &[String]) -> Result<Self> {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT ticker, topic, sensitivity::text FROM instrument_exposures WHERE ticker = ANY($1)",
        )
        .bind(tickers)
        .fetch_all(storage.pool())
        .await?;
        let mut map: HashMap<String, HashMap<String, f64>> = HashMap::new();
        for (ticker, topic, value) in rows {
            if let Ok(value) = value.parse() {
                map.entry(ticker).or_default().insert(topic, value);
            }
        }
        Ok(Self(map))
    }

    /// Влияние разметки событий на бумаги, сильные сначала.
    pub(crate) fn impacts(&self, topics: &Value) -> Vec<Impact> {
        let mut out: Vec<Impact> = self
            .0
            .iter()
            .filter_map(|(ticker, sensitivity)| {
                let mut total = 0.0;
                let mut strongest: Option<(&'static str, f64)> = None;
                for tag in topics.as_array().into_iter().flatten() {
                    let Some(topic) = tag["topic"].as_str().and_then(topics::find) else {
                        continue;
                    };
                    let direction = tag["direction"].as_f64().unwrap_or(0.0);
                    let strength = tag["strength"].as_f64().unwrap_or(1.0);
                    let part =
                        direction * strength * sensitivity.get(topic.id).copied().unwrap_or(0.0)
                            / 3.0;
                    total += part;
                    if strongest.is_none_or(|(_, best)| part.abs() > best.abs()) {
                        strongest = Some((topic.id, part));
                    }
                }
                let (topic, _) = strongest.filter(|(_, part)| part.abs() > 0.0)?;
                Some(Impact {
                    ticker: ticker.clone(),
                    value: (total * 100.0).round() / 100.0,
                    topic,
                })
            })
            // Слабее половины балла — шум: сила 1 при чувствительности 1 даёт 0,33.
            .filter(|impact| impact.value.abs() >= 0.5)
            .collect();
        out.sort_by(|a, b| {
            b.value
                .abs()
                .total_cmp(&a.value.abs())
                .then(a.ticker.cmp(&b.ticker))
        });
        out
    }
}

/// Профили бумаг для модели и панели; `measured` — измеренная реакция на темы
/// (см. калибровку), где она заметна.
pub(crate) async fn describe(storage: &PostgresStorage, tickers: &[String]) -> Result<Value> {
    Ok(sqlx::query_scalar(
        "SELECT coalesce(json_agg(json_build_object('ticker', p.ticker, 'sector', p.sector, 'note', p.note, \
           'drafted', p.drafted_at IS NOT NULL, 'confirmed', p.confirmed_at IS NOT NULL, \
           'exposures', (SELECT coalesce(json_agg(json_build_object('topic', e.topic, 'sensitivity', e.sensitivity, 'note', e.note, \
              'origin', e.origin) ORDER BY abs(e.sensitivity) DESC, e.topic), '[]') FROM instrument_exposures e WHERE e.ticker = p.ticker), \
           'measured', (SELECT coalesce(json_agg(json_build_object('topic', m.topic, 'effect_percent', m.effect_percent, \
              'observations', m.observations, 'correlation', m.correlation, 'suggested', m.suggested) ORDER BY abs(m.effect_percent) DESC), '[]') \
              FROM exposure_suggestions m WHERE m.ticker = p.ticker AND m.suggested <> 0)) \
           ORDER BY p.ticker), '[]') FROM instrument_profiles p WHERE p.ticker = ANY($1)",
    )
    .bind(tickers)
    .fetch_one(storage.pool())
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_move_exporters_and_banks_in_opposite_directions() {
        let exposures = Exposures(HashMap::from([
            (
                "LKOH".to_owned(),
                HashMap::from([
                    ("oil".to_owned(), 2.0),
                    ("ruble".to_owned(), -1.0),
                    ("sanctions".to_owned(), 1.0),
                ]),
            ),
            (
                "SBER".to_owned(),
                HashMap::from([("key_rate".to_owned(), -1.0), ("sanctions".to_owned(), 1.0)]),
            ),
            (
                "MGNT".to_owned(),
                HashMap::from([("consumer".to_owned(), 1.0)]),
            ),
        ]));
        // Brent up strongly, sanctions tightened.
        let event = json!([{"topic": "oil", "direction": 1, "strength": 3}, {"topic": "sanctions", "direction": -1, "strength": 2}]);
        let impacts = exposures.impacts(&event);
        assert_eq!(
            impacts,
            vec![
                Impact {
                    ticker: "LKOH".to_owned(),
                    value: 1.33,
                    topic: "oil"
                },
                Impact {
                    ticker: "SBER".to_owned(),
                    value: -0.67,
                    topic: "sanctions"
                },
            ],
            "a consumer stock is not touched"
        );
        assert!(
            exposures
                .impacts(&json!([{"topic": "key_rate", "direction": -1, "strength": 1}]))
                .is_empty(),
            "too weak to show"
        );
        let cut =
            exposures.impacts(&json!([{"topic": "key_rate", "direction": -1, "strength": 3}]));
        assert_eq!(
            (cut[0].ticker.as_str(), cut[0].value),
            ("SBER", 1.0),
            "a rate cut helps the bank"
        );
    }
}
