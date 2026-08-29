//! Повтор временно неуспешных идемпотентных запросов.
//!
//! Этот модуль предназначен для чтений. Торговые mutation нельзя безусловно
//! повторять после timeout: сервер мог принять заявку, хотя клиент не получил
//! ответ. Для них используется reconciliation в `engine::BatchExecutor` с тем же UUID.

use std::{future::Future, time::Duration};

use rand::Rng;
use tonic::{Code, Response};

use crate::{Error, Result};

#[derive(Clone, Copy, Debug)]
/// Параметры exponential backoff для безопасно повторяемых операций.
pub struct RetryPolicy {
    /// Максимальное число попыток, включая первую.
    pub max_attempts: u32,
    /// Задержка перед первым повтором.
    pub initial_delay: Duration,
    /// Верхняя граница задержки между попытками.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(5),
        }
    }
}

/// Выполняет идемпотентную операцию с exponential backoff и небольшим jitter.
///
/// Повторяются только ошибки, которые [`is_retryable`] считает временными.
/// Для `ResourceExhausted` по возможности учитывается `x-ratelimit-reset`.
pub async fn idempotent<T, F, Fut>(policy: RetryPolicy, mut operation: F) -> Result<Response<T>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Response<T>>>,
{
    let attempts = policy.max_attempts.max(1);
    for attempt in 1..=attempts {
        match operation().await {
            Ok(response) => return Ok(response),
            Err(error) if attempt < attempts && is_retryable(&error) => {
                let delay = retry_delay(policy, attempt, &error);
                tracing::warn!(attempt, ?delay, error = %error, "retrying idempotent T-Invest request");
                tokio::time::sleep(delay).await;
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("attempt loop always returns")
}

#[must_use]
/// Возвращает `true` только для временных gRPC-статусов, подходящих для retry.
pub fn is_retryable(error: &Error) -> bool {
    matches!(
        error,
        Error::Status(status)
            if matches!(
                status.code(),
                Code::Unavailable | Code::DeadlineExceeded | Code::ResourceExhausted
            )
    )
}

fn retry_delay(policy: RetryPolicy, attempt: u32, error: &Error) -> Duration {
    if let Error::Status(status) = error {
        if status.code() == Code::ResourceExhausted {
            if let Some(reset) = status
                .metadata()
                .get("x-ratelimit-reset")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
            {
                return Duration::from_secs(reset).min(policy.max_delay);
            }
        }
    }

    let exponent = attempt.saturating_sub(1).min(20);
    let multiplier = 1_u32 << exponent;
    let base = policy
        .initial_delay
        .saturating_mul(multiplier)
        .min(policy.max_delay);
    let jitter_ceiling = u64::try_from(base.as_millis() / 4).unwrap_or(u64::MAX);
    let jitter = if jitter_ceiling == 0 {
        Duration::ZERO
    } else {
        Duration::from_millis(rand::thread_rng().gen_range(0..=jitter_ceiling))
    };
    base.saturating_add(jitter).min(policy.max_delay)
}

#[cfg(test)]
mod tests {
    use tonic::Code;

    use super::is_retryable;
    use crate::Error;

    #[test]
    fn retries_only_transient_statuses() {
        assert!(is_retryable(&Error::from(tonic::Status::new(
            Code::Unavailable,
            "temporary",
        ))));
        assert!(!is_retryable(&Error::from(tonic::Status::new(
            Code::InvalidArgument,
            "permanent",
        ))));
    }
}
