//! Инструменты модели и их исполнение внутри одного решения.
use std::collections::HashMap;

use chrono::{Duration, Utc};
use serde_json::{Value, json};
use tinkoff_rs::{
    Result,
    broker::Portfolio,
    domain::{
        Candle, CandleHistoryRequest, CandleInterval, InstrumentId, datetime_to_timestamp,
        timestamp_to_datetime,
    },
    engine::{ReferenceData, ReferenceRequirement},
    proto::{
        GetAssetReportsRequest, GetForecastRequest, Quotation, Recommendation,
        get_asset_reports_response::AssetReportPeriodType,
    },
};
use uuid::Uuid;

use super::{
    Context, Instrument,
    client::ToolCall,
    indicators::{SUPPORTED_INDICATORS, indicator, number, round},
    keywords, market,
    memory::past_decisions,
    theses, truncate,
};
use crate::{
    documents::{
        Kind,
        embed::Vectors,
        exposures, facts,
        rss::NewsItem,
        search::{self, Filter, Hit, Subject},
        topics,
    },
    history,
};

/// Документов в одном ответе поиска.
const SEARCH_LIMIT: u64 = 20;
/// Глубина поиска по умолчанию — неделя, наибольшая — срок хранения новостей.
const SEARCH_DEFAULT_HOURS: u64 = 168;
const SEARCH_MAX_HOURS: u64 = 8760;
/// Сколько документов запоминается для журнала решения.
const SOURCES_LIMIT: usize = 60;

