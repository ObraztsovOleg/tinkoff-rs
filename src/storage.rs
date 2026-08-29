//! Durable-storage для portfolio runtime версии 2.
//!
//! Модуль задаёт одинаковые гарантии для in-memory и `PostgreSQL` реализаций.
//! Главное правило — новое состояние стратегии, marker обработанного события,
//! target, effective risk и все ноги batch сохраняются как одно целое.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{
    Error, Result,
    domain::{
        AccountId, Candle, CandleInterval, ExecutionFill, InstrumentId, OrderIntent, OrderStatus,
        OrderUpdate, StrategyId,
    },
    engine::{AccountRiskState, RebalanceBatch, RebalanceBatchStatus},
};

/// Durable-состояние одной ноги между планированием и исполнением.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntentStatus {
    /// Нога сохранена, но право на отправку ещё не захвачено worker-ом.
    Pending,
    /// Worker атомарно захватил ногу и мог начать сетевой запрос.
    Submitting,
    /// Брокер знает заявку, но полное исполнение пока не подтверждено.
    Accepted,
    /// Запрошенное количество исполнено полностью.
    Filled,
    /// Заявка отменена.
    Cancelled,
    /// Заявка отклонена брокером либо локальным risk-контролем.
    Rejected,
    /// Не удалось доказать, был ли запрос принят брокером; нужен reconciliation.
    Uncertain,
}

impl IntentStatus {
    #[must_use]
    /// Возвращает стабильное строковое представление для `PostgreSQL`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Submitting => "submitting",
            Self::Accepted => "accepted",
            Self::Filled => "filled",
            Self::Cancelled => "cancelled",
            Self::Rejected => "rejected",
            Self::Uncertain => "uncertain",
        }
    }
}

/// Сохранённый intent вместе с его текущим durable-статусом.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredIntent {
    /// Неизменяемые параметры ноги и стабильный request UUID.
    pub intent: OrderIntent,
    /// Последнее подтверждённое storage состояние.
    pub status: IntentStatus,
}

#[async_trait]
/// Гарантии хранения для [`crate::engine::PortfolioEngine`] и
/// [`crate::engine::BatchExecutor`].
pub trait Storage: Send + Sync {
    /// Регистрирует strategy ID за конкретными account и kind.
    ///
    /// Повторная регистрация обновляет config только при совпадении владельца и
    /// типа; использовать тот же ID для другой стратегии нельзя.
    async fn register_portfolio_strategy(
        &self,
        strategy_id: &StrategyId,
        account_id: &AccountId,
        kind: &str,
        config: &Value,
    ) -> Result<()>;

    /// Загружает последний атомарно подтверждённый JSON-state стратегии.
    async fn load_portfolio_strategy_state(
        &self,
        strategy_id: &StrategyId,
    ) -> Result<Option<Value>>;

    /// Атомарно сохраняет marker события, state, batch, ноги и risk-state.
    ///
    /// Возвращает `false`, если `(strategy_id, event_id)` уже был обработан. В
    /// этом случае ни одна часть payload не должна быть записана повторно.
    async fn commit_portfolio_event(
        &self,
        strategy_id: &StrategyId,
        event_id: &str,
        state: &Value,
        batch: Option<&RebalanceBatch>,
        intents: &[OrderIntent],
        risk_state: Option<&AccountRiskState>,
    ) -> Result<bool>;

    /// Загружает durable risk-state выделенного счёта.
    async fn load_risk_state(&self, account_id: &AccountId) -> Result<Option<AccountRiskState>>;
    /// Сохраняет risk-state отдельно, например после периодической переоценки/reset.
    async fn save_risk_state(&self, state: &AccountRiskState) -> Result<()>;
    /// Загружает метаданные одного batch.
    async fn load_batch(&self, batch_id: Uuid) -> Result<Option<RebalanceBatch>>;
    /// Загружает все ноги batch в порядке `leg_index`.
    async fn batch_intents(&self, batch_id: Uuid) -> Result<Vec<StoredIntent>>;
    /// Возвращает batch-и конкретного счёта и стратегии, которые нужно
    /// восстановить после restart. Фильтр по обоим владельцам не позволяет
    /// одному runtime исполнять или отклонять заявки другого ансамбля.
    async fn pending_batch_ids(
        &self,
        account_id: &AccountId,
        strategy_id: &StrategyId,
    ) -> Result<Vec<Uuid>>;
    /// Меняет durable-стадию batch и необязательную причину остановки.
    async fn set_batch_status(
        &self,
        batch_id: Uuid,
        status: RebalanceBatchStatus,
        reason: Option<&str>,
    ) -> Result<()>;

    /// Атомарно переводит intent `Pending → Submitting`.
    ///
    /// `true` означает, что вызывающий worker получил исключительное право на
    /// первичную отправку. Для любого другого исходного статуса возвращает false.
    async fn claim_intent(&self, intent_id: Uuid) -> Result<bool>;
    /// Проверяет незавершённую заявку по счёту и инструменту.
    async fn has_pending_order(
        &self,
        account_id: &AccountId,
        instrument_id: &InstrumentId,
    ) -> Result<bool>;
    /// Проверяет наличие любой незавершённой/неопределённой заявки счёта.
    async fn has_pending_orders(&self, account_id: &AccountId) -> Result<bool>;
    /// Терминально отклоняет intent с аудируемой причиной.
    async fn reject_intent(&self, intent_id: Uuid, reason: &str) -> Result<()>;
    /// Помечает исход заявки неопределённым до reconciliation.
    async fn mark_uncertain(&self, intent_id: Uuid, reason: &str) -> Result<()>;
    /// Атомарно сохраняет broker update и синхронизирует статус intent.
    async fn record_order_update(&self, intent_id: Uuid, update: &OrderUpdate) -> Result<()>;
    /// Upsert-ит свечу по инструменту, интервалу и времени открытия.
    async fn store_candle(&self, candle: &Candle) -> Result<()>;
    /// Идемпотентно сохраняет фактическое исполнение по `trade_id`.
    async fn record_fill(&self, fill: &ExecutionFill) -> Result<()>;
}

