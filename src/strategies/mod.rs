//! Детерминированные portfolio-target реализации пятнадцати встроенных стратегий
//! и их взвешенного ансамбля.
//!
//! Стратегии читают только закрытые свечи, хранят сериализуемое состояние без
//! broker-specific объектов и никогда не выставляют заявки сами. Они возвращают
//! **полный** целевой портфель: положительный signed weight означает long,
//! отрицательный — short, нулевой — закрытие позиции. События warm-up только
//! наполняют историю и принципиально не могут породить торговый target.

mod config;
mod ensemble;
mod math;
mod state;
mod weights;

use std::collections::HashSet;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use serde_json::Value;
use uuid::Uuid;

pub use config::{
    AiModelConfig, BettingAgainstBetaConfig, BuiltinStrategySpec, CrossSectionalMomentumConfig,
    DividendMonthConfig, EqualRiskContributionConfig, FiftyTwoWeekHighConfig,
    IntermediateMomentumConfig, IntradaySameClockConfig, LowVolatilityConfig,
    MarketIntradayMomentumConfig, MovingAverageTrendConfig, PairsTradingConfig,
    QualityProfitabilityConfig, RangeBreakoutConfig, ResidualMomentumConfig,
    ReturnSeasonalityConfig, RiskManagedMomentumConfig, ShortTermReversalConfig,
    TimeSeriesMomentumConfig, ValueCompositeConfig, VolatilityManagedConfig,
};
pub use ensemble::{WeightedEnsembleStrategy, WeightedStrategySpec};

use crate::{
    Error, Result,
    domain::{AccountId, Candle, CandleInterval, InstrumentId, StrategyId},
    engine::{
        DataRequirement, DataRole, EventPhase, PortfolioStrategy, ReferenceRequirement,
        ScheduleAnchor, ScheduleRequirement, StrategyEvent, StrategyEventKind, TargetPortfolio,
        TargetWeight,
    },
};
use state::BuiltinState;

