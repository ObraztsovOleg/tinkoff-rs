//! Risk-policy счёта: бюджет, дневной стоп и стоп по просадке, отделение
//! внешних денежных потоков от результата стратегии.

use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::PORTFOLIO_RUNTIME_VERSION;
use crate::{
    Error, Result,
    domain::{AccountId, StrategyId},
};

/// Настраиваемая часть risk-policy.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RiskConfig {
    /// Максимальный капитал стратегии в рублях; прибыль выше него не реинвестируется.
    #[serde(with = "rust_decimal::serde::str")]
    pub budget_rub: Decimal,
    /// Допустимый убыток от начала сессии в процентах; `None` отключает stop.
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub daily_loss_percent: Option<Decimal>,
    /// Допустимая просадка от high-water mark; `None` отключает stop.
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub max_drawdown_percent: Option<Decimal>,
}

impl RiskConfig {
    /// Проверяет денежный формат бюджета и согласованность процентных лимитов.
    pub fn validate(&self) -> Result<()> {
        if self.budget_rub <= Decimal::ZERO || self.budget_rub.scale() > 2 {
            return Err(Error::Configuration(
                "risk.budget_rub must be positive and have at most two decimal places".to_owned(),
            ));
        }
        for (name, value) in [
            ("daily_loss_percent", self.daily_loss_percent),
            ("max_drawdown_percent", self.max_drawdown_percent),
        ] {
            if value
                .is_some_and(|percent| percent <= Decimal::ZERO || percent > Decimal::ONE_HUNDRED)
            {
                return Err(Error::Configuration(format!(
                    "risk.{name} must be greater than zero and no greater than 100"
                )));
            }
        }
        if self
            .daily_loss_percent
            .zip(self.max_drawdown_percent)
            .is_some_and(|(daily, drawdown)| daily > drawdown)
        {
            return Err(Error::Configuration(
                "risk.daily_loss_percent cannot exceed risk.max_drawdown_percent".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Полная risk-конфигурация, зафиксированная вместе с торговым решением.
///
/// Помимо настроек пользователя содержит неизменяемые системные ограничения:
/// gross `2`, absolute net `1` и вес одного инструмента `0.5`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EffectiveRiskConfig {
    /// Настройки текущего risk epoch.
    pub configured: RiskConfig,
    /// Максимальная сумма абсолютных весов портфеля.
    #[serde(with = "rust_decimal::serde::str")]
    pub max_gross_exposure: Decimal,
    /// Максимальный модуль суммы знаковых весов.
    #[serde(with = "rust_decimal::serde::str")]
    pub max_net_exposure: Decimal,
    /// Максимальный модуль веса отдельного инструмента.
    #[serde(with = "rust_decimal::serde::str")]
    pub max_instrument_exposure: Decimal,
}

impl From<RiskConfig> for EffectiveRiskConfig {
    fn from(configured: RiskConfig) -> Self {
        Self {
            configured,
            max_gross_exposure: Decimal::TWO,
            max_net_exposure: Decimal::ONE,
            max_instrument_exposure: Decimal::new(5, 1),
        }
    }
}

/// Причина запрета на увеличение риска.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskBlockReason {
    /// Достигнут дневной процент убытка.
    DailyLoss,
    /// Достигнута максимальная просадка от high-water mark.
    MaxDrawdown,
    /// Обнаружено пополнение, вывод или движение бумаг неизвестного происхождения.
    UnknownExternalCashFlow,
}

/// Сохранённая блокировка торговли.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RiskBlock {
    /// Причина перехода в close-only.
    pub reason: RiskBlockReason,
    /// Время обнаружения условия.
    pub blocked_at: DateTime<Utc>,
    /// Сессия, в которой возникла блокировка.
    pub session_date: NaiveDate,
    /// Человекочитаемое пояснение для CLI и аудита.
    pub detail: String,
}

/// Разрешённый risk manager-ом набор операций.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskMode {
    /// Можно сокращать и увеличивать позиции в пределах лимитов.
    Normal,
    /// Можно только сокращать позиции до нуля; новые открытия запрещены.
    CloseOnly,
}

/// Durable-состояние risk manager-а для одного счёта и одного risk epoch.
///
/// Оно сохраняет все базы сравнения, поэтому перезапуск процесса не обнуляет
/// дневной stop или накопленную просадку.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AccountRiskState {
    /// Версия формата, защищающая от восстановления legacy-state.
    pub runtime_version: i16,
    /// UUID периода, в котором бюджет и начальный anchor неизменны.
    pub epoch: Uuid,
    /// Выделенный счёт, которому принадлежит состояние.
    pub account_id: AccountId,
    /// Единственная стратегия-владелец risk-state счёта.
    pub strategy_id: StrategyId,
    /// Бюджет фиксируется внутри risk epoch. Его изменение требует reset.
    #[serde(with = "rust_decimal::serde::str")]
    pub budget_rub: Decimal,
    /// Начало epoch используется для сверки внешних движений по счёту.
    pub anchor_observed_at: DateTime<Utc>,
    /// Полная mark-to-market equity счёта в начале epoch.
    #[serde(with = "rust_decimal::serde::str")]
    pub account_equity_at_anchor: Decimal,
    /// `min(budget_rub, account_equity_at_anchor)` — капитал, выделенный стратегии.
    #[serde(with = "rust_decimal::serde::str")]
    pub allocated_principal: Decimal,
    /// Сумма распознанных внешних денежных потоков после anchor.
    #[serde(with = "rust_decimal::serde::str")]
    pub external_cash_flows_since_anchor: Decimal,
    /// Strategy equity в начале текущей торговой сессии.
    #[serde(with = "rust_decimal::serde::str")]
    pub day_start_equity: Decimal,
    /// Максимальная strategy equity за epoch — база drawdown-stop.
    #[serde(with = "rust_decimal::serde::str")]
    pub high_water_equity: Decimal,
    /// Текущая торговая дата MOEX.
    pub session_date: NaiveDate,
    /// Активная блокировка, если runtime находится в close-only.
    pub blocked: Option<RiskBlock>,
    /// Время последней оценки риска.
    pub updated_at: DateTime<Utc>,
    /// Аудит-причина последнего ручного начала нового epoch.
    pub last_reset_reason: Option<String>,
}

