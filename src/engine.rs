//! Portfolio-target runtime для одной внешней стратегии и одного выделенного
//! Sandbox-счёта. Внешней стратегией может быть взвешенный ансамбль внутренних
//! сигналов: engine всё равно получает один полный согласованный target.
//!
//! Стратегия здесь отвечает только за полный целевой портфель со знаковыми
//! весами. Перевод весов в лоты, проверка риска, надёжное сохранение решения и
//! сетевые изменения у брокера разделены на самостоятельные этапы. Каждая
//! заявка входит в неизменяемый batch ребалансировки: сначала должны завершиться
//! сокращения позиций, и только после этого разрешается открывать новые.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;
use tonic::Code;
use uuid::Uuid;

use crate::{
    Error, Result,
    broker::{Portfolio, Trading},
    domain::{
        AccountId, Candle, CandleInterval, ExecutionPhase, InstrumentId, MarginConsent,
        OrderIntent, OrderSide, OrderStatus, OrderUpdate, PortfolioSnapshot, Price, StrategyId,
    },
    storage::{BatchExecutionGate, IntentStatus, Storage, StoredIntent},
};

/// Версия формата durable-state текущего portfolio runtime.
///
/// Версия `1` принадлежит удалённой intent-архитектуре и новым runtime не
/// восстанавливается.
pub const PORTFOLIO_RUNTIME_VERSION: i16 = 2;

/// Режим обработки события относительно запуска торгового runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventPhase {
    /// Загрузка исторических данных для заполнения состояния; заявки запрещены.
    Warmup,
    /// Рабочий поток данных, в котором стратегия может сформировать target.
    Live,
}

/// Опорный момент официальной торговой сессии MOEX.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleAnchor {
    /// Начало основной сессии.
    SessionOpen,
    /// Окончание основной сессии.
    SessionClose,
    /// Начало аукциона закрытия.
    ClosingAuctionStart,
}

/// Событие, рассчитанное относительно официального расписания биржи.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ScheduleEvent {
    /// Точка сессии, к которой привязано событие.
    pub anchor: ScheduleAnchor,
    /// Торговая дата MOEX, а не календарная дата процесса в UTC.
    pub session_date: NaiveDate,
    /// Фактический момент с учётом настроенного смещения.
    pub scheduled_at: DateTime<Utc>,
}

/// Полезная нагрузка, которую может получить стратегия.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "data")]
pub enum StrategyEventKind {
    /// Новая историческая либо потоковая свеча.
    Candle(Candle),
    /// Наступление требуемого момента торговой сессии.
    Schedule(ScheduleEvent),
    /// Изменение состояния ранее выставленной заявки.
    Order(OrderUpdate),
    /// Периодическая переоценка risk state без торгового сигнала.
    RiskCheck,
}

/// Идемпотентное входное событие стратегии.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StrategyEvent {
    /// Стабильный уникальный ID: повтор с тем же ID не применяется второй раз.
    pub id: String,
    /// Разрешено ли событию приводить к торговому решению.
    pub phase: EventPhase,
    /// Время исходного факта, а не момент обработки очередью.
    pub occurred_at: DateTime<Utc>,
    /// Тип события и его данные.
    pub payload: StrategyEventKind,
}

/// Роль инструмента в наборе данных стратегии.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataRole {
    /// Инструмент входит в target и может получать ненулевой вес.
    Tradable,
    /// Инструмент нужен лишь для расчёта сигнала, например benchmark.
    Reference,
}

/// История, необходимая стратегии до начала live-обработки.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DataRequirement {
    /// UID инструмента.
    pub instrument_id: InstrumentId,
    /// Торгуемый это инструмент или справочный ряд.
    pub role: DataRole,
    /// Интервал требуемых свечей.
    pub interval: CandleInterval,
    /// Минимальное количество закрытых баров для прогрева.
    pub warmup_bars: usize,
    /// Нужно ли ждать окончательного закрытия текущей свечи.
    pub waiting_close: bool,
}

/// Требование вызвать стратегию в определённый момент сессии.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ScheduleRequirement {
    /// Опорный момент официального расписания.
    pub anchor: ScheduleAnchor,
    /// Знаковое смещение от опорного момента в секундах.
    pub offset_seconds: i64,
}

/// Проверенные торговые свойства инструмента, нужные planner-у.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstrumentInfo {
    /// UID инструмента.
    pub instrument_id: InstrumentId,
    /// Количество бумаг в биржевом лоте.
    pub lot_size: i64,
    /// Валюта расчётов; runtime принимает только RUB.
    pub currency: String,
    /// Площадка инструмента; runtime принимает только MOEX.
    pub exchange: String,
    /// Разрешена ли короткая позиция.
    pub short_enabled: bool,
    /// Разрешена ли торговля через API.
    pub api_trade_available: bool,
    /// Разрешены ли рыночные заявки.
    pub market_order_available: bool,
}

