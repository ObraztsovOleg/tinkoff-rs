//! Внеочередные решения: скачки цен, всплески событий о бумагах блока, важные
//! события эмитентов и резкие движения рынка (индекс, нефть, валюты).
use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, NaiveDate, Utc};
use tinkoff_rs::{Result, domain::InstrumentId, proto::GetLastPricesRequest};

use super::{
    AiJob, Context, Instrument, Trigger, instruments,
    market::{self, Class},
    theses, truncate,
};
use crate::{VenueApi, documents::search};

/// Как часто сторож проверяет новые события в хранилище документов.
pub(crate) const NEWS_POLL: Duration = Duration::minutes(5);
/// Окно «быстрого» скачка цены.
pub(crate) const FAST_WINDOW: Duration = Duration::minutes(10);
/// Пауза после решения для важного события эмитента (короче обычной).
const MATERIAL_COOLDOWN_MINUTES: u32 = 10;

/// Движение ряда рынка с открытия дня.
#[derive(Clone, Debug)]
pub(crate) struct Move {
    pub key: &'static str,
    pub class: Class,
    pub percent: f64,
}

/// Важное событие эмитента из разметки документов.
#[derive(Clone, Debug)]
pub(crate) struct Material {
    pub id: String,
    pub time: DateTime<Utc>,
    pub title: String,
    pub importance: i16,
    pub tickers: Vec<String>,
}

/// Состояние сторожа одной компоненты.
#[derive(Default)]
pub(crate) struct JobWatch {
    pub(crate) instruments: Vec<Instrument>,
    pub(crate) last_decision: Option<DateTime<Utc>>,
    /// Цены на момент последнего решения.
    pub(crate) reference: HashMap<InstrumentId, f64>,
    /// Цены последних минут для быстрого скачка.
    pub(crate) recent: std::collections::VecDeque<(DateTime<Utc>, HashMap<InstrumentId, f64>)>,
    pub(crate) day: Option<NaiveDate>,
    pub(crate) decisions_today: u32,
    /// События о бумагах блока за последние сутки: время и заголовок; повторы
    /// разных агентств уже объединены.
    pub(crate) events: Vec<(DateTime<Utc>, String)>,
    /// Важные события о бумагах блока по разметке.
    pub(crate) material: Vec<Material>,
    /// События, по которым уже спрашивали модель.
    pub(crate) material_seen: HashSet<String>,
    /// Уровень движения ряда рынка, уже вызвавший решение сегодня: 1 — порог, 2 — двойной…
    pub(crate) bands: HashMap<&'static str, i64>,
    pub(crate) bands_day: Option<NaiveDate>,
    /// Открытые тезисы и уже сработавшие условия: (тезис, номер условия).
    pub(crate) theses: Vec<theses::Thesis>,
    pub(crate) fired: HashSet<(uuid::Uuid, usize)>,
}

/// Следит за ценами и новостями и решает, когда спросить модель внеочередно.
#[derive(Default)]
pub(crate) struct Watch {
    pub(crate) jobs: Vec<JobWatch>,
    pub(crate) last_news_poll: Option<DateTime<Utc>>,
    /// Ряды рынка и открытие дня каждого: (дата, значение).
    pub(crate) market: Vec<market::Series>,
    pub(crate) market_day: Option<NaiveDate>,
    pub(crate) opens: HashMap<&'static str, f64>,
}

pub(crate) fn percent_change(now: f64, before: f64) -> Option<f64> {
    (before > 0.0 && now.is_finite()).then(|| (now / before - 1.0) * 100.0)
}

impl Watch {
    /// Можно ли сейчас спросить модель: дневной предел и, для внеочередных, пауза.
    pub(crate) fn allowed(
        &mut self,
        index: usize,
        job: &AiJob,
        now: DateTime<Utc>,
        extra: bool,
    ) -> bool {
        let cooldown = if extra {
            job.config.min_minutes_between_decisions
        } else {
            0
        };
        self.allowed_after(index, job, now, cooldown)
    }

