//! Детерминированная математика сигналов и портфельных весов.
//!
//! Все вычисления выполняются в [`Decimal`]: это медленнее `f64`, зато одинаковые
//! входные свечи дают одинаковый target на разных машинах. Знак веса задаёт сторону
//! позиции (`+` — long, `-` — short), абсолютное значение — долю strategy capital.

use std::cmp::Ordering;

use rust_decimal::Decimal;

use crate::{
    Error, Result,
    domain::{Candle, InstrumentId},
};

/// Жёсткий лимит одного инструмента: 0,5 от доступного strategy capital.
pub(crate) const MAX_INSTRUMENT_WEIGHT: Decimal = Decimal::from_parts(5, 0, 0, false, 1);

/// Возвращает арифметическое среднее `Σx / n`.
///
/// Пустая выборка считается ошибкой стратегии, потому что нулевое среднее могло бы
/// незаметно превратить отсутствие данных в торговый сигнал.
pub(crate) fn mean(values: &[Decimal]) -> Result<Decimal> {
    if values.is_empty() {
        return Err(Error::Strategy {
            strategy: "built-in".to_owned(),
            message: "cannot calculate a mean of an empty sample".to_owned(),
        });
    }
    let count = Decimal::from(
        u64::try_from(values.len())
            .map_err(|_| Error::Configuration("sample is too large".to_owned()))?,
    );
    Ok(values.iter().copied().sum::<Decimal>() / count)
}

/// Вычисляет несмещённую выборочную дисперсию `Σ(x - mean)² / (n - 1)`.
///
/// Деление на `n - 1`, а не на `n`, согласуется с оценками волатильности и
/// ковариаций, используемыми в OLS и ERC.
pub(crate) fn variance(values: &[Decimal]) -> Result<Decimal> {
    if values.len() < 2 {
        return Err(Error::Strategy {
            strategy: "built-in".to_owned(),
            message: "at least two observations are required".to_owned(),
        });
    }
    let average = mean(values)?;
    let squared = values
        .iter()
        .map(|value| {
            let difference = *value - average;
            difference * difference
        })
        .sum::<Decimal>();
    let denominator = Decimal::from(
        u64::try_from(values.len() - 1)
            .map_err(|_| Error::Configuration("sample is too large".to_owned()))?,
    );
    Ok(squared / denominator)
}

/// Вычисляет выборочную ковариацию двух синхронных рядов.
///
/// Ряды должны быть одинаковой длины: индекс в обоих срезах обозначает одну и ту
/// же торговую сессию.
pub(crate) fn covariance(left: &[Decimal], right: &[Decimal]) -> Result<Decimal> {
    if left.len() != right.len() || left.len() < 2 {
        return Err(Error::Strategy {
            strategy: "built-in".to_owned(),
            message: "covariance samples must have the same length of at least two".to_owned(),
        });
    }
    let left_mean = mean(left)?;
    let right_mean = mean(right)?;
    let sum = left
        .iter()
        .zip(right)
        .map(|(left, right)| (*left - left_mean) * (*right - right_mean))
        .sum::<Decimal>();
    let denominator = Decimal::from(
        u64::try_from(left.len() - 1)
            .map_err(|_| Error::Configuration("sample is too large".to_owned()))?,
    );
    Ok(sum / denominator)
}

/// Вычисляет квадратный корень методом Ньютона без перехода к `f64`.
///
/// Итерация `next = (x + value / x) / 2` останавливается при изменении не более
/// `10⁻¹⁸` либо после 96 шагов. Отрицательное подкоренное значение — ошибка данных.
pub(crate) fn square_root(value: Decimal) -> Result<Decimal> {
    if value < Decimal::ZERO {
        return Err(Error::Strategy {
            strategy: "built-in".to_owned(),
            message: "cannot calculate a square root of a negative value".to_owned(),
        });
    }
    if value.is_zero() {
        return Ok(Decimal::ZERO);
    }

    let two = Decimal::from(2);
    let mut estimate = if value > Decimal::ONE {
        value / two
    } else {
        Decimal::ONE
    };
    let tolerance = Decimal::new(1, 18);
    // Метод Ньютона квадратично сходится для положительного начального приближения.
    for _ in 0..96 {
        let next = (estimate + value / estimate) / two;
        if (next - estimate).abs() <= tolerance {
            return Ok(next);
        }
        estimate = next;
    }
    Ok(estimate)
}

