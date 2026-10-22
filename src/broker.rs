//! Broker-neutral интерфейсы и маршрутизация production/sandbox RPC.
//!
//! Capability traits (`Accounts`, `Portfolio`, `Trading` и другие) описывают не
//! конкретный gRPC client, а небольшую прикладную возможность. Благодаря этому
//! `BatchExecutor<P, T, S>` принимает любые реализации портфеля, торговли и storage:
//! настоящий broker, sandbox или test double.
//!
//! Важная граница безопасности: [`ProductionBroker`] реализует чтения, но не
//! [`Trading`]. Реальные mutation доступны только через [`ProductionTrading`],
//! полученный с явным [`LiveTradingPermit`]. [`SandboxBroker`] реализует и
//! чтение, и sandbox trading/admin.

use std::pin::Pin;

use async_trait::async_trait;
use chrono::Utc;
use futures::{Stream, StreamExt};

use crate::{
    ApiResponse, Error, ProductionApi, Result, SandboxApi,
    api::{MarketDataApiClient, OperationsApiClient, OrdersApiClient, UsersApiClient},
    domain::{
        AccountId, AccountSummary, Candle, CandleHistoryRequest, InstrumentId, MarginConsent,
        Money, OperationEvent, OrderIntent, OrderKind, OrderRequestId, OrderSide, OrderStatus,
        OrderUpdate, PortfolioPosition, PortfolioSnapshot, Price, TradingAvailability,
        datetime_to_timestamp, timestamp_to_datetime,
    },
    proto::{
        self, BrokerReportRequest, BrokerReportResponse, CloseSandboxAccountRequest,
        GetAccountsRequest, GetCandlesRequest, GetDividendsForeignIssuerRequest,
        GetDividendsForeignIssuerResponse, GetOrderStateRequest, GetTradingStatusRequest,
        OpenSandboxAccountRequest, OrderDirection, OrderExecutionReportStatus, OrderIdType,
        OrderType, PortfolioRequest, PostOrderRequest, PriceType, SandboxPayInRequest,
        TimeInForceType, get_candles_request::CandleSource, operations_stream_response,
        order_state_stream_response,
    },
    retry::{RetryPolicy, idempotent},
    stream::{self, MarketDataStream, MarketDataSubscription, StreamConfig},
};

/// Поток нормализованных обновлений заявок.
pub type OrderEventStream = Pin<Box<dyn Stream<Item = Result<OrderUpdate>> + Send + 'static>>;
/// Поток нормализованных операций по счетам.
pub type OperationEventStream =
    Pin<Box<dyn Stream<Item = Result<OperationEvent>> + Send + 'static>>;

#[async_trait]
/// Исторические котировки и текущая доступность торгов.
pub trait MarketData: Send + Sync {
    /// Загружает исторические свечи с retry безопасных чтений.
    async fn candles(&self, request: CandleHistoryRequest) -> Result<ApiResponse<Vec<Candle>>>;
    /// Возвращает разрешённые сейчас виды заявок для инструмента.
    async fn trading_availability(
        &self,
        instrument_id: &InstrumentId,
    ) -> Result<ApiResponse<TradingAvailability>>;
}

#[async_trait]
/// Получение доступных текущему токену счетов.
pub trait Accounts: Send + Sync {
    /// Возвращает краткие данные всех доступных счетов выбранного контура.
    async fn accounts(&self) -> Result<ApiResponse<Vec<AccountSummary>>>;
}

#[async_trait]
/// Получение актуального снимка портфеля.
pub trait Portfolio: Send + Sync {
    /// Загружает позиции и общую стоимость указанного счёта.
    async fn portfolio(&self, account_id: &AccountId) -> Result<ApiResponse<PortfolioSnapshot>>;
}

#[async_trait]
/// Минимальные торговые операции, необходимые executor.
///
/// Здесь намеренно нет автоматического retry: timeout после mutation может быть
/// неоднозначным. Вызывающий executor обязан сначала проверить `order_state` с
/// тем же request UUID.
pub trait Trading: Send + Sync {
    /// Выставляет заявку, описанную уже проверенным [`OrderIntent`].
    async fn place_order(&self, intent: &OrderIntent) -> Result<ApiResponse<OrderUpdate>>;
    /// Ищет заявку по client request UUID, а не только по exchange ID.
    async fn order_state(
        &self,
        account_id: &AccountId,
        request_id: OrderRequestId,
    ) -> Result<ApiResponse<OrderUpdate>>;
}

#[async_trait]
/// Подписка на изменения состояния заявок.
pub trait OrdersStream: Send + Sync {
    /// Открывает восстанавливаемый поток для перечисленных счетов.
    async fn order_updates(&self, accounts: Vec<AccountId>) -> Result<OrderEventStream>;
}

