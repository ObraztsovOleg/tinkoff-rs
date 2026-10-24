//! Взвешенный ансамбль: несколько встроенных стратегий на одном счёте, каждая
//! на своём наборе инструментов или с долей общего сигнала.

use std::collections::HashSet;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    BuiltinStrategy, BuiltinStrategySpec, EXPIRY_REBALANCE_VALIDITY, deterministic_batch_id,
};
use crate::{
    Error, Result,
    domain::{AccountId, InstrumentId, StrategyId},
    engine::{
        DataRequirement, DataRole, EventPhase, PortfolioStrategy, ReferenceRequirement,
        ScheduleAnchor, ScheduleRequirement, StrategyEvent, StrategyEventKind, TargetPortfolio,
        TargetWeight,
    },
};

/// Одна компонента взвешенного ансамбля.
///
/// `signal_weight` задаёт относительную долю сигнала. Перед агрегацией все
/// коэффициенты делятся на их общую сумму, поэтому значения `1, 1, 2` означают
/// те же доли, что `0.25, 0.25, 0.50`. При единственной компоненте её эффективный
/// коэффициент всегда равен единице.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WeightedStrategySpec {
    /// Относительный вклад полного target этой компоненты в общий target.
    #[serde(default = "default_signal_weight", with = "rust_decimal::serde::str")]
    pub signal_weight: Decimal,
    /// Инструменты компоненты — подмножество общего universe; по умолчанию весь universe.
    ///
    /// Компоненты с одинаковым набором образуют комбинацию и складывают сигналы по
    /// общим бумагам. Разные наборы на одном счёте не должны пересекаться, иначе две
    /// независимые стратегии управляли бы одной позицией.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruments: Option<Vec<InstrumentId>>,
    /// Вид встроенной стратегии и её собственные параметры.
    #[serde(flatten)]
    pub strategy: BuiltinStrategySpec,
}

/// Значение веса компоненты по умолчанию.
const fn default_signal_weight() -> Decimal {
    Decimal::ONE
}

/// Последний полный target компоненты, используемый до её следующего сигнала.
///
/// Для дневных и месячных стратегий `holding_valid_until` отсутствует: их
/// `TargetPortfolio::valid_until` ограничивает только отправку конкретного batch.
/// Для двух внутридневных стратегий поле одновременно задаёт конец holding-slot,
/// после которого ансамбль обязан убрать вклад и выпустить закрывающий target.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct CachedComponentTarget {
    source_as_of: DateTime<Utc>,
    holding_valid_until: Option<DateTime<Utc>>,
    weights: Vec<TargetWeight>,
}

/// Сериализуемое состояние одной компоненты ансамбля.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct WeightedComponentState {
    /// Защищает от восстановления state в стратегию другого вида после перестановки TOML.
    kind: String,
    /// Изменение относительного вклада требует нового `strategy_id`.
    #[serde(with = "rust_decimal::serde::str")]
    signal_weight: Decimal,
    /// Не позволяет применить историю к другому набору параметров того же kind.
    strategy_config: Value,
    /// Собственный набор инструментов; отсутствует, если компонента видит весь universe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instruments: Option<Vec<InstrumentId>>,
    strategy_state: Value,
    last_target: Option<CachedComponentTarget>,
}

/// Полный durable state ансамбля, атомарно сохраняемый внешним engine.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct WeightedEnsembleState {
    /// Точный порядок UID является частью совместимости durable state.
    universe: Vec<InstrumentId>,
    components: Vec<WeightedComponentState>,
}

/// Полная конфигурация ансамбля, сохраняемая storage для аудита.
///
/// Universe находится здесь отдельно от параметров компонент, потому что один и
/// тот же kind может быть корректен для нескольких несовместимых наборов бумаг.
#[derive(Serialize)]
struct WeightedEnsembleConfig<'a> {
    universe: &'a [InstrumentId],
    strategies: &'a [WeightedStrategySpec],
}

