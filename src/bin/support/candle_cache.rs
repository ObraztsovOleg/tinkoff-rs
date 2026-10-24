//! Общий кэш исторических свечей в `PostgreSQL`.
//!
//! Процессы стенда прогреваются на одной и той же истории: без кэша каждый
//! заново скачивает её у брокера и упирается в лимиты API. Законченные свечи
//! хранятся в таблице `candles`, а `candle_coverage` помнит, какие отрезки
//! времени скачаны целиком. Запрос берёт из базы покрытое, у брокера —
//! только недостающее, и пополняет кэш. Свечи короче минуты и склеенные
//! серии фьючерсов (их поправка меняется при переходе) не кэшируются: кэш
//! стоит под свечами отдельных контрактов.
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use tinkoff_rs::{
    Error, Result,
    domain::{Candle, CandleHistoryRequest, CandleInterval, InstrumentId, Price},
    storage::PostgresStorage,
};
use tracing::warn;

/// Код интервала в таблице `candles`; `None` — интервал не кэшируется.
pub(super) const fn code(interval: CandleInterval) -> Option<&'static str> {
    use CandleInterval as I;
    Some(match interval {
        I::OneMinute => "1m",
        I::TwoMinutes => "2m",
        I::ThreeMinutes => "3m",
        I::FiveMinutes => "5m",
        I::TenMinutes => "10m",
        I::FifteenMinutes => "15m",
        I::ThirtyMinutes => "30m",
        I::OneHour => "1h",
        I::TwoHours => "2h",
        I::FourHours => "4h",
        I::OneDay => "1d",
        I::OneWeek => "1w",
        I::OneMonth => "1mo",
        I::FiveSeconds | I::TenSeconds | I::ThirtySeconds => return None,
    })
}

/// Длительность свечи: позже этого от начала свеча уже закончена.
const fn step(interval: CandleInterval) -> Duration {
    use CandleInterval as I;
    match interval {
        I::OneMinute => Duration::minutes(1),
        I::TwoMinutes => Duration::minutes(2),
        I::ThreeMinutes => Duration::minutes(3),
        I::FiveMinutes => Duration::minutes(5),
        I::TenMinutes => Duration::minutes(10),
        I::FifteenMinutes => Duration::minutes(15),
        I::ThirtyMinutes => Duration::minutes(30),
        I::OneHour => Duration::hours(1),
        I::TwoHours => Duration::hours(2),
        I::FourHours => Duration::hours(4),
        I::OneDay => Duration::days(1),
        I::OneWeek => Duration::weeks(1),
        I::OneMonth => Duration::days(31),
        I::FiveSeconds => Duration::seconds(5),
        I::TenSeconds => Duration::seconds(10),
        I::ThirtySeconds => Duration::seconds(30),
    }
}

/// Части `[from, to]`, не покрытые отрезками `covered` (по возрастанию начала).
pub(super) fn missing(from: DateTime<Utc>, to: DateTime<Utc>, covered: &[Range]) -> Vec<Range> {
    let mut out = Vec::new();
    let mut cursor = from;
    for &(start, end) in covered {
        if end <= cursor {
            continue;
        }
        if start >= to {
            break;
        }
        if start > cursor {
            out.push((cursor, start.min(to)));
        }
        cursor = cursor.max(end);
        if cursor >= to {
            break;
        }
    }
    if cursor < to {
        out.push((cursor, to));
    }
    out
}

/// Сбой брокера пробрасывается; сбой базы отправляет запрос брокеру целиком.
enum Failure {
    Broker(Error),
    Database(Error),
}

fn db(error: sqlx::Error) -> Failure {
    Failure::Database(Error::from(error))
}

/// Свеча из базы: начало, OHLC, объём.
type Row = (DateTime<Utc>, Decimal, Decimal, Decimal, Decimal, i64);
/// Отрезок времени.
type Range = (DateTime<Utc>, DateTime<Utc>);

/// Кэш поверх базы стенда.
pub(super) struct CandleCache {
    storage: Arc<PostgresStorage>,
}

impl CandleCache {
    pub(super) const fn new(storage: Arc<PostgresStorage>) -> Self {
        Self { storage }
    }

    /// Свечи окна: покрытое — из базы, недостающее — через `fetch` с
    /// пополнением кэша. Сбой базы не мешает: запрос уходит брокеру целиком.
    pub(super) async fn candles<F, Fut>(
        &self,
        request: CandleHistoryRequest,
        fetch: F,
    ) -> Result<Vec<Candle>>
    where
        F: Fn(CandleHistoryRequest) -> Fut,
        Fut: std::future::Future<Output = Result<Vec<Candle>>>,
    {
        let Some(code) = code(request.interval) else {
            return fetch(request).await;
        };
        match self.read_through(code, &request, &fetch).await {
            Ok(candles) => Ok(candles),
            Err(Failure::Broker(error)) => Err(error),
            Err(Failure::Database(error)) => {
                warn!(%error, instrument_id = %request.instrument_id, "candle cache unavailable; history is read from the broker");
                fetch(request).await
            }
        }
    }

