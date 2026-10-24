//! Состояние рынка для модели и поводов к решению: индекс Мосбиржи, индекс
//! гособлигаций, нефть Brent (ближайший фьючерс), юань, доллар, золото и
//! ключевая ставка. Так модель видит, как рынок уже отреагировал на события.
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use tinkoff_rs::{
    Result,
    domain::{CandleHistoryRequest, CandleInterval, InstrumentId, timestamp_to_datetime},
    proto::{GetLastPricesRequest, InstrumentStatus, InstrumentsRequest},
};

use super::indicators::{number, round};
// Фьючерс Brent переходит на следующий контракт за ROLL_DAYS дней до исполнения.
use crate::rules::ROLL_DAYS;
use crate::{VenueApi, VenueBroker, history};

/// Вид ряда: от него зависит порог внеочередного решения.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Class {
    Index,
    Commodity,
    Currency,
    Other,
}

/// Один ряд рыночного контекста.
#[derive(Clone, Debug)]
pub(crate) struct Series {
    pub key: &'static str,
    pub name: &'static str,
    pub uid: String,
    pub class: Class,
}

/// Индекс Мосбиржи — ориентир итогов решений.
pub(crate) const IMOEX: &str = "4821c9aa-36e8-4743-b37c-861e58581b25";
/// Постоянные UID T-Invest (индексы, валюты, золото).
const FIXED: &[(&str, &str, &str, Class)] = &[
    ("IMOEX", "индекс Мосбиржи", IMOEX, Class::Index),
    (
        "RGBI",
        "индекс гособлигаций RGBI",
        "fceffb27-3c51-4101-834c-d28c98ada458",
        Class::Other,
    ),
    (
        "CNYRUB",
        "юань, ₽",
        "4587ab1d-a9c9-4910-a0d6-86c7b9c42510",
        Class::Currency,
    ),
    (
        "USDRUB",
        "доллар, ₽",
        "a22a1263-8e1b-4546-a1aa-416463f104d3",
        Class::Currency,
    ),
    (
        "GOLD",
        "золото, ₽ за грамм",
        "258e2b93-54e8-4f2d-ba3d-a507c47e3ae2",
        Class::Other,
    ),
];
/// Ключевая ставка Банка России (индикатор, только последнее значение).
const KEY_RATE: &str = "e8442bbf-829d-4c24-8582-7fcf85fa511c";
/// Ближайший фьючерс Brent ищется раз в сутки.
static BRENT: Mutex<Option<(DateTime<Utc>, Option<Series>)>> = Mutex::new(None);

/// Ближайший полноразмерный фьючерс на Brent (BR, не мини).
async fn brent(api: &VenueApi) -> Option<Series> {
    let now = Utc::now();
    if let Ok(cache) = BRENT.lock() {
        if let Some((_, series)) = cache
            .as_ref()
            .filter(|(at, _)| now - *at < Duration::hours(24))
        {
            return series.clone();
        }
    }
    let found = api
        .instruments()
        .futures(InstrumentsRequest {
            instrument_status: Some(InstrumentStatus::Base as i32),
            instrument_exchange: None,
        })
        .await
        .ok()?
        .into_inner()
        .instruments
        .into_iter()
        .filter(|f| f.basic_asset.eq_ignore_ascii_case("brent") && f.name.starts_with("BR-"))
        .filter_map(|f| {
            let expires = timestamp_to_datetime(f.expiration_date?).ok()?;
            (expires > now + Duration::days(ROLL_DAYS)).then_some((expires, f))
        })
        .min_by_key(|(expires, _)| *expires)
        .map(|(_, f)| Series {
            key: "BRENT",
            name: "нефть Brent, $ (ближайший фьючерс)",
            uid: f.uid,
            class: Class::Commodity,
        });
    if let Ok(mut cache) = BRENT.lock() {
        *cache = Some((now, found.clone()));
    }
    found
}

/// Ряды контекста: постоянные и текущий фьючерс Brent.
pub(crate) async fn series(api: &VenueApi) -> Vec<Series> {
    let mut out: Vec<Series> = FIXED
        .iter()
        .map(|(key, name, uid, class)| Series {
            key,
            name,
            uid: (*uid).to_owned(),
            class: *class,
        })
        .collect();
    out.extend(brent(api).await);
    out
}