/// In-memory реализация тех же storage-гарантий для offline-использования.
///
/// Один mutex играет роль транзакционной границы: внешней долговечности при
/// падении процесса нет, но частично записанного event payload быть не может.
#[derive(Default)]
pub struct MemoryStorage {
    inner: Mutex<MemoryState>,
}

#[derive(Default)]
struct MemoryState {
    strategy_states: HashMap<StrategyId, Value>,
    strategies: HashMap<StrategyId, (AccountId, String, Value)>,
    processed_events: HashSet<(StrategyId, String)>,
    batches: HashMap<Uuid, (RebalanceBatch, Option<String>)>,
    risk_states: HashMap<AccountId, AccountRiskState>,
    intents: HashMap<Uuid, (OrderIntent, IntentStatus, Option<String>)>,
    orders: HashMap<Uuid, OrderUpdate>,
    candles: HashMap<(InstrumentId, CandleInterval, chrono::DateTime<chrono::Utc>), Candle>,
    fills: HashMap<String, ExecutionFill>,
}

#[async_trait]
impl Storage for MemoryStorage {
    async fn register_portfolio_strategy(
        &self,
        strategy_id: &StrategyId,
        account_id: &AccountId,
        kind: &str,
        config: &Value,
    ) -> Result<()> {
        if kind.trim().is_empty() {
            return Err(Error::Configuration(
                "portfolio strategy kind cannot be empty".to_owned(),
            ));
        }
        let mut inner = self.inner.lock().await;
        match inner.strategies.get_mut(strategy_id) {
            Some((stored_account, _, _)) if stored_account != account_id => {
                return Err(Error::Storage(format!(
                    "strategy `{strategy_id}` is registered to another account"
                )));
            }
            Some((_, stored_kind, _)) if stored_kind != kind => {
                return Err(Error::Storage(format!(
                    "strategy `{strategy_id}` is registered with kind `{stored_kind}`"
                )));
            }
            Some((_, _, stored_config)) => {
                *stored_config = config.clone();
            }
            None => {
                inner.strategies.insert(
                    strategy_id.clone(),
                    (account_id.clone(), kind.to_owned(), config.clone()),
                );
            }
        }
        Ok(())
    }

    async fn load_portfolio_strategy_state(
        &self,
        strategy_id: &StrategyId,
    ) -> Result<Option<Value>> {
        Ok(self
            .inner
            .lock()
            .await
            .strategy_states
            .get(strategy_id)
            .cloned())
    }

    async fn commit_portfolio_event(
        &self,
        strategy_id: &StrategyId,
        event_id: &str,
        state: &Value,
        batch: Option<&RebalanceBatch>,
        intents: &[OrderIntent],
        risk_state: Option<&AccountRiskState>,
    ) -> Result<bool> {
        validate_atomic_payload(strategy_id, event_id, batch, intents, risk_state)?;
        // Mutex удерживается до завершения всех insert-ов: для MemoryStorage это
        // эквивалент одной транзакции PostgreSQL в части атомарной видимости.
        let mut inner = self.inner.lock().await;
        if risk_state.is_some_and(|risk| {
            inner
                .risk_states
                .get(&risk.account_id)
                .is_some_and(|existing| existing.strategy_id != risk.strategy_id)
        }) {
            return Err(Error::Storage(
                "account risk state is owned by another strategy".to_owned(),
            ));
        }
        // Marker проверяется до изменения state/batch, обеспечивая exactly-once
        // применение события в пределах этого storage.
        let event_key = (strategy_id.clone(), event_id.to_owned());
        if inner.processed_events.contains(&event_key) {
            return Ok(false);
        }
        if !inner.strategies.contains_key(strategy_id) {
            return Err(Error::Storage(format!(
                "portfolio strategy `{strategy_id}` is not registered"
            )));
        }
        inner.processed_events.insert(event_key);
        inner
            .strategy_states
            .insert(strategy_id.clone(), state.clone());
        if let Some(batch) = batch {
            inner.batches.insert(batch.id, (batch.clone(), None));
            for intent in intents {
                inner
                    .intents
                    .insert(intent.id, (intent.clone(), IntentStatus::Pending, None));
            }
        }
        if let Some(risk_state) = risk_state {
            inner
                .risk_states
                .insert(risk_state.account_id.clone(), risk_state.clone());
        }
        Ok(true)
    }

    async fn load_risk_state(&self, account_id: &AccountId) -> Result<Option<AccountRiskState>> {
        Ok(self.inner.lock().await.risk_states.get(account_id).cloned())
    }

    async fn save_risk_state(&self, state: &AccountRiskState) -> Result<()> {
        let mut inner = self.inner.lock().await;
        if inner
            .risk_states
            .get(&state.account_id)
            .is_some_and(|existing| existing.strategy_id != state.strategy_id)
        {
            return Err(Error::Storage(
                "account risk state is owned by another strategy".to_owned(),
            ));
        }
        inner
            .risk_states
            .insert(state.account_id.clone(), state.clone());
        Ok(())
    }

    async fn load_batch(&self, batch_id: Uuid) -> Result<Option<RebalanceBatch>> {
        Ok(self
            .inner
            .lock()
            .await
            .batches
            .get(&batch_id)
            .map(|(batch, _)| batch.clone()))
    }