#[async_trait]
/// Подписка на новые брокерские операции.
pub trait OperationsStream: Send + Sync {
    /// Открывает восстанавливаемый поток операций для перечисленных счетов.
    async fn operation_updates(&self, accounts: Vec<AccountId>) -> Result<OperationEventStream>;
}

#[async_trait]
/// Production-only отчёты, отсутствующие в sandbox broker interface.
pub trait Reports: Send + Sync {
    /// Запрашивает брокерский отчёт raw protobuf-типами.
    async fn broker_report(
        &self,
        request: BrokerReportRequest,
    ) -> Result<ApiResponse<BrokerReportResponse>>;

    /// Запрашивает отчёт о доходах от эмитентов за пределами РФ.
    async fn dividends_foreign_issuer(
        &self,
        request: GetDividendsForeignIssuerRequest,
    ) -> Result<ApiResponse<GetDividendsForeignIssuerResponse>>;
}

#[async_trait]
/// Управление жизненным циклом виртуальных sandbox-счетов.
///
/// Runtime не вызывает эти методы автоматически, чтобы запуск стратегии не
/// создавал и не пополнял счета неявно.
pub trait SandboxAdmin: Send + Sync {
    /// Открывает виртуальный счёт и возвращает его ID.
    async fn open_account(&self, name: Option<String>) -> Result<ApiResponse<AccountId>>;
    /// Добавляет виртуальные средства на sandbox-счёт.
    async fn pay_in(&self, account_id: &AccountId, amount: Money) -> Result<ApiResponse<Money>>;
    /// Закрывает виртуальный sandbox-счёт.
    async fn close_account(&self, account_id: &AccountId) -> Result<ApiResponse<()>>;
}

#[derive(Clone)]
/// Production broker с возможностями чтения, но без отправки заявок.
///
/// Production mutation намеренно требует [`ProductionTrading`]:
///
/// ```compile_fail
/// use tinkoff_rs::{broker::{Trading, ProductionBroker}};
/// fn needs_trading<T: Trading>() {}
/// needs_trading::<ProductionBroker>();
/// ```
///
/// Sandbox administration также недоступно этому типу:
///
/// ```compile_fail
/// use tinkoff_rs::{broker::{SandboxAdmin, ProductionBroker}};
/// fn needs_admin<T: SandboxAdmin>() {}
/// needs_admin::<ProductionBroker>();
/// ```
pub struct ProductionBroker {
    api: ProductionApi,
    retry: RetryPolicy,
}

impl ProductionBroker {
    /// Оборачивает production API и включает стандартный retry чтений.
    #[must_use]
    pub fn new(api: ProductionApi) -> Self {
        Self {
            api,
            retry: RetryPolicy::default(),
        }
    }

    /// Заменяет политику retry только для идемпотентных запросов чтения.
    #[must_use]
    pub const fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Потребляет read-only broker и создаёт отдельный trading capability.
    ///
    /// Значение permit нельзя сконструировать напрямую извне модуля: штатный
    /// путь — [`LiveTradingPermit::from_env`].
    #[must_use]
    pub fn enable_trading(self, _permit: LiveTradingPermit) -> ProductionTrading {
        ProductionTrading { broker: self }
    }

    /// Возвращает ссылку на underlying production API для редких raw RPC.
    #[must_use]
    pub const fn api(&self) -> &ProductionApi {
        &self.api
    }

    /// Открывает управляемый поток рыночных данных с initial subscriptions.
    pub fn market_data_stream(
        &self,
        subscriptions: Vec<MarketDataSubscription>,
        config: StreamConfig,
    ) -> Result<MarketDataStream> {
        stream::market_data_stream(self.api.raw().market_data_stream(), subscriptions, config)
    }

    /// Открывает reconnecting raw protobuf поток изменений портфеля.
    pub fn portfolio_stream(
        &self,
        accounts: Vec<AccountId>,
        config: StreamConfig,
    ) -> Result<tokio_stream::wrappers::ReceiverStream<Result<proto::PortfolioStreamResponse>>>
    {
        stream::portfolio_stream(self.api.raw().operations_stream(), accounts, config)
    }

    /// Открывает reconnecting raw protobuf поток позиций.
    pub fn positions_stream(
        &self,
        accounts: Vec<AccountId>,
        with_initial_positions: bool,
        config: StreamConfig,
    ) -> Result<tokio_stream::wrappers::ReceiverStream<Result<proto::PositionsStreamResponse>>>
    {
        stream::positions_stream(
            self.api.raw().operations_stream(),
            accounts,
            with_initial_positions,
            config,
        )
    }
}

