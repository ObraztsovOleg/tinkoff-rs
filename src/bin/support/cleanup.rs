//! Audited maintenance liquidation, independent of strategy-generated signals.
use super::{
    BatchExecutionOutcome, BatchExecutor, BotConfig, Decimal, Duration, Error, PlanningContext,
    Portfolio, PortfolioStrategy, Result, RiskMode, RiskObservation, RuntimeIds, StdDuration,
    Storage, Utc, WeightedEnsembleStrategy, connect_storage, connect_venue, control,
    current_moex_session_date, ensure_account_exists, ensure_risk_owner, info, initialize_risk,
    inspect_instruments, portfolio_is_flat, refresh_execution_data, rub_equity,
    scan_external_cash_flows, validate_dedicated_portfolio,
};
use tinkoff_rs::engine::{TargetPlanner, TargetPortfolio, TargetWeight};
use uuid::Uuid;

pub(super) async fn run(config: &BotConfig, ids: &RuntimeIds, reason: &str) -> Result<()> {
    if reason.trim().is_empty() {
        return Err(Error::Configuration(
            "cleanup requires a nonempty reason".to_owned(),
        ));
    }
    let storage = connect_storage(config).await?;
    storage.migrate().await?;
    let mut lease = storage.acquire_runtime_lease(&ids.account_id).await?;
    super::venue::ensure_not_halted(&storage, config.contour).await?;
    let (api, broker) = connect_venue(config).await?;
    ensure_account_exists(&broker, &ids.account_id).await?;
    let strategy = WeightedEnsembleStrategy::new(
        ids.strategy_id.clone(),
        ids.account_id.clone(),
        ids.universe.clone(),
        config.strategies.clone(),
    )?;
    let requirements = strategy.data_requirements();
    storage
        .register_portfolio_strategy(
            &ids.strategy_id,
            &ids.account_id,
            strategy.kind(),
            &strategy.config()?,
        )
        .await?;
    let state = storage
        .load_portfolio_strategy_state(&ids.strategy_id)
        .await?
        .unwrap_or(strategy.state()?);
    let managed = control::managed_universe(&storage, &ids.account_id, &ids.universe).await?;
    let mut instruments = inspect_instruments(
        &api,
        &broker,
        &managed,
        &requirements,
        false,
        &std::collections::HashSet::new(),
    )
    .await?;
    let executor = BatchExecutor::new(
        ids.account_id.clone(),
        ids.strategy_id.clone(),
        broker.clone(),
        broker.clone(),
        storage.clone(),
    );
    if let Some(risk) = storage.load_risk_state(&ids.account_id).await? {
        ensure_risk_owner(&risk, ids)?;
    }
    if storage.has_unfinished_batches(&ids.account_id).await?
        && storage
            .pending_batch_ids(&ids.account_id, &ids.strategy_id)
            .await?
            .is_empty()
    {
        return Err(Error::RiskRejected(
            "cleanup cannot reconcile batches owned by another strategy".to_owned(),
        ));
    }
    // Suppress every ordinary pending leg before recovery; a confirmation gate
    // must not keep cleanup waiting forever or allow a reduction to be resubmitted.
    sqlx::query("INSERT INTO batch_controls(batch_id,status,revision) SELECT b.id,'rejected',0 FROM rebalance_batches b WHERE b.account_id=$1 AND b.strategy_id=$2 AND b.status NOT IN ('completed','failed') AND b.event_id NOT LIKE 'cleanup:%' AND b.event_id NOT LIKE 'risk:%' ON CONFLICT(batch_id) DO UPDATE SET status='rejected' WHERE batch_controls.revision<>-1")
        .bind(ids.account_id.as_str()).bind(ids.strategy_id.as_str()).execute(storage.pool()).await?;
    for order in api.active_orders(&ids.account_id).await? {
        lease.check().await?;
        api.cancel_order(&ids.account_id, order.order_id).await?;
    }
    for _ in 0..30 {
        lease.check().await?;
        executor.recover_pending_batches_close_only().await?;
        if !storage.has_unfinished_batches(&ids.account_id).await?
            && !storage.has_pending_orders(&ids.account_id).await?
        {
            break;
        }
        tokio::time::sleep(StdDuration::from_secs(1)).await;
    }
    if storage.has_unfinished_batches(&ids.account_id).await?
        || storage.has_pending_orders(&ids.account_id).await?
    {
        return Err(Error::RiskRejected(
            "cleanup deferred: orders still require reconciliation".to_owned(),
        ));
    }
    let portfolio = broker.portfolio(&ids.account_id).await?.data;
    validate_dedicated_portfolio(&portfolio, &managed)?;
    if portfolio_is_flat(&portfolio) {
        if !api.active_orders(&ids.account_id).await?.is_empty() {
            return Err(Error::RiskRejected(
                "cleanup deferred: cancellation not yet confirmed".to_owned(),
            ));
        }
        finalize_performance(&storage, &api, &portfolio).await?;
        info!(account_id = %ids.account_id, %reason, "cleanup complete; account is flat");
        return Ok(());
    }
    let prices = refresh_execution_data(&api, &mut instruments).await?;
    if instruments
        .iter()
        .any(|i| !i.api_trade_available || !i.market_order_available)
    {
        return Err(Error::RiskRejected(
            "cleanup deferred: waiting for market orders to become available".to_owned(),
        ));
    }
    let now = Utc::now();
    let session = current_moex_session_date(
        &api,
        now,
        storage
            .load_risk_state(&ids.account_id)
            .await?
            .map(|s| s.session_date),
    )
    .await?;
    let mut risk = initialize_risk(&api, &storage, &config.risk, ids, &portfolio, session).await?;
    let (flows, recognized) = scan_external_cash_flows(&api, risk.state()).await?;
    let mut decision = risk.observe(&RiskObservation {
        current_equity: rub_equity(&portfolio)?,
        session_date: session,
        observed_at: now,
        external_cash_flows: flows,
        external_cash_flows_recognized: recognized,
        portfolio_is_flat: false,
    })?;
    decision.mode = RiskMode::CloseOnly;
    let target = TargetPortfolio {
        batch_id: Uuid::new_v4(),
        event_id: format!("cleanup:{}:{reason}", Uuid::new_v4()),
        strategy_id: ids.strategy_id.clone(),
        account_id: ids.account_id.clone(),
        source_as_of: now,
        generated_at: now,
        valid_until: now + Duration::minutes(5),
        weights: managed
            .iter()
            .cloned()
            .map(|instrument_id| TargetWeight {
                instrument_id,
                weight: Decimal::ZERO,
            })
            .collect(),
    };
    let planned = TargetPlanner.plan(
        &target,
        &PlanningContext {
            portfolio,
            instruments,
            prices,
            risk: decision.clone(),
            min_trade_fraction: Decimal::ZERO,
        },
    )?;
    lease.check().await?;
    storage
        .commit_portfolio_event(
            &ids.strategy_id,
            &target.event_id,
            &state,
            Some(&planned.batch),
            &planned.intents,
            Some(&decision.state),
        )
        .await?;
    info!(batch_id = %planned.batch.id, %reason, "maintenance cleanup batch recorded");
    for _ in 0..30 {
        lease.check().await?;
        let outcome = executor.execute_batch_close_only(planned.batch.id).await?;
        if matches!(
            outcome,
            BatchExecutionOutcome::Completed | BatchExecutionOutcome::Failed
        ) {
            break;
        }
        tokio::time::sleep(StdDuration::from_secs(1)).await;
    }
    let remaining = broker.portfolio(&ids.account_id).await?.data;
    let orders = api.active_orders(&ids.account_id).await?;
    if !portfolio_is_flat(&remaining)
        || !orders.is_empty()
        || storage.has_unfinished_batches(&ids.account_id).await?
    {
        return Err(Error::RiskRejected(
            "cleanup deferred: portfolio or orders remain unsettled".to_owned(),
        ));
    }
    finalize_performance(&storage, &api, &remaining).await?;
    info!(account_id = %ids.account_id, %reason, "cleanup complete; account is flat");
    Ok(())
}