/// Преобразует закрытия свечей в простые доходности `P[t] / P[t-1] - 1`.
///
/// Для `n` свечей возвращается `n - 1` доходностей. Неположительная предыдущая
/// цена отклоняется, так как отношение цен в этом случае финансово бессмысленно.
pub(crate) fn simple_returns(candles: &[Candle]) -> Result<Vec<Decimal>> {
    candles
        .windows(2)
        .map(|window| {
            let previous = window[0].close.0;
            if previous <= Decimal::ZERO {
                return Err(Error::Strategy {
                    strategy: "built-in".to_owned(),
                    message: "close prices must be positive".to_owned(),
                });
            }
            Ok(window[1].close.0 / previous - Decimal::ONE)
        })
        .collect()
}

/// Возвращает доходность между двумя границами, заданными числом свечей назад.
///
/// Для `(252, 21)` формула равна `P[t-21] / P[t-252] - 1`, то есть последний
/// условный месяц исключён. `None` означает недостаток истории или неверные границы.
pub(crate) fn return_between(
    candles: &[Candle],
    start_days_ago: usize,
    end_days_ago: usize,
) -> Option<Decimal> {
    if start_days_ago <= end_days_ago || candles.len() <= start_days_ago {
        return None;
    }
    let end_index = candles.len().checked_sub(1 + end_days_ago)?;
    let start_index = candles.len().checked_sub(1 + start_days_ago)?;
    let start = candles.get(start_index)?.close.0;
    let end = candles.get(end_index)?.close.0;
    (start > Decimal::ZERO).then_some(end / start - Decimal::ONE)
}

/// Оценивает однофакторную OLS-регрессию `y = alpha + beta * x + residual`.
///
/// Возвращает `(alpha, beta)`, где `beta = cov(y, x) / var(x)`, а
/// `alpha = mean(y) - beta * mean(x)`. Нулевая дисперсия фактора не позволяет
/// идентифицировать beta и поэтому считается ошибкой.
pub(crate) fn ordinary_least_squares(
    dependent: &[Decimal],
    independent: &[Decimal],
) -> Result<(Decimal, Decimal)> {
    if dependent.len() != independent.len() || dependent.len() < 2 {
        return Err(Error::Strategy {
            strategy: "built-in".to_owned(),
            message: "regression samples must have equal lengths of at least two".to_owned(),
        });
    }
    let independent_variance = variance(independent)?;
    if independent_variance.is_zero() {
        return Err(Error::Strategy {
            strategy: "built-in".to_owned(),
            message: "benchmark variance is zero".to_owned(),
        });
    }
    let beta = covariance(dependent, independent)? / independent_variance;
    let alpha = mean(dependent)? - beta * mean(independent)?;
    Ok((alpha, beta))
}

/// Строит dollar-neutral портфель из нижнего и верхнего хвостов ранга.
///
/// `scores` сортируются по возрастанию; UID служит стабильным tie-breaker. В каждом
/// хвосте выбирается `max(1, floor(n * quantile_percent / 100))` бумаг. Long- и
/// short-стороны получают по 0,5 gross, поэтому при наличии обеих сторон общий
/// gross равен 1, а net — 0. При `high_is_long = false` направления инвертируются.
pub(crate) fn rank_long_short(
    universe: &[InstrumentId],
    mut scores: Vec<(InstrumentId, Decimal)>,
    quantile_percent: usize,
    high_is_long: bool,
) -> Option<Vec<(InstrumentId, Decimal)>> {
    if scores.len() < 2 || quantile_percent == 0 || quantile_percent >= 50 {
        return None;
    }
    scores.sort_by(|left, right| {
        left.1
            .partial_cmp(&right.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.0.as_str().cmp(right.0.as_str()))
    });
    let count = scores
        .len()
        .saturating_mul(quantile_percent)
        .checked_div(100)
        .unwrap_or_default()
        .max(1)
        .min(scores.len() / 2);
    let low = &scores[..count];
    let high = &scores[scores.len() - count..];
    // Каждая сторона использует половину капитала: 0,5 / число бумаг в хвосте.
    let leg_weight = Decimal::new(5, 1) / Decimal::from(u64::try_from(count).ok()?);

    let mut selected = Vec::with_capacity(count * 2);
    for (instrument_id, _) in low {
        selected.push((
            instrument_id.clone(),
            if high_is_long {
                -leg_weight
            } else {
                leg_weight
            },
        ));
    }
    for (instrument_id, _) in high {
        selected.push((
            instrument_id.clone(),
            if high_is_long {
                leg_weight
            } else {
                -leg_weight
            },
        ));
    }
    Some(fill_universe(universe, &selected))
}