#[derive(Clone)]
/// Отдельная capability-обёртка, разрешающая production-заявки.
///
/// Она реализует [`Trading`], тогда как [`ProductionBroker`] — нет.
pub struct ProductionTrading {
    broker: ProductionBroker,
}

#[derive(Clone)]
/// Broker изолированного sandbox-контура.
///
/// Production-only отчёты намеренно отсутствуют:
///
/// ```compile_fail
/// use tinkoff_rs::{broker::{Reports, SandboxBroker}};
/// fn needs_reports<T: Reports>() {}
/// needs_reports::<SandboxBroker>();
/// ```
pub struct SandboxBroker {
    api: SandboxApi,
    retry: RetryPolicy,
}

impl SandboxBroker {
    /// Оборачивает sandbox API и включает стандартный retry чтений.
    #[must_use]
    pub fn new(api: SandboxApi) -> Self {
        Self {
            api,
            retry: RetryPolicy::default(),
        }
    }

    /// Заменяет политику retry только для идемпотентных sandbox-чтений.
    #[must_use]
    pub const fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Возвращает ссылку на underlying sandbox API для редких raw RPC.
    #[must_use]
    pub const fn api(&self) -> &SandboxApi {
        &self.api
    }

    /// Открывает управляемый sandbox-поток рыночных данных.
    pub fn market_data_stream(
        &self,
        subscriptions: Vec<MarketDataSubscription>,
        config: StreamConfig,
    ) -> Result<MarketDataStream> {
        stream::market_data_stream(self.api.raw().market_data_stream(), subscriptions, config)
    }

    /// Открывает reconnecting raw protobuf поток sandbox-портфеля.
    pub fn portfolio_stream(
        &self,
        accounts: Vec<AccountId>,
        config: StreamConfig,
    ) -> Result<tokio_stream::wrappers::ReceiverStream<Result<proto::PortfolioStreamResponse>>>
    {
        stream::portfolio_stream(self.api.raw().operations_stream(), accounts, config)
    }

    /// Открывает reconnecting raw protobuf поток sandbox-позиций.
    pub fn positions_stream(
        &self,
        accounts: Vec<AccountId>,
        with_initial_positions: bool,
        config: StreamConfig,
    ) -> Result<tokio_stream::wrappers::ReceiverStream<Result<proto::PositionsStreamResponse>>>
    {
        stream::positions_stream(
            self.api.raw().operations_stream(),
            accounts,
            with_initial_positions,
            config,
        )
    }
}

#[derive(Clone, Copy, Debug)]
/// Явное разрешение текущему процессу создавать production trading client.
///
/// Это дополнительный предохранитель, а не замена ограничениям токена, risk
/// policy и контролю конфигурации счёта.
pub struct LiveTradingPermit(());

impl LiveTradingPermit {
    /// Разрешение, которое приложение выдаёт по собственной процедуре оператора:
    /// например, явный реальный контур в конфигурации, отдельный токен счёта и
    /// аварийная остановка. Вызов — явное решение кода приложения, а не окружения.
    #[must_use]
    pub const fn granted_by_operator() -> Self {
        Self(())
    }

    /// Возвращает permit только когда `TINKOFF_ALLOW_LIVE_TRADING` равно `true`
    /// без учёта регистра; иначе возвращает `LiveTradingDisabled`.
    pub fn from_env() -> Result<Self> {
        let enabled = std::env::var("TINKOFF_ALLOW_LIVE_TRADING")
            .unwrap_or_default()
            .eq_ignore_ascii_case("true");
        if enabled {
            Ok(Self(()))
        } else {
            Err(Error::LiveTradingDisabled)
        }
    }
}

#[async_trait]
impl MarketData for ProductionBroker {
    async fn candles(&self, request: CandleHistoryRequest) -> Result<ApiResponse<Vec<Candle>>> {
        fetch_candles(self.api.raw().market_data(), request, self.retry).await
    }

    async fn trading_availability(
        &self,
        instrument_id: &InstrumentId,
    ) -> Result<ApiResponse<TradingAvailability>> {
        fetch_trading_availability(
            self.api.raw().market_data(),
            instrument_id.clone(),
            self.retry,
        )
        .await
    }
}

#[async_trait]
impl MarketData for SandboxBroker {
    async fn candles(&self, request: CandleHistoryRequest) -> Result<ApiResponse<Vec<Candle>>> {
        fetch_candles(self.api.raw().market_data(), request, self.retry).await
    }

    async fn trading_availability(
        &self,
        instrument_id: &InstrumentId,
    ) -> Result<ApiResponse<TradingAvailability>> {
        fetch_trading_availability(
            self.api.raw().market_data(),
            instrument_id.clone(),
            self.retry,
        )
        .await
    }
}