    async fn read_through<F, Fut>(
        &self,
        code: &str,
        request: &CandleHistoryRequest,
        fetch: &F,
    ) -> std::result::Result<Vec<Candle>, Failure>
    where
        F: Fn(CandleHistoryRequest) -> Fut,
        Fut: std::future::Future<Output = Result<Vec<Candle>>>,
    {
        let id = request.instrument_id.as_str();
        let covered: Vec<Range> = sqlx::query_as(
            "SELECT from_time, to_time FROM candle_coverage WHERE instrument_id = $1 AND interval = $2 \
             AND from_time < $4 AND to_time > $3 ORDER BY from_time",
        )
        .bind(id)
        .bind(code)
        .bind(request.from)
        .bind(request.to)
        .fetch_all(self.storage.pool())
        .await
        .map_err(db)?;
        // Законченными считаются свечи, начавшиеся не позже этого момента.
        let settled = Utc::now() - step(request.interval);
        let mut fresh: Vec<Candle> = Vec::new();
        for (from, to) in missing(request.from, request.to, &covered) {
            let piece = fetch(CandleHistoryRequest {
                instrument_id: request.instrument_id.clone(),
                interval: request.interval,
                from,
                to,
                limit: request.limit,
            })
            .await
            .map_err(Failure::Broker)?;
            let mut transaction = self.storage.pool().begin().await.map_err(db)?;
            for candle in piece.iter().filter(|c| c.complete) {
                sqlx::query(
                    "INSERT INTO candles (instrument_id, interval, open_time, open, high, low, close, volume, complete) \
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,true) ON CONFLICT (instrument_id, interval, open_time) DO UPDATE SET \
                     open=EXCLUDED.open, high=EXCLUDED.high, low=EXCLUDED.low, close=EXCLUDED.close, volume=EXCLUDED.volume, complete=true",
                )
                .bind(id)
                .bind(code)
                .bind(candle.time)
                .bind(candle.open.0)
                .bind(candle.high.0)
                .bind(candle.low.0)
                .bind(candle.close.0)
                .bind(candle.volume)
                .execute(&mut *transaction)
                .await
                .map_err(db)?;
            }
            let end = to.min(settled);
            if end > from {
                sqlx::query("INSERT INTO candle_coverage (instrument_id, interval, from_time, to_time) VALUES ($1,$2,$3,$4)")
                    .bind(id)
                    .bind(code)
                    .bind(from)
                    .bind(end)
                    .execute(&mut *transaction)
                    .await
                    .map_err(db)?;
            }
            transaction.commit().await.map_err(db)?;
            fresh.extend(piece.into_iter().filter(|c| !c.complete));
        }
        self.compact(id, code).await.map_err(Failure::Database)?;
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT open_time, open, high, low, close, volume FROM candles WHERE instrument_id = $1 AND interval = $2 \
             AND complete AND open_time >= $3 AND open_time <= $4 ORDER BY open_time",
        )
        .bind(id)
        .bind(code)
        .bind(request.from)
        .bind(request.to)
        .fetch_all(self.storage.pool())
        .await
        .map_err(db)?;
        let mut candles: Vec<Candle> = rows
            .into_iter()
            .map(|(time, open, high, low, close, volume)| Candle {
                instrument_id: InstrumentId::new(id.to_owned()).expect("cached ids are not empty"),
                interval: request.interval,
                open: Price(open),
                high: Price(high),
                low: Price(low),
                close: Price(close),
                volume,
                time,
                complete: true,
            })
            .collect();
        // Незаконченная последняя свеча — как её отдал брокер.
        for candle in fresh {
            if !candles.iter().any(|c| c.time == candle.time) {
                candles.push(candle);
            }
        }
        candles.sort_by_key(|c| c.time);
        Ok(candles)
    }

    /// Сливает перекрывающиеся отрезки покрытия, когда их набирается много.
    async fn compact(&self, id: &str, code: &str) -> Result<()> {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM candle_coverage WHERE instrument_id = $1 AND interval = $2",
        )
        .bind(id)
        .bind(code)
        .fetch_one(self.storage.pool())
        .await?;
        if count < 32 {
            return Ok(());
        }
        let mut transaction = self.storage.pool().begin().await?;
        let ranges: Vec<Range> = sqlx::query_as(
            "DELETE FROM candle_coverage WHERE instrument_id = $1 AND interval = $2 RETURNING from_time, to_time",
        )
        .bind(id)
        .bind(code)
        .fetch_all(&mut *transaction)
        .await?;
        for (from, to) in merge(ranges) {
            sqlx::query("INSERT INTO candle_coverage (instrument_id, interval, from_time, to_time) VALUES ($1,$2,$3,$4)")
                .bind(id)
                .bind(code)
                .bind(from)
                .bind(to)
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        Ok(())
    }
}

