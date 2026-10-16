//! Брокер-независимые типы предметной области.
//!
//! Это граница между protobuf T-Invest и бизнес-логикой. Поля API сначала
//! преобразуются в типы этого модуля, после чего стратегия, storage и executor
//! не обязаны знать устройство сгенерированных контрактов.
//!
//! Идентификаторы представлены отдельными newtype-типами: компилятор не даст
//! случайно передать `AccountId` вместо `InstrumentId`, хотя внутри оба содержат
//! строку.

use std::{fmt, str::FromStr};

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Error, Result, proto};

macro_rules! string_id {
    ($name:ident) => {
        #[doc = concat!("Непустой строковый идентификатор `", stringify!($name), "`.")]
        #[doc = "\n\nКонструктор проверяет только непустое значение; формат и существование ID проверяет API."]
        #[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Создаёт идентификатор и отклоняет пустую строку.
            pub fn new(value: impl Into<String>) -> Result<Self> {
                let value = value.into();
                if value.trim().is_empty() {
                    return Err(Error::InvalidArgument(
                        concat!(stringify!($name), " cannot be empty").to_owned(),
                    ));
                }
                Ok(Self(value))
            }

            /// Возвращает строковое представление без копирования.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Извлекает владеющую строку, потребляя идентификатор.
            #[must_use]
            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = Error;

            fn from_str(value: &str) -> Result<Self> {
                Self::new(value)
            }
        }
    };
}

string_id!(InstrumentId);
string_id!(AccountId);
string_id!(StrategyId);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
/// Клиентский UUID запроса на выставление заявки.
///
/// Этот ID служит ключом идемпотентности: при reconciliation повторная попытка
/// обязана использовать то же значение, иначе можно создать вторую заявку.
pub struct OrderRequestId(Uuid);