#[async_trait]
impl Accounts for ProductionBroker {
    async fn accounts(&self) -> Result<ApiResponse<Vec<AccountSummary>>> {
        fetch_production_accounts(self.api.raw().users(), self.retry).await
    }
}

#[async_trait]
impl Accounts for SandboxBroker {
    async fn accounts(&self) -> Result<ApiResponse<Vec<AccountSummary>>> {
        // В отличие от production UsersService, sandbox-счета принадлежат
        // SandboxService. Снаружи различие скрыто общим trait Accounts.
        let raw = self.api.raw();
        let response = idempotent(self.retry, || {
            let mut client = raw.sandbox();
            async move {
                Ok(client
                    .get_sandbox_accounts(GetAccountsRequest { status: None })
                    .await?)
            }
        })
        .await?;
        response_from_accounts(response)
    }
}

#[async_trait]
impl Portfolio for ProductionBroker {
    async fn portfolio(&self, account_id: &AccountId) -> Result<ApiResponse<PortfolioSnapshot>> {
        fetch_production_portfolio(self.api.raw().operations(), account_id.clone(), self.retry)
            .await
    }
}

#[async_trait]
impl Portfolio for SandboxBroker {
    async fn portfolio(&self, account_id: &AccountId) -> Result<ApiResponse<PortfolioSnapshot>> {
        // Ответ SandboxService имеет ту же PortfolioResponse-модель, поэтому
        // дальше используется общий mapper с production-контуром.
        let raw = self.api.raw();
        let request = PortfolioRequest {
            account_id: account_id.to_string(),
            currency: None,
        };
        let response = idempotent(self.retry, || {
            let mut client = raw.sandbox();
            let request = request.clone();
            async move { Ok(client.get_sandbox_portfolio(request).await?) }
        })
        .await?;
        map_portfolio_response(response)
    }
}

#[async_trait]
impl Trading for ProductionTrading {
    async fn place_order(&self, intent: &OrderIntent) -> Result<ApiResponse<OrderUpdate>> {
        place_production_order(self.broker.api.raw().orders(), intent).await
    }

    async fn order_state(
        &self,
        account_id: &AccountId,
        request_id: OrderRequestId,
    ) -> Result<ApiResponse<OrderUpdate>> {
        get_production_order_state(self.broker.api.raw().orders(), account_id, request_id).await
    }
}

#[async_trait]
impl Trading for SandboxBroker {
    async fn place_order(&self, intent: &OrderIntent) -> Result<ApiResponse<OrderUpdate>> {
        let availability = self.trading_availability(&intent.instrument_id).await?.data;
        if !availability.api_trade_available
            || (intent.kind == OrderKind::Market && !availability.market_order_available)
        {
            return Err(Error::RiskRejected(format!(
                "trading is currently unavailable for {}",
                intent.instrument_id
            )));
        }
        // Recovery may submit an intent well after planning. Validate a fresh
        // quote again immediately before the mutation, independently of candles.
        let response = self
            .api
            .raw()
            .market_data()
            .get_last_prices(proto::GetLastPricesRequest {
                instrument_id: vec![intent.instrument_id.to_string()],
                ..Default::default()
            })
            .await?
            .into_inner();
        let quote = response
            .last_prices
            .iter()
            .find(|p| p.instrument_uid == intent.instrument_id.as_str())
            .ok_or_else(|| Error::RiskRejected("current execution quote is missing".to_owned()))?;
        Price::from_recent_trade(quote, Utc::now())?;
        // order_request строит единый PostOrderRequest; меняется только RPC,
        // который принимает его в выбранном контуре.
        let mut client = self.api.raw().sandbox();
        let response = client.post_sandbox_order(order_request(intent)?).await?;
        Ok(map_post_order_response(response, intent.request_id))
    }

    async fn order_state(
        &self,
        account_id: &AccountId,
        request_id: OrderRequestId,
    ) -> Result<ApiResponse<OrderUpdate>> {
        let mut client = self.api.raw().sandbox();
        let response = client
            .get_sandbox_order_state(order_state_request(account_id, request_id))
            .await?;
        Ok(map_order_state_response(response, request_id))
    }
}

#[async_trait]
impl Reports for ProductionBroker {
    async fn broker_report(
        &self,
        request: BrokerReportRequest,
    ) -> Result<ApiResponse<BrokerReportResponse>> {
        let raw = self.api.raw();
        let response = idempotent(self.retry, || {
            let mut client = raw.operations();
            let request = request.clone();
            async move { Ok(client.get_broker_report(request).await?) }
        })
        .await?;
        Ok(ApiResponse::from_tonic(response))
    }