/// Исходный снимок счёта для начала нового risk epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RiskAnchor {
    /// Счёт, для которого создаётся epoch.
    pub account_id: AccountId,
    /// Стратегия-владелец счёта.
    pub strategy_id: StrategyId,
    /// Полная mark-to-market equity счёта в момент anchor.
    pub account_equity: Decimal,
    /// Торговая дата MOEX в момент anchor.
    pub session_date: NaiveDate,
    /// Время получения исходного снимка.
    pub observed_at: DateTime<Utc>,
}

/// Свежие данные счёта для очередной оценки риска.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RiskObservation {
    /// Полная текущая mark-to-market equity с позициями и расходами.
    pub current_equity: Decimal,
    /// Текущая торговая дата MOEX.
    pub session_date: NaiveDate,
    /// Время получения наблюдения.
    pub observed_at: DateTime<Utc>,
    /// Накопленная сумма внешних денежных потоков с начала epoch.
    pub external_cash_flows: Decimal,
    /// `false`, если runtime не смог однозначно классифицировать движения.
    pub external_cash_flows_recognized: bool,
    /// Нет ли открытых позиций; требуется для снятия дневной блокировки.
    pub portfolio_is_flat: bool,
}

/// Результат очередной оценки risk-policy и капитал для sizing.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RiskDecision {
    /// Normal либо запрет новых открытий.
    pub mode: RiskMode,
    /// Equity, относимая к стратегии после исключения внешних потоков.
    #[serde(with = "rust_decimal::serde::str")]
    pub strategy_equity: Decimal,
    /// Капитал, который разрешено использовать в расчёте target lots.
    #[serde(with = "rust_decimal::serde::str")]
    pub sizing_capital: Decimal,
    /// Настраиваемые и фиксированные лимиты, действовавшие при решении.
    pub effective: EffectiveRiskConfig,
    /// Новое durable-состояние для атомарного сохранения.
    pub state: AccountRiskState,
}

/// Stateful-калькулятор бюджета, дневного убытка и high-water drawdown.
pub struct RiskManager {
    config: RiskConfig,
    state: AccountRiskState,
}