/// Срок жизни дневного target: после него planner обязан запросить новый сигнал.
const DAILY_VALIDITY: Duration = Duration::hours(8);
/// Месячный target допускает исполнение в течение недели с учётом выходных.
const MONTHLY_VALIDITY: Duration = Duration::days(7);
/// Внутридневной прогноз действует только один 30-минутный слот.
const INTRADAY_VALIDITY: Duration = Duration::minutes(30);
/// Открытие сессии включает intraday-сигналы, только если пришло вовремя.
const INTRADAY_OPEN_GRACE_SECONDS: i64 = 120;
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
            Self::ValueComposite(ValueCompositeConfig { quantile_percent })
            | Self::QualityProfitability(QualityProfitabilityConfig { quantile_percent }) => {
                validate_ranking_universe(universe)?;
                validate_quantile(*quantile_percent)?;
            }
            Self::RiskManagedMomentum(config) => {
                validate_ranking_universe(universe)?;
                validate_formation_skip(config.formation_days, config.skip_days)?;
                validate_warmup_window(
                    config.formation_days.max(config.volatility_days),
                    1,
                    "formation_days",
                )?;
                validate_quantile(config.quantile_percent)?;
                if config.volatility_days < 2 {
                    return configuration_error("volatility_days must be at least 2");
                }
                validate_percent(
                    config.target_annual_volatility_percent,
                    "target_annual_volatility_percent",
                    false,
                )?;
                if config.max_leverage <= Decimal::ZERO || config.max_leverage > Decimal::from(2) {
                    return configuration_error("max_leverage must be in (0, 2]");
                }
            }
            Self::ReturnSeasonality(config) => {
                validate_ranking_universe(universe)?;
                validate_quantile(config.quantile_percent)?;
                if !(1..=20).contains(&config.years) {
                    return configuration_error("years must be in 1..=20");
                }
            }
            Self::DividendMonth(config) => {
                if !(1..=120).contains(&config.entry_days) {
                    return configuration_error("entry_days must be in 1..=120");
                }
            }
            Self::AiModel(config) => validate_ai_model(config, universe)?,
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
            // Сигнал берётся из справочных данных; дневные свечи нужны для синхронности
            // рядов и проверки, что инструмент торгуется.
            BuiltinStrategySpec::ValueComposite(_)
            | BuiltinStrategySpec::QualityProfitability(_)
            | BuiltinStrategySpec::DividendMonth(_)
            | BuiltinStrategySpec::AiModel(_) => (CandleInterval::OneDay, 2),
            BuiltinStrategySpec::RiskManagedMomentum(config) => (
                CandleInterval::OneDay,
                config.formation_days.max(config.volatility_days) + 1,
            ),
            BuiltinStrategySpec::ReturnSeasonality(config) => {
                (CandleInterval::OneMonth, config.years * 12 + 1)
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
            // Модель пересматривает портфель на открытии и через каждый интервал.
            BuiltinStrategySpec::AiModel(config) => config
                .decision_offsets_seconds()
                .into_iter()
                .map(|offset_seconds| ScheduleRequirement {
                    anchor: ScheduleAnchor::SessionOpen,
                    offset_seconds,
                })
                .collect(),
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
                | BuiltinStrategySpec::ValueComposite(_)
                | BuiltinStrategySpec::QualityProfitability(_)
                | BuiltinStrategySpec::RiskManagedMomentum(_)
                | BuiltinStrategySpec::ReturnSeasonality(_)
        )
    }

    /// Справочные данные брокера, без которых стратегия не может построить сигнал.
    fn references(&self) -> Vec<ReferenceRequirement> {
        match &self.spec {
            BuiltinStrategySpec::ValueComposite(_)
            | BuiltinStrategySpec::QualityProfitability(_) => {
                vec![ReferenceRequirement::Fundamentals]
            }
            BuiltinStrategySpec::DividendMonth(_) => vec![ReferenceRequirement::Dividends],
            _ => Vec::new(),
        }
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
            BuiltinStrategySpec::ValueComposite(config) => Ok(self.value_composite_weights(config)),
            BuiltinStrategySpec::QualityProfitability(config) => {
                Ok(self.quality_profitability_weights(config))
            }
            BuiltinStrategySpec::RiskManagedMomentum(config) => {
                self.risk_managed_momentum_weights(config)
            }
            BuiltinStrategySpec::ReturnSeasonality(config) => {
                self.return_seasonality_weights(config, session_date)
            }
            BuiltinStrategySpec::DividendMonth(config) => {
                Ok(self.dividend_month_weights(config, session_date))
            }
            BuiltinStrategySpec::AiModel(config) => {
                Ok(self.ai_model_weights(config, session_date, scheduled_at(event)))
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

    /// Передаёт runtime виды справочных данных, нужных сигналу.
    fn reference_requirements(&self) -> Vec<ReferenceRequirement> {
        self.references()
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
                    // Запоздалое событие старой сессии не может вернуть state назад,
                    // а повтор открытия посреди сессии не включает intraday-сигналы.
                    let on_time = event.occurred_at - schedule.scheduled_at
                        <= Duration::seconds(INTRADAY_OPEN_GRACE_SECONDS);
                    if on_time
                        && self
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
            StrategyEventKind::Advice(advice) => {
                // Решение относится к компоненте с тем же набором инструментов;
                // само событие заявку не создаёт.
                let same_universe = advice.universe.len() == self.universe.len()
                    && advice
                        .universe
                        .iter()
                        .all(|instrument_id| self.universe.contains(instrument_id));
                // Если у компоненты свой профиль, решение принимается только от него:
                // так несколько моделей на одном наборе не подменяют решения друг друга.
                let own_model = match &self.spec {
                    BuiltinStrategySpec::AiModel(config) => {
                        config.model_profile.as_deref().is_none_or(|profile| {
                            advice.model.split_once(':').map(|(name, _)| name) == Some(profile)
                        })
                    }
                    _ => false,
                };
                if own_model && same_universe {
                    self.state.ai_advice = Some(advice.clone());
                    // Live-решение (всплеск новостей, скачок цены) исполняется сразу.
                    if event.phase == EventPhase::Live {
                        return Ok(self.ai_model_target(event, advice));
                    }
                }
                Ok(None)
            }
            StrategyEventKind::Reference(reference) => {
                // Сохраняются только запрошенные этой стратегией данные; пустой ответ
                // по виду, который она не запрашивала, не стирает прежний снимок.
                let wanted = self.references();
                if wanted.contains(&ReferenceRequirement::Fundamentals)
                    && !reference.fundamentals.is_empty()
                {
                    self.state.fundamentals = reference
                        .fundamentals
                        .iter()
                        .filter(|row| self.universe.contains(&row.instrument_id))
                        .cloned()
                        .collect();
                    self.state.fundamentals_observed_at = Some(reference.observed_at);
                }
                if wanted.contains(&ReferenceRequirement::Dividends) {
                    self.state.dividends = reference
                        .dividends
                        .iter()
                        .filter(|row| self.universe.contains(&row.instrument_id))
                        .cloned()
                        .collect();
                    self.state.dividends_observed_at = Some(reference.observed_at);
                }
                Ok(None)
            }
            StrategyEventKind::Order(_) | StrategyEventKind::RiskCheck => Ok(None),
        }
    }
}