/// Знаковая доля strategy capital для одного инструмента.
///
/// Положительное значение означает long, отрицательное — short, ноль — полное
/// закрытие позиции. Например, `0.25` соответствует 25% `sizing_capital`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TargetWeight {
    /// UID инструмента из настроенной торговой вселенной.
    pub instrument_id: InstrumentId,
    /// Знаковый вес в долях единицы.
    #[serde(with = "rust_decimal::serde::str")]
    pub weight: Decimal,
}

/// Полное желаемое состояние всех инструментов торговой вселенной.
///
/// Target является снимком, а не набором команд: в `weights` обязана быть ровно
/// одна запись для каждого торгуемого UID, включая нулевые веса.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TargetPortfolio {
    /// UUID будущего durable batch.
    pub batch_id: Uuid,
    /// ID события, породившего target.
    pub event_id: String,
    /// Экземпляр стратегии-источника.
    pub strategy_id: StrategyId,
    /// Выделенный Sandbox-счёт стратегии.
    pub account_id: AccountId,
    /// Последний момент рыночных данных, использованных в сигнале.
    pub source_as_of: DateTime<Utc>,
    /// Время вычисления target.
    pub generated_at: DateTime<Utc>,
    /// После этого времени исполнять target нельзя.
    pub valid_until: DateTime<Utc>,
    /// Полный набор знаковых весов торговой вселенной.
    pub weights: Vec<TargetWeight>,
}

#[async_trait]
/// Контракт stateful-стратегии без прямого доступа к storage и брокеру.
///
/// Такой интерфейс оставляет сетевые операции engine-у, поэтому стратегию можно
/// вычислять детерминированно и откатывать при ошибке сохранения.
pub trait PortfolioStrategy: Send + Sync {
    /// Возвращает уникальный ID экземпляра стратегии.
    fn id(&self) -> &StrategyId;
    /// Возвращает выделенный Sandbox-счёт экземпляра.
    fn account_id(&self) -> &AccountId;
    /// Возвращает стабильный строковый `kind`, используемый в config/storage.
    fn kind(&self) -> &'static str;
    /// Сериализует эффективную конфигурацию для аудита.
    fn config(&self) -> Result<Value>;
    /// Объявляет требуемые ряды свечей и объём warmup.
    fn data_requirements(&self) -> Vec<DataRequirement>;
    /// Объявляет необходимые таймеры относительно сессии MOEX.
    fn schedule_requirements(&self) -> Vec<ScheduleRequirement>;
    /// Сериализует внутреннее состояние после последнего события.
    fn state(&self) -> Result<Value>;
    /// Восстанавливает сохранённое состояние или выполняет rollback in-memory.
    fn restore_state(&mut self, state: Value) -> Result<()>;
    /// Применяет событие и при необходимости возвращает полный target.
    async fn on_event(&mut self, event: &StrategyEvent) -> Result<Option<TargetPortfolio>>;
}

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
    ///     sizing_capital * signed_weight / (price * lot_size)
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
            // API принимает лоты, поэтому цена одной бумаги умножается на lot size.
            let lot_value = price.0 * Decimal::from(info.lot_size);
            // Decimal::trunc округляет и long, и short к нулю.
            let desired_lots = (target_value / lot_value).trunc().to_i64().ok_or_else(|| {
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

/// Результат отправки или reconciliation одной ноги.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecutionOutcome {
    /// Storage обновлён свежим состоянием заявки брокера.
    Submitted(OrderUpdate),
    /// Операция уже была обработана другим worker-ом.
    AlreadyHandled,
    /// Новая отправка запрещена, потому что execution deadline уже прошёл.
    Suppressed,
}

/// Результат одной попытки провести весь batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BatchExecutionOutcome {
    /// Нужны новые broker updates или повторная сверка после restart.
    AwaitingFills,
    /// Все reduction и opening legs полностью исполнены.
    Completed,
    /// Продолжать batch небезопасно либо одна из ног терминально завершилась.
    Failed,
}

/// Durable-исполнитель сохранённых двухфазных batch-ей.
///
/// Executor никогда не принимает торговое решение сам. Он загружает уже
/// сохранённые ноги, выполняет все `Reduction`, заново читает портфель и только
/// после сверки разрешает `Opening`. Стабильный `request_id` позволяет безопасно
/// выяснить исход неоднозначного сетевого ответа после сбоя.
pub struct BatchExecutor<P, T, S> {
    account_id: AccountId,
    strategy_id: StrategyId,
    portfolio: Arc<P>,
    trading: Arc<T>,
    storage: Arc<S>,
}

