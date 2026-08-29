//! Внутренний транспортный слой поверх `tonic`.
//!
//! Он создаёт TLS channel и автоматически добавляет к каждому unary/stream
//! запросу авторизацию, имя приложения и уникальный tracking id. Остальные
//! модули поэтому работают с уже аутентифицированным каналом.

use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use tonic::{
    Request, Status,
    metadata::{Ascii, MetadataValue},
    service::{Interceptor, interceptor::InterceptedService},
    transport::{Channel, ClientTlsConfig, Endpoint},
};
use uuid::Uuid;

use crate::{Error, Result};

#[doc(hidden)]
/// gRPC channel, к которому уже подключён [`AuthInterceptor`].
pub type AuthenticatedChannel = InterceptedService<Channel, AuthInterceptor>;

#[derive(Clone, Debug)]
pub(crate) struct TransportConfig {
    pub endpoint: String,
    pub app_name: String,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
}

#[derive(Clone)]
#[doc(hidden)]
/// Добавляет обязательную metadata к каждому исходящему gRPC-запросу.
pub struct AuthInterceptor {
    authorization: MetadataValue<Ascii>,
    app_name: MetadataValue<Ascii>,
}

impl AuthInterceptor {
    fn new(token: &SecretString, app_name: &str) -> Result<Self> {
        let authorization = format!("Bearer {}", token.expose_secret())
            .parse()
            .map_err(|_| Error::InvalidMetadata("authorization"))?;
        let app_name = app_name
            .parse()
            .map_err(|_| Error::InvalidMetadata("x-app-name"))?;

        Ok(Self {
            authorization,
            app_name,
        })
    }
}

impl Interceptor for AuthInterceptor {
    fn call(&mut self, mut request: Request<()>) -> std::result::Result<Request<()>, Status> {
        // Токен хранится в SecretString до построения interceptor и не выводится
        // через Debug. Здесь находится единственная точка добавления Bearer.
        request
            .metadata_mut()
            .insert("authorization", self.authorization.clone());
        request
            .metadata_mut()
            .insert("x-app-name", self.app_name.clone());

        // Новый client-side tracking id помогает сопоставить конкретный запрос
        // с логами; серверный id ответа дополнительно читается в ApiMetadata.
        let tracking_id = Uuid::new_v4()
            .to_string()
            .parse()
            .map_err(|_| Status::internal("failed to encode tracking id"))?;
        request.metadata_mut().insert("x-tracking-id", tracking_id);
        Ok(request)
    }
}

pub(crate) async fn connect(
    token: &SecretString,
    config: &TransportConfig,
) -> Result<AuthenticatedChannel> {
    // Endpoint также задаёт общий timeout каждого запроса. HTTPS использует
    // системное хранилище доверенных корневых сертификатов.
    let endpoint = Endpoint::from_shared(config.endpoint.clone())
        .map_err(|_| Error::InvalidEndpoint(config.endpoint.clone()))?
        .connect_timeout(config.connect_timeout)
        .timeout(config.request_timeout);
    let endpoint = if config.endpoint.starts_with("https://") {
        endpoint.tls_config(ClientTlsConfig::new().with_native_roots())?
    } else {
        endpoint
    };
    let channel = endpoint.connect().await?;
    let interceptor = AuthInterceptor::new(token, &config.app_name)?;
    Ok(InterceptedService::new(channel, interceptor))
}