/// Последние цены рядов одним запросом.
pub(crate) async fn last_prices(api: &VenueApi, uids: &[String]) -> Result<Vec<(String, f64)>> {
    // Серия фьючерсов котируется по привязанному контракту.
    let contracts: Vec<String> = uids
        .iter()
        .map(|uid| {
            InstrumentId::new(uid.clone())
                .map_or_else(|_| uid.clone(), |id| api.series().contract(&id).to_string())
        })
        .collect();
    let response = api
        .market_data()
        .get_last_prices(GetLastPricesRequest {
            instrument_id: contracts,
            ..Default::default()
        })
        .await?
        .into_inner();
    Ok(response
        .last_prices
        .into_iter()
        .filter_map(|price| {
            let value = price
                .price
                .map(|q| q.units as f64 + f64::from(q.nano) / 1e9)?;
            let id = InstrumentId::new(price.instrument_uid).ok()?;
            Some((api.series().of_broker(id).to_string(), value))
        })
        .collect())
}

fn percent(now: f64, before: f64) -> Value {
    if before > 0.0 && now.is_finite() {
        round((now / before - 1.0) * 100.0)
    } else {
        Value::Null
    }
}

/// Дневные свечи ряда за полтора месяца.
async fn closes(broker: &VenueBroker, uid: &str) -> Result<Vec<(chrono::NaiveDate, f64, f64)>> {
    let now = Utc::now();
    let candles = history::load(
        broker,
        CandleHistoryRequest {
            instrument_id: InstrumentId::new(uid.to_owned())?,
            interval: CandleInterval::OneDay,
            from: now - Duration::days(45),
            to: now,
            limit: None,
        },
        Some(30),
    )
    .await?;
    Ok(candles
        .iter()
        .map(|c| (c.time.date_naive(), number(c.open.0), number(c.close.0)))
        .collect())
}

/// Открытие текущего дня ряда (дневная свеча сегодняшней даты).
pub(crate) async fn day_open(broker: &VenueBroker, uid: &str) -> Result<Option<f64>> {
    let today = Utc::now().date_naive();
    Ok(closes(broker, uid)
        .await?
        .into_iter()
        .rev()
        .find(|(date, ..)| *date == today)
        .map(|(_, open, _)| open))
}

/// Контекст для модели: последнее значение и изменения за сегодня, день, 5 и
/// 20 торговых дней.
pub(crate) async fn context(api: &VenueApi, broker: &VenueBroker) -> Result<Value> {
    let series = series(api).await;
    let mut uids: Vec<String> = series.iter().map(|s| s.uid.clone()).collect();
    uids.push(KEY_RATE.to_owned());
    let prices = last_prices(api, &uids).await.unwrap_or_default();
    let price = |uid: &str| prices.iter().find(|(id, _)| id == uid).map(|(_, p)| *p);
    let mut out = Vec::new();
    for item in &series {
        let Ok(daily) = closes(broker, &item.uid).await else {
            continue;
        };
        let Some(last) = price(&item.uid).or_else(|| daily.last().map(|(_, _, close)| *close))
        else {
            continue;
        };
        let today = Utc::now().date_naive();
        let finished: Vec<f64> = daily
            .iter()
            .filter(|(date, ..)| *date < today)
            .map(|(_, _, close)| *close)
            .collect();
        let back = |n: usize| {
            finished
                .len()
                .checked_sub(n)
                .and_then(|i| finished.get(i))
                .copied()
                .unwrap_or(f64::NAN)
        };
        let open = daily
            .iter()
            .find(|(date, ..)| *date == today)
            .map(|(_, open, _)| *open);
        out.push(json!({
            "key": item.key, "name": item.name, "last": round(last),
            "today_from_open_percent": open.map_or(Value::Null, |open| percent(last, open)),
            "change_1d_percent": percent(last, back(1)),
            "change_5d_percent": percent(last, back(5)),
            "change_20d_percent": percent(last, back(20)),
        }));
    }
    Ok(json!({"as_of": Utc::now(), "series": out, "key_rate_percent": price(KEY_RATE).map(round)}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_are_percent_of_the_earlier_value() {
        assert_eq!(percent(110.0, 100.0), json!(10.0));
        assert_eq!(percent(97.5, 100.0), json!(-2.5));
        assert_eq!(percent(1.0, f64::NAN), Value::Null);
        assert_eq!(percent(1.0, 0.0), Value::Null);
        let keys: Vec<&str> = FIXED.iter().map(|(key, ..)| *key).collect();
        assert_eq!(keys, vec!["IMOEX", "RGBI", "CNYRUB", "USDRUB", "GOLD"]);
    }
}
