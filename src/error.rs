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
    /// Соединение, удерживающее эксклюзивный lease счёта, потеряно. Runtime
    /// обязан прекратить заявки; повторный захват lease безопасен, потому что
    /// он не удастся, пока счётом владеет другой процесс.
    #[error("runtime lease lost: {0}")]
    LeaseLost(String),
    #[error("configuration error: {0}")]
    Configuration(String),
    #[error("decimal conversion failed for units={units}, nano={nano}")]
    InvalidDecimal { units: i64, nano: i32 },
    #[error("invalid protobuf timestamp seconds={seconds}, nanos={nanos}")]
    InvalidTimestamp { seconds: i64, nanos: i32 },
}

impl Error {
    /// Возвращает `true`, если ошибка вызвана временной недоступностью сети или API.
    ///
    /// Сюда относятся обрыв соединения (`Transport`, gRPC `Unknown`/`Internal` с
    /// транспортной ошибкой h2), таймауты, отмена запроса по истечении времени,
    /// ограничение частоты и `Unavailable`. Такие ошибки не означают ошибку в
    /// конфигурации или логике: runtime должен дождаться сети и продолжить работу.
    ///
    /// Ошибки авторизации, аргументов, риска, стратегии и хранилища сюда не входят.
    /// `StreamClosed` считается временной, кроме явного отказа сервера в подписке.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Transport(_) | Self::Timeout(_) | Self::StreamCommandClosed => true,
            Self::StreamClosed(message) => !message.contains("rejected"),
            Self::Status(status) => is_transient_code(status.code()),
            _ => false,
        }
    }
}

/// gRPC-коды временной недоступности. `Unknown` и `Internal` tonic выдаёт при
/// обрыве HTTP/2-соединения («transport error», «h2 protocol error»), поэтому
/// они тоже повторяются: после восстановления сети запрос снова будет успешным.
pub(crate) fn is_transient_code(code: tonic::Code) -> bool {
    matches!(
        code,
        tonic::Code::Unavailable
            | tonic::Code::DeadlineExceeded
            | tonic::Code::ResourceExhausted
            | tonic::Code::Cancelled
            | tonic::Code::Aborted
            | tonic::Code::Unknown
            | tonic::Code::Internal
    )
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tonic::{Code, Status};

    use super::Error;

    #[test]
    fn network_failures_are_transient() {
        for code in [
            Code::Unavailable,
            Code::DeadlineExceeded,
            Code::ResourceExhausted,
            Code::Cancelled,
            Code::Aborted,
            Code::Unknown,
            Code::Internal,
        ] {
            assert!(
                Error::from(Status::new(code, "transport error")).is_transient(),
                "{code:?} must be retried"
            );
        }
        assert!(Error::Timeout(Duration::from_secs(60)).is_transient());
        assert!(Error::StreamClosed("market data channel closed".to_owned()).is_transient());
        assert!(Error::StreamCommandClosed.is_transient());
    }

    #[test]
    fn configuration_auth_and_risk_failures_are_not_transient() {
        for code in [
            Code::Unauthenticated,
            Code::PermissionDenied,
            Code::InvalidArgument,
            Code::NotFound,
            Code::FailedPrecondition,
        ] {
            assert!(!Error::from(Status::new(code, "permanent")).is_transient());
        }
        assert!(!Error::RiskRejected("stale".to_owned()).is_transient());
        assert!(!Error::Storage("lease lost".to_owned()).is_transient());
        assert!(!Error::LeaseLost("heartbeat failed".to_owned()).is_transient());
        assert!(!Error::Configuration("bad".to_owned()).is_transient());
        assert!(
            !Error::StreamClosed("candle subscription rejected: uid=x".to_owned()).is_transient()
        );
    }
}
