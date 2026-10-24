//! Durable commands. Every broker mutation runs inside the existing account lease.
use super::{
    AccountId, BatchExecutor, BotConfig, Decimal, Duration, Error, HashMap, InstrumentId,
    InstrumentInfo, LiveRuntime, MarketData, MissedTickBehavior, Money, PlanningContext, Portfolio,
    PortfolioEngine, PortfolioSnapshot, PortfolioStrategy, PostgresStorage, Price, Result,
    RiskMode, RiskObservation, RuntimeIds, StdDuration, Storage, Utc, WeightedEnsembleStrategy,
    connect_storage, connect_venue, current_moex_session_date, ensure_account_exists, info,
    initialize_risk, portfolio_is_flat, refresh_execution_data, resilience, rub_equity,
    scan_external_cash_flows, validate_dedicated_portfolio, warn, watch,
};
use rust_decimal::prelude::ToPrimitive;
use serde_json::{Value, json};
use sqlx::Row;
use tinkoff_rs::{
    broker::Trading,
    domain::{ExecutionPhase, MarginConsent, OrderIntent, OrderKind, OrderSide},
    engine::{
        PlannedBatch, RebalanceBatch, RebalanceBatchStatus, RiskDecision, TargetPlanner,
        TargetPortfolio, TargetWeight,
    },
    proto::SandboxPayInRequest,
    storage::IntentStatus,
};
use uuid::Uuid;

use super::{VenueApi, VenueBroker, instruments, venue::Contour};

pub(super) struct Settings {
    pub enabled: bool,
    pub cleanup_requested: bool,
    budget: Option<Decimal>,
    revision: i64,
    mode: String,
}

/// Регистрирует строку управления стратегии. Реальный счёт создаётся выключенным,
/// с подтверждением каждого плана и без закрытия по расписанию; строка другого
/// контура или реальная строка без подтверждения останавливают запуск.
pub(super) async fn register(
    storage: &PostgresStorage,
    ids: &RuntimeIds,
    contour: Contour,
) -> Result<()> {
    sqlx::query("INSERT INTO strategy_controls(strategy_id,account_id,enabled,contour,execution_mode,close_after) VALUES($1,$2,CASE WHEN $3='production' THEN false ELSE COALESCE((SELECT mode='campaign' FROM dashboard_settings WHERE id=true),true) END,$3,CASE WHEN $3='production' THEN 'confirm' ELSE 'auto' END,$3<>'production') ON CONFLICT(strategy_id) DO NOTHING")
        .bind(ids.strategy_id.as_str()).bind(ids.account_id.as_str()).bind(contour.label()).execute(storage.pool()).await?;
    let row = sqlx::query(
        "SELECT account_id,contour,execution_mode FROM strategy_controls WHERE strategy_id=$1",
    )
    .bind(ids.strategy_id.as_str())
    .fetch_one(storage.pool())
    .await?;
    let account: String = row.try_get("account_id")?;
    let stored: String = row.try_get("contour")?;
    let mode: String = row.try_get("execution_mode")?;
    if account != ids.account_id.as_str() {
        return Err(reject("control account does not match runtime"));
    }
    if stored != contour.label() {
        return Err(reject("control contour does not match runtime"));
    }
    if contour == Contour::Production && mode != "confirm" {
        return Err(reject("a real account must confirm every plan"));
    }
    Ok(())
}

pub(super) async fn managed_universe(
    storage: &PostgresStorage,
    account: &AccountId,
    universe: &[InstrumentId],
) -> Result<Vec<InstrumentId>> {
    let mut result = universe.to_vec();
    let extra: Vec<String> = sqlx::query_scalar(
        "SELECT instrument_id FROM account_managed_instruments WHERE account_id=$1",
    )
    .bind(account.as_str())
    .fetch_all(storage.pool())
    .await?;
    for uid in extra {
        let id = InstrumentId::new(uid)?;
        if !result.contains(&id) {
            result.push(id);
        }
    }
    Ok(result)
}

/// A paused account still needs a lease holder for commands and protective closing.
/// No strategy events are generated here, even if a resume command arrives.
///
/// Сервис команд счёта без автоматических сигналов (`run --service-only`).
/// Как и основной runtime, сервис переживает отсутствие сети: команды остаются
/// в очереди, risk-check повторяется, а после обрыва при запуске сервис
/// переподключается с задержкой до минуты.
pub(super) async fn serve(config: BotConfig, ids: RuntimeIds) -> Result<()> {
    resilience::run_with_reconnect(
        "account service",
        resilience::shutdown_signal(),
        resilience::Backoff::session(),
        |mut shutdown| {
            let config = &config;
            let ids = &ids;
            async move {
                let runtime = tokio::select! {
                    runtime = start_service(config, ids) => runtime?,
                    () = resilience::shutdown_requested(&mut shutdown) => return Ok(()),
                };
                serve_commands(runtime, &mut shutdown).await
            }
        },
    )
    .await
}

async fn start_service(config: &BotConfig, ids: &RuntimeIds) -> Result<LiveRuntime> {
    let storage = connect_storage(config).await?;
    storage.migrate().await?;
    let lease = storage.acquire_runtime_lease(&ids.account_id).await?;
    super::venue::ensure_not_halted(&storage, config.contour).await?;
    let (api, broker) = connect_venue(config).await?;
    ensure_account_exists(&broker, &ids.account_id).await?;
    let strategy = WeightedEnsembleStrategy::new(
        ids.strategy_id.clone(),
        ids.account_id.clone(),
        ids.universe.clone(),
        config.strategies.clone(),
    )?;
    let initial_state = strategy.state()?;
    let engine = PortfolioEngine::new(Box::new(strategy), storage.clone()).await?;
    register(&storage, ids, config.contour).await?;
    if storage
        .load_portfolio_strategy_state(&ids.strategy_id)
        .await?
        .is_none()
    {
        storage
            .commit_portfolio_event(
                &ids.strategy_id,
                "control:initialize",
                &initial_state,
                None,
                &[],
                None,
            )
            .await?;
    }
    let portfolio = broker.portfolio(&ids.account_id).await?.data;
    let session = current_moex_session_date(
        &api,
        Utc::now(),
        storage
            .load_risk_state(&ids.account_id)
            .await?
            .map(|s| s.session_date),
    )
    .await?;
    let risk = initialize_risk(&api, &storage, &config.risk, ids, &portfolio, session).await?;
    let mut executor = BatchExecutor::new(
        ids.account_id.clone(),
        ids.strategy_id.clone(),
        broker.clone(),
        broker.clone(),
        storage.clone(),
    );
    // Подтверждённые вручную планы реального счёта проходят ту же проверку стакана.
    if let Some(guard) = super::spread_guard(config, &api) {
        executor = executor.with_guard(guard);
    }
    let costs = instruments::ExecutionCosts::load(&storage, config.contour.label()).await;
    Ok(LiveRuntime {
        account_id: ids.account_id.clone(),
        strategy_id: ids.strategy_id.clone(),
        universe: ids.universe.clone(),
        instruments: Vec::new(),
        latest_prices: HashMap::new(),
        api,
        broker,
        storage,
        engine,
        executor,
        runtime_lease: lease,
        risk,
        health: resilience::NetworkHealth::default(),
        pending_session_date: None,
        pending_open: None,
        ai_jobs: Vec::new(),
        ai_done: std::collections::HashSet::new(),
        ai_failures: HashMap::new(),
        ai_watch: super::ai::Watch::default(),
        costs,
    })
}

