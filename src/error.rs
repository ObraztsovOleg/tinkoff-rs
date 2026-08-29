//! Единый тип ошибок библиотеки и торгового runtime.
//!
//! Публичные методы возвращают [`Result`], поэтому вызывающий код может
//! передавать ошибку выше оператором `?`, не разбирая её на каждом уровне.

use std::time::Duration;

/// Сокращение для результата, в котором ошибка всегда имеет тип [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
/// Ошибки конфигурации, транспорта, API, стратегии, риска и хранилища.
///
/// Варианты [`Error::Status`] и [`Error::Timeout`] особенно важны при работе с
/// заявками: по ним нельзя всегда определить, успел ли сервер принять mutation.
/// Поэтому неоднозначные ошибки обрабатывает `BatchExecutor`, а не общий retry-слой.
pub enum Error {
    #[error("invalid endpoint `{0}`")]
    InvalidEndpoint(String),
    #[error("invalid metadata value for `{0}`")]
    InvalidMetadata(&'static str),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("gRPC transport error: {0}")]
    Transport(#[from] tonic::transport::Error),
    #[error("T-Invest API returned {0}")]
    Status(Box<tonic::Status>),
    #[error("request timed out after {0:?}")]
    Timeout(Duration),
    #[error("stream closed: {0}")]
    StreamClosed(String),
    #[error("stream command channel closed")]
    StreamCommandClosed,
    #[error("capability is not available: {0}")]
    UnsupportedCapability(&'static str),
    #[error("live trading is disabled")]
    LiveTradingDisabled,
    #[error("risk check rejected the intent: {0}")]
    RiskRejected(String),
    #[error("strategy `{strategy}` failed: {message}")]
    Strategy { strategy: String, message: String },
    #[error("storage error: {0}")]
    Storage(String),
    #[error("configuration error: {0}")]
    Configuration(String),
    #[error("decimal conversion failed for units={units}, nano={nano}")]
    InvalidDecimal { units: i64, nano: i32 },
    #[error("invalid protobuf timestamp seconds={seconds}, nanos={nanos}")]
    InvalidTimestamp { seconds: i64, nanos: i32 },
}

impl From<tonic::Status> for Error {
    fn from(status: tonic::Status) -> Self {
        Self::Status(Box::new(status))
    }
}

#[cfg(feature = "postgres")]
impl From<sqlx::Error> for Error {
    fn from(error: sqlx::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

#[cfg(feature = "postgres")]
impl From<sqlx::migrate::MigrateError> for Error {
    fn from(error: sqlx::migrate::MigrateError) -> Self {
        Self::Storage(error.to_string())
    }
}
