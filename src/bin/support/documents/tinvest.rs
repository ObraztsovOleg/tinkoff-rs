//! Новости T-Invest: полные тексты Интерфакса, РИА Новостей и ТАСС и раскрытие
//! информации эмитентов (`eDisclosure`) с отметками инструментов и важности.
use std::time::Duration as StdDuration;

use chrono::{Duration, Utc};
use tinkoff_rs::{
    Result,
    domain::timestamp_to_datetime,
    proto::{NewsItem as ApiNewsItem, NewsRequest},
    storage::PostgresStorage,
};

use super::{Document, Kind, sources::Tinvest, store};
use crate::VenueApi;

/// Страниц за один проход. Лимиты запросов токена общие со стратегиями: история
/// догружается по частям с сохранённого курсора, а не одним залпом.
const MAX_PAGES: usize = 8;
const PAGE_SIZE: i32 = 100;
/// Пауза между страницами.
const PAGE_PAUSE: StdDuration = StdDuration::from_secs(1);

/// Новость T-Invest как документ.
pub(crate) fn to_document(item: &ApiNewsItem) -> Option<Document> {
    let published = timestamp_to_datetime(item.ts?).ok()?;
    let kind = if item.source.eq_ignore_ascii_case("edisclosure") {
        Kind::Disclosure
    } else {
        Kind::News
    };
    let mut document = Document::new(
        format!("tinvest:{}", item.id),
        &item.source,
        kind,
        &item.title,
        published,
    );
    item.content.trim().clone_into(&mut document.body);
    document.lead = item.summary.clone().unwrap_or_default();
    document.tables = item
        .tables
        .iter()
        .map(|table| table.table.clone())
        .filter(|table| !table.trim().is_empty())
        .collect();
    for info in item
        .instrument_id
        .iter()
        .filter_map(|i| i.instrument.as_ref())
    {
        if !info.instrument_uid.is_empty() && !document.instruments.contains(&info.instrument_uid) {
            document.instruments.push(info.instrument_uid.clone());
            document.tickers.push(info.ticker.clone());
        }
    }
    document.priority = item.priority;
    Some(document)
}

/// Курсор незаконченной загрузки истории: следующий проход продолжит с него.
pub(crate) type Backfill = Option<i64>;

/// Новые новости с последнего прохода: страницы от свежих к старым до уже
/// сохранённых; затем, если первая загрузка не закончена, — продолжение истории
/// с сохранённого курсора до границы `backfill_hours`. Возвращает число новых
/// документов и курсор, если история ещё не догружена.
pub(crate) async fn collect(
    api: &VenueApi,
    storage: &PostgresStorage,
    settings: &Tinvest,
    backfill: Backfill,
) -> Result<(usize, Backfill)> {
    let horizon = Utc::now() - Duration::hours(i64::from(settings.backfill_hours));
    let (mut cursor, mut added, mut head) = (None, 0, true);
    let mut pending = backfill;
    for page in 0..MAX_PAGES {
        if page > 0 {
            tokio::time::sleep(PAGE_PAUSE).await;
        }
        let response = api
            .instruments()
            .news(NewsRequest {
                cursor,
                limit: Some(PAGE_SIZE),
            })
            .await?
            .into_inner();
        let documents: Vec<Document> = response
            .items
            .iter()
            .filter_map(to_document)
            .filter(|d| d.published >= horizon)
            .collect();
        let fresh = store(storage, &documents).await?;
        added += fresh;
        let reached_horizon = documents.len() < response.items.len() || !response.has_next;
        if reached_horizon {
            return Ok((added, None));
        }
        if head && fresh == 0 {
            // Свежие уже сохранены: продолжаем незаконченную историю или выходим.
            let Some(saved) = pending.take() else {
                return Ok((added, None));
            };
            head = false;
            cursor = Some(saved);
            continue;
        }
        cursor = response.next_cursor;
    }
    // Проход исчерпан: следующий продолжит с этого места — после долгого простоя
    // это пропущенные свежие новости, иначе первая загрузка истории.
    Ok((added, cursor))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tinkoff_rs::proto::{NewsInstrument, NewsInstrumentInfo, Table};

    #[test]
    fn disclosures_keep_instrument_tags_tables_and_priority() {
        let item = ApiNewsItem {
            id: 109_974_538,
            source: "eDisclosure".to_owned(),
            title: "Сбербанк: решения совета директоров".to_owned(),
            content: "Совет директоров рекомендовал дивиденды.\n".to_owned(),
            summary: None,
            tables: vec![Table {
                table: "Показатель | Значение".to_owned(),
            }],
            instrument_id: vec![NewsInstrument {
                instrument: Some(NewsInstrumentInfo {
                    instrument_uid: "e6123145-9665-43e0-8413-cd61b8aa9b13".to_owned(),
                    ticker: "SBER".to_owned(),
                    class_code: "TQBR".to_owned(),
                }),
            }],
            priority: true,
            ts: Some(prost_types::Timestamp {
                seconds: 1_792_390_000,
                nanos: 0,
            }),
        };
        let document = to_document(&item).unwrap();
        assert_eq!(
            (
                document.id.as_str(),
                document.kind,
                document.tickers.as_slice(),
                document.priority
            ),
            (
                "tinvest:109974538",
                Kind::Disclosure,
                &["SBER".to_owned()][..],
                true
            )
        );
        assert_eq!(document.body, "Совет директоров рекомендовал дивиденды.");
        assert_eq!(document.tables, vec!["Показатель | Значение"]);
        let news = ApiNewsItem {
            source: "interfax".to_owned(),
            ts: None,
            ..item
        };
        assert!(
            to_document(&news).is_none(),
            "a news item without time is skipped"
        );
    }
}
