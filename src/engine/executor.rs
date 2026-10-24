//! Исполнение durable batch у брокера: отправка ног, reconciliation,
//! повтор неоднозначных ответов и завершение фаз.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tonic::Code;
use uuid::Uuid;

use super::RebalanceBatchStatus;
use crate::{
    Error, Result,
    broker::{Portfolio, Trading},
    domain::{
        AccountId, ExecutionPhase, OrderIntent, OrderSide, OrderStatus, OrderUpdate,
        PortfolioSnapshot, StrategyId,
    },
    storage::{BatchExecutionGate, IntentStatus, Storage, StoredIntent},
};

/// Решение проверки рынка перед первой отправкой ноги.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubmissionDecision {
    /// Отправлять как запланировано.
    Proceed,
    /// Не отправлять сейчас: нога остаётся `Pending`, следующий проход batch
    /// спросит снова. Причина попадает в статус batch.
    Defer(String),
    /// Не отправлять совсем: нога отклоняется с причиной.
    Skip(String),
}

/// Проверка рыночных условий (например, ширины спреда и глубины стакана)
/// непосредственно перед первой отправкой ноги.
///
/// Вызывается только для ещё не захваченной `Pending` ноги — до CAS и до
/// сетевого вызова, поэтому отложенная нога ничего не меняет в durable state.
/// Сверка уже отправленных заявок и их повторная отправка после `NotFound`
/// проверку не проходят: исход прежнего запроса важнее цены.
#[async_trait]
pub trait SubmissionGuard: Send + Sync {
    /// Можно ли отправлять ногу сейчас.
    async fn check(&self, intent: &OrderIntent) -> SubmissionDecision;
}

/// Итог одного прохода по фазе.
enum PhaseProgress {
    /// Все ноги фазы исполнены.
    Complete,
    /// Нужны новые события брокера или следующий проход; причина — для статуса.
    Waiting(Option<String>),
}

/// Результат отправки или reconciliation одной ноги.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecutionOutcome {
    /// Storage обновлён свежим состоянием заявки брокера.
    Submitted(OrderUpdate),
    /// Операция уже была обработана другим worker-ом.
    AlreadyHandled,
    /// Новая отправка запрещена, потому что execution deadline уже прошёл.
    Suppressed,
}

/// Результат одной попытки провести весь batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BatchExecutionOutcome {
    /// Нужны новые broker updates или повторная сверка после restart.
    AwaitingFills,
    /// Все reduction и opening legs полностью исполнены.
    Completed,
    /// Продолжать batch небезопасно либо одна из ног терминально завершилась.
    Failed,
}

/// Durable-исполнитель сохранённых двухфазных batch-ей.
///
/// Executor никогда не принимает торговое решение сам. Он загружает уже
/// сохранённые ноги, выполняет все `Reduction`, заново читает портфель и только
/// после сверки разрешает `Opening`. Стабильный `request_id` позволяет безопасно
/// выяснить исход неоднозначного сетевого ответа после сбоя.
pub struct BatchExecutor<P, T, S> {
    account_id: AccountId,
    strategy_id: StrategyId,
    portfolio: Arc<P>,
    trading: Arc<T>,
    storage: Arc<S>,
    guard: Option<Arc<dyn SubmissionGuard>>,
}

