#![cfg(feature = "engine")]

use async_trait::async_trait;
use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tinkoff_rs::{
    ApiResponse, Error, Result,
    broker::{Portfolio, Trading},
    domain::{
        AccountId, ExecutionPhase, InstrumentId, MarginConsent, OrderIntent, OrderRequestId,
        OrderSide, OrderUpdate, PortfolioSnapshot, StrategyId,
    },
    engine::{
        BatchExecutionOutcome, BatchExecutor, RebalanceBatch, RebalanceBatchStatus, RiskAnchor,
        RiskConfig, RiskManager, TargetPortfolio, TargetWeight,
    },
    storage::{BatchExecutionGate, IntentStatus, MemoryStorage, Storage},
};
use uuid::Uuid;

#[derive(Default)]
struct NeverSubmit {
    reconciliations: AtomicUsize,
}
#[async_trait]
impl Portfolio for NeverSubmit {
    async fn portfolio(&self, _: &AccountId) -> Result<ApiResponse<PortfolioSnapshot>> {
        panic!("blocked execution must not request a new portfolio")
    }
}
#[async_trait]
impl Trading for NeverSubmit {
    async fn place_order(&self, _: &OrderIntent) -> Result<ApiResponse<OrderUpdate>> {
        panic!("blocked execution submitted a broker order")
    }
    async fn order_state(
        &self,
        _: &AccountId,
        _: OrderRequestId,
    ) -> Result<ApiResponse<OrderUpdate>> {
        self.reconciliations.fetch_add(1, Ordering::SeqCst);
        Err(Error::Status(Box::new(tonic::Status::not_found(
            "request absent",
        ))))
    }
}

async fn fixture<S: Storage>(storage: &S) -> (RebalanceBatch, OrderIntent) {
    let now = Utc::now();
    let strategy = StrategyId::new(format!("control-{}", Uuid::new_v4())).unwrap();
    let account = AccountId::new(format!("account-{}", Uuid::new_v4())).unwrap();
    let uid = InstrumentId::new("instrument").unwrap();
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
    let target = TargetPortfolio {
        batch_id: id,
        event_id: "control-fixture".into(),
        strategy_id: strategy.clone(),
        account_id: account.clone(),
        source_as_of: now,
        generated_at: now,
        valid_until: now + Duration::minutes(5),
        weights: vec![TargetWeight {
            instrument_id: uid.clone(),
            weight: Decimal::new(1, 1),
        }],
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
    let intent = OrderIntent::market(
        id,
        0,
        &batch.event_id,
        strategy.clone(),
        account,
        uid,
        OrderSide::Buy,
        1,
        1,
        1,
        ExecutionPhase::Opening,
        MarginConsent::CoveredOnly,
        now,
    );
    storage
        .commit_portfolio_event(
            &strategy,
            &batch.event_id,
            &json!({}),
            Some(&batch),
            std::slice::from_ref(&intent),
            Some(risk.state()),
        )
        .await
        .unwrap();
    (batch, intent)
}

#[tokio::test]
async fn approval_hold_survives_executor_recreation_and_does_not_claim() {
    let storage = Arc::new(MemoryStorage::default());
    let (batch, intent) = fixture(storage.as_ref()).await;
    storage
        .set_batch_execution_gate(batch.id, BatchExecutionGate::Hold)
        .await;
    let broker = Arc::new(NeverSubmit::default());
    for _ in 0..2 {
        let executor = BatchExecutor::new(
            batch.account_id.clone(),
            batch.strategy_id.clone(),
            broker.clone(),
            broker.clone(),
            storage.clone(),
        );
        assert_eq!(
            executor.execute_batch(batch.id).await.unwrap(),
            BatchExecutionOutcome::AwaitingFills
        );
        assert!(!storage.claim_intent(intent.id).await.unwrap());
    }
    assert_eq!(
        storage.batch_intents(batch.id).await.unwrap()[0].status,
        IntentStatus::Pending
    );
}

#[tokio::test]
async fn cancellation_of_ambiguous_request_reconciles_without_resubmission() {
    let storage = Arc::new(MemoryStorage::default());
    let (batch, intent) = fixture(storage.as_ref()).await;
    assert!(storage.claim_intent(intent.id).await.unwrap());
    storage
        .mark_uncertain(intent.id, "simulated disconnect")
        .await
        .unwrap();
    storage
        .set_batch_execution_gate(batch.id, BatchExecutionGate::Cancel)
        .await;
    let broker = Arc::new(NeverSubmit::default());
    let executor = BatchExecutor::new(
        batch.account_id.clone(),
        batch.strategy_id.clone(),
        broker.clone(),
        broker.clone(),
        storage.clone(),
    );
    assert_eq!(
        executor.execute_batch(batch.id).await.unwrap(),
        BatchExecutionOutcome::Failed
    );
    assert_eq!(broker.reconciliations.load(Ordering::SeqCst), 1);
    assert_eq!(
        storage.batch_intents(batch.id).await.unwrap()[0].status,
        IntentStatus::Rejected
    );
}

#[cfg(feature = "postgres")]
#[tokio::test]
#[ignore = "requires CONTROL_TEST_DATABASE_URL pointing to an isolated PostgreSQL"]
async fn postgres_pause_and_approval_are_checked_in_atomic_claim() {
    use tinkoff_rs::storage::PostgresStorage;
    let storage = PostgresStorage::connect(&std::env::var("CONTROL_TEST_DATABASE_URL").unwrap(), 5)
        .await
        .unwrap();
    storage.migrate().await.unwrap();
    // Exercise the schema used by account-command routing, including when the
    // binary was built incrementally after a new migration was added.
    sqlx::query("SELECT execution_strategy_id FROM control_commands WHERE false")
        .execute(storage.pool())
        .await
        .unwrap();
    sqlx::query("SELECT superseded_by FROM strategy_controls WHERE false")
        .execute(storage.pool())
        .await
        .unwrap();
    let (batch, intent) = fixture(&storage).await;
    sqlx::query("INSERT INTO strategy_controls(strategy_id,account_id,enabled,execution_mode) VALUES($1,$2,false,'confirm')")
        .bind(batch.strategy_id.as_str()).bind(batch.account_id.as_str()).execute(storage.pool()).await.unwrap();
    assert!(!storage.claim_intent(intent.id).await.unwrap());
    sqlx::query("UPDATE strategy_controls SET enabled=true WHERE strategy_id=$1")
        .bind(batch.strategy_id.as_str())
        .execute(storage.pool())
        .await
        .unwrap();
    assert_eq!(
        storage.batch_execution_gate(batch.id).await.unwrap(),
        BatchExecutionGate::Hold
    );
    sqlx::query("INSERT INTO batch_controls(batch_id,status,revision) VALUES($1,'approved',0)")
        .bind(batch.id)
        .execute(storage.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE strategy_controls SET revision=1 WHERE strategy_id=$1")
        .bind(batch.strategy_id.as_str())
        .execute(storage.pool())
        .await
        .unwrap();
    assert!(!storage.claim_intent(intent.id).await.unwrap());
    sqlx::query("UPDATE batch_controls SET revision=1 WHERE batch_id=$1")
        .bind(batch.id)
        .execute(storage.pool())
        .await
        .unwrap();
    assert!(storage.claim_intent(intent.id).await.unwrap());
    assert!(!storage.claim_intent(intent.id).await.unwrap());
}
