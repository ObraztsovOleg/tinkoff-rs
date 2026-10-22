//! Торговый контур процесса: виртуальный Sandbox или реальный брокерский счёт.
//!
//! Runtime видит оба контура через [`VenueApi`] и [`VenueBroker`]. Реальный
//! контур — это `contour = "production"` в TOML и токен реального счёта в
//! `TINKOFF_TOKEN_PRODUCTION`. Аварийная остановка оператора (`live_settings.halted`)
//! запрещает такому процессу стартовать для торговли.
//!
//! Обе обёртки делят книгу непрерывных фьючерсов ([`SeriesBook`]): серия
//! `futures:BR` в запросах к брокеру становится привязанным контрактом, а в
//! ответах (позиции, свечи) контракт снова становится серией.
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use secrecy::SecretString;
use serde::Deserialize;
use tinkoff_rs::{
    ApiResponse, Error, ProductionApi, Result, SandboxApi,
    api::{InstrumentsApiClient, MarketDataApiClient},
    broker::{
        Accounts, LiveTradingPermit, MarketData, Portfolio, ProductionBroker, ProductionTrading,
        SandboxBroker, Trading,
    },
    domain::{
        AccountId, AccountSummary, Candle, CandleHistoryRequest, InstrumentId, OrderIntent,
        OrderKind, OrderRequestId, OrderUpdate, PortfolioSnapshot, Price, TradingAvailability,
    },
    proto::{
        AccessLevel, AccountStatus, CancelOrderRequest, GetLastPricesRequest,
        GetOperationsByCursorRequest, GetOperationsByCursorResponse, GetOrdersRequest, OrderState,
    },
    storage::PostgresStorage,
    stream::{MarketDataStream, MarketDataSubscription, StreamConfig},
};

use super::instruments::{SeriesBook, series_code};

/// Торговый контур из TOML. По умолчанию — Sandbox.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(super) enum Contour {
    #[default]
    Sandbox,
    /// Реальный счёт. Конфигурация обязана задать оба процентных стопа.
    Production,
}

impl Contour {
    /// Значение для логов и JSON: `sandbox` или `production`.
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Sandbox => "sandbox",
            Self::Production => "production",
        }
    }

    /// Название контура в сообщениях о готовности runtime.
    pub(super) const fn title(self) -> &'static str {
        match self {
            Self::Sandbox => "Sandbox",
            Self::Production => "Production",
        }
    }

    /// Переменная окружения с токеном этого контура.
    pub(super) const fn token_variable(self) -> &'static str {
        match self {
            Self::Sandbox => "TINKOFF_TOKEN",
            Self::Production => "TINKOFF_TOKEN_PRODUCTION",
        }
    }
}

/// Процесс, который может изменить реальный счёт, не стартует при аварийной
/// остановке оператора. Sandbox этой проверки не требует.
pub(super) async fn ensure_not_halted(storage: &PostgresStorage, contour: Contour) -> Result<()> {
    if contour == Contour::Sandbox {
        return Ok(());
    }
    let halted: bool = sqlx::query_scalar("SELECT halted FROM live_settings WHERE id")
        .fetch_optional(storage.pool())
        .await?
        .unwrap_or(false);
    if halted {
        Err(Error::LiveTradingDisabled)
    } else {
        Ok(())
    }
}

#[derive(Clone)]
enum ApiKind {
    Sandbox(SandboxApi),
    Production(ProductionApi),
}

/// Raw RPC выбранного контура, которых нет в broker-neutral traits. Запросы с
/// идентификаторами инструментов идут к контракту из [`Self::series`].
#[derive(Clone)]
pub(super) struct VenueApi {
    kind: ApiKind,
    series: Arc<SeriesBook>,
}

impl VenueApi {
    pub(super) const fn contour(&self) -> Contour {
        match self.kind {
            ApiKind::Sandbox(_) => Contour::Sandbox,
            ApiKind::Production(_) => Contour::Production,
        }
    }

    /// Непрерывные фьючерсы процесса.
    pub(super) fn series(&self) -> &SeriesBook {
        &self.series
    }

    pub(super) fn instruments(&self) -> InstrumentsApiClient {
        match &self.kind {
            ApiKind::Sandbox(api) => api.raw().instruments(),
            ApiKind::Production(api) => api.raw().instruments(),
        }
    }