    async fn dividends_foreign_issuer(
        &self,
        request: GetDividendsForeignIssuerRequest,
    ) -> Result<ApiResponse<GetDividendsForeignIssuerResponse>> {
        let raw = self.api.raw();
        let response = idempotent(self.retry, || {
            let mut client = raw.operations();
            let request = request.clone();
            async move { Ok(client.get_dividends_foreign_issuer(request).await?) }
        })
        .await?;
        Ok(ApiResponse::from_tonic(response))
    }
}

#[async_trait]
impl SandboxAdmin for SandboxBroker {
    async fn open_account(&self, name: Option<String>) -> Result<ApiResponse<AccountId>> {
        let mut client = self.api.raw().sandbox();
        let response = client
            .open_sandbox_account(OpenSandboxAccountRequest { name })
            .await?;
        let response = ApiResponse::from_tonic(response);
        let metadata = response.metadata;
        Ok(ApiResponse {
            data: AccountId::new(response.data.account_id)?,
            metadata,
        })
    }

    async fn pay_in(&self, account_id: &AccountId, amount: Money) -> Result<ApiResponse<Money>> {
        let mut client = self.api.raw().sandbox();
        let response = client
            .sandbox_pay_in(SandboxPayInRequest {
                account_id: account_id.to_string(),
                amount: Some(amount.to_proto()?),
            })
            .await?;
        let response = ApiResponse::from_tonic(response);
        let metadata = response.metadata;
        let balance = response
            .data
            .balance
            .ok_or_else(|| Error::InvalidArgument("sandbox response omitted balance".to_owned()))?;
        Ok(ApiResponse {
            data: Money::from_proto(balance)?,
            metadata,
        })
    }

    async fn close_account(&self, account_id: &AccountId) -> Result<ApiResponse<()>> {
        let mut client = self.api.raw().sandbox();
        let response = client
            .close_sandbox_account(CloseSandboxAccountRequest {
                account_id: account_id.to_string(),
            })
            .await?;
        Ok(ApiResponse::from_tonic(response).map(|_| ()))
    }
}

macro_rules! stream_capabilities {
    // Production и sandbox stream RPC имеют одинаковые protobuf-контракты.
    // Макрос не меняет семантику: он только устраняет две одинаковые реализации
    // OrdersStream/OperationsStream и подставляет способ получить raw client.
    ($broker:ty, $raw:expr) => {
        #[async_trait]
        impl OrdersStream for $broker {
            async fn order_updates(&self, accounts: Vec<AccountId>) -> Result<OrderEventStream> {
                let stream = stream::order_state_stream(
                    ($raw)(self).orders_stream(),
                    accounts,
                    StreamConfig::default(),
                )?;
                Ok(Box::pin(stream.filter_map(|item| async move {
                    match item {
                        Ok(response) => map_order_stream_response(response),
                        Err(error) => Some(Err(error)),
                    }
                })))
            }
        }

        #[async_trait]
        impl OperationsStream for $broker {
            async fn operation_updates(
                &self,
                accounts: Vec<AccountId>,
            ) -> Result<OperationEventStream> {
                let stream = stream::operations_stream(
                    ($raw)(self).operations_stream(),
                    accounts,
                    StreamConfig::default(),
                )?;
                Ok(Box::pin(stream.filter_map(|item| async move {
                    match item {
                        Ok(response) => map_operation_stream_response(response),
                        Err(error) => Some(Err(error)),
                    }
                })))
            }
        }
    };
}

stream_capabilities!(ProductionBroker, |broker: &ProductionBroker| broker
    .api
    .raw());
stream_capabilities!(SandboxBroker, |broker: &SandboxBroker| broker.api.raw());

fn map_order_stream_response(
    response: proto::OrderStateStreamResponse,
) -> Option<Result<OrderUpdate>> {
    let order_state_stream_response::Payload::OrderState(order) = response.payload? else {
        return None;
    };
    Some((|| {
        let request_id = order
            .order_request_id
            .ok_or_else(|| Error::InvalidArgument("order stream omitted request id".to_owned()))?
            .parse()?;
        Ok(OrderUpdate {
            request_id,
            exchange_order_id: Some(order.order_id),
            status: map_order_status(order.execution_report_status),
            lots_requested: order.lots_requested,
            lots_executed: order.lots_executed,
            observed_at: order
                .completion_time
                .or(order.created_at)
                .map(timestamp_to_datetime)
                .transpose()?
                .unwrap_or_else(Utc::now),
        })
    })())
}