impl OrderRequestId {
    /// Генерирует новый случайный UUID v4.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Оборачивает уже существующий UUID без генерации нового.
    #[must_use]
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Возвращает внутренний UUID.
    #[must_use]
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for OrderRequestId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for OrderRequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for OrderRequestId {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        Uuid::parse_str(value)
            .map(Self)
            .map_err(|error| Error::InvalidArgument(format!("invalid order request id: {error}")))
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
/// Точное десятичное значение цены или количества.
///
/// В финансовом коде не используется `f64`, чтобы избежать ошибок двоичного
/// округления. При сериализации значение записывается строкой.
pub struct Price(#[serde(with = "rust_decimal::serde::str")] pub Decimal);

impl Price {
    /// Проверяет положительную цену сделки и её возраст перед sandbox-исполнением.
    /// Допускается до 30 секунд рассогласования часов; отсутствующее время запрещено.
    pub fn from_recent_trade(value: &proto::LastPrice, now: DateTime<Utc>) -> Result<Self> {
        let observed = value.time.ok_or_else(|| {
            Error::RiskRejected("execution quote timestamp is missing".to_owned())
        })?;
        let age = now - timestamp_to_datetime(observed)?;
        if age > chrono::Duration::minutes(5) || age < -chrono::Duration::seconds(30) {
            return Err(Error::RiskRejected(
                "execution quote is stale or future-dated".to_owned(),
            ));
        }
        let price =
            Self::from_quotation(value.price.ok_or_else(|| {
                Error::RiskRejected("execution quote price is missing".to_owned())
            })?)?;
        if price.0 <= Decimal::ZERO {
            return Err(Error::RiskRejected(
                "execution quote must be positive".to_owned(),
            ));
        }
        Ok(price)
    }

    /// Оборачивает точное [`Decimal`] значение.
    #[must_use]
    pub const fn new(value: Decimal) -> Self {
        Self(value)
    }

    /// Преобразует protobuf `Quotation { units, nano }` в точное значение.
    pub fn from_quotation(value: proto::Quotation) -> Result<Self> {
        let total = i128::from(value.units)
            .checked_mul(1_000_000_000)
            .and_then(|units| units.checked_add(i128::from(value.nano)))
            .ok_or(Error::InvalidDecimal {
                units: value.units,
                nano: value.nano,
            })?;
        Ok(Self(Decimal::from_i128_with_scale(total, 9)))
    }

    /// Преобразует значение обратно в protobuf с точностью до девяти знаков.
    pub fn to_quotation(self) -> Result<proto::Quotation> {
        let mut value = self.0.round_dp(9);
        value.rescale(9);
        let total = value.mantissa();
        let units = total / 1_000_000_000;
        let nano = total % 1_000_000_000;
        Ok(proto::Quotation {
            units: i64::try_from(units)
                .map_err(|_| Error::InvalidArgument("price is too large".to_owned()))?,
            nano: i32::try_from(nano)
                .map_err(|_| Error::InvalidArgument("price nanos are out of range".to_owned()))?,
        })
    }
}

impl fmt::Display for Price {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
/// Денежная сумма вместе с кодом валюты, например `rub`.
pub struct Money {
    /// Строковый код валюты из API.
    pub currency: String,
    /// Точная сумма в указанной валюте.
    pub amount: Price,
}

impl Money {
    /// Преобразует protobuf `MoneyValue` в доменный тип.
    pub fn from_proto(value: proto::MoneyValue) -> Result<Self> {
        Ok(Self {
            currency: value.currency,
            amount: Price::from_quotation(proto::Quotation {
                units: value.units,
                nano: value.nano,
            })?,
        })
    }

    /// Преобразует доменную сумму в protobuf `MoneyValue`.
    pub fn to_proto(&self) -> Result<proto::MoneyValue> {
        let quotation = self.amount.to_quotation()?;
        Ok(proto::MoneyValue {
            currency: self.currency.clone(),
            units: quotation.units,
            nano: quotation.nano,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
/// Интервал исторической или потоковой свечи.
pub enum CandleInterval {
    OneMinute,
    FiveMinutes,
    FifteenMinutes,
    OneHour,
    OneDay,
    TwoMinutes,
    ThreeMinutes,
    TenMinutes,
    ThirtyMinutes,
    TwoHours,
    FourHours,
    OneWeek,
    OneMonth,
    FiveSeconds,
    TenSeconds,
    ThirtySeconds,
}

impl CandleInterval {
    /// Возвращает enum исторического unary-запроса `GetCandles`.
    #[must_use]
    pub const fn as_proto(self) -> proto::CandleInterval {
        match self {
            Self::OneMinute => proto::CandleInterval::CandleInterval1Min,
            Self::FiveMinutes => proto::CandleInterval::CandleInterval5Min,
            Self::FifteenMinutes => proto::CandleInterval::CandleInterval15Min,
            Self::OneHour => proto::CandleInterval::Hour,
            Self::OneDay => proto::CandleInterval::Day,
            Self::TwoMinutes => proto::CandleInterval::CandleInterval2Min,
            Self::ThreeMinutes => proto::CandleInterval::CandleInterval3Min,
            Self::TenMinutes => proto::CandleInterval::CandleInterval10Min,
            Self::ThirtyMinutes => proto::CandleInterval::CandleInterval30Min,
            Self::TwoHours => proto::CandleInterval::CandleInterval2Hour,
            Self::FourHours => proto::CandleInterval::CandleInterval4Hour,
            Self::OneWeek => proto::CandleInterval::Week,
            Self::OneMonth => proto::CandleInterval::Month,
            Self::FiveSeconds => proto::CandleInterval::CandleInterval5Sec,
            Self::TenSeconds => proto::CandleInterval::CandleInterval10Sec,
            Self::ThirtySeconds => proto::CandleInterval::CandleInterval30Sec,
        }
    }

    /// Возвращает enum stream-подписки или `None`, если gRPC stream такой
    /// интервал не поддерживает.
    #[must_use]
    pub const fn as_subscription_proto(self) -> Option<proto::SubscriptionInterval> {
        match self {
            Self::OneMinute => Some(proto::SubscriptionInterval::OneMinute),
            Self::FiveMinutes => Some(proto::SubscriptionInterval::FiveMinutes),
            Self::FifteenMinutes => Some(proto::SubscriptionInterval::FifteenMinutes),
            Self::OneHour => Some(proto::SubscriptionInterval::OneHour),
            Self::OneDay => Some(proto::SubscriptionInterval::OneDay),
            Self::TwoMinutes => Some(proto::SubscriptionInterval::SubscriptionInterval2Min),
            Self::ThreeMinutes => Some(proto::SubscriptionInterval::SubscriptionInterval3Min),
            Self::TenMinutes => Some(proto::SubscriptionInterval::SubscriptionInterval10Min),
            Self::ThirtyMinutes => Some(proto::SubscriptionInterval::SubscriptionInterval30Min),
            Self::TwoHours => Some(proto::SubscriptionInterval::SubscriptionInterval2Hour),
            Self::FourHours => Some(proto::SubscriptionInterval::SubscriptionInterval4Hour),
            Self::OneWeek => Some(proto::SubscriptionInterval::Week),
            Self::OneMonth => Some(proto::SubscriptionInterval::Month),
            Self::FiveSeconds | Self::TenSeconds | Self::ThirtySeconds => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
/// OHLCV-свеча в едином формате приложения.
pub struct Candle {
    /// UID торгового инструмента.
    pub instrument_id: InstrumentId,
    /// Длительность свечи.
    pub interval: CandleInterval,
    /// Цена открытия.
    pub open: Price,
    /// Максимальная цена.
    pub high: Price,
    /// Минимальная цена.
    pub low: Price,
    /// Цена закрытия или текущее значение формирующейся свечи.
    pub close: Price,
    /// Объём в единицах, возвращённых API.
    pub volume: i64,
    /// Время начала свечи в UTC.
    pub time: DateTime<Utc>,
    /// `true`, если свеча окончательно закрыта и больше не должна измениться.
    pub complete: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
/// Нормализованное событие публичного market-data stream.
///
/// Это транспортный тип: готовый runtime превращает его в
/// [`crate::engine::StrategyEvent`], добавляя фазу warmup/live и стабильный ID.
pub enum MarketDataEvent {
    /// Закрытая либо промежуточная OHLCV-свеча.
    Candle(Candle),
    /// Heartbeat сервера, подтверждающий живое соединение.
    Heartbeat(DateTime<Utc>),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
/// Параметры unary-запроса исторических свечей.
pub struct CandleHistoryRequest {
    /// UID запрашиваемого инструмента.
    pub instrument_id: InstrumentId,
    /// Требуемый интервал свечей.
    pub interval: CandleInterval,
    /// Включаемая нижняя граница диапазона в UTC.
    pub from: DateTime<Utc>,
    /// Верхняя граница диапазона в UTC.
    pub to: DateTime<Utc>,
    /// Необязательный лимит, проверяемый helper-методом в диапазоне 1..=2500.
    pub limit: Option<i32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
/// Возможность выставлять разные виды заявок прямо сейчас.
pub struct TradingAvailability {
    /// Инструмент в целом доступен для торговли через API.
    pub api_trade_available: bool,
    /// Разрешены рыночные заявки.
    pub market_order_available: bool,
    /// Разрешены лимитные заявки.
    pub limit_order_available: bool,
    /// Разрешены заявки по лучшей цене.
    pub best_price_available: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
/// Краткая информация об одном брокерском или sandbox-счёте.
pub struct AccountSummary {
    /// Идентификатор, который передаётся в методы портфеля и заявок.
    pub id: AccountId,
    /// Пользовательское название счёта.
    pub name: String,
    /// Числовой enum статуса из protobuf-контракта.
    pub status: i32,
    /// Числовой enum уровня доступа текущего токена.
    pub access_level: i32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
/// Одна позиция в портфеле.
pub struct PortfolioPosition {
    /// UID торгового инструмента.
    pub instrument_id: InstrumentId,
    /// Текущее количество инструмента, а не число заявок.
    pub quantity: Decimal,
    /// Текущая цена, если API смог её определить.
    pub current_price: Option<Money>,
    /// Признак блокировки позиции активной заявкой или иной операцией.
    pub blocked: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
/// Снимок портфеля, используемый в том числе risk-политикой.
pub struct PortfolioSnapshot {
    /// Счёт, которому принадлежит снимок.
    pub account_id: AccountId,
    /// Общая стоимость портфеля, если API вернул показатель.
    pub total: Option<Money>,
    /// Инвестиционные позиции счёта; рублёвый денежный остаток входит в `total`.
    pub positions: Vec<PortfolioPosition>,
    /// Локальное время получения и преобразования ответа.
    pub observed_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
/// Направление торговой заявки.
pub enum OrderSide {
    /// Покупка.
    Buy,
    /// Продажа.
    Sell,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
/// Способ определения цены заявки.
pub enum OrderKind {
    /// Рыночная заявка без заданной цены.
    Market,
    /// Лимитная заявка с обязательным `limit_price`.
    Limit,
    /// Заявка по лучшей доступной цене.
    BestPrice,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Фаза двухступенчатой ребалансировки портфеля.
pub enum ExecutionPhase {
    /// Закрытие либо уменьшение уже существующей позиции.
    Reduction,
    /// Открытие новой позиции либо увеличение существующей.
    Opening,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Явное согласие runtime на непокрытую маржинальную операцию.
pub enum MarginConsent {
    /// Заявка обязана быть покрыта существующей позицией или денежными средствами.
    CoveredOnly,
    /// После прохождения risk-check разрешена непокрытая заявка в Sandbox.
    UncoveredAllowed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
/// Нормализованный жизненный цикл заявки.
pub enum OrderStatus {
    /// Локальная заявка ещё не подтверждена API.
    Pending,
    /// Заявка принята, но пока не исполнена.
    New,
    /// Исполнена только часть запрошенных лотов.
    PartiallyFilled,
    /// Исполнена полностью.
    Filled,
    /// Отменена.
    Cancelled,
    /// Отклонена API или локальным risk-контролем.
    Rejected,
    /// API вернул неизвестный текущей версии enum.
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
/// Намерение стратегии совершить сделку, ещё не являющееся заявкой на бирже.
///
/// Разделение `OrderIntent` и сетевого вызова позволяет сначала надёжно
/// сохранить решение стратегии и UUID, затем проверить риск и только после
/// этого выполнить mutation. Intent существует только внутри batch: поля
/// `batch_id` и `leg_index` обязательны, а `target_lots` описывает ожидаемую
/// позицию после этой ноги. Это позволяет после restart сверить фактическую
/// позицию и не повторить неоднозначно завершившуюся операцию.
pub struct OrderIntent {
    /// Внутренний primary key намерения в storage.
    pub id: Uuid,
    /// Стабильный UUID сетевого запроса для идемпотентности и reconciliation.
    pub request_id: OrderRequestId,
    /// Durable batch, в составе которого исполняется заявка.
    pub batch_id: Uuid,
    /// Стабильный номер ноги внутри batch.
    pub leg_index: i32,
    /// ID рыночного события, породившего решение.
    pub event_id: String,
    /// Экземпляр стратегии-источника.
    pub strategy_id: StrategyId,
    /// Счёт, на котором предполагается исполнение.
    pub account_id: AccountId,
    /// UID инструмента.
    pub instrument_id: InstrumentId,
    /// Купить или продать.
    pub side: OrderSide,
    /// Рыночная, лимитная или best-price заявка.
    pub kind: OrderKind,
    /// Количество лотов; это не обязательно количество бумаг.
    pub lots: i64,
    /// Полное знаковое целевое количество лотов после исполнения ноги.
    pub target_lots: i64,
    /// Число бумаг в одном лоте на момент планирования batch.
    pub lot_size: i64,
    /// Сначала выполняются все reduction legs, затем opening legs.
    pub execution_phase: ExecutionPhase,
    /// Явное согласие на непокрытую маржинальную заявку.
    pub margin_consent: MarginConsent,
    /// Цена лимитной заявки; для market должна быть `None`.
    pub limit_price: Option<Price>,
    /// Время торгового сигнала, используемое проверкой его свежести.
    pub created_at: DateTime<Utc>,
}

impl OrderIntent {
    /// Создаёт batch-only рыночное намерение с новыми внутренним ID и request UUID.
    ///
    /// Метод ничего не отправляет брокеру. Реальная отправка происходит только
    /// через `engine::BatchExecutor` или явный вызов `Trading::place_order`.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn market(
        batch_id: Uuid,
        leg_index: i32,
        event_id: impl Into<String>,
        strategy_id: StrategyId,
        account_id: AccountId,
        instrument_id: InstrumentId,
        side: OrderSide,
        lots: i64,
        target_lots: i64,
        lot_size: i64,
        execution_phase: ExecutionPhase,
        margin_consent: MarginConsent,
        created_at: DateTime<Utc>,
    ) -> Self {
        // `request_id` генерируется ровно один раз вместе с durable intent.
        // Executor при retry обязан переиспользовать его, а не создавать новый.
        Self {
            id: Uuid::new_v4(),
            request_id: OrderRequestId::new(),
            batch_id,
            leg_index,
            event_id: event_id.into(),
            strategy_id,
            account_id,
            instrument_id,
            side,
            kind: OrderKind::Market,
            lots,
            target_lots,
            lot_size,
            execution_phase,
            margin_consent,
            limit_price: None,
            created_at,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
/// Текущее состояние заявки, нормализованное из unary или stream-ответа.
pub struct OrderUpdate {
    /// Client request UUID, связывающий обновление с `OrderIntent`.
    pub request_id: OrderRequestId,
    /// Идентификатор биржевой заявки, если он уже известен.
    pub exchange_order_id: Option<String>,
    /// Нормализованный статус.
    pub status: OrderStatus,
    /// Изначально запрошенное количество лотов.
    pub lots_requested: i64,
    /// Уже исполненное количество лотов.
    pub lots_executed: i64,
    /// Время наблюдения этого состояния.
    pub observed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
/// Отдельное фактическое исполнение части заявки.
pub struct ExecutionFill {
    /// Уникальный идентификатор сделки.
    pub trade_id: String,
    /// UUID исходного запроса.
    pub request_id: OrderRequestId,
    /// Цена исполнения.
    pub price: Price,
    /// Исполненное количество лотов.
    pub lots: i64,
    /// Время исполнения.
    pub executed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
/// Нормализованное сообщение потока брокерских операций.
pub struct OperationEvent {
    /// ID операции у брокера.
    pub id: String,
    /// Счёт операции.
    pub account_id: AccountId,
    /// Инструмент, если операция с ним связана.
    pub instrument_id: Option<InstrumentId>,
    /// Числовой protobuf enum типа операции.
    pub operation_type: i32,
    /// Числовой protobuf enum состояния операции.
    pub state: i32,
    /// Денежный результат операции, если присутствует.
    pub payment: Option<Money>,
    /// Время операции или локальное время получения при отсутствии timestamp.
    pub observed_at: DateTime<Utc>,
}

/// Проверяет protobuf timestamp и переводит его в UTC.
///
/// Отрицательные наносекунды и значения вне поддерживаемого chrono диапазона
/// отклоняются явно, чтобы не подменять повреждённое время локальным `now()`.
pub fn timestamp_to_datetime(value: prost_types::Timestamp) -> Result<DateTime<Utc>> {
    let nanos = u32::try_from(value.nanos).map_err(|_| Error::InvalidTimestamp {
        seconds: value.seconds,
        nanos: value.nanos,
    })?;
    DateTime::from_timestamp(value.seconds, nanos).ok_or(Error::InvalidTimestamp {
        seconds: value.seconds,
        nanos: value.nanos,
    })
}

#[must_use]
/// Переводит UTC-время в protobuf timestamp без потери наносекунд.
pub fn datetime_to_timestamp(value: DateTime<Utc>) -> prost_types::Timestamp {
    let nanos = i32::try_from(value.timestamp_subsec_nanos()).unwrap_or_default();
    prost_types::Timestamp {
        seconds: value.timestamp(),
        nanos,
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;

    use super::Price;
    use crate::proto::Quotation;

    #[test]
    fn quotation_round_trip_is_exact() {
        let price = Price::from_quotation(Quotation {
            units: -12,
            nano: -345_678_901,
        })
        .expect("valid quotation");

        assert_eq!(price.0, Decimal::new(-12_345_678_901, 9));
        assert_eq!(
            price.to_quotation().expect("representable price"),
            Quotation {
                units: -12,
                nano: -345_678_901,
            }
        );
    }
}