/// Объединение перекрывающихся и смежных отрезков.
pub(super) fn merge(
    mut ranges: Vec<(DateTime<Utc>, DateTime<Utc>)>,
) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    ranges.sort_by_key(|range| range.0);
    let mut out: Vec<(DateTime<Utc>, DateTime<Utc>)> = Vec::new();
    for (from, to) in ranges {
        match out.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => out.push((from, to)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(day: u32) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(&format!("2026-10-{day:02}T00:00:00Z"))
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn only_what_is_not_cached_is_asked_from_the_broker() {
        assert_eq!(missing(at(1), at(10), &[]), vec![(at(1), at(10))]);
        assert_eq!(missing(at(1), at(10), &[(at(1), at(10))]), vec![]);
        assert_eq!(
            missing(
                at(1),
                at(20),
                &[(at(3), at(5)), (at(4), at(8)), (at(12), at(15))]
            ),
            vec![(at(1), at(3)), (at(8), at(12)), (at(15), at(20))]
        );
        assert_eq!(
            missing(at(5), at(7), &[(at(1), at(6))]),
            vec![(at(6), at(7))]
        );
        assert_eq!(
            merge(vec![
                (at(5), at(7)),
                (at(1), at(3)),
                (at(3), at(4)),
                (at(6), at(9))
            ]),
            vec![(at(1), at(4)), (at(5), at(9))]
        );
        assert_eq!(
            code(CandleInterval::FiveSeconds),
            None,
            "sub-minute candles are not cached"
        );
        assert_eq!(code(CandleInterval::OneDay), Some("1d"));
    }

    /// Повторный прогрев читает историю из базы, расширенный — докачивает хвост.
    #[tokio::test]
    #[ignore = "requires DOCUMENTS_TEST_DATABASE_URL: an isolated PostgreSQL with migrations"]
    async fn history_is_downloaded_once_and_extended_by_the_missing_tail() {
        use std::sync::Mutex;
        let storage = Arc::new(
            PostgresStorage::connect(&std::env::var("DOCUMENTS_TEST_DATABASE_URL").unwrap(), 2)
                .await
                .unwrap(),
        );
        storage.migrate().await.unwrap();
        let id = InstrumentId::new(format!("cache-test-{}", uuid::Uuid::new_v4())).unwrap();
        let cache = CandleCache::new(storage.clone());
        let asked: Arc<Mutex<Vec<Range>>> = Arc::default();
        let fetch = |request: CandleHistoryRequest| {
            let asked = asked.clone();
            async move {
                asked.lock().unwrap().push((request.from, request.to));
                let mut day = request.from;
                let mut out = Vec::new();
                while day < request.to {
                    out.push(Candle {
                        instrument_id: request.instrument_id.clone(),
                        interval: request.interval,
                        open: Price(Decimal::ONE),
                        high: Price(Decimal::ONE),
                        low: Price(Decimal::ONE),
                        close: Price(Decimal::from(day.timestamp() / 86_400 % 100)),
                        volume: 1,
                        time: day,
                        complete: true,
                    });
                    day += Duration::days(1);
                }
                Ok(out)
            }
        };
        let request = |from, to| CandleHistoryRequest {
            instrument_id: id.clone(),
            interval: CandleInterval::OneDay,
            from,
            to,
            limit: None,
        };
        let first = cache.candles(request(at(1), at(10)), fetch).await.unwrap();
        let again = cache.candles(request(at(1), at(10)), fetch).await.unwrap();
        let wider = cache.candles(request(at(1), at(15)), fetch).await.unwrap();
        assert_eq!(
            *asked.lock().unwrap(),
            vec![(at(1), at(10)), (at(10), at(15))]
        );
        assert_eq!((first.len(), again.len(), wider.len()), (9, 9, 14));
        assert_eq!(
            first.iter().map(|c| c.close).collect::<Vec<_>>(),
            again[..9].iter().map(|c| c.close).collect::<Vec<_>>()
        );
        for statement in [
            "DELETE FROM candles WHERE instrument_id = $1",
            "DELETE FROM candle_coverage WHERE instrument_id = $1",
        ] {
            sqlx::query(statement)
                .bind(id.to_string())
                .execute(storage.pool())
                .await
                .unwrap();
        }
    }
}
