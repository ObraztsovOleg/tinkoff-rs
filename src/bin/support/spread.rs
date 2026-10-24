//! Защита рыночных заявок от широкого спреда и тонкого стакана.
//!
//! Перед первой отправкой ноги читается стакан (глубина [`DEPTH`]) и считается,
//! по какой средней цене рыночная заявка этого объёма прошла бы по видимым
//! заявкам. Если цена хуже середины спреда больше чем на порог (на реальном
//! счёте по умолчанию 0,5%), нога откладывается: executor спросит снова на
//! следующем проходе (risk check — раз в 30 секунд). Пока стакан плохой дольше
//! [`SpreadGuard::wait`], сокращения позиций и переносы фьючерсов исполняются всё
//! равно — риск важнее цены, — а новые позиции не открываются.
//!
//! Защитные закрытия (`risk:`, `cleanup:`) не проверяются никогда. Если стакан
//! прочитать не удалось, заявка уходит как раньше: сбой рыночных данных не должен
//! останавливать торговлю.

use std::{collections::HashMap, sync::Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use tinkoff_rs::{
    Result,
    domain::{ExecutionPhase, InstrumentId, OrderIntent, OrderSide, Price},
    engine::{SubmissionDecision, SubmissionGuard},
    proto::GetOrderBookRequest,
};
use tracing::{info, warn};
use uuid::Uuid;

use crate::venue::VenueApi;

/// Глубина стакана, по которой оценивается цена исполнения.
const DEPTH: i32 = 20;

/// Одна сторона стакана: (цена за бумагу, лотов) от лучшей цены вглубь.
pub(super) type Side = Vec<(Decimal, i64)>;

/// Источник стакана: брокер в работе, фиксированные данные в тестах.
#[async_trait]
pub(super) trait OrderBooks: Send + Sync {
    /// Заявки на покупку и на продажу инструмента от лучшей цены.
    async fn order_book(&self, instrument: &InstrumentId) -> Result<(Side, Side)>;
}

#[async_trait]
impl OrderBooks for VenueApi {
    async fn order_book(&self, instrument: &InstrumentId) -> Result<(Side, Side)> {
        // Серия `futures:BR` торгуется привязанным контрактом.
        let contract = self.series().contract(instrument);
        let book = self
            .market_data()
            .get_order_book(GetOrderBookRequest {
                depth: DEPTH,
                instrument_id: Some(contract.to_string()),
                ..Default::default()
            })
            .await?
            .into_inner();
        let side = |orders: Vec<tinkoff_rs::proto::Order>| -> Result<Side> {
            orders
                .into_iter()
                .filter_map(|order| Some((order.price?, order.quantity)))
                .map(|(price, lots)| Ok((Price::from_quotation(price)?.0, lots)))
                .collect()
        };
        Ok((side(book.bids)?, side(book.asks)?))
    }
}

/// Насколько средняя цена рыночной заявки на `lots` лотов хуже середины спреда,
/// доля. `None` — встречных заявок в видимом стакане не хватает на весь объём.
pub(super) fn execution_cost(
    bids: &Side,
    asks: &Side,
    side: OrderSide,
    lots: i64,
) -> Option<Decimal> {
    let (bid, ask) = (bids.first()?.0, asks.first()?.0);
    let mid = (bid + ask) / Decimal::TWO;
    if mid <= Decimal::ZERO || lots <= 0 {
        return None;
    }
    let book = match side {
        OrderSide::Buy => asks,
        OrderSide::Sell => bids,
    };
    let (mut left, mut paid) = (lots, Decimal::ZERO);
    for (price, available) in book {
        let take = left.min(*available);
        paid += *price * Decimal::from(take);
        left -= take;
        if left == 0 {
            let average = paid / Decimal::from(lots);
            return Some((average - mid).abs() / mid);
        }
    }
    None
}

/// Решение для ноги на реальном рынке.
pub(super) struct SpreadGuard<B> {
    books: B,
    /// Допустимое удорожание относительно середины спреда, доля.
    limit: Decimal,
    /// Сколько ждать нормального стакана.
    wait: Duration,
    /// Когда нога впервые упёрлась в плохой стакан (в памяти процесса: после
    /// перезапуска ожидание начинается заново).
    first_wide: Mutex<HashMap<Uuid, DateTime<Utc>>>,
}

impl<B: OrderBooks> SpreadGuard<B> {
    pub(super) fn new(books: B, limit: Decimal, wait: Duration) -> Self {
        Self {
            books,
            limit,
            wait,
            first_wide: Mutex::new(HashMap::new()),
        }
    }

    fn forget(&self, intent: &OrderIntent) {
        if let Ok(mut map) = self.first_wide.lock() {
            map.remove(&intent.id);
        }
    }

    /// Решение по уже оценённому стакану в момент `now`.
    fn decide(
        &self,
        intent: &OrderIntent,
        cost: Option<Decimal>,
        now: DateTime<Utc>,
    ) -> SubmissionDecision {
        if cost.is_some_and(|cost| cost <= self.limit) {
            self.forget(intent);
            return SubmissionDecision::Proceed;
        }
        let percent = |value: Decimal| (value * Decimal::ONE_HUNDRED).round_dp(2);
        let reason = cost.map_or_else(
            || format!("{}: в стакане не хватает встречных заявок на {} лот.", intent.instrument_id, intent.lots),
            |cost| {
                format!(
                    "{}: рыночная заявка на {} лот. ушла бы на {}% хуже середины спреда (порог {}%)",
                    intent.instrument_id,
                    intent.lots,
                    percent(cost),
                    percent(self.limit)
                )
            },
        );
        let since = self
            .first_wide
            .lock()
            .map_or(now, |mut map| *map.entry(intent.id).or_insert(now));
        if now - since < self.wait {
            info!(%reason, "market order deferred until the order book recovers");
            return SubmissionDecision::Defer(reason);
        }
        self.forget(intent);
        // Риск важнее цены: сокращение позиции и перенос фьючерса исполняются.
        if intent.execution_phase == ExecutionPhase::Reduction
            || intent.event_id.starts_with("roll:")
        {
            warn!(%reason, "order book stayed thin; reducing the position at market anyway");
            return SubmissionDecision::Proceed;
        }
        SubmissionDecision::Skip(format!(
            "{reason}; стакан не восстановился за {} мин — новая позиция не открыта",
            self.wait.num_minutes()
        ))
    }
}

#[async_trait]
impl<B: OrderBooks> SubmissionGuard for SpreadGuard<B> {
    async fn check(&self, intent: &OrderIntent) -> SubmissionDecision {
        // Защитное закрытие при стопе и «Закрыть позиции» не ждут стакана.
        if intent.event_id.starts_with("risk:") || intent.event_id.starts_with("cleanup:") {
            return SubmissionDecision::Proceed;
        }
        match self.books.order_book(&intent.instrument_id).await {
            Ok((bids, asks)) => {
                let cost = execution_cost(&bids, &asks, intent.side, intent.lots);
                self.decide(intent, cost, Utc::now())
            }
            Err(error) => {
                warn!(%error, instrument = %intent.instrument_id, "order book unavailable; sending the market order unchecked");
                SubmissionDecision::Proceed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use tinkoff_rs::domain::{AccountId, MarginConsent, StrategyId};

    use super::*;

    fn d(value: &str) -> Decimal {
        value.parse().unwrap()
    }

    fn intent(side: OrderSide, lots: i64, phase: ExecutionPhase, event: &str) -> OrderIntent {
        OrderIntent::market(
            Uuid::new_v4(),
            0,
            event,
            StrategyId::new("s").unwrap(),
            AccountId::new("a").unwrap(),
            InstrumentId::new("sber").unwrap(),
            side,
            lots,
            lots,
            1,
            phase,
            MarginConsent::CoveredOnly,
            Utc::now(),
        )
    }

    struct Book(Side, Side);

    #[async_trait]
    impl OrderBooks for Book {
        async fn order_book(&self, _: &InstrumentId) -> Result<(Side, Side)> {
            Ok((self.0.clone(), self.1.clone()))
        }
    }

    #[test]
    fn the_cost_walks_the_book_for_the_whole_order() {
        let bids = vec![(d("99.9"), 10), (d("99.5"), 100)];
        let asks = vec![(d("100.1"), 10), (d("101"), 100)];
        // Середина 100: 10 лотов по 100.1 — 0,1%; 20 лотов — средняя 100.55, 0,55%.
        assert_eq!(
            execution_cost(&bids, &asks, OrderSide::Buy, 10),
            Some(d("0.001"))
        );
        assert_eq!(
            execution_cost(&bids, &asks, OrderSide::Buy, 20),
            Some(d("0.0055"))
        );
        assert_eq!(
            execution_cost(&bids, &asks, OrderSide::Sell, 20),
            Some(d("0.003"))
        );
        assert_eq!(
            execution_cost(&bids, &asks, OrderSide::Buy, 111),
            None,
            "not enough offers"
        );
        assert_eq!(
            execution_cost(&bids, &Vec::new(), OrderSide::Sell, 1),
            None,
            "one-sided book"
        );
    }

    #[tokio::test]
    async fn a_thin_book_defers_then_reductions_go_and_openings_are_skipped() {
        let thin = Book(vec![(d("98"), 5)], vec![(d("102"), 5)]);
        let guard = SpreadGuard::new(thin, d("0.005"), Duration::minutes(30));
        let opening = intent(OrderSide::Buy, 3, ExecutionPhase::Opening, "schedule:open");
        let reduction = intent(
            OrderSide::Sell,
            3,
            ExecutionPhase::Reduction,
            "schedule:open",
        );
        let roll = intent(
            OrderSide::Buy,
            3,
            ExecutionPhase::Opening,
            "roll:BR:2026-10-28",
        );
        let now = Utc::now();
        for leg in [&opening, &reduction, &roll] {
            assert!(
                matches!(guard.decide(leg, Some(d("0.02")), now), SubmissionDecision::Defer(reason) if reason.contains("2.00%"))
            );
        }
        let later = now + Duration::minutes(31);
        assert_eq!(
            guard.decide(&reduction, Some(d("0.02")), later),
            SubmissionDecision::Proceed
        );
        assert_eq!(
            guard.decide(&roll, Some(d("0.02")), later),
            SubmissionDecision::Proceed
        );
        assert!(
            matches!(guard.decide(&opening, Some(d("0.02")), later), SubmissionDecision::Skip(reason) if reason.contains("30 мин"))
        );
        // Через сеть: 2% против порога 0,5% — отложить; защитное закрытие — сразу.
        assert!(matches!(
            guard
                .check(&intent(
                    OrderSide::Buy,
                    1,
                    ExecutionPhase::Opening,
                    "schedule:x"
                ))
                .await,
            SubmissionDecision::Defer(_)
        ));
        assert_eq!(
            guard
                .check(&intent(
                    OrderSide::Sell,
                    1,
                    ExecutionPhase::Reduction,
                    "risk:abc"
                ))
                .await,
            SubmissionDecision::Proceed
        );
        assert_eq!(
            guard
                .check(&intent(
                    OrderSide::Sell,
                    1,
                    ExecutionPhase::Reduction,
                    "cleanup:abc"
                ))
                .await,
            SubmissionDecision::Proceed
        );
    }

    #[tokio::test]
    async fn a_normal_book_sends_at_once_and_resets_the_wait() {
        let guard = SpreadGuard::new(
            Book(vec![(d("99.95"), 50)], vec![(d("100.05"), 50)]),
            d("0.005"),
            Duration::minutes(30),
        );
        let leg = intent(OrderSide::Buy, 10, ExecutionPhase::Opening, "schedule:open");
        let now = Utc::now();
        assert!(matches!(
            guard.decide(&leg, Some(d("0.02")), now - Duration::minutes(40)),
            SubmissionDecision::Defer(_)
        ));
        assert_eq!(guard.check(&leg).await, SubmissionDecision::Proceed);
        // Ожидание сброшено: новый плохой стакан снова откладывает, а не пропускает.
        assert!(matches!(
            guard.decide(&leg, Some(d("0.02")), now),
            SubmissionDecision::Defer(_)
        ));
    }
}
