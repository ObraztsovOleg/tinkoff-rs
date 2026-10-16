//! Работа runtime без сети: перезапуск сессии, учёт простоя и корректная остановка.
//!
//! Временная недоступность сети или T-Invest API не является причиной завершать
//! процесс стратегии. Живой цикл пропускает торговое решение, сохраняет свечи
//! в состоянии стратегии и повторяет попытку на следующем событии. Если
//! потерян сам поток данных или сеть пропала во время запуска, сессия целиком
//! создаётся заново с экспоненциальной задержкой до одной минуты.

use std::{
    future::Future,
    time::{Duration as StdDuration, Instant},
};

use rand::Rng;
use tinkoff_rs::{Error, Result};
use tokio::sync::watch;
use tracing::{info, warn};

/// Сессия, проработавшая дольше этого срока, считается здоровой: следующий
/// обрыв снова начинается с короткой задержки.
const HEALTHY_SESSION: StdDuration = StdDuration::from_secs(5 * 60);
/// Как часто повторять предупреждение во время длительного простоя.
const OUTAGE_LOG_INTERVAL: StdDuration = StdDuration::from_secs(60);
/// После такого простоя живая сессия пересоздаётся целиком: новые соединения,
/// подписки и прогрев. Так runtime не зависит от «полуоткрытых» TCP-соединений.
const REBUILD_AFTER: StdDuration = StdDuration::from_secs(3 * 60);

/// Флаг остановки по SIGINT/SIGTERM, общий для всех сессий процесса.
///
/// `watch` хранит последнее значение, поэтому сигнал, пришедший во время
/// запуска или паузы между сессиями, не теряется.
pub(crate) fn shutdown_signal() -> watch::Receiver<bool> {
    let (sender, receiver) = watch::channel(false);
    tokio::spawn(async move {
        wait_for_termination().await;
        let _ = sender.send(true);
    });
    receiver
}

#[cfg(unix)]
async fn wait_for_termination() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut terminate) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
        }
        Err(error) => {
            warn!(%error, "SIGTERM handler unavailable; only Ctrl-C stops the runtime");
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(not(unix))]
async fn wait_for_termination() {
    let _ = tokio::signal::ctrl_c().await;
}

/// Завершается, когда запрошена остановка процесса.
pub(crate) async fn shutdown_requested(shutdown: &mut watch::Receiver<bool>) {
    while !*shutdown.borrow_and_update() {
        if shutdown.changed().await.is_err() {
            // Отправитель живёт до конца процесса; закрытие канала означает выход.
            return;
        }
    }
}

/// Запускает `session` повторно, пока ошибка временная и остановка не запрошена.
///
/// Аккаунтный lease берётся внутри сессии и освобождается при её завершении,
/// поэтому между попытками другой процесс (например, cleanup) может получить
/// счёт, а следующая попытка корректно завершится ошибкой занятого lease.
pub(crate) async fn run_with_reconnect<F, Fut>(
    name: &'static str,
    shutdown: watch::Receiver<bool>,
    mut backoff: Backoff,
    mut session: F,
) -> Result<()>
where
    F: FnMut(watch::Receiver<bool>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let mut waiter = shutdown.clone();
    loop {
        let started = Instant::now();
        match session(shutdown.clone()).await {
            Ok(()) => return Ok(()),
            Err(error) if restartable(&error) => {
                if started.elapsed() >= HEALTHY_SESSION {
                    backoff.reset();
                }
                let delay = backoff.next_delay();
                warn!(
                    runtime = name,
                    %error,
                    retry_in_seconds = delay.as_secs(),
                    "runtime session interrupted; reconnecting without stopping the strategy"
                );
                tokio::select! {
                    () = shutdown_requested(&mut waiter) => {
                        info!("shutdown requested");
                        return Ok(());
                    }
                    () = tokio::time::sleep(delay) => {}
                }
            }
            Err(error) => return Err(error),
        }
    }
}

/// Ошибки, после которых сессию можно безопасно создать заново.
///
/// Потерянный lease тоже сюда относится: заявки уже остановлены завершением
/// сессии, а новый захват lease не удастся, пока счётом владеет другой процесс.
fn restartable(error: &Error) -> bool {
    error.is_transient() || matches!(error, Error::LeaseLost(_))
}

/// Экспоненциальная задержка с небольшим jitter и верхней границей.
#[derive(Debug)]
pub(crate) struct Backoff {
    initial: StdDuration,
    max: StdDuration,
    attempt: u32,
}

impl Backoff {
    /// Задержки между перезапусками сессии runtime: 2 с, 4 с, … до 1 минуты.
    pub(crate) const fn session() -> Self {
        Self::new(StdDuration::from_secs(2), StdDuration::from_secs(60))
    }

    pub(crate) const fn new(initial: StdDuration, max: StdDuration) -> Self {
        Self {
            initial,
            max,
            attempt: 0,
        }
    }

    pub(crate) fn next_delay(&mut self) -> StdDuration {
        let exponent = self.attempt.min(16);
        self.attempt = self.attempt.saturating_add(1);
        let base = self.initial.saturating_mul(1_u32 << exponent).min(self.max);
        let jitter_ceiling = u64::try_from(base.as_millis() / 10).unwrap_or(0);
        let jitter = if jitter_ceiling == 0 {
            StdDuration::ZERO
        } else {
            StdDuration::from_millis(rand::thread_rng().gen_range(0..=jitter_ceiling))
        };
        base.saturating_add(jitter).min(self.max)
    }