/// Строит long-only портфель из инструментов с минимальными значениями score.
///
/// Выбранные бумаги получают равные веса с индивидуальным потолком 0,5. Функция
/// применяется к low-volatility, где меньшая оценка риска считается лучшей.
pub(crate) fn rank_long_only_low(
    universe: &[InstrumentId],
    mut scores: Vec<(InstrumentId, Decimal)>,
    quantile_percent: usize,
) -> Option<Vec<(InstrumentId, Decimal)>> {
    if scores.is_empty() || quantile_percent == 0 || quantile_percent > 100 {
        return None;
    }
    scores.sort_by(|left, right| {
        left.1
            .partial_cmp(&right.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.0.as_str().cmp(right.0.as_str()))
    });
    let count = scores
        .len()
        .saturating_mul(quantile_percent)
        .checked_div(100)
        .unwrap_or_default()
        .max(1)
        .min(scores.len());
    let weight = Decimal::ONE / Decimal::from(u64::try_from(count).ok()?);
    let selected = scores[..count]
        .iter()
        .map(|(instrument_id, _)| (instrument_id.clone(), weight.min(MAX_INSTRUMENT_WEIGHT)))
        .collect::<Vec<_>>();
    Some(fill_universe(universe, &selected))
}

/// Масштабирует произвольные signed scores до заданного суммарного gross.
///
/// Gross — это `Σ|weight|`. Алгоритм «water filling» сначала фиксирует инструменты,
/// чей пропорциональный вес превысил бы 0,5, вычитает их из доступного gross, затем
/// перераспределяет остаток пропорционально между незажатыми инструментами. Знаки
/// исходных score сохраняются.
pub(crate) fn normalize_gross(
    universe: &[InstrumentId],
    raw: &[(InstrumentId, Decimal)],
    target_gross: Decimal,
) -> Vec<(InstrumentId, Decimal)> {
    let mut allocations = raw
        .iter()
        .filter(|(_, value)| !value.is_zero())
        .map(|(instrument_id, value)| (instrument_id.clone(), *value, false))
        .collect::<Vec<_>>();
    let target_gross = target_gross.max(Decimal::ZERO);
    let mut remaining = target_gross;

    // Повтор нужен, потому что после фиксации одного крупного веса другие доли
    // пересчитываются относительно меньшего остатка и тоже могут упереться в лимит.
    loop {
        let raw_total = allocations
            .iter()
            .filter(|(_, _, fixed)| !fixed)
            .map(|(_, value, _)| value.abs())
            .sum::<Decimal>();
        if raw_total.is_zero() || remaining <= Decimal::ZERO {
            break;
        }
        let mut newly_fixed = false;
        for (_, value, fixed) in &mut allocations {
            if *fixed {
                continue;
            }
            let proposed = remaining * value.abs() / raw_total;
            if proposed > MAX_INSTRUMENT_WEIGHT {
                *value = if value.is_sign_negative() {
                    -MAX_INSTRUMENT_WEIGHT
                } else {
                    MAX_INSTRUMENT_WEIGHT
                };
                *fixed = true;
                remaining -= MAX_INSTRUMENT_WEIGHT;
                newly_fixed = true;
            }
        }
        if !newly_fixed {
            for (_, value, fixed) in &mut allocations {
                if !*fixed {
                    let sign = if value.is_sign_negative() {
                        -Decimal::ONE
                    } else {
                        Decimal::ONE
                    };
                    *value = sign * remaining * value.abs() / raw_total;
                    *fixed = true;
                }
            }
            break;
        }
    }

    let selected = allocations
        .into_iter()
        .map(|(instrument_id, value, _)| (instrument_id, value))
        .collect::<Vec<_>>();
    fill_universe(universe, &selected)
}