impl<P, T, S> BatchExecutor<P, T, S>
where
    P: Portfolio,
    T: Trading,
    S: Storage,
{
    #[must_use]
    /// Собирает executor для единственной пары счёт/стратегия.
    ///
    /// Явная привязка владельца является частью safety boundary: даже если в
    /// общей БД лежат batch-и других процессов, этот executor не имеет права
    /// менять их статусы или отправлять их ноги брокеру.
    pub fn new(
        account_id: AccountId,
        strategy_id: StrategyId,
        portfolio: Arc<P>,
        trading: Arc<T>,
        storage: Arc<S>,
    ) -> Self {
        Self {
            account_id,
            strategy_id,
            portfolio,
            trading,
            storage,
        }
    }

    /// Продолжает normal-исполнение одного сохранённого batch.
    pub async fn execute_batch(&self, batch_id: Uuid) -> Result<BatchExecutionOutcome> {
        self.execute_batch_mode(batch_id, true).await
    }

    /// Восстанавливает сокращения, но не отправляет новые opening legs.
    ///
    /// Используется после risk-lock: ещё не отправленные открытия отклоняются,
    /// а потенциально отправленные только сверяются с брокером до терминального
    /// статуса. Неопределённую заявку нельзя объявлять Failed и забывать.
    pub async fn execute_batch_close_only(&self, batch_id: Uuid) -> Result<BatchExecutionOutcome> {
        self.execute_batch_mode(batch_id, false).await
    }

    async fn execute_batch_mode(
        &self,
        batch_id: Uuid,
        allow_openings: bool,
    ) -> Result<BatchExecutionOutcome> {
        let batch =
            self.storage.load_batch(batch_id).await?.ok_or_else(|| {
                Error::Storage(format!("rebalance batch `{batch_id}` was not found"))
            })?;
        if batch.account_id != self.account_id || batch.strategy_id != self.strategy_id {
            return Err(Error::RiskRejected(format!(
                "batch `{batch_id}` belongs to account `{}` and strategy `{}`, not this executor",
                batch.account_id, batch.strategy_id
            )));
        }
        let mut intents = self.storage.batch_intents(batch_id).await?;
        if !matches!(
            batch.status,
            RebalanceBatchStatus::Completed | RebalanceBatchStatus::Failed
        ) {
            match self.storage.batch_execution_gate(batch_id).await? {
                BatchExecutionGate::Ready => {}
                BatchExecutionGate::Hold => return Ok(BatchExecutionOutcome::AwaitingFills),
                BatchExecutionGate::Cancel => {
                    return self
                        .settle_failed_batch(
                            batch_id,
                            &intents,
                            "operator control cancelled execution",
                        )
                        .await;
                }
            }
        }
        if intents.iter().any(|stored| {
            stored.intent.batch_id != batch.id
                || stored.intent.account_id != batch.account_id
                || stored.intent.strategy_id != batch.strategy_id
                || stored.intent.event_id != batch.event_id
        }) {
            return Err(Error::Storage(format!(
                "batch `{batch_id}` contains an intent with mismatched ownership"
            )));
        }

        // Терминальный отказ reduction означает, что даже безопасная часть
        // исходного target недостижима. Он проверяется до reconciliation, чтобы
        // другая active reduction уже не получила разрешение на resubmit.
        if phase_has_terminal_failure(&intents, ExecutionPhase::Reduction) {
            return self
                .settle_failed_batch(
                    batch_id,
                    &intents,
                    "a reduction leg is rejected or cancelled",
                )
                .await;
        }

        // После crash reduction могла уже уйти брокеру. Сначала сверяем ровно
        // одну такую ногу. Остальные active/pending в этом проходе не трогаются:
        // если текущая стала terminal, дальнейшие отправки сразу запрещаются.
        if let Some(stored) = intents.iter().find(|stored| {
            stored.intent.execution_phase == ExecutionPhase::Reduction
                && matches!(
                    stored.status,
                    IntentStatus::Submitting | IntentStatus::Accepted | IntentStatus::Uncertain
                )
        }) {
            self.submit_or_reconcile(&stored.intent, true, None).await?;
            intents = self.storage.batch_intents(batch_id).await?;
            if phase_has_terminal_failure(&intents, ExecutionPhase::Reduction) {
                return self
                    .settle_failed_batch(
                        batch_id,
                        &intents,
                        "a reduction leg is rejected or cancelled",
                    )
                    .await;
            }
            if !phase_is_filled(&intents, ExecutionPhase::Reduction) {
                self.storage
                    .set_batch_status(batch_id, RebalanceBatchStatus::AwaitingFills, None)
                    .await?;
                return Ok(BatchExecutionOutcome::AwaitingFills);
            }
        }
        if !phase_is_filled(&intents, ExecutionPhase::Reduction) {
            // Pending reduction тоже нельзя слепо отправить после restart:
            // позицию могли вручную изменить между plan и recovery. target_lots
            // позволяет восстановить и проверить source каждой Pending ноги.
            let refreshed = self.portfolio.portfolio(&batch.account_id).await?.data;
            if refreshed.account_id != batch.account_id
                || refreshed.positions.iter().any(|position| position.blocked)
            {
                self.storage
                    .set_batch_status(
                        batch_id,
                        RebalanceBatchStatus::AwaitingFills,
                        Some("fresh portfolio is unavailable or contains blocked positions"),
                    )
                    .await?;
                return Ok(BatchExecutionOutcome::AwaitingFills);
            }
            if !pending_sources_match(&refreshed, &intents, ExecutionPhase::Reduction)? {
                return self
                    .settle_failed_batch(
                        batch_id,
                        &intents,
                        "fresh positions do not match the pending reduction sources",
                    )
                    .await;
            }
        }
        self.storage
            .set_batch_status(batch_id, RebalanceBatchStatus::Reducing, None)
            .await?;
        if !self
            .execute_phase(&intents, ExecutionPhase::Reduction, None)
            .await?
        {
            intents = self.storage.batch_intents(batch_id).await?;
            if phase_has_terminal_failure(&intents, ExecutionPhase::Reduction) {
                return self
                    .settle_failed_batch(
                        batch_id,
                        &intents,
                        "a reduction leg is rejected or cancelled",
                    )
                    .await;
            }
            self.storage
                .set_batch_status(batch_id, RebalanceBatchStatus::AwaitingFills, None)
                .await?;
            return Ok(BatchExecutionOutcome::AwaitingFills);
        }

        intents = self.storage.batch_intents(batch_id).await?;
        // Решение о переходе к openings принимается по свежему durable state, а
        // не только по ответам текущего worker-а.
        if phase_has_terminal_failure(&intents, ExecutionPhase::Reduction) {
            return self
                .settle_failed_batch(
                    batch_id,
                    &intents,
                    "a reduction leg is rejected or cancelled",
                )
                .await;
        }
        if !phase_is_filled(&intents, ExecutionPhase::Reduction) {
            self.storage
                .set_batch_status(batch_id, RebalanceBatchStatus::AwaitingFills, None)
                .await?;
            return Ok(BatchExecutionOutcome::AwaitingFills);
        }
        let opening_legs = intents
            .iter()
            .filter(|stored| stored.intent.execution_phase == ExecutionPhase::Opening)
            .collect::<Vec<_>>();
        if opening_legs.is_empty() {
            self.storage
                .set_batch_status(batch_id, RebalanceBatchStatus::Completed, None)
                .await?;
            return Ok(BatchExecutionOutcome::Completed);
        }

        // `valid_until` — крайний срок *новой отправки*. Просроченный target не
        // отменяет reductions и не позволяет забыть заявку, которая могла уже
        // попасть брокеру: такие opening legs только reconciliate-ятся.
        let opening_suppression = if !allow_openings {
            Some("opening phase suppressed by close-only risk mode")
        } else if Utc::now() > batch.target.valid_until {
            Some("opening phase suppressed because target has expired")
        } else {
            None
        };
        if let Some(reason) = opening_suppression {
            return self
                .settle_suppressed_openings(batch_id, &intents, reason)
                .await;
        }

        // Если полный target уже невозможен, ни одна active нога ниже не должна
        // получить разрешение на resubmit после broker NotFound.
        if phase_has_terminal_failure(&intents, ExecutionPhase::Opening) {
            return self
                .settle_failed_batch(
                    batch_id,
                    &intents,
                    "an opening leg is rejected or cancelled",
                )
                .await;
        }

        // Если до restart хотя бы одна opening leg уже могла уйти брокеру,
        // сначала только выясняем её исход. Оставшиеся Pending legs в этом же
        // проходе не отправляются: следующий проход заново прочитает портфель.
        if let Some(stored) = opening_legs.iter().find(|stored| {
            matches!(
                stored.status,
                IntentStatus::Submitting | IntentStatus::Accepted | IntentStatus::Uncertain
            )
        }) {
            self.submit_or_reconcile(&stored.intent, true, Some(batch.target.valid_until))
                .await?;
            intents = self.storage.batch_intents(batch_id).await?;
            // Проверяется результат каждой отдельной reconciliation. Если она
            // стала terminal, остальные legs обрабатываются только no-resubmit.
            if phase_has_terminal_failure(&intents, ExecutionPhase::Opening) {
                return self
                    .settle_failed_batch(
                        batch_id,
                        &intents,
                        "an opening leg is rejected or cancelled",
                    )
                    .await;
            }
            return self.finish_or_wait_openings(batch_id, &intents).await;
        }
        if phase_is_filled(&intents, ExecutionPhase::Opening) {
            self.storage
                .set_batch_status(batch_id, RebalanceBatchStatus::Completed, None)
                .await?;
            return Ok(BatchExecutionOutcome::Completed);
        }

        // Нельзя рассчитывать openings из снимка до reductions. Только теперь,
        // когда reductions подтверждены и нет неопределённой opening-заявки,
        // повторно читаем фактические позиции и сверяем source каждой Pending leg.
        let refreshed = self.portfolio.portfolio(&batch.account_id).await?.data;
        if refreshed.account_id != batch.account_id
            || refreshed.positions.iter().any(|position| position.blocked)
        {
            self.storage
                .set_batch_status(
                    batch_id,
                    RebalanceBatchStatus::AwaitingFills,
                    Some("fresh portfolio is unavailable or contains blocked positions"),
                )
                .await?;
            return Ok(BatchExecutionOutcome::AwaitingFills);
        }
        if !pending_sources_match(&refreshed, &intents, ExecutionPhase::Opening)? {
            return self
                .settle_failed_batch(
                    batch_id,
                    &intents,
                    "fresh positions do not match the pending opening sources",
                )
                .await;
        }
        self.storage
            .set_batch_status(batch_id, RebalanceBatchStatus::Opening, None)
            .await?;
        if !self
            .execute_phase(
                &intents,
                ExecutionPhase::Opening,
                Some(batch.target.valid_until),
            )
            .await?
        {
            intents = self.storage.batch_intents(batch_id).await?;
            if phase_has_terminal_failure(&intents, ExecutionPhase::Opening) {
                return self
                    .settle_failed_batch(
                        batch_id,
                        &intents,
                        "an opening leg is rejected or cancelled",
                    )
                    .await;
            }
            self.storage
                .set_batch_status(batch_id, RebalanceBatchStatus::AwaitingFills, None)
                .await?;
            return Ok(BatchExecutionOutcome::AwaitingFills);
        }
        self.storage
            .set_batch_status(batch_id, RebalanceBatchStatus::Completed, None)
            .await?;
        Ok(BatchExecutionOutcome::Completed)
    }

    /// Продолжает все незавершённые batch-и после запуска процесса.
    pub async fn recover_pending_batches(&self) -> Result<Vec<(Uuid, BatchExecutionOutcome)>> {
        self.recover_pending_batches_mode(true).await
    }

    /// Восстанавливает durable batch-и, не разрешая новые открытия.
    pub async fn recover_pending_batches_close_only(
        &self,
    ) -> Result<Vec<(Uuid, BatchExecutionOutcome)>> {
        self.recover_pending_batches_mode(false).await
    }

    async fn recover_pending_batches_mode(
        &self,
        allow_openings: bool,
    ) -> Result<Vec<(Uuid, BatchExecutionOutcome)>> {
        let mut outcomes = Vec::new();
        for batch_id in self
            .storage
            .pending_batch_ids(&self.account_id, &self.strategy_id)
            .await?
        {
            outcomes.push((
                batch_id,
                self.execute_batch_mode(batch_id, allow_openings).await?,
            ));
        }
        Ok(outcomes)
    }

    /// Завершает уже возможные broker openings без права послать новую заявку.
    async fn settle_suppressed_openings(
        &self,
        batch_id: Uuid,
        intents: &[StoredIntent],
        reason: &str,
    ) -> Result<BatchExecutionOutcome> {
        for stored in intents
            .iter()
            .filter(|stored| stored.intent.execution_phase == ExecutionPhase::Opening)
        {
            match stored.status {
                IntentStatus::Pending => {
                    // Отклоняем только действительно не захваченную ногу. Если
                    // конкурент уже сделал Pending→Submitting, свежая загрузка
                    // ниже увидит active status и оставит batch recoverable.
                    self.storage
                        .reject_pending_intent(stored.intent.id, reason)
                        .await?;
                }
                IntentStatus::Submitting | IntentStatus::Accepted | IntentStatus::Uncertain => {
                    self.reconcile_without_resubmission(&stored.intent, reason)
                        .await?;
                }
                IntentStatus::Filled | IntentStatus::Cancelled | IntentStatus::Rejected => {}
            }
        }
        let refreshed = self.storage.batch_intents(batch_id).await?;
        if phase_has_active(&refreshed, ExecutionPhase::Opening) {
            self.storage
                .set_batch_status(batch_id, RebalanceBatchStatus::AwaitingFills, Some(reason))
                .await?;
            return Ok(BatchExecutionOutcome::AwaitingFills);
        }
        if phase_is_filled(&refreshed, ExecutionPhase::Opening) {
            self.storage
                .set_batch_status(batch_id, RebalanceBatchStatus::Completed, None)
                .await?;
            return Ok(BatchExecutionOutcome::Completed);
        }
        self.storage
            .set_batch_status(batch_id, RebalanceBatchStatus::Failed, Some(reason))
            .await?;
        Ok(BatchExecutionOutcome::Failed)
    }

    /// Классифицирует opening phase после reconciliation без новых отправок.
    async fn finish_or_wait_openings(
        &self,
        batch_id: Uuid,
        intents: &[StoredIntent],
    ) -> Result<BatchExecutionOutcome> {
        if phase_is_filled(intents, ExecutionPhase::Opening) {
            self.storage
                .set_batch_status(batch_id, RebalanceBatchStatus::Completed, None)
                .await?;
            return Ok(BatchExecutionOutcome::Completed);
        }
        if phase_has_terminal_failure(intents, ExecutionPhase::Opening) {
            return self
                .settle_failed_batch(batch_id, intents, "an opening leg is rejected or cancelled")
                .await;
        }
        self.storage
            .set_batch_status(batch_id, RebalanceBatchStatus::AwaitingFills, None)
            .await?;
        Ok(BatchExecutionOutcome::AwaitingFills)
    }

    /// Доводит невозможный полный target до безопасного terminal-состояния.
    ///
    /// Наличие одной rejected/cancelled ноги ещё не разрешает забыть остальные:
    /// Pending отклоняются локально, а заявки, которые могли попасть брокеру,
    /// только reconciliate-ятся. Batch остаётся recoverable, пока хотя бы одна
    /// такая заявка не получила terminal status.
    async fn settle_failed_batch(
        &self,
        batch_id: Uuid,
        intents: &[StoredIntent],
        reason: &str,
    ) -> Result<BatchExecutionOutcome> {
        for stored in intents {
            match stored.status {
                IntentStatus::Pending => {
                    // CAS не позволяет локальному settle отменить заявку после
                    // того, как другой worker получил право на broker RPC.
                    self.storage
                        .reject_pending_intent(stored.intent.id, reason)
                        .await?;
                }
                IntentStatus::Submitting | IntentStatus::Accepted | IntentStatus::Uncertain => {
                    self.reconcile_without_resubmission(&stored.intent, reason)
                        .await?;
                }
                IntentStatus::Filled | IntentStatus::Cancelled | IntentStatus::Rejected => {}
            }
        }
        let refreshed = self.storage.batch_intents(batch_id).await?;
        if intents_have_active_legs(&refreshed) {
            self.storage
                .set_batch_status(batch_id, RebalanceBatchStatus::AwaitingFills, Some(reason))
                .await?;
            return Ok(BatchExecutionOutcome::AwaitingFills);
        }
        self.storage
            .set_batch_status(batch_id, RebalanceBatchStatus::Failed, Some(reason))
            .await?;
        Ok(BatchExecutionOutcome::Failed)
    }

    async fn execute_phase(
        &self,
        intents: &[StoredIntent],
        phase: ExecutionPhase,
        submission_deadline: Option<DateTime<Utc>>,
    ) -> Result<bool> {
        for stored in intents
            .iter()
            .filter(|stored| stored.intent.execution_phase == phase)
        {
            match stored.status {
                IntentStatus::Filled => {}
                IntentStatus::Cancelled | IntentStatus::Rejected => return Ok(false),
                IntentStatus::Pending => {
                    // Deadline проверяется непосредственно перед CAS/сетевым
                    // вызовом, а не только при загрузке batch: portfolio refresh
                    // сам может занять оставшееся время жизни target.
                    if submission_deadline.is_some_and(|deadline| Utc::now() > deadline) {
                        self.storage
                            .reject_pending_intent(
                                stored.intent.id,
                                "opening target expired before broker submission",
                            )
                            .await?;
                        return Ok(false);
                    }
                    // CAS Pending→Submitting даёт право отправки только одному
                    // worker-у и закрывает окно двойной заявки.
                    if !self.storage.claim_intent(stored.intent.id).await? {
                        return Ok(false);
                    }
                    // CAS сам является await и мог завершиться уже после
                    // deadline. В этот момент RPC ещё точно не начинался, поэтому
                    // захваченный Submitting можно безопасно сделать Rejected.
                    if submission_deadline.is_some_and(|deadline| Utc::now() > deadline) {
                        self.storage
                            .reject_intent(
                                stored.intent.id,
                                "opening target expired while claiming broker submission",
                            )
                            .await?;
                        return Ok(false);
                    }
                    let update = self
                        .submit_or_reconcile(&stored.intent, false, submission_deadline)
                        .await?;
                    if !matches!(
                        update,
                        ExecutionOutcome::Submitted(OrderUpdate {
                            status: OrderStatus::Filled,
                            ..
                        })
                    ) {
                        return Ok(false);
                    }
                }
                IntentStatus::Submitting | IntentStatus::Accepted | IntentStatus::Uncertain => {
                    // После crash неизвестно, дошёл ли предыдущий запрос. Сначала
                    // запрашиваем брокера по тому же request_id, не отправляем заново.
                    let update = self
                        .submit_or_reconcile(&stored.intent, true, submission_deadline)
                        .await?;
                    if !matches!(
                        update,
                        ExecutionOutcome::Submitted(OrderUpdate {
                            status: OrderStatus::Filled,
                            ..
                        })
                    ) {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    }

    async fn submit_or_reconcile(
        &self,
        intent: &OrderIntent,
        reconcile_first: bool,
        resubmission_deadline: Option<DateTime<Utc>>,
    ) -> Result<ExecutionOutcome> {
        if reconcile_first {
            return self.reconcile(intent, None, resubmission_deadline).await;
        }
        match self.trading.place_order(intent).await {
            Ok(response) => {
                self.storage
                    .record_order_update(intent.id, &response.data)
                    .await?;
                Ok(ExecutionOutcome::Submitted(response.data))
            }
            Err(error) if ambiguous_order_error(&error) => {
                self.reconcile(intent, Some(error), resubmission_deadline)
                    .await
            }
            Err(error) => {
                self.storage
                    .reject_intent(intent.id, &error.to_string())
                    .await?;
                Err(error)
            }
        }
    }

    async fn reconcile(
        &self,
        intent: &OrderIntent,
        original: Option<Error>,
        resubmission_deadline: Option<DateTime<Utc>>,
    ) -> Result<ExecutionOutcome> {
        match self
            .trading
            .order_state(&intent.account_id, intent.request_id)
            .await
        {
            Ok(response) => {
                self.storage
                    .record_order_update(intent.id, &response.data)
                    .await?;
                Ok(ExecutionOutcome::Submitted(response.data))
            }
            Err(Error::Status(status)) if status.code() == Code::NotFound => {
                // Только подтверждённый NotFound означает, что request_id у
                // брокера отсутствует. Но перед повтором нужно ещё доказать, что
                // фактическая позиция не изменилась относительно source этой ноги.
                if resubmission_deadline.is_some_and(|deadline| Utc::now() > deadline) {
                    self.storage
                        .reject_intent(
                            intent.id,
                            "opening target expired before broker resubmission",
                        )
                        .await?;
                    return Ok(ExecutionOutcome::Suppressed);
                }
                let portfolio = self.portfolio.portfolio(&intent.account_id).await?.data;
                if portfolio.account_id != intent.account_id
                    || portfolio.positions.iter().any(|position| position.blocked)
                    || !intent_source_matches(&portfolio, intent)?
                {
                    self.storage
                        .reject_intent(
                            intent.id,
                            "broker request is absent, but the current position no longer matches the intent source",
                        )
                        .await?;
                    return Ok(ExecutionOutcome::Suppressed);
                }
                // Portfolio RPC тоже занимает время; opening deadline повторно
                // проверяется непосредственно перед mutation RPC.
                if resubmission_deadline.is_some_and(|deadline| Utc::now() > deadline) {
                    self.storage
                        .reject_intent(
                            intent.id,
                            "opening target expired while validating broker resubmission",
                        )
                        .await?;
                    return Ok(ExecutionOutcome::Suppressed);
                }
                if self.storage.batch_execution_gate(intent.batch_id).await?
                    != BatchExecutionGate::Ready
                {
                    self.storage
                        .reject_intent(intent.id, "operator control forbids resubmission")
                        .await?;
                    return Ok(ExecutionOutcome::Suppressed);
                }
                match self.trading.place_order(intent).await {
                    Ok(response) => {
                        self.storage
                            .record_order_update(intent.id, &response.data)
                            .await?;
                        Ok(ExecutionOutcome::Submitted(response.data))
                    }
                    Err(error) if ambiguous_order_error(&error) => {
                        self.storage
                            .mark_uncertain(intent.id, &error.to_string())
                            .await?;
                        Err(error)
                    }
                    Err(error) => {
                        // InvalidArgument/PermissionDenied и прочие однозначные
                        // ответы доказывают, что заявка не принята. Оставлять их
                        // Uncertain означало бы бесконечно повторять невалидный RPC.
                        self.storage
                            .reject_intent(intent.id, &error.to_string())
                            .await?;
                        Err(error)
                    }
                }
            }
            Err(error) => {
                let reason = original.map_or_else(
                    || error.to_string(),
                    |original| {
                        format!("submission failed: {original}; reconciliation failed: {error}")
                    },
                );
                self.storage.mark_uncertain(intent.id, &reason).await?;
                Err(error)
            }
        }
    }

    /// Сверяет потенциально отправленную заявку, но никогда не повторяет её.
    ///
    /// Это отличие критично для close-only и просроченного target: подтверждённый
    /// `NotFound` здесь означает безопасный локальный Reject, а не разрешение
    /// снова открыть позицию.
    async fn reconcile_without_resubmission(
        &self,
        intent: &OrderIntent,
        suppression_reason: &str,
    ) -> Result<()> {
        match self
            .trading
            .order_state(&intent.account_id, intent.request_id)
            .await
        {
            Ok(response) => {
                self.storage
                    .record_order_update(intent.id, &response.data)
                    .await?;
                Ok(())
            }
            Err(Error::Status(status)) if status.code() == Code::NotFound => {
                self.storage
                    .reject_intent(
                        intent.id,
                        &format!("{suppression_reason}; request_id is absent at broker"),
                    )
                    .await
            }
            Err(error) => {
                self.storage
                    .mark_uncertain(
                        intent.id,
                        &format!("{suppression_reason}; reconciliation failed: {error}"),
                    )
                    .await?;
                Err(error)
            }
        }
    }
}

fn phase_has_terminal_failure(intents: &[StoredIntent], phase: ExecutionPhase) -> bool {
    intents.iter().any(|stored| {
        stored.intent.execution_phase == phase
            && matches!(
                stored.status,
                IntentStatus::Cancelled | IntentStatus::Rejected
            )
    })
}

fn phase_has_active(intents: &[StoredIntent], phase: ExecutionPhase) -> bool {
    intents.iter().any(|stored| {
        stored.intent.execution_phase == phase
            && matches!(
                stored.status,
                IntentStatus::Pending
                    | IntentStatus::Submitting
                    | IntentStatus::Accepted
                    | IntentStatus::Uncertain
            )
    })
}

fn intents_have_active_legs(intents: &[StoredIntent]) -> bool {
    intents.iter().any(|stored| {
        matches!(
            stored.status,
            IntentStatus::Pending
                | IntentStatus::Submitting
                | IntentStatus::Accepted
                | IntentStatus::Uncertain
        )
    })
}

fn phase_is_filled(intents: &[StoredIntent], phase: ExecutionPhase) -> bool {
    intents
        .iter()
        .filter(|stored| stored.intent.execution_phase == phase)
        .all(|stored| stored.status == IntentStatus::Filled)
}

fn pending_sources_match(
    portfolio: &PortfolioSnapshot,
    intents: &[StoredIntent],
    phase: ExecutionPhase,
) -> Result<bool> {
    let pending_legs = intents
        .iter()
        .filter(|stored| stored.intent.execution_phase == phase)
        .collect::<Vec<_>>();
    // Filled legs уже закономерно изменили позицию. Сверяются только ещё не
    // отправленные ноги; active legs до этой функции не допускаются executor-ом.
    for stored in pending_legs
        .into_iter()
        .filter(|stored| stored.status == IntentStatus::Pending)
    {
        if !intent_source_matches(portfolio, &stored.intent)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Сверяет фактическое количество бумаг с позицией, от которой была построена leg.
///
/// Intent хранит конечный `target_lots` и положительный размер delta. Поэтому
/// исходная позиция восстанавливается однозначно: для buy это `target - lots`,
/// для sell — `target + lots`. Проверка используется и для Pending legs, и перед
/// resubmit ранее захваченной, но отсутствующей у брокера заявки.
fn intent_source_matches(portfolio: &PortfolioSnapshot, intent: &OrderIntent) -> Result<bool> {
    let source_lots = match intent.side {
        OrderSide::Buy => intent.target_lots.checked_sub(intent.lots),
        OrderSide::Sell => intent.target_lots.checked_add(intent.lots),
    }
    .ok_or_else(|| Error::RiskRejected("order source lots overflow".to_owned()))?;
    let expected_quantity = Decimal::from(source_lots) * Decimal::from(intent.lot_size);
    let actual_quantity = portfolio
        .positions
        .iter()
        .filter(|position| position.instrument_id == intent.instrument_id)
        .map(|position| position.quantity)
        .sum::<Decimal>();
    Ok(actual_quantity == expected_quantity)
}

fn ambiguous_order_error(error: &Error) -> bool {
    // Эти ошибки не доказывают, что mutation не дошла до брокера. Повторная
    // отправка без reconciliation могла бы создать дублирующую заявку.
    matches!(
        error,
        Error::Status(status)
            if matches!(
                status.code(),
                Code::Cancelled
                    | Code::Unknown
                    | Code::DeadlineExceeded
                    | Code::Internal
                    | Code::Unavailable
            )
    ) || matches!(error, Error::Timeout(_))
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
        assert!(
            storage
                .pending_batch_ids(&account, &id)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
