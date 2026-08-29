//! Полностью offline-пример пользовательской `PortfolioStrategy`.
//!
//! Стратегия один раз на событии открытия сессии задаёт целевой вес `0.5`, то есть
//! направляет половину доступного strategy capital в один инструмент. Пример создаёт
//! только `PortfolioEngine` и in-memory storage: API-клиент и `BatchExecutor` здесь
//! отсутствуют, поэтому рассчитанный batch физически невозможно отправить брокеру.

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tinkoff_rs::{
    Error, Result,
    domain::{
        AccountId, CandleInterval, InstrumentId, Money, PortfolioSnapshot, Price, StrategyId,
    },
    engine::{
        DataRequirement, DataRole, EventPhase, InstrumentInfo, PlanningContext, PortfolioEngine,
        PortfolioStrategy, RiskAnchor, RiskConfig, RiskManager, RiskObservation, ScheduleAnchor,
        ScheduleEvent, ScheduleRequirement, StrategyEvent, StrategyEventKind, TargetPortfolio,
        TargetWeight,
    },
    storage::MemoryStorage,
};
use uuid::Uuid;

/// Минимальная стратегия «половина капитала в одном инструменте».
///
/// Поле `generated` — часть устойчивого состояния: после восстановления процесса
/// оно не даёт повторно создать target для уже обработанного учебного сценария.
struct HalfCapitalStrategy {
    strategy_id: StrategyId,
    account_id: AccountId,
    instrument_id: InstrumentId,
    generated: bool,
}

#[async_trait]
impl PortfolioStrategy for HalfCapitalStrategy {
    /// Уникальный ID экземпляра стратегии, а не название её алгоритма.
    fn id(&self) -> &StrategyId {
        &self.strategy_id
    }

    /// Выделенный счёт, target которого разрешено изменять этой стратегии.
    fn account_id(&self) -> &AccountId {
        &self.account_id
    }

    /// Стабильное имя типа стратегии для хранения и диагностики.
    fn kind(&self) -> &'static str {
        "offline-half-capital"
    }

    /// Сериализуемая конфигурация, которая фиксируется вместе с runtime state.
    fn config(&self) -> Result<Value> {
        Ok(json!({ "weight": "0.5" }))
    }

    /// Минимальная история, нужная алгоритму до перехода в live-фазу.
    /// В реальной стратегии `warmup_bars` соответствует длине индикатора.
    fn data_requirements(&self) -> Vec<DataRequirement> {
        vec![DataRequirement {
            instrument_id: self.instrument_id.clone(),
            role: DataRole::Tradable,
            interval: CandleInterval::OneDay,
            warmup_bars: 1,
            waiting_close: true,
        }]
    }

    /// Просим одно событие ровно в момент официального открытия сессии.
    fn schedule_requirements(&self) -> Vec<ScheduleRequirement> {
        vec![ScheduleRequirement {
            anchor: ScheduleAnchor::SessionOpen,
            offset_seconds: 0,
        }]
    }

    /// Состояние алгоритма, которое engine сохраняет после обработки события.
    fn state(&self) -> Result<Value> {
        Ok(json!({ "generated": self.generated }))
    }

    /// Восстанавливает состояние и строго проверяет его форму.
    fn restore_state(&mut self, state: Value) -> Result<()> {
        self.generated = state
            .get("generated")
            .and_then(Value::as_bool)
            .ok_or_else(|| Error::Strategy {
                strategy: self.strategy_id.to_string(),
                message: "state.generated must be a boolean".to_owned(),
            })?;
        Ok(())
    }

    /// Возвращает полный target portfolio либо `None`, если ребалансировка не нужна.
    ///
    /// Warmup и прочие виды событий только обновляли бы индикаторы. Здесь сигнал
    /// разрешён исключительно в live-фазе и только один раз на schedule event.
    async fn on_event(&mut self, event: &StrategyEvent) -> Result<Option<TargetPortfolio>> {
        if event.phase != EventPhase::Live
            || !matches!(event.payload, StrategyEventKind::Schedule(_))
            || self.generated
        {
            return Ok(None);
        }
        self.generated = true;
        Ok(Some(TargetPortfolio {
            batch_id: Uuid::new_v4(),
            event_id: event.id.clone(),
            strategy_id: self.strategy_id.clone(),
            account_id: self.account_id.clone(),
            // В настоящей дневной стратегии source_as_of указывает закрытие свечи,
            // известной до открытия текущей сессии. Здесь это условная прошлая дата.
            source_as_of: event.occurred_at - Duration::days(1),
            generated_at: event.occurred_at,
            valid_until: event.occurred_at + Duration::hours(1),
            weights: vec![TargetWeight {
                instrument_id: self.instrument_id.clone(),
                // Signed weight +0.5 означает long на 50% sizing capital.
                // Отрицательное значение означало бы короткую позицию.
                weight: Decimal::new(5, 1),
            }],
        }))
    }
}

