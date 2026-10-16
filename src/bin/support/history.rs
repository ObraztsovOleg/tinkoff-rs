//! Bounded, newest-first history pagination shared by startup and catchup.
use chrono::Duration;
use tinkoff_rs::{
    Result,
    broker::MarketData,
    domain::{Candle, CandleHistoryRequest, CandleInterval},
};

// Windows also stay below the maximum number of bars in an always-open market.
fn bounds(interval: CandleInterval) -> (Duration, Duration, i32) {
    use CandleInterval as I;
    match interval {
        I::FiveSeconds => (Duration::minutes(180), Duration::seconds(5), 2500),
        I::TenSeconds => (Duration::minutes(180), Duration::seconds(10), 1250),
        I::ThirtySeconds => (Duration::hours(18), Duration::seconds(30), 2500),
        I::OneMinute => (Duration::hours(23), Duration::minutes(1), 2400),
        I::TwoMinutes => (Duration::hours(23), Duration::minutes(2), 1200),
        I::ThreeMinutes => (Duration::hours(23), Duration::minutes(3), 750),
        I::FiveMinutes => (Duration::days(6), Duration::minutes(5), 2400),
        I::TenMinutes => (Duration::days(6), Duration::minutes(10), 1200),
        I::FifteenMinutes => (Duration::days(14), Duration::minutes(15), 2400),
        I::ThirtyMinutes => (Duration::days(14), Duration::minutes(30), 1200),
        I::OneHour => (Duration::days(60), Duration::hours(1), 2400),
        I::TwoHours => (Duration::days(60), Duration::hours(2), 2400),
        I::FourHours => (Duration::days(60), Duration::hours(4), 700),
        I::OneDay => (Duration::days(365), Duration::days(1), 2400),
        I::OneWeek => (Duration::days(365), Duration::weeks(1), 300),
        I::OneMonth => (Duration::days(365 * 5), Duration::days(32), 120),
    }
}

pub(super) async fn load(
    broker: &impl MarketData,
    request: CandleHistoryRequest,
    latest: Option<usize>,
) -> Result<Vec<Candle>> {
    let (span, minimum, limit) = bounds(request.interval);
    let mut cursor = request.to;
    let mut candles = Vec::new();
    while cursor > request.from {
        let start = (cursor - span).max(request.from);
        let query_start = start.min(cursor - minimum - Duration::seconds(1));
        let page = broker
            .candles(CandleHistoryRequest {
                instrument_id: request.instrument_id.clone(),
                interval: request.interval,
                from: query_start,
                to: cursor,
                limit: Some(limit),
            })
            .await?
            .data;
        candles.extend(
            page.into_iter()
                .filter(|c| c.complete && c.time >= request.from && c.time <= request.to),
        );
        candles.sort_by_key(|c| c.time);
        candles.dedup_by_key(|c| c.time);
        if latest.is_some_and(|n| candles.len() >= n) {
            break;
        }
        cursor = start;
    }
    if let Some(n) = latest {
        let discard = candles.len().saturating_sub(n);
        candles.drain(..discard);
    }
    Ok(candles)
}

#[cfg(test)]
mod tests {
    use super::{bounds, load};
    use chrono::{Duration, Utc};
    use rust_decimal::Decimal;
    use std::sync::Mutex;
    use tinkoff_rs::{
        ApiResponse, Result,
        broker::MarketData,
        domain::{
            Candle, CandleHistoryRequest, CandleInterval, InstrumentId, Price, TradingAvailability,
        },
    };

    struct History {
        bars: Vec<Candle>,
        requests: Mutex<Vec<CandleHistoryRequest>>,
    }

    #[async_trait::async_trait]
    impl MarketData for History {
        async fn candles(&self, request: CandleHistoryRequest) -> Result<ApiResponse<Vec<Candle>>> {
            assert!(request.to - request.from <= Duration::weeks(3));
            assert!(request.limit.unwrap() <= 1200);
            let mut data = self
                .bars
                .iter()
                .filter(|c| c.time >= request.from && c.time <= request.to)
                .cloned()
                .collect::<Vec<_>>();
            data.reverse();
            data.extend(data.clone());
            self.requests.lock().unwrap().push(request);
            Ok(ApiResponse {
                data,
                metadata: tinkoff_rs::ApiMetadata::default(),
            })
        }
        async fn trading_availability(
            &self,
            _: &InstrumentId,
        ) -> Result<ApiResponse<TradingAvailability>> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn paginates_latest_complete_bars_without_duplicates_or_gaps() {
        let end = Utc::now();
        let uid = InstrumentId::new("test-uid").unwrap();
        let bars = (0..2000)
            .map(|i| Candle {
                instrument_id: uid.clone(),
                interval: CandleInterval::ThirtyMinutes,
                time: end - Duration::minutes((1999 - i) * 30),
                complete: i != 1999,
                open: Price::new(Decimal::ONE),
                high: Price::new(Decimal::ONE),
                low: Price::new(Decimal::ONE),
                close: Price::new(Decimal::ONE),
                volume: 1,
            })
            .collect();
        let source = History {
            bars,
            requests: Mutex::new(Vec::new()),
        };
        let result = load(
            &source,
            CandleHistoryRequest {
                instrument_id: uid,
                interval: CandleInterval::ThirtyMinutes,
                from: end - Duration::days(45),
                to: end,
                limit: None,
            },
            Some(1001),
        )
        .await
        .unwrap();
        assert_eq!(result.len(), 1001);
        assert_eq!(result.last().unwrap().time, end - Duration::minutes(30));
        assert!(
            result
                .windows(2)
                .all(|w| w[1].time - w[0].time == Duration::minutes(30))
        );
        assert_eq!(source.requests.lock().unwrap().len(), 2);
    }

    #[test]
    fn history_windows_obey_api_and_capacity_limits() {
        let (span, _, limit) = bounds(CandleInterval::ThirtyMinutes);
        assert!(span <= Duration::weeks(3));
        assert!(span.num_minutes() / 30 + 1 < i64::from(limit));
        assert_eq!(bounds(CandleInterval::OneMonth).2, 120);
        assert_eq!(bounds(CandleInterval::OneDay).2, 2400);
    }
}
