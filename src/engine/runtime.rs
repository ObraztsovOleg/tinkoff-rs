//! Применение событий к стратегии: прогрев, решения, durable batch и откат
//! состояния стратегии при ошибке сохранения.

use std::sync::Arc;

use chrono::Utc;
use rust_decimal::Decimal;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{
    DataRequirement, EventPhase, PlanningContext, PortfolioStrategy, RebalanceBatch,
    ReferenceRequirement, RiskMode, ScheduleRequirement, StrategyEvent, StrategyEventKind,
    TargetPlanner, TargetPortfolio, TargetWeight,
};
use crate::{
    Error, Result,
    domain::{AccountId, OrderIntent, StrategyId},
    storage::Storage,
};

/// Результат применения одного события к стратегии и planner-у.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineOutcome {
    /// Target стратегии либо принудительная ликвидация в close-only.
    pub target: Option<TargetPortfolio>,
    /// Созданный durable batch, если событие потребовало ребалансировки.
    pub batch: Option<RebalanceBatch>,
    /// Ноги batch в порядке исполнения.
    pub intents: Vec<OrderIntent>,
    /// `true`, если `event_id` уже был атомарно обработан ранее.
    pub duplicate: bool,
}

/// Оркестратор ровно одной portfolio-стратегии.
///
/// Engine сериализует доступ к mutable-state стратегии, выполняет rollback при
/// ошибке и атомарно сохраняет подтверждённый переход состояния.
pub struct PortfolioEngine<S> {
    account_id: AccountId,
    strategy_id: StrategyId,
    storage: Arc<S>,
    strategy: Mutex<Box<dyn PortfolioStrategy>>,
    planner: TargetPlanner,
}

impl<S: Storage> PortfolioEngine<S> {
    /// Регистрирует экземпляр стратегии и восстанавливает его durable-state.
    ///
    /// Storage отклонит повторное использование `strategy_id` другим account
    /// или другим `kind`.
    pub async fn new(mut strategy: Box<dyn PortfolioStrategy>, storage: Arc<S>) -> Result<Self> {
        let account_id = strategy.account_id().clone();
        let strategy_id = strategy.id().clone();
        storage
            .register_portfolio_strategy(
                &strategy_id,
                &account_id,
                strategy.kind(),
                &strategy.config()?,
            )
            .await?;
        if let Some(state) = storage.load_portfolio_strategy_state(&strategy_id).await? {
            strategy.restore_state(state)?;
        }
        Ok(Self {
            account_id,
            strategy_id,
            storage,
            strategy: Mutex::new(strategy),
            planner: TargetPlanner,
        })
    }

    /// Возвращает требования стратегии к рыночным данным.
    pub async fn data_requirements(&self) -> Vec<DataRequirement> {
        self.strategy.lock().await.data_requirements()
    }

    /// Возвращает требования стратегии к расписанию MOEX.
    pub async fn schedule_requirements(&self) -> Vec<ScheduleRequirement> {
        self.strategy.lock().await.schedule_requirements()
    }

    /// Возвращает требования стратегии к справочным данным брокера.
    pub async fn reference_requirements(&self) -> Vec<ReferenceRequirement> {
        self.strategy.lock().await.reference_requirements()
    }

    /// Applies closed historical candles and commits one atomic warmup checkpoint.
    /// Live events and targets are forbidden. All in-memory changes roll back on failure.
    pub async fn process_warmup_batch(&self, events: &[StrategyEvent]) -> Result<bool> {
        if events.is_empty() {
            return Ok(true);
        }
        if events.iter().any(|event| {
            event.phase != EventPhase::Warmup
                || event.id.is_empty()
                || !matches!(&event.payload, StrategyEventKind::Candle(c) if c.complete)
        }) {
            return Err(Error::InvalidArgument(
                "warmup batch requires closed candles in Warmup phase".to_owned(),
            ));
        }
        let mut strategy = self.strategy.lock().await;
        let previous = strategy.state()?;
        let result = async {
            for event in events {
                if let StrategyEventKind::Candle(candle) = &event.payload {
                    self.storage.store_candle(candle).await?;
                }
                if strategy.on_event(event).await?.is_some() {
                    return Err(Error::Strategy {
                        strategy: self.strategy_id.to_string(),
                        message: "warmup events cannot emit targets".to_owned(),
                    });
                }
            }
            let ids = events.iter().map(|e| e.id.clone()).collect::<Vec<_>>();
            self.storage
                .commit_warmup(&self.strategy_id, &ids, &strategy.state()?)
                .await
        }
        .await;
        if !matches!(result, Ok(true)) {
            strategy.restore_state(previous)?;
        }
        result
    }