async fn serve_commands(
    mut runtime: LiveRuntime,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<()> {
    let mut commands = tokio::time::interval(StdDuration::from_secs(1));
    commands.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut risk_tick = tokio::time::interval(StdDuration::from_secs(30));
    risk_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    info!(strategy_id=%runtime.strategy_id, "account control service ready; automatic signals disabled");
    loop {
        let (context, result) = tokio::select! {
            () = resilience::shutdown_requested(shutdown) => return Ok(()),
            _ = commands.tick() => ("account commands", runtime.poll_controls().await),
            _ = risk_tick.tick() => ("risk-check", runtime.service_risk_check().await),
        };
        match result {
            Ok(()) => {
                if context == "risk-check" {
                    runtime.health.success();
                }
            }
            Err(error) if error.is_transient() => {
                runtime.health.failure(context, &error);
                if runtime.health.needs_rebuild() {
                    // Долгий простой: сервис пересоздаётся с новыми соединениями.
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

impl LiveRuntime {
    /// Периодический контроль риска сервиса: защитное закрытие либо recovery заявок.
    async fn service_risk_check(&mut self) -> Result<()> {
        self.runtime_lease.check().await?;
        let (portfolio, decision) = self.control_observation().await?;
        self.sample_performance(&portfolio, &decision).await?;
        let settings = self.control_settings().await?;
        if decision.mode == RiskMode::CloseOnly || settings.cleanup_requested {
            self.cancel_batches(None).await?;
            if let Err(error) = self.protective_close(settings.cleanup_requested).await {
                warn!(%error, "protective cleanup waits for reconciliation or trading availability");
            }
        } else {
            // Includes durable manual orders; ordinary strategy plans remain gated.
            self.executor.recover_pending_batches().await?;
        }
        self.update_service_need(&portfolio).await
    }

    pub(super) async fn control_settings(&self) -> Result<Settings> {
        let r = sqlx::query("SELECT enabled,cleanup_requested,execution_budget,revision,execution_mode FROM strategy_controls WHERE strategy_id=$1 AND account_id=$2")
            .bind(self.strategy_id.as_str()).bind(self.account_id.as_str()).fetch_one(self.storage.pool()).await?;
        Ok(Settings {
            enabled: r.try_get("enabled")?,
            cleanup_requested: r.try_get("cleanup_requested")?,
            budget: r.try_get("execution_budget")?,
            revision: r.try_get("revision")?,
            mode: r.try_get("execution_mode")?,
        })
    }

    pub(super) async fn apply_execution_budget(&self, decision: &mut RiskDecision) -> Result<()> {
        let settings = self.control_settings().await?;
        // Budget changes change available sizing, never the original stop-loss anchors.
        let pnl = decision.strategy_equity - decision.state.allocated_principal;
        let equity = decision.state.account_equity_at_anchor
            + pnl
            + decision.state.external_cash_flows_since_anchor;
        decision.sizing_capital = settings
            .budget
            .map_or(decision.sizing_capital, |budget| {
                budget.min((budget + pnl).max(Decimal::ZERO))
            })
            .min(equity.max(Decimal::ZERO));
        decision.effective.configured.budget_rub =
            settings.budget.unwrap_or(decision.state.budget_rub);
        Ok(())
    }

    async fn control_observation(&mut self) -> Result<(PortfolioSnapshot, RiskDecision)> {
        self.runtime_lease.check().await?;
        let portfolio = self.broker.portfolio(&self.account_id).await?.data;
        let (flows, recognized) = scan_external_cash_flows(&self.api, self.risk.state()).await?;
        let session =
            current_moex_session_date(&self.api, Utc::now(), Some(self.risk.state().session_date))
                .await?;
        let mut decision = self.risk.observe(&RiskObservation {
            current_equity: rub_equity(&portfolio)?,
            session_date: session,
            observed_at: Utc::now(),
            external_cash_flows: flows,
            external_cash_flows_recognized: recognized,
            portfolio_is_flat: portfolio_is_flat(&portfolio),
        })?;
        self.storage.save_risk_state(&decision.state).await?;
        self.apply_execution_budget(&mut decision).await?;
        Ok((portfolio, decision))
    }

    pub(super) async fn poll_controls(&mut self) -> Result<()> {
        self.runtime_lease.check().await?;
        let row = sqlx::query("UPDATE control_commands SET status='running',updated_at=now() WHERE id=(SELECT id FROM control_commands WHERE COALESCE(execution_strategy_id,strategy_id)=$1 AND account_id=$2 AND status IN ('queued','running') AND kind IN ('pause','resume','manual_order','cancel_batch','approve_batch','execution_mode','budget','pay_in','new_run','cleanup') ORDER BY created_at,id LIMIT 1 FOR UPDATE SKIP LOCKED) RETURNING id,kind,payload")
            .bind(self.strategy_id.as_str()).bind(self.account_id.as_str()).fetch_optional(self.storage.pool()).await?;
        let Some(row) = row else {
            if !self.control_settings().await?.enabled {
                for batch_id in self
                    .storage
                    .pending_batch_ids(&self.account_id, &self.strategy_id)
                    .await?
                {
                    let batch = self.owned_batch(batch_id).await?;
                    if !batch.event_id.starts_with("manual:") {
                        self.cancel_batches(Some(batch_id)).await?;
                    }
                }
            }
            return Ok(());
        };
        let id: Uuid = row.try_get("id")?;
        let kind: String = row.try_get("kind")?;
        let payload: Value = row.try_get("payload")?;
        let result = self.execute_command(id, &kind, &payload).await;
        match result {
            Ok(value) => {
                self.finish_command(id, "succeeded", &value).await?;
            }
            Err(error) => {
                // Network/storage interruptions leave commands recoverable; pay_in has
                // already moved to uncertain before its non-idempotent RPC.
                let status = if error.is_transient()
                    || matches!(
                        error,
                        Error::Status(_) | Error::Storage(_) | Error::LeaseLost(_)
                    ) {
                    "running"
                } else {
                    "failed"
                };
                self.finish_command(id, status, &json!({"error":error.to_string()}))
                    .await?;
                warn!(%id, %kind, %error, "account command did not complete");
            }
        }
        Ok(())
    }

    async fn finish_command(&self, id: Uuid, status: &str, result: &Value) -> Result<()> {
        sqlx::query("UPDATE control_commands SET status=$2,result=$3,updated_at=now() WHERE id=$1 AND status='running'")
            .bind(id).bind(status).bind(result).execute(self.storage.pool()).await?;
        Ok(())
    }

    async fn execute_command(&mut self, id: Uuid, kind: &str, payload: &Value) -> Result<Value> {
        // Реальный счёт: каждый план подтверждается, денег и ручных заявок стенд не трогает.
        if self.api.contour() == Contour::Production {
            match kind {
                "pay_in" => return Err(reject("virtual pay-in exists only in Sandbox")),
                "manual_order" => {
                    return Err(reject(
                        "manual orders on a real account are placed in the broker app",
                    ));
                }
                "execution_mode"
                    if payload.get("mode").and_then(Value::as_str) != Some("confirm") =>
                {
                    return Err(reject("a real account confirms every plan"));
                }
                _ => {}
            }
        }
        match kind {
            "pause" => {
                self.pause().await?;
                self.mark_mixed(id, "pause").await?;
                self.cancel_batches(None).await?;
                self.require_settled().await?;
                Ok(json!({"enabled":false,"positions_preserved":true}))
            }
            "resume" => {
                self.require_settled().await?;
                let (portfolio, decision) = self.control_observation().await?;
                validate_dedicated_portfolio(&portfolio, &self.universe)?;
                if decision.mode == RiskMode::CloseOnly {
                    return Err(reject(
                        "risk block remains active; resume does not reset risk",
                    ));
                }
                sqlx::query("UPDATE strategy_controls SET enabled=true,cleanup_requested=false,needs_service=true,updated_at=now() WHERE strategy_id=$1")
                    .bind(self.strategy_id.as_str()).execute(self.storage.pool()).await?;
                Ok(
                    json!({"enabled":true,"readiness":"normal runtime must confirm subscriptions and warmup"}),
                )
            }
            "manual_order" => self.manual_order(id, payload).await,
            "cancel_batch" => {
                let batch = self.owned_batch(uuid_field(payload, "batch_id")?).await?;
                if protective_event(&batch.event_id) {
                    return Err(reject("protective closing cannot be disabled"));
                }
                self.mark_mixed(id, "cancel_batch").await?;
                self.cancel_batches(Some(batch.id)).await?;
                let unsettled = self
                    .storage
                    .batch_intents(batch.id)
                    .await?
                    .iter()
                    .any(|i| active(i.status));
                if unsettled {
                    return Err(Error::Storage(
                        "cancellation awaits broker reconciliation".into(),
                    ));
                }
                Ok(json!({"batch_id":batch.id,"cancelled":true}))
            }
            "approve_batch" => self.approve_batch(id, payload).await,
            "execution_mode" => {
                let mode = text_field(payload, "mode")?;
                if !matches!(mode, "auto" | "confirm") {
                    return Err(reject("execution mode must be auto or confirm"));
                }
                if mode == "confirm" {
                    self.mark_mixed(id, "confirmation_mode").await?;
                }
                self.cancel_batches(None).await?;
                self.require_settled().await?;
                sqlx::query("UPDATE strategy_controls SET execution_mode=$2,revision=revision+1,updated_at=now() WHERE strategy_id=$1")
                    .bind(self.strategy_id.as_str()).bind(mode).execute(self.storage.pool()).await?;
                Ok(json!({"mode":mode}))
            }
            "budget" => self.change_budget(id, payload).await,
            "pay_in" => self.pay_in(id, payload).await,
            "new_run" => {
                self.require_settled().await?;
                let (portfolio, decision) = self.control_observation().await?;
                if !portfolio_is_flat(&portfolio) {
                    return Err(reject("a new clean run requires an empty account"));
                }
                if self.control_settings().await?.mode != "auto" {
                    return Err(reject("a clean run requires automatic execution mode"));
                }
                let run = self.create_run(&portfolio, &decision, Some(id)).await?;
                Ok(json!({"run_id":run,"risk_epoch_preserved":true}))
            }
            "cleanup" => {
                self.pause().await?;
                self.mark_mixed(id, "cleanup").await?;
                sqlx::query("UPDATE strategy_controls SET cleanup_requested=true,needs_service=true,updated_at=now() WHERE strategy_id=$1")
                    .bind(self.strategy_id.as_str()).execute(self.storage.pool()).await?;
                self.cancel_batches(None).await?;
                if let Err(error) = self.protective_close(true).await {
                    warn!(%error, "cleanup remains requested until positions can close");
                }
                Ok(
                    json!({"cleanup_requested":true,"completion":"positions and orders are checked on every risk tick"}),
                )
            }
            _ => Err(reject("unsupported account command")),
        }
    }

    async fn pause(&self) -> Result<()> {
        sqlx::query("UPDATE strategy_controls SET enabled=false,needs_service=true,updated_at=now() WHERE strategy_id=$1")
            .bind(self.strategy_id.as_str()).execute(self.storage.pool()).await?;
        Ok(())
    }

    async fn require_settled(&self) -> Result<()> {
        if self
            .storage
            .has_unfinished_batches(&self.account_id)
            .await?
            || self.storage.has_pending_orders(&self.account_id).await?
        {
            return Err(Error::Storage("orders are still being reconciled".into()));
        }
        if !self.api.active_orders(&self.account_id).await?.is_empty() {
            return Err(reject(
                "account contains broker orders that must be cancelled first",
            ));
        }
        Ok(())
    }

    async fn owned_batch(&self, id: Uuid) -> Result<RebalanceBatch> {
        let batch = self
            .storage
            .load_batch(id)
            .await?
            .ok_or_else(|| reject("unknown batch"))?;
        if batch.account_id != self.account_id || batch.strategy_id != self.strategy_id {
            return Err(reject("batch belongs to another account or strategy"));
        }
        Ok(batch)
    }

    /// Cancellation proves each possibly submitted UUID absent or terminal. A
    /// timeout never converts an uncertain order into a local cancellation.
    pub(super) async fn cancel_batches(&mut self, only: Option<Uuid>) -> Result<()> {
        let batches = if let Some(id) = only {
            vec![self.owned_batch(id).await?.id]
        } else {
            self.storage
                .pending_batch_ids(&self.account_id, &self.strategy_id)
                .await?
        };
        for id in batches {
            let batch = self.owned_batch(id).await?;
            if protective_event(&batch.event_id) {
                continue;
            }
            let protected: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM batch_controls WHERE batch_id=$1 AND revision=-1)",
            )
            .bind(id)
            .fetch_one(self.storage.pool())
            .await?;
            if protected {
                continue;
            }
            sqlx::query("INSERT INTO batch_controls(batch_id,status,revision) VALUES($1,'rejected',0) ON CONFLICT(batch_id) DO UPDATE SET status='rejected',updated_at=now()")
                .bind(id).execute(self.storage.pool()).await?;
            let intents = self.storage.batch_intents(id).await?;
            for stored in intents {
                let intent = stored.intent;
                if stored.status == IntentStatus::Pending {
                    self.storage
                        .reject_pending_intent(intent.id, "cancelled by account controller")
                        .await?;
                    continue;
                }
                if !active(stored.status) {
                    continue;
                }
                self.runtime_lease.check().await?;
                match self
                    .broker
                    .order_state(&self.account_id, intent.request_id)
                    .await
                {
                    Ok(response) => {
                        self.storage
                            .record_order_update(intent.id, &response.data)
                            .await?;
                        if matches!(
                            response.data.status,
                            tinkoff_rs::domain::OrderStatus::New
                                | tinkoff_rs::domain::OrderStatus::PartiallyFilled
                                | tinkoff_rs::domain::OrderStatus::Unknown
                                | tinkoff_rs::domain::OrderStatus::Pending
                        ) {
                            let Some(order_id) = response.data.exchange_order_id else {
                                continue;
                            };
                            self.api.cancel_order(&self.account_id, order_id).await?;
                            let after = self
                                .broker
                                .order_state(&self.account_id, intent.request_id)
                                .await?;
                            self.storage
                                .record_order_update(intent.id, &after.data)
                                .await?;
                        }
                    }
                    Err(Error::Status(status)) if status.code() == tonic::Code::NotFound => {
                        self.storage
                            .reject_intent(
                                intent.id,
                                "cancelled; request UUID confirmed absent at broker",
                            )
                            .await?;
                    }
                    Err(error) => {
                        self.storage
                            .mark_uncertain(
                                intent.id,
                                "cancellation awaiting broker reconciliation",
                            )
                            .await?;
                        return Err(error);
                    }
                }
            }
            if !self
                .storage
                .batch_intents(id)
                .await?
                .iter()
                .any(|i| active(i.status))
            {
                self.storage
                    .set_batch_status(
                        id,
                        RebalanceBatchStatus::Failed,
                        Some("cancelled by account controller"),
                    )
                    .await?;
            }
        }
        Ok(())
    }

    async fn account_instruments(
        &self,
        extra: Option<InstrumentId>,
    ) -> Result<Vec<InstrumentInfo>> {
        let mut universe =
            managed_universe(&self.storage, &self.account_id, &self.universe).await?;
        if let Some(id) = extra {
            if !universe.contains(&id) {
                universe.push(id);
            }
        }
        let mut instruments = Vec::new();
        for id in universe {
            instruments.push(load_instrument(&self.api, &self.broker, id).await?.0);
        }
        self.costs.apply(&mut instruments);
        Ok(instruments)
    }

    async fn mark_mixed(&mut self, id: Uuid, kind: &str) -> Result<()> {
        let (portfolio, decision) = self.control_observation().await?;
        self.sample_performance(&portfolio, &decision).await?;
        sqlx::query("UPDATE strategy_runs SET classification='mixed',interventions=CASE WHEN interventions @> $3::jsonb THEN interventions ELSE interventions || $3::jsonb END,updated_at=now() WHERE strategy_id=$1 AND account_id=$2 AND ended_at IS NULL")
            .bind(self.strategy_id.as_str()).bind(self.account_id.as_str()).bind(json!([{"id":id,"kind":kind}])).execute(self.storage.pool()).await?;
        Ok(())
    }

    async fn manual_order(&mut self, id: Uuid, payload: &Value) -> Result<Value> {
        self.pause().await?;
        if self.storage.load_batch(id).await?.is_some() {
            let batch = self.owned_batch(id).await?;
            if batch.event_id != format!("manual:{id}") {
                return Err(reject("command UUID already belongs to another operation"));
            }
            let (_, decision) = self.control_observation().await?;
            let outcome = if decision.mode == RiskMode::CloseOnly {
                self.executor.execute_batch_close_only(id).await?
            } else {
                self.executor.execute_batch(id).await?
            };
            return Ok(json!({"batch_id":id,"outcome":format!("{outcome:?}"),"enabled":false}));
        }
        let uid = InstrumentId::new(text_field(payload, "instrument_id")?)?;
        let side = match text_field(payload, "side")? {
            "buy" => OrderSide::Buy,
            "sell" => OrderSide::Sell,
            _ => return Err(reject("side must be buy or sell")),
        };
        let lots = payload
            .get("lots")
            .and_then(Value::as_i64)
            .filter(|v| *v > 0)
            .ok_or_else(|| reject("lots must be a positive integer"))?;
        let (kind, limit) = match text_field(payload, "order_type")? {
            "market" => (OrderKind::Market, None),
            "limit" => (
                OrderKind::Limit,
                Some(Price(positive_decimal(payload, "price")?)),
            ),
            _ => return Err(reject("order type must be market or limit")),
        };
        let (info, tick) = load_instrument(&self.api, &self.broker, uid.clone()).await?;
        if limit.is_some_and(|p| tick.is_some_and(|tick| p.0 % tick != Decimal::ZERO)) {
            return Err(reject(
                "limit price does not match the exchange price increment",
            ));
        }
        let availability = self.broker.trading_availability(&uid).await?.data;
        if !availability.api_trade_available
            || (kind == OrderKind::Market && !availability.market_order_available)
            || (kind == OrderKind::Limit && !availability.limit_order_available)
        {
            return Err(reject("requested order type is currently unavailable"));
        }
        self.mark_mixed(id, "manual_order").await?;
        self.cancel_batches(None).await?;
        self.require_settled().await?;
        let (portfolio, mut decision) = self.control_observation().await?;
        let mut instruments = self.account_instruments(Some(uid.clone())).await?;
        let prices = refresh_execution_data(&self.api, &mut instruments).await?;
        if prices.len() != instruments.len() {
            return Err(reject(
                "fresh execution prices or trading availability missing",
            ));
        }
        decision.sizing_capital = decision
            .sizing_capital
            .min(rub_equity(&portfolio)?.max(Decimal::ZERO));
        let current = position_lots(&portfolio, &info)?;
        let desired = match side {
            OrderSide::Buy => current.checked_add(lots),
            OrderSide::Sell => current.checked_sub(lots),
        }
        .ok_or_else(|| reject("requested position overflows"))?;
        let is_reduction = current != 0
            && (desired == 0
                || (current.signum() == desired.signum()
                    && desired.unsigned_abs() < current.unsigned_abs()));
        if decision.mode == RiskMode::CloseOnly && !is_reduction {
            return Err(reject(
                "risk block permits only reducing existing positions",
            ));
        }
        if desired < 0 && !info.short_enabled {
            return Err(reject("short trading is unavailable"));
        }
        let mut quantities = portfolio
            .positions
            .iter()
            .map(|p| (p.instrument_id.clone(), p.quantity))
            .collect::<HashMap<_, _>>();
        quantities.insert(
            uid.clone(),
            Decimal::from(desired) * Decimal::from(info.lot_size),
        );
        // Limits use the worse of the current and proposed fill price for this leg.
        let mut conservative_prices = prices.clone();
        if let Some(limit) = limit {
            let price = conservative_prices
                .get_mut(&uid)
                .ok_or_else(|| reject("current price missing"))?;
            price.0 = price.0.max(limit.0);
        }
        if !is_reduction {
            validate_exposure(
                &quantities,
                &piece_prices(&conservative_prices, &instruments),
                &decision,
            )?;
        }
        let values = piece_prices(&prices, &instruments);
        let now = Utc::now();
        let target = TargetPortfolio {
            batch_id: id,
            event_id: format!("manual:{id}"),
            strategy_id: self.strategy_id.clone(),
            account_id: self.account_id.clone(),
            source_as_of: now,
            generated_at: now,
            valid_until: now + Duration::minutes(5),
            weights: instruments
                .iter()
                .map(|i| TargetWeight {
                    instrument_id: i.instrument_id.clone(),
                    weight: if decision.sizing_capital > Decimal::ZERO {
                        quantities
                            .get(&i.instrument_id)
                            .copied()
                            .unwrap_or_default()
                            * values[&i.instrument_id].0
                            / decision.sizing_capital
                    } else {
                        Decimal::ZERO
                    },
                })
                .collect(),
        };
        let intents = manual_intents(&target, &info, side, current, desired, kind, limit)?;
        let batch = RebalanceBatch {
            id,
            event_id: target.event_id.clone(),
            strategy_id: self.strategy_id.clone(),
            account_id: self.account_id.clone(),
            target,
            effective_risk: decision.effective.clone(),
            sizing_capital: decision.sizing_capital,
            status: RebalanceBatchStatus::Planned,
            created_at: now,
        };
        sqlx::query("INSERT INTO account_managed_instruments(account_id,instrument_id,metadata) VALUES($1,$2,$3) ON CONFLICT DO NOTHING").bind(self.account_id.as_str()).bind(uid.as_str()).bind(json!({"origin":"manual","command_id":id})).execute(self.storage.pool()).await?;
        self.commit_control_batch(&PlannedBatch { batch, intents }, &decision)
            .await?;
        let outcome = if decision.mode == RiskMode::CloseOnly {
            self.executor.execute_batch_close_only(id).await?
        } else {
            self.executor.execute_batch(id).await?
        };
        Ok(json!({"batch_id":id,"outcome":format!("{outcome:?}"),"enabled":false}))
    }

    /// Непрерывные фьючерсы: когда наступил переход, позиция серии одним пакетом
    /// переносится из привязанного контракта в следующий; позиции в других
    /// контрактах серии (остаток прерванного переноса) закрываются. `true` —
    /// сессию нужно пересоздать, чтобы история и подписки привязались к новому
    /// контракту. Вне торгов перенос ждёт: до последнего дня торгов старого
    /// контракта ещё есть время.
    pub(super) async fn roll_futures(&mut self) -> Result<bool> {
        let rolls = self.api.series().due_rolls(Utc::now());
        let (portfolio, decision) = self.control_observation().await?;
        let targets: Vec<&InstrumentId> = rolls.iter().map(|r| &r.to.uid).collect();
        let leftovers: Vec<InstrumentId> = portfolio
            .positions
            .iter()
            .filter(|p| {
                !p.quantity.is_zero()
                    && !targets.contains(&&p.instrument_id)
                    && self
                        .api
                        .series()
                        .other_contract_series(&p.instrument_id)
                        .is_some()
            })
            .map(|p| p.instrument_id.clone())
            .collect();
        if rolls.is_empty() && leftovers.is_empty() {
            return Ok(false);
        }
        let close_only = decision.mode == RiskMode::CloseOnly;
        // Свойства и текущие лоты всех участников переноса.
        let mut infos: HashMap<InstrumentId, InstrumentInfo> = HashMap::new();
        let mut held: HashMap<InstrumentId, i64> = HashMap::new();
        for id in rolls
            .iter()
            .flat_map(|r| [r.series.clone(), r.to.uid.clone()])
            .chain(leftovers.iter().cloned())
        {
            let (info, _) = load_instrument(&self.api, &self.broker, id.clone()).await?;
            held.insert(id.clone(), position_lots(&portfolio, &info)?);
            infos.insert(id, info);
        }
        for roll in &rolls {
            info!(series = %roll.series, from = %roll.from.ticker, to = %roll.to.ticker, lots = held[&roll.series], "futures roll is due");
        }
        for uid in &leftovers {
            warn!(instrument_id = %uid, lots = held[uid], "closing a position left in another contract of a futures series");
        }
        let legs: Vec<(InstrumentInfo, i64, i64)> =
            roll_legs(&rolls, &leftovers, &held, close_only)
                .into_iter()
                .map(|(id, current, desired)| (infos[&id].clone(), current, desired))
                .collect();
        if legs.is_empty() {
            return Ok(!rolls.is_empty());
        }
        let mut instruments: Vec<InstrumentInfo> =
            legs.iter().map(|(info, ..)| info.clone()).collect();
        let prices = refresh_execution_data(&self.api, &mut instruments).await?;
        if prices.len() != instruments.len() {
            info!("futures roll waits for trading in both contracts");
            return Ok(false);
        }
        let event_id = format!(
            "roll:{}",
            legs.iter()
                .map(|(info, current, desired)| format!(
                    "{}:{current}>{desired}",
                    info.instrument_id
                ))
                .collect::<Vec<_>>()
                .join(",")
        );
        let id = Uuid::new_v5(
            &Uuid::NAMESPACE_URL,
            format!("{}:{event_id}", self.account_id).as_bytes(),
        );
        if self.storage.load_batch(id).await?.is_none() {
            let now = Utc::now();
            let values = piece_prices(&prices, &instruments);
            let capital = decision.sizing_capital.max(Decimal::ONE);
            let target = TargetPortfolio {
                batch_id: id,
                event_id: event_id.clone(),
                strategy_id: self.strategy_id.clone(),
                account_id: self.account_id.clone(),
                source_as_of: now,
                generated_at: now,
                valid_until: now + Duration::minutes(5),
                weights: legs
                    .iter()
                    .map(|(info, _, desired)| TargetWeight {
                        instrument_id: info.instrument_id.clone(),
                        weight: Decimal::from(*desired)
                            * Decimal::from(info.lot_size)
                            * values[&info.instrument_id].0
                            / capital,
                    })
                    .collect(),
            };
            let mut intents = Vec::new();
            for (info, current, desired) in &legs {
                let side = if desired > current {
                    OrderSide::Buy
                } else {
                    OrderSide::Sell
                };
                intents.extend(manual_intents(
                    &target,
                    info,
                    side,
                    *current,
                    *desired,
                    OrderKind::Market,
                    None,
                )?);
            }
            // Протокол исполнения: все сокращения раньше открытий.
            intents.sort_by_key(|intent| intent.execution_phase != ExecutionPhase::Reduction);
            for (index, intent) in intents.iter_mut().enumerate() {
                intent.leg_index = i32::try_from(index).map_err(|_| reject("too many legs"))?;
            }
            for (info, ..) in &legs {
                sqlx::query("INSERT INTO account_managed_instruments(account_id,instrument_id,metadata) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
                    .bind(self.account_id.as_str())
                    .bind(info.instrument_id.as_str())
                    .bind(json!({"origin":"futures-roll"}))
                    .execute(self.storage.pool())
                    .await?;
            }
            let batch = RebalanceBatch {
                id,
                event_id,
                strategy_id: self.strategy_id.clone(),
                account_id: self.account_id.clone(),
                target,
                effective_risk: decision.effective.clone(),
                sizing_capital: decision.sizing_capital,
                status: RebalanceBatchStatus::Planned,
                created_at: now,
            };
            self.commit_control_batch(&PlannedBatch { batch, intents }, &decision)
                .await?;
        }
        let outcome = if close_only {
            self.executor.execute_batch_close_only(id).await?
        } else {
            self.executor.execute_batch(id).await?
        };
        info!(batch_id = %id, ?outcome, "futures roll batch processed");
        Ok(matches!(
            outcome,
            tinkoff_rs::engine::BatchExecutionOutcome::Completed
        ) && !rolls.is_empty())
    }

    async fn commit_control_batch(
        &mut self,
        plan: &PlannedBatch,
        decision: &RiskDecision,
    ) -> Result<()> {
        self.runtime_lease.check().await?;
        let state = self
            .storage
            .load_portfolio_strategy_state(&self.strategy_id)
            .await?
            .ok_or_else(|| Error::Storage("strategy state has not been initialized".into()))?;
        self.storage
            .commit_portfolio_event(
                &self.strategy_id,
                &plan.batch.event_id,
                &state,
                Some(&plan.batch),
                &plan.intents,
                Some(&decision.state),
            )
            .await?;
        Ok(())
    }

    async fn approve_batch(&mut self, command: Uuid, payload: &Value) -> Result<Value> {
        let batch = self.owned_batch(uuid_field(payload, "batch_id")?).await?;
        if let Some(replacement) = self.storage.load_batch(command).await? {
            if replacement.strategy_id == self.strategy_id
                && replacement.event_id == format!("approval-refresh:{command}")
            {
                return Ok(
                    json!({"requires_confirmation":true,"batch_id":command,"replaced_batch_id":batch.id}),
                );
            }
        }
        let previously_approved: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM batch_controls WHERE batch_id=$1 AND status='approved' AND preview->>'command_id'=$2)")
            .bind(batch.id).bind(command.to_string()).fetch_one(self.storage.pool()).await?;
        if previously_approved {
            let (_, decision) = self.control_observation().await?;
            let outcome = if decision.mode == RiskMode::CloseOnly {
                self.executor.execute_batch_close_only(batch.id).await?
            } else {
                self.executor.execute_batch(batch.id).await?
            };
            return Ok(json!({"batch_id":batch.id,"outcome":format!("{outcome:?}")}));
        }
        let settings = self.control_settings().await?;
        if !settings.enabled {
            return Err(reject("enable the strategy before approving a plan"));
        }
        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM batch_controls WHERE batch_id=$1")
                .bind(batch.id)
                .fetch_optional(self.storage.pool())
                .await?;
        if status.as_deref() != Some("awaiting") {
            return Err(reject("plan is not awaiting approval"));
        }
        if Utc::now() > batch.target.valid_until {
            self.cancel_batches(Some(batch.id)).await?;
            return Err(reject("plan has expired"));
        }
        let old = self.storage.batch_intents(batch.id).await?;
        if old.iter().any(|i| i.status != IntentStatus::Pending) {
            return Err(reject("plan has already started execution"));
        }
        let (portfolio, mut decision) = self.control_observation().await?;
        if decision.mode != RiskMode::Normal {
            return Err(reject("risk block prevents approval"));
        }
        validate_dedicated_portfolio(&portfolio, &self.universe)?;
        decision.sizing_capital = decision
            .sizing_capital
            .min(rub_equity(&portfolio)?.max(Decimal::ZERO));
        let mut instruments = self.account_instruments(None).await?;
        instruments.retain(|i| self.universe.contains(&i.instrument_id));
        let prices = refresh_execution_data(&self.api, &mut instruments).await?;
        let plan = TargetPlanner.plan(
            &batch.target,
            &PlanningContext {
                portfolio,
                instruments,
                prices,
                risk: decision.clone(),
                min_trade_fraction: self.costs.min_trade_fraction,
            },
        )?;
        self.mark_mixed(command, "approve_batch").await?;
        if !same_plan(
            &old.iter().map(|i| i.intent.clone()).collect::<Vec<_>>(),
            &plan.intents,
        ) {
            self.cancel_batches(Some(batch.id)).await?;
            let mut replacement = plan;
            replacement.batch.id = command;
            replacement.batch.target.batch_id = command;
            replacement.batch.event_id = format!("approval-refresh:{command}");
            replacement.batch.target.event_id = replacement.batch.event_id.clone();
            for intent in &mut replacement.intents {
                intent.batch_id = command;
                intent.event_id.clone_from(&replacement.batch.event_id);
            }
            self.commit_control_batch(&replacement, &decision).await?;
            return Ok(
                json!({"requires_confirmation":true,"batch_id":command,"replaced_batch_id":batch.id}),
            );
        }
        // Bind approval to the exact current control revision. A concurrent pause
        // invalidates it before the executor's atomic claim.
        let changed=sqlx::query("UPDATE batch_controls SET status='approved',revision=$2,preview=$3,updated_at=now() WHERE batch_id=$1 AND status='awaiting' AND EXISTS(SELECT 1 FROM strategy_controls WHERE strategy_id=$4 AND enabled AND revision=$2)")
            .bind(batch.id).bind(settings.revision).bind(json!({"command_id":command,"revalidated_at":Utc::now()})).bind(self.strategy_id.as_str()).execute(self.storage.pool()).await?.rows_affected();
        if changed != 1 {
            return Err(reject("controls changed while validating approval"));
        }
        let outcome = self.executor.execute_batch(batch.id).await?;
        Ok(json!({"batch_id":batch.id,"outcome":format!("{outcome:?}")}))
    }

    async fn change_budget(&mut self, id: Uuid, payload: &Value) -> Result<Value> {
        let amount = positive_decimal(payload, "amount")?;
        let settings = self.control_settings().await?;
        let previous = settings.budget.unwrap_or(self.risk.state().budget_rub);
        let budget = next_budget(previous, text_field(payload, "operation")?, amount)?;
        // Reject an impossible request before cancelling any existing plan.
        self.check_budget_fits(budget).await?;
        self.cancel_batches(None).await?;
        self.require_settled().await?;
        // Cancellation may have raced with a fill, so validate the settled state again.
        let minimum_budget = self.check_budget_fits(budget).await?;
        self.mark_mixed(id, "budget").await?;
        // The journal shows "was -> now" and the computed difference of a set request.
        let delta = budget
            .checked_sub(previous)
            .ok_or_else(|| reject("budget difference overflowed"))?;
        let result = json!({"execution_budget":budget.to_string(),"previous_budget":previous.to_string(),"delta":delta.to_string(),"minimum_budget":minimum_budget.to_string(),"risk_epoch_preserved":true});
        let mut tx = self.storage.pool().begin().await?;
        sqlx::query("UPDATE strategy_controls SET execution_budget=$2,revision=revision+1,updated_at=now() WHERE strategy_id=$1").bind(self.strategy_id.as_str()).bind(budget).execute(&mut *tx).await?;
        sqlx::query("UPDATE control_commands SET status='succeeded',result=$2,updated_at=now() WHERE id=$1 AND status='running'").bind(id).bind(&result).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(result)
    }

    async fn check_budget_fits(&mut self, budget: Decimal) -> Result<Decimal> {
        let (portfolio, decision) = self.control_observation().await?;
        let mut instruments = self.account_instruments(None).await?;
        let prices = if portfolio_is_flat(&portfolio) {
            HashMap::new()
        } else {
            refresh_execution_data(&self.api, &mut instruments).await?
        };
        let quantities = portfolio
            .positions
            .iter()
            .map(|p| (p.instrument_id.clone(), p.quantity))
            .collect();
        let minimum = minimum_budget(&quantities, &piece_prices(&prices, &instruments), &decision)?;
        let pnl = decision.strategy_equity - decision.state.allocated_principal;
        let minimum_budget = (minimum - pnl.min(Decimal::ZERO)).max(Decimal::ZERO);
        if budget < minimum_budget {
            return Err(reject(&format!(
                "budget is below current exposure; minimum_budget={minimum_budget}"
            )));
        }
        if budget > rub_equity(&portfolio)?.max(Decimal::ZERO) {
            return Err(reject(
                "budget exceeds account equity; fund the account first",
            ));
        }
        Ok(minimum_budget)
    }

    async fn pay_in(&mut self, id: Uuid, payload: &Value) -> Result<Value> {
        let amount = positive_decimal(payload, "amount")?;
        let money = Money {
            currency: "rub".into(),
            amount: Price(amount),
        }
        .to_proto()?;
        let (portfolio, decision) = self.control_observation().await?;
        self.sample_performance(&portfolio, &decision).await?;
        self.runtime_lease.check().await?;
        // SandboxPayIn has no idempotency key. Persist uncertainty BEFORE the RPC;
        // on a lost response or crash the controller never automatically retries.
        let count=sqlx::query("UPDATE control_commands SET status='uncertain',result=$2,updated_at=now() WHERE id=$1 AND status='running'")
            .bind(id).bind(json!({"message":"pay-in submitted once; if response is lost reconcile account operations before any new deposit","amount":amount.to_string()})).execute(self.storage.pool()).await?.rows_affected();
        if count != 1 {
            return Err(reject("deposit is already submitted"));
        }
        let response = self
            .api
            .sandbox()?
            .raw()
            .sandbox()
            .sandbox_pay_in(SandboxPayInRequest {
                account_id: self.account_id.to_string(),
                amount: Some(money),
            })
            .await;
        match response {
            Ok(response) => {
                let result = json!({"deposited":amount.to_string(),"balance":response.into_inner().balance.map(Money::from_proto).transpose()?.map(|m|m.amount.0.to_string()),"execution_budget_changed":false});
                sqlx::query("UPDATE control_commands SET status='succeeded',result=$2,updated_at=now() WHERE id=$1 AND status='uncertain'").bind(id).bind(&result).execute(self.storage.pool()).await?;
                Ok(result)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(super) async fn protective_close(&mut self, cleanup: bool) -> Result<()> {
        self.executor.recover_pending_batches_close_only().await?;
        let (portfolio, mut decision) = self.control_observation().await?;
        self.sample_performance(&portfolio, &decision).await?;
        if self
            .storage
            .has_unfinished_batches(&self.account_id)
            .await?
            || self.storage.has_pending_orders(&self.account_id).await?
        {
            return Ok(());
        }
        if portfolio_is_flat(&portfolio) {
            if cleanup {
                sqlx::query("WITH ended AS (UPDATE strategy_runs SET ended_at=now(),updated_at=now() WHERE strategy_id=$1 AND ended_at IS NULL RETURNING id) UPDATE strategy_controls SET cleanup_requested=false,updated_at=now() WHERE strategy_id=$1").bind(self.strategy_id.as_str()).execute(self.storage.pool()).await?;
            }
            self.update_service_need(&portfolio).await?;
            return Ok(());
        }
        let mut instruments = self.account_instruments(None).await?;
        validate_dedicated_portfolio(
            &portfolio,
            &instruments
                .iter()
                .map(|i| i.instrument_id.clone())
                .collect::<Vec<_>>(),
        )?;
        // Unheld instruments must not block a necessary close on an active one.
        instruments.retain(|i| {
            portfolio
                .positions
                .iter()
                .any(|p| p.instrument_id == i.instrument_id && !p.quantity.is_zero())
        });
        let prices = refresh_execution_data(&self.api, &mut instruments).await?;
        if prices.len() != instruments.len() {
            return Err(reject("protective close waits for trading to open"));
        }
        decision.mode = RiskMode::CloseOnly;
        let now = Utc::now();
        let target = TargetPortfolio {
            batch_id: Uuid::new_v4(),
            event_id: format!(
                "{}:{}",
                if cleanup { "cleanup" } else { "risk" },
                Uuid::new_v4()
            ),
            strategy_id: self.strategy_id.clone(),
            account_id: self.account_id.clone(),
            source_as_of: now,
            generated_at: now,
            valid_until: now + Duration::minutes(5),
            weights: instruments
                .iter()
                .map(|i| TargetWeight {
                    instrument_id: i.instrument_id.clone(),
                    weight: Decimal::ZERO,
                })
                .collect(),
        };
        let mut portfolio = portfolio;
        portfolio.positions.retain(|p| !p.quantity.is_zero());
        let plan = TargetPlanner.plan(
            &target,
            &PlanningContext {
                portfolio,
                instruments,
                prices,
                risk: decision.clone(),
                // Защитное закрытие обнуляет позиции: порог сделки к нему не относится.
                min_trade_fraction: Decimal::ZERO,
            },
        )?;
        self.commit_control_batch(&plan, &decision).await?;
        self.executor
            .execute_batch_close_only(plan.batch.id)
            .await?;
        Ok(())
    }

    async fn update_service_need(&self, portfolio: &PortfolioSnapshot) -> Result<()> {
        let active_orders = self
            .storage
            .has_unfinished_batches(&self.account_id)
            .await?
            || self.storage.has_pending_orders(&self.account_id).await?;
        sqlx::query("UPDATE strategy_controls SET needs_service=($2 OR EXISTS(SELECT 1 FROM control_commands WHERE COALESCE(execution_strategy_id,strategy_id)=$1 AND status IN ('queued','running'))),updated_at=now() WHERE strategy_id=$1")
            .bind(self.strategy_id.as_str()).bind(!portfolio_is_flat(portfolio)||active_orders).execute(self.storage.pool()).await?;
        Ok(())
    }

    async fn create_run(
        &self,
        portfolio: &PortfolioSnapshot,
        decision: &RiskDecision,
        explicit: Option<Uuid>,
    ) -> Result<Uuid> {
        let equity = rub_equity(portfolio)?;
        let settings = self.control_settings().await?;
        let principal = settings
            .budget
            .unwrap_or(decision.state.budget_rub)
            .min(equity);
        let id = explicit.unwrap_or_else(Uuid::new_v4);
        let mut tx = self.storage.pool().begin().await?;
        if let Some(existing) =
            sqlx::query_scalar::<_, Uuid>("SELECT id FROM strategy_runs WHERE id=$1")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
        {
            return Ok(existing);
        }
        let active=sqlx::query("SELECT id,strategy_id FROM strategy_runs WHERE account_id=$1 AND ended_at IS NULL FOR UPDATE").bind(self.account_id.as_str()).fetch_optional(&mut *tx).await?;
        if let Some(active) = active {
            let strategy: String = active.try_get("strategy_id")?;
            if explicit.is_none() && strategy == self.strategy_id.as_str() {
                return Ok(active.try_get("id")?);
            }
            if principal <= Decimal::ZERO {
                return Err(reject(
                    "a statistical run requires positive funded principal",
                ));
            }
            sqlx::query("UPDATE strategy_runs SET ended_at=now(),updated_at=now() WHERE account_id=$1 AND ended_at IS NULL").bind(self.account_id.as_str()).execute(&mut *tx).await?;
        }
        if principal <= Decimal::ZERO {
            return Err(reject(
                "a statistical run requires positive funded principal",
            ));
        }
        let classification = if settings.mode == "confirm" || !portfolio_is_flat(portfolio) {
            "mixed"
        } else {
            "clean"
        };
        sqlx::query("INSERT INTO strategy_runs(id,strategy_id,account_id,classification,anchor_equity,principal,anchor_external_cash_flows,last_equity,last_external_cash_flows) VALUES($1,$2,$3,$4,$5,$6,$7,$5,$7)")
            .bind(id).bind(self.strategy_id.as_str()).bind(self.account_id.as_str()).bind(classification).bind(equity).bind(principal).bind(decision.state.external_cash_flows_since_anchor).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(id)
    }

    pub(super) async fn sample_performance(
        &mut self,
        portfolio: &PortfolioSnapshot,
        decision: &RiskDecision,
    ) -> Result<()> {
        let has_run: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM strategy_runs WHERE strategy_id=$1 AND account_id=$2 AND ended_at IS NULL)")
            .bind(self.strategy_id.as_str()).bind(self.account_id.as_str()).fetch_one(self.storage.pool()).await?;
        if !has_run {
            // A final flat observation ends cleanup statistics. Do not create a
            // new clean run merely because the paused risk service ticks once
            // more before its supervisor stops it.
            let settled_run: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM strategy_runs WHERE strategy_id=$1 AND ended_at IS NOT NULL) AND EXISTS(SELECT 1 FROM strategy_controls WHERE strategy_id=$1 AND NOT enabled AND NOT cleanup_requested) AND NOT EXISTS(SELECT 1 FROM control_commands WHERE COALESCE(execution_strategy_id,strategy_id)=$1 AND status IN ('queued','running'))")
                .bind(self.strategy_id.as_str()).fetch_one(self.storage.pool()).await?;
            if settled_run {
                self.update_service_need(portfolio).await?;
                return Ok(());
            }
        }
        // A bankrupt existing run must record its loss rather than retain the
        // previous positive equity. Only a new run requires positive capital.
        if rub_equity(portfolio)? <= Decimal::ZERO && !has_run {
            self.update_service_need(portfolio).await?;
            return Ok(());
        }
        let run = self.create_run(portfolio, decision, None).await?;
        let equity = rub_equity(portfolio)?;
        let flow = decision.state.external_cash_flows_since_anchor;
        let mut tx = self.storage.pool().begin().await?;
        sqlx::query("INSERT INTO performance_samples(run_id,observed_at,equity,external_cash_flows) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING")
            .bind(run).bind(portfolio.observed_at).bind(equity).bind(flow).execute(&mut *tx).await?;
        sqlx::query("UPDATE strategy_runs SET last_equity=$2,last_external_cash_flows=$3,peak_pnl=GREATEST(peak_pnl,$2-anchor_equity-($3-anchor_external_cash_flows)),max_drawdown_percent=GREATEST(max_drawdown_percent,(GREATEST(peak_pnl,$2-anchor_equity-($3-anchor_external_cash_flows))-($2-anchor_equity-($3-anchor_external_cash_flows)))*100/(principal+GREATEST(peak_pnl,0))),updated_at=now() WHERE id=$1")
            .bind(run).bind(equity).bind(flow).execute(&mut *tx).await?;
        tx.commit().await?;
        self.update_service_need(portfolio).await?;
        Ok(())
    }
}

fn reject(message: &str) -> Error {
    Error::RiskRejected(message.to_owned())
}
fn text_field<'a>(payload: &'a Value, key: &str) -> Result<&'a str> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| reject(&format!("{key} is required")))
}
fn uuid_field(payload: &Value, key: &str) -> Result<Uuid> {
    Uuid::parse_str(text_field(payload, key)?).map_err(|_| reject("invalid UUID"))
}
fn positive_decimal(payload: &Value, key: &str) -> Result<Decimal> {
    let amount = text_field(payload, key)?
        .parse::<Decimal>()
        .map_err(|_| reject("amount must be a decimal string"))?;
    if amount <= Decimal::ZERO || amount.scale() > 9 {
        return Err(reject(
            "amount must be positive with at most 9 decimal places",
        ));
    }
    Ok(amount)
}
fn protective_event(event: &str) -> bool {
    event.starts_with("risk:") || event.starts_with("cleanup:")
}
fn active(status: IntentStatus) -> bool {
    matches!(
        status,
        IntentStatus::Pending
            | IntentStatus::Submitting
            | IntentStatus::Accepted
            | IntentStatus::Uncertain
    )
}
fn next_budget(current: Decimal, operation: &str, amount: Decimal) -> Result<Decimal> {
    let value = match operation {
        "set" => Some(amount),
        "add" => current.checked_add(amount),
        "subtract" => current.checked_sub(amount),
        _ => None,
    }
    .filter(|v| *v > Decimal::ZERO)
    .ok_or_else(|| reject("operation must keep a positive budget"))?;
    Ok(value)
}

/// Свойства инструмента любого поддерживаемого типа (см. [`instruments::describe`]).
async fn load_instrument(
    api: &VenueApi,
    broker: &VenueBroker,
    id: InstrumentId,
) -> Result<(InstrumentInfo, Option<Decimal>)> {
    instruments::describe(api, broker, &id)
        .await
        .map_err(|error| reject(&error.to_string()))
}

/// Рублёвая стоимость одной бумаги (контракта) вместо котировки: для облигаций
/// и фьючерсов котировка — проценты номинала и пункты.
fn piece_prices(
    prices: &HashMap<InstrumentId, Price>,
    instruments: &[InstrumentInfo],
) -> HashMap<InstrumentId, Price> {
    instruments
        .iter()
        .filter_map(|info| {
            prices
                .get(&info.instrument_id)
                .map(|price| (info.instrument_id.clone(), Price(info.piece_value(price.0))))
        })
        .collect()
}

fn position_lots(portfolio: &PortfolioSnapshot, info: &InstrumentInfo) -> Result<i64> {
    let quantity = portfolio
        .positions
        .iter()
        .filter(|p| p.instrument_id == info.instrument_id)
        .map(|p| p.quantity)
        .sum::<Decimal>();
    let value = quantity / Decimal::from(info.lot_size);
    if value.fract() != Decimal::ZERO {
        return Err(reject("position is not divisible by lot size"));
    }
    value
        .to_i64()
        .filter(|v| *v != i64::MIN)
        .ok_or_else(|| reject("position lots overflow"))
}

fn minimum_budget(
    quantities: &HashMap<InstrumentId, Decimal>,
    prices: &HashMap<InstrumentId, Price>,
    decision: &RiskDecision,
) -> Result<Decimal> {
    let mut gross = Decimal::ZERO;
    let mut net = Decimal::ZERO;
    let mut minimum = Decimal::ZERO;
    for (uid, quantity) in quantities {
        if quantity.is_zero() {
            continue;
        }
        let price = prices
            .get(uid)
            .ok_or_else(|| reject("fresh price missing for a held position"))?;
        let value = *quantity * price.0;
        gross += value.abs();
        net += value;
        minimum = minimum.max(value.abs() / decision.effective.max_instrument_exposure);
    }
    Ok(minimum
        .max(gross / decision.effective.max_gross_exposure)
        .max(net.abs() / decision.effective.max_net_exposure))
}
fn validate_exposure(
    quantities: &HashMap<InstrumentId, Decimal>,
    prices: &HashMap<InstrumentId, Price>,
    decision: &RiskDecision,
) -> Result<()> {
    let minimum = minimum_budget(quantities, prices, decision)?;
    if minimum > decision.sizing_capital {
        return Err(reject(&format!(
            "order exceeds execution budget or exposure limits; required_capital={minimum}"
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn manual_intents(
    target: &TargetPortfolio,
    info: &InstrumentInfo,
    side: OrderSide,
    current: i64,
    desired: i64,
    kind: OrderKind,
    limit: Option<Price>,
) -> Result<Vec<OrderIntent>> {
    if desired == i64::MIN {
        return Err(reject("target lots overflow"));
    }
    let mut steps = Vec::new();
    if current != 0 && (desired == 0 || current.signum() != desired.signum()) {
        steps.push((current, 0, ExecutionPhase::Reduction));
        if desired != 0 {
            steps.push((0, desired, ExecutionPhase::Opening));
        }
    } else {
        steps.push((
            current,
            desired,
            if desired.unsigned_abs() < current.unsigned_abs() {
                ExecutionPhase::Reduction
            } else {
                ExecutionPhase::Opening
            },
        ));
    }
    let mut result = Vec::new();
    for (index, (source, target_lots, phase)) in steps.into_iter().enumerate() {
        let lots = source
            .checked_sub(target_lots)
            .and_then(i64::checked_abs)
            .ok_or_else(|| reject("lots overflow"))?;
        let mut intent = OrderIntent::market(
            target.batch_id,
            i32::try_from(index).map_err(|_| reject("too many legs"))?,
            target.event_id.clone(),
            target.strategy_id.clone(),
            target.account_id.clone(),
            info.instrument_id.clone(),
            side,
            lots,
            target_lots,
            info.lot_size,
            phase,
            if target_lots < 0 {
                MarginConsent::UncoveredAllowed
            } else {
                MarginConsent::CoveredOnly
            },
            target.generated_at,
        );
        intent.kind = kind;
        intent.limit_price = limit;
        result.push(intent);
    }
    Ok(result)
}
/// Ноги переноса непрерывных фьючерсов: позиция серии (привязанный контракт)
/// закрывается, в следующий контракт открывается тот же объём, если он ещё не
/// открыт; остатки в других контрактах закрываются. При
/// риск-блокировке позиция только закрывается. Возвращает
/// (идентификатор, текущие лоты, целевые лоты) без пустых ног.
pub(super) fn roll_legs(
    rolls: &[instruments::Roll],
    leftovers: &[InstrumentId],
    held: &HashMap<InstrumentId, i64>,
    close_only: bool,
) -> Vec<(InstrumentId, i64, i64)> {
    let lots = |id: &InstrumentId| held.get(id).copied().unwrap_or_default();
    let mut legs = Vec::new();
    for roll in rolls {
        let (series, moved) = (lots(&roll.series), lots(&roll.to.uid));
        // Позиция, уже открытая в следующем контракте (прерванный перенос), не
        // наращивается: старый контракт только закрывается.
        let target = if moved == 0 { series } else { moved };
        legs.push((roll.series.clone(), series, 0));
        legs.push((
            roll.to.uid.clone(),
            moved,
            if close_only { 0 } else { target },
        ));
    }
    for uid in leftovers {
        legs.push((uid.clone(), lots(uid), 0));
    }
    legs.retain(|(_, current, desired)| current != desired);
    legs
}

fn same_plan(left: &[OrderIntent], right: &[OrderIntent]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(a, b)| {
            a.instrument_id == b.instrument_id
                && a.side == b.side
                && a.lots == b.lots
                && a.target_lots == b.target_lots
                && a.execution_phase == b.execution_phase
                && a.kind == b.kind
                && a.limit_price == b.limit_price
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tinkoff_rs::domain::StrategyId;
    #[test]
    fn budget_changes_are_positive_and_checked() {
        assert_eq!(
            next_budget(Decimal::from(100), "subtract", Decimal::from(30)).unwrap(),
            Decimal::from(70)
        );
        assert!(next_budget(Decimal::from(100), "subtract", Decimal::from(100)).is_err());
        assert!(next_budget(Decimal::MAX, "add", Decimal::ONE).is_err());
    }
    #[test]
    fn deposit_rejects_negative_and_excess_precision() {
        assert!(positive_decimal(&json!({"amount":"-1"}), "amount").is_err());
        assert!(positive_decimal(&json!({"amount":"0.0000000001"}), "amount").is_err());
        assert_eq!(
            positive_decimal(&json!({"amount":"10.25"}), "amount").unwrap(),
            Decimal::new(1025, 2)
        );
    }
    #[test]
    fn manual_crossing_preserves_exact_lots_and_limit() {
        let now = Utc::now();
        let id = Uuid::new_v4();
        let uid = InstrumentId::new("uid").unwrap();
        let target = TargetPortfolio {
            batch_id: id,
            event_id: format!("manual:{id}"),
            strategy_id: StrategyId::new("s").unwrap(),
            account_id: AccountId::new("a").unwrap(),
            source_as_of: now,
            generated_at: now,
            valid_until: now + Duration::minutes(5),
            weights: vec![],
        };
        let info = InstrumentInfo {
            instrument_id: uid,
            lot_size: 10,
            currency: "rub".into(),
            exchange: "moex".into(),
            short_enabled: true,
            api_trade_available: true,
            market_order_available: true,
            kind: tinkoff_rs::engine::InstrumentKind::Share,
            price_multiplier: Decimal::ONE,
            accrued_interest: Decimal::ZERO,
            commission_rate: Decimal::ZERO,
        };
        let legs = manual_intents(
            &target,
            &info,
            OrderSide::Sell,
            3,
            -2,
            OrderKind::Limit,
            Some(Price(Decimal::from(100))),
        )
        .unwrap();
        assert_eq!(legs.len(), 2);
        assert_eq!(legs.iter().map(|i| i.lots).sum::<i64>(), 5);
        assert_eq!(legs[0].execution_phase, ExecutionPhase::Reduction);
        assert_eq!(legs[1].target_lots, -2);
        assert!(legs.iter().all(
            |i| i.kind == OrderKind::Limit && i.limit_price == Some(Price(Decimal::from(100)))
        ));
        let mut changed = legs.clone();
        changed[0].target_lots = 1;
        assert!(!same_plan(&legs, &changed));
        assert!(same_plan(&legs, &legs));
    }
}
