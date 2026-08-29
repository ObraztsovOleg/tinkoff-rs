//! Низкоуровневые клиенты production и sandbox контуров.
//!
//! [`ProductionApi`] и [`SandboxApi`] выбирают endpoint своим типом. Builder
//! настраивает transport и устанавливает соединение, а метод `raw()` открывает
//! generated protobuf clients для всех RPC текущего snapshot.
//!
//! Прикладному коду обычно удобнее обернуть API в тип из [`crate::broker`]. Raw
//! доступ нужен, когда соответствующий доменный helper ещё не реализован.

use std::time::Duration;

use secrecy::SecretString;

use crate::{
    PRODUCTION_ENDPOINT, Result, SANDBOX_ENDPOINT,
    proto::{
        instruments_service_client::InstrumentsServiceClient,
        market_data_service_client::MarketDataServiceClient,
        market_data_stream_service_client::MarketDataStreamServiceClient,
        operations_service_client::OperationsServiceClient,
        operations_stream_service_client::OperationsStreamServiceClient,
        orders_service_client::OrdersServiceClient,
        orders_stream_service_client::OrdersStreamServiceClient,
        sandbox_service_client::SandboxServiceClient, signal_service_client::SignalServiceClient,
        stop_orders_service_client::StopOrdersServiceClient,
        users_service_client::UsersServiceClient,
    },
    transport::{AuthenticatedChannel, TransportConfig, connect},
};

/// Generated клиент справочника инструментов.
pub type InstrumentsApiClient = InstrumentsServiceClient<AuthenticatedChannel>;
/// Generated клиент unary-методов рыночных данных.
pub type MarketDataApiClient = MarketDataServiceClient<AuthenticatedChannel>;
/// Generated bidirectional клиент потоковых рыночных данных.
pub type MarketDataStreamApiClient = MarketDataStreamServiceClient<AuthenticatedChannel>;
/// Generated клиент портфеля и операций.
pub type OperationsApiClient = OperationsServiceClient<AuthenticatedChannel>;
/// Generated клиент потоков портфеля, позиций и операций.
pub type OperationsStreamApiClient = OperationsStreamServiceClient<AuthenticatedChannel>;
/// Generated клиент production-заявок.
pub type OrdersApiClient = OrdersServiceClient<AuthenticatedChannel>;
/// Generated клиент потока обновлений заявок.
pub type OrdersStreamApiClient = OrdersStreamServiceClient<AuthenticatedChannel>;
/// Generated клиент аналитических сигналов.
pub type SignalApiClient = SignalServiceClient<AuthenticatedChannel>;
/// Generated клиент стоп-заявок.
pub type StopOrdersApiClient = StopOrdersServiceClient<AuthenticatedChannel>;
/// Generated клиент счетов пользователя.
pub type UsersApiClient = UsersServiceClient<AuthenticatedChannel>;
/// Generated клиент административных и торговых RPC песочницы.
pub type SandboxServiceApiClient = SandboxServiceClient<AuthenticatedChannel>;

#[derive(Clone)]
struct CommonRawApi {
    channel: AuthenticatedChannel,
}

macro_rules! common_raw_accessors {
    () => {
        /// Создаёт независимый handle generated `InstrumentsService` client.
        #[must_use]
        pub fn instruments(&self) -> InstrumentsApiClient {
            InstrumentsServiceClient::new(self.inner.channel.clone())
        }

        /// Создаёт handle unary `MarketDataService` client.
        #[must_use]
        pub fn market_data(&self) -> MarketDataApiClient {
            MarketDataServiceClient::new(self.inner.channel.clone())
        }

        /// Создаёт handle bidirectional `MarketDataStreamService` client.
        #[must_use]
        pub fn market_data_stream(&self) -> MarketDataStreamApiClient {
            MarketDataStreamServiceClient::new(self.inner.channel.clone())
        }

        /// Создаёт handle unary `OperationsService` client.
        #[must_use]
        pub fn operations(&self) -> OperationsApiClient {
            OperationsServiceClient::new(self.inner.channel.clone())
        }

        /// Создаёт handle `OperationsStreamService` client.
        #[must_use]
        pub fn operations_stream(&self) -> OperationsStreamApiClient {
            OperationsStreamServiceClient::new(self.inner.channel.clone())
        }

        /// Создаёт handle unary `OrdersService` client.
        #[must_use]
        pub fn orders(&self) -> OrdersApiClient {
            OrdersServiceClient::new(self.inner.channel.clone())
        }

        /// Создаёт handle `OrdersStreamService` client.
        #[must_use]
        pub fn orders_stream(&self) -> OrdersStreamApiClient {
            OrdersStreamServiceClient::new(self.inner.channel.clone())
        }

        /// Создаёт handle `SignalService` client.
        #[must_use]
        pub fn signals(&self) -> SignalApiClient {
            SignalServiceClient::new(self.inner.channel.clone())
        }

        /// Создаёт handle `StopOrdersService` client.
        #[must_use]
        pub fn stop_orders(&self) -> StopOrdersApiClient {
            StopOrdersServiceClient::new(self.inner.channel.clone())
        }

        /// Создаёт handle `UsersService` client.
        #[must_use]
        pub fn users(&self) -> UsersApiClient {
            UsersServiceClient::new(self.inner.channel.clone())
        }
    };
}