fn map_operation_stream_response(
    response: proto::OperationsStreamResponse,
) -> Option<Result<OperationEvent>> {
    let operations_stream_response::Payload::Operation(operation) = response.payload? else {
        return None;
    };
    Some((|| {
        Ok(OperationEvent {
            id: operation.id,
            account_id: AccountId::new(operation.broker_account_id)?,
            instrument_id: if operation.instrument_uid.is_empty() {
                None
            } else {
                Some(InstrumentId::new(operation.instrument_uid)?)
            },
            operation_type: operation.r#type,
            state: operation.state,
            payment: operation.payment.map(Money::from_proto).transpose()?,
            observed_at: operation
                .date
                .map(timestamp_to_datetime)
                .transpose()?
                .unwrap_or_else(Utc::now),
        })
    })())
}

#[allow(deprecated)]
async fn fetch_candles(
    client: MarketDataApiClient,
    request: CandleHistoryRequest,
    retry: RetryPolicy,
) -> Result<ApiResponse<Vec<Candle>>> {
    // Валидация до сети делает ошибки предсказуемыми и одинаковыми для обоих
    // контуров. Сам запрос является чтением, поэтому допускает idempotent retry.
    if request
        .limit
        .is_some_and(|limit| !(1..=2_500).contains(&limit))
    {
        return Err(Error::InvalidArgument(
            "candle limit must be between 1 and 2500".to_owned(),
        ));
    }
    if request.from >= request.to {
        return Err(Error::InvalidArgument(
            "candle range start must be before end".to_owned(),
        ));
    }

    let proto_request = GetCandlesRequest {
        figi: None,
        from: Some(datetime_to_timestamp(request.from)),
        to: Some(datetime_to_timestamp(request.to)),
        interval: request.interval.as_proto() as i32,
        instrument_id: Some(request.instrument_id.to_string()),
        candle_source_type: Some(CandleSource::Unspecified as i32),
        limit: request.limit,
    };
    let response = idempotent(retry, || {
        let mut client = client.clone();
        let request = proto_request.clone();
        async move { Ok(client.get_candles(request).await?) }
    })
    .await?;
    let response = ApiResponse::from_tonic(response);
    let metadata = response.metadata;
    let candles = response
        .data
        .candles
        .into_iter()
        .map(|candle| map_historic_candle(request.instrument_id.clone(), request.interval, candle))
        .collect::<Result<Vec<_>>>()?;
    Ok(ApiResponse {
        data: candles,
        metadata,
    })
}

#[allow(deprecated)]
async fn fetch_trading_availability(
    client: MarketDataApiClient,
    instrument_id: InstrumentId,
    retry: RetryPolicy,
) -> Result<ApiResponse<TradingAvailability>> {
    let request = GetTradingStatusRequest {
        figi: None,
        instrument_id: Some(instrument_id.to_string()),
    };
    let response = idempotent(retry, || {
        let mut client = client.clone();
        let request = request.clone();
        async move { Ok(client.get_trading_status(request).await?) }
    })
    .await?;
    Ok(
        ApiResponse::from_tonic(response).map(|status| TradingAvailability {
            api_trade_available: status.api_trade_available_flag,
            market_order_available: status.market_order_available_flag,
            limit_order_available: status.limit_order_available_flag,
            best_price_available: status.bestprice_order_available_flag,
        }),
    )
}

async fn fetch_production_accounts(
    client: UsersApiClient,
    retry: RetryPolicy,
) -> Result<ApiResponse<Vec<AccountSummary>>> {
    let response = idempotent(retry, || {
        let mut client = client.clone();
        async move {
            Ok(client
                .get_accounts(GetAccountsRequest { status: None })
                .await?)
        }
    })
    .await?;
    response_from_accounts(response)
}