impl RiskManager {
    /// Начинает новый risk epoch от свежего снимка счёта.
    ///
    /// Выделенный principal равен минимуму бюджета и фактической equity: config
    /// сам по себе не пополняет Sandbox-счёт.
    pub fn new(config: RiskConfig, anchor: RiskAnchor) -> Result<Self> {
        config.validate()?;
        if anchor.account_equity < Decimal::ZERO {
            return Err(Error::Configuration(
                "anchor equity cannot be negative".to_owned(),
            ));
        }
        // allocated_principal = min(budget_rub, account_equity_at_anchor).
        // Это не перевод денег, а верхняя граница доли счёта для стратегии.
        let allocated = config.budget_rub.min(anchor.account_equity);
        let budget_rub = config.budget_rub;
        Ok(Self {
            config,
            state: AccountRiskState {
                runtime_version: PORTFOLIO_RUNTIME_VERSION,
                epoch: Uuid::new_v4(),
                account_id: anchor.account_id,
                strategy_id: anchor.strategy_id,
                budget_rub,
                anchor_observed_at: anchor.observed_at,
                account_equity_at_anchor: anchor.account_equity,
                allocated_principal: allocated,
                external_cash_flows_since_anchor: Decimal::ZERO,
                day_start_equity: allocated,
                high_water_equity: allocated,
                session_date: anchor.session_date,
                blocked: None,
                updated_at: anchor.observed_at,
                last_reset_reason: None,
            },
        })
    }

    /// Восстанавливает manager из durable-state после перезапуска процесса.
    ///
    /// Бюджет является частью epoch. Несовпадение с TOML отклоняется, чтобы
    /// случайное редактирование config не сменило базу расчёта убытка.
    pub fn from_state(config: RiskConfig, state: AccountRiskState) -> Result<Self> {
        config.validate()?;
        if state.runtime_version != PORTFOLIO_RUNTIME_VERSION {
            return Err(Error::Configuration(format!(
                "risk state runtime version {} is unsupported",
                state.runtime_version
            )));
        }
        if state.budget_rub != config.budget_rub {
            return Err(Error::Configuration(format!(
                "risk.budget_rub changed from {} to {}; start a new risk epoch with `risk-reset --reason ...`",
                state.budget_rub, config.budget_rub
            )));
        }
        Ok(Self { config, state })
    }

    #[must_use]
    /// Возвращает конфигурацию активного epoch.
    pub const fn config(&self) -> &RiskConfig {
        &self.config
    }

    #[must_use]
    /// Возвращает последнее вычисленное durable-состояние.
    pub const fn state(&self) -> &AccountRiskState {
        &self.state
    }

    /// Учитывает свежую equity, обновляет stop-ы и возвращает режим торговли.
    ///
    /// Формула отделяет результат стратегии от внесений и выводов после anchor:
    ///
    /// `strategy_equity = allocated_principal + current_account_equity
    ///                     - account_equity_at_anchor - external_cash_flows`.
    pub fn observe(&mut self, observation: &RiskObservation) -> Result<RiskDecision> {
        if observation.current_equity < Decimal::ZERO {
            return Err(Error::RiskRejected(
                "account equity cannot be negative".to_owned(),
            ));
        }
        self.state.external_cash_flows_since_anchor = observation.external_cash_flows;
        // Из изменения полной equity вычитаются внешние потоки. Поэтому обычное
        // пополнение не выглядит прибылью стратегии, а вывод — её убытком.
        let strategy_equity = self.state.allocated_principal + observation.current_equity
            - self.state.account_equity_at_anchor
            - observation.external_cash_flows;

        // На первой оценке новой сессии фиксируем новую базу дневного stop-а.
        if observation.session_date < self.state.session_date {
            return Err(Error::RiskRejected(format!(
                "MOEX session date regressed from {} to {}",
                self.state.session_date, observation.session_date
            )));
        }
        if observation.session_date > self.state.session_date {
            self.state.session_date = observation.session_date;
            self.state.day_start_equity = strategy_equity;
        }
        // Дневной lock снимается лишь в следующую сессию и после ликвидации.
        // Max-drawdown и неизвестный cash flow требуют явного risk-reset.
        if self.state.blocked.as_ref().is_some_and(|block| {
            block.reason == RiskBlockReason::DailyLoss
                && observation.session_date > block.session_date
                && observation.portfolio_is_flat
        }) {
            self.state.blocked = None;
        }
        // High-water mark никогда сам не уменьшается внутри одного epoch.
        self.state.high_water_equity = self.state.high_water_equity.max(strategy_equity);

        if !observation.external_cash_flows_recognized {
            self.block(
                RiskBlockReason::UnknownExternalCashFlow,
                observation.observed_at,
                "unrecognized cash or securities transfer".to_owned(),
            );
        } else if !self.state.blocked.as_ref().is_some_and(|block| {
            matches!(
                block.reason,
                RiskBlockReason::UnknownExternalCashFlow | RiskBlockReason::MaxDrawdown
            )
        }) {
            if threshold_reached(
                self.state.high_water_equity,
                strategy_equity,
                self.config.max_drawdown_percent,
            ) {
                self.block(
                    RiskBlockReason::MaxDrawdown,
                    observation.observed_at,
                    "maximum drawdown reached".to_owned(),
                );
            } else if self.state.blocked.is_none()
                && threshold_reached(
                    self.state.day_start_equity,
                    strategy_equity,
                    self.config.daily_loss_percent,
                )
            {
                self.block(
                    RiskBlockReason::DailyLoss,
                    observation.observed_at,
                    "daily loss limit reached".to_owned(),
                );
            }
        }
        self.state.updated_at = observation.observed_at;
        Ok(self.decision(strategy_equity))
    }