    /// Применяет событие и атомарно сохраняет state, batch, risk и все ноги.
    ///
    /// До вызова стратегии сохраняется предыдущий state. При любой ошибке
    /// стратегии, planner-а или storage in-memory state откатывается. Повторный
    /// `event_id` также откатывается и возвращает `duplicate = true`.
    pub async fn process_event(
        &self,
        event: &StrategyEvent,
        planning: Option<&PlanningContext>,
    ) -> Result<EngineOutcome> {
        if event.id.trim().is_empty() {
            return Err(Error::InvalidArgument(
                "strategy event id cannot be empty".to_owned(),
            ));
        }
        if let StrategyEventKind::Candle(candle) = &event.payload {
            self.storage.store_candle(candle).await?;
        }
        let mut strategy = self.strategy.lock().await;
        // Snapshot нужен для транзакционной семантики поверх mutable-объекта:
        // стратегия меняет себя до того, как PostgreSQL подтвердит commit.
        let previous = strategy.state()?;
        let mut target = match strategy.on_event(event).await {
            Ok(target) => target,
            Err(error) => {
                strategy.restore_state(previous)?;
                return Err(Error::Strategy {
                    strategy: self.strategy_id.to_string(),
                    message: error.to_string(),
                });
            }
        };
        if target.is_none() && event.phase == EventPhase::Live {
            if let Some(context) = planning {
                if context.risk.mode == RiskMode::CloseOnly
                    && context
                        .portfolio
                        .positions
                        .iter()
                        .any(|position| !position.quantity.is_zero())
                {
                    // Risk-check не обязан порождать сигнал стратегии. Но если
                    // уже действует lock и счёт не пуст, engine сам создаёт
                    // нулевой полный target, чтобы начать ликвидацию.
                    let generated_at = Utc::now();
                    target = Some(TargetPortfolio {
                        batch_id: Uuid::new_v4(),
                        event_id: event.id.clone(),
                        strategy_id: self.strategy_id.clone(),
                        account_id: self.account_id.clone(),
                        source_as_of: event.occurred_at,
                        generated_at,
                        valid_until: generated_at + chrono::Duration::minutes(5),
                        weights: context
                            .instruments
                            .iter()
                            .map(|instrument| TargetWeight {
                                instrument_id: instrument.instrument_id.clone(),
                                weight: Decimal::ZERO,
                            })
                            .collect(),
                    });
                }
            }
        }
        if let Some(target) = &target {
            if event.phase == EventPhase::Warmup {
                strategy.restore_state(previous)?;
                return Err(Error::Strategy {
                    strategy: self.strategy_id.to_string(),
                    message: "warmup events cannot emit targets".to_owned(),
                });
            }
            if target.strategy_id != self.strategy_id
                || target.account_id != self.account_id
                || target.event_id != event.id
            {
                strategy.restore_state(previous)?;
                return Err(Error::Strategy {
                    strategy: self.strategy_id.to_string(),
                    message: "target identity does not match strategy, account, or event"
                        .to_owned(),
                });
            }
        }

        let planned = match (&target, planning) {
            (Some(target), Some(context)) => match self.planner.plan(target, context) {
                Ok(planned) => Some(planned),
                Err(error) => {
                    strategy.restore_state(previous)?;
                    return Err(error);
                }
            },
            (Some(_), None) => {
                strategy.restore_state(previous)?;
                return Err(Error::Configuration(
                    "a live target requires a planning context".to_owned(),
                ));
            }
            (None, _) => None,
        };
        let updated = strategy.state()?;
        let (batch, intents, risk_state) = match (&planned, planning) {
            (Some(planned), Some(context)) => (
                Some(&planned.batch),
                planned.intents.as_slice(),
                Some(&context.risk.state),
            ),
            (Some(_), None) => {
                strategy.restore_state(previous)?;
                return Err(Error::Configuration(
                    "planned batch lost its planning context".to_owned(),
                ));
            }
            (None, _) => (None, &[][..], None),
        };
        // Единственная storage-транзакция связывает event marker, новый state,
        // target, effective risk и все legs. Поэтому crash не оставит решение
        // стратегии без заявок либо заявки без соответствующего решения.
        let committed = self
            .storage
            .commit_portfolio_event(
                &self.strategy_id,
                &event.id,
                &updated,
                batch,
                intents,
                risk_state,
            )
            .await;
        match committed {
            Ok(true) => Ok(EngineOutcome {
                target,
                batch: planned.as_ref().map(|value| value.batch.clone()),
                intents: planned.map_or_else(Vec::new, |value| value.intents),
                duplicate: false,
            }),
            Ok(false) => {
                strategy.restore_state(previous)?;
                Ok(EngineOutcome {
                    target: None,
                    batch: None,
                    intents: Vec::new(),
                    duplicate: true,
                })
            }
            Err(error) => {
                strategy.restore_state(previous)?;
                Err(error)
            }
        }
    }
}