/// Независимо нормализует long- и short-части до `+0,5` и `-0,5` gross.
///
/// Внутри каждой стороны сохраняются относительные величины исходных score. Если
/// одна сторона пуста, другая всё равно занимает только 0,5: функция не удваивает
/// односторонний риск ради достижения gross 1.
pub(crate) fn normalize_long_short(
    universe: &[InstrumentId],
    raw: &[(InstrumentId, Decimal)],
) -> Vec<(InstrumentId, Decimal)> {
    let positive_total = raw
        .iter()
        .filter(|(_, value)| value.is_sign_positive())
        .map(|(_, value)| *value)
        .sum::<Decimal>();
    let negative_total = raw
        .iter()
        .filter(|(_, value)| value.is_sign_negative())
        .map(|(_, value)| value.abs())
        .sum::<Decimal>();
    let side_gross = Decimal::new(5, 1);
    // Нормализация выполняется отдельно, поэтому сумма положительных весов равна
    // +0,5, а сумма абсолютных отрицательных весов — 0,5.
    let selected = raw
        .iter()
        .filter_map(|(instrument_id, value)| {
            let weight = if value.is_sign_positive() && !positive_total.is_zero() {
                side_gross * *value / positive_total
            } else if value.is_sign_negative() && !negative_total.is_zero() {
                -(side_gross * value.abs() / negative_total)
            } else {
                Decimal::ZERO
            };
            (!weight.is_zero()).then(|| (instrument_id.clone(), weight))
        })
        .collect::<Vec<_>>();
    fill_universe(universe, &selected)
}

/// Дополняет частичный target нулевыми весами для всей торгуемой вселенной.
///
/// Полный список важен: нулевой вес явно приказывает planner закрыть существующую
/// позицию, тогда как отсутствие UID могло бы неоднозначно означать «не менять».
pub(crate) fn fill_universe(
    universe: &[InstrumentId],
    selected: &[(InstrumentId, Decimal)],
) -> Vec<(InstrumentId, Decimal)> {
    universe
        .iter()
        .map(|instrument_id| {
            let weight = selected
                .iter()
                .find(|(candidate, _)| candidate == instrument_id)
                .map_or(Decimal::ZERO, |(_, weight)| *weight);
            (instrument_id.clone(), weight)
        })
        .collect()
}

/// Строит симметричную выборочную ковариационную матрицу рядов доходностей.
///
/// Если длины рядов различаются, используются их одинаковые последние хвосты —
/// свежие наблюдения всех инструментов. Элемент `[i][j]` равен `cov(r_i, r_j)`.
pub(crate) fn covariance_matrix(series: &[Vec<Decimal>]) -> Result<Vec<Vec<Decimal>>> {
    if series.is_empty() || series.iter().any(|values| values.len() < 2) {
        return Err(Error::Strategy {
            strategy: "equal-risk-contribution".to_owned(),
            message: "all covariance series need at least two observations".to_owned(),
        });
    }
    let length = series.iter().map(Vec::len).min().unwrap_or_default();
    let aligned = series
        .iter()
        .map(|values| &values[values.len() - length..])
        .collect::<Vec<_>>();
    let mut matrix = vec![vec![Decimal::ZERO; series.len()]; series.len()];
    for row in 0..series.len() {
        for column in row..series.len() {
            let value = covariance(aligned[row], aligned[column])?;
            matrix[row][column] = value;
            matrix[column][row] = value;
        }
    }
    Ok(matrix)
}