/// Runtime-компонента ансамбля с независимым состоянием и последним сигналом.
struct WeightedComponent {
    signal_weight: Decimal,
    /// Инструменты компоненты в порядке общего universe.
    universe: Vec<InstrumentId>,
    strategy: BuiltinStrategy,
    last_target: Option<CachedComponentTarget>,
}

/// Объединяет одну или несколько встроенных стратегий в один торгуемый target.
///
/// Каждая компонента независимо получает все события и хранит собственную историю.
/// Когда хотя бы одна компонента формирует новый target, ансамбль обновляет её
/// cached target и пересчитывает полный портфель:
///
/// `combined_weight(uid) = Σ(component_weight × component_target(uid)) / Σweights`.
///
/// Компонента без первого сигнала временно вносит ноль. Дневной или месячный target
/// действует до следующего сигнала той же компоненты; внутридневной дополнительно
/// снимается по окончании своего holding-slot. Поэтому дневная стратегия может
/// инициировать пересчёт, не стирая месячный сигнал.
pub struct WeightedEnsembleStrategy {
    strategy_id: StrategyId,
    account_id: AccountId,
    universe: Vec<InstrumentId>,
    specs: Vec<WeightedStrategySpec>,
    components: Vec<WeightedComponent>,
    total_signal_weight: Decimal,
}

impl WeightedEnsembleStrategy {
    /// Валидирует конфигурацию и создаёт ансамбль; список из одной стратегии допустим.
    pub fn new(
        strategy_id: StrategyId,
        account_id: AccountId,
        universe: Vec<InstrumentId>,
        specs: Vec<WeightedStrategySpec>,
    ) -> Result<Self> {
        if specs.is_empty() {
            return Err(Error::Configuration(
                "strategies must contain at least one component".to_owned(),
            ));
        }

        let mut total_signal_weight = Decimal::ZERO;
        let mut components = Vec::with_capacity(specs.len());
        for (index, spec) in specs.iter().enumerate() {
            if spec.signal_weight <= Decimal::ZERO {
                return Err(Error::Configuration(format!(
                    "strategies[{index}].signal_weight must be positive"
                )));
            }
            total_signal_weight = total_signal_weight
                .checked_add(spec.signal_weight)
                .ok_or_else(|| {
                    Error::Configuration("sum of signal weights overflowed".to_owned())
                })?;

            components.push(Self::build_component(
                &strategy_id,
                &account_id,
                &universe,
                index,
                spec,
            )?);
        }
        Self::validate_component_sets(&universe, &components)?;

        Ok(Self {
            strategy_id,
            account_id,
            universe,
            specs,
            components,
            total_signal_weight,
        })
    }

    /// Создаёт одну изолированную runtime-компоненту с детерминированным внутренним ID.
    ///
    /// Storage видит только внешний ID ансамбля. Внутренний ID используется для
    /// проверки identity дочернего target и позволяет безопасно собрать временный
    /// набор компонент перед атомарным `restore_state`.
    fn build_component(
        strategy_id: &StrategyId,
        account_id: &AccountId,
        universe: &[InstrumentId],
        index: usize,
        spec: &WeightedStrategySpec,
    ) -> Result<WeightedComponent> {
        let component_id = StrategyId::new(format!(
            "{}::{}::{}",
            strategy_id,
            index + 1,
            spec.strategy.kind()
        ))?;
        let component_universe = Self::component_universe(universe, index, spec)?;
        let strategy = spec.strategy.clone().build(
            component_id,
            account_id.clone(),
            component_universe.clone(),
        )?;
        Ok(WeightedComponent {
            signal_weight: spec.signal_weight,
            universe: component_universe,
            strategy,
            last_target: None,
        })
    }

