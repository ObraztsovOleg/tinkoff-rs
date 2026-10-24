//! Planner: полный target со знаковыми весами превращается в ноги заявок в
//! лотах — сначала сокращения, затем открытия.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{EffectiveRiskConfig, InstrumentInfo, RiskDecision, RiskMode, TargetPortfolio};
use crate::{
    Error, Result,
    domain::{
        AccountId, ExecutionPhase, InstrumentId, MarginConsent, OrderIntent, OrderSide,
        PortfolioSnapshot, Price, StrategyId,
    },
};

/// Жизненный цикл durable batch ребалансировки.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RebalanceBatchStatus {
    /// Target и ноги сохранены, исполнение ещё не началось.
    Planned,
    /// Выполняются закрытия и сокращения позиций.
    Reducing,
    /// Сокращения подтверждены, выполняются новые открытия.
    Opening,
    /// Хотя бы одна нога ещё не подтверждена как полностью исполненная.
    AwaitingFills,
    /// Все требуемые ноги исполнены.
    Completed,
    /// Batch остановлен из-за терминальной ошибки или запрета риска.
    Failed,
}

impl RebalanceBatchStatus {
    #[must_use]
    /// Возвращает стабильное значение статуса для `PostgreSQL`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Reducing => "reducing",
            Self::Opening => "opening",
            Self::AwaitingFills => "awaiting_fills",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

/// Зафиксированное решение о ребалансировке и действовавшие при нём лимиты.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RebalanceBatch {
    /// UUID batch, совпадающий с `target.batch_id`.
    pub id: Uuid,
    /// Идемпотентный ID входного события.
    pub event_id: String,
    /// Стратегия-источник решения.
    pub strategy_id: StrategyId,
    /// Выделенный Sandbox-счёт.
    pub account_id: AccountId,
    /// Полный целевой портфель.
    pub target: TargetPortfolio,
    /// Снимок risk-лимитов в момент планирования.
    pub effective_risk: EffectiveRiskConfig,
    /// Капитал, использованный при переводе весов в лоты.
    #[serde(with = "rust_decimal::serde::str")]
    pub sizing_capital: Decimal,
    /// Текущая стадия исполнения.
    pub status: RebalanceBatchStatus,
    /// Время создания плана.
    pub created_at: DateTime<Utc>,
}

/// Все свежие данные, необходимые для безопасного перевода target в заявки.
#[derive(Clone, Debug)]
pub struct PlanningContext {
    /// Текущие фактические позиции счёта.
    pub portfolio: PortfolioSnapshot,
    /// Проверенные свойства всей торговой вселенной.
    pub instruments: Vec<InstrumentInfo>,
    /// Актуальные цены одного инструмента, не одного лота.
    pub prices: HashMap<InstrumentId, Price>,
    /// Последнее решение risk manager-а.
    pub risk: RiskDecision,
    /// Изменение позиции меньше этой доли `sizing_capital` не исполняется:
    /// мелкая перебалансировка только платит комиссию. Ноль — без порога.
    pub min_trade_fraction: Decimal,
}

/// Результат planner-а: durable batch и упорядоченные ноги.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedBatch {
    /// Метаданные и полный target.
    pub batch: RebalanceBatch,
    /// Сначала все reduction legs, затем все opening legs.
    pub intents: Vec<OrderIntent>,
}

/// Проверяет target и детерминированно переводит знаковые веса в лоты.
#[derive(Clone, Copy, Debug, Default)]
pub struct TargetPlanner;