    /// Дневной предел и пауза `cooldown` минут после прошлого решения.
    fn allowed_after(
        &mut self,
        index: usize,
        job: &AiJob,
        now: DateTime<Utc>,
        cooldown: u32,
    ) -> bool {
        self.ensure(index);
        let watch = &mut self.jobs[index];
        if watch.day != Some(now.date_naive()) {
            watch.day = Some(now.date_naive());
            watch.decisions_today = 0;
        }
        if watch.decisions_today >= job.config.max_decisions_per_day {
            return false;
        }
        watch
            .last_decision
            .is_none_or(|last| now - last >= Duration::minutes(i64::from(cooldown)))
    }

    /// Отмечает решение (в том числе неудачное): от его цен считаются скачки.
    pub(crate) fn decided(&mut self, index: usize, now: DateTime<Utc>) {
        self.ensure(index);
        let watch = &mut self.jobs[index];
        watch.last_decision = Some(now);
        watch.decisions_today += 1;
        if let Some((_, prices)) = watch.recent.back() {
            watch.reference.clone_from(prices);
        }
    }

    pub(crate) fn ensure(&mut self, index: usize) {
        while self.jobs.len() <= index {
            self.jobs.push(JobWatch::default());
        }
    }

    /// Раз в минуту в торговую сессию: цены всех бумаг и, раз в 5 минут, новости.
    /// Возвращает компоненты, которым нужно внеочередное решение, и его причину.
    pub(crate) async fn check(
        &mut self,
        context: &Context<'_>,
        jobs: &[AiJob],
        now: DateTime<Utc>,
    ) -> Result<Vec<(usize, Trigger)>> {
        for (index, job) in jobs.iter().enumerate() {
            self.ensure(index);
            if self.jobs[index].instruments.is_empty() {
                self.jobs[index].instruments = instruments(context.api, &job.universe).await?;
            }
        }
        let prices = last_prices(context.api, jobs).await?;
        let moves = self.market_moves(context, now).await;
        if self
            .last_news_poll
            .is_none_or(|last| now - last >= NEWS_POLL)
        {
            self.last_news_poll = Some(now);
            // Документы собирает отдельный сборщик; сторож только читает хранилище.
            if let Some(storage) = context.storage {
                let open = if jobs.iter().any(|job| job.config.memory) {
                    theses::open_theses(storage, context.strategy_id)
                        .await?
                        .into_iter()
                        .map(|(thesis, _)| thesis)
                        .collect()
                } else {
                    Vec::new()
                };
                for watch in &mut self.jobs {
                    watch.theses = open
                        .iter()
                        .filter(|thesis| {
                            watch.instruments.iter().any(|i| i.ticker == thesis.ticker)
                        })
                        .cloned()
                        .collect();
                    let subjects: Vec<_> =
                        watch.instruments.iter().map(Instrument::subject).collect();
                    let hits = search::about(storage, &subjects, now - Duration::hours(24)).await?;
                    watch.material = hits
                        .iter()
                        .filter_map(|hit| {
                            Some(Material {
                                id: hit.cluster.clone(),
                                time: hit.time,
                                title: hit.title.clone(),
                                importance: hit.importance?,
                                tickers: hit.tickers.clone(),
                            })
                        })
                        .collect();
                    watch.events = hits.into_iter().map(|hit| (hit.time, hit.title)).collect();
                }
            }
        }
        Ok(self.evaluate(jobs, &prices, &moves, now))
    }

    /// Движения рядов рынка с открытия дня. Ошибка источника не мешает
    /// остальным поводам: движения просто не оцениваются.
    async fn market_moves(&mut self, context: &Context<'_>, now: DateTime<Utc>) -> Vec<Move> {
        let today = now.date_naive();
        if self.market_day != Some(today) {
            self.market = market::series(context.api).await;
            self.opens.clear();
            self.market_day = Some(today);
        }
        for series in &self.market {
            if !self.opens.contains_key(series.key) {
                if let Ok(Some(open)) = market::day_open(context.broker, &series.uid).await {
                    self.opens.insert(series.key, open);
                }
            }
        }
        let uids: Vec<String> = self.market.iter().map(|s| s.uid.clone()).collect();
        let Ok(prices) = market::last_prices(context.api, &uids).await else {
            return Vec::new();
        };
        self.market
            .iter()
            .filter_map(|series| {
                let open = *self.opens.get(series.key)?;
                let last = prices.iter().find(|(uid, _)| *uid == series.uid)?.1;
                Some(Move {
                    key: series.key,
                    class: series.class,
                    percent: percent_change(last, open)?,
                })
            })
            .collect()
    }