    /// Набор компоненты в порядке общего universe; без `instruments` — весь universe.
    fn component_universe(
        universe: &[InstrumentId],
        index: usize,
        spec: &WeightedStrategySpec,
    ) -> Result<Vec<InstrumentId>> {
        let Some(instruments) = &spec.instruments else {
            return Ok(universe.to_vec());
        };
        let mut unique = HashSet::with_capacity(instruments.len());
        if instruments.is_empty()
            || instruments
                .iter()
                .any(|instrument_id| !unique.insert(instrument_id))
        {
            return Err(Error::Configuration(format!(
                "strategies[{index}].instruments must be a nonempty list of distinct UIDs"
            )));
        }
        if let Some(unknown) = instruments
            .iter()
            .find(|instrument_id| !universe.contains(instrument_id))
        {
            return Err(Error::Configuration(format!(
                "strategies[{index}].instruments contains `{unknown}` outside the universe"
            )));
        }
        Ok(universe
            .iter()
            .filter(|instrument_id| unique.contains(instrument_id))
            .cloned()
            .collect())
    }

    /// Наборы компонент либо совпадают (комбинация), либо не пересекаются.
    ///
    /// Каждый UID universe должен принадлежать хотя бы одной компоненте: иначе
    /// бумага навсегда получила бы нулевую цель без стратегии-владельца.
    fn validate_component_sets(
        universe: &[InstrumentId],
        components: &[WeightedComponent],
    ) -> Result<()> {
        for (left_index, left) in components.iter().enumerate() {
            for (right_offset, right) in components[left_index + 1..].iter().enumerate() {
                // Несколько ИИ-компонент на одном наборе — соревнование моделей:
                // решение адресуется компоненте по профилю, поэтому профили явные и разные.
                if let (BuiltinStrategySpec::AiModel(a), BuiltinStrategySpec::AiModel(b)) =
                    (&left.strategy.spec, &right.strategy.spec)
                {
                    if left.universe == right.universe
                        && (a.model_profile.is_none() || a.model_profile == b.model_profile)
                    {
                        return Err(Error::Configuration(format!(
                            "ai-model strategies[{left_index}] and strategies[{}] trade one set: give each its own model_profile",
                            left_index + right_offset + 1
                        )));
                    }
                }
                if left.universe != right.universe
                    && left
                        .universe
                        .iter()
                        .any(|instrument_id| right.universe.contains(instrument_id))
                {
                    return Err(Error::Configuration(format!(
                        "strategies[{left_index}] and strategies[{}] share instruments but are not one combination; use identical or disjoint instrument sets",
                        left_index + right_offset + 1
                    )));
                }
            }
        }
        if let Some(uncovered) = universe.iter().find(|instrument_id| {
            !components
                .iter()
                .any(|component| component.universe.contains(instrument_id))
        }) {
            return Err(Error::Configuration(format!(
                "universe instrument `{uncovered}` is not traded by any strategy"
            )));
        }
        Ok(())
    }

    /// Сохраняемое представление набора: `None` для компоненты на всём universe.
    fn stored_instruments(&self, component: &WeightedComponent) -> Option<Vec<InstrumentId>> {
        (component.universe != self.universe).then(|| component.universe.clone())
    }

    /// Только эти targets описывают позицию на ограниченный внутридневной слот.
    fn is_intraday_kind(kind: &str) -> bool {
        matches!(kind, "market-intraday-momentum" | "intraday-same-clock")
    }

    /// Сливает одинаковые candle requirements разных компонент.
    ///
    /// Для общей пары UID/interval берётся самый длинный warm-up. Это исключает
    /// повторную загрузку одинаковой истории и дублирование stream subscription.
    fn merged_data_requirements(&self) -> Vec<DataRequirement> {
        let mut merged: Vec<DataRequirement> = Vec::new();
        for requirement in self
            .components
            .iter()
            .flat_map(|component| component.strategy.data_requirements())
        {
            if let Some(existing) = merged.iter_mut().find(|existing| {
                existing.instrument_id == requirement.instrument_id
                    && existing.interval == requirement.interval
            }) {
                existing.warmup_bars = existing.warmup_bars.max(requirement.warmup_bars);
                existing.waiting_close |= requirement.waiting_close;
                if requirement.role == DataRole::Tradable {
                    existing.role = DataRole::Tradable;
                }
            } else {
                merged.push(requirement);
            }
        }
        merged
    }