#[tokio::main]
/// Собирает искусственный портфель, запускает planner и печатает результат.
/// Ни токен, ни `DATABASE_URL`, ни сетевой доступ для примера не нужны.
async fn main() -> Result<()> {
    let account_id = AccountId::new("offline-account")?;
    let strategy_id = StrategyId::new("offline-half-capital")?;
    let instrument_id = InstrumentId::new("offline-moex-uid")?;
    let storage = Arc::new(MemoryStorage::default());
    // Engine сохраняет target и рассчитанные legs в памяти, но сам заявки не шлёт.
    let engine = PortfolioEngine::new(
        Box::new(HalfCapitalStrategy {
            strategy_id: strategy_id.clone(),
            account_id: account_id.clone(),
            instrument_id: instrument_id.clone(),
            generated: false,
        }),
        storage,
    )
    .await?;

    let now = Utc::now();
    // Бюджет — верхняя граница sizing capital. При equity 30 000 ₽ и весе 0,5
    // целевая денежная позиция равна 15 000 ₽ до округления к целым лотам.
    let risk_config = RiskConfig {
        budget_rub: Decimal::from(30_000),
        daily_loss_percent: Some(Decimal::from(5)),
        max_drawdown_percent: Some(Decimal::from(25)),
    };
    let mut risk = RiskManager::new(
        risk_config,
        RiskAnchor {
            account_id: account_id.clone(),
            strategy_id: strategy_id.clone(),
            account_equity: Decimal::from(30_000),
            session_date: now.date_naive(),
            observed_at: now,
        },
    )?;
    // Нулевые внешние потоки и неизменная equity означают P&L = 0: оба stop
    // остаются разблокированными, а planner может строить открывающую leg.
    let risk = risk.observe(&RiskObservation {
        current_equity: Decimal::from(30_000),
        session_date: now.date_naive(),
        observed_at: now,
        external_cash_flows: Decimal::ZERO,
        external_cash_flows_recognized: true,
        portfolio_is_flat: true,
    })?;
    let portfolio = PortfolioSnapshot {
        account_id: account_id.clone(),
        total: Some(Money {
            currency: "rub".to_owned(),
            amount: Price::new(Decimal::from(30_000)),
        }),
        positions: Vec::new(),
        observed_at: now,
    };
    let planning = PlanningContext {
        portfolio,
        instruments: vec![InstrumentInfo {
            instrument_id: instrument_id.clone(),
            // Один лот = 10 бумаг по 100 ₽, то есть 1 000 ₽ на лот.
            // Target 15 000 ₽ преобразуется planner в 15 целых лотов.
            lot_size: 10,
            currency: "rub".to_owned(),
            exchange: "moex".to_owned(),
            short_enabled: true,
            api_trade_available: true,
            market_order_available: true,
        }],
        prices: HashMap::from([(instrument_id, Price::new(Decimal::from(100)))]),
        risk,
    };
    let event = StrategyEvent {
        id: "offline-session-open".to_owned(),
        phase: EventPhase::Live,
        occurred_at: now,
        payload: StrategyEventKind::Schedule(ScheduleEvent {
            anchor: ScheduleAnchor::SessionOpen,
            session_date: now.date_naive(),
            scheduled_at: now,
        }),
    };
    // process_event лишь планирует и сохраняет batch. Для отправки потребовался бы
    // отдельный BatchExecutor с брокером, которого в offline-примере намеренно нет.
    let outcome = engine.process_event(&event, Some(&planning)).await?;
    println!(
        "offline target planned: batch={}, legs={}; no broker or executor was created",
        outcome.batch.is_some(),
        outcome.intents.len()
    );
    Ok(())
}