async fn finalize_performance(
    storage: &super::PostgresStorage,
    api: &super::VenueApi,
    portfolio: &super::PortfolioSnapshot,
) -> Result<()> {
    if let Some(risk) = storage.load_risk_state(&portfolio.account_id).await? {
        let (flows, recognized) = scan_external_cash_flows(api, &risk).await?;
        if !recognized {
            return Err(Error::RiskRejected(
                "cannot finalize performance with unrecognized cash flows".into(),
            ));
        }
        sqlx::query("WITH updated AS (UPDATE strategy_runs SET last_equity=$2,last_external_cash_flows=$3,ended_at=now(),updated_at=now(),peak_pnl=GREATEST(peak_pnl,$2-anchor_equity-($3-anchor_external_cash_flows)),max_drawdown_percent=GREATEST(max_drawdown_percent,(GREATEST(peak_pnl,$2-anchor_equity-($3-anchor_external_cash_flows))-($2-anchor_equity-($3-anchor_external_cash_flows)))*100/(principal+GREATEST(peak_pnl,0))) WHERE account_id=$1 AND ended_at IS NULL RETURNING id) INSERT INTO performance_samples(run_id,observed_at,equity,external_cash_flows) SELECT id,$4,$2,$3 FROM updated ON CONFLICT DO NOTHING")
            .bind(portfolio.account_id.as_str()).bind(rub_equity(portfolio)?).bind(flows).bind(portfolio.observed_at).execute(storage.pool()).await?;
    }
    Ok(())
}