    /// Чистая оценка триггеров по ценам минуты и последним новостям.
    pub(crate) fn evaluate(
        &mut self,
        jobs: &[AiJob],
        prices: &HashMap<InstrumentId, f64>,
        moves: &[Move],
        now: DateTime<Utc>,
    ) -> Vec<(usize, Trigger)> {
        let mut triggers = Vec::new();
        for (index, job) in jobs.iter().enumerate() {
            self.ensure(index);
            let watch = &mut self.jobs[index];
            let current: HashMap<InstrumentId, f64> = job
                .universe
                .iter()
                .filter_map(|uid| prices.get(uid).map(|price| (uid.clone(), *price)))
                .collect();
            if watch.reference.is_empty() {
                watch.reference.clone_from(&current);
            }
            watch.recent.push_back((now, current.clone()));
            while watch
                .recent
                .front()
                .is_some_and(|(time, _)| now - *time > FAST_WINDOW + Duration::minutes(1))
            {
                watch.recent.pop_front();
            }
            let mut reasons = Vec::new();
            let oldest = watch
                .recent
                .front()
                .filter(|(time, _)| now - *time >= FAST_WINDOW - Duration::minutes(1));
            for instrument in &watch.instruments {
                let Some(price) = current.get(&instrument.uid) else {
                    continue;
                };
                let slow = job
                    .config
                    .price_jump_percent
                    .to_string()
                    .parse::<f64>()
                    .unwrap_or(0.0);
                let fast = job
                    .config
                    .fast_jump_percent
                    .to_string()
                    .parse::<f64>()
                    .unwrap_or(0.0);
                if let Some(change) = watch
                    .reference
                    .get(&instrument.uid)
                    .and_then(|before| percent_change(*price, *before))
                {
                    if slow > 0.0 && change.abs() >= slow {
                        reasons.push(format!(
                            "{} {change:+.2}% с прошлого решения",
                            instrument.ticker
                        ));
                        continue;
                    }
                }
                if let Some(change) = oldest
                    .and_then(|(_, before)| before.get(&instrument.uid))
                    .and_then(|before| percent_change(*price, *before))
                {
                    if fast > 0.0 && change.abs() >= fast {
                        reasons.push(format!("{} {change:+.2}% за 10 минут", instrument.ticker));
                    }
                }
            }
            let mut kind = "price_jump";
            if job.config.news_burst_items > 0 {
                let since = (now - Duration::minutes(i64::from(job.config.news_burst_minutes)))
                    .max(watch.last_decision.unwrap_or(DateTime::<Utc>::MIN_UTC));
                let burst: Vec<&(DateTime<Utc>, String)> = watch
                    .events
                    .iter()
                    .filter(|(time, _)| *time > since)
                    .collect();
                if burst.len() >= job.config.news_burst_items as usize {
                    if reasons.is_empty() {
                        kind = "news_burst";
                    }
                    reasons.push(format!(
                        "{} событий о бумагах блока за {} мин: {}",
                        burst.len(),
                        job.config.news_burst_minutes,
                        burst
                            .iter()
                            .take(3)
                            .map(|(_, title)| truncate(title, 80))
                            .collect::<Vec<_>>()
                            .join("; ")
                    ));
                }
            }
            // Резкое движение рынка: каждый следующий уровень (порог, двойной…) — один раз за день.
            let watch = &mut self.jobs[index];
            if watch.bands_day != Some(now.date_naive()) {
                watch.bands.clear();
                watch.bands_day = Some(now.date_naive());
            }
            let mut shocks = Vec::new();
            for change in moves {
                let threshold = match change.class {
                    Class::Index => &job.config.market_move_percent,
                    Class::Commodity => &job.config.commodity_move_percent,
                    Class::Currency => &job.config.currency_move_percent,
                    Class::Other => continue,
                };
                let threshold: f64 = threshold.to_string().parse().unwrap_or(0.0);
                if threshold <= 0.0 {
                    continue;
                }
                #[allow(clippy::cast_possible_truncation)]
                let band = (change.percent.abs() / threshold).floor() as i64;
                if band >= 1 && band > watch.bands.get(change.key).copied().unwrap_or(0) {
                    watch.bands.insert(change.key, band);
                    shocks.push(format!("{} {:+.2}% с открытия", change.key, change.percent));
                }
            }
            // Важное событие эмитента: каждое — один раз, с короткой паузой.
            let material: Vec<Material> = if job.config.material_importance == 0 {
                Vec::new()
            } else {
                let since = watch.last_decision.unwrap_or(DateTime::<Utc>::MIN_UTC);
                watch
                    .material
                    .iter()
                    .filter(|m| {
                        m.time > since && m.importance >= i16::from(job.config.material_importance)
                    })
                    .filter(|m| !watch.material_seen.contains(&m.id))
                    .cloned()
                    .collect()
            };
            if !material.is_empty()
                && self.allowed_after(index, job, now, MATERIAL_COOLDOWN_MINUTES)
            {
                let watch = &mut self.jobs[index];
                watch
                    .material_seen
                    .extend(material.iter().map(|m| m.id.clone()));
                let mut details: Vec<String> = material
                    .iter()
                    .take(3)
                    .map(|m| {
                        format!(
                            "{} (важность {}): {}",
                            m.tickers.join(", "),
                            m.importance,
                            truncate(&m.title, 100)
                        )
                    })
                    .collect();
                details.extend(shocks);
                details.extend(reasons);
                triggers.push((index, Trigger::new("material_event", details.join("; "))));
                continue;
            }
            // Условие тезиса: цена дошла до уровня или наступила дата — каждое один раз.
            let watch = &mut self.jobs[index];
            let mut conditions = Vec::new();
            for thesis in &watch.theses {
                let price = watch
                    .instruments
                    .iter()
                    .find(|i| i.ticker == thesis.ticker)
                    .and_then(|i| current.get(&i.uid).copied());
                for (number, condition) in thesis.conditions.iter().enumerate() {
                    if condition.met(price, now.date_naive())
                        && watch.fired.insert((thesis.id, number))
                    {
                        conditions.push(format!(
                            "{}: {} (тезис «{}»{})",
                            thesis.ticker,
                            condition.describe(),
                            truncate(&thesis.thesis, 80),
                            price.map(|p| format!(", цена {p}")).unwrap_or_default()
                        ));
                    }
                }
            }
            if !conditions.is_empty() {
                kind = "thesis_condition";
                conditions.extend(shocks);
                conditions.extend(reasons);
                reasons = conditions;
            } else if !shocks.is_empty() {
                kind = "macro_shock";
                shocks.extend(reasons);
                reasons = shocks;
            }
            if !reasons.is_empty() && self.allowed(index, job, now, true) {
                triggers.push((index, Trigger::new(kind, reasons.join("; "))));
            }
        }
        triggers
    }
}