    pub(super) fn market_data(&self) -> MarketDataApiClient {
        match &self.kind {
            ApiKind::Sandbox(api) => api.raw().market_data(),
            ApiKind::Production(api) => api.raw().market_data(),
        }
    }

    /// Активные заявки счёта у брокера (только чтение).
    pub(super) async fn active_orders(&self, account_id: &AccountId) -> Result<Vec<OrderState>> {
        let request = GetOrdersRequest {
            account_id: account_id.to_string(),
            ..Default::default()
        };
        let response = match &self.kind {
            ApiKind::Sandbox(api) => api.raw().sandbox().get_sandbox_orders(request).await?,
            ApiKind::Production(api) => api.raw().orders().get_orders(request).await?,
        };
        Ok(response.into_inner().orders)
    }

    /// Отменяет заявку по биржевому ID. На реальном счёте нужна та же
    /// капабилити, что и для выставления: отмена тоже меняет состояние счёта.
    pub(super) async fn cancel_order(
        &self,
        account_id: &AccountId,
        order_id: String,
    ) -> Result<()> {
        let request = CancelOrderRequest {
            account_id: account_id.to_string(),
            order_id,
            order_id_type: None,
        };
        match &self.kind {
            ApiKind::Sandbox(api) => {
                api.raw().sandbox().cancel_sandbox_order(request).await?;
            }
            ApiKind::Production(api) => {
                api.raw().orders().cancel_order(request).await?;
            }
        }
        Ok(())
    }

    /// Одна страница операций счёта.
    pub(super) async fn operations_page(
        &self,
        request: GetOperationsByCursorRequest,
    ) -> Result<GetOperationsByCursorResponse> {
        let response = match &self.kind {
            ApiKind::Sandbox(api) => {
                api.raw()
                    .sandbox()
                    .get_sandbox_operations_by_cursor(request)
                    .await?
            }
            ApiKind::Production(api) => {
                api.raw()
                    .operations()
                    .get_operations_by_cursor(request)
                    .await?
            }
        };
        Ok(response.into_inner())
    }

    /// Виртуальное пополнение есть только в Sandbox.
    pub(super) fn sandbox(&self) -> Result<&SandboxApi> {
        match &self.kind {
            ApiKind::Sandbox(api) => Ok(api),
            ApiKind::Production(_) => Err(Error::UnsupportedCapability(
                "virtual pay-in exists only in Sandbox; fund a real account in the broker app",
            )),
        }
    }
}

enum BrokerKind {
    Sandbox(SandboxBroker),
    Production {
        read: ProductionBroker,
        trade: ProductionTrading,
    },
}

/// Брокер выбранного контура для engine и executor.
pub(super) struct VenueBroker {
    kind: BrokerKind,
    series: Arc<SeriesBook>,
}

impl VenueBroker {
    pub(super) const fn contour(&self) -> Contour {
        match self.kind {
            BrokerKind::Sandbox(_) => Contour::Sandbox,
            BrokerKind::Production { .. } => Contour::Production,
        }
    }

    /// Подписки уже содержат контракты: их строит runtime через [`SeriesBook::contract`].
    pub(super) fn market_data_stream(
        &self,
        subscriptions: Vec<MarketDataSubscription>,
        config: StreamConfig,
    ) -> Result<MarketDataStream> {
        match &self.kind {
            BrokerKind::Sandbox(broker) => broker.market_data_stream(subscriptions, config),
            BrokerKind::Production { read, .. } => read.market_data_stream(subscriptions, config),
        }
    }

    async fn raw_candles(&self, request: CandleHistoryRequest) -> Result<ApiResponse<Vec<Candle>>> {
        match &self.kind {
            BrokerKind::Sandbox(broker) => broker.candles(request).await,
            BrokerKind::Production { read, .. } => read.candles(request).await,
        }
    }
}

/// Свечи одного контракта без подстановки серий: источник склейки истории.
struct RawCandles<'a>(&'a VenueBroker);