impl TargetPlanner {
    /// Строит двухфазный batch относительно фактического портфеля.
    ///
    /// Для каждого инструмента используется формула:
    ///
    /// `target_lots = trunc_toward_zero(
    ///     sizing_capital * signed_weight / (piece_value(price) * lot_size)
    /// )`.
    ///
    /// Усечение к нулю не позволяет округлением увеличить абсолютную позицию.
    pub fn plan(
        &self,
        target: &TargetPortfolio,
        context: &PlanningContext,
    ) -> Result<PlannedBatch> {
        validate_target(target, context)?;
        let mut effective_target = target.clone();
        if context.risk.mode == RiskMode::CloseOnly {
            // В close-only сигнал стратегии не исполняется: полный target
            // принудительно становится нулевым и planner строит ликвидацию.
            for target_weight in &mut effective_target.weights {
                target_weight.weight = Decimal::ZERO;
            }
        }

        let positions = current_lots(&context.portfolio, &context.instruments)?;
        let info_by_id: HashMap<_, _> = context
            .instruments
            .iter()
            .map(|info| (info.instrument_id.clone(), info))
            .collect();
        let mut weights = effective_target.weights.clone();
        weights.sort_by(|left, right| {
            left.instrument_id
                .as_str()
                .cmp(right.instrument_id.as_str())
        });

        let mut reductions = Vec::new();
        let mut openings = Vec::new();
        for target_weight in weights {
            let info = info_by_id[&target_weight.instrument_id];
            let price = context
                .prices
                .get(&target_weight.instrument_id)
                .ok_or_else(|| {
                    Error::RiskRejected(format!(
                        "missing price for `{}`",
                        target_weight.instrument_id
                    ))
                })?;
            if price.0 <= Decimal::ZERO {
                return Err(Error::RiskRejected(format!(
                    "non-positive price for `{}`",
                    target_weight.instrument_id
                )));
            }
            // Знаковая стоимость позиции = разрешённый капитал × target weight.
            let target_value = context.risk.sizing_capital * target_weight.weight;
            // API принимает лоты: рублёвая стоимость бумаги (с учётом типа:
            // пункты фьючерса, проценты номинала облигации) умножается на лот.
            let lot_value = info.piece_value(price.0) * Decimal::from(info.lot_size);
            // Резерв под комиссию: лот стоит вместе с комиссией за сделку.
            let lot_cost = lot_value * (Decimal::ONE + info.commission_rate);
            // Decimal::trunc округляет и long, и short к нулю.
            let desired_lots = (target_value / lot_cost).trunc().to_i64().ok_or_else(|| {
                Error::RiskRejected(format!(
                    "target lots overflow for `{}`",
                    target_weight.instrument_id
                ))
            })?;
            let current = positions
                .get(&target_weight.instrument_id)
                .copied()
                .unwrap_or_default();
            // `abs(i64::MIN)` и разность двух противоположных крайних значений
            // не представимы в i64. Такое количество всё равно нельзя передать
            // broker API, поэтому отклоняем snapshot до построения intents.
            if desired_lots == i64::MIN || current == i64::MIN {
                return Err(Error::RiskRejected(format!(
                    "position lots are outside the supported range for `{}`",
                    target_weight.instrument_id
                )));
            }
            let desired_lots = after_trade_band(
                current,
                desired_lots,
                lot_value,
                context.min_trade_fraction * context.risk.sizing_capital,
            );
            append_legs(
                &effective_target,
                target_weight.instrument_id,
                current,
                desired_lots,
                info.lot_size,
                &mut reductions,
                &mut openings,
            )?;
        }
        // Порядок внутри Vec является частью протокола исполнения: ни одна
        // opening leg не должна предшествовать reduction leg.
        reductions.extend(openings);
        for (index, intent) in reductions.iter_mut().enumerate() {
            intent.leg_index = i32::try_from(index)
                .map_err(|_| Error::RiskRejected("rebalance contains too many legs".to_owned()))?;
        }
        Ok(PlannedBatch {
            batch: RebalanceBatch {
                id: effective_target.batch_id,
                event_id: effective_target.event_id.clone(),
                strategy_id: effective_target.strategy_id.clone(),
                account_id: effective_target.account_id.clone(),
                target: effective_target,
                effective_risk: context.risk.effective.clone(),
                sizing_capital: context.risk.sizing_capital,
                status: RebalanceBatchStatus::Planned,
                created_at: Utc::now(),
            },
            intents: reductions,
        })
    }
}

