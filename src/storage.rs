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
    engine::{AccountRiskState, PORTFOLIO_RUNTIME_VERSION, RebalanceBatch, RebalanceBatchStatus},
};

/// Durable-состояние одной ноги между планированием и исполнением.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BatchExecutionGate {
    /// The current controller permits execution.
    #[default]
    Ready,
    /// A durable plan awaits operator approval; no leg may be submitted.
    Hold,
    /// Reconcile existing requests, but never submit or resubmit any leg.
    Cancel,
}

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

    /// Терминальный intent больше никогда не должен возвращаться в состояние,
    /// из которого executor способен повторно отправить заявку.
    const fn is_terminal(self) -> bool {
        matches!(self, Self::Filled | Self::Cancelled | Self::Rejected)
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
    /// Повторная регистрация допустима только при точном совпадении владельца,
    /// типа и immutable config; любое изменение требует нового strategy ID.
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

    /// Atomically commits a batch of warmup markers and the final strategy state.
    /// No targets, intents or risk changes can be included. A duplicate rolls back the whole batch.
    async fn commit_warmup(
        &self,
        _strategy_id: &StrategyId,
        _event_ids: &[String],
        _state: &Value,
    ) -> Result<bool> {
        Err(Error::UnsupportedCapability("batched warmup persistence"))
    }

    /// Загружает durable risk-state выделенного счёта.
    async fn load_risk_state(&self, account_id: &AccountId) -> Result<Option<AccountRiskState>>;
    /// Сохраняет risk-state отдельно после периодической переоценки.
    /// Обычный save не меняет owner или epoch; reset использует отдельный CAS.
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
    /// Проверяет все незавершённые batch-и счёта независимо от прежнего owner.
    ///
    /// Account-wide проверка нужна перед передачей risk epoch другому
    /// `strategy_id`: исторический batch старого владельца нельзя потерять только
    /// потому, что новый runtime фильтрует recovery по собственному ID.
    async fn has_unfinished_batches(&self, account_id: &AccountId) -> Result<bool>;
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

    /// Latest durable operator decision, checked on recovery as well as submission.
    async fn batch_execution_gate(&self, _batch_id: Uuid) -> Result<BatchExecutionGate> {
        Ok(BatchExecutionGate::Ready)
    }
    /// Проверяет незавершённую заявку по счёту и инструменту.
    async fn has_pending_order(
        &self,
        account_id: &AccountId,
        instrument_id: &InstrumentId,
    ) -> Result<bool>;
    /// Проверяет наличие любой незавершённой/неопределённой заявки счёта.
    async fn has_pending_orders(&self, account_id: &AccountId) -> Result<bool>;
    /// Атомарно отклоняет только ещё не захваченную ногу `Pending → Rejected`.
    ///
    /// Возвращает `false`, если другой worker уже начал отправку либо intent не
    /// существует. Этот CAS используется при подавлении просроченных openings.
    async fn reject_pending_intent(&self, intent_id: Uuid, reason: &str) -> Result<bool>;
    /// Терминально отклоняет intent с аудируемой причиной.
    /// Уже терминальный intent остаётся неизменным.
    async fn reject_intent(&self, intent_id: Uuid, reason: &str) -> Result<()>;
    /// Помечает исход заявки неопределённым до reconciliation.
    /// Уже терминальный intent остаётся неизменным.
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
    batch_gates: HashMap<Uuid, BatchExecutionGate>,
}