pub(crate) fn tools(memory: bool) -> Vec<Value> {
    let tickers = json!({"type": "array", "items": {"type": "string"}, "description": "Тикеры инструментов блока"});
    let interval = json!({"type": "string", "enum": ["day", "hour"], "description": "Свечи: day (по умолчанию) или hour"});
    let kinds = json!({"type": "array", "items": {"type": "string", "enum": ["news", "article", "disclosure", "press_release", "report", "exchange", "regulator"]},
        "description": "Виды документов: news — ленты агентств, article — деловые издания, disclosure — раскрытие информации эмитентом (существенные факты), press_release, report — отчётность, exchange — сообщения биржи, regulator — Банк России"});
    let mut tools = vec![
        json!({"name": "search_documents", "description": "Поиск по документам: новости Интерфакса, РИА, ТАСС с полными текстами, раскрытие информации эмитентов, статьи деловых изданий, сообщения биржи и Банка России — история до года. Ищет слова в любом падеже (и по смыслу, если включены векторы); тикеры подставляют названия компаний. Одно событие от разных агентств — одна запись со списком источников. Возвращает фрагменты и id для read_document.",
               "parameters": {"type": "object", "properties": {
                   "query": {"type": "string", "description": "Что ищем: слова или фраза, например «санкции нефть экспорт» или «дивиденды рекомендация совета директоров»"},
                   "tickers": tickers, "kinds": kinds,
                   "topics": {"type": "array", "items": {"type": "string", "enum": topics::ids()}, "description": "Темы разметки: геополитика, санкции, нефть, рубль, ставка…"},
                   "min_importance": {"type": "integer", "description": "Не ниже важности разметки 1–5"},
                   "hours": {"type": "integer", "description": "Глубина, часов: по умолчанию 168 (неделя), до 8760"},
                   "limit": {"type": "integer", "description": "До 20 событий"}}}}),
        json!({"name": "read_document", "description": "Полный текст документа по id из поиска или сводки: страницы по ~8000 символов, таблицы — на первой странице, другие источники того же события.",
               "parameters": {"type": "object", "properties": {"id": {"type": "string"},
                   "page": {"type": "integer", "description": "Страница, с 1"}}, "required": ["id"]}}),
        json!({"name": "get_calendar", "description": "Календарь эмитентов блока: даты публикации отчётности и дивидендов (последний день покупки) на ближайшие дни.",
               "parameters": {"type": "object", "properties": {"tickers": tickers,
                   "days": {"type": "integer", "description": "Вперёд, дней: по умолчанию 30, до 120"}}}}),
        json!({"name": "get_market_context", "description": "Состояние рынка: индекс МосБиржи, индекс гособлигаций, нефть Brent, юань и доллар к рублю, золото, ключевая ставка — значение и изменения с открытия, за день, 5 и 20 дней.",
               "parameters": {"type": "object", "properties": {}}}),
        json!({"name": "get_exposure", "description": "Чувствительность бумаг блока к темам рынка (−2…+2: насколько бумаге хорошо движение темы вверх) с пояснениями: через неё события без упоминания бумаги влияют на неё. measured — измеренная реакция за 120 дней: effect_percent — движение бумаги (сверх индекса, для общерыночных тем — целиком) на единицу сигнала темы в дни событий, observations — число таких дней.",
               "parameters": {"type": "object", "properties": {"tickers": tickers}}}),
        json!({"name": "get_facts", "description": "Ключевые цифры последней отчётности и прогнозов эмитентов блока из документов: выручка, EBITDA, прибыль, долг, денежный поток, дивиденд, производство — с прошлым периодом и изменением.",
               "parameters": {"type": "object", "properties": {"tickers": tickers}}}),
        json!({"name": "get_analysts", "description": "Прогнозы инвестдомов: консенсус-цена на 12 месяцев, разброс, рекомендации и последние прогнозы по бумагам.",
               "parameters": {"type": "object", "properties": {"tickers": tickers}}}),
        json!({"name": "get_candles", "description": "Свечи инструмента (OHLCV): дневные до 250 дней или часовые до 250 часов.",
               "parameters": {"type": "object", "properties": {"ticker": {"type": "string"}, "interval": interval,
                   "count": {"type": "integer", "description": "До 250"}}, "required": ["ticker"]}}),
        json!({"name": "get_indicators", "description": format!("Индикаторы по дневным или часовым свечам. Доступны: {SUPPORTED_INDICATORS}."),
               "parameters": {"type": "object", "properties": {"tickers": tickers, "interval": interval,
                   "indicators": {"type": "array", "items": {"type": "string"}}}, "required": ["tickers", "indicators"]}}),
        json!({"name": "get_fundamentals", "description": "Мультипликаторы (P/B, P/E, EV/EBITDA, ROE, ROA, маржа, долг) и ближайшие дивиденды.",
               "parameters": {"type": "object", "properties": {"tickers": tickers}}}),
        json!({"name": "get_portfolio", "description": "Текущие позиции блока: доли капитала, стоимость, свободные деньги.",
               "parameters": {"type": "object", "properties": {}}}),
        json!({"name": "get_decisions", "description": "Твои прошлые решения: время, причина, доли бумаг и обоснование.",
               "parameters": {"type": "object", "properties": {"limit": {"type": "integer", "description": "До 30"}}}}),
    ];
    if memory {
        tools.push(json!({"name": "remember", "description": "Сохранить долгосрочную заметку: тезис по бумаге, план, условие выхода, урок. Заметки видны тебе во всех следующих решениях.",
               "parameters": {"type": "object", "properties": {"note": {"type": "string", "description": "До 1000 символов"}, "tickers": tickers,
                   "expires_in_days": {"type": "integer", "description": "Через сколько дней забыть (1–365); без значения — бессрочно"}}, "required": ["note"]}}));
        let conditions = json!({"type": "array", "items": {"type": "object", "properties": {
            "kind": {"type": "string", "enum": ["price_below", "price_above", "date"]},
            "value": {"description": "Уровень цены (число) или дата YYYY-MM-DD"},
            "note": {"type": "string", "description": "Что делать, когда условие наступит"}}, "required": ["kind", "value"]},
            "description": "Условия выхода или пересмотра: стенд спросит тебя внеочередно, когда условие наступит"});
        tools.push(json!({"name": "open_thesis", "description": "Открыть тезис по бумаге блока: направление, обоснование, горизонт, условия выхода. Цена открытия запоминается, при закрытии считается результат.",
               "parameters": {"type": "object", "properties": {"ticker": {"type": "string"},
                   "direction": {"type": "string", "enum": ["long", "short", "avoid"]},
                   "thesis": {"type": "string", "description": "Почему, на каких фактах, что должно произойти"},
                   "horizon_days": {"type": "integer"}, "exit_conditions": conditions,
                   "sources": {"type": "array", "items": {"type": "string"}, "description": "id документов-оснований"}},
                   "required": ["ticker", "direction", "thesis"]}}));
        tools.push(json!({"name": "update_thesis", "description": "Уточнить открытый тезис: текст, горизонт или условия.",
               "parameters": {"type": "object", "properties": {"id": {"type": "string"}, "thesis": {"type": "string"},
                   "horizon_days": {"type": "integer"}, "exit_conditions": conditions}, "required": ["id"]}}));
        tools.push(json!({"name": "close_thesis", "description": "Закрыть тезис: отработан, опровергнут или устарел. Результат считается по текущей цене.",
               "parameters": {"type": "object", "properties": {"id": {"type": "string"}, "reason": {"type": "string"}}, "required": ["id", "reason"]}}));
        tools.push(json!({"name": "forget", "description": "Закрыть заметку, когда она отработана или опровергнута.",
               "parameters": {"type": "object", "properties": {"id": {"type": "string"}, "reason": {"type": "string"}}, "required": ["id", "reason"]}}));
    }
    tools.push(json!({"name": "submit_decision", "description": "Итоговое решение. hold=true — ничего не менять. Иначе целевые доли капитала блока в процентах; не указанный инструмент — 0%.",
           "parameters": {"type": "object", "properties": {
               "hold": {"type": "boolean", "description": "true — сохранить текущие позиции без сделок"},
               "weights": {"type": "array", "items": {"type": "object", "properties": {
                   "ticker": {"type": "string"}, "weight_percent": {"type": "number"}}, "required": ["ticker", "weight_percent"]}},
               "rationale": {"type": "string", "description": "Кратко, по-русски, со ссылкой на факты"},
               "sources": {"type": "array", "items": {"type": "string"}, "description": "id документов, на которых основано решение"},
               "confidence_percent": {"type": "number"}}, "required": ["rationale"]}}));
    tools
}

