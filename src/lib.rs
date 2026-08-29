//! Типизированный SDK для T-Invest API и portfolio-target runtime для Sandbox.
//!
//! # Как читать проект
//!
//! Основной поток данных устроен так:
//!
//! ```text
//! T-Invest gRPC
//!     -> api (соединение и generated clients)
//!     -> broker (удобные capability-интерфейсы)
//!     -> stream (котировки и переподключение)
//!     -> engine::PortfolioEngine
//!     -> engine::TargetPortfolio
//!     -> engine::TargetPlanner + RiskManager
//!     -> engine::BatchExecutor
//!     -> broker::Trading
//!     -> T-Invest gRPC
//! ```
//!
//! Модуль [`domain`] находится в центре архитектуры: стратегии и исполнитель
//! работают с его типами, а не с protobuf. Готовый торговый runtime намеренно
//! поддерживает только T-Invest Sandbox и запускает один ансамбль из одной или
//! нескольких стратегий на счёт.
//!
//! # Безопасное разделение контуров
//!
//! Sandbox и production представлены разными Rust-типами. [`broker::ProductionBroker`]
//! даёт production-доступ только для чтения. Чтобы получить реализацию
//! [`broker::Trading`], вызывающий код должен отдельно создать
//! [`broker::LiveTradingPermit`] и [`broker::ProductionTrading`]. Это делает
//! случайную отправку реальной заявки сложнее уже на этапе компиляции.
//!
//! Для знакомства начните с `examples/portfolio-strategy-demo.rs`: пример не открывает
//! соединение и не отправляет заявки.

pub mod api;
pub mod broker;
pub mod domain;
pub mod error;
pub mod proto;
pub mod response;
pub mod retry;
pub mod stream;

#[cfg(feature = "engine")]
pub mod engine;

#[cfg(feature = "engine")]
pub mod storage;

#[cfg(feature = "strategies")]
pub mod strategies;
mod transport;

pub use api::{
    ProductionApi, ProductionApiBuilder, ProductionRawApi, SandboxApi, SandboxApiBuilder,
    SandboxRawApi,
};
pub use error::{Error, Result};
pub use response::{ApiMetadata, ApiResponse, RateLimit};

/// Официальный gRPC endpoint реального торгового контура.
pub const PRODUCTION_ENDPOINT: &str = "https://invest-public-api.tbank.ru:443";
/// Официальный gRPC endpoint изолированной песочницы.
pub const SANDBOX_ENDPOINT: &str = "https://sandbox-invest-public-api.tbank.ru:443";