    async fn batch_intents(&self, batch_id: Uuid) -> Result<Vec<StoredIntent>> {
        let mut intents: Vec<_> = self
            .inner
            .lock()
            .await
            .intents
            .values()
            .filter(|(intent, _, _)| intent.batch_id == batch_id)
            .map(|(intent, status, _)| StoredIntent {
                intent: intent.clone(),
                status: *status,
            })
            .collect();
        intents.sort_by_key(|stored| stored.intent.leg_index);
        Ok(intents)
    }

    async fn pending_batch_ids(
        &self,
        account_id: &AccountId,
        strategy_id: &StrategyId,
    ) -> Result<Vec<Uuid>> {
        let mut ids: Vec<_> = self
            .inner
            .lock()
            .await
            .batches
            .values()
            .filter(|(batch, _)| {
                &batch.account_id == account_id
                    && &batch.strategy_id == strategy_id
                    && !matches!(
                        batch.status,
                        RebalanceBatchStatus::Completed | RebalanceBatchStatus::Failed
                    )
            })
            .map(|(batch, _)| batch.id)
            .collect();
        ids.sort_unstable();
        Ok(ids)
    }

    async fn set_batch_status(
        &self,
        batch_id: Uuid,
        status: RebalanceBatchStatus,
        reason: Option<&str>,
    ) -> Result<()> {
        let mut inner = self.inner.lock().await;
        let Some((batch, stored_reason)) = inner.batches.get_mut(&batch_id) else {
            return Err(Error::Storage(format!(
                "rebalance batch `{batch_id}` was not found"
            )));
        };
        batch.status = status;
        *stored_reason = reason.map(str::to_owned);
        Ok(())
    }

    async fn claim_intent(&self, intent_id: Uuid) -> Result<bool> {
        let mut inner = self.inner.lock().await;
        let Some((_, status, _)) = inner.intents.get_mut(&intent_id) else {
            return Ok(false);
        };
        // Смена статуса выполняется под тем же mutex: два executor-а не могут
        // одновременно получить `true` для одной и той же ноги.
        if *status != IntentStatus::Pending {
            return Ok(false);
        }
        *status = IntentStatus::Submitting;
        Ok(true)
    }

    async fn has_pending_order(
        &self,
        account_id: &AccountId,
        instrument_id: &InstrumentId,
    ) -> Result<bool> {
        Ok(self
            .inner
            .lock()
            .await
            .intents
            .values()
            .any(|(intent, status, _)| {
                &intent.account_id == account_id
                    && &intent.instrument_id == instrument_id
                    && matches!(
                        status,
                        IntentStatus::Pending
                            | IntentStatus::Submitting
                            | IntentStatus::Accepted
                            | IntentStatus::Uncertain
                    )
            }))
    }

    async fn has_pending_orders(&self, account_id: &AccountId) -> Result<bool> {
        Ok(self
            .inner
            .lock()
            .await
            .intents
            .values()
            .any(|(intent, status, _)| {
                &intent.account_id == account_id
                    && matches!(
                        status,
                        IntentStatus::Pending
                            | IntentStatus::Submitting
                            | IntentStatus::Accepted
                            | IntentStatus::Uncertain
                    )
            }))
    }

    async fn reject_intent(&self, intent_id: Uuid, reason: &str) -> Result<()> {
        let mut inner = self.inner.lock().await;
        set_memory_intent_status(&mut inner, intent_id, IntentStatus::Rejected, Some(reason));
        Ok(())
    }

    async fn mark_uncertain(&self, intent_id: Uuid, reason: &str) -> Result<()> {
        let mut inner = self.inner.lock().await;
        set_memory_intent_status(&mut inner, intent_id, IntentStatus::Uncertain, Some(reason));
        Ok(())
    }

    async fn record_order_update(&self, intent_id: Uuid, update: &OrderUpdate) -> Result<()> {
        let mut inner = self.inner.lock().await;
        set_memory_intent_status(
            &mut inner,
            intent_id,
            status_from_order(update.status),
            None,
        );
        inner
            .orders
            .insert(update.request_id.as_uuid(), update.clone());
        Ok(())
    }

    async fn store_candle(&self, candle: &Candle) -> Result<()> {
        self.inner.lock().await.candles.insert(
            (candle.instrument_id.clone(), candle.interval, candle.time),
            candle.clone(),
        );
        Ok(())
    }

    async fn record_fill(&self, fill: &ExecutionFill) -> Result<()> {
        self.inner
            .lock()
            .await
            .fills
            .insert(fill.trade_id.clone(), fill.clone());
        Ok(())
    }
}

fn set_memory_intent_status(
    state: &mut MemoryState,
    intent_id: Uuid,
    status: IntentStatus,
    reason: Option<&str>,
) {
    if let Some((_, stored_status, stored_reason)) = state.intents.get_mut(&intent_id) {
        *stored_status = status;
        if let Some(reason) = reason {
            *stored_reason = Some(reason.to_owned());
        }
    }
}

fn status_from_order(status: OrderStatus) -> IntentStatus {
    // Pending/New/partial/unknown не являются подтверждением полного fill и
    // потому остаются незавершёнными с точки зрения batch executor-а.
    match status {
        OrderStatus::Filled => IntentStatus::Filled,
        OrderStatus::Cancelled => IntentStatus::Cancelled,
        OrderStatus::Rejected => IntentStatus::Rejected,
        OrderStatus::Pending
        | OrderStatus::New
        | OrderStatus::PartiallyFilled
        | OrderStatus::Unknown => IntentStatus::Accepted,
    }
}