    /// Начинает новый risk epoch с обязательной человекочитаемой причиной.
    ///
    /// Reset запрещён при незавершённых заявках и не позволяет обойти дневной
    /// stop в той же сессии. Именно через этот метод безопасно менять бюджет.
    pub fn reset(
        &mut self,
        new_config: RiskConfig,
        anchor: RiskAnchor,
        reason: &str,
        has_pending_orders: bool,
    ) -> Result<RiskDecision> {
        if reason.trim().is_empty() {
            return Err(Error::Configuration(
                "risk reset requires a reason".to_owned(),
            ));
        }
        if has_pending_orders {
            return Err(Error::RiskRejected(
                "risk reset requires no pending orders".to_owned(),
            ));
        }
        if self
            .state
            .blocked
            .as_ref()
            .is_some_and(|block| block.reason == RiskBlockReason::DailyLoss)
            && anchor.session_date == self.state.session_date
        {
            return Err(Error::RiskRejected(
                "a daily loss stop cannot be reset during the same session".to_owned(),
            ));
        }
        let mut replacement = Self::new(new_config, anchor)?;
        replacement.state.last_reset_reason = Some(reason.to_owned());
        let decision = replacement.decision(replacement.state.allocated_principal);
        *self = replacement;
        Ok(decision)
    }

    fn block(&mut self, reason: RiskBlockReason, at: DateTime<Utc>, detail: String) {
        self.state.blocked = Some(RiskBlock {
            reason,
            blocked_at: at,
            session_date: self.state.session_date,
            detail,
        });
    }

    fn decision(&self, strategy_equity: Decimal) -> RiskDecision {
        RiskDecision {
            mode: if self.state.blocked.is_some() {
                RiskMode::CloseOnly
            } else {
                RiskMode::Normal
            },
            strategy_equity,
            // sizing_capital = min(budget_rub, max(strategy_equity, 0)).
            // Прибыль сверх бюджета не реинвестируется, а убыток сразу уменьшает
            // размер следующих позиций.
            sizing_capital: self
                .config
                .budget_rub
                .min(strategy_equity.max(Decimal::ZERO)),
            effective: self.config.clone().into(),
            state: self.state.clone(),
        }
    }
}

fn threshold_reached(anchor: Decimal, current: Decimal, threshold: Option<Decimal>) -> bool {
    let Some(threshold) = threshold else {
        return false;
    };
    if anchor <= Decimal::ZERO {
        return false;
    }
    // loss_percent = max(anchor - current, 0) / anchor * 100.
    // Рост equity даёт нулевой убыток, а отсутствующий threshold отключает stop.
    (anchor - current).max(Decimal::ZERO) * Decimal::ONE_HUNDRED / anchor >= threshold
}