#[async_trait]
impl MarketData for RawCandles<'_> {
    async fn candles(&self, request: CandleHistoryRequest) -> Result<ApiResponse<Vec<Candle>>> {
        self.0.raw_candles(request).await
    }

    async fn trading_availability(
        &self,
        instrument_id: &InstrumentId,
    ) -> Result<ApiResponse<TradingAvailability>> {
        self.0.trading_availability(instrument_id).await
    }
}

/// Подключает контур из TOML. Токен берётся только из окружения процесса:
/// `TINKOFF_TOKEN` для Sandbox, `TINKOFF_TOKEN_PRODUCTION` для реального счёта.
pub(super) async fn connect(
    contour: Contour,
    app_name: &str,
) -> Result<(VenueApi, Arc<VenueBroker>)> {
    let name = contour.token_variable();
    let token =
        std::env::var(name).map_err(|_| Error::Configuration(format!("{name} is required")))?;
    if token.trim().is_empty() {
        return Err(Error::Configuration(format!("{name} cannot be empty")));
    }
    let token = SecretString::new(token);
    let series = Arc::new(SeriesBook::default());
    match contour {
        Contour::Sandbox => {
            let api = SandboxApi::builder(token)
                .app_name(app_name.to_owned())
                .connect()
                .await?;
            let broker = SandboxBroker::new(api.clone());
            Ok((
                VenueApi {
                    kind: ApiKind::Sandbox(api),
                    series: series.clone(),
                },
                Arc::new(VenueBroker {
                    kind: BrokerKind::Sandbox(broker),
                    series,
                }),
            ))
        }
        Contour::Production => {
            let api = ProductionApi::builder(token)
                .app_name(app_name.to_owned())
                .connect()
                .await?;
            let read = ProductionBroker::new(api.clone());
            // Реальный контур выбран явно в TOML оператора; остановка проверяется в БД.
            let trade = read
                .clone()
                .enable_trading(LiveTradingPermit::granted_by_operator());
            Ok((
                VenueApi {
                    kind: ApiKind::Production(api),
                    series: series.clone(),
                },
                Arc::new(VenueBroker {
                    kind: BrokerKind::Production { read, trade },
                    series,
                }),
            ))
        }
    }
}

/// Причина, по которой счёт нельзя использовать в выбранном контуре.
pub(super) fn account_problem(contour: Contour, account: &AccountSummary) -> Option<&'static str> {
    if contour == Contour::Sandbox {
        return None;
    }
    if account.status != AccountStatus::Open as i32 {
        return Some("real account is not open");
    }
    if account.access_level != AccessLevel::AccountAccessLevelFullAccess as i32 {
        return Some(
            "token has no full access to the real account; a read-only token cannot trade",
        );
    }
    None
}

/// Проверяет, что счёт доступен токену, а реальный счёт открыт с полным доступом.
pub(super) async fn ensure_account(broker: &VenueBroker, account_id: &AccountId) -> Result<()> {
    let contour = broker.contour();
    let accounts = broker.accounts().await?.data;
    let Some(account) = accounts.iter().find(|account| &account.id == account_id) else {
        return Err(Error::Configuration(format!(
            "{} account `{account_id}` is unavailable to this token",
            contour.title()
        )));
    };
    match account_problem(contour, account) {
        Some(problem) => Err(Error::Configuration(format!("{problem}: {account_id}"))),
        None => Ok(()),
    }
}

#[async_trait]
impl MarketData for VenueBroker {
    async fn candles(&self, request: CandleHistoryRequest) -> Result<ApiResponse<Vec<Candle>>> {
        if series_code(&request.instrument_id).is_none() {
            return self.raw_candles(request).await;
        }
        let data = self.series.candles(&RawCandles(self), request).await?;
        Ok(ApiResponse {
            data,
            metadata: tinkoff_rs::ApiMetadata::default(),
        })
    }

    async fn trading_availability(
        &self,
        instrument_id: &InstrumentId,
    ) -> Result<ApiResponse<TradingAvailability>> {
        let contract = self.series.contract(instrument_id);
        match &self.kind {
            BrokerKind::Sandbox(broker) => broker.trading_availability(&contract).await,
            BrokerKind::Production { read, .. } => read.trading_availability(&contract).await,
        }
    }
}