fn quotation(value: Option<&Quotation>) -> Value {
    value.map_or(Value::Null, |q| {
        round(q.units as f64 + f64::from(q.nano) / 1e9)
    })
}

fn recommendation(value: i32) -> &'static str {
    match Recommendation::try_from(value) {
        Ok(Recommendation::Buy) => "buy",
        Ok(Recommendation::Hold) => "hold",
        Ok(Recommendation::Sell) => "sell",
        _ => "unknown",
    }
}

fn period(year: i32, number: i32, kind: i32) -> String {
    match AssetReportPeriodType::try_from(kind) {
        Ok(AssetReportPeriodType::PeriodTypeQuarter) => format!("{number}-й квартал {year}"),
        Ok(AssetReportPeriodType::PeriodTypeSemiannual) => format!("{number}-е полугодие {year}"),
        Ok(AssetReportPeriodType::PeriodTypeAnnual) => format!("{year} год"),
        _ => year.to_string(),
    }
}

/// Состояние инструментов внутри одного решения: кэш свечей, новостей и данных брокера.
pub(crate) struct Workspace<'a> {
    pub(crate) context: &'a Context<'a>,
    pub(crate) instruments: Vec<Instrument>,
    /// Ленты без базы документов (пробный анализ вне стенда).
    pub(crate) fallback_news: Vec<NewsItem>,
    pub(crate) news_hours: u32,
    /// Поиск по смыслу, если настроен сервис векторов.
    pub(crate) vectors: Option<Vectors>,
    pub(crate) candles: HashMap<(InstrumentId, bool), Vec<Candle>>,
    pub(crate) reference: Option<ReferenceData>,
    /// Документы, которые видела модель: сводка, поиск и чтение.
    pub(crate) sources: Vec<Value>,
    /// Последние цены бумаг блока: UID → цена.
    pub(crate) prices: HashMap<String, f64>,
}