#[cfg(all(test, feature = "strategies"))]
mod warmup_batch_tests {
    use super::{EventPhase, PortfolioEngine, StrategyEvent, StrategyEventKind};
    use crate::{
        domain::{AccountId, Candle, CandleInterval, InstrumentId, Price, StrategyId},
        storage::{MemoryStorage, Storage},
        strategies::{
            BuiltinStrategySpec, MovingAverageTrendConfig, WeightedEnsembleStrategy,
            WeightedStrategySpec,
        },
    };
    use chrono::{Duration, Utc};
    use rust_decimal::Decimal;
    use std::sync::Arc;

    #[tokio::test]
    async fn warmup_checkpoint_is_atomic_and_never_accepts_live_events() {
        let id = StrategyId::new("warmup-test").unwrap();
        let account = AccountId::new("account").unwrap();
        let uid = InstrumentId::new("uid").unwrap();
        let storage = Arc::new(MemoryStorage::default());
        let strategy = WeightedEnsembleStrategy::new(
            id.clone(),
            account.clone(),
            vec![uid.clone()],
            vec![WeightedStrategySpec {
                signal_weight: Decimal::ONE,
                instruments: None,
                strategy: BuiltinStrategySpec::MovingAverageTrend(
                    MovingAverageTrendConfig::default(),
                ),
            }],
        )
        .unwrap();
        let engine = PortfolioEngine::new(Box::new(strategy), storage.clone())
            .await
            .unwrap();
        let now = Utc::now();
        let event = |n| StrategyEvent {
            id: format!("candle-{n}"),
            phase: EventPhase::Warmup,
            occurred_at: now + Duration::days(n),
            payload: StrategyEventKind::Candle(Candle {
                instrument_id: uid.clone(),
                interval: CandleInterval::OneDay,
                time: now + Duration::days(n),
                complete: true,
                volume: 1,
                open: Price::new(Decimal::ONE),
                high: Price::new(Decimal::ONE),
                low: Price::new(Decimal::ONE),
                close: Price::new(Decimal::ONE),
            }),
        };
        assert!(engine.process_warmup_batch(&[event(0)]).await.unwrap());
        let before = storage.load_portfolio_strategy_state(&id).await.unwrap();
        assert!(
            !engine
                .process_warmup_batch(&[event(1), event(0)])
                .await
                .unwrap()
        );
        assert_eq!(
            storage.load_portfolio_strategy_state(&id).await.unwrap(),
            before
        );
        assert!(engine.process_warmup_batch(&[event(1)]).await.unwrap());
        let mut live = event(2);
        live.phase = EventPhase::Live;
        assert!(engine.process_warmup_batch(&[live]).await.is_err());
        assert!(!storage.has_pending_orders(&account).await.unwrap());
        assert_eq!(
            storage
                .pending_batch_ids(&account, &id)
                .await
                .unwrap()
                .len(),
            0
        );
    }
}