    /// Сливает одинаковые session anchors, чтобы один момент породил одно событие.
    fn merged_schedule_requirements(&self) -> Vec<ScheduleRequirement> {
        let mut merged = Vec::new();
        for requirement in self
            .components
            .iter()
            .flat_map(|component| component.strategy.schedule_requirements())
        {
            if !merged.contains(&requirement) {
                merged.push(requirement);
            }
        }
        merged
    }

    /// Проверяет форму дочернего target до помещения в durable cache ансамбля.
    fn validate_component_target(
        account_id: &AccountId,
        universe: &[InstrumentId],
        component_id: &StrategyId,
        event: &StrategyEvent,
        target: &TargetPortfolio,
    ) -> Result<()> {
        if target.strategy_id != *component_id
            || target.account_id != *account_id
            || target.event_id != event.id
            || target.weights.len() != universe.len()
        {
            return Err(Error::Strategy {
                strategy: component_id.to_string(),
                message: "component returned a target with invalid identity or shape".to_owned(),
            });
        }
        let mut seen = HashSet::with_capacity(target.weights.len());
        if target.weights.iter().any(|weight| {
            !universe.contains(&weight.instrument_id) || !seen.insert(&weight.instrument_id)
        }) {
            return Err(Error::Strategy {
                strategy: component_id.to_string(),
                message: "component target contains an unknown or duplicate instrument".to_owned(),
            });
        }
        Ok(())
    }

    /// Проверяет durable cache до его помещения в staged runtime-компоненту.
    fn validate_cached_target(
        universe: &[InstrumentId],
        component_id: &StrategyId,
        component_kind: &str,
        target: &CachedComponentTarget,
    ) -> Result<()> {
        let mut seen = HashSet::with_capacity(target.weights.len());
        if target.weights.len() != universe.len()
            || target.weights.iter().any(|weight| {
                !universe.contains(&weight.instrument_id) || !seen.insert(&weight.instrument_id)
            })
        {
            return Err(Error::Strategy {
                strategy: component_id.to_string(),
                message: "stored component target does not match configured universe".to_owned(),
            });
        }

        let intraday = Self::is_intraday_kind(component_kind);
        if intraday != target.holding_valid_until.is_some()
            || target
                .holding_valid_until
                .is_some_and(|expires_at| expires_at <= target.source_as_of)
        {
            return Err(Error::Strategy {
                strategy: component_id.to_string(),
                message: "stored component target has an invalid holding expiry".to_owned(),
            });
        }
        Ok(())
    }

    /// Строит взвешенную сумму последних полных targets в порядке общего universe.
    fn aggregate_weights(&self) -> Result<Vec<TargetWeight>> {
        self.universe
            .iter()
            .map(|instrument_id| {
                // Сначала складывается числитель, и только затем выполняется одно
                // деление. Это сохраняет точное взаимное уничтожение сигналов вроде
                // `1 * +1` и `2 * -0.5`, не округляя отдельно коэффициенты 1/3 и 2/3.
                let mut weighted_sum = Decimal::ZERO;
                for component in &self.components {
                    // Чужая для компоненты бумага: её вклад в этот UID равен нулю.
                    let Some(target) = component
                        .last_target
                        .as_ref()
                        .filter(|_| component.universe.contains(instrument_id))
                    else {
                        continue;
                    };
                    let target_weight = target
                        .weights
                        .iter()
                        .find(|weight| weight.instrument_id == *instrument_id)
                        .ok_or_else(|| Error::Strategy {
                            strategy: self.strategy_id.to_string(),
                            message: "cached component target lost a universe instrument"
                                .to_owned(),
                        })?
                        .weight;
                    let weighted_target = target_weight
                        .checked_mul(component.signal_weight)
                        .ok_or_else(|| Error::Strategy {
                            strategy: self.strategy_id.to_string(),
                            message: "weighted component target overflowed".to_owned(),
                        })?;
                    weighted_sum = weighted_sum.checked_add(weighted_target).ok_or_else(|| {
                        Error::Strategy {
                            strategy: self.strategy_id.to_string(),
                            message: "combined target weight overflowed".to_owned(),
                        }
                    })?;
                }
                let combined = weighted_sum
                    .checked_div(self.total_signal_weight)
                    .ok_or_else(|| Error::Strategy {
                        strategy: self.strategy_id.to_string(),
                        message: "cannot normalize combined target weight".to_owned(),
                    })?;
                Ok(TargetWeight {
                    instrument_id: instrument_id.clone(),
                    weight: combined,
                })
            })
            .collect()
    }
}