fn response_from_accounts(
    response: tonic::Response<proto::GetAccountsResponse>,
) -> Result<ApiResponse<Vec<AccountSummary>>> {
    // Сначала metadata отделяется от payload, затем protobuf Account
    // преобразуется в более узкий доменный AccountSummary.
    let response = ApiResponse::from_tonic(response);
    let metadata = response.metadata;
    let accounts = response
        .data
        .accounts
        .into_iter()
        .map(|account| {
            Ok(AccountSummary {
                id: AccountId::new(account.id)?,
                name: account.name,
                status: account.status,
                access_level: account.access_level,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ApiResponse {
        data: accounts,
        metadata,
    })
}

async fn fetch_production_portfolio(
    client: OperationsApiClient,
    account_id: AccountId,
    retry: RetryPolicy,
) -> Result<ApiResponse<PortfolioSnapshot>> {
    let request = PortfolioRequest {
        account_id: account_id.to_string(),
        currency: None,
    };
    let response = idempotent(retry, || {
        let mut client = client.clone();
        let request = request.clone();
        async move { Ok(client.get_portfolio(request).await?) }
    })
    .await?;
    map_portfolio_response(response)
}

fn map_portfolio_response(
    response: tonic::Response<proto::PortfolioResponse>,
) -> Result<ApiResponse<PortfolioSnapshot>> {
    // Mapper общий для production и sandbox. Необязательные поля API не
    // подменяются фиктивными денежными значениями: отсутствие сохраняется как None.
    let response = ApiResponse::from_tonic(response);
    let metadata = response.metadata;
    let data = response.data;
    let positions = data
        .positions
        .into_iter()
        // RUB settlement cash contributes to total equity, but is not a tradable
        // position. Keep other currencies so dedicated-account checks still reject them.
        .filter(|position| {
            !(position.instrument_type == "currency" && position.figi == "RUB000UTSTOM")
        })
        .map(|position| {
            let quantity = position
                .quantity
                .map(Price::from_quotation)
                .transpose()?
                .map_or(rust_decimal::Decimal::ZERO, |price| price.0);
            Ok(PortfolioPosition {
                instrument_id: InstrumentId::new(position.instrument_uid)?,
                quantity,
                current_price: position.current_price.map(Money::from_proto).transpose()?,
                blocked: position.blocked,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ApiResponse {
        data: PortfolioSnapshot {
            account_id: AccountId::new(data.account_id)?,
            total: data
                .total_amount_portfolio
                .map(Money::from_proto)
                .transpose()?,
            positions,
            observed_at: Utc::now(),
        },
        metadata,
    })
}

async fn place_production_order(
    mut client: OrdersApiClient,
    intent: &OrderIntent,
) -> Result<ApiResponse<OrderUpdate>> {
    // Mutation вызывается ровно один раз. Retry/reconciliation находится выше,
    // в BatchExecutor, где уже есть durable intent и стабильный request UUID.
    let response = client.post_order(order_request(intent)?).await?;
    Ok(map_post_order_response(response, intent.request_id))
}

async fn get_production_order_state(
    mut client: OrdersApiClient,
    account_id: &AccountId,
    request_id: OrderRequestId,
) -> Result<ApiResponse<OrderUpdate>> {
    let response = client
        .get_order_state(order_state_request(account_id, request_id))
        .await?;
    Ok(map_order_state_response(response, request_id))
}

#[allow(deprecated)]
fn order_request(intent: &OrderIntent) -> Result<PostOrderRequest> {
    if intent.lots <= 0 {
        return Err(Error::InvalidArgument(
            "order lots must be positive".to_owned(),
        ));
    }
    if matches!(intent.kind, OrderKind::Limit) && intent.limit_price.is_none() {
        return Err(Error::InvalidArgument(
            "limit orders require a price".to_owned(),
        ));
    }
    // instrument_id — канонический UID. Deprecated figi оставляется пустым.
    // order_id получает стабильный request UUID из intent.
    Ok(PostOrderRequest {
        figi: None,
        quantity: intent.lots,
        price: intent.limit_price.map(Price::to_quotation).transpose()?,
        direction: match intent.side {
            OrderSide::Buy => OrderDirection::Buy,
            OrderSide::Sell => OrderDirection::Sell,
        } as i32,
        account_id: intent.account_id.to_string(),
        order_type: match intent.kind {
            OrderKind::Market => OrderType::Market,
            OrderKind::Limit => OrderType::Limit,
            OrderKind::BestPrice => OrderType::Bestprice,
        } as i32,
        order_id: intent.request_id.to_string(),
        instrument_id: intent.instrument_id.to_string(),
        time_in_force: TimeInForceType::TimeInForceDay as i32,
        price_type: PriceType::Currency as i32,
        confirm_margin_trade: matches!(intent.margin_consent, MarginConsent::UncoveredAllowed),
    })
}

fn order_state_request(account_id: &AccountId, request_id: OrderRequestId) -> GetOrderStateRequest {
    GetOrderStateRequest {
        account_id: account_id.to_string(),
        order_id: request_id.to_string(),
        price_type: PriceType::Currency as i32,
        order_id_type: Some(OrderIdType::Request as i32),
    }
}

fn map_post_order_response(
    response: tonic::Response<proto::PostOrderResponse>,
    request_id: OrderRequestId,
) -> ApiResponse<OrderUpdate> {
    let response = ApiResponse::from_tonic(response);
    let metadata = response.metadata;
    ApiResponse {
        data: OrderUpdate {
            request_id,
            exchange_order_id: Some(response.data.order_id),
            status: map_order_status(response.data.execution_report_status),
            lots_requested: response.data.lots_requested,
            lots_executed: response.data.lots_executed,
            observed_at: Utc::now(),
        },
        metadata,
    }
}

fn map_order_state_response(
    response: tonic::Response<proto::OrderState>,
    request_id: OrderRequestId,
) -> ApiResponse<OrderUpdate> {
    let response = ApiResponse::from_tonic(response);
    let metadata = response.metadata;
    ApiResponse {
        data: OrderUpdate {
            request_id,
            exchange_order_id: Some(response.data.order_id),
            status: map_order_status(response.data.execution_report_status),
            lots_requested: response.data.lots_requested,
            lots_executed: response.data.lots_executed,
            observed_at: Utc::now(),
        },
        metadata,
    }
}

fn map_order_status(value: i32) -> OrderStatus {
    match OrderExecutionReportStatus::try_from(value) {
        Ok(OrderExecutionReportStatus::ExecutionReportStatusFill) => OrderStatus::Filled,
        Ok(OrderExecutionReportStatus::ExecutionReportStatusRejected) => OrderStatus::Rejected,
        Ok(OrderExecutionReportStatus::ExecutionReportStatusCancelled) => OrderStatus::Cancelled,
        Ok(OrderExecutionReportStatus::ExecutionReportStatusNew) => OrderStatus::New,
        Ok(OrderExecutionReportStatus::ExecutionReportStatusPartiallyfill) => {
            OrderStatus::PartiallyFilled
        }
        _ => OrderStatus::Unknown,
    }
}

fn map_historic_candle(
    instrument_id: InstrumentId,
    interval: crate::domain::CandleInterval,
    candle: proto::HistoricCandle,
) -> Result<Candle> {
    fn required_price(value: Option<proto::Quotation>, field: &str) -> Result<Price> {
        value
            .map(Price::from_quotation)
            .transpose()?
            .ok_or_else(|| Error::InvalidArgument(format!("historic candle omitted {field} price")))
    }

    let time = candle
        .time
        .ok_or_else(|| Error::InvalidArgument("historic candle omitted time".to_owned()))?;
    Ok(Candle {
        instrument_id,
        interval,
        open: required_price(candle.open, "open")?,
        high: required_price(candle.high, "high")?,
        low: required_price(candle.low, "low")?,
        close: required_price(candle.close, "close")?,
        volume: candle.volume,
        time: timestamp_to_datetime(time)?,
        complete: candle.is_complete,
    })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::order_request;
    use uuid::Uuid;

    use crate::domain::{
        AccountId, ExecutionPhase, InstrumentId, MarginConsent, OrderIntent, OrderSide, StrategyId,
    };

    #[test]
    fn portfolio_cash_is_equity_not_an_outside_security_position() {
        let position = |uid: &str, figi: &str, kind: &str| crate::proto::PortfolioPosition {
            instrument_uid: uid.to_owned(),
            figi: figi.to_owned(),
            instrument_type: kind.to_owned(),
            quantity: Some(crate::proto::Quotation { units: 1, nano: 0 }),
            ..Default::default()
        };
        let response = crate::proto::PortfolioResponse {
            account_id: "account".to_owned(),
            positions: vec![
                position("rub", "RUB000UTSTOM", "currency"),
                position("share", "share-figi", "share"),
                position("usd", "usd-figi", "currency"),
            ],
            total_amount_portfolio: Some(crate::proto::MoneyValue {
                currency: "rub".to_owned(),
                units: 1_000_000,
                nano: 0,
            }),
            ..Default::default()
        };
        let portfolio = super::map_portfolio_response(tonic::Response::new(response))
            .unwrap()
            .data;
        assert_eq!(
            portfolio
                .positions
                .iter()
                .map(|p| p.instrument_id.as_str())
                .collect::<Vec<_>>(),
            vec!["share", "usd"]
        );
        assert_eq!(
            portfolio.total.unwrap().amount.0,
            rust_decimal::Decimal::from(1_000_000)
        );
    }

    #[test]
    #[allow(deprecated)]
    fn order_request_uses_instrument_id_and_not_deprecated_figi() {
        let intent = OrderIntent::market(
            Uuid::new_v4(),
            0,
            "event-1",
            StrategyId::new("portfolio-v2").expect("strategy id"),
            AccountId::new("account").expect("account id"),
            InstrumentId::new("BBG004730N88").expect("instrument id"),
            OrderSide::Buy,
            1,
            1,
            1,
            ExecutionPhase::Opening,
            MarginConsent::CoveredOnly,
            Utc::now(),
        );
        let request = order_request(&intent).expect("valid order");
        assert_eq!(request.instrument_id, "BBG004730N88");
        assert_eq!(request.figi, None);
    }
}
