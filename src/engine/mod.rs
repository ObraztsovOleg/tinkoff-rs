//! Portfolio-target runtime для одной внешней стратегии и одного выделенного
//! Sandbox-счёта. Внешней стратегией может быть взвешенный ансамбль внутренних
//! сигналов: engine всё равно получает один полный согласованный target.
//!
//! Стратегия здесь отвечает только за полный целевой портфель со знаковыми
//! весами. Перевод весов в лоты, проверка риска, надёжное сохранение решения и
//! сетевые изменения у брокера разделены на самостоятельные этапы. Каждая
//! заявка входит в неизменяемый batch ребалансировки: сначала должны завершиться
//! сокращения позиций, и только после этого разрешается открывать новые.

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    Result,
    domain::{AccountId, Candle, CandleInterval, InstrumentId, OrderUpdate, StrategyId},
};

mod executor;
mod planner;
mod risk;
mod runtime;

pub use executor::{
    BatchExecutionOutcome, BatchExecutor, ExecutionOutcome, SubmissionDecision, SubmissionGuard,
};
pub use planner::{
    PlannedBatch, PlanningContext, RebalanceBatch, RebalanceBatchStatus, TargetPlanner,
};
pub use risk::{
    AccountRiskState, EffectiveRiskConfig, RiskAnchor, RiskBlock, RiskBlockReason, RiskConfig,
    RiskDecision, RiskManager, RiskMode, RiskObservation,
};
pub use runtime::{EngineOutcome, PortfolioEngine};

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
    /// Справочные данные брокера: мультипликаторы и объявленные дивиденды.
    ///
    /// Runtime подаёт их перед открытием сессии в фазе Warmup: событие обновляет
    /// state стратегии, но само никогда не порождает заявку.
    Reference(ReferenceData),
    /// Решение внешней ИИ-модели для компоненты `ai-model`.
    ///
    /// Runtime получает его перед открытием сессии и подаёт в фазе Warmup: событие
    /// только сохраняется в state, а target появляется на live-открытии той же
    /// сессии и проходит обычные лимиты стратегии и risk manager.
    Advice(ModelAdvice),
}

/// Решение ИИ-модели: целевые веса внутри набора инструментов компоненты.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelAdvice {
    /// Момент получения ответа модели.
    pub observed_at: DateTime<Utc>,
    /// Торговая сессия, для которой принято решение.
    pub session_date: NaiveDate,
    /// Инструменты компоненты: решение принимает только компонента с тем же набором.
    pub universe: Vec<InstrumentId>,
    /// Профиль и модель, давшие решение, для аудита.
    pub model: String,
    /// Целевые доли капитала компоненты; отсутствующий инструмент — ноль.
    pub weights: Vec<AdviceWeight>,
    /// Обоснование решения от модели.
    #[serde(default)]
    pub rationale: String,
}

/// Целевая доля одного инструмента в решении модели (отрицательная — short).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AdviceWeight {
    /// Торгуемый UID.
    pub instrument_id: InstrumentId,
    /// Доля капитала компоненты, от -1 до 1.
    #[serde(with = "rust_decimal::serde::str")]
    pub weight: Decimal,
}

/// Внешние данные, которые стратегия не может вывести из свечей.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReferenceData {
    /// Момент получения ответа брокера.
    pub observed_at: DateTime<Utc>,
    /// Текущие мультипликаторы и показатели рентабельности (если запрошены).
    #[serde(default)]
    pub fundamentals: Vec<InstrumentFundamentals>,
    /// Объявленные и прошедшие дивиденды в окне запроса (если запрошены).
    #[serde(default)]
    pub dividends: Vec<DividendEvent>,
}

/// Текущий снимок показателей эмитента. `None` — показатель не опубликован.
///
/// T-Invest возвращает отсутствующее значение как ноль; runtime переводит такой
/// ноль в `None`, чтобы ранжирование не принимало пропуск за реальное значение.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstrumentFundamentals {
    /// Торгуемый UID, к которому относится показатель.
    pub instrument_id: InstrumentId,
    /// Цена к балансовой стоимости (P/B).
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub price_to_book: Option<Decimal>,
    /// Цена к прибыли за 12 месяцев (P/E).
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub price_to_earnings: Option<Decimal>,
    /// Стоимость компании к EBITDA (EV/EBITDA).
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub ev_to_ebitda: Option<Decimal>,
    /// Рентабельность капитала, %.
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub return_on_equity: Option<Decimal>,
    /// Рентабельность активов, %.
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub return_on_assets: Option<Decimal>,
    /// Чистая маржа, %.
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub net_margin: Option<Decimal>,
    /// Долг к собственному капиталу, %.
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub debt_to_equity: Option<Decimal>,
}

/// Дивиденд с известным последним днём покупки (T-1 перед датой фиксации).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DividendEvent {
    /// Торгуемый UID акции.
    pub instrument_id: InstrumentId,
    /// Последний день, когда покупка ещё даёт право на дивиденд.
    pub last_buy_date: NaiveDate,
    /// Размер выплаты на акцию, если известен.
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub dividend_net: Option<Decimal>,
}

/// Вид справочных данных, которые нужно загрузить для стратегии.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceRequirement {
    /// Мультипликаторы и рентабельность (`GetAssetFundamentals`).
    Fundamentals,
    /// Календарь дивидендов (`GetDividends`).
    Dividends,
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

/// Тип инструмента: от него зависит, сколько рублей стоит единица цены.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstrumentKind {
    /// Акция: цена — рубли за бумагу.
    #[default]
    Share,
    /// Биржевой фонд: цена — рубли за пай.
    Etf,
    /// Облигация: цена — процент от номинала, сверху платится НКД.
    Bond,
    /// Фьючерс: цена — пункты, у пункта своя рублёвая стоимость.
    Futures,
    /// Валюта: цена — рубли за единицу.
    Currency,
}

const fn one() -> Decimal {
    Decimal::ONE
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
    /// Тип инструмента.
    #[serde(default)]
    pub kind: InstrumentKind,
    /// Рублей за единицу цены одной бумаги: 1 у акций, фондов и валюты,
    /// номинал / 100 у облигаций, стоимость шага цены / шаг у фьючерсов.
    #[serde(default = "one", with = "rust_decimal::serde::str")]
    pub price_multiplier: Decimal,
    /// НКД одной облигации в рублях; у остальных типов ноль.
    #[serde(default, with = "rust_decimal::serde::str")]
    pub accrued_interest: Decimal,
    /// Комиссия брокера за сделку долей от её объёма (0.0005 = 0,05%).
    #[serde(default, with = "rust_decimal::serde::str")]
    pub commission_rate: Decimal,
}

impl InstrumentInfo {
    /// Рублёвая стоимость одной бумаги (одного контракта) по цене `price`.
    #[must_use]
    pub fn piece_value(&self, price: Decimal) -> Decimal {
        price * self.price_multiplier + self.accrued_interest
    }
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
    /// Объявляет внешние справочные данные; по умолчанию стратегии хватает свечей.
    fn reference_requirements(&self) -> Vec<ReferenceRequirement> {
        Vec::new()
    }
    /// Сериализует внутреннее состояние после последнего события.
    fn state(&self) -> Result<Value>;
    /// Восстанавливает сохранённое состояние или выполняет rollback in-memory.
    fn restore_state(&mut self, state: Value) -> Result<()>;
    /// Применяет событие и при необходимости возвращает полный target.
    async fn on_event(&mut self, event: &StrategyEvent) -> Result<Option<TargetPortfolio>>;
}
