//! Сигналы встроенных стратегий: целевые веса по закрытым свечам, отчётности,
//! дивидендам и решениям ИИ-модели.

use std::cmp::Ordering;

use chrono::{DateTime, Duration, Timelike, Utc};
use rust_decimal::Decimal;

use super::{
    AiModelConfig, BettingAgainstBetaConfig, BuiltinStrategy, BuiltinStrategySpec,
    CrossSectionalMomentumConfig, DAILY_VALIDITY, DividendMonthConfig, EqualRiskContributionConfig,
    FiftyTwoWeekHighConfig, INTRADAY_VALIDITY, IntermediateMomentumConfig, LowVolatilityConfig,
    MovingAverageTrendConfig, PairsTradingConfig, QualityProfitabilityConfig, RangeBreakoutConfig,
    ResidualMomentumConfig, ReturnSeasonalityConfig, RiskManagedMomentumConfig,
    ShortTermReversalConfig, TimeSeriesMomentumConfig, ValueCompositeConfig,
    VolatilityManagedConfig, composite_scores,
    math::{
        MAX_INSTRUMENT_WEIGHT, covariance_matrix, equal_risk_weights, fill_universe, mean,
        normalize_gross, normalize_long_short, ordinary_least_squares, rank_long_only_low,
        rank_long_short, return_between, shrink_covariance, simple_returns, square_root, variance,
    },
    state::{PairPosition, PairState},
};
use crate::{
    Error, Result,
    domain::{Candle, CandleInterval, InstrumentId},
    engine::{StrategyEvent, TargetPortfolio},
};