impl Workspace<'_> {
    pub(crate) fn find(&self, ticker: &str) -> Option<&Instrument> {
        let ticker = ticker.trim().to_uppercase();
        self.instruments
            .iter()
            .find(|i| i.ticker.to_uppercase() == ticker || i.uid.as_str() == ticker)
    }

    /// Последняя цена бумаги блока по тикеру.
    pub(crate) fn price_of(&self, ticker: &str) -> Option<f64> {
        let instrument = self.find(ticker)?;
        self.prices.get(instrument.uid.as_str()).copied()
    }

    pub(crate) fn requested(&self, input: &Value) -> Vec<Instrument> {
        let tickers: Vec<&str> = input["tickers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        if tickers.is_empty() {
            return self.instruments.clone();
        }
        tickers
            .iter()
            .filter_map(|t| self.find(t).cloned())
            .collect()
    }

    /// Дневные (до 260) или часовые (до 260) свечи, загружаются один раз за решение.
    pub(crate) async fn candles(
        &mut self,
        instrument: &InstrumentId,
        hourly: bool,
    ) -> Result<&[Candle]> {
        let key = (instrument.clone(), hourly);
        if !self.candles.contains_key(&key) {
            let now = Utc::now();
            let (interval, depth) = if hourly {
                (CandleInterval::OneHour, Duration::days(45))
            } else {
                (CandleInterval::OneDay, Duration::days(400))
            };
            let candles = history::load(
                self.context.broker,
                CandleHistoryRequest {
                    instrument_id: instrument.clone(),
                    interval,
                    from: now - depth,
                    to: now,
                    limit: None,
                },
                Some(260),
            )
            .await?;
            self.candles.insert(key.clone(), candles);
        }
        Ok(&self.candles[&key])
    }

    pub(crate) async fn portfolio(&self) -> Result<Value> {
        let snapshot = self
            .context
            .broker
            .portfolio(self.context.account_id)
            .await?
            .data;
        let equity = snapshot
            .total
            .as_ref()
            .map_or(f64::NAN, |m| number(m.amount.0));
        let positions: Vec<Value> = self
            .instruments
            .iter()
            .map(|instrument| {
                let position = snapshot.positions.iter().find(|p| p.instrument_id == instrument.uid);
                let quantity = position.map_or(0.0, |p| number(p.quantity));
                let price = position.and_then(|p| p.current_price.as_ref()).map_or(f64::NAN, |m| number(m.amount.0));
                let value = if quantity == 0.0 { 0.0 } else { quantity * price };
                json!({"ticker": instrument.ticker, "quantity": round(quantity), "price": round(price),
                       "value_rub": round(value), "weight_percent": round(value / equity * 100.0)})
            })
            .collect();
        Ok(json!({"equity_rub": round(equity), "positions": positions}))
    }

    /// Бумаги запроса как предметы поиска документов; без тикеров — никакие.
    pub(crate) fn subjects(&self, input: &Value) -> Vec<Subject> {
        if !input["tickers"].is_array() {
            return Vec::new();
        }
        self.requested(input)
            .iter()
            .map(Instrument::subject)
            .collect()
    }

    /// Запоминает документ для журнала решения.
    pub(crate) fn note_source(&mut self, hit: &Hit, read: bool) {
        if let Some(known) = self.sources.iter_mut().find(|s| s["id"] == hit.id.as_str()) {
            if read {
                known["read"] = json!(true);
            }
            return;
        }
        if self.sources.len() < SOURCES_LIMIT {
            self.sources.push(json!({"id": hit.id, "title": hit.title, "source": hit.source, "sources": hit.sources,
                                     "kind": hit.kind, "published": hit.time, "link": hit.url, "read": read}));
        }
    }

    async fn search_tool(&mut self, input: &Value) -> Result<Value> {
        let Some(storage) = self.context.storage else {
            return Ok(self.news_fallback(input));
        };
        let hours = input["hours"]
            .as_u64()
            .map_or(SEARCH_DEFAULT_HOURS, |h| h.clamp(1, SEARCH_MAX_HOURS));
        let (from, to) = search::default_window(u32::try_from(hours).unwrap_or(u32::MAX));
        let filter = Filter {
            text: input["query"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_owned(),
            subjects: self.subjects(input),
            kinds: input["kinds"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter_map(Kind::parse)
                .collect(),
            sources: Vec::new(),
            topics: input["topics"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|t| topics::find(t).is_some())
                .map(str::to_owned)
                .collect(),
            min_importance: input["min_importance"]
                .as_i64()
                .and_then(|i| i16::try_from(i.clamp(1, 5)).ok()),
            from,
            to,
            limit: input["limit"]
                .as_u64()
                .map_or(10, |l| l.clamp(1, SEARCH_LIMIT)) as usize,
        };
        let hits = search::search(storage, &filter, self.vectors.as_ref()).await?;
        for hit in &hits {
            self.note_source(hit, false);
        }
        Ok(json!({"count": hits.len(), "events": hits}))
    }

    async fn read_tool(&mut self, input: &Value) -> Result<Value> {
        let Some(storage) = self.context.storage else {
            return Ok(json!({"error": "документы недоступны в этом запуске"}));
        };
        let Some(id) = input["id"].as_str() else {
            return Ok(json!({"error": "укажи id документа"}));
        };
        let page = input["page"].as_u64().map_or(1, |p| p.max(1)) as usize;
        let Some(document) = search::read(storage, id, page).await? else {
            return Ok(json!({"error": "документ не найден"}));
        };
        let hit = Hit {
            id: id.to_owned(),
            cluster: String::new(),
            time: serde_json::from_value(document["time"].clone()).unwrap_or_else(|_| Utc::now()),
            source: document["source"].as_str().unwrap_or_default().to_owned(),
            sources: Vec::new(),
            kind: document["kind"].as_str().unwrap_or_default().to_owned(),
            title: document["title"].as_str().unwrap_or_default().to_owned(),
            url: String::new(),
            tickers: Vec::new(),
            priority: false,
            importance: None,
            summary: None,
            event_type: None,
            topics: Value::Null,
            snippet: String::new(),
            about: false,
            impacts: Vec::new(),
            score: 0.0,
        };
        self.note_source(&hit, true);
        Ok(document)
    }

    async fn calendar_tool(&mut self, input: &Value) -> Result<Value> {
        let days = input["days"].as_i64().map_or(30, |d| d.clamp(1, 120));
        let now = Utc::now();
        let (from, to) = (now - Duration::days(1), now + Duration::days(days));
        let mut events = Vec::new();
        for instrument in self
            .requested(input)
            .into_iter()
            .filter(|i| i.kind == "share")
        {
            let response = self
                .context
                .api
                .instruments()
                .get_asset_reports(GetAssetReportsRequest {
                    instrument_id: instrument.uid.to_string(),
                    from: Some(datetime_to_timestamp(from)),
                    to: Some(datetime_to_timestamp(to)),
                })
                .await;
            match response {
                Ok(response) => {
                    for event in response.into_inner().events {
                        let Some(date) = event
                            .report_date
                            .and_then(|t| timestamp_to_datetime(t).ok())
                        else {
                            continue;
                        };
                        events.push(json!({"ticker": instrument.ticker, "date": date.date_naive(), "event": "отчётность",
                                           "period": period(event.period_year, event.period_num, event.period_type)}));
                    }
                }
                Err(error) => {
                    events.push(json!({"ticker": instrument.ticker, "error": error.message()}));
                }
            }
        }
        let instruments = self.instruments.clone();
        let reference = self.reference_data().await?;
        for dividend in &reference.dividends {
            let Some(instrument) = instruments.iter().find(|i| i.uid == dividend.instrument_id)
            else {
                continue;
            };
            let date = dividend.last_buy_date;
            if date >= from.date_naive() && date <= to.date_naive() {
                events.push(json!({"ticker": instrument.ticker, "date": date, "event": "дивиденды: последний день покупки",
                                   "dividend_net": dividend.dividend_net.map(|v| v.to_string())}));
            }
        }
        events.sort_by(|a, b| a["date"].as_str().cmp(&b["date"].as_str()));
        Ok(json!({"from": from.date_naive(), "to": to.date_naive(), "events": events}))
    }

    async fn analysts_tool(&mut self, input: &Value) -> Result<Value> {
        let mut out = serde_json::Map::new();
        for instrument in self.requested(input) {
            if instrument.kind != "share" {
                out.insert(
                    instrument.ticker.clone(),
                    json!({"note": "прогнозы аналитиков есть только по акциям"}),
                );
                continue;
            }
            let response = self
                .context
                .api
                .instruments()
                .get_forecast_by(GetForecastRequest {
                    instrument_id: instrument.uid.to_string(),
                })
                .await;
            let value = match response {
                Ok(response) => {
                    let forecast = response.into_inner();
                    let mut targets = forecast.targets;
                    targets.sort_by_key(|t| {
                        std::cmp::Reverse(t.recommendation_date.as_ref().map(|d| d.seconds))
                    });
                    json!({
                        "consensus": forecast.consensus.map(|c| json!({
                            "recommendation": recommendation(c.recommendation), "current_price": quotation(c.current_price.as_ref()),
                            "target": quotation(c.consensus.as_ref()), "min": quotation(c.min_target.as_ref()),
                            "max": quotation(c.max_target.as_ref()), "upside_percent": quotation(c.price_change_rel.as_ref()),
                        })),
                        "latest": targets.iter().take(6).map(|t| json!({
                            "company": t.company, "recommendation": recommendation(t.recommendation),
                            "target": quotation(t.target_price.as_ref()),
                            "date": t.recommendation_date.and_then(|d| timestamp_to_datetime(d).ok()).map(|d| d.date_naive()),
                        })).collect::<Vec<_>>(),
                    })
                }
                Err(error) => json!({"error": error.message()}),
            };
            out.insert(instrument.ticker.clone(), value);
        }
        Ok(Value::Object(out))
    }

    async fn facts_tool(&mut self, input: &Value) -> Result<Value> {
        let Some(storage) = self.context.storage else {
            return Ok(json!({"error": "документы недоступны в этом запуске"}));
        };
        let tickers: Vec<String> = self
            .requested(input)
            .into_iter()
            .map(|i| i.ticker)
            .collect();
        Ok(json!({"documents": facts::latest(storage, &tickers).await?}))
    }

    async fn exposure_tool(&mut self, input: &Value) -> Result<Value> {
        let Some(storage) = self.context.storage else {
            return Ok(json!({"error": "профили бумаг недоступны в этом запуске"}));
        };
        let tickers: Vec<String> = self
            .requested(input)
            .into_iter()
            .map(|i| i.ticker)
            .collect();
        let profiles = exposures::describe(storage, &tickers).await?;
        Ok(
            json!({"scale": "чувствительность к движению темы вверх: +2 — сильно выигрывает, -2 — сильно проигрывает",
                  "topics": topics::TOPICS.iter().map(|t| json!({"topic": t.id, "name": t.name, "up": t.up})).collect::<Vec<_>>(),
                  "profiles": profiles}),
        )
    }

    /// Новости лент из памяти процесса, когда базы документов нет.
    pub(crate) fn news_fallback(&mut self, input: &Value) -> Value {
        let hours = input["hours"]
            .as_u64()
            .map_or(self.news_hours, |h| (h as u32).clamp(1, 720));
        let limit = input["limit"].as_u64().map_or(20, |l| l.clamp(1, 50)) as usize;
        let since = Utc::now() - Duration::hours(i64::from(hours));
        let mut words: Vec<String> = input["query"]
            .as_str()
            .unwrap_or_default()
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|w| w.chars().count() >= 3)
            .map(str::to_lowercase)
            .collect();
        for instrument in self
            .requested(input)
            .iter()
            .filter(|_| input["tickers"].is_array())
        {
            words.extend(keywords(instrument));
        }
        let found: Vec<&NewsItem> = self
            .fallback_news
            .iter()
            .filter(|item| item.published >= since)
            .filter(|item| {
                let text = format!("{} {}", item.title, item.summary).to_lowercase();
                words.is_empty()
                    || words
                        .iter()
                        .any(|w| !w.is_empty() && text.contains(w.as_str()))
            })
            .take(limit)
            .collect();
        for item in &found {
            if !self.sources.iter().any(|s| s["link"] == item.link.as_str())
                && self.sources.len() < 40
            {
                self.sources.push(json!({"title": item.title, "link": item.link, "source": item.source, "published": item.published}));
            }
        }
        json!({"count": found.len(), "news": found})
    }

    pub(crate) async fn call(&mut self, call: &ToolCall) -> Value {
        let result = match call.name.as_str() {
            "search_documents" | "get_news" => self.search_tool(&call.input).await,
            "read_document" => self.read_tool(&call.input).await,
            "get_calendar" => self.calendar_tool(&call.input).await,
            "get_analysts" => self.analysts_tool(&call.input).await,
            "get_market_context" => market::context(self.context.api, self.context.broker).await,
            "get_exposure" => self.exposure_tool(&call.input).await,
            "get_facts" => self.facts_tool(&call.input).await,
            "get_candles" => self.candles_tool(&call.input).await,
            "get_indicators" => self.indicators_tool(&call.input).await,
            "get_fundamentals" => self.fundamentals_tool(&call.input).await,
            "get_portfolio" => self.portfolio().await,
            "get_decisions" => self.decisions_tool(&call.input).await,
            "remember" => self.remember(&call.input).await,
            "open_thesis" | "update_thesis" | "close_thesis" => {
                self.thesis_tool(&call.name, &call.input).await
            }
            "forget" => self.forget(&call.input).await,
            other => Ok(json!({"error": format!("unknown tool {other}")})),
        };
        result.unwrap_or_else(|error| json!({"error": error.to_string()}))
    }

    pub(crate) async fn candles_tool(&mut self, input: &Value) -> Result<Value> {
        let Some(instrument) = input["ticker"].as_str().and_then(|t| self.find(t)).cloned() else {
            return Ok(json!({"error": "unknown ticker; use tickers of the block"}));
        };
        let hourly = input["interval"] == "hour";
        let count = input["count"]
            .as_u64()
            .or_else(|| input["days"].as_u64())
            .map_or(30, |d| d.clamp(1, 250)) as usize;
        let candles = self.candles(&instrument.uid, hourly).await?;
        let tail = &candles[candles.len().saturating_sub(count)..];
        Ok(
            json!({"ticker": instrument.ticker, "interval": if hourly { "hour" } else { "day" }, "candles": tail.iter().map(|c| json!({
            "time": if hourly { c.time.to_rfc3339() } else { c.time.date_naive().to_string() }, "open": round(number(c.open.0)), "high": round(number(c.high.0)),
            "low": round(number(c.low.0)), "close": round(number(c.close.0)), "volume": c.volume})).collect::<Vec<_>>()}),
        )
    }

    pub(crate) async fn indicators_tool(&mut self, input: &Value) -> Result<Value> {
        let names: Vec<String> = input["indicators"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .take(30)
            .map(str::to_lowercase)
            .collect();
        let hourly = input["interval"] == "hour";
        let mut out = serde_json::Map::new();
        for instrument in self.requested(input) {
            let candles = self.candles(&instrument.uid, hourly).await?;
            let values: serde_json::Map<String, Value> = names
                .iter()
                .map(|name| (name.clone(), indicator(name, candles)))
                .collect();
            out.insert(instrument.ticker.clone(), Value::Object(values));
        }
        Ok(Value::Object(out))
    }

    /// Мультипликаторы и дивиденды бумаг блока, один раз за решение.
    async fn reference_data(&mut self) -> Result<&ReferenceData> {
        if self.reference.is_none() {
            // Мультипликаторы и дивиденды есть только у акций.
            let universe: Vec<InstrumentId> = self
                .instruments
                .iter()
                .filter(|i| i.kind == "share")
                .map(|i| i.uid.clone())
                .collect();
            self.reference = Some(
                crate::load_reference(
                    self.context.api,
                    &universe,
                    &[
                        ReferenceRequirement::Fundamentals,
                        ReferenceRequirement::Dividends,
                    ],
                )
                .await?,
            );
        }
        Ok(self.reference.as_ref().expect("loaded above"))
    }

    pub(crate) async fn fundamentals_tool(&mut self, input: &Value) -> Result<Value> {
        let requested = self.requested(input);
        let reference = self.reference_data().await?;
        let mut out = serde_json::Map::new();
        for instrument in requested {
            if instrument.kind != "share" {
                out.insert(
                    instrument.ticker.clone(),
                    json!({"note": "мультипликаторы и дивиденды есть только у акций", "kind": instrument.kind, "details": instrument.details}),
                );
                continue;
            }
            let fundamentals = reference
                .fundamentals
                .iter()
                .find(|f| f.instrument_id == instrument.uid);
            let dividends: Vec<Value> = reference.dividends.iter().filter(|d| d.instrument_id == instrument.uid)
                .map(|d| json!({"last_buy_date": d.last_buy_date, "dividend_net": d.dividend_net.map(|v| v.to_string())})).collect();
            out.insert(
                instrument.ticker.clone(),
                json!({"fundamentals": fundamentals, "dividends": dividends}),
            );
        }
        Ok(Value::Object(out))
    }

    pub(crate) async fn decisions_tool(&self, input: &Value) -> Result<Value> {
        let Some(storage) = self.context.storage else {
            return Ok(json!({"decisions": [], "note": "журнал недоступен в этом запуске"}));
        };
        let limit = input["limit"].as_i64().map_or(10, |l| l.clamp(1, 30));
        Ok(json!({"decisions": past_decisions(storage, self.context.strategy_id, limit).await?}))
    }

    async fn thesis_tool(&mut self, name: &str, input: &Value) -> Result<Value> {
        let Some(storage) = self.context.storage else {
            return Ok(json!({"error": "память недоступна в этом запуске"}));
        };
        if !self.context.persist_memory {
            return Ok(json!({"saved": false, "note": "пробный анализ: тезисы не меняются"}));
        }
        let strategy = self.context.strategy_id;
        match name {
            "open_thesis" => {
                let Some(instrument) = input["ticker"].as_str().and_then(|t| self.find(t)).cloned()
                else {
                    return Ok(json!({"error": "тикер не из блока"}));
                };
                let price = self.price_of(&instrument.ticker);
                theses::open(storage, strategy, &instrument.ticker, input, price).await
            }
            "update_thesis" => theses::update(storage, strategy, input).await,
            _ => theses::close(storage, strategy, input, |ticker| self.price_of(ticker)).await,
        }
    }

    pub(crate) async fn remember(&self, input: &Value) -> Result<Value> {
        let Some(storage) = self.context.storage else {
            return Ok(json!({"error": "память недоступна в этом запуске"}));
        };
        if !self.context.persist_memory {
            return Ok(json!({"saved": false, "note": "пробный анализ: заметка не сохраняется"}));
        }
        let note = input["note"].as_str().unwrap_or_default().trim();
        if note.is_empty() {
            return Ok(json!({"error": "пустая заметка"}));
        }
        let active: i64 = sqlx::query_scalar("SELECT count(*) FROM ai_memory WHERE strategy_id=$1 AND kind='note' AND closed_at IS NULL AND (expires_at IS NULL OR expires_at>now())")
            .bind(self.context.strategy_id).fetch_one(storage.pool()).await?;
        if active >= 50 {
            return Ok(json!({"error": "в памяти уже 50 заметок: закрой устаревшие через forget"}));
        }
        let tickers: Vec<String> = self
            .requested(input)
            .iter()
            .filter(|_| input["tickers"].is_array())
            .map(|i| i.ticker.clone())
            .collect();
        let expires = input["expires_in_days"]
            .as_i64()
            .map(|d| Utc::now() + Duration::days(d.clamp(1, 365)));
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO ai_memory(id,strategy_id,note,tickers,expires_at) VALUES($1,$2,$3,$4,$5)",
        )
        .bind(id)
        .bind(self.context.strategy_id)
        .bind(truncate(note, 1000))
        .bind(json!(tickers))
        .bind(expires)
        .execute(storage.pool())
        .await?;
        Ok(json!({"saved": id}))
    }

    pub(crate) async fn forget(&self, input: &Value) -> Result<Value> {
        let Some(storage) = self.context.storage else {
            return Ok(json!({"error": "память недоступна в этом запуске"}));
        };
        if !self.context.persist_memory {
            return Ok(json!({"closed": false, "note": "пробный анализ: память не меняется"}));
        }
        let Some(id) = input["id"].as_str().and_then(|id| Uuid::parse_str(id).ok()) else {
            return Ok(json!({"error": "укажи id заметки"}));
        };
        let reason = truncate(input["reason"].as_str().unwrap_or("без причины"), 500);
        let closed = sqlx::query("UPDATE ai_memory SET closed_at=now(),closed_reason=$3 WHERE id=$1 AND strategy_id=$2 AND closed_at IS NULL")
            .bind(id).bind(self.context.strategy_id).bind(reason)
            .execute(storage.pool()).await?.rows_affected();
        Ok(json!({"closed": closed == 1}))
    }
}