fn validate_target(target: &TargetPortfolio, context: &PlanningContext) -> Result<()> {
    // Проверка времени запрещает look-ahead (`source_as_of > generated_at`) и
    // исполнение устаревшего решения (`now > valid_until`).
    if target.event_id.trim().is_empty()
        || target.source_as_of > target.generated_at
        || target.generated_at > target.valid_until
        || Utc::now() > target.valid_until
    {
        return Err(Error::Strategy {
            strategy: target.strategy_id.to_string(),
            message: "target has an empty event id, invalid timestamps, or has expired".to_owned(),
        });
    }
    if target.account_id != context.portfolio.account_id {
        return Err(Error::RiskRejected(
            "target and portfolio account mismatch".to_owned(),
        ));
    }
    if context.instruments.is_empty() {
        return Err(Error::Configuration(
            "tradable universe cannot be empty".to_owned(),
        ));
    }
    let universe: HashSet<_> = context
        .instruments
        .iter()
        .map(|info| info.instrument_id.clone())
        .collect();
    if universe.len() != context.instruments.len() || target.weights.len() != universe.len() {
        return Err(Error::Strategy {
            strategy: target.strategy_id.to_string(),
            message: "target must contain exactly one weight for every tradable instrument"
                .to_owned(),
        });
    }
    let mut seen = HashSet::new();
    // gross = sum(abs(weight)); net = sum(weight). Проверяются обе величины:
    // market-neutral target может иметь net=0, но всё равно gross=2.
    let mut gross = Decimal::ZERO;
    let mut net = Decimal::ZERO;
    for weight in &target.weights {
        if !universe.contains(&weight.instrument_id) || !seen.insert(weight.instrument_id.clone()) {
            return Err(Error::Strategy {
                strategy: target.strategy_id.to_string(),
                message: format!("unknown or duplicate target `{}`", weight.instrument_id),
            });
        }
        if weight.weight.abs() > context.risk.effective.max_instrument_exposure {
            return Err(Error::RiskRejected(format!(
                "instrument `{}` exceeds the per-instrument exposure limit",
                weight.instrument_id
            )));
        }
        let info = context
            .instruments
            .iter()
            .find(|info| info.instrument_id == weight.instrument_id)
            .expect("validated universe membership");
        if weight.weight < Decimal::ZERO && !info.short_enabled {
            return Err(Error::RiskRejected(format!(
                "short is unavailable for `{}`",
                weight.instrument_id
            )));
        }
        if info.lot_size <= 0
            || !info.api_trade_available
            || !info.market_order_available
            || !info.currency.eq_ignore_ascii_case("rub")
            || !info.exchange.to_ascii_lowercase().contains("moex")
        {
            return Err(Error::RiskRejected(format!(
                "instrument `{}` is not an available RUB/MOEX market-order instrument",
                weight.instrument_id
            )));
        }
        gross += weight.weight.abs();
        net += weight.weight;
    }
    if gross > context.risk.effective.max_gross_exposure
        || net.abs() > context.risk.effective.max_net_exposure
    {
        return Err(Error::RiskRejected(
            "target exceeds gross or net exposure limit".to_owned(),
        ));
    }
    Ok(())
}

/// Мелкое изменение позиции не исполняется: комиссия за него не окупается.
/// Закрытие позиции и разворот исполняются всегда; новая позиция меньше
/// порога не открывается.
fn after_trade_band(current: i64, desired: i64, lot_value: Decimal, band: Decimal) -> i64 {
    if band <= Decimal::ZERO || desired == current || desired == 0 {
        return desired;
    }
    if current != 0 && current.signum() != desired.signum() {
        return desired;
    }
    let change = Decimal::from(desired.abs_diff(current)) * lot_value;
    if change < band { current } else { desired }
}

