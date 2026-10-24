#![cfg(feature = "engine")]

//! Проверка рынка перед отправкой ноги: отложенная нога не уходит брокеру и не
//! меняет durable state, соседние ноги исполняются, отклонённая — завершает
//! batch без заявок.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use serde_json::json;
use tinkoff_rs::{
    ApiMetadata, ApiResponse, Result,
    broker::{Portfolio, Trading},
    domain::{
        AccountId, ExecutionPhase, InstrumentId, MarginConsent, OrderIntent, OrderRequestId,
        OrderSide, OrderStatus, OrderUpdate, PortfolioSnapshot, StrategyId,
    },
    engine::{
        BatchExecutionOutcome, BatchExecutor, RebalanceBatch, RebalanceBatchStatus, RiskAnchor,
        RiskConfig, RiskManager, SubmissionDecision, SubmissionGuard, TargetPortfolio,
        TargetWeight,
    },
    storage::{IntentStatus, MemoryStorage, Storage},
};
use uuid::Uuid;

/// Брокер, у которого каждая рыночная заявка сразу исполняется; счёт пуст.
#[derive(Default)]
struct Broker {
    placed: Mutex<Vec<InstrumentId>>,
}

#[async_trait]
impl Portfolio for Broker {
    async fn portfolio(&self, account: &AccountId) -> Result<ApiResponse<PortfolioSnapshot>> {
        Ok(ApiResponse {
            data: PortfolioSnapshot {
                account_id: account.clone(),
                total: None,
                positions: Vec::new(),
                observed_at: Utc::now(),
            },
            metadata: ApiMetadata::default(),
        })
    }
}

#[async_trait]
impl Trading for Broker {
    async fn place_order(&self, intent: &OrderIntent) -> Result<ApiResponse<OrderUpdate>> {
        self.placed
            .lock()
            .unwrap()
            .push(intent.instrument_id.clone());
        Ok(ApiResponse {
            data: OrderUpdate {
                request_id: intent.request_id,
                exchange_order_id: Some(format!("x-{}", intent.leg_index)),
                status: OrderStatus::Filled,
                lots_requested: intent.lots,
                lots_executed: intent.lots,
                observed_at: Utc::now(),
            },
            metadata: ApiMetadata::default(),
        })
    }

    async fn order_state(
        &self,
        _: &AccountId,
        _: OrderRequestId,
    ) -> Result<ApiResponse<OrderUpdate>> {
        panic!("no order was ambiguous")
    }
}

/// Решения проверки по инструменту; по умолчанию — отправлять.
#[derive(Default)]
struct Guard(Mutex<HashMap<String, SubmissionDecision>>);

impl Guard {
    fn set(&self, instrument: &str, decision: SubmissionDecision) {
        self.0
            .lock()
            .unwrap()
            .insert(instrument.to_owned(), decision);
    }
}

#[async_trait]
impl SubmissionGuard for Guard {
    async fn check(&self, intent: &OrderIntent) -> SubmissionDecision {
        self.0
            .lock()
            .unwrap()
            .get(intent.instrument_id.as_str())
            .cloned()
            .unwrap_or(SubmissionDecision::Proceed)
    }
}

/// Batch из двух открытий по одному лоту: `thin` и `liquid`.
async fn fixture(storage: &MemoryStorage) -> (RebalanceBatch, Vec<OrderIntent>) {
    let now = Utc::now();
    let strategy = StrategyId::new(format!("guard-{}", Uuid::new_v4())).unwrap();
    let account = AccountId::new(format!("account-{}", Uuid::new_v4())).unwrap();
    storage
        .register_portfolio_strategy(&strategy, &account, "test", &json!({}))
        .await
        .unwrap();
    let config = RiskConfig {
        budget_rub: Decimal::from(1000),
        daily_loss_percent: None,
        max_drawdown_percent: None,
    };
    let risk = RiskManager::new(
        config.clone(),
        RiskAnchor {
            account_id: account.clone(),
            strategy_id: strategy.clone(),
            account_equity: Decimal::from(1000),
            session_date: now.date_naive(),
            observed_at: now,
        },
    )
    .unwrap();
    let id = Uuid::new_v4();
    let names = ["thin", "liquid"];
    let target = TargetPortfolio {
        batch_id: id,
        event_id: "schedule:open".into(),
        strategy_id: strategy.clone(),
        account_id: account.clone(),
        source_as_of: now,
        generated_at: now,
        valid_until: now + Duration::minutes(30),
        weights: names
            .iter()
            .map(|name| TargetWeight {
                instrument_id: InstrumentId::new(*name).unwrap(),
                weight: Decimal::new(1, 1),
            })
            .collect(),
    };
    let batch = RebalanceBatch {
        id,
        event_id: target.event_id.clone(),
        strategy_id: strategy.clone(),
        account_id: account.clone(),
        target,
        effective_risk: config.into(),
        sizing_capital: Decimal::from(1000),
        status: RebalanceBatchStatus::Planned,
        created_at: now,
    };
    let intents: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(leg, name)| {
            OrderIntent::market(
                id,
                i32::try_from(leg).unwrap(),
                &batch.event_id,
                strategy.clone(),
                account.clone(),
                InstrumentId::new(*name).unwrap(),
                OrderSide::Buy,
                1,
                1,
                1,
                ExecutionPhase::Opening,
                MarginConsent::CoveredOnly,
                now,
            )
        })
        .collect();
    storage
        .commit_portfolio_event(
            &strategy,
            &batch.event_id,
            &json!({}),
            Some(&batch),
            &intents,
            Some(risk.state()),
        )
        .await
        .unwrap();
    (batch, intents)
}