#[derive(Clone)]
/// Raw generated clients реального контура.
///
/// Тип намеренно не содержит `SandboxService`.
pub struct ProductionRawApi {
    inner: CommonRawApi,
}

impl ProductionRawApi {
    common_raw_accessors!();
}

#[derive(Clone)]
/// Raw generated clients sandbox-контура, включая [`Self::sandbox`].
pub struct SandboxRawApi {
    inner: CommonRawApi,
}

impl SandboxRawApi {
    common_raw_accessors!();

    /// Создаёт handle `SandboxService` client.
    #[must_use]
    pub fn sandbox(&self) -> SandboxServiceApiClient {
        SandboxServiceClient::new(self.inner.channel.clone())
    }
}

#[derive(Clone)]
/// Корень подключения к реальному T-Invest API.
///
/// Этот тип сам по себе лишь предоставляет raw clients. Безопасное разделение
/// чтения и торговли реализовано типами из [`crate::broker`].
pub struct ProductionApi {
    raw: ProductionRawApi,
}

impl ProductionApi {
    /// Начинает настройку production-подключения с секретным токеном.
    #[must_use]
    pub fn builder(token: SecretString) -> ProductionApiBuilder {
        ProductionApiBuilder::new(token)
    }

    /// Возвращает клонируемый набор generated client accessors.
    #[must_use]
    pub fn raw(&self) -> ProductionRawApi {
        self.raw.clone()
    }
}

#[derive(Clone)]
/// Корень подключения к изолированному sandbox T-Invest API.
pub struct SandboxApi {
    raw: SandboxRawApi,
}

impl SandboxApi {
    /// Начинает настройку sandbox-подключения с sandbox-токеном.
    #[must_use]
    pub fn builder(token: SecretString) -> SandboxApiBuilder {
        SandboxApiBuilder::new(token)
    }

    /// Возвращает generated clients, включая `SandboxService`.
    #[must_use]
    pub fn raw(&self) -> SandboxRawApi {
        self.raw.clone()
    }
}

#[derive(Clone)]
struct BuilderConfig {
    token: SecretString,
    transport: TransportConfig,
}

impl BuilderConfig {
    fn new(token: SecretString, endpoint: &str) -> Self {
        Self {
            token,
            transport: TransportConfig {
                endpoint: endpoint.to_owned(),
                app_name: "ObraztsovOleg.tinkoff-rs".to_owned(),
                connect_timeout: Duration::from_secs(10),
                request_timeout: Duration::from_secs(30),
            },
        }
    }

    fn app_name(mut self, app_name: impl Into<String>) -> Self {
        self.transport.app_name = app_name.into();
        self
    }

    fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.transport.endpoint = endpoint.into();
        self
    }

    fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.transport.connect_timeout = timeout;
        self
    }

    fn request_timeout(mut self, timeout: Duration) -> Self {
        self.transport.request_timeout = timeout;
        self
    }

    async fn connect(self) -> Result<CommonRawApi> {
        let channel = connect(&self.token, &self.transport).await?;
        Ok(CommonRawApi { channel })
    }
}

macro_rules! api_builder {
    ($builder:ident, $api:ident, $raw:ident, $endpoint:expr) => {
        /// Builder transport-настроек выбранного типом API-контура.
        pub struct $builder {
            inner: BuilderConfig,
        }

        impl $builder {
            fn new(token: SecretString) -> Self {
                Self {
                    inner: BuilderConfig::new(token, $endpoint),
                }
            }

            /// Задаёт значение заголовка `x-app-name` для всех запросов.
            #[must_use]
            pub fn app_name(mut self, app_name: impl Into<String>) -> Self {
                self.inner = self.inner.app_name(app_name);
                self
            }

            /// Переопределяет endpoint, не меняя тип выбранного контура.
            ///
            /// В production-коде обычно не нужен; предназначен прежде всего для
            /// integration-тестов и контролируемых прокси.
            #[must_use]
            pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
                self.inner = self.inner.endpoint(endpoint);
                self
            }

            /// Задаёт максимальное время установления TCP/TLS-соединения.
            #[must_use]
            pub fn connect_timeout(mut self, timeout: Duration) -> Self {
                self.inner = self.inner.connect_timeout(timeout);
                self
            }

            /// Задаёт общий timeout одного gRPC-запроса.
            #[must_use]
            pub fn request_timeout(mut self, timeout: Duration) -> Self {
                self.inner = self.inner.request_timeout(timeout);
                self
            }

            /// Устанавливает соединение и возвращает API выбранного контура.
            pub async fn connect(self) -> Result<$api> {
                Ok($api {
                    raw: $raw {
                        inner: self.inner.connect().await?,
                    },
                })
            }
        }
    };
}

api_builder!(
    ProductionApiBuilder,
    ProductionApi,
    ProductionRawApi,
    PRODUCTION_ENDPOINT
);
api_builder!(
    SandboxApiBuilder,
    SandboxApi,
    SandboxRawApi,
    SANDBOX_ENDPOINT
);