fn validate_atomic_payload(
    strategy_id: &StrategyId,
    event_id: &str,
    batch: Option<&RebalanceBatch>,
    intents: &[OrderIntent],
    risk_state: Option<&AccountRiskState>,
) -> Result<()> {
    // Batch, его legs и risk snapshot образуют неделимый агрегат. Запрет
    // частичных комбинаций позволяет обеим Storage-реализациям давать engine-у
    // одинаковые гарантии ещё до начала фактической транзакции.
    match batch {
        None if !intents.is_empty() || risk_state.is_some() => {
            return Err(Error::Storage(
                "intents and risk state require a rebalance batch".to_owned(),
            ));
        }
        None => return Ok(()),
        Some(batch)
            if batch.strategy_id != *strategy_id
                || batch.event_id != event_id
                || batch.id != batch.target.batch_id =>
        {
            return Err(Error::Storage(
                "rebalance batch identity mismatch".to_owned(),
            ));
        }
        Some(_) => {}
    }
    let batch = batch.expect("batch checked above");
    if risk_state.is_none_or(|risk| {
        risk.account_id != batch.account_id || risk.strategy_id != batch.strategy_id
    }) {
        return Err(Error::Storage(
            "rebalance batch requires matching account risk state".to_owned(),
        ));
    }
    let mut legs = HashSet::new();
    for intent in intents {
        // leg_index уникален внутри batch, а все identity-поля должны ссылаться
        // на то же исходное событие/стратегию/счёт. Иначе recovery не сможет
        // доказать, к какому решению относится заявка.
        if intent.batch_id != batch.id
            || intent.strategy_id != *strategy_id
            || intent.account_id != batch.account_id
            || intent.event_id != event_id
            || intent.lots <= 0
            || intent.lot_size <= 0
            || intent.leg_index < 0
            || !legs.insert(intent.leg_index)
        {
            return Err(Error::Storage(
                "invalid or duplicate rebalance leg".to_owned(),
            ));
        }
    }
    Ok(())
}

#[cfg(feature = "postgres")]
mod postgres {
    use async_trait::async_trait;
    use rust_decimal::Decimal;
    use serde_json::Value;
    use sqlx::{PgConnection, PgPool, Row, postgres::PgPoolOptions};
    use uuid::Uuid;

    use super::{IntentStatus, Storage, StoredIntent, status_from_order, validate_atomic_payload};
    use crate::{
        Error, Result,
        domain::{
            AccountId, Candle, CandleInterval, ExecutionFill, ExecutionPhase, InstrumentId,
            MarginConsent, OrderIntent, OrderKind, OrderRequestId, OrderSide, OrderStatus,
            OrderUpdate, Price, StrategyId,
        },
        engine::{
            AccountRiskState, EffectiveRiskConfig, PORTFOLIO_RUNTIME_VERSION, RebalanceBatch,
            RebalanceBatchStatus, TargetPortfolio,
        },
    };

    static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

    /// Namespace session advisory lock-а, отделяющий runtime lease от
    /// возможных advisory lock-ов других подсистем в той же БД.
    const RUNTIME_LEASE_NAMESPACE: &[u8] = b"tinkoff-rs:portfolio-runtime:v2:account:";

    /// Эксклюзивная session lease одного Sandbox-счёта.
    ///
    /// Объект намеренно удерживает отсоединённый от пула `PgConnection`.
    /// PostgreSQL освобождает session advisory lock автоматически при
    /// закрытии этого соединения, поэтому lease действует ровно до `Drop`.
    #[must_use = "dropping RuntimeLease releases exclusive ownership of the account"]
    pub struct RuntimeLease {
        account_id: AccountId,
        _connection: PgConnection,
    }

    impl RuntimeLease {
        /// Возвращает счёт, эксклюзивно закреплённый за этой lease.
        #[must_use]
        pub const fn account_id(&self) -> &AccountId {
            &self.account_id
        }
    }

    /// PostgreSQL-реализация durable storage runtime версии 2.
    #[derive(Clone)]
    pub struct PostgresStorage {
        pool: PgPool,
    }

    impl PostgresStorage {
        /// Создаёт пул соединений с заданным верхним пределом.
        pub async fn connect(database_url: &str, max_connections: u32) -> Result<Self> {
            let pool = PgPoolOptions::new()
                .max_connections(max_connections)
                .connect(database_url)
                .await?;
            Ok(Self { pool })
        }

        #[must_use]
        /// Оборачивает готовый пул, не выполняя миграции автоматически.
        pub fn from_pool(pool: PgPool) -> Self {
            Self { pool }
        }

        #[must_use]
        /// Возвращает underlying pool для контролируемых внешних запросов.
        pub const fn pool(&self) -> &PgPool {
            &self.pool
        }

        /// Пытается эксклюзивно закрепить `account_id` за текущим runtime.
        ///
        /// Session advisory lock берётся на отсоединённом от пула соединении:
        /// пока возвращённый [`RuntimeLease`] жив, второй процесс не сможет
        /// получить lease для того же счёта.
        pub async fn acquire_runtime_lease(&self, account_id: &AccountId) -> Result<RuntimeLease> {
            let pooled = self.pool.acquire().await?;
            let mut connection = pooled.detach();
            let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
                .bind(runtime_lease_key(account_id))
                .fetch_one(&mut connection)
                .await?;
            if !acquired {
                return Err(Error::Storage(format!(
                    "runtime lease for account `{account_id}` is already held by another process"
                )));
            }
            Ok(RuntimeLease {
                account_id: account_id.clone(),
                _connection: connection,
            })
        }

        /// Применяет встроенные SQL-миграции в правильном порядке.
        ///
        /// Миграция runtime v2 сама прекращается при незавершённых legacy-intents.
        pub async fn migrate(&self) -> Result<()> {
            MIGRATOR.run(&self.pool).await?;
            Ok(())
        }
    }