impl<P, T, S> BatchExecutor<P, T, S>
where
    P: Portfolio,
    T: Trading,
    S: Storage,
{
    #[must_use]
    /// Собирает executor для единственной пары счёт/стратегия.
    ///
    /// Явная привязка владельца является частью safety boundary: даже если в
    /// общей БД лежат batch-и других процессов, этот executor не имеет права
    /// менять их статусы или отправлять их ноги брокеру.
    pub fn new(
        account_id: AccountId,
        strategy_id: StrategyId,
        portfolio: Arc<P>,
        trading: Arc<T>,
        storage: Arc<S>,
    ) -> Self {
        Self {
            account_id,
            strategy_id,
            portfolio,
            trading,
            storage,
            guard: None,
        }
    }

    #[must_use]
    /// Подключает проверку рынка перед первой отправкой каждой ноги.
    pub fn with_guard(mut self, guard: Arc<dyn SubmissionGuard>) -> Self {
        self.guard = Some(guard);
        self
    }

    /// Продолжает normal-исполнение одного сохранённого batch.
    pub async fn execute_batch(&self, batch_id: Uuid) -> Result<BatchExecutionOutcome> {
        self.execute_batch_mode(batch_id, true).await
    }

    /// Восстанавливает сокращения, но не отправляет новые opening legs.
    ///
    /// Используется после risk-lock: ещё не отправленные открытия отклоняются,
    /// а потенциально отправленные только сверяются с брокером до терминального
    /// статуса. Неопределённую заявку нельзя объявлять Failed и забывать.
    pub async fn execute_batch_close_only(&self, batch_id: Uuid) -> Result<BatchExecutionOutcome> {
        self.execute_batch_mode(batch_id, false).await
    }

    async fn execute_batch_mode(
        &self,
        batch_id: Uuid,
        allow_openings: bool,
    ) -> Result<BatchExecutionOutcome> {
        let batch =
            self.storage.load_batch(batch_id).await?.ok_or_else(|| {
                Error::Storage(format!("rebalance batch `{batch_id}` was not found"))
            })?;
        if batch.account_id != self.account_id || batch.strategy_id != self.strategy_id {
            return Err(Error::RiskRejected(format!(
                "batch `{batch_id}` belongs to account `{}` and strategy `{}`, not this executor",
                batch.account_id, batch.strategy_id
            )));
        }
        let mut intents = self.storage.batch_intents(batch_id).await?;
        if !matches!(
            batch.status,
            RebalanceBatchStatus::Completed | RebalanceBatchStatus::Failed
        ) {
            match self.storage.batch_execution_gate(batch_id).await? {
                BatchExecutionGate::Ready => {}
                BatchExecutionGate::Hold => return Ok(BatchExecutionOutcome::AwaitingFills),
                BatchExecutionGate::Cancel => {
                    return self
                        .settle_failed_batch(
                            batch_id,
                            &intents,
                            "operator control cancelled execution",
                        )
                        .await;
                }
            }
        }
        if intents.iter().any(|stored| {
            stored.intent.batch_id != batch.id
                || stored.intent.account_id != batch.account_id
                || stored.intent.strategy_id != batch.strategy_id
                || stored.intent.event_id != batch.event_id
        }) {
            return Err(Error::Storage(format!(
                "batch `{batch_id}` contains an intent with mismatched ownership"
            )));
        }

        // Терминальный отказ reduction означает, что даже безопасная часть
        // исходного target недостижима. Он проверяется до reconciliation, чтобы
        // другая active reduction уже не получила разрешение на resubmit.
        if phase_has_terminal_failure(&intents, ExecutionPhase::Reduction) {
            return self
                .settle_failed_batch(
                    batch_id,
                    &intents,
                    "a reduction leg is rejected or cancelled",
                )
                .await;
        }

        // После crash reduction могла уже уйти брокеру. Сначала сверяем ровно
        // одну такую ногу. Остальные active/pending в этом проходе не трогаются:
        // если текущая стала terminal, дальнейшие отправки сразу запрещаются.
        if let Some(stored) = intents.iter().find(|stored| {
            stored.intent.execution_phase == ExecutionPhase::Reduction
                && matches!(
                    stored.status,
                    IntentStatus::Submitting | IntentStatus::Accepted | IntentStatus::Uncertain
                )
        }) {
            self.submit_or_reconcile(&stored.intent, true, None).await?;
            intents = self.storage.batch_intents(batch_id).await?;
            if phase_has_terminal_failure(&intents, ExecutionPhase::Reduction) {
                return self
                    .settle_failed_batch(
                        batch_id,
                        &intents,
                        "a reduction leg is rejected or cancelled",
                    )
                    .await;
            }
            if !phase_is_filled(&intents, ExecutionPhase::Reduction) {
                self.storage
                    .set_batch_status(batch_id, RebalanceBatchStatus::AwaitingFills, None)
                    .await?;
                return Ok(BatchExecutionOutcome::AwaitingFills);
            }
        }
        if !phase_is_filled(&intents, ExecutionPhase::Reduction) {
            // Pending reduction тоже нельзя слепо отправить после restart:
            // позицию могли вручную изменить между plan и recovery. target_lots
            // позволяет восстановить и проверить source каждой Pending ноги.
            let refreshed = self.portfolio.portfolio(&batch.account_id).await?.data;
            if refreshed.account_id != batch.account_id
                || refreshed.positions.iter().any(|position| position.blocked)
            {
                self.storage
                    .set_batch_status(
                        batch_id,
                        RebalanceBatchStatus::AwaitingFills,
                        Some("fresh portfolio is unavailable or contains blocked positions"),
                    )
                    .await?;
                return Ok(BatchExecutionOutcome::AwaitingFills);
            }
            if !pending_sources_match(&refreshed, &intents, ExecutionPhase::Reduction)? {
                return self
                    .settle_failed_batch(
                        batch_id,
                        &intents,
                        "fresh positions do not match the pending reduction sources",
                    )
                    .await;
            }
        }
        self.storage
            .set_batch_status(batch_id, RebalanceBatchStatus::Reducing, None)
            .await?;
        if let PhaseProgress::Waiting(reason) = self
            .execute_phase(&intents, ExecutionPhase::Reduction, None)
            .await?
        {
            intents = self.storage.batch_intents(batch_id).await?;
            if phase_has_terminal_failure(&intents, ExecutionPhase::Reduction) {
                return self
                    .settle_failed_batch(
                        batch_id,
                        &intents,
                        "a reduction leg is rejected or cancelled",
                    )
                    .await;
            }
            self.storage
                .set_batch_status(
                    batch_id,
                    RebalanceBatchStatus::AwaitingFills,
                    reason.as_deref(),
                )
                .await?;
            return Ok(BatchExecutionOutcome::AwaitingFills);
        }

        intents = self.storage.batch_intents(batch_id).await?;
        // Решение о переходе к openings принимается по свежему durable state, а
        // не только по ответам текущего worker-а.
        if phase_has_terminal_failure(&intents, ExecutionPhase::Reduction) {
            return self
                .settle_failed_batch(
                    batch_id,
                    &intents,
                    "a reduction leg is rejected or cancelled",
                )
                .await;
        }
        if !phase_is_filled(&intents, ExecutionPhase::Reduction) {
            self.storage
                .set_batch_status(batch_id, RebalanceBatchStatus::AwaitingFills, None)
                .await?;
            return Ok(BatchExecutionOutcome::AwaitingFills);
        }
        let opening_legs = intents
            .iter()
            .filter(|stored| stored.intent.execution_phase == ExecutionPhase::Opening)
            .collect::<Vec<_>>();
        if opening_legs.is_empty() {
            self.storage
                .set_batch_status(batch_id, RebalanceBatchStatus::Completed, None)
                .await?;
            return Ok(BatchExecutionOutcome::Completed);
        }

        // `valid_until` — крайний срок *новой отправки*. Просроченный target не
        // отменяет reductions и не позволяет забыть заявку, которая могла уже
        // попасть брокеру: такие opening legs только reconciliate-ятся.
        let opening_suppression = if !allow_openings {
            Some("opening phase suppressed by close-only risk mode")
        } else if Utc::now() > batch.target.valid_until {
            Some("opening phase suppressed because target has expired")
        } else {
            None
        };
        if let Some(reason) = opening_suppression {
            return self
                .settle_suppressed_openings(batch_id, &intents, reason)
                .await;
        }

        // Если полный target уже невозможен, ни одна active нога ниже не должна
        // получить разрешение на resubmit после broker NotFound.
        if phase_has_terminal_failure(&intents, ExecutionPhase::Opening) {
            return self
                .settle_failed_batch(
                    batch_id,
                    &intents,
                    "an opening leg is rejected or cancelled",
                )
                .await;
        }

        // Если до restart хотя бы одна opening leg уже могла уйти брокеру,
        // сначала только выясняем её исход. Оставшиеся Pending legs в этом же
        // проходе не отправляются: следующий проход заново прочитает портфель.
        if let Some(stored) = opening_legs.iter().find(|stored| {
            matches!(
                stored.status,
                IntentStatus::Submitting | IntentStatus::Accepted | IntentStatus::Uncertain
            )
        }) {
            self.submit_or_reconcile(&stored.intent, true, Some(batch.target.valid_until))
                .await?;
            intents = self.storage.batch_intents(batch_id).await?;
            // Проверяется результат каждой отдельной reconciliation. Если она
            // стала terminal, остальные legs обрабатываются только no-resubmit.
            if phase_has_terminal_failure(&intents, ExecutionPhase::Opening) {
                return self
                    .settle_failed_batch(
                        batch_id,
                        &intents,
                        "an opening leg is rejected or cancelled",
                    )
                    .await;
            }
            return self.finish_or_wait_openings(batch_id, &intents).await;
        }
        if phase_is_filled(&intents, ExecutionPhase::Opening) {
            self.storage
                .set_batch_status(batch_id, RebalanceBatchStatus::Completed, None)
                .await?;
            return Ok(BatchExecutionOutcome::Completed);
        }

        // Нельзя рассчитывать openings из снимка до reductions. Только теперь,
        // когда reductions подтверждены и нет неопределённой opening-заявки,
        // повторно читаем фактические позиции и сверяем source каждой Pending leg.
        let refreshed = self.portfolio.portfolio(&batch.account_id).await?.data;
        if refreshed.account_id != batch.account_id
            || refreshed.positions.iter().any(|position| position.blocked)
        {
            self.storage
                .set_batch_status(
                    batch_id,
                    RebalanceBatchStatus::AwaitingFills,
                    Some("fresh portfolio is unavailable or contains blocked positions"),
                )
                .await?;
            return Ok(BatchExecutionOutcome::AwaitingFills);
        }
        if !pending_sources_match(&refreshed, &intents, ExecutionPhase::Opening)? {
            return self
                .settle_failed_batch(
                    batch_id,
                    &intents,
                    "fresh positions do not match the pending opening sources",
                )
                .await;
        }
        self.storage
            .set_batch_status(batch_id, RebalanceBatchStatus::Opening, None)
            .await?;
        if let PhaseProgress::Waiting(reason) = self
            .execute_phase(
                &intents,
                ExecutionPhase::Opening,
                Some(batch.target.valid_until),
            )
            .await?
        {
            intents = self.storage.batch_intents(batch_id).await?;
            if phase_has_terminal_failure(&intents, ExecutionPhase::Opening) {
                return self
                    .settle_failed_batch(
                        batch_id,
                        &intents,
                        "an opening leg is rejected or cancelled",
                    )
                    .await;
            }
            self.storage
                .set_batch_status(
                    batch_id,
                    RebalanceBatchStatus::AwaitingFills,
                    reason.as_deref(),
                )
                .await?;
            return Ok(BatchExecutionOutcome::AwaitingFills);
        }
        self.storage
            .set_batch_status(batch_id, RebalanceBatchStatus::Completed, None)
            .await?;
        Ok(BatchExecutionOutcome::Completed)
    }

    /// Продолжает все незавершённые batch-и после запуска процесса.
    pub async fn recover_pending_batches(&self) -> Result<Vec<(Uuid, BatchExecutionOutcome)>> {
        self.recover_pending_batches_mode(true).await
    }

    /// Восстанавливает durable batch-и, не разрешая новые открытия.
    pub async fn recover_pending_batches_close_only(
        &self,
    ) -> Result<Vec<(Uuid, BatchExecutionOutcome)>> {
        self.recover_pending_batches_mode(false).await
    }

    async fn recover_pending_batches_mode(
        &self,
        allow_openings: bool,
    ) -> Result<Vec<(Uuid, BatchExecutionOutcome)>> {
        let mut outcomes = Vec::new();
        for batch_id in self
            .storage
            .pending_batch_ids(&self.account_id, &self.strategy_id)
            .await?
        {
            outcomes.push((
                batch_id,
                self.execute_batch_mode(batch_id, allow_openings).await?,
            ));
        }
        Ok(outcomes)
    }

    /// Завершает уже возможные broker openings без права послать новую заявку.
    async fn settle_suppressed_openings(
        &self,
        batch_id: Uuid,
        intents: &[StoredIntent],
        reason: &str,
    ) -> Result<BatchExecutionOutcome> {
        for stored in intents
            .iter()
            .filter(|stored| stored.intent.execution_phase == ExecutionPhase::Opening)
        {
            match stored.status {
                IntentStatus::Pending => {
                    // Отклоняем только действительно не захваченную ногу. Если
                    // конкурент уже сделал Pending→Submitting, свежая загрузка
                    // ниже увидит active status и оставит batch recoverable.
                    self.storage
                        .reject_pending_intent(stored.intent.id, reason)
                        .await?;
                }
                IntentStatus::Submitting | IntentStatus::Accepted | IntentStatus::Uncertain => {
                    self.reconcile_without_resubmission(&stored.intent, reason)
                        .await?;
                }
                IntentStatus::Filled | IntentStatus::Cancelled | IntentStatus::Rejected => {}
            }
        }
        let refreshed = self.storage.batch_intents(batch_id).await?;
        if phase_has_active(&refreshed, ExecutionPhase::Opening) {
            self.storage
                .set_batch_status(batch_id, RebalanceBatchStatus::AwaitingFills, Some(reason))
                .await?;
            return Ok(BatchExecutionOutcome::AwaitingFills);
        }
        if phase_is_filled(&refreshed, ExecutionPhase::Opening) {
            self.storage
                .set_batch_status(batch_id, RebalanceBatchStatus::Completed, None)
                .await?;
            return Ok(BatchExecutionOutcome::Completed);
        }
        self.storage
            .set_batch_status(batch_id, RebalanceBatchStatus::Failed, Some(reason))
            .await?;
        Ok(BatchExecutionOutcome::Failed)
    }

    /// Классифицирует opening phase после reconciliation без новых отправок.
    async fn finish_or_wait_openings(
        &self,
        batch_id: Uuid,
        intents: &[StoredIntent],
    ) -> Result<BatchExecutionOutcome> {
        if phase_is_filled(intents, ExecutionPhase::Opening) {
            self.storage
                .set_batch_status(batch_id, RebalanceBatchStatus::Completed, None)
                .await?;
            return Ok(BatchExecutionOutcome::Completed);
        }
        if phase_has_terminal_failure(intents, ExecutionPhase::Opening) {
            return self
                .settle_failed_batch(batch_id, intents, "an opening leg is rejected or cancelled")
                .await;
        }
        self.storage
            .set_batch_status(batch_id, RebalanceBatchStatus::AwaitingFills, None)
            .await?;
        Ok(BatchExecutionOutcome::AwaitingFills)
    }

    /// Доводит невозможный полный target до безопасного terminal-состояния.
    ///
    /// Наличие одной rejected/cancelled ноги ещё не разрешает забыть остальные:
    /// Pending отклоняются локально, а заявки, которые могли попасть брокеру,
    /// только reconciliate-ятся. Batch остаётся recoverable, пока хотя бы одна
    /// такая заявка не получила terminal status.
    async fn settle_failed_batch(
        &self,
        batch_id: Uuid,
        intents: &[StoredIntent],
        reason: &str,
    ) -> Result<BatchExecutionOutcome> {
        for stored in intents {
            match stored.status {
                IntentStatus::Pending => {
                    // CAS не позволяет локальному settle отменить заявку после
                    // того, как другой worker получил право на broker RPC.
                    self.storage
                        .reject_pending_intent(stored.intent.id, reason)
                        .await?;
                }
                IntentStatus::Submitting | IntentStatus::Accepted | IntentStatus::Uncertain => {
                    self.reconcile_without_resubmission(&stored.intent, reason)
                        .await?;
                }
                IntentStatus::Filled | IntentStatus::Cancelled | IntentStatus::Rejected => {}
            }
        }
        let refreshed = self.storage.batch_intents(batch_id).await?;
        if intents_have_active_legs(&refreshed) {
            self.storage
                .set_batch_status(batch_id, RebalanceBatchStatus::AwaitingFills, Some(reason))
                .await?;
            return Ok(BatchExecutionOutcome::AwaitingFills);
        }
        self.storage
            .set_batch_status(batch_id, RebalanceBatchStatus::Failed, Some(reason))
            .await?;
        Ok(BatchExecutionOutcome::Failed)
    }

    async fn execute_phase(
        &self,
        intents: &[StoredIntent],
        phase: ExecutionPhase,
        submission_deadline: Option<DateTime<Utc>>,
    ) -> Result<PhaseProgress> {
        // Отложенная проверкой рынка нога не мешает отправить остальные ноги той
        // же фазы: порядок важен только между фазами.
        let mut deferred: Option<String> = None;
        for stored in intents
            .iter()
            .filter(|stored| stored.intent.execution_phase == phase)
        {
            match stored.status {
                IntentStatus::Filled => {}
                IntentStatus::Cancelled | IntentStatus::Rejected => {
                    return Ok(PhaseProgress::Waiting(None));
                }
                IntentStatus::Pending => {
                    // Deadline проверяется непосредственно перед CAS/сетевым
                    // вызовом, а не только при загрузке batch: portfolio refresh
                    // сам может занять оставшееся время жизни target.
                    if submission_deadline.is_some_and(|deadline| Utc::now() > deadline) {
                        self.storage
                            .reject_pending_intent(
                                stored.intent.id,
                                "opening target expired before broker submission",
                            )
                            .await?;
                        return Ok(PhaseProgress::Waiting(None));
                    }
                    if let Some(guard) = &self.guard {
                        match guard.check(&stored.intent).await {
                            SubmissionDecision::Proceed => {}
                            SubmissionDecision::Defer(reason) => {
                                deferred.get_or_insert(reason);
                                continue;
                            }
                            SubmissionDecision::Skip(reason) => {
                                self.storage
                                    .reject_pending_intent(stored.intent.id, &reason)
                                    .await?;
                                return Ok(PhaseProgress::Waiting(Some(reason)));
                            }
                        }
                    }
                    // CAS Pending→Submitting даёт право отправки только одному
                    // worker-у и закрывает окно двойной заявки.
                    if !self.storage.claim_intent(stored.intent.id).await? {
                        return Ok(PhaseProgress::Waiting(None));
                    }
                    // CAS сам является await и мог завершиться уже после
                    // deadline. В этот момент RPC ещё точно не начинался, поэтому
                    // захваченный Submitting можно безопасно сделать Rejected.
                    if submission_deadline.is_some_and(|deadline| Utc::now() > deadline) {
                        self.storage
                            .reject_intent(
                                stored.intent.id,
                                "opening target expired while claiming broker submission",
                            )
                            .await?;
                        return Ok(PhaseProgress::Waiting(None));
                    }
                    let update = self
                        .submit_or_reconcile(&stored.intent, false, submission_deadline)
                        .await?;
                    if !matches!(
                        update,
                        ExecutionOutcome::Submitted(OrderUpdate {
                            status: OrderStatus::Filled,
                            ..
                        })
                    ) {
                        return Ok(PhaseProgress::Waiting(None));
                    }
                }
                IntentStatus::Submitting | IntentStatus::Accepted | IntentStatus::Uncertain => {
                    // После crash неизвестно, дошёл ли предыдущий запрос. Сначала
                    // запрашиваем брокера по тому же request_id, не отправляем заново.
                    let update = self
                        .submit_or_reconcile(&stored.intent, true, submission_deadline)
                        .await?;
                    if !matches!(
                        update,
                        ExecutionOutcome::Submitted(OrderUpdate {
                            status: OrderStatus::Filled,
                            ..
                        })
                    ) {
                        return Ok(PhaseProgress::Waiting(None));
                    }
                }
            }
        }
        Ok(deferred.map_or(PhaseProgress::Complete, |reason| {
            PhaseProgress::Waiting(Some(reason))
        }))
    }

    async fn submit_or_reconcile(
        &self,
        intent: &OrderIntent,
        reconcile_first: bool,
        resubmission_deadline: Option<DateTime<Utc>>,
    ) -> Result<ExecutionOutcome> {
        if reconcile_first {
            return self.reconcile(intent, None, resubmission_deadline).await;
        }
        match self.trading.place_order(intent).await {
            Ok(response) => {
                self.storage
                    .record_order_update(intent.id, &response.data)
                    .await?;
                Ok(ExecutionOutcome::Submitted(response.data))
            }
            Err(error) if ambiguous_order_error(&error) => {
                self.reconcile(intent, Some(error), resubmission_deadline)
                    .await
            }
            Err(error) => {
                self.storage
                    .reject_intent(intent.id, &error.to_string())
                    .await?;
                Err(error)
            }
        }
    }

    async fn reconcile(
        &self,
        intent: &OrderIntent,
        original: Option<Error>,
        resubmission_deadline: Option<DateTime<Utc>>,
    ) -> Result<ExecutionOutcome> {
        match self
            .trading
            .order_state(&intent.account_id, intent.request_id)
            .await
        {
            Ok(response) => {
                self.storage
                    .record_order_update(intent.id, &response.data)
                    .await?;
                Ok(ExecutionOutcome::Submitted(response.data))
            }
            Err(Error::Status(status)) if status.code() == Code::NotFound => {
                // Только подтверждённый NotFound означает, что request_id у
                // брокера отсутствует. Но перед повтором нужно ещё доказать, что
                // фактическая позиция не изменилась относительно source этой ноги.
                if resubmission_deadline.is_some_and(|deadline| Utc::now() > deadline) {
                    self.storage
                        .reject_intent(
                            intent.id,
                            "opening target expired before broker resubmission",
                        )
                        .await?;
                    return Ok(ExecutionOutcome::Suppressed);
                }
                let portfolio = self.portfolio.portfolio(&intent.account_id).await?.data;
                if portfolio.account_id != intent.account_id
                    || portfolio.positions.iter().any(|position| position.blocked)
                    || !intent_source_matches(&portfolio, intent)?
                {
                    self.storage
                        .reject_intent(
                            intent.id,
                            "broker request is absent, but the current position no longer matches the intent source",
                        )
                        .await?;
                    return Ok(ExecutionOutcome::Suppressed);
                }
                // Portfolio RPC тоже занимает время; opening deadline повторно
                // проверяется непосредственно перед mutation RPC.
                if resubmission_deadline.is_some_and(|deadline| Utc::now() > deadline) {
                    self.storage
                        .reject_intent(
                            intent.id,
                            "opening target expired while validating broker resubmission",
                        )
                        .await?;
                    return Ok(ExecutionOutcome::Suppressed);
                }
                if self.storage.batch_execution_gate(intent.batch_id).await?
                    != BatchExecutionGate::Ready
                {
                    self.storage
                        .reject_intent(intent.id, "operator control forbids resubmission")
                        .await?;
                    return Ok(ExecutionOutcome::Suppressed);
                }
                match self.trading.place_order(intent).await {
                    Ok(response) => {
                        self.storage
                            .record_order_update(intent.id, &response.data)
                            .await?;
                        Ok(ExecutionOutcome::Submitted(response.data))
                    }
                    Err(error) if ambiguous_order_error(&error) => {
                        self.storage
                            .mark_uncertain(intent.id, &error.to_string())
                            .await?;
                        Err(error)
                    }
                    Err(error) => {
                        // InvalidArgument/PermissionDenied и прочие однозначные
                        // ответы доказывают, что заявка не принята. Оставлять их
                        // Uncertain означало бы бесконечно повторять невалидный RPC.
                        self.storage
                            .reject_intent(intent.id, &error.to_string())
                            .await?;
                        Err(error)
                    }
                }
            }
            Err(error) => {
                let reason = original.map_or_else(
                    || error.to_string(),
                    |original| {
                        format!("submission failed: {original}; reconciliation failed: {error}")
                    },
                );
                self.storage.mark_uncertain(intent.id, &reason).await?;
                Err(error)
            }
        }
    }

    /// Сверяет потенциально отправленную заявку, но никогда не повторяет её.
    ///
    /// Это отличие критично для close-only и просроченного target: подтверждённый
    /// `NotFound` здесь означает безопасный локальный Reject, а не разрешение
    /// снова открыть позицию.
    async fn reconcile_without_resubmission(
        &self,
        intent: &OrderIntent,
        suppression_reason: &str,
    ) -> Result<()> {
        match self
            .trading
            .order_state(&intent.account_id, intent.request_id)
            .await
        {
            Ok(response) => {
                self.storage
                    .record_order_update(intent.id, &response.data)
                    .await?;
                Ok(())
            }
            Err(Error::Status(status)) if status.code() == Code::NotFound => {
                self.storage
                    .reject_intent(
                        intent.id,
                        &format!("{suppression_reason}; request_id is absent at broker"),
                    )
                    .await
            }
            Err(error) => {
                self.storage
                    .mark_uncertain(
                        intent.id,
                        &format!("{suppression_reason}; reconciliation failed: {error}"),
                    )
                    .await?;
                Err(error)
            }
        }
    }
}