impl MemoryStorage {
    /// Set a control decision for deterministic execution tests and embedded users.
    pub async fn set_batch_execution_gate(&self, id: Uuid, gate: BatchExecutionGate) {
        self.inner.lock().await.batch_gates.insert(id, gate);
    }
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
            Some((_, _, stored_config)) if stored_config != config => {
                return Err(Error::Storage(format!(
                    "strategy `{strategy_id}` configuration changed; use a new strategy_id"
                )));
            }
            Some((_, _, _)) => {}
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
        if let Some(risk) = risk_state {
            validate_risk_state_version(risk)?;
            if let Some(existing) = inner.risk_states.get(&risk.account_id) {
                validate_matching_risk_epoch(existing, risk)?;
            }
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
        if let Some(batch) = batch {
            // Один account/strategy может иметь лишь один незавершённый batch.
            // Иначе два полных target-а начнут спорить за одни и те же позиции.
            let unfinished_exists = inner.batches.values().any(|(stored, _)| {
                stored.account_id == batch.account_id
                    && !matches!(
                        stored.status,
                        RebalanceBatchStatus::Completed | RebalanceBatchStatus::Failed
                    )
            });
            if unfinished_exists {
                return Err(Error::Storage(format!(
                    "account `{}` already has an unfinished rebalance batch",
                    batch.account_id
                )));
            }
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

    async fn commit_warmup(
        &self,
        strategy_id: &StrategyId,
        event_ids: &[String],
        state: &Value,
    ) -> Result<bool> {
        let mut inner = self.inner.lock().await;
        if !inner.strategies.contains_key(strategy_id) {
            return Err(Error::Storage(
                "warmup strategy is not registered".to_owned(),
            ));
        }
        let keys = event_ids
            .iter()
            .map(|id| (strategy_id.clone(), id.clone()))
            .collect::<HashSet<_>>();
        if keys.len() != event_ids.len()
            || keys.iter().any(|key| inner.processed_events.contains(key))
        {
            return Ok(false);
        }
        inner.processed_events.extend(keys);
        inner
            .strategy_states
            .insert(strategy_id.clone(), state.clone());
        Ok(true)
    }

    async fn load_risk_state(&self, account_id: &AccountId) -> Result<Option<AccountRiskState>> {
        Ok(self.inner.lock().await.risk_states.get(account_id).cloned())
    }

    async fn save_risk_state(&self, state: &AccountRiskState) -> Result<()> {
        validate_risk_state_version(state)?;
        let mut inner = self.inner.lock().await;
        if let Some(existing) = inner.risk_states.get(&state.account_id) {
            validate_matching_risk_epoch(existing, state)?;
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

    async fn has_unfinished_batches(&self, account_id: &AccountId) -> Result<bool> {
        Ok(self.inner.lock().await.batches.values().any(|(batch, _)| {
            &batch.account_id == account_id
                && !matches!(
                    batch.status,
                    RebalanceBatchStatus::Completed | RebalanceBatchStatus::Failed
                )
        }))
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
        // Поздний worker не имеет права вернуть завершённый batch в recoverable
        // состояние. Повтор того же terminal результата идемпотентен, а попытка
        // объявить Failed как Completed (или наоборот) является конфликтом.
        if matches!(
            batch.status,
            RebalanceBatchStatus::Completed | RebalanceBatchStatus::Failed
        ) {
            return if batch.status == status {
                Ok(())
            } else {
                Err(Error::Storage(format!(
                    "rebalance batch `{batch_id}` is already terminal as `{}` and cannot become `{}`",
                    batch.status.as_str(),
                    status.as_str()
                )))
            };
        }
        batch.status = status;
        *stored_reason = reason.map(str::to_owned);
        Ok(())
    }

    async fn claim_intent(&self, intent_id: Uuid) -> Result<bool> {
        let mut inner = self.inner.lock().await;
        if let Some((intent, _, _)) = inner.intents.get(&intent_id) {
            if inner
                .batch_gates
                .get(&intent.batch_id)
                .copied()
                .unwrap_or_default()
                != BatchExecutionGate::Ready
            {
                return Ok(false);
            }
        }
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

    async fn batch_execution_gate(&self, batch_id: Uuid) -> Result<BatchExecutionGate> {
        Ok(self
            .inner
            .lock()
            .await
            .batch_gates
            .get(&batch_id)
            .copied()
            .unwrap_or_default())
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

    async fn reject_pending_intent(&self, intent_id: Uuid, reason: &str) -> Result<bool> {
        let mut inner = self.inner.lock().await;
        let Some((_, status, stored_reason)) = inner.intents.get_mut(&intent_id) else {
            return Ok(false);
        };
        // Тот же mutex делает проверку и переход единым CAS: после
        // Pending→Submitting этот метод уже не имеет права отменять ногу.
        if *status != IntentStatus::Pending {
            return Ok(false);
        }
        *status = IntentStatus::Rejected;
        *stored_reason = Some(reason.to_owned());
        Ok(true)
    }

    async fn reject_intent(&self, intent_id: Uuid, reason: &str) -> Result<()> {
        let mut inner = self.inner.lock().await;
        let Some((_, status, stored_reason)) = inner.intents.get_mut(&intent_id) else {
            return Err(Error::Storage(format!(
                "intent `{intent_id}` was not found"
            )));
        };
        // Первый терминальный исход неизменяем: поздняя локальная ошибка не может
        // превратить подтверждённый Filled/Cancelled в Rejected.
        if !status.is_terminal() {
            *status = IntentStatus::Rejected;
            *stored_reason = Some(reason.to_owned());
        }
        Ok(())
    }

    async fn mark_uncertain(&self, intent_id: Uuid, reason: &str) -> Result<()> {
        let mut inner = self.inner.lock().await;
        let Some((_, status, stored_reason)) = inner.intents.get_mut(&intent_id) else {
            return Err(Error::Storage(format!(
                "intent `{intent_id}` was not found"
            )));
        };
        // Timeout имеет право уточнить только незавершённую заявку. Терминальное
        // подтверждение сильнее более поздней сетевой неопределённости.
        if !status.is_terminal() {
            *status = IntentStatus::Uncertain;
            *stored_reason = Some(reason.to_owned());
        }
        Ok(())
    }

    async fn record_order_update(&self, intent_id: Uuid, update: &OrderUpdate) -> Result<()> {
        let mut inner = self.inner.lock().await;
        let Some((intent, current_status, _)) = inner.intents.get(&intent_id) else {
            return Err(Error::Storage(format!(
                "intent `{intent_id}` was not found"
            )));
        };
        validate_order_update_identity(intent.request_id.as_uuid(), intent.lots, update)?;
        let current_status = *current_status;
        let existing = inner.orders.get(&update.request_id.as_uuid());
        if !should_apply_order_update(current_status, existing, update)? {
            return Ok(());
        }
        let accepted = merge_order_update(existing, update);
        inner.orders.insert(update.request_id.as_uuid(), accepted);
        let (_, stored_status, stored_reason) = inner
            .intents
            .get_mut(&intent_id)
            .expect("intent cannot disappear while MemoryStorage mutex is held");
        *stored_status = status_from_order(update.status);
        // Свежий broker snapshot заменяет прежнюю локальную timeout/error-причину.
        *stored_reason = None;
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

fn validate_risk_state_version(state: &AccountRiskState) -> Result<()> {
    if state.runtime_version != PORTFOLIO_RUNTIME_VERSION {
        return Err(Error::Storage(format!(
            "cannot save account risk state with runtime version {}",
            state.runtime_version
        )));
    }
    Ok(())
}

fn validate_matching_risk_epoch(
    existing: &AccountRiskState,
    replacement: &AccountRiskState,
) -> Result<()> {
    if existing.account_id != replacement.account_id
        || existing.strategy_id != replacement.strategy_id
        || existing.epoch != replacement.epoch
    {
        return Err(Error::Storage(format!(
            "ordinary risk-state save for account `{}` cannot change owner or epoch (stored strategy `{}`, epoch {}; received strategy `{}`, epoch {})",
            replacement.account_id,
            existing.strategy_id,
            existing.epoch,
            replacement.strategy_id,
            replacement.epoch
        )));
    }
    Ok(())
}

fn validate_order_update_identity(
    expected_request_id: Uuid,
    expected_lots: i64,
    update: &OrderUpdate,
) -> Result<()> {
    if update.request_id.as_uuid() != expected_request_id {
        return Err(Error::Storage(format!(
            "broker update request_id `{}` does not match intent request_id `{expected_request_id}`",
            update.request_id
        )));
    }
    if update.lots_requested != expected_lots
        || update.lots_executed < 0
        || update.lots_executed > update.lots_requested
        || (update.status == OrderStatus::Filled && update.lots_executed != update.lots_requested)
    {
        return Err(Error::Storage(format!(
            "broker update for request `{}` has inconsistent lots: intent={}, requested={}, executed={}",
            update.request_id, expected_lots, update.lots_requested, update.lots_executed
        )));
    }
    Ok(())
}

/// Определяет, несёт ли broker update монотонно более свежую информацию.
///
/// Сравниваются три независимых признака: время наблюдения, число исполненных
/// лотов и стадия broker status. Регресс хотя бы одного признака отбрасывает весь
/// update, чтобы строки `orders` и `trade_intents` оставались согласованы.
fn should_apply_order_update(
    current_intent_status: IntentStatus,
    existing: Option<&OrderUpdate>,
    incoming: &OrderUpdate,
) -> Result<bool> {
    let incoming_intent_status = status_from_order(incoming.status);
    // Terminal→active запрещён. Из terminal-состояний допустимо только более
    // сильное подтверждение полного fill: Cancelled/Rejected→Filled. Повтор того
    // же terminal может уточнить exchange_order_id и время наблюдения.
    if current_intent_status.is_terminal()
        && (!incoming_intent_status.is_terminal()
            || (current_intent_status != incoming_intent_status
                && intent_status_progress(incoming_intent_status)
                    <= intent_status_progress(current_intent_status)))
    {
        return Ok(false);
    }
    let Some(existing) = existing else {
        return Ok(true);
    };
    if let (Some(stored), Some(received)) = (
        existing.exchange_order_id.as_deref(),
        incoming.exchange_order_id.as_deref(),
    ) {
        if stored != received {
            return Err(Error::Storage(format!(
                "broker update for request `{}` changed exchange_order_id from `{stored}` to `{received}`",
                incoming.request_id
            )));
        }
    }
    if existing.lots_requested != incoming.lots_requested {
        return Err(Error::Storage(format!(
            "broker update for request `{}` changed lots_requested from {} to {}",
            incoming.request_id, existing.lots_requested, incoming.lots_requested
        )));
    }
    Ok(existing.observed_at <= incoming.observed_at
        && existing.lots_executed <= incoming.lots_executed
        && order_status_progress(existing.status) <= order_status_progress(incoming.status))
}

/// Сохраняет уже известный exchange order ID, если более свежий ответ его опустил.
fn merge_order_update(existing: Option<&OrderUpdate>, incoming: &OrderUpdate) -> OrderUpdate {
    let mut accepted = incoming.clone();
    if accepted.exchange_order_id.is_none() {
        accepted.exchange_order_id = existing.and_then(|value| value.exchange_order_id.clone());
    }
    accepted
}

/// Частичный порядок broker status: следующий update не может двигаться назад.
const fn order_status_progress(status: OrderStatus) -> u8 {
    match status {
        OrderStatus::Unknown => 0,
        OrderStatus::Pending => 1,
        OrderStatus::New => 2,
        OrderStatus::PartiallyFilled => 3,
        OrderStatus::Cancelled | OrderStatus::Rejected => 4,
        OrderStatus::Filled => 5,
    }
}

const fn intent_status_progress(status: IntentStatus) -> u8 {
    match status {
        IntentStatus::Pending => 0,
        IntentStatus::Submitting => 1,
        IntentStatus::Accepted | IntentStatus::Uncertain => 2,
        IntentStatus::Cancelled | IntentStatus::Rejected => 3,
        IntentStatus::Filled => 4,
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

    use super::{
        BatchExecutionGate, IntentStatus, Storage, StoredIntent, merge_order_update,
        should_apply_order_update, status_from_order, validate_atomic_payload,
        validate_order_update_identity, validate_risk_state_version,
    };
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

    const BATCH_GATE_SQL: &str = "SELECT CASE \
        WHEN b.event_id LIKE 'cleanup:%' OR b.event_id LIKE 'risk:%' OR p.revision=-1 THEN 'ready' \
        WHEN p.status='rejected' THEN 'cancel' \
        WHEN b.event_id LIKE 'manual:%' THEN 'ready' \
        WHEN c.enabled=false THEN 'cancel' \
        WHEN p.revision IS NOT NULL AND p.revision<>c.revision THEN 'cancel' \
        WHEN (p.status='awaiting' OR p.status='approved') AND (b.target->>'valid_until')::timestamptz <= now() THEN 'cancel' \
        WHEN p.status='awaiting' OR (c.execution_mode='confirm' AND p.status IS DISTINCT FROM 'approved') THEN 'hold' \
        ELSE 'ready' END AS gate FROM rebalance_batches b \
        LEFT JOIN strategy_controls c ON c.strategy_id=b.strategy_id \
        LEFT JOIN batch_controls p ON p.batch_id=b.id WHERE b.id=$1";

    fn decode_gate(value: &str) -> BatchExecutionGate {
        match value {
            "ready" => BatchExecutionGate::Ready,
            "hold" => BatchExecutionGate::Hold,
            _ => BatchExecutionGate::Cancel,
        }
    }

    /// Namespace session advisory lock-а, отделяющий runtime lease от
    /// возможных advisory lock-ов других подсистем в той же БД.
    const RUNTIME_LEASE_NAMESPACE: &[u8] = b"tinkoff-rs:portfolio-runtime:v2:account:";

    /// Эксклюзивная session lease одного Sandbox-счёта.
    ///
    /// Объект намеренно удерживает отсоединённый от пула `PgConnection`.
    /// `PostgreSQL` освобождает session advisory lock автоматически при
    /// закрытии этого соединения, поэтому lease действует ровно до `Drop`.
    #[must_use = "dropping RuntimeLease releases exclusive ownership of the account"]
    pub struct RuntimeLease {
        account_id: AccountId,
        connection: PgConnection,
    }

    impl RuntimeLease {
        /// Возвращает счёт, эксклюзивно закреплённый за этой lease.
        #[must_use]
        pub const fn account_id(&self) -> &AccountId {
            &self.account_id
        }

        /// Проверяет, что session с advisory lock всё ещё жива.
        ///
        /// `RuntimeLease` держит отдельное соединение, которое не участвует в
        /// запросах пула. Без heartbeat серверный `idle_session_timeout` или
        /// сетевой разрыв мог бы освободить lock незаметно для торгового цикла.
        /// После ошибки вызывающий runtime обязан прекратить выставление заявок.
        pub async fn check(&mut self) -> Result<()> {
            let heartbeat: i32 = sqlx::query_scalar("SELECT 1")
                .fetch_one(&mut self.connection)
                .await
                .map_err(|error| {
                    Error::LeaseLost(format!(
                        "runtime lease heartbeat for account `{}` failed; exclusive ownership is no longer proven: {error}",
                        self.account_id
                    ))
                })?;
            if heartbeat != 1 {
                return Err(Error::LeaseLost(format!(
                    "runtime lease heartbeat for account `{}` returned an invalid value; exclusive ownership is no longer proven",
                    self.account_id
                )));
            }
            Ok(())
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
                connection,
            })
        }

        /// Атомарно заменяет risk epoch после уже выполненных `risk-reset`
        /// проверок пустого счёта и отсутствия заявок.
        ///
        /// В отличие от обычного [`Storage::save_risk_state`] этот метод может
        /// передать счёт новому `strategy_id`. CAS по прежним owner и epoch не
        /// позволяет перезаписать состояние, изменившееся между чтением и reset.
        pub async fn replace_risk_state_for_reset(
            &self,
            expected_strategy_id: &StrategyId,
            expected_epoch: Uuid,
            replacement: &AccountRiskState,
        ) -> Result<()> {
            if replacement.runtime_version != PORTFOLIO_RUNTIME_VERSION {
                return Err(Error::Storage(format!(
                    "cannot save risk reset with runtime version {}",
                    replacement.runtime_version
                )));
            }
            // FK проверяет лишь существование strategy_id. Дополнительная
            // account-проверка не позволяет передать epoch стратегии, которая
            // была зарегистрирована для другого выделенного счёта.
            let replacement_owner: Option<String> = sqlx::query_scalar(
                "SELECT account_id FROM strategy_instances WHERE id=$1 AND runtime_version=2",
            )
            .bind(replacement.strategy_id.as_str())
            .fetch_optional(&self.pool)
            .await?;
            if replacement_owner.as_deref() != Some(replacement.account_id.as_str()) {
                return Err(Error::Storage(format!(
                    "risk reset strategy `{}` is not registered for account `{}`",
                    replacement.strategy_id, replacement.account_id
                )));
            }
            let changed = sqlx::query(
                "UPDATE account_risk_state SET strategy_id=$4, epoch=$5, state=$6, \
                 runtime_version=2, updated_at=$7 WHERE account_id=$1 \
                 AND strategy_id=$2 AND epoch=$3 AND runtime_version=2",
            )
            .bind(replacement.account_id.as_str())
            .bind(expected_strategy_id.as_str())
            .bind(expected_epoch)
            .bind(replacement.strategy_id.as_str())
            .bind(replacement.epoch)
            .bind(encode_json(replacement, "account risk state")?)
            .bind(replacement.updated_at)
            .execute(&self.pool)
            .await?
            .rows_affected();
            if changed != 1 {
                return Err(Error::Storage(format!(
                    "risk reset for account `{}` lost its owner/epoch compare-and-swap",
                    replacement.account_id
                )));
            }
            Ok(())
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
    /// `PostgreSQL` advisory lock. FNV-1a здесь не защищает секреты; он нужен
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
                 ON CONFLICT (id) DO UPDATE SET updated_at = now() \
                 WHERE strategy_instances.runtime_version = 2 \
                 AND strategy_instances.account_id = EXCLUDED.account_id \
                 AND strategy_instances.kind = EXCLUDED.kind \
                 AND strategy_instances.config = EXCLUDED.config",
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
                    "strategy `{strategy_id}` belongs to legacy runtime version one, another account/kind, or another immutable configuration; use a new strategy_id"
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
                if risk_state.is_some_and(|risk| risk.blocked.is_some()) {
                    sqlx::query("INSERT INTO batch_controls(batch_id,status,revision) VALUES($1,'approved',-1)")
                        .bind(batch.id).execute(&mut *transaction).await?;
                } else if !event_id.starts_with("manual:")
                    && !event_id.starts_with("cleanup:")
                    && !event_id.starts_with("risk:")
                {
                    sqlx::query(
                        "INSERT INTO batch_controls(batch_id,status,revision,preview) \
                        SELECT $1,'awaiting',revision,$3 FROM strategy_controls \
                        WHERE strategy_id=$2 AND execution_mode='confirm'",
                    )
                    .bind(batch.id)
                    .bind(strategy_id.as_str())
                    .bind(serde_json::json!({"target": batch.target, "intents": intents}))
                    .execute(&mut *transaction)
                    .await?;
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

        async fn commit_warmup(
            &self,
            strategy_id: &StrategyId,
            event_ids: &[String],
            state: &Value,
        ) -> Result<bool> {
            let mut transaction = self.pool.begin().await?;
            let inserted = sqlx::query(
                "INSERT INTO processed_events (strategy_id,event_id) SELECT $1, unnest($2::text[]) ON CONFLICT DO NOTHING"
            ).bind(strategy_id.as_str()).bind(event_ids).execute(&mut *transaction).await?.rows_affected();
            if inserted != u64::try_from(event_ids.len()).unwrap_or(u64::MAX) {
                transaction.rollback().await?;
                return Ok(false);
            }
            let updated = sqlx::query(
                "INSERT INTO strategy_state (strategy_id,state,version,runtime_version) VALUES ($1,$2,1,2) \
                 ON CONFLICT (strategy_id) DO UPDATE SET state=EXCLUDED.state,version=strategy_state.version+1, \
                 runtime_version=2,updated_at=now() WHERE strategy_state.runtime_version=2"
            ).bind(strategy_id.as_str()).bind(state).execute(&mut *transaction).await?.rows_affected();
            if updated != 1 {
                return Err(Error::Storage(
                    "warmup cannot overwrite legacy state".to_owned(),
                ));
            }
            transaction.commit().await?;
            Ok(true)
        }

        async fn load_risk_state(
            &self,
            account_id: &AccountId,
        ) -> Result<Option<AccountRiskState>> {
            let row = sqlx::query(
                "SELECT account_id, strategy_id, epoch, state, runtime_version \
                 FROM account_risk_state WHERE account_id = $1",
            )
            .bind(account_id.as_str())
            .fetch_optional(&self.pool)
            .await?;
            let Some(row) = row else {
                return Ok(None);
            };
            let runtime_version: i16 = row.try_get("runtime_version")?;
            if runtime_version != PORTFOLIO_RUNTIME_VERSION {
                return Err(Error::Storage(format!(
                    "risk state for account `{account_id}` uses unsupported runtime version {runtime_version}"
                )));
            }
            let state: AccountRiskState = decode_json(row.try_get("state")?, "account risk state")?;
            validate_risk_state_version(&state)?;
            let stored_account: String = row.try_get("account_id")?;
            let stored_strategy: String = row.try_get("strategy_id")?;
            let stored_epoch: Uuid = row.try_get("epoch")?;
            // Реляционные identity-поля служат индексируемым источником истины,
            // но JSON нужен для полного snapshot. Расхождение означает ручное
            // повреждение БД и должно остановить runtime, а не сменить владельца.
            if stored_account != state.account_id.as_str()
                || stored_account != account_id.as_str()
                || stored_strategy != state.strategy_id.as_str()
                || stored_epoch != state.epoch
            {
                return Err(Error::Storage(format!(
                    "risk state identity columns do not match JSON for account `{account_id}`"
                )));
            }
            Ok(Some(state))
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

        async fn has_unfinished_batches(&self, account_id: &AccountId) -> Result<bool> {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM rebalance_batches WHERE account_id = $1 \
                 AND status NOT IN ('completed', 'failed'))",
            )
            .bind(account_id.as_str())
            .fetch_one(&self.pool)
            .await?;
            Ok(exists)
        }

        async fn set_batch_status(
            &self,
            batch_id: Uuid,
            status: RebalanceBatchStatus,
            reason: Option<&str>,
        ) -> Result<()> {
            let changed = sqlx::query(
                "UPDATE rebalance_batches SET status = $2, reason = $3, updated_at = now() \
                 WHERE id = $1 AND status NOT IN ('completed', 'failed')",
            )
            .bind(batch_id)
            .bind(status.as_str())
            .bind(reason)
            .execute(&self.pool)
            .await?
            .rows_affected();
            if changed == 1 {
                return Ok(());
            }
            let current: Option<String> =
                sqlx::query_scalar("SELECT status FROM rebalance_batches WHERE id=$1")
                    .bind(batch_id)
                    .fetch_optional(&self.pool)
                    .await?;
            match current.as_deref() {
                Some(current) if current == status.as_str() => Ok(()),
                Some(current @ ("completed" | "failed")) => Err(Error::Storage(format!(
                    "rebalance batch `{batch_id}` is already terminal as `{current}` and cannot become `{}`",
                    status.as_str()
                ))),
                Some(current) => Err(Error::Storage(format!(
                    "rebalance batch `{batch_id}` could not transition from `{current}`"
                ))),
                None => Err(Error::Storage(format!(
                    "rebalance batch `{batch_id}` was not found"
                ))),
            }
        }

        async fn claim_intent(&self, intent_id: Uuid) -> Result<bool> {
            let mut transaction = self.pool.begin().await?;
            // The HTTP control transaction takes the same row lock before pause
            // or cancel. Whichever commits first determines the submission right.
            sqlx::query("SELECT c.strategy_id FROM strategy_controls c JOIN trade_intents i ON i.strategy_id=c.strategy_id WHERE i.id=$1 FOR UPDATE OF c")
                .bind(intent_id).fetch_optional(&mut *transaction).await?;
            let batch_id: Option<Uuid> =
                sqlx::query_scalar("SELECT batch_id FROM trade_intents WHERE id=$1")
                    .bind(intent_id)
                    .fetch_optional(&mut *transaction)
                    .await?
                    .flatten();
            let Some(batch_id) = batch_id else {
                return Ok(false);
            };
            let gate: String = sqlx::query_scalar(BATCH_GATE_SQL)
                .bind(batch_id)
                .fetch_one(&mut *transaction)
                .await?;
            if decode_gate(&gate) != BatchExecutionGate::Ready {
                return Ok(false);
            }
            let changed = sqlx::query(
                "UPDATE trade_intents SET status = 'submitting', updated_at = now() \
                 WHERE id = $1 AND runtime_version = 2 AND status = 'pending'",
            )
            .bind(intent_id)
            .execute(&mut *transaction)
            .await?
            .rows_affected();
            transaction.commit().await?;
            Ok(changed == 1)
        }

        async fn batch_execution_gate(&self, batch_id: Uuid) -> Result<BatchExecutionGate> {
            let gate: Option<String> = sqlx::query_scalar(BATCH_GATE_SQL)
                .bind(batch_id)
                .fetch_optional(&self.pool)
                .await?;
            Ok(gate
                .as_deref()
                .map_or(BatchExecutionGate::Cancel, decode_gate))
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

        async fn reject_pending_intent(&self, intent_id: Uuid, reason: &str) -> Result<bool> {
            // Условный UPDATE не позволяет отменить ногу после того, как другой
            // worker уже выполнил Pending→Submitting и получил право на сеть.
            let changed = sqlx::query(
                "UPDATE trade_intents SET status='rejected', reason=$2, updated_at=now() \
                 WHERE id=$1 AND runtime_version=2 AND status='pending'",
            )
            .bind(intent_id)
            .bind(reason)
            .execute(&self.pool)
            .await?
            .rows_affected();
            Ok(changed == 1)
        }

        async fn reject_intent(&self, intent_id: Uuid, reason: &str) -> Result<()> {
            transition_active_intent_status(&self.pool, intent_id, IntentStatus::Rejected, reason)
                .await
        }

        async fn mark_uncertain(&self, intent_id: Uuid, reason: &str) -> Result<()> {
            transition_active_intent_status(&self.pool, intent_id, IntentStatus::Uncertain, reason)
                .await
        }

        async fn record_order_update(&self, intent_id: Uuid, update: &OrderUpdate) -> Result<()> {
            // Broker view (`orders`) и локальный статус intent изменяются одной
            // транзакцией. FOR UPDATE сериализует конкурирующие poll/stream
            // ответы одной заявки и не даёт позднему stale-ответу откатить статус.
            let mut transaction = self.pool.begin().await?;
            let intent_row = sqlx::query(
                "SELECT request_id, status, lots FROM trade_intents \
                 WHERE id=$1 AND runtime_version=2 FOR UPDATE",
            )
            .bind(intent_id)
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or_else(|| Error::Storage(format!("intent `{intent_id}` was not found")))?;
            let expected_request_id: Uuid = intent_row.try_get("request_id")?;
            let expected_lots: i64 = intent_row.try_get("lots")?;
            let current_status_value: String = intent_row.try_get("status")?;
            let current_status = intent_status(&current_status_value)?;
            validate_order_update_identity(expected_request_id, expected_lots, update)?;

            // Обе UNIQUE-связи проверяются явно. OR может вернуть две строки только
            // при перекрёстно повреждённых request_id/intent_id — такую БД нельзя
            // молча "исправлять" очередным upsert-ом.
            let order_rows = sqlx::query(
                "SELECT request_id, intent_id, exchange_order_id, status, lots_requested, \
                 lots_executed, observed_at FROM orders \
                 WHERE request_id=$1 OR intent_id=$2 FOR UPDATE",
            )
            .bind(update.request_id.as_uuid())
            .bind(intent_id)
            .fetch_all(&mut *transaction)
            .await?;
            if order_rows.len() > 1 {
                return Err(Error::Storage(format!(
                    "request `{}` and intent `{intent_id}` refer to different order rows",
                    update.request_id
                )));
            }
            let existing = order_rows
                .first()
                .map(|row| {
                    let stored_intent_id: Uuid = row.try_get("intent_id")?;
                    let stored_request_id: Uuid = row.try_get("request_id")?;
                    if stored_intent_id != intent_id
                        || stored_request_id != update.request_id.as_uuid()
                    {
                        return Err(Error::Storage(format!(
                            "request `{}` is not linked one-to-one with intent `{intent_id}`",
                            update.request_id
                        )));
                    }
                    row_to_order_update(row)
                })
                .transpose()?;
            if !should_apply_order_update(current_status, existing.as_ref(), update)? {
                transaction.commit().await?;
                return Ok(());
            }
            let accepted = merge_order_update(existing.as_ref(), update);
            let order_changed = if existing.is_some() {
                sqlx::query(
                    "UPDATE orders SET exchange_order_id=$3, status=$4, lots_requested=$5, \
                     lots_executed=$6, observed_at=$7, updated_at=now() \
                     WHERE request_id=$1 AND intent_id=$2",
                )
                .bind(accepted.request_id.as_uuid())
                .bind(intent_id)
                .bind(&accepted.exchange_order_id)
                .bind(order_status(accepted.status))
                .bind(accepted.lots_requested)
                .bind(accepted.lots_executed)
                .bind(accepted.observed_at)
                .execute(&mut *transaction)
                .await?
                .rows_affected()
            } else {
                sqlx::query(
                    "INSERT INTO orders (request_id, intent_id, exchange_order_id, status, \
                     lots_requested, lots_executed, observed_at) VALUES ($1,$2,$3,$4,$5,$6,$7)",
                )
                .bind(accepted.request_id.as_uuid())
                .bind(intent_id)
                .bind(&accepted.exchange_order_id)
                .bind(order_status(accepted.status))
                .bind(accepted.lots_requested)
                .bind(accepted.lots_executed)
                .bind(accepted.observed_at)
                .execute(&mut *transaction)
                .await?
                .rows_affected()
            };
            if order_changed != 1 {
                return Err(Error::Storage(format!(
                    "broker order for request `{}` lost its compare-and-swap",
                    update.request_id
                )));
            }
            let intent_changed = sqlx::query(
                "UPDATE trade_intents SET status=$2, reason=NULL, updated_at=now() \
                 WHERE id=$1 AND runtime_version=2 AND request_id=$3 AND status=$4",
            )
            .bind(intent_id)
            .bind(status_from_order(accepted.status).as_str())
            .bind(accepted.request_id.as_uuid())
            .bind(&current_status_value)
            .execute(&mut *transaction)
            .await?
            .rows_affected();
            if intent_changed != 1 {
                return Err(Error::Storage(format!(
                    "intent `{intent_id}` lost its status compare-and-swap"
                )));
            }
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
        validate_risk_state_version(state)?;
        let changed = sqlx::query(
            "INSERT INTO account_risk_state (account_id,strategy_id,epoch,state,runtime_version,updated_at) \
             VALUES ($1,$2,$3,$4,2,$5) ON CONFLICT (account_id) DO UPDATE SET \
             state=EXCLUDED.state, updated_at=EXCLUDED.updated_at \
             WHERE account_risk_state.runtime_version=2 \
             AND account_risk_state.strategy_id=EXCLUDED.strategy_id \
             AND account_risk_state.epoch=EXCLUDED.epoch",
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
                "ordinary risk-state save for account `{}` cannot change owner or epoch",
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

    fn row_to_order_update(row: &sqlx::postgres::PgRow) -> Result<OrderUpdate> {
        let status: String = row.try_get("status")?;
        Ok(OrderUpdate {
            request_id: OrderRequestId::from_uuid(row.try_get("request_id")?),
            exchange_order_id: row.try_get("exchange_order_id")?,
            status: stored_order_status(&status)?,
            lots_requested: row.try_get("lots_requested")?,
            lots_executed: row.try_get("lots_executed")?,
            observed_at: row.try_get("observed_at")?,
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

    /// Меняет только активный intent. Условие в SQL является частью state
    /// machine: Filled/Cancelled/Rejected никогда не возвращаются в active state
    /// и не меняют уже зафиксированную terminal-причину.
    async fn transition_active_intent_status(
        pool: &PgPool,
        intent_id: Uuid,
        status: IntentStatus,
        reason: &str,
    ) -> Result<()> {
        let changed = sqlx::query(
            "UPDATE trade_intents SET status=$2, reason=$3, updated_at=now() \
             WHERE id=$1 AND runtime_version=2 \
             AND status IN ('pending', 'submitting', 'accepted', 'uncertain')",
        )
        .bind(intent_id)
        .bind(status.as_str())
        .bind(reason)
        .execute(pool)
        .await?
        .rows_affected();
        if changed == 1 {
            return Ok(());
        }
        let current: Option<String> = sqlx::query_scalar(
            "SELECT status FROM trade_intents WHERE id=$1 AND runtime_version=2",
        )
        .bind(intent_id)
        .fetch_optional(pool)
        .await?;
        let Some(current) = current else {
            return Err(Error::Storage(format!(
                "intent `{intent_id}` was not found"
            )));
        };
        if intent_status(&current)?.is_terminal() {
            return Ok(());
        }
        Err(Error::Storage(format!(
            "intent `{intent_id}` could not transition from `{current}` to `{}`",
            status.as_str()
        )))
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

    fn stored_order_status(value: &str) -> Result<OrderStatus> {
        match value {
            "pending" => Ok(OrderStatus::Pending),
            "new" => Ok(OrderStatus::New),
            "partially_filled" => Ok(OrderStatus::PartiallyFilled),
            "filled" => Ok(OrderStatus::Filled),
            "cancelled" => Ok(OrderStatus::Cancelled),
            "rejected" => Ok(OrderStatus::Rejected),
            "unknown" => Ok(OrderStatus::Unknown),
            _ => Err(Error::Storage(format!("unknown order status `{value}`"))),
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