/// Последние цены всех бумаг компонент одним запросом.
pub(crate) async fn last_prices(
    api: &VenueApi,
    jobs: &[AiJob],
) -> Result<HashMap<InstrumentId, f64>> {
    // Серия фьючерсов котируется по привязанному контракту.
    let mut ids: Vec<String> = jobs
        .iter()
        .flat_map(|j| {
            j.universe
                .iter()
                .map(|id| api.series().contract(id).to_string())
        })
        .collect();
    ids.sort();
    ids.dedup();
    let response = api
        .market_data()
        .get_last_prices(GetLastPricesRequest {
            instrument_id: ids,
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
            Some((
                api.series()
                    .of_broker(InstrumentId::new(price.instrument_uid).ok()?),
                value,
            ))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use tinkoff_rs::strategies::AiModelConfig;

    fn watched(config: AiModelConfig) -> (Watch, Vec<AiJob>, InstrumentId) {
        let uid = InstrumentId::new("sber-uid".to_owned()).unwrap();
        let job = AiJob {
            universe: vec![uid.clone()],
            config,
        };
        let mut watch = Watch::default();
        watch.ensure(0);
        watch.jobs[0].instruments = vec![Instrument {
            uid: uid.clone(),
            ticker: "SBER".to_owned(),
            name: "Сбербанк".to_owned(),
            lot: 1,
            kind: "share".to_owned(),
            details: serde_json::json!({}),
        }];
        (watch, vec![job], uid)
    }

    #[test]
    fn price_jumps_trigger_a_decision_with_cooldown_and_daily_cap() {
        let start = DateTime::from_timestamp(1_792_390_000, 0).unwrap();
        let config = AiModelConfig {
            news_burst_items: 0,
            ..AiModelConfig::default()
        };
        let (mut watch, jobs, uid) = watched(config);
        let at = |minutes: i64, price: f64| {
            (
                start + Duration::minutes(minutes),
                HashMap::from([(uid.clone(), price)]),
            )
        };
        // Calm minutes: the first price becomes the reference.
        for minute in 0..5 {
            let (now, prices) = at(minute, 100.0 + minute as f64 * 0.05);
            assert!(watch.evaluate(&jobs, &prices, &[], now).is_empty());
        }
        // +1.1% within ten minutes: a fast jump.
        let (now, prices) = at(10, 101.15);
        let triggers = watch.evaluate(&jobs, &prices, &[], now);
        assert_eq!(triggers.len(), 1);
        assert!(
            triggers[0].1.details.contains("за 10 минут"),
            "{}",
            triggers[0].1.details
        );
        watch.decided(0, now);
        // Still moving, but within the 20-minute cooldown: no new decision.
        let (now, prices) = at(15, 104.0);
        assert!(watch.evaluate(&jobs, &prices, &[], now).is_empty());
        // After the cooldown: +2.8% since the decision at 101.15.
        let (now, prices) = at(31, 104.0);
        let triggers = watch.evaluate(&jobs, &prices, &[], now);
        assert!(
            triggers[0].1.details.contains("с прошлого решения"),
            "{}",
            triggers[0].1.details
        );
        // The daily cap stops any further model call that day.
        watch.jobs[0].decisions_today = jobs[0].config.max_decisions_per_day;
        let (now, prices) = at(90, 120.0);
        assert!(watch.evaluate(&jobs, &prices, &[], now).is_empty());
    }

    #[test]
    fn a_news_burst_about_the_block_triggers_a_decision() {
        let start = DateTime::from_timestamp(1_792_390_000, 0).unwrap();
        let (mut watch, jobs, uid) = watched(AiModelConfig::default());
        let prices = HashMap::from([(uid, 100.0)]);
        // Events about the block, already merged across agencies by the document store.
        let item =
            |minutes: i64, title: &str| (start - Duration::minutes(minutes), title.to_owned());
        watch.jobs[0].events = vec![
            item(5, "Сбербанк отчитался"),
            item(10, "Акции Сбербанка растут"),
            item(45, "Старое событие"),
        ];
        assert!(
            watch.evaluate(&jobs, &prices, &[], start).is_empty(),
            "two events are not a burst"
        );
        watch.jobs[0].events.push(item(20, "SBER: дивиденды"));
        let triggers = watch.evaluate(&jobs, &prices, &[], start);
        assert_eq!(
            (
                triggers[0].1.kind,
                triggers[0].1.details.starts_with("3 событий")
            ),
            ("news_burst", true)
        );
        // News older than the last decision does not count again.
        watch.decided(0, start - Duration::minutes(25));
        watch.jobs[0].last_decision = Some(start - Duration::minutes(7));
        assert!(watch.evaluate(&jobs, &prices, &[], start).is_empty());
    }

    #[test]
    fn material_events_and_market_moves_ask_the_model_once_per_level() {
        let start = DateTime::from_timestamp(1_792_390_000, 0).unwrap();
        let (mut watch, jobs, uid) = watched(AiModelConfig {
            news_burst_items: 0,
            ..AiModelConfig::default()
        });
        let prices = HashMap::from([(uid, 100.0)]);
        let material = |id: &str, minutes: i64, importance: i16| Material {
            id: id.to_owned(),
            time: start - Duration::minutes(minutes),
            title: format!("Событие {id}"),
            importance,
            tickers: vec!["SBER".to_owned()],
        };
        watch.jobs[0].material = vec![material("a", 5, 3)];
        assert!(
            watch.evaluate(&jobs, &prices, &[], start).is_empty(),
            "importance 3 is below the default 4"
        );
        watch.jobs[0].material.push(material("b", 4, 4));
        let triggers = watch.evaluate(&jobs, &prices, &[], start);
        assert_eq!(triggers[0].1.kind, "material_event");
        assert!(
            triggers[0]
                .1
                .details
                .starts_with("SBER (важность 4): Событие b"),
            "{}",
            triggers[0].1.details
        );
        watch.decided(0, start);
        // The same event never asks twice; a new one after the short pause does.
        let later = start + Duration::minutes(11);
        assert!(watch.evaluate(&jobs, &prices, &[], later).is_empty());
        watch.jobs[0].material.push(Material {
            time: later - Duration::minutes(1),
            ..material("c", 0, 5)
        });
        assert_eq!(
            watch.evaluate(&jobs, &prices, &[], later)[0].1.kind,
            "material_event"
        );
        watch.decided(0, later);
        // IMOEX −2.3% from the open (threshold 2%): one decision; −3% stays in the same band.
        let shock = |percent| {
            vec![Move {
                key: "IMOEX",
                class: Class::Index,
                percent,
            }]
        };
        let after = later + Duration::minutes(30);
        let triggers = watch.evaluate(&jobs, &prices, &shock(-2.3), after);
        assert_eq!(
            (triggers[0].1.kind, triggers[0].1.details.as_str()),
            ("macro_shock", "IMOEX -2.30% с открытия")
        );
        watch.decided(0, after);
        assert!(
            watch
                .evaluate(&jobs, &prices, &shock(-3.0), after + Duration::minutes(30))
                .is_empty()
        );
        let deeper = watch.evaluate(&jobs, &prices, &shock(-4.1), after + Duration::minutes(40));
        assert_eq!(deeper[0].1.kind, "macro_shock", "a double move asks again");
        // Brent within 3% and the bond index never trigger.
        let calm = vec![
            Move {
                key: "BRENT",
                class: Class::Commodity,
                percent: 2.9,
            },
            Move {
                key: "RGBI",
                class: Class::Other,
                percent: -9.0,
            },
        ];
        assert!(
            watch
                .evaluate(&jobs, &prices, &calm, after + Duration::minutes(90))
                .is_empty()
        );
    }

    #[test]
    fn a_thesis_condition_asks_the_model_once() {
        let start = DateTime::from_timestamp(1_792_390_000, 0).unwrap();
        let (mut watch, jobs, uid) = watched(AiModelConfig {
            news_burst_items: 0,
            price_jump_percent: Decimal::ZERO,
            fast_jump_percent: Decimal::ZERO,
            ..AiModelConfig::default()
        });
        watch.jobs[0].theses = vec![theses::Thesis {
            id: uuid::Uuid::new_v4(),
            ticker: "SBER".to_owned(),
            direction: "long".to_owned(),
            thesis: "Банк выиграет от снижения ставки".to_owned(),
            conditions: vec![theses::Condition::PriceBelow(95.0)],
            entry_price: Some(100.0),
        }];
        let at = |price: f64| HashMap::from([(uid.clone(), price)]);
        assert!(watch.evaluate(&jobs, &at(97.0), &[], start).is_empty());
        let triggers = watch.evaluate(&jobs, &at(94.5), &[], start + Duration::minutes(1));
        assert_eq!(triggers[0].1.kind, "thesis_condition");
        assert!(
            triggers[0].1.details.starts_with("SBER: цена не выше 95"),
            "{}",
            triggers[0].1.details
        );
        watch.decided(0, start + Duration::minutes(1));
        assert!(
            watch
                .evaluate(&jobs, &at(94.0), &[], start + Duration::minutes(40))
                .is_empty(),
            "the same condition never asks twice"
        );
    }
}