fn phase_has_terminal_failure(intents: &[StoredIntent], phase: ExecutionPhase) -> bool {
    intents.iter().any(|stored| {
        stored.intent.execution_phase == phase
            && matches!(
                stored.status,
                IntentStatus::Cancelled | IntentStatus::Rejected
            )
    })
}

fn phase_has_active(intents: &[StoredIntent], phase: ExecutionPhase) -> bool {
    intents.iter().any(|stored| {
        stored.intent.execution_phase == phase
            && matches!(
                stored.status,
                IntentStatus::Pending
                    | IntentStatus::Submitting
                    | IntentStatus::Accepted
                    | IntentStatus::Uncertain
            )
    })
}

fn intents_have_active_legs(intents: &[StoredIntent]) -> bool {
    intents.iter().any(|stored| {
        matches!(
            stored.status,
            IntentStatus::Pending
                | IntentStatus::Submitting
                | IntentStatus::Accepted
                | IntentStatus::Uncertain
        )
    })
}

fn phase_is_filled(intents: &[StoredIntent], phase: ExecutionPhase) -> bool {
    intents
        .iter()
        .filter(|stored| stored.intent.execution_phase == phase)
        .all(|stored| stored.status == IntentStatus::Filled)
}

fn pending_sources_match(
    portfolio: &PortfolioSnapshot,
    intents: &[StoredIntent],
    phase: ExecutionPhase,
) -> Result<bool> {
    let pending_legs = intents
        .iter()
        .filter(|stored| stored.intent.execution_phase == phase)
        .collect::<Vec<_>>();
    // Filled legs уже закономерно изменили позицию. Сверяются только ещё не
    // отправленные ноги; active legs до этой функции не допускаются executor-ом.
    for stored in pending_legs
        .into_iter()
        .filter(|stored| stored.status == IntentStatus::Pending)
    {
        if !intent_source_matches(portfolio, &stored.intent)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Сверяет фактическое количество бумаг с позицией, от которой была построена leg.
///
/// Intent хранит конечный `target_lots` и положительный размер delta. Поэтому
/// исходная позиция восстанавливается однозначно: для buy это `target - lots`,
/// для sell — `target + lots`. Проверка используется и для Pending legs, и перед
/// resubmit ранее захваченной, но отсутствующей у брокера заявки.
fn intent_source_matches(portfolio: &PortfolioSnapshot, intent: &OrderIntent) -> Result<bool> {
    let source_lots = match intent.side {
        OrderSide::Buy => intent.target_lots.checked_sub(intent.lots),
        OrderSide::Sell => intent.target_lots.checked_add(intent.lots),
    }
    .ok_or_else(|| Error::RiskRejected("order source lots overflow".to_owned()))?;
    let expected_quantity = Decimal::from(source_lots) * Decimal::from(intent.lot_size);
    let actual_quantity = portfolio
        .positions
        .iter()
        .filter(|position| position.instrument_id == intent.instrument_id)
        .map(|position| position.quantity)
        .sum::<Decimal>();
    Ok(actual_quantity == expected_quantity)
}

fn ambiguous_order_error(error: &Error) -> bool {
    // Эти ошибки не доказывают, что mutation не дошла до брокера. Повторная
    // отправка без reconciliation могла бы создать дублирующую заявку.
    matches!(
        error,
        Error::Status(status)
            if matches!(
                status.code(),
                Code::Cancelled
                    | Code::Unknown
                    | Code::DeadlineExceeded
                    | Code::Internal
                    | Code::Unavailable
            )
    ) || matches!(error, Error::Timeout(_))
}