#[async_trait]
impl Accounts for VenueBroker {
    async fn accounts(&self) -> Result<ApiResponse<Vec<AccountSummary>>> {
        match &self.kind {
            BrokerKind::Sandbox(broker) => broker.accounts().await,
            BrokerKind::Production { read, .. } => read.accounts().await,
        }
    }
}

#[async_trait]
impl Portfolio for VenueBroker {
    async fn portfolio(&self, account_id: &AccountId) -> Result<ApiResponse<PortfolioSnapshot>> {
        let mut response = match &self.kind {
            BrokerKind::Sandbox(broker) => broker.portfolio(account_id).await?,
            BrokerKind::Production { read, .. } => read.portfolio(account_id).await?,
        };
        // Позиция в привязанном контракте — позиция серии.
        for position in &mut response.data.positions {
            position.instrument_id = self.series.of_broker(position.instrument_id.clone());
        }
        Ok(response)
    }
}

#[async_trait]
impl Trading for VenueBroker {
    async fn place_order(&self, intent: &OrderIntent) -> Result<ApiResponse<OrderUpdate>> {
        // Заявка по серии уходит в привязанный контракт.
        let mapped;
        let intent = if series_code(&intent.instrument_id).is_some() {
            let mut copy = intent.clone();
            copy.instrument_id = self.series.contract(&intent.instrument_id);
            mapped = copy;
            &mapped
        } else {
            intent
        };
        let (read, trade) = match &self.kind {
            BrokerKind::Sandbox(broker) => return broker.place_order(intent).await,
            BrokerKind::Production { read, trade } => (read, trade),
        };
        // Те же проверки, что делает SandboxBroker: заявка, отложенная recovery,
        // уходит только при открытых торгах и по свежей котировке.
        let availability = read.trading_availability(&intent.instrument_id).await?.data;
        if !availability.api_trade_available
            || (intent.kind == OrderKind::Market && !availability.market_order_available)
        {
            return Err(Error::RiskRejected(format!(
                "trading is currently unavailable for {}",
                intent.instrument_id
            )));
        }
        let response = read
            .api()
            .raw()
            .market_data()
            .get_last_prices(GetLastPricesRequest {
                instrument_id: vec![intent.instrument_id.to_string()],
                ..Default::default()
            })
            .await?
            .into_inner();
        let quote = response
            .last_prices
            .iter()
            .find(|price| price.instrument_uid == intent.instrument_id.as_str())
            .ok_or_else(|| Error::RiskRejected("current execution quote is missing".to_owned()))?;
        Price::from_recent_trade(quote, Utc::now())?;
        trade.place_order(intent).await
    }

    async fn order_state(
        &self,
        account_id: &AccountId,
        request_id: OrderRequestId,
    ) -> Result<ApiResponse<OrderUpdate>> {
        match &self.kind {
            BrokerKind::Sandbox(broker) => broker.order_state(account_id, request_id).await,
            BrokerKind::Production { trade, .. } => trade.order_state(account_id, request_id).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(status: AccountStatus, access: AccessLevel) -> AccountSummary {
        AccountSummary {
            id: AccountId::new("2000000001".to_owned()).unwrap(),
            name: "robot".to_owned(),
            status: status as i32,
            access_level: access as i32,
        }
    }

    #[test]
    fn real_account_needs_open_status_and_full_access() {
        let ok = account(
            AccountStatus::Open,
            AccessLevel::AccountAccessLevelFullAccess,
        );
        assert_eq!(account_problem(Contour::Production, &ok), None);
        let read_only = account(AccountStatus::Open, AccessLevel::AccountAccessLevelReadOnly);
        assert!(account_problem(Contour::Production, &read_only).is_some());
        let closed = account(
            AccountStatus::Closed,
            AccessLevel::AccountAccessLevelFullAccess,
        );
        assert!(account_problem(Contour::Production, &closed).is_some());
        // Sandbox-счета не проверяются на уровень доступа реального токена.
        assert_eq!(account_problem(Contour::Sandbox, &read_only), None);
    }

    #[test]
    fn each_contour_reads_its_own_token() {
        assert_eq!(Contour::Sandbox.token_variable(), "TINKOFF_TOKEN");
        assert_eq!(
            Contour::Production.token_variable(),
            "TINKOFF_TOKEN_PRODUCTION"
        );
    }
}