impl BuiltinStrategy {
    /// Строит сигнал SMA: `+1` выше верхней полосы, `-1` ниже нижней, иначе `0`.
    ///
    /// После расчёта сырые сигналы отдельно нормализуются до `+0,5/-0,5` gross.
    pub(super) fn moving_average_weights(
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
    pub(super) fn range_breakout_weights(
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
    pub(super) fn time_series_momentum_weights(
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
    pub(super) fn cross_sectional_momentum_weights(
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
    pub(super) fn intermediate_momentum_weights(
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
    pub(super) fn fifty_two_week_high_weights(
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
    pub(super) fn residual_momentum_weights(
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
    pub(super) fn pairs_weights(
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
    pub(super) fn select_pair(&self, config: &PairsTradingConfig) -> Result<Option<PairState>> {
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
    pub(super) fn short_term_reversal_weights(
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
    pub(super) fn market_intraday_weights(
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
    pub(super) fn intraday_same_clock_target(
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
    pub(super) fn low_volatility_weights(
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

    /// Value: средний перцентиль B/P, E/P и EBITDA/EV; long дешёвых, short дорогих.
    ///
    /// Мультипликатор ≤ 0 или отсутствующий не участвует в своём ранге; бумага без
    /// единого показателя не получает позиции. Сигнала нет, пока снимок не покрывает
    /// хотя бы 80% universe.
    pub(super) fn value_composite_weights(
        &self,
        config: &ValueCompositeConfig,
    ) -> Option<Vec<(InstrumentId, Decimal)>> {
        self.state.fundamentals_observed_at?;
        let inverse = |value: Option<Decimal>| {
            value
                .filter(|v| *v > Decimal::ZERO)
                .map(|v| Decimal::ONE / v)
        };
        let rows = self
            .universe
            .iter()
            .map(|instrument_id| {
                let row = self.state.fundamentals_for(instrument_id);
                (
                    instrument_id.clone(),
                    vec![
                        inverse(row.and_then(|f| f.price_to_book)),
                        inverse(row.and_then(|f| f.price_to_earnings)),
                        inverse(row.and_then(|f| f.ev_to_ebitda)),
                    ],
                )
            })
            .collect::<Vec<_>>();
        let scores = composite_scores(&rows, self.universe.len())?;
        rank_long_short(&self.universe, scores, config.quantile_percent, true)
    }

    /// Quality: средний перцентиль ROE, ROA, чистой маржи и низкого долга к капиталу.
    pub(super) fn quality_profitability_weights(
        &self,
        config: &QualityProfitabilityConfig,
    ) -> Option<Vec<(InstrumentId, Decimal)>> {
        self.state.fundamentals_observed_at?;
        let rows = self
            .universe
            .iter()
            .map(|instrument_id| {
                let row = self.state.fundamentals_for(instrument_id);
                (
                    instrument_id.clone(),
                    vec![
                        row.and_then(|f| f.return_on_equity),
                        row.and_then(|f| f.return_on_assets),
                        row.and_then(|f| f.net_margin),
                        // Меньший долг лучше: знак инвертируется для общего направления ранга.
                        row.and_then(|f| f.debt_to_equity)
                            .filter(|v| *v >= Decimal::ZERO)
                            .map(|v| -v),
                    ],
                )
            })
            .collect::<Vec<_>>();
        let scores = composite_scores(&rows, self.universe.len())?;
        rank_long_short(&self.universe, scores, config.quantile_percent, true)
    }

    /// Momentum 12–1, масштабированный до целевой волатильности (Barroso & Santa-Clara).
    ///
    /// Волатильность оценивается ex ante: текущие веса применяются к дневным
    /// доходностям последних `volatility_days` свечей. Множитель
    /// `target / realized` ограничен `max_leverage`; нулевая оценка закрывает портфель.
    pub(super) fn risk_managed_momentum_weights(
        &self,
        config: &RiskManagedMomentumConfig,
    ) -> Result<Option<Vec<(InstrumentId, Decimal)>>> {
        let mut scores = Vec::new();
        for instrument_id in &self.universe {
            let candles = self.state.candles(instrument_id, CandleInterval::OneDay);
            let Some(score) = return_between(candles, config.formation_days, config.skip_days)
            else {
                return Ok(None);
            };
            scores.push((instrument_id.clone(), score));
        }
        let Some(base) = rank_long_short(&self.universe, scores, config.quantile_percent, true)
        else {
            return Ok(None);
        };
        let mut portfolio_returns = vec![Decimal::ZERO; config.volatility_days];
        for (instrument_id, weight) in base.iter().filter(|(_, weight)| !weight.is_zero()) {
            let candles = self.state.candles(instrument_id, CandleInterval::OneDay);
            if candles.len() < config.volatility_days + 1 {
                return Ok(None);
            }
            let returns = simple_returns(&candles[candles.len() - config.volatility_days - 1..])?;
            for (total, value) in portfolio_returns.iter_mut().zip(returns) {
                *total += *weight * value;
            }
        }
        let annualized_variance = variance(&portfolio_returns)? * Decimal::from(252);
        let target = config.target_annual_volatility_percent / Decimal::from(100);
        let scale = if annualized_variance <= Decimal::ZERO {
            Decimal::ZERO
        } else {
            (target / square_root(annualized_variance)?).clamp(Decimal::ZERO, config.max_leverage)
        };
        Ok(Some(
            base.into_iter()
                .map(|(instrument_id, weight)| (instrument_id, weight * scale))
                .collect(),
        ))
    }

    /// Сезонность: средняя доходность того же календарного месяца за `years` прошлых лет.
    ///
    /// Месяц сигнала — месяц сессии, в которую строится target. Доходность месяца
    /// считается по закрытиям соседних месячных свечей; без полной истории по
    /// каждой бумаге сигнал откладывается.
    pub(super) fn return_seasonality_weights(
        &self,
        config: &ReturnSeasonalityConfig,
        session_date: chrono::NaiveDate,
    ) -> Result<Option<Vec<(InstrumentId, Decimal)>>> {
        use chrono::Datelike;
        let mut scores = Vec::new();
        for instrument_id in &self.universe {
            let candles = self.state.candles(instrument_id, CandleInterval::OneMonth);
            let mut returns = Vec::with_capacity(config.years);
            for lag in 1..=config.years {
                let year = session_date.year() - i32::try_from(lag).unwrap_or(i32::MAX);
                let Some(index) = candles.iter().position(|candle| {
                    candle.time.year() == year && candle.time.month() == session_date.month()
                }) else {
                    return Ok(None);
                };
                if index == 0 {
                    return Ok(None);
                }
                let previous = candles[index - 1].close.0;
                if previous <= Decimal::ZERO {
                    return Ok(None);
                }
                returns.push(candles[index].close.0 / previous - Decimal::ONE);
            }
            scores.push((instrument_id.clone(), mean(&returns)?));
        }
        Ok(rank_long_short(
            &self.universe,
            scores,
            config.quantile_percent,
            true,
        ))
    }

    /// Веса из решения ИИ-модели для этой сессии с лимитами стратегии.
    ///
    /// Без решения на эту дату сигнала нет и позиции не меняются. Инструменты вне
    /// набора отбрасываются, доля каждого ограничена `max_weight_percent`, short
    /// обнуляется без `allow_short`, а суммарная экспозиция не превышает 100%.
    pub(super) fn ai_model_weights(
        &self,
        config: &AiModelConfig,
        session_date: chrono::NaiveDate,
        not_before: DateTime<Utc>,
    ) -> Option<Vec<(InstrumentId, Decimal)>> {
        // Решение должно быть получено для этого пересмотра, а не прошлого часа.
        let advice = self.state.ai_advice.as_ref().filter(|advice| {
            advice.session_date == session_date
                && advice.observed_at >= not_before - Duration::minutes(2)
        })?;
        let cap = config.max_weight_percent / Decimal::ONE_HUNDRED;
        let floor = if config.allow_short {
            -cap
        } else {
            Decimal::ZERO
        };
        let mut legs: Vec<(InstrumentId, Decimal)> = Vec::new();
        for advice in &advice.weights {
            if !self.universe.contains(&advice.instrument_id) {
                continue;
            }
            let weight = advice.weight.clamp(floor, cap);
            legs.retain(|(id, _)| id != &advice.instrument_id);
            if !weight.is_zero() {
                legs.push((advice.instrument_id.clone(), weight));
            }
        }
        let gross: Decimal = legs.iter().map(|(_, weight)| weight.abs()).sum();
        if gross > Decimal::ONE {
            for leg in &mut legs {
                leg.1 = (leg.1 / gross).round_dp(6);
            }
        }
        Some(fill_universe(&self.universe, &legs))
    }

    /// Внеочередное решение модели (live): новая цель сразу, без ожидания расписания.
    pub(super) fn ai_model_target(
        &self,
        event: &StrategyEvent,
        advice: &crate::engine::ModelAdvice,
    ) -> Option<TargetPortfolio> {
        let BuiltinStrategySpec::AiModel(config) = &self.spec else {
            return None;
        };
        let weights = self.ai_model_weights(config, advice.session_date, advice.observed_at)?;
        Some(self.make_target(event, weights, DAILY_VALIDITY))
    }

    /// Дивидендная премия: long бумаг за `entry_days` до последнего дня покупки.
    ///
    /// Позиция закрывается на открытии последнего дня покупки, до дивидендного
    /// гэпа: Sandbox может не начислять сам дивиденд. Без календаря сигнала нет;
    /// пустой календарь означает полностью закрытый портфель.
    pub(super) fn dividend_month_weights(
        &self,
        config: &DividendMonthConfig,
        session_date: chrono::NaiveDate,
    ) -> Option<Vec<(InstrumentId, Decimal)>> {
        self.state.dividends_observed_at?;
        let entry = chrono::Duration::days(i64::from(config.entry_days));
        let mut selected = self
            .universe
            .iter()
            .filter(|instrument_id| {
                self.state.dividends.iter().any(|dividend| {
                    &dividend.instrument_id == *instrument_id
                        && session_date < dividend.last_buy_date
                        && session_date >= dividend.last_buy_date - entry
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        selected.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        if selected.is_empty() {
            return Some(fill_universe(&self.universe, &[]));
        }
        let weight =
            (Decimal::ONE / Decimal::from(selected.len() as u64)).min(MAX_INSTRUMENT_WEIGHT);
        let legs = selected
            .into_iter()
            .map(|instrument_id| (instrument_id, weight))
            .collect::<Vec<_>>();
        Some(fill_universe(&self.universe, &legs))
    }

    /// Строит BAB-портфель: long low-beta и short high-beta с gross до 2.
    ///
    /// Сначала OLS-beta притягивается к единице:
    /// `shrunk_beta = shrinkage*beta + (1-shrinkage)*1`. Внутри каждой половины
    /// бумаги получают inverse-|beta| веса (с floor 0,0001). Gross ног выбирается
    /// так, чтобы их beta-экспозиции совпали: при `L + S = 2` используется
    /// `L = 2*beta_high/(beta_low+beta_high)`, `S = 2-L`. Затем L ограничивается
    /// диапазоном 0,5..1,5, а каждый инструмент — весом 0,5.
    pub(super) fn betting_against_beta_weights(
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
    pub(super) fn equal_risk_contribution_weights(
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
    pub(super) fn volatility_managed_weights(
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
}