async fn statuses(storage: &MemoryStorage, batch: &RebalanceBatch) -> Vec<IntentStatus> {
    storage
        .batch_intents(batch.id)
        .await
        .unwrap()
        .into_iter()
        .map(|stored| stored.status)
        .collect()
}

#[tokio::test]
async fn a_deferred_leg_waits_without_a_claim_while_the_rest_of_the_phase_executes() {
    let storage = Arc::new(MemoryStorage::default());
    let (batch, _) = fixture(storage.as_ref()).await;
    let broker = Arc::new(Broker::default());
    let guard = Arc::new(Guard::default());
    guard.set("thin", SubmissionDecision::Defer("thin book".into()));
    let executor = BatchExecutor::new(
        batch.account_id.clone(),
        batch.strategy_id.clone(),
        broker.clone(),
        broker.clone(),
        storage.clone(),
    )
    .with_guard(guard.clone());

    assert_eq!(
        executor.execute_batch(batch.id).await.unwrap(),
        BatchExecutionOutcome::AwaitingFills
    );
    assert_eq!(
        *broker.placed.lock().unwrap(),
        vec![InstrumentId::new("liquid").unwrap()]
    );
    assert_eq!(
        statuses(&storage, &batch).await,
        vec![IntentStatus::Pending, IntentStatus::Filled],
        "the deferred leg is not claimed"
    );
    // Ещё один проход с тем же стаканом ничего не отправляет.
    assert_eq!(
        executor.execute_batch(batch.id).await.unwrap(),
        BatchExecutionOutcome::AwaitingFills
    );
    assert_eq!(broker.placed.lock().unwrap().len(), 1);

    // Стакан восстановился: нога уходит, batch завершён.
    guard.set("thin", SubmissionDecision::Proceed);
    assert_eq!(
        executor.execute_batch(batch.id).await.unwrap(),
        BatchExecutionOutcome::Completed
    );
    assert_eq!(broker.placed.lock().unwrap().len(), 2);
    assert_eq!(
        statuses(&storage, &batch).await,
        vec![IntentStatus::Filled, IntentStatus::Filled]
    );
}

#[tokio::test]
async fn a_skipped_leg_fails_the_batch_without_any_order() {
    let storage = Arc::new(MemoryStorage::default());
    let (batch, _) = fixture(storage.as_ref()).await;
    let broker = Arc::new(Broker::default());
    let guard = Arc::new(Guard::default());
    guard.set("thin", SubmissionDecision::Skip("book stayed thin".into()));
    let executor = BatchExecutor::new(
        batch.account_id.clone(),
        batch.strategy_id.clone(),
        broker.clone(),
        broker.clone(),
        storage.clone(),
    )
    .with_guard(guard);

    assert_eq!(
        executor.execute_batch(batch.id).await.unwrap(),
        BatchExecutionOutcome::Failed
    );
    assert!(broker.placed.lock().unwrap().is_empty());
    assert_eq!(
        statuses(&storage, &batch).await,
        vec![IntentStatus::Rejected, IntentStatus::Rejected]
    );
}

#[tokio::test]
async fn without_a_guard_every_leg_is_sent_as_before() {
    let storage = Arc::new(MemoryStorage::default());
    let (batch, _) = fixture(storage.as_ref()).await;
    let broker = Arc::new(Broker::default());
    let executor = BatchExecutor::new(
        batch.account_id.clone(),
        batch.strategy_id.clone(),
        broker.clone(),
        broker.clone(),
        storage.clone(),
    );
    assert_eq!(
        executor.execute_batch(batch.id).await.unwrap(),
        BatchExecutionOutcome::Completed
    );
    assert_eq!(broker.placed.lock().unwrap().len(), 2);
}