/// Применяет диагональный shrinkage к ковариационной матрице.
///
/// Диагональные дисперсии сохраняются, а каждая ковариация умножается на
/// `1 - diagonal_weight`. Это снижает влияние нестабильных корреляций; при весе 1
/// матрица становится диагональной.
pub(crate) fn shrink_covariance(matrix: &mut [Vec<Decimal>], diagonal_weight: Decimal) {
    let off_diagonal_weight = Decimal::ONE - diagonal_weight;
    for (row_index, row) in matrix.iter_mut().enumerate() {
        for (column_index, value) in row.iter_mut().enumerate() {
            if row_index != column_index {
                *value *= off_diagonal_weight;
            }
        }
    }
}

/// Итерационно находит long-only веса Equal Risk Contribution.
///
/// Для весов `w` маржинальный риск в терминах дисперсии равен `Σw`, а вклад
/// инструмента `RC_i = w_i * (Σw)_i`. Цель — сделать все `RC_i` равными
/// `portfolio_variance / n`. На каждом шаге вес корректируется множителем
/// `sqrt(target_RC / current_RC)` и снова нормируется до суммы 1.
///
/// Возвращается последнее приближение, даже если лимит итераций достигнут; ошибки
/// используются только для некорректной матрицы или неположительной дисперсии.
pub(crate) fn equal_risk_weights(
    covariance: &[Vec<Decimal>],
    max_iterations: usize,
    tolerance: Decimal,
) -> Result<Vec<Decimal>> {
    let count = covariance.len();
    if count == 0 || covariance.iter().any(|row| row.len() != count) {
        return Err(Error::Strategy {
            strategy: "equal-risk-contribution".to_owned(),
            message: "covariance matrix must be non-empty and square".to_owned(),
        });
    }
    let mut weights = vec![
        Decimal::ONE
            / Decimal::from(u64::try_from(count).map_err(|_| {
                Error::Configuration("universe is too large".to_owned())
            })?,);
        count
    ];
    let count_decimal = Decimal::from(
        u64::try_from(count)
            .map_err(|_| Error::Configuration("universe is too large".to_owned()))?,
    );

    for _ in 0..max_iterations {
        // marginal = Σw; scalar w'Σw ниже — полная дисперсия портфеля.
        let marginal = covariance
            .iter()
            .map(|row| {
                row.iter()
                    .zip(&weights)
                    .map(|(covariance, weight)| *covariance * *weight)
                    .sum::<Decimal>()
            })
            .collect::<Vec<_>>();
        let portfolio_variance = weights
            .iter()
            .zip(&marginal)
            .map(|(weight, marginal)| *weight * *marginal)
            .sum::<Decimal>();
        if portfolio_variance <= Decimal::ZERO {
            return Err(Error::Strategy {
                strategy: "equal-risk-contribution".to_owned(),
                message: "portfolio variance is not positive".to_owned(),
            });
        }
        let target_contribution = portfolio_variance / count_decimal;
        let contributions = weights
            .iter()
            .zip(&marginal)
            .map(|(weight, marginal)| *weight * *marginal)
            .collect::<Vec<_>>();
        let maximum_error = contributions
            .iter()
            .map(|contribution| (*contribution - target_contribution).abs())
            .max()
            .unwrap_or_default();
        if maximum_error <= tolerance * portfolio_variance {
            return Ok(weights);
        }

        // Инструмент с чрезмерным вкладом получает множитель < 1, с недостаточным
        // вкладом — > 1. Квадратный корень смягчает шаг и стабилизирует сходимость.
        for index in 0..count {
            if contributions[index] > Decimal::ZERO {
                weights[index] *= square_root(target_contribution / contributions[index])?;
            }
        }
        let total = weights.iter().copied().sum::<Decimal>();
        if total <= Decimal::ZERO {
            return Err(Error::Strategy {
                strategy: "equal-risk-contribution".to_owned(),
                message: "ERC solver produced zero weights".to_owned(),
            });
        }
        for weight in &mut weights {
            *weight /= total;
        }
    }
    Ok(weights)
}