fn current_lots(
    portfolio: &PortfolioSnapshot,
    instruments: &[InstrumentInfo],
) -> Result<HashMap<InstrumentId, i64>> {
    let info: HashMap<_, _> = instruments
        .iter()
        .map(|item| (item.instrument_id.clone(), item))
        .collect();
    let mut lots = HashMap::new();
    for position in &portfolio.positions {
        let Some(instrument) = info.get(&position.instrument_id) else {
            if !position.quantity.is_zero() {
                return Err(Error::RiskRejected(format!(
                    "portfolio contains an instrument outside the configured universe: `{}`",
                    position.instrument_id
                )));
            }
            continue;
        };
        if position.blocked {
            return Err(Error::RiskRejected(format!(
                "position `{}` is blocked",
                position.instrument_id
            )));
        }
        // Portfolio API хранит количество бумаг, planner работает в лотах.
        // Дробный результат означает несогласованные данные, а не повод округлять.
        let quotient = position.quantity / Decimal::from(instrument.lot_size);
        if quotient.fract() != Decimal::ZERO {
            return Err(Error::RiskRejected(format!(
                "position `{}` is not divisible by its lot size",
                position.instrument_id
            )));
        }
        lots.insert(
            position.instrument_id.clone(),
            quotient.to_i64().ok_or_else(|| {
                Error::RiskRejected(format!(
                    "position lots overflow for `{}`",
                    position.instrument_id
                ))
            })?,
        );
    }
    Ok(lots)
}

fn append_legs(
    target: &TargetPortfolio,
    instrument_id: InstrumentId,
    current: i64,
    desired: i64,
    lot_size: i64,
    reductions: &mut Vec<OrderIntent>,
    openings: &mut Vec<OrderIntent>,
) -> Result<()> {
    if current == desired {
        return Ok(());
    }
    // `common` — позиция, которую можно оставить без сделки. При сохранении
    // направления сначала сокращаем модуль до common, затем при необходимости
    // увеличиваем. При развороте long↔short common равен нулю: старая сторона
    // полностью закрывается до открытия новой.
    let common = if current.signum() == desired.signum() {
        current.abs().min(desired.abs()) * current.signum()
    } else {
        0
    };
    if current != common {
        reductions.push(make_intent(
            target,
            instrument_id.clone(),
            current,
            common,
            lot_size,
            ExecutionPhase::Reduction,
        )?);
    }
    if common != desired {
        openings.push(make_intent(
            target,
            instrument_id,
            common,
            desired,
            lot_size,
            ExecutionPhase::Opening,
        )?);
    }
    Ok(())
}

fn make_intent(
    target: &TargetPortfolio,
    instrument_id: InstrumentId,
    from_lots: i64,
    target_lots: i64,
    lot_size: i64,
    phase: ExecutionPhase,
) -> Result<OrderIntent> {
    // Знак delta определяет сторону, а API получает положительное число лотов.
    let delta = target_lots.checked_sub(from_lots).ok_or_else(|| {
        Error::RiskRejected(format!(
            "order delta overflow for instrument `{instrument_id}`"
        ))
    })?;
    let lots = delta.checked_abs().ok_or_else(|| {
        Error::RiskRejected(format!(
            "order size overflow for instrument `{instrument_id}`"
        ))
    })?;
    Ok(OrderIntent::market(
        target.batch_id,
        0,
        target.event_id.clone(),
        target.strategy_id.clone(),
        target.account_id.clone(),
        instrument_id,
        if delta > 0 {
            OrderSide::Buy
        } else {
            OrderSide::Sell
        },
        lots,
        target_lots,
        lot_size,
        phase,
        if phase == ExecutionPhase::Reduction {
            MarginConsent::CoveredOnly
        } else {
            MarginConsent::UncoveredAllowed
        },
        target.generated_at,
    ))
}