    pub(crate) fn reset(&mut self) {
        self.attempt = 0;
    }
}

/// Учёт простоя сети внутри живой сессии: одно предупреждение в начале,
/// напоминание раз в минуту и сообщение о восстановлении.
#[derive(Debug, Default)]
pub(crate) struct NetworkHealth {
    outage_started: Option<Instant>,
    last_report: Option<Instant>,
    failed_events: u64,
}

impl NetworkHealth {
    pub(crate) fn failure(&mut self, context: &str, error: &Error) {
        let now = Instant::now();
        self.failed_events = self.failed_events.saturating_add(1);
        match self.outage_started {
            None => {
                self.outage_started = Some(now);
                self.last_report = Some(now);
                warn!(
                    context,
                    %error,
                    "network or T-Invest API unavailable; decision skipped, runtime keeps running"
                );
            }
            Some(started) => {
                if self
                    .last_report
                    .is_none_or(|last| now.duration_since(last) >= OUTAGE_LOG_INTERVAL)
                {
                    self.last_report = Some(now);
                    warn!(
                        context,
                        %error,
                        outage_seconds = now.duration_since(started).as_secs(),
                        failed_events = self.failed_events,
                        "still waiting for network or T-Invest API"
                    );
                }
            }
        }
    }

    pub(crate) fn success(&mut self) {
        if let Some(started) = self.outage_started.take() {
            info!(
                outage_seconds = started.elapsed().as_secs(),
                failed_events = self.failed_events,
                "network restored; live decisions resumed"
            );
        }
        self.last_report = None;
        self.failed_events = 0;
    }

    /// Простой длится достаточно долго, чтобы пересоздать сессию.
    pub(crate) fn needs_rebuild(&self) -> bool {
        self.outage_started
            .is_some_and(|started| started.elapsed() >= REBUILD_AFTER)
    }

    #[cfg(test)]
    pub(crate) const fn is_degraded(&self) -> bool {
        self.outage_started.is_some()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicU32, Ordering},
        },
        time::Duration as StdDuration,
    };

    use tinkoff_rs::Error;
    use tokio::sync::watch;

    use super::{Backoff, NetworkHealth, run_with_reconnect};

    #[test]
    fn backoff_grows_to_cap_and_resets() {
        let mut backoff = Backoff::new(StdDuration::from_secs(2), StdDuration::from_secs(60));
        let first = backoff.next_delay();
        assert!(first >= StdDuration::from_secs(2) && first <= StdDuration::from_millis(2200));
        for _ in 0..10 {
            assert!(backoff.next_delay() <= StdDuration::from_secs(60));
        }
        assert_eq!(backoff.next_delay(), StdDuration::from_secs(60));
        backoff.reset();
        assert!(backoff.next_delay() <= StdDuration::from_millis(2200));
    }

    #[test]
    fn health_tracks_outage_until_success() {
        let mut health = NetworkHealth::default();
        assert!(!health.is_degraded());
        health.failure("risk-check", &Error::Timeout(StdDuration::from_secs(30)));
        health.failure("risk-check", &Error::Timeout(StdDuration::from_secs(30)));
        assert!(health.is_degraded());
        health.success();
        assert!(!health.is_degraded());
    }

    fn quick() -> Backoff {
        Backoff::new(StdDuration::from_millis(1), StdDuration::from_millis(5))
    }

    #[tokio::test]
    async fn transient_session_failures_reconnect_until_success() {
        let (_sender, shutdown) = watch::channel(false);
        let attempts = Arc::new(AtomicU32::new(0));
        let counter = attempts.clone();
        let result = run_with_reconnect("test", shutdown, quick(), move |_| {
            let counter = counter.clone();
            async move {
                if counter.fetch_add(1, Ordering::SeqCst) < 3 {
                    Err(Error::from(tonic::Status::unknown("transport error")))
                } else {
                    Ok(())
                }
            }
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(attempts.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn lost_lease_restarts_the_session() {
        let (_sender, shutdown) = watch::channel(false);
        let attempts = Arc::new(AtomicU32::new(0));
        let counter = attempts.clone();
        let result = run_with_reconnect("test", shutdown, quick(), move |_| {
            let counter = counter.clone();
            async move {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(Error::LeaseLost("heartbeat failed".to_owned()))
                } else {
                    Ok(())
                }
            }
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn permanent_failure_is_returned_without_retry() {
        let (_sender, shutdown) = watch::channel(false);
        let attempts = Arc::new(AtomicU32::new(0));
        let counter = attempts.clone();
        let result = run_with_reconnect("test", shutdown, quick(), move |_| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err(Error::from(tonic::Status::unauthenticated("bad token")))
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn shutdown_interrupts_reconnect_wait() {
        let (sender, shutdown) = watch::channel(false);
        let attempts = Arc::new(AtomicU32::new(0));
        let counter = attempts.clone();
        let slow = Backoff::new(StdDuration::from_secs(3600), StdDuration::from_secs(3600));
        let task = tokio::spawn(run_with_reconnect("test", shutdown, slow, move |_| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err(Error::Timeout(StdDuration::from_secs(1)))
            }
        }));
        while attempts.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        sender.send(true).unwrap();
        let finished = tokio::time::timeout(StdDuration::from_secs(5), task).await;
        assert!(
            finished
                .expect("shutdown must interrupt the wait")
                .unwrap()
                .is_ok()
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}