#[async_trait]
impl PortfolioStrategy for WeightedEnsembleStrategy {
    /// Возвращает ID всего ансамбля — именно он владеет счётом, risk-state и batch-ами.
    fn id(&self) -> &StrategyId {
        &self.strategy_id
    }

    /// Все компоненты ансамбля работают с одним выделенным Sandbox-счётом.
    fn account_id(&self) -> &AccountId {
        &self.account_id
    }

    /// Стабильный storage kind не зависит от количества и видов компонент.
    fn kind(&self) -> &'static str {
        "weighted-ensemble"
    }

    /// Сохраняет universe, полный список компонент и относительные веса для аудита.
    fn config(&self) -> Result<Value> {
        serde_json::to_value(WeightedEnsembleConfig {
            universe: &self.universe,
            strategies: &self.specs,
        })
        .map_err(|error| Error::Strategy {
            strategy: self.strategy_id.to_string(),
            message: error.to_string(),
        })
    }

    /// Возвращает объединённые требования без повторной загрузки одинаковых рядов.
    fn data_requirements(&self) -> Vec<DataRequirement> {
        self.merged_data_requirements()
    }

    /// Возвращает уникальный набор сессионных триггеров всех компонент.
    fn schedule_requirements(&self) -> Vec<ScheduleRequirement> {
        self.merged_schedule_requirements()
    }

    /// Объединяет справочные данные, которые нужны хотя бы одной компоненте.
    fn reference_requirements(&self) -> Vec<ReferenceRequirement> {
        let mut merged = Vec::new();
        for requirement in self
            .components
            .iter()
            .flat_map(|component| component.strategy.reference_requirements())
        {
            if !merged.contains(&requirement) {
                merged.push(requirement);
            }
        }
        merged
    }

    /// Сериализует независимое состояние и последний полный сигнал каждой компоненты.
    fn state(&self) -> Result<Value> {
        let components = self
            .components
            .iter()
            .map(|component| {
                Ok(WeightedComponentState {
                    kind: component.strategy.kind().to_owned(),
                    signal_weight: component.signal_weight,
                    strategy_config: component.strategy.config()?,
                    instruments: self.stored_instruments(component),
                    strategy_state: component.strategy.state()?,
                    last_target: component.last_target.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        serde_json::to_value(WeightedEnsembleState {
            universe: self.universe.clone(),
            components,
        })
        .map_err(|error| Error::Strategy {
            strategy: self.strategy_id.to_string(),
            message: error.to_string(),
        })
    }

    /// Полностью проверяет и восстанавливает временные компоненты, затем меняет runtime.
    ///
    /// Пока staged-набор не готов целиком, `self.components` не изменяется. Поэтому
    /// ошибка в поздней компоненте не оставляет частично восстановленный ансамбль.
    fn restore_state(&mut self, state: Value) -> Result<()> {
        let restored: WeightedEnsembleState =
            serde_json::from_value(state).map_err(|error| Error::Strategy {
                strategy: self.strategy_id.to_string(),
                message: format!(
                    "stored ensemble state is incompatible ({error}); use a new strategy_id"
                ),
            })?;
        if restored.universe != self.universe {
            return Err(Error::Strategy {
                strategy: self.strategy_id.to_string(),
                message: "stored ensemble universe changed; use a new strategy_id".to_owned(),
            });
        }
        if restored.components.len() != self.specs.len() {
            return Err(Error::Strategy {
                strategy: self.strategy_id.to_string(),
                message: "stored ensemble has another number of components; use a new strategy_id"
                    .to_owned(),
            });
        }

        let mut staged_components = Vec::with_capacity(self.specs.len());
        for (index, (spec, restored_component)) in
            self.specs.iter().zip(restored.components).enumerate()
        {
            let mut staged = Self::build_component(
                &self.strategy_id,
                &self.account_id,
                &self.universe,
                index,
                spec,
            )?;
            if restored_component.kind != staged.strategy.kind()
                || restored_component.signal_weight != staged.signal_weight
                || restored_component.strategy_config != staged.strategy.config()?
                || restored_component.instruments != self.stored_instruments(&staged)
            {
                return Err(Error::Strategy {
                    strategy: self.strategy_id.to_string(),
                    message: "stored ensemble component order, kind, signal_weight, instruments, or parameters changed; use a new strategy_id".to_owned(),
                });
            }
            staged
                .strategy
                .restore_state(restored_component.strategy_state)?;
            if let Some(target) = &restored_component.last_target {
                Self::validate_cached_target(
                    &staged.universe,
                    staged.strategy.id(),
                    staged.strategy.kind(),
                    target,
                )?;
            }
            staged.last_target = restored_component.last_target;
            staged_components.push(staged);
        }
        self.components = staged_components;
        Ok(())
    }

    /// Обновляет компоненты и выпускает target при новом либо истёкшем intraday-сигнале.
    async fn on_event(&mut self, event: &StrategyEvent) -> Result<Option<TargetPortfolio>> {
        let mut emitted = false;
        let mut valid_until: Option<DateTime<Utc>> = None;
        let account_id = self.account_id.clone();

        for component in &mut self.components {
            let intraday = Self::is_intraday_kind(component.strategy.kind());
            // Истечение применяется только в live-фазе. Иначе исторический warm-up
            // мог бы породить запрещённый closing target до запуска runtime.
            let official_intraday_close = intraday
                && matches!(
                    &event.payload,
                    StrategyEventKind::Schedule(schedule)
                        if schedule.anchor == ScheduleAnchor::SessionClose
                );
            let expired = event.phase == EventPhase::Live
                && (official_intraday_close
                    || component
                        .last_target
                        .as_ref()
                        .and_then(|target| target.holding_valid_until)
                        .is_some_and(|expires_at| event.occurred_at >= expires_at));
            let expired_had_exposure = if expired {
                // Нулевой cache можно удалить без batch: durable state всё равно
                // сохранится вместе с текущим событием, а позицию он не менял.
                let had_exposure = component.last_target.as_ref().is_some_and(|target| {
                    target.weights.iter().any(|weight| !weight.weight.is_zero())
                });
                component.last_target = None;
                had_exposure
            } else {
                false
            };

            let component_id = component.strategy.id().clone();
            if let Some(target) = component.strategy.on_event(event).await? {
                Self::validate_component_target(
                    &account_id,
                    &component.universe,
                    &component_id,
                    event,
                    &target,
                )?;
                emitted = true;
                valid_until = Some(valid_until.map_or(target.valid_until, |current| {
                    current.min(target.valid_until)
                }));
                component.last_target = Some(CachedComponentTarget {
                    source_as_of: target.source_as_of,
                    holding_valid_until: intraday.then_some(target.valid_until),
                    weights: target.weights,
                });
            } else if expired_had_exposure {
                // Если на том же событии появился новый target компоненты, он
                // заменяет старый и получает собственный полный deadline. Короткий
                // closing deadline нужен лишь когда замены действительно нет.
                emitted = true;
                let expiry_deadline = event.occurred_at + EXPIRY_REBALANCE_VALIDITY;
                valid_until = Some(
                    valid_until.map_or(expiry_deadline, |current| current.min(expiry_deadline)),
                );
            }
        }

        if !emitted {
            return Ok(None);
        }

        // Для audit timestamp берётся самый свежий источник среди реально
        // участвующих cached targets. Компоненты без первого сигнала дают ноль.
        let source_as_of = self
            .components
            .iter()
            .filter_map(|component| {
                component
                    .last_target
                    .as_ref()
                    .map(|target| target.source_as_of)
            })
            .max()
            .unwrap_or(event.occurred_at);
        let valid_until = valid_until.ok_or_else(|| Error::Strategy {
            strategy: self.strategy_id.to_string(),
            message: "ensemble emitted without an execution deadline".to_owned(),
        })?;
        Ok(Some(TargetPortfolio {
            batch_id: deterministic_batch_id(&self.strategy_id, &event.id),
            event_id: event.id.clone(),
            strategy_id: self.strategy_id.clone(),
            account_id: self.account_id.clone(),
            source_as_of,
            generated_at: event.occurred_at,
            valid_until,
            weights: self.aggregate_weights()?,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategies::{MovingAverageTrendConfig, RangeBreakoutConfig};

    fn uid(name: &str) -> InstrumentId {
        InstrumentId::new(name).unwrap()
    }

    fn spec(weight: i64, instruments: Option<&[&str]>, trend: bool) -> WeightedStrategySpec {
        WeightedStrategySpec {
            signal_weight: Decimal::from(weight),
            instruments: instruments.map(|names| names.iter().map(|name| uid(name)).collect()),
            strategy: if trend {
                BuiltinStrategySpec::MovingAverageTrend(MovingAverageTrendConfig::default())
            } else {
                BuiltinStrategySpec::RangeBreakout(RangeBreakoutConfig::default())
            },
        }
    }

    fn ai(profile: Option<&str>) -> WeightedStrategySpec {
        let profile = profile.map_or_else(String::new, |p| format!("model_profile = \"{p}\"\n"));
        toml::from_str(&format!(
            "signal_weight = \"1\"\nkind = \"ai-model\"\n{profile}"
        ))
        .unwrap()
    }

    #[test]
    fn models_compete_on_one_set_only_under_their_own_profiles() {
        assert!(
            ensemble(vec![
                ai(Some("deepseek")),
                ai(Some("qwen122")),
                ai(Some("qwen-flash"))
            ])
            .is_ok()
        );
        for clash in [
            vec![ai(Some("deepseek")), ai(Some("deepseek"))],
            vec![ai(None), ai(Some("qwen122"))],
        ] {
            let error = ensemble(clash).err().unwrap().to_string();
            assert!(error.contains("own model_profile"), "{error}");
        }
    }

    fn ensemble(specs: Vec<WeightedStrategySpec>) -> Result<WeightedEnsembleStrategy> {
        WeightedEnsembleStrategy::new(
            StrategyId::new("sleeves").unwrap(),
            AccountId::new("account").unwrap(),
            vec![uid("a"), uid("b"), uid("c")],
            specs,
        )
    }

    fn cached(weights: &[(&str, i64)]) -> CachedComponentTarget {
        CachedComponentTarget {
            source_as_of: Utc::now(),
            holding_valid_until: None,
            weights: weights
                .iter()
                .map(|(name, weight)| TargetWeight {
                    instrument_id: uid(name),
                    weight: Decimal::from(*weight),
                })
                .collect(),
        }
    }

    #[test]
    fn component_sets_must_be_identical_or_disjoint_and_cover_universe() {
        // Combination on {a,b} plus an independent block on {c}.
        let ok = ensemble(vec![
            spec(1, Some(&["b", "a"]), true),
            spec(1, Some(&["a", "b"]), false),
            spec(2, Some(&["c"]), true),
        ])
        .unwrap();
        assert_eq!(ok.components[0].universe, vec![uid("a"), uid("b")]);
        assert_eq!(ok.components[1].universe, ok.components[0].universe);
        assert!(
            ensemble(vec![
                spec(1, Some(&["a", "b"]), true),
                spec(1, Some(&["b", "c"]), false)
            ])
            .is_err()
        );
        assert!(
            ensemble(vec![spec(1, Some(&["a", "b"]), true)]).is_err(),
            "c is not traded"
        );
        assert!(
            ensemble(vec![
                spec(1, Some(&["a", "x"]), true),
                spec(1, Some(&["b", "c"]), false)
            ])
            .is_err()
        );
        assert!(
            ensemble(vec![
                spec(1, Some(&["a", "a"]), true),
                spec(1, Some(&["b", "c"]), false)
            ])
            .is_err()
        );
        assert!(ensemble(vec![spec(1, Some(&[]), true), spec(1, None, false)]).is_err());
        // Without instrument lists every component sees the whole universe, as before.
        assert!(ensemble(vec![spec(1, None, true), spec(1, None, false)]).is_ok());
    }

    #[test]
    fn disjoint_blocks_split_capital_by_signal_weight() {
        let mut strategy = ensemble(vec![
            spec(1, Some(&["a", "b"]), true),
            spec(1, Some(&["c"]), false),
        ])
        .unwrap();
        strategy.components[0].last_target = Some(cached(&[("a", 1), ("b", 0)]));
        strategy.components[1].last_target = Some(cached(&[("c", -1)]));
        let weights = strategy.aggregate_weights().unwrap();
        let by_uid = |name: &str| {
            weights
                .iter()
                .find(|w| w.instrument_id == uid(name))
                .unwrap()
                .weight
        };
        assert_eq!(by_uid("a"), Decimal::new(5, 1));
        assert_eq!(by_uid("b"), Decimal::ZERO);
        assert_eq!(by_uid("c"), Decimal::new(-5, 1));
    }

    #[test]
    fn stored_state_keeps_component_instruments() {
        let blocks = || {
            vec![
                spec(1, Some(&["a", "b"]), true),
                spec(1, Some(&["c"]), false),
            ]
        };
        let mut first = ensemble(blocks()).unwrap();
        first.components[1].last_target = Some(cached(&[("c", 1)]));
        let state = first.state().unwrap();
        assert!(state["components"][0]["instruments"].is_array());
        let mut restored = ensemble(blocks()).unwrap();
        restored.restore_state(state.clone()).unwrap();
        assert!(restored.components[1].last_target.is_some());
        // Same components, other split of the universe: incompatible durable state.
        let mut moved = ensemble(vec![
            spec(1, Some(&["a"]), true),
            spec(1, Some(&["b", "c"]), false),
        ])
        .unwrap();
        assert!(moved.restore_state(state).is_err());
        // Legacy state without instrument lists restores into a full-universe ensemble.
        let legacy = ensemble(vec![spec(1, None, true)])
            .unwrap()
            .state()
            .unwrap();
        assert!(legacy["components"][0].get("instruments").is_none());
        ensemble(vec![spec(1, None, true)])
            .unwrap()
            .restore_state(legacy)
            .unwrap();
    }

    #[cfg(feature = "cli")]
    #[test]
    fn toml_component_accepts_instrument_list() {
        let parsed: WeightedStrategySpec = toml::from_str(
            "signal_weight = \"0.5\"\nkind = \"moving-average-trend\"\ninstruments = [\"a\", \"b\"]\n",
        )
        .unwrap();
        assert_eq!(parsed.instruments, Some(vec![uid("a"), uid("b")]));
        let plain: WeightedStrategySpec = toml::from_str("kind = \"range-breakout\"\n").unwrap();
        assert!(plain.instruments.is_none());
    }
}
