//! Детерминированные portfolio-target реализации пятнадцати встроенных стратегий
//! и их взвешенного ансамбля.
//!
//! Стратегии читают только закрытые свечи, хранят сериализуемое состояние без
//! broker-specific объектов и никогда не выставляют заявки сами. Они возвращают
//! **полный** целевой портфель: положительный signed weight означает long,
//! отрицательный — short, нулевой — закрытие позиции. События warm-up только
//! наполняют историю и принципиально не могут породить торговый target.

mod config;
mod math;
mod state;

use std::{cmp::Ordering, collections::HashSet};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Timelike, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub use config::{
    BettingAgainstBetaConfig, BuiltinStrategySpec, CrossSectionalMomentumConfig,
    EqualRiskContributionConfig, FiftyTwoWeekHighConfig, IntermediateMomentumConfig,
    IntradaySameClockConfig, LowVolatilityConfig, MarketIntradayMomentumConfig,
    MovingAverageTrendConfig, PairsTradingConfig, RangeBreakoutConfig, ResidualMomentumConfig,
    ShortTermReversalConfig, TimeSeriesMomentumConfig, VolatilityManagedConfig,
};

use crate::{
    Error, Result,
    domain::{AccountId, Candle, CandleInterval, InstrumentId, StrategyId},
    engine::{
        DataRequirement, DataRole, EventPhase, PortfolioStrategy, ScheduleAnchor,
        ScheduleRequirement, StrategyEvent, StrategyEventKind, TargetPortfolio, TargetWeight,
    },
};
use math::{
    MAX_INSTRUMENT_WEIGHT, covariance_matrix, equal_risk_weights, fill_universe, mean,
    normalize_gross, normalize_long_short, ordinary_least_squares, rank_long_only_low,
    rank_long_short, return_between, shrink_covariance, simple_returns, square_root, variance,
};
use state::{BuiltinState, PairPosition, PairState};

/// Срок жизни дневного target: после него planner обязан запросить новый сигнал.
const DAILY_VALIDITY: Duration = Duration::hours(8);
/// Месячный target допускает исполнение в течение недели с учётом выходных.
const MONTHLY_VALIDITY: Duration = Duration::days(7);
/// Внутридневной прогноз действует только один 30-минутный слот.
const INTRADAY_VALIDITY: Duration = Duration::minutes(30);
/// Закрывающий rebalance после истечения внутридневного сигнала должен быть
/// отправлен быстро, но получает небольшой запас на сетевую задержку runtime.
const EXPIRY_REBALANCE_VALIDITY: Duration = Duration::minutes(5);
/// Верхняя оценка числа получасовых баров календарных суток.
///
/// Запас нужен внутридневным стратегиям: поток может содержать расширенную сессию
/// и аукционы, а opening candle нельзя вытеснить из bounded state до вечернего сигнала.
const MAX_HALF_HOUR_BARS_PER_SESSION: usize = 50;
/// Один history-запрос runtime не принимает больше 2500 закрытых свечей.
const MAX_WARMUP_BARS: usize = 2_500;

