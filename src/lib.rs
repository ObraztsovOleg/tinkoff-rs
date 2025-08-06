#[path = "stream/bi.stream.manager.rs"]
pub mod bi_stream_manager;
#[path = "point/point.manager.rs"]
pub mod point_manager;
#[path = "stream/bi.stream.request.rs"]
pub mod bi_stream_request;
#[path = "stream/service.stream.manager.rs"]
pub mod service_stream_manager;
#[path = "stream/service.stream.request.rs"]
pub mod service_stream_request;
#[path = "tcs.rs"]
pub mod tcs;

use tonic::{async_trait, client::Grpc, service::{interceptor::InterceptedService, Interceptor}, transport::{self, Channel, ClientTlsConfig}, Status};
use tcs::MarketDataRequest;
use secrecy::{ExposeSecret, Secret};

pub type TSResult<T> = std::result::Result<T, TSError>;

#[derive(Debug)]
pub enum TSError {
    TransportError(transport::Error),
    TransportIndividualError(reqwest::Error),
    StatusError(tonic::Status),
    SendError(flume::SendError<MarketDataRequest>),
    InfoError(String),
    ChannelSendError(String)
}
impl From<transport::Error> for TSError {
    fn from(e: transport::Error) -> Self {
        TSError::TransportError(e)
    }
}

impl From<reqwest::Error> for TSError {
    fn from(e: reqwest::Error) -> Self {
        TSError::TransportIndividualError(e)
    }
}
impl From<flume::SendError<MarketDataRequest>> for TSError {
    fn from(e: flume::SendError<MarketDataRequest>) -> Self {
        TSError::SendError(e)
    }
}
impl From<String> for TSError {
    fn from(e: String) -> Self {
        TSError::InfoError(e)
    }
}

impl<T> From<async_channel::SendError<T>> for TSError {
    fn from(error: async_channel::SendError<T>) -> Self {
        TSError::ChannelSendError(format!("Failed to send data to channel: {}", error))
    }
}

impl From<tonic::Status> for TSError {
    fn from(e: tonic::Status) -> Self {
        TSError::StatusError(e)
    }
}

#[derive(Debug)]
pub struct ServiceManager {
    token: Secret<String>
}

#[derive(Clone)]
pub struct DefaultInterceptor {
    token: Secret<String>,
}

impl Interceptor for DefaultInterceptor {
    fn call(&mut self, request: tonic::Request<()>) -> Result<tonic::Request<()>, Status> {
        let mut req = request;

        req.metadata_mut().append(
            "authorization",
            format!("bearer {}", self.token.expose_secret()).parse().unwrap()
        );
        req.metadata_mut().append(
            "x-tracking-id",
            uuid::Uuid::new_v4().to_string().parse().unwrap(),
        );
        req.metadata_mut()
            .append("x-app-name", "obrol.tinkoff".parse().unwrap());

        Ok(req)
    }
}


#[derive(derive_more::From, Clone)]
pub struct Investment(Grpc<InterceptedService<Channel, DefaultInterceptor>>);
#[derive(derive_more::From, Clone)]
pub struct SandboxInvestment(Grpc<InterceptedService<Channel, DefaultInterceptor>>);

impl Investment {
    pub fn into_inner(self) -> Grpc<InterceptedService<Channel, DefaultInterceptor>> {
        self.0
    }
}

impl SandboxInvestment {
    pub fn into_inner(self) -> Grpc<InterceptedService<Channel, DefaultInterceptor>> {
        self.0
    }
}

#[async_trait]
pub trait Service: Sized {
    async fn create_service(token: Secret<String>) -> TSResult<Self>;
}


macro_rules! impl_service {
    ([$(($type:ident, $address:expr)),*]) => {
        $(
            #[async_trait]
            impl Service for $type {
                async fn create_service(token: Secret<String>) -> TSResult<Self> {
                    let tls = ClientTlsConfig::new().with_native_roots();

                    let channel = Channel::from_static($address)
                        .tls_config(tls)?
                        .connect().await?;

                    Ok(
                        $type::from(Grpc::new(InterceptedService::new(
                            channel,
                            DefaultInterceptor { token: token }
                        )))
                    )
                }
            }
        )*
    };
}

impl_service!([
    (Investment, "https://invest-public-api.tinkoff.ru:443"),
    (SandboxInvestment, "https://sandbox-invest-public-api.tinkoff.ru:443")
]);