/// Средний перцентильный ранг нескольких показателей, где большее значение лучше.
///
/// Каждый показатель ранжируется только среди бумаг, у которых он есть; ничьи
/// получают средний ранг. Итог — среднее доступных перцентилей бумаги. `None`,
/// если показатели есть меньше чем у 80% universe или меньше чем у четырёх бумаг.
fn composite_scores(
    rows: &[(InstrumentId, Vec<Option<Decimal>>)],
    universe_len: usize,
) -> Option<Vec<(InstrumentId, Decimal)>> {
    let metrics = rows.first().map_or(0, |(_, values)| values.len());
    let mut sums = vec![(Decimal::ZERO, 0_u32); rows.len()];
    for metric in 0..metrics {
        let mut present = rows
            .iter()
            .enumerate()
            .filter_map(|(row, (_, values))| values[metric].map(|value| (row, value)))
            .collect::<Vec<_>>();
        if present.len() < 2 {
            continue;
        }
        present.sort_by_key(|item| item.1);
        let last = Decimal::from(present.len() as u64 - 1);
        let mut start = 0;
        while start < present.len() {
            let mut end = start;
            while end + 1 < present.len() && present[end + 1].1 == present[start].1 {
                end += 1;
            }
            // Средняя позиция группы равных значений, нормированная в [0, 1].
            let rank = Decimal::from((start + end) as u64) / Decimal::from(2) / last;
            for (row, _) in &present[start..=end] {
                sums[*row].0 += rank;
                sums[*row].1 += 1;
            }
            start = end + 1;
        }
    }
    let scores = rows
        .iter()
        .zip(sums)
        .filter(|(_, (_, count))| *count > 0)
        .map(|((instrument_id, _), (sum, count))| {
            (instrument_id.clone(), sum / Decimal::from(count))
        })
        .collect::<Vec<_>>();
    (scores.len() >= 4 && scores.len() * 5 >= universe_len * 4).then_some(scores)
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

/// Проверяет параметры `ai-model`; подключение к модели проверяет runtime.
fn validate_ai_model(config: &AiModelConfig, universe: &[InstrumentId]) -> Result<()> {
    if universe.is_empty() || universe.len() > 50 {
        return configuration_error("ai-model needs 1..=50 instruments");
    }
    if config.model_profile.as_ref().is_some_and(|name| {
        name.is_empty()
            || name.len() > 40
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }) {
        return configuration_error("model_profile must be 1..=40 latin letters, digits, - or _");
    }
    if config.instructions.chars().count() > 4000 {
        return configuration_error("instructions must be at most 4000 characters");
    }
    if config.news_feeds.len() > 20
        || config.news_feeds.iter().any(|url| {
            url.len() > 500 || !(url.starts_with("https://") || url.starts_with("http://"))
        })
    {
        return configuration_error("news_feeds: up to 20 http(s) URLs");
    }
    if !(1..=168).contains(&config.news_hours) {
        return configuration_error("news_hours must be in 1..=168");
    }
    if !(1..=20).contains(&config.max_tool_rounds) {
        return configuration_error("max_tool_rounds must be in 1..=20");
    }
    if config.decision_interval_minutes != 0
        && !(15..=240).contains(&config.decision_interval_minutes)
    {
        return configuration_error("decision_interval_minutes must be 0 or 15..=240");
    }
    if config.news_burst_items > 50 || !(5..=240).contains(&config.news_burst_minutes) {
        return configuration_error(
            "news_burst_items must be 0..=50 and news_burst_minutes 5..=240",
        );
    }
    if !(5..=240).contains(&config.min_minutes_between_decisions)
        || !(1..=100).contains(&config.max_decisions_per_day)
    {
        return configuration_error(
            "min_minutes_between_decisions must be 5..=240 and max_decisions_per_day 1..=100",
        );
    }
    if config.material_importance > 5 {
        return configuration_error("material_importance must be 0..=5");
    }
    for (name, value) in [
        ("price_jump_percent", config.price_jump_percent),
        ("fast_jump_percent", config.fast_jump_percent),
        ("market_move_percent", config.market_move_percent),
        ("commodity_move_percent", config.commodity_move_percent),
        ("currency_move_percent", config.currency_move_percent),
    ] {
        if value < Decimal::ZERO || value > Decimal::from(50) {
            return configuration_error(format!("{name} must be in 0..=50"));
        }
    }
    validate_percent(config.max_weight_percent, "max_weight_percent", false)
}

/// Время пересмотра по расписанию; для прочих событий — момент события.
fn scheduled_at(event: &StrategyEvent) -> DateTime<Utc> {
    match &event.payload {
        StrategyEventKind::Schedule(schedule) => schedule.scheduled_at,
        _ => event.occurred_at,
    }
}

/// Создаёт единообразную ошибку конфигурации с произвольным ожидаемым типом.
fn configuration_error<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::Configuration(message.into()))
}

#[cfg(test)]
mod tests;