/// Один настроенный экземпляр встроенной portfolio-стратегии.
///
/// Экземпляр жёстко связан с одним `strategy_id`, одним Sandbox-счётом и одной
/// торгуемой вселенной. Внутреннее состояние меняется только через `on_event` и
/// сохраняется engine после обработки события.
pub struct BuiltinStrategy {
    strategy_id: StrategyId,
    account_id: AccountId,
    universe: Vec<InstrumentId>,
    spec: BuiltinStrategySpec,
    state: BuiltinState,
}

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
        let strategy =
            spec.strategy
                .clone()
                .build(component_id, account_id.clone(), universe.to_vec())?;
        Ok(WeightedComponent {
            signal_weight: spec.signal_weight,
            strategy,
            last_target: None,
        })
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
                    let Some(target) = &component.last_target else {
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

impl BuiltinStrategySpec {
    /// Проверяет параметры сигнала совместно с торгуемой вселенной.
    ///
    /// Здесь проверяются не только диапазоны чисел, но и структурные требования:
    /// минимум 10 бумаг для ранжирования, отдельный benchmark, ровно один инструмент
    /// для market intraday и достаточная вселенная для pairs/ERC/volatility-managed.
    pub fn validate(&self, universe: &[InstrumentId]) -> Result<()> {
        validate_universe(universe)?;
        match self {
            Self::MovingAverageTrend(config) => {
                if config.fast_window == 0 || config.fast_window >= config.slow_window {
                    return configuration_error(
                        "moving-average windows must satisfy 0 < fast_window < slow_window",
                    );
                }
                validate_warmup_window(config.slow_window, 0, "slow_window")?;
                validate_percent(config.band_percent, "band_percent", true)?;
            }
            Self::RangeBreakout(config) => {
                validate_positive(config.lookback_days, "lookback_days")?;
                validate_warmup_window(config.lookback_days, 1, "lookback_days")?;
                validate_percent(config.filter_percent, "filter_percent", true)?;
            }
            Self::TimeSeriesMomentum(config) => {
                if config.horizons_days.is_empty()
                    || config.horizons_days.contains(&0)
                    || config
                        .horizons_days
                        .windows(2)
                        .any(|pair| pair[0] >= pair[1])
                {
                    return configuration_error(
                        "horizons_days must be a non-empty, strictly increasing positive list",
                    );
                }
                validate_positive(config.volatility_window_days, "volatility_window_days")?;
                validate_warmup_window(
                    config
                        .horizons_days
                        .iter()
                        .copied()
                        .max()
                        .unwrap_or_default()
                        .max(config.volatility_window_days),
                    1,
                    "momentum/volatility window",
                )?;
            }
            Self::CrossSectionalMomentum(config) => {
                validate_ranking_universe(universe)?;
                validate_formation_skip(config.formation_days, config.skip_days)?;
                validate_warmup_window(config.formation_days, 1, "formation_days")?;
                validate_quantile(config.quantile_percent)?;
            }
            Self::IntermediateMomentum(config) => {
                validate_ranking_universe(universe)?;
                if config.end_days_ago == 0 || config.start_days_ago <= config.end_days_ago {
                    return configuration_error(
                        "intermediate momentum requires start_days_ago > end_days_ago > 0",
                    );
                }
                validate_warmup_window(config.start_days_ago, 1, "start_days_ago")?;
                validate_quantile(config.quantile_percent)?;
            }
            Self::FiftyTwoWeekHigh(config) => {
                validate_ranking_universe(universe)?;
                validate_positive(config.lookback_days, "lookback_days")?;
                validate_warmup_window(config.lookback_days, 0, "lookback_days")?;
                validate_quantile(config.quantile_percent)?;
            }
            Self::ResidualMomentum(config) => {
                validate_ranking_universe(universe)?;
                validate_reference(&config.benchmark_uid, universe)?;
                validate_formation_skip(config.formation_days, config.skip_days)?;
                validate_warmup_window(config.formation_days, 1, "formation_days")?;
                validate_quantile(config.quantile_percent)?;
            }
            Self::PairsTrading(config) => {
                if universe.len() < 2 {
                    return configuration_error("pairs-trading requires at least two instruments");
                }
                validate_positive(config.formation_days, "formation_days")?;
                validate_warmup_window(config.formation_days, 0, "formation_days")?;
                validate_positive(config.trading_days, "trading_days")?;
                if config.entry_z_score <= Decimal::ZERO {
                    return configuration_error("entry_z_score must be positive");
                }
            }
            Self::ShortTermReversal(config) => {
                validate_ranking_universe(universe)?;
                validate_reference(&config.benchmark_uid, universe)?;
                validate_positive(config.lookback_days, "lookback_days")?;
                validate_warmup_window(config.lookback_days, 1, "lookback_days")?;
                validate_quantile(config.quantile_percent)?;
            }
            Self::MarketIntradayMomentum(config) => {
                if universe.len() != 1 {
                    return configuration_error(
                        "market-intraday-momentum requires exactly one tradable instrument",
                    );
                }
                if config.opening_window_minutes != 30 || config.final_window_minutes != 30 {
                    return configuration_error(
                        "portfolio runtime v2 market-intraday-momentum supports only 30-minute opening and final windows",
                    );
                }
            }
            Self::IntradaySameClock(config) => {
                validate_ranking_universe(universe)?;
                validate_positive(config.lookback_sessions, "lookback_sessions")?;
                let warmup_bars = config
                    .lookback_sessions
                    .checked_mul(MAX_HALF_HOUR_BARS_PER_SESSION)
                    .and_then(|value| value.checked_add(1))
                    .ok_or_else(|| {
                        Error::Configuration(
                            "lookback_sessions overflows the warmup size".to_owned(),
                        )
                    })?;
                validate_warmup_window(warmup_bars, 0, "lookback_sessions")?;
                validate_quantile(config.quantile_percent)?;
                if config.interval_minutes != 30 {
                    return configuration_error(
                        "portfolio runtime v2 intraday-same-clock supports only 30-minute candles",
                    );
                }
            }
            Self::LowVolatility(config) => {
                validate_ranking_universe(universe)?;
                if config.lookback_months < 2 {
                    return configuration_error("lookback_months must be at least 2");
                }
                validate_warmup_window(config.lookback_months, 1, "lookback_months")?;
                validate_quantile(config.quantile_percent)?;
            }
            Self::BettingAgainstBeta(config) => {
                validate_ranking_universe(universe)?;
                validate_reference(&config.benchmark_uid, universe)?;
                if config.lookback_days < 2 {
                    return configuration_error("lookback_days must be at least 2");
                }
                validate_warmup_window(config.lookback_days, 1, "lookback_days")?;
                if config.shrinkage < Decimal::ZERO || config.shrinkage > Decimal::ONE {
                    return configuration_error("shrinkage must be between 0 and 1");
                }
            }
            Self::EqualRiskContribution(config) => {
                if universe.len() < 2 {
                    return configuration_error(
                        "equal-risk-contribution requires at least two instruments",
                    );
                }
                if config.lookback_days < 2 {
                    return configuration_error("lookback_days must be at least 2");
                }
                validate_warmup_window(config.lookback_days, 1, "lookback_days")?;
                validate_percent(
                    config.diagonal_shrinkage_percent,
                    "diagonal_shrinkage_percent",
                    true,
                )?;
                validate_positive(config.max_iterations, "max_iterations")?;
                if config.tolerance <= Decimal::ZERO {
                    return configuration_error("tolerance must be positive");
                }
            }
            Self::VolatilityManaged(config) => {
                if universe.len() < 4 {
                    return configuration_error(
                        "volatility-managed requires at least four instruments",
                    );
                }
                if config.realized_variance_days < 2 {
                    return configuration_error("realized_variance_days must be at least 2");
                }
                validate_warmup_window(config.realized_variance_days, 1, "realized_variance_days")?;
                validate_percent(
                    config.target_annual_volatility_percent,
                    "target_annual_volatility_percent",
                    false,
                )?;
                if config.max_leverage <= Decimal::ZERO || config.max_leverage > Decimal::from(2) {
                    return configuration_error("max_leverage must be in (0, 2]");
                }
            }
        }
        Ok(())
    }

    /// Создаёт экземпляр с пустой историей после полной валидации параметров.
    ///
    /// Durable state восстанавливает engine отдельным вызовом `restore_state`; такое
    /// разделение не позволяет случайно принять несовместимый state до проверки config.
    pub fn build(
        self,
        strategy_id: StrategyId,
        account_id: AccountId,
        universe: Vec<InstrumentId>,
    ) -> Result<BuiltinStrategy> {
        BuiltinStrategy::new(strategy_id, account_id, universe, self)
    }
}

impl BuiltinStrategy {
    /// Валидирует config и создаёт встроенную стратегию с пустым состоянием.
    pub fn new(
        strategy_id: StrategyId,
        account_id: AccountId,
        universe: Vec<InstrumentId>,
        spec: BuiltinStrategySpec,
    ) -> Result<Self> {
        spec.validate(&universe)?;
        Ok(Self {
            strategy_id,
            account_id,
            universe,
            spec,
            state: BuiltinState::default(),
        })
    }

    /// Описывает свечи и глубину warm-up, необходимые выбранной формуле.
    ///
    /// Дополнительная свеча нужна там, где из `n + 1` цен строятся `n` доходностей.
    /// Бенчмарк добавляется как `Reference`: он участвует в сигнале, но никогда не
    /// попадает в целевые позиции.
    fn requirements(&self) -> Vec<DataRequirement> {
        let (interval, warmup_bars) = match &self.spec {
            BuiltinStrategySpec::MovingAverageTrend(config) => {
                (CandleInterval::OneDay, config.slow_window)
            }
            BuiltinStrategySpec::RangeBreakout(config) => {
                (CandleInterval::OneDay, config.lookback_days + 1)
            }
            BuiltinStrategySpec::TimeSeriesMomentum(config) => (
                CandleInterval::OneDay,
                config
                    .horizons_days
                    .iter()
                    .copied()
                    .max()
                    .unwrap_or_default()
                    .max(config.volatility_window_days)
                    + 1,
            ),
            BuiltinStrategySpec::CrossSectionalMomentum(config) => {
                (CandleInterval::OneDay, config.formation_days + 1)
            }
            BuiltinStrategySpec::IntermediateMomentum(config) => {
                (CandleInterval::OneDay, config.start_days_ago + 1)
            }
            BuiltinStrategySpec::FiftyTwoWeekHigh(config) => {
                (CandleInterval::OneDay, config.lookback_days)
            }
            BuiltinStrategySpec::ResidualMomentum(config) => {
                (CandleInterval::OneDay, config.formation_days + 1)
            }
            BuiltinStrategySpec::PairsTrading(config) => {
                (CandleInterval::OneDay, config.formation_days)
            }
            BuiltinStrategySpec::ShortTermReversal(config) => {
                (CandleInterval::OneDay, config.lookback_days + 1)
            }
            BuiltinStrategySpec::MarketIntradayMomentum(_) => {
                return self.market_intraday_requirements();
            }
            BuiltinStrategySpec::IntradaySameClock(config) => (
                CandleInterval::ThirtyMinutes,
                config
                    .lookback_sessions
                    .saturating_mul(MAX_HALF_HOUR_BARS_PER_SESSION)
                    .saturating_add(1),
            ),
            BuiltinStrategySpec::LowVolatility(config) => {
                (CandleInterval::OneMonth, config.lookback_months + 1)
            }
            BuiltinStrategySpec::BettingAgainstBeta(config) => {
                (CandleInterval::OneDay, config.lookback_days + 1)
            }
            BuiltinStrategySpec::EqualRiskContribution(config) => {
                (CandleInterval::OneDay, config.lookback_days + 1)
            }
            BuiltinStrategySpec::VolatilityManaged(config) => {
                (CandleInterval::OneDay, config.realized_variance_days + 1)
            }
        };

        let mut requirements = self
            .universe
            .iter()
            .cloned()
            .map(|instrument_id| DataRequirement {
                instrument_id,
                role: DataRole::Tradable,
                interval,
                warmup_bars,
                waiting_close: true,
            })
            .collect::<Vec<_>>();
        if let Some(benchmark_uid) = self.benchmark_uid() {
            requirements.push(DataRequirement {
                instrument_id: benchmark_uid.clone(),
                role: DataRole::Reference,
                interval: CandleInterval::OneDay,
                warmup_bars,
                waiting_close: true,
            });
        }
        requirements
    }

    /// Возвращает два ряда для market-intraday: прошлое дневное закрытие и полный
    /// внутридневной хвост, сохраняющий первую свечу до финального окна сессии.
    fn market_intraday_requirements(&self) -> Vec<DataRequirement> {
        let instrument_id = self.universe[0].clone();
        vec![
            DataRequirement {
                instrument_id: instrument_id.clone(),
                role: DataRole::Tradable,
                interval: CandleInterval::OneDay,
                warmup_bars: 2,
                waiting_close: true,
            },
            DataRequirement {
                instrument_id,
                role: DataRole::Tradable,
                interval: CandleInterval::ThirtyMinutes,
                // Одного бара недостаточно: bounded state вытеснил бы opening
                // candle задолго до closing trigger. Суточный запас сохраняет её.
                warmup_bars: MAX_HALF_HOUR_BARS_PER_SESSION,
                waiting_close: true,
            },
        ]
    }

    /// Возвращает внешний benchmark только для стратегий с рыночным фактором.
    fn benchmark_uid(&self) -> Option<&InstrumentId> {
        match &self.spec {
            BuiltinStrategySpec::ResidualMomentum(config) => Some(&config.benchmark_uid),
            BuiltinStrategySpec::ShortTermReversal(config) => Some(&config.benchmark_uid),
            BuiltinStrategySpec::BettingAgainstBeta(config) => Some(&config.benchmark_uid),
            _ => None,
        }
    }

    /// Ограничивает хранимую историю требуемым warm-up плюс небольшим запасом.
    ///
    /// Запас в восемь свечей позволяет пережить пограничные сессии и обновления,
    /// не превращая сериализованный state в бесконечный архив рынка.
    fn maximum_bars_for(&self, instrument_id: &InstrumentId, interval: CandleInterval) -> usize {
        self.requirements()
            .into_iter()
            .filter(|requirement| {
                &requirement.instrument_id == instrument_id && requirement.interval == interval
            })
            .map(|requirement| requirement.warmup_bars.saturating_add(8))
            .max()
            .unwrap_or(1)
    }

    /// Принимает только закрытую свечу из явно объявленных data requirements.
    fn accepts(&self, candle: &Candle) -> bool {
        candle.complete
            && self.requirements().iter().any(|requirement| {
                requirement.instrument_id == candle.instrument_id
                    && requirement.interval == candle.interval
            })
    }

    /// Описывает моменты, когда разрешено пересчитать scheduled target.
    ///
    /// Большинство сигналов исполняется на следующем открытии после закрытых данных;
    /// market-intraday — за 30 минут до аукциона закрытия; same-clock реагирует прямо
    /// на закрытие очередной 30-минутной свечи. Обе intraday-стратегии получают
    /// официальные open/close: open разрешает принимать регулярные свечи, close
    /// гарантированно снимает их cached target.
    fn schedules(&self) -> Vec<ScheduleRequirement> {
        match &self.spec {
            BuiltinStrategySpec::IntradaySameClock(_) => vec![
                ScheduleRequirement {
                    anchor: ScheduleAnchor::SessionOpen,
                    offset_seconds: 0,
                },
                ScheduleRequirement {
                    anchor: ScheduleAnchor::SessionClose,
                    offset_seconds: 0,
                },
            ],
            BuiltinStrategySpec::MarketIntradayMomentum(config) => vec![
                ScheduleRequirement {
                    anchor: ScheduleAnchor::SessionOpen,
                    offset_seconds: 0,
                },
                ScheduleRequirement {
                    anchor: ScheduleAnchor::ClosingAuctionStart,
                    offset_seconds: -i64::from(config.final_window_minutes) * 60,
                },
                ScheduleRequirement {
                    anchor: ScheduleAnchor::SessionClose,
                    offset_seconds: 0,
                },
            ],
            _ => vec![ScheduleRequirement {
                anchor: ScheduleAnchor::SessionOpen,
                offset_seconds: 0,
            }],
        }
    }

    /// Указывает стратегии, для которых повторный schedule внутри месяца игнорируется.
    fn should_rebalance_monthly(&self) -> bool {
        matches!(
            &self.spec,
            BuiltinStrategySpec::TimeSeriesMomentum(_)
                | BuiltinStrategySpec::CrossSectionalMomentum(_)
                | BuiltinStrategySpec::IntermediateMomentum(_)
                | BuiltinStrategySpec::FiftyTwoWeekHigh(_)
                | BuiltinStrategySpec::ResidualMomentum(_)
                | BuiltinStrategySpec::LowVolatility(_)
                | BuiltinStrategySpec::BettingAgainstBeta(_)
                | BuiltinStrategySpec::EqualRiskContribution(_)
                | BuiltinStrategySpec::VolatilityManaged(_)
        )
    }

    /// Вычисляет веса выбранной стратегии и оборачивает их в полный target.
    ///
    /// `None` означает «данных пока недостаточно», а не пустой портфель. Явное
    /// закрытие всех позиций представляется `Some(target)` с нулевыми весами.
    fn scheduled_target(
        &mut self,
        event: &StrategyEvent,
        session_date: chrono::NaiveDate,
    ) -> Result<Option<TargetPortfolio>> {
        if !self.latest_sources_are_aligned() {
            return Ok(None);
        }
        let spec = self.spec.clone();
        let Some(weights) = (match &spec {
            BuiltinStrategySpec::MovingAverageTrend(config) => self.moving_average_weights(config),
            BuiltinStrategySpec::RangeBreakout(config) => Ok(self.range_breakout_weights(config)),
            BuiltinStrategySpec::TimeSeriesMomentum(config) => {
                self.time_series_momentum_weights(config)
            }
            BuiltinStrategySpec::CrossSectionalMomentum(config) => {
                Ok(self.cross_sectional_momentum_weights(config))
            }
            BuiltinStrategySpec::IntermediateMomentum(config) => {
                Ok(self.intermediate_momentum_weights(config))
            }
            BuiltinStrategySpec::FiftyTwoWeekHigh(config) => {
                Ok(self.fifty_two_week_high_weights(config))
            }
            BuiltinStrategySpec::ResidualMomentum(config) => self.residual_momentum_weights(config),
            BuiltinStrategySpec::PairsTrading(config) => self.pairs_weights(config),
            BuiltinStrategySpec::ShortTermReversal(config) => {
                Ok(self.short_term_reversal_weights(config))
            }
            BuiltinStrategySpec::MarketIntradayMomentum(_) => {
                Ok(self.market_intraday_weights(session_date))
            }
            BuiltinStrategySpec::IntradaySameClock(_) => Ok(None),
            BuiltinStrategySpec::LowVolatility(config) => self.low_volatility_weights(config),
            BuiltinStrategySpec::BettingAgainstBeta(config) => {
                self.betting_against_beta_weights(config)
            }
            BuiltinStrategySpec::EqualRiskContribution(config) => {
                self.equal_risk_contribution_weights(config)
            }
            BuiltinStrategySpec::VolatilityManaged(config) => {
                self.volatility_managed_weights(config)
            }
        })?
        else {
            return Ok(None);
        };
        let validity = if matches!(&self.spec, BuiltinStrategySpec::MarketIntradayMomentum(_)) {
            INTRADAY_VALIDITY
        } else if self.should_rebalance_monthly() {
            MONTHLY_VALIDITY
        } else {
            DAILY_VALIDITY
        };
        Ok(Some(self.make_target(event, weights, validity)))
    }

    /// Проверяет, что все ряды одного интервала заканчиваются одной и той же свечой.
    ///
    /// Без этого кросс-секционный ранг мог бы сравнить сегодняшнюю цену одной бумаги
    /// со вчерашней ценой другой и создать look-ahead/асинхронное смещение.
    fn latest_sources_are_aligned(&self) -> bool {
        let requirements = self.requirements();
        requirements.iter().enumerate().all(|(index, left)| {
            let left_time = self
                .state
                .candles(&left.instrument_id, left.interval)
                .last()
                .map(|candle| candle.time);
            left_time.is_some()
                && requirements[index + 1..]
                    .iter()
                    .filter(|right| right.interval == left.interval)
                    .all(|right| {
                        self.state
                            .candles(&right.instrument_id, right.interval)
                            .last()
                            .is_some_and(|candle| Some(candle.time) == left_time)
                    })
        })
    }

    /// Строит сигнал SMA: `+1` выше верхней полосы, `-1` ниже нижней, иначе `0`.
    ///
    /// После расчёта сырые сигналы отдельно нормализуются до `+0,5/-0,5` gross.
    fn moving_average_weights(
        &self,
        config: &MovingAverageTrendConfig,
    ) -> Result<Option<Vec<(InstrumentId, Decimal)>>> {
        let band = config.band_percent / Decimal::from(100);
        let mut raw = Vec::new();
        for instrument_id in &self.universe {
            let candles = self.state.candles(instrument_id, CandleInterval::OneDay);
            if candles.len() < config.slow_window {
                return Ok(None);
            }
            let slow_slice = &candles[candles.len() - config.slow_window..];
            let fast_slice = &candles[candles.len() - config.fast_window..];
            let slow = mean(
                &slow_slice
                    .iter()
                    .map(|candle| candle.close.0)
                    .collect::<Vec<_>>(),
            )?;
            let fast = mean(
                &fast_slice
                    .iter()
                    .map(|candle| candle.close.0)
                    .collect::<Vec<_>>(),
            )?;
            let signal = if fast > slow * (Decimal::ONE + band) {
                Decimal::ONE
            } else if fast < slow * (Decimal::ONE - band) {
                -Decimal::ONE
            } else {
                Decimal::ZERO
            };
            raw.push((instrument_id.clone(), signal));
        }
        Ok(Some(normalize_long_short(&self.universe, &raw)))
    }

    /// Сравнивает последнее закрытие с диапазоном **предыдущих** свечей.
    ///
    /// Текущая свеча исключена из high/low, иначе цена не могла бы пробить максимум
    /// собственного окна. Процентный фильтр уменьшает реакцию на малые проколы.
    fn range_breakout_weights(
        &self,
        config: &RangeBreakoutConfig,
    ) -> Option<Vec<(InstrumentId, Decimal)>> {
        let filter = config.filter_percent / Decimal::from(100);
        let mut raw = Vec::new();
        for instrument_id in &self.universe {
            let candles = self.state.candles(instrument_id, CandleInterval::OneDay);
            if candles.len() < config.lookback_days + 1 {
                return None;
            }
            let previous = &candles[candles.len() - config.lookback_days - 1..candles.len() - 1];
            let high = previous
                .iter()
                .map(|candle| candle.high.0)
                .max()
                .unwrap_or_default();
            let low = previous
                .iter()
                .map(|candle| candle.low.0)
                .min()
                .unwrap_or_default();
            let close = candles
                .last()
                .map_or(Decimal::ZERO, |candle| candle.close.0);
            let signal = if close > high * (Decimal::ONE + filter) {
                Decimal::ONE
            } else if close < low * (Decimal::ONE - filter) {
                -Decimal::ONE
            } else {
                Decimal::ZERO
            };
            raw.push((instrument_id.clone(), signal));
        }
        Some(normalize_long_short(&self.universe, &raw))
    }

    /// Суммирует знаки доходностей разных горизонтов и делит их на волатильность.
    ///
    /// Сырой score равен `Σ sign(return_h) / stdev(daily_returns)`. Таким образом,
    /// направление задаёт согласованность трендов, а inverse-vol снижает вклад более
    /// шумных инструментов перед нормализацией long/short сторон.
    fn time_series_momentum_weights(
        &self,
        config: &TimeSeriesMomentumConfig,
    ) -> Result<Option<Vec<(InstrumentId, Decimal)>>> {
        let maximum_horizon = config
            .horizons_days
            .iter()
            .copied()
            .max()
            .unwrap_or_default();
        let mut raw = Vec::new();
        for instrument_id in &self.universe {
            let candles = self.state.candles(instrument_id, CandleInterval::OneDay);
            if candles.len() <= maximum_horizon || candles.len() <= config.volatility_window_days {
                return Ok(None);
            }
            let sign_sum = config
                .horizons_days
                .iter()
                .map(|horizon| {
                    return_between(candles, *horizon, 0).map_or(Decimal::ZERO, |value| match value
                        .cmp(&Decimal::ZERO)
                    {
                        Ordering::Greater => Decimal::ONE,
                        Ordering::Less => -Decimal::ONE,
                        Ordering::Equal => Decimal::ZERO,
                    })
                })
                .sum::<Decimal>();
            let returns =
                simple_returns(&candles[candles.len() - config.volatility_window_days - 1..])?;
            let volatility = square_root(variance(&returns)?)?;
            let score = if volatility.is_zero() {
                Decimal::ZERO
            } else {
                sign_sum / volatility
            };
            raw.push((instrument_id.clone(), score));
        }
        Ok(Some(normalize_long_short(&self.universe, &raw)))
    }

    /// Ранжирует доходность `P[t-skip] / P[t-formation] - 1`.
    ///
    /// Верхний хвост получает long, нижний — short; каждая сторона занимает 0,5 gross.
    fn cross_sectional_momentum_weights(
        &self,
        config: &CrossSectionalMomentumConfig,
    ) -> Option<Vec<(InstrumentId, Decimal)>> {
        let mut scores = Vec::new();
        for instrument_id in &self.universe {
            let candles = self.state.candles(instrument_id, CandleInterval::OneDay);
            let score = return_between(candles, config.formation_days, config.skip_days)?;
            scores.push((instrument_id.clone(), score));
        }
        rank_long_short(&self.universe, scores, config.quantile_percent, true)
    }

    /// Ранжирует доходность на промежуточном участке, по умолчанию 252–126 дней назад.
    fn intermediate_momentum_weights(
        &self,
        config: &IntermediateMomentumConfig,
    ) -> Option<Vec<(InstrumentId, Decimal)>> {
        let mut scores = Vec::new();
        for instrument_id in &self.universe {
            let candles = self.state.candles(instrument_id, CandleInterval::OneDay);
            let score = return_between(candles, config.start_days_ago, config.end_days_ago)?;
            scores.push((instrument_id.clone(), score));
        }
        rank_long_short(&self.universe, scores, config.quantile_percent, true)
    }

    /// Ранжирует отношение последнего закрытия к максимуму `high` formation-окна.
    ///
    /// Значение около единицы означает, что инструмент торгуется рядом с годовым
    /// максимумом; высокий хвост идёт в long, низкий — в short.
    fn fifty_two_week_high_weights(
        &self,
        config: &FiftyTwoWeekHighConfig,
    ) -> Option<Vec<(InstrumentId, Decimal)>> {
        let mut scores = Vec::new();
        for instrument_id in &self.universe {
            let candles = self.state.candles(instrument_id, CandleInterval::OneDay);
            if candles.len() < config.lookback_days {
                return None;
            }
            let window = &candles[candles.len() - config.lookback_days..];
            let high = window
                .iter()
                .map(|candle| candle.high.0)
                .max()
                .unwrap_or_default();
            if high <= Decimal::ZERO {
                return None;
            }
            let close = window.last().map_or(Decimal::ZERO, |candle| candle.close.0);
            scores.push((instrument_id.clone(), close / high));
        }
        rank_long_short(&self.universe, scores, config.quantile_percent, true)
    }

    /// Ранжирует накопленную доходность, не объяснённую рыночным benchmark.
    ///
    /// Для каждой бумаги оценивается `r_i = alpha + beta*r_m + epsilon`. Score —
    /// `sum(epsilon) / stdev(epsilon)` на formation-части без последних `skip_days`.
    /// Деление на стандартное отклонение делает остатки разных бумаг сопоставимыми.
    fn residual_momentum_weights(
        &self,
        config: &ResidualMomentumConfig,
    ) -> Result<Option<Vec<(InstrumentId, Decimal)>>> {
        let benchmark = self
            .state
            .candles(&config.benchmark_uid, CandleInterval::OneDay);
        if benchmark.len() < config.formation_days + 1 {
            return Ok(None);
        }
        let benchmark_returns =
            simple_returns(&benchmark[benchmark.len() - config.formation_days - 1..])?;
        // Остатки последних skip_days не входят в score: это отделяет formation
        // сигнал от недавнего разворота, как в классической momentum-конструкции.
        let score_length = config.formation_days - config.skip_days;
        let mut scores = Vec::new();
        for instrument_id in &self.universe {
            let candles = self.state.candles(instrument_id, CandleInterval::OneDay);
            if candles.len() < config.formation_days + 1 {
                return Ok(None);
            }
            let returns = simple_returns(&candles[candles.len() - config.formation_days - 1..])?;
            let Ok((alpha, beta)) = ordinary_least_squares(&returns, &benchmark_returns) else {
                return Ok(None);
            };
            // epsilon_t = r_i,t - alpha - beta * r_market,t.
            let residuals = returns
                .iter()
                .zip(&benchmark_returns)
                .map(|(value, benchmark)| *value - alpha - beta * *benchmark)
                .collect::<Vec<_>>();
            let usable = &residuals[..score_length];
            let standard_deviation = square_root(variance(usable)?)?;
            if standard_deviation.is_zero() {
                return Ok(None);
            }
            scores.push((
                instrument_id.clone(),
                usable.iter().copied().sum::<Decimal>() / standard_deviation,
            ));
        }
        Ok(rank_long_short(
            &self.universe,
            scores,
            config.quantile_percent,
            true,
        ))
    }

    /// Обновляет состояние пары и возвращает target на возврат спреда к среднему.
    ///
    /// Спред определяется как `left/left_base - right/right_base`, z-score — как
    /// `(spread - formation_mean) / formation_stdev`. При `z >= entry` открывается
    /// short spread, при `z <= -entry` — long spread. Выход происходит на пересечении
    /// среднего (`z = 0`) либо по истечении `trading_days`; каждая нога весит 0,5.
    fn pairs_weights(
        &mut self,
        config: &PairsTradingConfig,
    ) -> Result<Option<Vec<(InstrumentId, Decimal)>>> {
        if self.state.pair.is_none() {
            self.state.pair = self.select_pair(config)?;
        }
        let Some(mut pair) = self.state.pair.clone() else {
            return Ok(None);
        };
        let left = self.state.candles(&pair.left, CandleInterval::OneDay);
        let right = self.state.candles(&pair.right, CandleInterval::OneDay);
        let (Some(left_last), Some(right_last)) = (left.last(), right.last()) else {
            return Ok(None);
        };
        // День, которым закончился formation-период, только фиксирует пару и
        // пороги. Первый торговый z-score считается на следующей закрытой сессии.
        if left_last.time <= pair.formation_completed_at
            || right_last.time <= pair.formation_completed_at
        {
            self.state.pair = Some(pair);
            return Ok(None);
        }
        if pair.left_base <= Decimal::ZERO
            || pair.right_base <= Decimal::ZERO
            || pair.standard_deviation <= Decimal::ZERO
        {
            return Ok(None);
        }
        let spread = left_last.close.0 / pair.left_base - right_last.close.0 / pair.right_base;
        let z_score = (spread - pair.mean_spread) / pair.standard_deviation;

        // Позиция направлена против отклонения: дорогая относительно formation
        // сторона продаётся, дешёвая покупается. Уже открытая позиция не переворачивается
        // напрямую — сначала она закрывается при возврате к среднему.
        pair.position = match pair.position {
            PairPosition::Flat if z_score >= config.entry_z_score => PairPosition::ShortSpread,
            PairPosition::Flat if z_score <= -config.entry_z_score => PairPosition::LongSpread,
            PairPosition::ShortSpread if z_score <= Decimal::ZERO => PairPosition::Flat,
            PairPosition::LongSpread if z_score >= Decimal::ZERO => PairPosition::Flat,
            current => current,
        };
        pair.sessions_remaining = pair.sessions_remaining.saturating_sub(1);
        if pair.sessions_remaining == 0 {
            pair.position = PairPosition::Flat;
        }

        let selected = match pair.position {
            PairPosition::Flat => Vec::new(),
            PairPosition::LongSpread => vec![
                (pair.left.clone(), Decimal::new(5, 1)),
                (pair.right.clone(), -Decimal::new(5, 1)),
            ],
            PairPosition::ShortSpread => vec![
                (pair.left.clone(), -Decimal::new(5, 1)),
                (pair.right.clone(), Decimal::new(5, 1)),
            ],
        };
        self.state.pair = (pair.sessions_remaining > 0).then_some(pair);
        Ok(Some(fill_universe(&self.universe, &selected)))
    }

    /// Выбирает пару с минимальной суммой квадратов нормированного спреда.
    ///
    /// Обе цены нормируются на первое закрытие formation-окна. Для каждой комбинации
    /// считается `distance = Σ(left_norm - right_norm)²`; минимальная distance означает
    /// наиболее похожую историческую траекторию. Среднее и sigma победителя фиксируются
    /// на весь trading-период, чтобы будущее не меняло пороги задним числом.
    fn select_pair(&self, config: &PairsTradingConfig) -> Result<Option<PairState>> {
        let mut best: Option<(
            InstrumentId,
            InstrumentId,
            Decimal,
            Vec<Decimal>,
            Decimal,
            Decimal,
        )> = None;
        for left_index in 0..self.universe.len() {
            let left_id = &self.universe[left_index];
            let left = self.state.candles(left_id, CandleInterval::OneDay);
            if left.len() < config.formation_days {
                return Ok(None);
            }
            let left = &left[left.len() - config.formation_days..];
            let left_base = left[0].close.0;
            if left_base <= Decimal::ZERO {
                continue;
            }
            for right_id in &self.universe[left_index + 1..] {
                let right = self.state.candles(right_id, CandleInterval::OneDay);
                if right.len() < config.formation_days {
                    return Ok(None);
                }
                let right = &right[right.len() - config.formation_days..];
                let right_base = right[0].close.0;
                if right_base <= Decimal::ZERO {
                    continue;
                }
                let spreads = left
                    .iter()
                    .zip(right)
                    .map(|(left, right)| left.close.0 / left_base - right.close.0 / right_base)
                    .collect::<Vec<_>>();
                // Это SSD-критерий Gatev et al.; меньшая сумма означает более тесную пару.
                let distance = spreads
                    .iter()
                    .map(|spread| *spread * *spread)
                    .sum::<Decimal>();
                let replace = best
                    .as_ref()
                    .is_none_or(|(_, _, best_distance, _, _, _)| distance < *best_distance);
                if replace {
                    best = Some((
                        left_id.clone(),
                        right_id.clone(),
                        distance,
                        spreads,
                        left_base,
                        right_base,
                    ));
                }
            }
        }
        let Some((left, right, _, spreads, left_base, right_base)) = best else {
            return Ok(None);
        };
        let mean_spread = mean(&spreads)?;
        let standard_deviation = square_root(variance(&spreads)?)?;
        if standard_deviation.is_zero() {
            return Ok(None);
        }
        let formation_completed_at = self
            .state
            .candles(&left, CandleInterval::OneDay)
            .last()
            .expect("selected pair has a complete formation window")
            .time;
        Ok(Some(PairState {
            left,
            right,
            left_base,
            right_base,
            mean_spread,
            standard_deviation,
            formation_completed_at,
            sessions_remaining: config.trading_days,
            position: PairPosition::Flat,
        }))
    }

    /// Ранжирует краткосрочную доходность бумаги за вычетом доходности benchmark.
    ///
    /// Здесь `high_is_long = false`: нижний хвост недавних проигравших покупается,
    /// верхний хвост победителей продаётся, по 0,5 gross на сторону.
    fn short_term_reversal_weights(
        &self,
        config: &ShortTermReversalConfig,
    ) -> Option<Vec<(InstrumentId, Decimal)>> {
        let benchmark = self
            .state
            .candles(&config.benchmark_uid, CandleInterval::OneDay);
        let benchmark_return = return_between(benchmark, config.lookback_days, 0)?;
        let mut scores = Vec::new();
        for instrument_id in &self.universe {
            let candles = self.state.candles(instrument_id, CandleInterval::OneDay);
            let value = return_between(candles, config.lookback_days, 0)?;
            scores.push((instrument_id.clone(), value - benchmark_return));
        }
        rank_long_short(&self.universe, scores, config.quantile_percent, false)
    }

    /// Переносит знак движения первых 30 минут в позицию последних 30 минут сессии.
    ///
    /// Рост первой свечи относительно предыдущего дневного close даёт `+0,5`,
    /// падение — `-0,5`, отсутствие изменения — нулевой вес.
    fn market_intraday_weights(
        &self,
        session_date: chrono::NaiveDate,
    ) -> Option<Vec<(InstrumentId, Decimal)>> {
        // Сама дата свечи недостаточна: T-Invest может отдавать бары расширенной
        // сессии. Разрешение появляется только после официального SessionOpen.
        if self.state.active_intraday_session != Some(session_date) {
            return None;
        }
        let session_open_at = self.state.intraday_session_open_at?;
        let instrument_id = &self.universe[0];
        let daily = self.state.candles(instrument_id, CandleInterval::OneDay);
        let previous_close = daily
            .iter()
            .rev()
            .find(|candle| candle.time.date_naive() < session_date)
            .map(|candle| candle.close.0);
        let opening_close = self
            .state
            .candles(instrument_id, CandleInterval::ThirtyMinutes)
            .iter()
            .filter(|candle| {
                candle.time.date_naive() == session_date && candle.time >= session_open_at
            })
            .min_by_key(|candle| candle.time)
            .map(|candle| candle.close.0);
        let (Some(previous_close), Some(opening_close)) = (previous_close, opening_close) else {
            return None;
        };
        if previous_close <= Decimal::ZERO {
            return None;
        }
        let weight = match opening_close.cmp(&previous_close) {
            Ordering::Greater => MAX_INSTRUMENT_WEIGHT,
            Ordering::Less => -MAX_INSTRUMENT_WEIGHT,
            Ordering::Equal => Decimal::ZERO,
        };
        Some(vec![(instrument_id.clone(), weight)])
    }

    /// Прогнозирует следующий 30-минутный слот по его среднему возврату в прошлых сессиях.
    ///
    /// После закрытия текущей свечи определяется время следующего слота. Для каждой
    /// бумаги берутся последние `lookback_sessions` свечей с теми же часом и минутой,
    /// затем средние доходности ранжируются в long/short хвосты. Target создаётся лишь
    /// после получения синхронной свечи для всей вселенной и только один раз на timestamp.
    fn intraday_same_clock_target(
        &mut self,
        event: &StrategyEvent,
        candle: &Candle,
    ) -> Result<Option<TargetPortfolio>> {
        let BuiltinStrategySpec::IntradaySameClock(config) = &self.spec else {
            return Ok(None);
        };
        let Some(session_open_at) = self.state.intraday_session_open_at else {
            return Ok(None);
        };
        if self.state.active_intraday_session != Some(candle.time.date_naive())
            || candle.time < session_open_at
        {
            // Только официальный SessionOpen включает live-сигналы. При старте
            // процесса посреди дня без восстановленного open-marker стратегия
            // безопасно ждёт следующую сессию, сохраняя свечи как историю.
            return Ok(None);
        }
        if self
            .state
            .last_intraday_session_close
            .is_some_and(|closed| candle.time.date_naive() <= closed)
        {
            // Stream и schedule доставляются независимо. Если финальная свеча
            // пришла уже после official close, она остаётся в истории,
            // но не может породить новую overnight-позицию.
            return Ok(None);
        }
        if self.state.last_intraday_source == Some(candle.time) {
            return Ok(None);
        }
        let complete_cross_section = self.universe.iter().all(|instrument_id| {
            self.state
                .candles(instrument_id, CandleInterval::ThirtyMinutes)
                .last()
                .is_some_and(|latest| latest.time == candle.time)
        });
        if !complete_cross_section {
            return Ok(None);
        }

        let next_slot = candle.time + Duration::minutes(i64::from(config.interval_minutes));
        let mut scores = Vec::new();
        for instrument_id in &self.universe {
            let same_slot_returns = self
                .state
                .candles(instrument_id, CandleInterval::ThirtyMinutes)
                .iter()
                .filter(|candidate| {
                    candidate.time < next_slot
                        && candidate.time.hour() == next_slot.hour()
                        && candidate.time.minute() == next_slot.minute()
                        && candidate.open.0 > Decimal::ZERO
                })
                .rev()
                .take(config.lookback_sessions)
                .map(|candidate| candidate.close.0 / candidate.open.0 - Decimal::ONE)
                .collect::<Vec<_>>();
            if same_slot_returns.len() < config.lookback_sessions {
                return Ok(None);
            }
            scores.push((instrument_id.clone(), mean(&same_slot_returns)?));
        }
        let Some(weights) = rank_long_short(&self.universe, scores, config.quantile_percent, true)
        else {
            return Ok(None);
        };
        self.state.last_intraday_source = Some(candle.time);
        Ok(Some(self.make_target(event, weights, INTRADAY_VALIDITY)))
    }

    /// Выбирает нижний хвост по стандартному отклонению месячных доходностей.
    ///
    /// Портфель long-only и равновзвешенный среди выбранных бумаг; индивидуальный
    /// вес дополнительно ограничивается 0,5 strategy capital.
    fn low_volatility_weights(
        &self,
        config: &LowVolatilityConfig,
    ) -> Result<Option<Vec<(InstrumentId, Decimal)>>> {
        let mut scores = Vec::new();
        for instrument_id in &self.universe {
            let candles = self.state.candles(instrument_id, CandleInterval::OneMonth);
            if candles.len() < config.lookback_months + 1 {
                return Ok(None);
            }
            let returns = simple_returns(&candles[candles.len() - config.lookback_months - 1..])?;
            scores.push((instrument_id.clone(), square_root(variance(&returns)?)?));
        }
        Ok(rank_long_only_low(
            &self.universe,
            scores,
            config.quantile_percent,
        ))
    }

    /// Строит BAB-портфель: long low-beta и short high-beta с gross до 2.
    ///
    /// Сначала OLS-beta притягивается к единице:
    /// `shrunk_beta = shrinkage*beta + (1-shrinkage)*1`. Внутри каждой половины
    /// бумаги получают inverse-|beta| веса (с floor 0,0001). Gross ног выбирается
    /// так, чтобы их beta-экспозиции совпали: при `L + S = 2` используется
    /// `L = 2*beta_high/(beta_low+beta_high)`, `S = 2-L`. Затем L ограничивается
    /// диапазоном 0,5..1,5, а каждый инструмент — весом 0,5.
    fn betting_against_beta_weights(
        &self,
        config: &BettingAgainstBetaConfig,
    ) -> Result<Option<Vec<(InstrumentId, Decimal)>>> {
        let benchmark = self
            .state
            .candles(&config.benchmark_uid, CandleInterval::OneDay);
        if benchmark.len() < config.lookback_days + 1 {
            return Ok(None);
        }
        let benchmark_returns =
            simple_returns(&benchmark[benchmark.len() - config.lookback_days - 1..])?;
        if variance(&benchmark_returns)?.is_zero() {
            return Ok(None);
        }
        let mut betas = Vec::new();
        for instrument_id in &self.universe {
            let candles = self.state.candles(instrument_id, CandleInterval::OneDay);
            if candles.len() < config.lookback_days + 1 {
                return Ok(None);
            }
            let returns = simple_returns(&candles[candles.len() - config.lookback_days - 1..])?;
            let (_, beta) = ordinary_least_squares(&returns, &benchmark_returns)?;
            // Shrinkage к beta=1 уменьшает ошибку экстремальных выборочных оценок.
            let shrunk = config.shrinkage * beta + (Decimal::ONE - config.shrinkage);
            betas.push((instrument_id.clone(), shrunk));
        }
        betas.sort_by(|left, right| {
            left.1
                .cmp(&right.1)
                .then_with(|| left.0.as_str().cmp(right.0.as_str()))
        });
        let leg_size = betas.len() / 2;
        if leg_size == 0 {
            return Ok(None);
        }
        let low = &betas[..leg_size];
        let high = &betas[betas.len() - leg_size..];
        // Floor защищает inverse-beta веса от взрыва при beta около нуля.
        let beta_floor = Decimal::new(1, 4);
        let low_inverse = low
            .iter()
            .map(|(_, beta)| Decimal::ONE / beta.abs().max(beta_floor))
            .collect::<Vec<_>>();
        let high_inverse = high
            .iter()
            .map(|(_, beta)| Decimal::ONE / beta.abs().max(beta_floor))
            .collect::<Vec<_>>();
        let low_inverse_sum = low_inverse.iter().copied().sum::<Decimal>();
        let high_inverse_sum = high_inverse.iter().copied().sum::<Decimal>();
        if low_inverse_sum.is_zero() || high_inverse_sum.is_zero() {
            return Ok(None);
        }
        let low_unit = low_inverse
            .into_iter()
            .map(|weight| weight / low_inverse_sum)
            .collect::<Vec<_>>();
        let high_unit = high_inverse
            .into_iter()
            .map(|weight| weight / high_inverse_sum)
            .collect::<Vec<_>>();
        let low_beta = low
            .iter()
            .zip(&low_unit)
            .map(|((_, beta), weight)| *beta * *weight)
            .sum::<Decimal>()
            .max(beta_floor);
        let high_beta = high
            .iter()
            .zip(&high_unit)
            .map(|((_, beta), weight)| *beta * *weight)
            .sum::<Decimal>()
            .max(beta_floor);
        // Решение системы L + S = 2 и L*beta_low = S*beta_high.
        let mut long_gross = Decimal::from(2) * high_beta / (low_beta + high_beta);
        long_gross = long_gross.clamp(Decimal::new(5, 1), Decimal::new(15, 1));
        let short_gross = Decimal::from(2) - long_gross;
        let mut selected = low
            .iter()
            .zip(low_unit)
            .map(|((instrument_id, _), weight)| {
                (
                    instrument_id.clone(),
                    (long_gross * weight).min(MAX_INSTRUMENT_WEIGHT),
                )
            })
            .collect::<Vec<_>>();
        selected.extend(
            high.iter()
                .zip(high_unit)
                .map(|((instrument_id, _), weight)| {
                    (
                        instrument_id.clone(),
                        -(short_gross * weight).min(MAX_INSTRUMENT_WEIGHT),
                    )
                }),
        );
        Ok(Some(fill_universe(&self.universe, &selected)))
    }

    /// Строит long-only ERC-портфель по shrunk ковариационной матрице.
    ///
    /// После оценки `Σ` решатель ищет веса, для которых каждый вклад в дисперсию
    /// `w_i(Σw)_i` одинаков. Полученные веса суммируются к единице и затем каждый
    /// ограничивается 0,5. Остаток перераспределяется между остальными бумагами,
    /// поэтому при как минимум двух инструментах long gross остаётся равен единице.
    fn equal_risk_contribution_weights(
        &self,
        config: &EqualRiskContributionConfig,
    ) -> Result<Option<Vec<(InstrumentId, Decimal)>>> {
        let mut returns_by_instrument = Vec::new();
        for instrument_id in &self.universe {
            let candles = self.state.candles(instrument_id, CandleInterval::OneDay);
            if candles.len() < config.lookback_days + 1 {
                return Ok(None);
            }
            returns_by_instrument.push(simple_returns(
                &candles[candles.len() - config.lookback_days - 1..],
            )?);
        }
        let mut matrix = covariance_matrix(&returns_by_instrument)?;
        // Диагональный shrinkage сохраняет дисперсии и уменьшает только ковариации.
        shrink_covariance(
            &mut matrix,
            config.diagonal_shrinkage_percent / Decimal::from(100),
        );
        let weights = equal_risk_weights(&matrix, config.max_iterations, config.tolerance)?;
        let selected = self
            .universe
            .iter()
            .cloned()
            .zip(weights)
            .collect::<Vec<_>>();
        Ok(Some(normalize_gross(
            &self.universe,
            &selected,
            Decimal::ONE,
        )))
    }

    /// Масштабирует равновзвешенную корзину обратно реализованной дисперсии.
    ///
    /// Сначала для каждого дня строится средняя доходность вселенной, затем
    /// `RV_annual = sample_variance(portfolio_returns) * 252`. Итоговый gross равен
    /// `min(max_leverage, target_volatility² / RV_annual)` и распределяется поровну
    /// с индивидуальным cap 0,5. Это именно inverse-variance, а не target-vol
    /// `target / realized_vol`. Исследовательская формула может дать 2, но
    /// исполнимый long-only target дополнительно ограничен hard net limit 1.
    fn volatility_managed_weights(
        &self,
        config: &VolatilityManagedConfig,
    ) -> Result<Option<Vec<(InstrumentId, Decimal)>>> {
        let mut returns_by_instrument = Vec::new();
        for instrument_id in &self.universe {
            let candles = self.state.candles(instrument_id, CandleInterval::OneDay);
            if candles.len() < config.realized_variance_days + 1 {
                return Ok(None);
            }
            returns_by_instrument.push(simple_returns(
                &candles[candles.len() - config.realized_variance_days - 1..],
            )?);
        }
        let mut portfolio_returns = Vec::with_capacity(config.realized_variance_days);
        let count = Decimal::from(
            u64::try_from(returns_by_instrument.len())
                .map_err(|_| Error::Configuration("universe is too large".to_owned()))?,
        );
        for index in 0..config.realized_variance_days {
            // Равные веса на уровне доходностей создают рыночную корзину до leverage.
            portfolio_returns.push(
                returns_by_instrument
                    .iter()
                    .map(|returns| returns[index])
                    .sum::<Decimal>()
                    / count,
            );
        }
        let annualized_variance = variance(&portfolio_returns)? * Decimal::from(252);
        let target_volatility = config.target_annual_volatility_percent / Decimal::from(100);
        // Нулевая/отрицательная оценка variance не используется как бесконечное
        // плечо: безопасный результат в таком случае — полностью закрытый портфель.
        let requested_leverage = if annualized_variance <= Decimal::ZERO {
            Decimal::ZERO
        } else {
            (target_volatility * target_volatility / annualized_variance)
                .clamp(Decimal::ZERO, config.max_leverage)
        };
        // У long-only корзины net равен gross. Поэтому запрошенное формулой
        // плечо выше 1 несовместимо с фиксированным `max_net_exposure = 1`.
        // Ограничиваем target здесь, чтобы planner не останавливал runtime
        // ошибкой на обычном сигнале низкой волатильности.
        let executable_leverage = requested_leverage.min(Decimal::ONE);
        let raw = self
            .universe
            .iter()
            .cloned()
            .map(|instrument_id| (instrument_id, Decimal::ONE))
            .collect::<Vec<_>>();
        Ok(Some(normalize_gross(
            &self.universe,
            &raw,
            executable_leverage,
        )))
    }

    /// Формирует полный `TargetPortfolio` и его временные метки аудита.
    ///
    /// `source_as_of` — время самой свежей использованной свечи, `generated_at` —
    /// время события, `valid_until` — крайний срок исполнения. Batch ID стабилен для
    /// пары `(strategy_id, event_id)`, поэтому повтор события не создаёт второй batch.
    fn make_target(
        &self,
        event: &StrategyEvent,
        weights: Vec<(InstrumentId, Decimal)>,
        validity: Duration,
    ) -> TargetPortfolio {
        let source_as_of = self.state.newest_source_time().unwrap_or(event.occurred_at);
        TargetPortfolio {
            batch_id: deterministic_batch_id(&self.strategy_id, &event.id),
            event_id: event.id.clone(),
            strategy_id: self.strategy_id.clone(),
            account_id: self.account_id.clone(),
            source_as_of,
            generated_at: event.occurred_at,
            valid_until: event.occurred_at + validity,
            weights: weights
                .into_iter()
                .map(|(instrument_id, weight)| TargetWeight {
                    instrument_id,
                    weight,
                })
                .collect(),
        }
    }
}

/// Детерминированно преобразует `(strategy_id, event_id)` в UUID-подобный batch ID.
///
/// Два независимых 64-битных rolling hash дают стабильные 128 бит; затем выставляются
/// version/variant bits UUID. Это идентификатор идемпотентности, а не криптографический
/// hash и не средство защиты от злоумышленника.
fn deterministic_batch_id(strategy_id: &StrategyId, event_id: &str) -> Uuid {
    let mut left = 0xcbf2_9ce4_8422_2325_u64;
    let mut right = 0x8422_2325_cbf2_9ce4_u64;
    for byte in strategy_id
        .as_str()
        .bytes()
        .chain([0xff])
        .chain(event_id.bytes())
    {
        left ^= u64::from(byte);
        left = left.wrapping_mul(0x0000_0100_0000_01b3);
        right ^= u64::from(byte.rotate_left(1));
        right = right.wrapping_mul(0x9e37_79b1_85eb_ca87);
    }
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&left.to_be_bytes());
    bytes[8..].copy_from_slice(&right.to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

#[async_trait]
impl PortfolioStrategy for BuiltinStrategy {
    /// Возвращает устойчивый идентификатор экземпляра стратегии.
    fn id(&self) -> &StrategyId {
        &self.strategy_id
    }

    /// Возвращает единственный Sandbox-счёт, принадлежащий этому процессу стратегии.
    fn account_id(&self) -> &AccountId {
        &self.account_id
    }

    /// Возвращает kebab-case имя встроенного алгоритма.
    fn kind(&self) -> &'static str {
        self.spec.kind()
    }

    /// Сериализует эффективную конфигурацию для аудита и storage.
    fn config(&self) -> Result<Value> {
        serde_json::to_value(&self.spec).map_err(|error| Error::Strategy {
            strategy: self.strategy_id.to_string(),
            message: error.to_string(),
        })
    }

    /// Передаёт engine полный список рядов, требуемых для warm-up и live-обработки.
    fn data_requirements(&self) -> Vec<DataRequirement> {
        self.requirements()
    }

    /// Передаёт runtime расписание, привязанное к официальной MOEX-сессии.
    fn schedule_requirements(&self) -> Vec<ScheduleRequirement> {
        self.schedules()
    }

    /// Сериализует накопленные свечи и маркеры ребалансировки.
    fn state(&self) -> Result<Value> {
        serde_json::to_value(&self.state).map_err(|error| Error::Strategy {
            strategy: self.strategy_id.to_string(),
            message: error.to_string(),
        })
    }

    /// Восстанавливает durable state после рестарта процесса.
    fn restore_state(&mut self, state: Value) -> Result<()> {
        self.state = serde_json::from_value(state).map_err(|error| Error::Strategy {
            strategy: self.strategy_id.to_string(),
            message: error.to_string(),
        })?;
        Ok(())
    }

    /// Обрабатывает одно упорядоченное событие и при необходимости возвращает target.
    ///
    /// Candle сначала сохраняется, но во время warm-up торговый результат запрещён.
    /// Scheduled стратегии формируют target только на нужном anchor и не чаще своей
    /// периодичности. Order/RiskCheck не меняют формулу сигнала.
    async fn on_event(&mut self, event: &StrategyEvent) -> Result<Option<TargetPortfolio>> {
        match &event.payload {
            StrategyEventKind::Candle(candle) => {
                if !self.accepts(candle) {
                    return Ok(None);
                }
                let maximum_bars = self.maximum_bars_for(&candle.instrument_id, candle.interval);
                self.state.insert_candle(candle.clone(), maximum_bars);
                // Warm-up обновляет тот же state, что и live, но не имеет права
                // породить заявку: исполнение возможно лишь на будущем live-событии.
                if event.phase == EventPhase::Warmup {
                    return Ok(None);
                }
                if matches!(&self.spec, BuiltinStrategySpec::IntradaySameClock(_)) {
                    return self.intraday_same_clock_target(event, candle);
                }
                Ok(None)
            }
            StrategyEventKind::Schedule(schedule) => {
                if !self
                    .schedules()
                    .iter()
                    .any(|requirement| requirement.anchor == schedule.anchor)
                {
                    return Ok(None);
                }
                if schedule.anchor == ScheduleAnchor::SessionOpen
                    && matches!(
                        &self.spec,
                        BuiltinStrategySpec::MarketIntradayMomentum(_)
                            | BuiltinStrategySpec::IntradaySameClock(_)
                    )
                {
                    // Дата и время берутся только из официального расписания.
                    // Запоздалое событие старой сессии не может вернуть state назад.
                    if self
                        .state
                        .last_intraday_session_close
                        .is_none_or(|closed| schedule.session_date > closed)
                        && self
                            .state
                            .active_intraday_session
                            .is_none_or(|active| schedule.session_date >= active)
                    {
                        self.state.active_intraday_session = Some(schedule.session_date);
                        self.state.intraday_session_open_at = Some(schedule.scheduled_at);
                    }
                    return Ok(None);
                }
                if schedule.anchor == ScheduleAnchor::SessionClose
                    && matches!(
                        &self.spec,
                        BuiltinStrategySpec::MarketIntradayMomentum(_)
                            | BuiltinStrategySpec::IntradaySameClock(_)
                    )
                {
                    // Сама компонента на close новый сигнал не выпускает.
                    // Ensemble удаляет cached intraday target и строит
                    // закрывающий общий target на том же событии.
                    if self
                        .state
                        .active_intraday_session
                        .is_none_or(|active| active <= schedule.session_date)
                    {
                        self.state.active_intraday_session = None;
                        self.state.intraday_session_open_at = None;
                    }
                    self.state.last_intraday_session_close = Some(
                        self.state
                            .last_intraday_session_close
                            .map_or(schedule.session_date, |closed| {
                                closed.max(schedule.session_date)
                            }),
                    );
                    return Ok(None);
                }
                if event.phase == EventPhase::Warmup {
                    // При single-flight runtime передаёт пропущенный session
                    // boundary как Warmup. Open/close gate выше сохранить нужно,
                    // но обычный scheduled сигнал в этой фазе запрещён.
                    return Ok(None);
                }
                if self.should_rebalance_monthly()
                    && !self.state.is_new_month(schedule.session_date)
                {
                    return Ok(None);
                }
                if matches!(&self.spec, BuiltinStrategySpec::ShortTermReversal(_))
                    && !self.state.is_new_week(schedule.session_date)
                {
                    return Ok(None);
                }

                let result = self.scheduled_target(event, schedule.session_date)?;
                // Период помечается только после реального target. Недостаток данных
                // оставляет возможность повторить расчёт на следующей сессии.
                if result.is_some() {
                    if self.should_rebalance_monthly() {
                        self.state.mark_month(schedule.session_date);
                    }
                    if matches!(&self.spec, BuiltinStrategySpec::ShortTermReversal(_)) {
                        self.state.mark_week(schedule.session_date);
                    }
                }
                Ok(result)
            }
            StrategyEventKind::Order(_) | StrategyEventKind::RiskCheck => Ok(None),
        }
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
            {
                return Err(Error::Strategy {
                    strategy: self.strategy_id.to_string(),
                    message: "stored ensemble component order, kind, signal_weight, or parameters changed; use a new strategy_id".to_owned(),
                });
            }
            staged
                .strategy
                .restore_state(restored_component.strategy_state)?;
            if let Some(target) = &restored_component.last_target {
                Self::validate_cached_target(
                    &self.universe,
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
        let universe = self.universe.clone();

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
                    &universe,
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

/// Проверяет непустую вселенную и уникальность каждого UID.
fn validate_universe(universe: &[InstrumentId]) -> Result<()> {
    if universe.is_empty() {
        return configuration_error("tradable universe cannot be empty");
    }
    let mut unique = HashSet::with_capacity(universe.len());
    if universe
        .iter()
        .any(|instrument_id| !unique.insert(instrument_id.as_str()))
    {
        return configuration_error("tradable universe contains duplicate instrument UIDs");
    }
    Ok(())
}

/// Требует минимум десять бумаг, чтобы 20%-е хвосты содержали осмысленную выборку.
fn validate_ranking_universe(universe: &[InstrumentId]) -> Result<()> {
    if universe.len() < 10 {
        return configuration_error("ranking strategy requires at least ten instruments");
    }
    Ok(())
}

/// Запрещает использовать benchmark одновременно как торгуемую позицию.
fn validate_reference(reference: &InstrumentId, universe: &[InstrumentId]) -> Result<()> {
    if universe.contains(reference) {
        return configuration_error("benchmark_uid must not also be a tradable universe member");
    }
    Ok(())
}

/// Проверяет корректность momentum-окна: дальняя граница строго больше skip.
fn validate_formation_skip(formation_days: usize, skip_days: usize) -> Result<()> {
    if skip_days == 0 || formation_days <= skip_days {
        return configuration_error("formation_days must be greater than skip_days > 0");
    }
    Ok(())
}

/// Проверяет размер крайнего хвоста: от 1% включительно до 50% исключительно.
fn validate_quantile(quantile_percent: usize) -> Result<()> {
    if quantile_percent == 0 || quantile_percent >= 50 {
        return configuration_error("quantile_percent must be in 1..50");
    }
    Ok(())
}

/// Проверяет процентное поле в диапазоне `0..=100` либо `0 < value <= 100`.
fn validate_percent(value: Decimal, name: &str, zero_allowed: bool) -> Result<()> {
    if value > Decimal::from(100)
        || (zero_allowed && value < Decimal::ZERO)
        || (!zero_allowed && value <= Decimal::ZERO)
    {
        return configuration_error(format!("{name} must be in the supported percentage range"));
    }
    Ok(())
}

/// Проверяет целочисленное окно/лимит на строго положительное значение.
fn validate_positive(value: usize, name: &str) -> Result<()> {
    if value == 0 {
        return configuration_error(format!("{name} must be positive"));
    }
    Ok(())
}

/// Проверяет итоговую глубину history-запроса и одновременно исключает overflow.
fn validate_warmup_window(value: usize, extra_bars: usize, name: &str) -> Result<()> {
    let required = value.checked_add(extra_bars).ok_or_else(|| {
        Error::Configuration(format!("{name} overflows the required warmup size"))
    })?;
    if required == 0 || required > MAX_WARMUP_BARS {
        return configuration_error(format!(
            "{name} requires {required} warmup bars; supported range is 1..={MAX_WARMUP_BARS}"
        ));
    }
    Ok(())
}

/// Создаёт единообразную ошибку конфигурации с произвольным ожидаемым типом.
fn configuration_error<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::Configuration(message.into()))
}