    /// Детерминированно сводит namespace и `account_id` в 64-битный ключ
    /// PostgreSQL advisory lock. FNV-1a здесь не защищает секреты; он нужен
    /// только для стабильного одинакового key в независимых процессах.
    fn runtime_lease_key(account_id: &AccountId) -> i64 {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for byte in RUNTIME_LEASE_NAMESPACE
            .iter()
            .copied()
            .chain(account_id.as_str().bytes())
        {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        i64::from_be_bytes(hash.to_be_bytes())
    }

    #[async_trait]
    impl Storage for PostgresStorage {
        async fn register_portfolio_strategy(
            &self,
            strategy_id: &StrategyId,
            account_id: &AccountId,
            kind: &str,
            config: &Value,
        ) -> Result<()> {
            let changed = sqlx::query(
                "INSERT INTO strategy_instances (id, account_id, kind, config, runtime_version) \
                 VALUES ($1, $2, $3, $4, 2) \
                 ON CONFLICT (id) DO UPDATE SET config = EXCLUDED.config, \
                 updated_at = now() WHERE strategy_instances.runtime_version = 2 \
                 AND strategy_instances.account_id = EXCLUDED.account_id \
                 AND strategy_instances.kind = EXCLUDED.kind",
            )
            .bind(strategy_id.as_str())
            .bind(account_id.as_str())
            .bind(kind)
            .bind(config)
            .execute(&self.pool)
            .await?
            .rows_affected();
            if changed != 1 {
                return Err(Error::Storage(format!(
                    "strategy `{strategy_id}` belongs to legacy runtime version one, another account, or another kind; use a new strategy_id"
                )));
            }
            Ok(())
        }

        async fn load_portfolio_strategy_state(
            &self,
            strategy_id: &StrategyId,
        ) -> Result<Option<Value>> {
            let row = sqlx::query(
                "SELECT state, runtime_version FROM strategy_state WHERE strategy_id = $1",
            )
            .bind(strategy_id.as_str())
            .fetch_optional(&self.pool)
            .await?;
            let Some(row) = row else {
                return Ok(None);
            };
            let runtime_version: i16 = row.try_get("runtime_version")?;
            if runtime_version != PORTFOLIO_RUNTIME_VERSION {
                return Err(Error::Storage(format!(
                    "strategy state `{strategy_id}` uses unsupported runtime version {runtime_version}"
                )));
            }
            Ok(Some(row.try_get("state")?))
        }

        async fn commit_portfolio_event(
            &self,
            strategy_id: &StrategyId,
            event_id: &str,
            state: &Value,
            batch: Option<&RebalanceBatch>,
            intents: &[OrderIntent],
            risk_state: Option<&AccountRiskState>,
        ) -> Result<bool> {
            validate_atomic_payload(strategy_id, event_id, batch, intents, risk_state)?;
            // PostgreSQL transaction — общая граница commit для event marker,
            // state transition, target, risk snapshot и каждой ноги batch.
            let mut transaction = self.pool.begin().await?;
            // Уникальность (strategy_id, event_id) обеспечивает идемпотентность.
            // Конфликт приводит к rollback до любых остальных изменений.
            let inserted = sqlx::query(
                "INSERT INTO processed_events (strategy_id, event_id) VALUES ($1, $2) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(strategy_id.as_str())
            .bind(event_id)
            .execute(&mut *transaction)
            .await?
            .rows_affected();
            if inserted == 0 {
                transaction.rollback().await?;
                return Ok(false);
            }
            sqlx::query(
                "INSERT INTO strategy_state (strategy_id, state, version, runtime_version) \
                 VALUES ($1, $2, 1, 2) ON CONFLICT (strategy_id) DO UPDATE SET \
                 state = EXCLUDED.state, version = strategy_state.version + 1, \
                 runtime_version = 2, updated_at = now() \
                 WHERE strategy_state.runtime_version = 2",
            )
            .bind(strategy_id.as_str())
            .bind(state)
            .execute(&mut *transaction)
            .await?;
            if let Some(batch) = batch {
                // Сначала сохраняется parent batch, затем его immutable legs и
                // соответствующий risk-state; наружу они станут видимы вместе.
                insert_batch(&mut transaction, batch).await?;
                for intent in intents {
                    insert_intent(&mut transaction, intent).await?;
                }
                upsert_risk_state(
                    &mut transaction,
                    risk_state.expect("validated matching risk state"),
                )
                .await?;
            }
            transaction.commit().await?;
            Ok(true)
        }

        async fn load_risk_state(
            &self,
            account_id: &AccountId,
        ) -> Result<Option<AccountRiskState>> {
            let row = sqlx::query(
                "SELECT state FROM account_risk_state WHERE account_id = $1 AND runtime_version = 2",
            )
            .bind(account_id.as_str())
            .fetch_optional(&self.pool)
            .await?;
            row.map(|row| decode_json(row.try_get("state")?, "account risk state"))
                .transpose()
        }

        async fn save_risk_state(&self, state: &AccountRiskState) -> Result<()> {
            let mut transaction = self.pool.begin().await?;
            upsert_risk_state(&mut transaction, state).await?;
            transaction.commit().await?;
            Ok(())
        }

        async fn load_batch(&self, batch_id: Uuid) -> Result<Option<RebalanceBatch>> {
            let row = sqlx::query(
                "SELECT id, event_id, strategy_id, account_id, target, effective_risk, \
                 sizing_capital, status, created_at FROM rebalance_batches WHERE id = $1",
            )
            .bind(batch_id)
            .fetch_optional(&self.pool)
            .await?;
            row.as_ref().map(row_to_batch).transpose()
        }

        async fn batch_intents(&self, batch_id: Uuid) -> Result<Vec<StoredIntent>> {
            let rows = sqlx::query(
                "SELECT id, request_id, batch_id, leg_index, event_id, strategy_id, account_id, \
                 instrument_id, side, kind, lots, target_lots, lot_size, execution_phase, margin_consent, \
                 limit_price, status, created_at FROM trade_intents \
                 WHERE runtime_version = 2 AND batch_id = $1 ORDER BY leg_index",
            )
            .bind(batch_id)
            .fetch_all(&self.pool)
            .await?;
            rows.iter().map(row_to_stored_intent).collect()
        }

        async fn pending_batch_ids(
            &self,
            account_id: &AccountId,
            strategy_id: &StrategyId,
        ) -> Result<Vec<Uuid>> {
            let rows = sqlx::query(
                "SELECT id FROM rebalance_batches WHERE account_id = $1 AND strategy_id = $2 \
                 AND status NOT IN ('completed', 'failed') ORDER BY created_at",
            )
            .bind(account_id.as_str())
            .bind(strategy_id.as_str())
            .fetch_all(&self.pool)
            .await?;
            rows.iter()
                .map(|row| row.try_get("id").map_err(Error::from))
                .collect()
        }

        async fn set_batch_status(
            &self,
            batch_id: Uuid,
            status: RebalanceBatchStatus,
            reason: Option<&str>,
        ) -> Result<()> {
            let changed = sqlx::query(
                "UPDATE rebalance_batches SET status = $2, reason = $3, updated_at = now() WHERE id = $1",
            )
            .bind(batch_id)
            .bind(status.as_str())
            .bind(reason)
            .execute(&self.pool)
            .await?
            .rows_affected();
            if changed != 1 {
                return Err(Error::Storage(format!(
                    "rebalance batch `{batch_id}` was not found"
                )));
            }
            Ok(())
        }

        async fn claim_intent(&self, intent_id: Uuid) -> Result<bool> {
            // Условный UPDATE является межпроцессным compare-and-swap: ровно
            // один executor увидит rows_affected == 1 и получит право отправки.
            let changed = sqlx::query(
                "UPDATE trade_intents SET status = 'submitting', updated_at = now() \
                 WHERE id = $1 AND runtime_version = 2 AND status = 'pending'",
            )
            .bind(intent_id)
            .execute(&self.pool)
            .await?
            .rows_affected();
            Ok(changed == 1)
        }

        async fn has_pending_order(
            &self,
            account_id: &AccountId,
            instrument_id: &InstrumentId,
        ) -> Result<bool> {
            let row = sqlx::query(
                "SELECT EXISTS(SELECT 1 FROM trade_intents WHERE runtime_version = 2 \
                 AND account_id = $1 AND instrument_id = $2 \
                 AND status IN ('pending', 'submitting', 'accepted', 'uncertain')) AS exists",
            )
            .bind(account_id.as_str())
            .bind(instrument_id.as_str())
            .fetch_one(&self.pool)
            .await?;
            Ok(row.try_get("exists")?)
        }

        async fn has_pending_orders(&self, account_id: &AccountId) -> Result<bool> {
            let row = sqlx::query(
                "SELECT EXISTS(SELECT 1 FROM trade_intents WHERE runtime_version = 2 \
                 AND account_id = $1 AND status IN ('pending', 'submitting', 'accepted', 'uncertain')) AS exists",
            )
            .bind(account_id.as_str())
            .fetch_one(&self.pool)
            .await?;
            Ok(row.try_get("exists")?)
        }

        async fn reject_intent(&self, intent_id: Uuid, reason: &str) -> Result<()> {
            set_intent_status(&self.pool, intent_id, "rejected", Some(reason)).await
        }

        async fn mark_uncertain(&self, intent_id: Uuid, reason: &str) -> Result<()> {
            set_intent_status(&self.pool, intent_id, "uncertain", Some(reason)).await
        }

        async fn record_order_update(&self, intent_id: Uuid, update: &OrderUpdate) -> Result<()> {
            // Broker view (`orders`) и локальный статус intent изменяются одной
            // транзакцией, чтобы recovery не увидел противоречивую пару.
            let mut transaction = self.pool.begin().await?;
            sqlx::query(
                "INSERT INTO orders (request_id, intent_id, exchange_order_id, status, \
                 lots_requested, lots_executed, observed_at) VALUES ($1,$2,$3,$4,$5,$6,$7) \
                 ON CONFLICT (request_id) DO UPDATE SET exchange_order_id=EXCLUDED.exchange_order_id, \
                 status=EXCLUDED.status, lots_requested=EXCLUDED.lots_requested, \
                 lots_executed=EXCLUDED.lots_executed, observed_at=EXCLUDED.observed_at",
            )
            .bind(update.request_id.as_uuid())
            .bind(intent_id)
            .bind(&update.exchange_order_id)
            .bind(order_status(update.status))
            .bind(update.lots_requested)
            .bind(update.lots_executed)
            .bind(update.observed_at)
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "UPDATE trade_intents SET status=$2, updated_at=now() WHERE id=$1 AND runtime_version=2",
            )
            .bind(intent_id)
            .bind(status_from_order(update.status).as_str())
            .execute(&mut *transaction)
            .await?;
            transaction.commit().await?;
            Ok(())
        }

        async fn store_candle(&self, candle: &Candle) -> Result<()> {
            sqlx::query(
                "INSERT INTO candles (instrument_id, interval, open_time, open, high, low, close, volume, complete) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT (instrument_id, interval, open_time) \
                 DO UPDATE SET open=EXCLUDED.open, high=EXCLUDED.high, low=EXCLUDED.low, \
                 close=EXCLUDED.close, volume=EXCLUDED.volume, complete=EXCLUDED.complete",
            )
            .bind(candle.instrument_id.as_str())
            .bind(candle_interval(candle.interval))
            .bind(candle.time)
            .bind(candle.open.0)
            .bind(candle.high.0)
            .bind(candle.low.0)
            .bind(candle.close.0)
            .bind(candle.volume)
            .bind(candle.complete)
            .execute(&self.pool)
            .await?;
            Ok(())
        }

        async fn record_fill(&self, fill: &ExecutionFill) -> Result<()> {
            sqlx::query(
                "INSERT INTO fills (trade_id, request_id, price, lots, executed_at) \
                 VALUES ($1,$2,$3,$4,$5) ON CONFLICT (trade_id) DO NOTHING",
            )
            .bind(&fill.trade_id)
            .bind(fill.request_id.as_uuid())
            .bind(fill.price.0)
            .bind(fill.lots)
            .bind(fill.executed_at)
            .execute(&self.pool)
            .await?;
            Ok(())
        }
    }

    async fn insert_batch(
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        batch: &RebalanceBatch,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO rebalance_batches (id,event_id,strategy_id,account_id,target,effective_risk, \
             sizing_capital,status,created_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
        )
        .bind(batch.id)
        .bind(&batch.event_id)
        .bind(batch.strategy_id.as_str())
        .bind(batch.account_id.as_str())
        .bind(encode_json(&batch.target, "target portfolio")?)
        .bind(encode_json(&batch.effective_risk, "effective risk config")?)
        .bind(batch.sizing_capital)
        .bind(batch.status.as_str())
        .bind(batch.created_at)
        .execute(&mut **transaction)
        .await?;
        Ok(())
    }

    async fn insert_intent(
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        intent: &OrderIntent,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO trade_intents (id,request_id,batch_id,leg_index,event_id,strategy_id, \
             account_id,instrument_id,side,kind,lots,target_lots,lot_size,execution_phase,margin_consent, \
             limit_price,status,created_at,runtime_version) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,'pending',$17,2)",
        )
        .bind(intent.id)
        .bind(intent.request_id.as_uuid())
        .bind(intent.batch_id)
        .bind(intent.leg_index)
        .bind(&intent.event_id)
        .bind(intent.strategy_id.as_str())
        .bind(intent.account_id.as_str())
        .bind(intent.instrument_id.as_str())
        .bind(side(intent.side))
        .bind(kind(intent.kind))
        .bind(intent.lots)
        .bind(intent.target_lots)
        .bind(intent.lot_size)
        .bind(execution_phase(intent.execution_phase))
        .bind(margin_consent(intent.margin_consent))
        .bind(intent.limit_price.map(|price| price.0))
        .bind(intent.created_at)
        .execute(&mut **transaction)
        .await?;
        Ok(())
    }

    async fn upsert_risk_state(
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        state: &AccountRiskState,
    ) -> Result<()> {
        let changed = sqlx::query(
            "INSERT INTO account_risk_state (account_id,strategy_id,epoch,state,runtime_version,updated_at) \
             VALUES ($1,$2,$3,$4,2,$5) ON CONFLICT (account_id) DO UPDATE SET \
             strategy_id=EXCLUDED.strategy_id, epoch=EXCLUDED.epoch, state=EXCLUDED.state, \
             runtime_version=2, updated_at=EXCLUDED.updated_at \
             WHERE account_risk_state.runtime_version=2 \
             AND account_risk_state.strategy_id=EXCLUDED.strategy_id",
        )
        .bind(state.account_id.as_str())
        .bind(state.strategy_id.as_str())
        .bind(state.epoch)
        .bind(encode_json(state, "account risk state")?)
        .bind(state.updated_at)
        .execute(&mut **transaction)
        .await?
        .rows_affected();
        if changed != 1 {
            return Err(Error::Storage(format!(
                "account `{}` is already owned by another runtime strategy",
                state.account_id
            )));
        }
        Ok(())
    }

    fn row_to_batch(row: &sqlx::postgres::PgRow) -> Result<RebalanceBatch> {
        let target: TargetPortfolio = decode_json(row.try_get("target")?, "target portfolio")?;
        let effective_risk: EffectiveRiskConfig =
            decode_json(row.try_get("effective_risk")?, "effective risk config")?;
        Ok(RebalanceBatch {
            id: row.try_get("id")?,
            event_id: row.try_get("event_id")?,
            strategy_id: StrategyId::new(row.try_get::<String, _>("strategy_id")?)?,
            account_id: AccountId::new(row.try_get::<String, _>("account_id")?)?,
            target,
            effective_risk,
            sizing_capital: row.try_get("sizing_capital")?,
            status: batch_status(&row.try_get::<String, _>("status")?)?,
            created_at: row.try_get("created_at")?,
        })
    }

    fn row_to_stored_intent(row: &sqlx::postgres::PgRow) -> Result<StoredIntent> {
        let status: String = row.try_get("status")?;
        Ok(StoredIntent {
            intent: row_to_intent(row)?,
            status: intent_status(&status)?,
        })
    }

    fn row_to_intent(row: &sqlx::postgres::PgRow) -> Result<OrderIntent> {
        let side_value: String = row.try_get("side")?;
        let kind_value: String = row.try_get("kind")?;
        let phase: String = row.try_get("execution_phase")?;
        let consent: String = row.try_get("margin_consent")?;
        let limit_price: Option<Decimal> = row.try_get("limit_price")?;
        Ok(OrderIntent {
            id: row.try_get("id")?,
            request_id: OrderRequestId::from_uuid(row.try_get("request_id")?),
            batch_id: row.try_get("batch_id")?,
            leg_index: row.try_get("leg_index")?,
            event_id: row.try_get("event_id")?,
            strategy_id: StrategyId::new(row.try_get::<String, _>("strategy_id")?)?,
            account_id: AccountId::new(row.try_get::<String, _>("account_id")?)?,
            instrument_id: InstrumentId::new(row.try_get::<String, _>("instrument_id")?)?,
            side: match side_value.as_str() {
                "buy" => OrderSide::Buy,
                "sell" => OrderSide::Sell,
                _ => return Err(Error::Storage(format!("unknown order side `{side_value}`"))),
            },
            kind: match kind_value.as_str() {
                "market" => OrderKind::Market,
                "limit" => OrderKind::Limit,
                "best_price" => OrderKind::BestPrice,
                _ => return Err(Error::Storage(format!("unknown order kind `{kind_value}`"))),
            },
            lots: row.try_get("lots")?,
            target_lots: row.try_get("target_lots")?,
            lot_size: row.try_get("lot_size")?,
            execution_phase: match phase.as_str() {
                "reduction" => ExecutionPhase::Reduction,
                "opening" => ExecutionPhase::Opening,
                _ => return Err(Error::Storage(format!("unknown execution phase `{phase}`"))),
            },
            margin_consent: match consent.as_str() {
                "covered_only" => MarginConsent::CoveredOnly,
                "uncovered_allowed" => MarginConsent::UncoveredAllowed,
                _ => {
                    return Err(Error::Storage(format!(
                        "unknown margin consent `{consent}`"
                    )));
                }
            },
            limit_price: limit_price.map(Price::new),
            created_at: row.try_get("created_at")?,
        })
    }

    async fn set_intent_status(
        pool: &PgPool,
        intent_id: Uuid,
        status: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE trade_intents SET status=$2, reason=$3, updated_at=now() \
             WHERE id=$1 AND runtime_version=2",
        )
        .bind(intent_id)
        .bind(status)
        .bind(reason)
        .execute(pool)
        .await?;
        Ok(())
    }

    fn encode_json<T: serde::Serialize>(value: &T, what: &str) -> Result<Value> {
        serde_json::to_value(value)
            .map_err(|error| Error::Storage(format!("cannot encode {what}: {error}")))
    }

    fn decode_json<T: serde::de::DeserializeOwned>(value: Value, what: &str) -> Result<T> {
        serde_json::from_value(value)
            .map_err(|error| Error::Storage(format!("cannot decode {what}: {error}")))
    }

    const fn side(value: OrderSide) -> &'static str {
        match value {
            OrderSide::Buy => "buy",
            OrderSide::Sell => "sell",
        }
    }

    const fn kind(value: OrderKind) -> &'static str {
        match value {
            OrderKind::Market => "market",
            OrderKind::Limit => "limit",
            OrderKind::BestPrice => "best_price",
        }
    }

    const fn execution_phase(value: ExecutionPhase) -> &'static str {
        match value {
            ExecutionPhase::Reduction => "reduction",
            ExecutionPhase::Opening => "opening",
        }
    }

    const fn margin_consent(value: MarginConsent) -> &'static str {
        match value {
            MarginConsent::CoveredOnly => "covered_only",
            MarginConsent::UncoveredAllowed => "uncovered_allowed",
        }
    }

    const fn order_status(value: OrderStatus) -> &'static str {
        match value {
            OrderStatus::Pending => "pending",
            OrderStatus::New => "new",
            OrderStatus::PartiallyFilled => "partially_filled",
            OrderStatus::Filled => "filled",
            OrderStatus::Cancelled => "cancelled",
            OrderStatus::Rejected => "rejected",
            OrderStatus::Unknown => "unknown",
        }
    }

    fn intent_status(value: &str) -> Result<IntentStatus> {
        match value {
            "pending" => Ok(IntentStatus::Pending),
            "submitting" => Ok(IntentStatus::Submitting),
            "accepted" => Ok(IntentStatus::Accepted),
            "filled" => Ok(IntentStatus::Filled),
            "cancelled" => Ok(IntentStatus::Cancelled),
            "rejected" => Ok(IntentStatus::Rejected),
            "uncertain" => Ok(IntentStatus::Uncertain),
            _ => Err(Error::Storage(format!("unknown intent status `{value}`"))),
        }
    }

    fn batch_status(value: &str) -> Result<RebalanceBatchStatus> {
        match value {
            "planned" => Ok(RebalanceBatchStatus::Planned),
            "reducing" => Ok(RebalanceBatchStatus::Reducing),
            "opening" => Ok(RebalanceBatchStatus::Opening),
            "awaiting_fills" => Ok(RebalanceBatchStatus::AwaitingFills),
            "completed" => Ok(RebalanceBatchStatus::Completed),
            "failed" => Ok(RebalanceBatchStatus::Failed),
            _ => Err(Error::Storage(format!("unknown batch status `{value}`"))),
        }
    }

    const fn candle_interval(value: CandleInterval) -> &'static str {
        match value {
            CandleInterval::OneMinute => "1m",
            CandleInterval::FiveMinutes => "5m",
            CandleInterval::FifteenMinutes => "15m",
            CandleInterval::OneHour => "1h",
            CandleInterval::OneDay => "1d",
            CandleInterval::TwoMinutes => "2m",
            CandleInterval::ThreeMinutes => "3m",
            CandleInterval::TenMinutes => "10m",
            CandleInterval::ThirtyMinutes => "30m",
            CandleInterval::TwoHours => "2h",
            CandleInterval::FourHours => "4h",
            CandleInterval::OneWeek => "1w",
            CandleInterval::OneMonth => "1mo",
            CandleInterval::FiveSeconds => "5s",
            CandleInterval::TenSeconds => "10s",
            CandleInterval::ThirtySeconds => "30s",
        }
    }
}

#[cfg(feature = "postgres")]
pub use postgres::{PostgresStorage, RuntimeLease};
