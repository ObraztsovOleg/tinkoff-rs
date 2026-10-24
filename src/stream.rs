//! Управляемые gRPC streams с bounded channels и автоматическим reconnect.
//!
//! Market data stream состоит из двух частей:
//!
//! - [`MarketDataStreamHandle`] принимает команды subscribe/unsubscribe/shutdown;
//! - [`MarketDataStream::events`] выдаёт ответы сервера вызывающему коду.
//!
//! Background task хранит registry подписок. После временного разрыва он создаёт
//! соединение заново и повторяет все актуальные subscriptions. Ограниченная
//! ёмкость каналов создаёт backpressure и не позволяет бесконтрольно накапливать
//! сообщения в памяти.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use rand::Rng;
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tonic::Streaming;

use crate::{
    Error, Result,
    api::{MarketDataStreamApiClient, OperationsStreamApiClient, OrdersStreamApiClient},
    domain::{
        AccountId, Candle, CandleInterval, InstrumentId, MarketDataEvent, Price,
        timestamp_to_datetime,
    },
    proto::{
        self, CandleInstrument, CandleSource, InfoInstrument, LastPriceInstrument,
        MarketDataRequest, MarketDataResponse, OperationsStreamRequest, OperationsStreamResponse,
        OrderBookInstrument, OrderBookType, OrderStateStreamRequest, OrderStateStreamResponse,
        PingDelaySettings, PortfolioStreamRequest, PortfolioStreamResponse, PositionsStreamRequest,
        PositionsStreamResponse, SubscribeCandlesRequest, SubscribeInfoRequest,
        SubscribeLastPriceRequest, SubscribeOrderBookRequest, SubscribeTradesRequest,
        SubscriptionAction, TradeInstrument, TradeSourceType, market_data_request::Payload,
    },
};

/// Преобразует raw protobuf payload в broker-neutral [`MarketDataEvent`].
///
/// Подтверждения подписок и пока не поддержанные payload намеренно возвращают
/// `None`: это не ошибка рыночных данных. Аргумент `candle_is_complete` передаёт
/// семантику выбранной подписки в доменную свечу.
pub fn market_event(
    response: MarketDataResponse,
    candle_is_complete: bool,
) -> Result<Option<MarketDataEvent>> {
    use proto::market_data_response::Payload;

    match response.payload {
        Some(Payload::Candle(candle)) => {
            let instrument = if candle.instrument_uid.is_empty() {
                candle.figi.clone()
            } else {
                candle.instrument_uid.clone()
            };
            let interval = subscription_interval(candle.interval)?;
            let time = candle
                .time
                .ok_or_else(|| Error::InvalidArgument("stream candle omitted time".to_owned()))?;
            Ok(Some(MarketDataEvent::Candle(Candle {
                instrument_id: InstrumentId::new(instrument)?,
                interval,
                open: required_stream_price(candle.open, "open")?,
                high: required_stream_price(candle.high, "high")?,
                low: required_stream_price(candle.low, "low")?,
                close: required_stream_price(candle.close, "close")?,
                volume: candle.volume,
                time: timestamp_to_datetime(time)?,
                complete: candle_is_complete,
            })))
        }
        Some(Payload::Ping(ping)) => ping
            .time
            .map(timestamp_to_datetime)
            .transpose()
            .map(|time| time.map(MarketDataEvent::Heartbeat)),
        _ => Ok(None),
    }
}

fn required_stream_price(value: Option<proto::Quotation>, field: &str) -> Result<Price> {
    value
        .map(Price::from_quotation)
        .transpose()?
        .ok_or_else(|| Error::InvalidArgument(format!("stream candle omitted {field} price")))
}

fn subscription_interval(value: i32) -> Result<CandleInterval> {
    use proto::SubscriptionInterval;

    match SubscriptionInterval::try_from(value) {
        Ok(SubscriptionInterval::OneMinute) => Ok(CandleInterval::OneMinute),
        Ok(SubscriptionInterval::FiveMinutes) => Ok(CandleInterval::FiveMinutes),
        Ok(SubscriptionInterval::FifteenMinutes) => Ok(CandleInterval::FifteenMinutes),
        Ok(SubscriptionInterval::OneHour) => Ok(CandleInterval::OneHour),
        Ok(SubscriptionInterval::OneDay) => Ok(CandleInterval::OneDay),
        Ok(SubscriptionInterval::SubscriptionInterval2Min) => Ok(CandleInterval::TwoMinutes),
        Ok(SubscriptionInterval::SubscriptionInterval3Min) => Ok(CandleInterval::ThreeMinutes),
        Ok(SubscriptionInterval::SubscriptionInterval10Min) => Ok(CandleInterval::TenMinutes),
        Ok(SubscriptionInterval::SubscriptionInterval30Min) => Ok(CandleInterval::ThirtyMinutes),
        Ok(SubscriptionInterval::SubscriptionInterval2Hour) => Ok(CandleInterval::TwoHours),
        Ok(SubscriptionInterval::SubscriptionInterval4Hour) => Ok(CandleInterval::FourHours),
        Ok(SubscriptionInterval::Week) => Ok(CandleInterval::OneWeek),
        Ok(SubscriptionInterval::Month) => Ok(CandleInterval::OneMonth),
        Ok(SubscriptionInterval::Unspecified) | Err(_) => Err(Error::InvalidArgument(format!(
            "unsupported stream candle interval {value}"
        ))),
    }
}

#[derive(Clone, Copy, Debug)]
/// Настройки bounded channels, heartbeat и восстановления соединения.
pub struct StreamConfig {
    /// Размер command/event channel; при заполнении producer ждёт consumer.
    pub capacity: usize,
    /// Интервал heartbeat/ping.
    pub ping_delay: Duration,
    /// Начальная задержка перед reconnect.
    pub initial_reconnect_delay: Duration,
    /// Верхняя граница exponential reconnect backoff.
    pub max_reconnect_delay: Duration,
    /// Максимум reconnect-попыток; `None` означает без ограничения.
    pub max_reconnect_attempts: Option<u32>,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            capacity: 256,
            ping_delay: Duration::from_secs(30),
            initial_reconnect_delay: Duration::from_millis(250),
            max_reconnect_delay: Duration::from_secs(30),
            max_reconnect_attempts: None,
        }
    }
}

impl StreamConfig {
    /// Проверяет ёмкость канала и допустимый интервал ping.
    pub fn validate(self) -> Result<Self> {
        if self.capacity == 0 {
            return Err(Error::InvalidArgument(
                "stream capacity must be positive".to_owned(),
            ));
        }
        if !(Duration::from_secs(1)..=Duration::from_secs(180)).contains(&self.ping_delay) {
            return Err(Error::InvalidArgument(
                "stream ping delay must be between 1 and 180 seconds".to_owned(),
            ));
        }
        Ok(self)
    }

    fn ping_millis(self) -> Result<i32> {
        i32::try_from(self.ping_delay.as_millis()).map_err(|_| {
            Error::InvalidArgument("stream ping delay does not fit into milliseconds".to_owned())
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Одна подписка market data stream.
///
/// Расширенные поля вариантов публичны, а методы-конструкторы ниже выставляют
/// безопасные типовые значения.
pub enum MarketDataSubscription {
    /// Поток OHLCV-свечей.
    Candles {
        /// Один или несколько UID инструментов.
        instruments: Vec<InstrumentId>,
        /// Интервал свечей.
        interval: CandleInterval,
        /// Ждать только закрытые свечи.
        waiting_close: bool,
        /// Источник свечей согласно protobuf contract.
        source: CandleSource,
    },
    /// Биржевой стакан заданной глубины.
    OrderBook {
        /// UID инструментов.
        instruments: Vec<InstrumentId>,
        /// Число уровней стакана.
        depth: i32,
        /// Тип стакана согласно API.
        order_book_type: OrderBookType,
    },
    /// Поток обезличенных сделок.
    Trades {
        /// UID инструментов.
        instruments: Vec<InstrumentId>,
        /// Источник сделок.
        source: TradeSourceType,
        /// Запрашивать открытый интерес, когда он доступен.
        with_open_interest: bool,
    },
    /// Поток торгового статуса инструментов.
    Info {
        /// UID инструментов.
        instruments: Vec<InstrumentId>,
    },
    /// Поток цен последних сделок.
    LastPrice {
        /// UID инструментов.
        instruments: Vec<InstrumentId>,
    },
}

impl MarketDataSubscription {
    /// Создаёт candle subscription по закрытым свечам.
    pub fn candles(instruments: Vec<InstrumentId>, interval: CandleInterval) -> Result<Self> {
        validate_instruments(&instruments)?;
        if interval.as_subscription_proto().is_none() {
            return Err(Error::InvalidArgument(
                "sub-second candle intervals are not supported by the gRPC stream".to_owned(),
            ));
        }
        Ok(Self::Candles {
            instruments,
            interval,
            waiting_close: true,
            source: CandleSource::Unspecified,
        })
    }

    /// Создаёт subscription стакана и проверяет поддержанную глубину.
    pub fn order_book(instruments: Vec<InstrumentId>, depth: i32) -> Result<Self> {
        validate_instruments(&instruments)?;
        if !matches!(depth, 1 | 10 | 20 | 30 | 40 | 50) {
            return Err(Error::InvalidArgument(
                "order book depth must be one of 1, 10, 20, 30, 40, 50".to_owned(),
            ));
        }
        Ok(Self::OrderBook {
            instruments,
            depth,
            order_book_type: OrderBookType::OrderbookTypeAll,
        })
    }

    /// Создаёт поток всех обезличенных сделок без open interest.
    pub fn trades(instruments: Vec<InstrumentId>) -> Result<Self> {
        validate_instruments(&instruments)?;
        Ok(Self::Trades {
            instruments,
            source: TradeSourceType::TradeSourceAll,
            with_open_interest: false,
        })
    }

    /// Создаёт поток статусов торговли инструментами.
    pub fn info(instruments: Vec<InstrumentId>) -> Result<Self> {
        validate_instruments(&instruments)?;
        Ok(Self::Info { instruments })
    }

    /// Создаёт поток цен последних сделок.
    pub fn last_price(instruments: Vec<InstrumentId>) -> Result<Self> {
        validate_instruments(&instruments)?;
        Ok(Self::LastPrice { instruments })
    }

    fn key(&self) -> String {
        format!("{self:?}")
    }

    #[allow(deprecated)]
    fn request(&self, action: SubscriptionAction) -> Result<MarketDataRequest> {
        let payload = match self {
            Self::Candles {
                instruments,
                interval,
                waiting_close,
                source,
            } => {
                let interval = interval.as_subscription_proto().ok_or_else(|| {
                    Error::InvalidArgument("unsupported stream candle interval".to_owned())
                })? as i32;
                Payload::SubscribeCandlesRequest(SubscribeCandlesRequest {
                    subscription_action: action as i32,
                    instruments: instruments
                        .iter()
                        .map(|instrument| CandleInstrument {
                            figi: String::new(),
                            interval,
                            instrument_id: instrument.to_string(),
                        })
                        .collect(),
                    waiting_close: *waiting_close,
                    candle_source_type: Some(*source as i32),
                })
            }
            Self::OrderBook {
                instruments,
                depth,
                order_book_type,
            } => Payload::SubscribeOrderBookRequest(SubscribeOrderBookRequest {
                subscription_action: action as i32,
                instruments: instruments
                    .iter()
                    .map(|instrument| OrderBookInstrument {
                        figi: String::new(),
                        depth: *depth,
                        instrument_id: instrument.to_string(),
                        order_book_type: *order_book_type as i32,
                    })
                    .collect(),
            }),
            Self::Trades {
                instruments,
                source,
                with_open_interest,
            } => Payload::SubscribeTradesRequest(SubscribeTradesRequest {
                subscription_action: action as i32,
                instruments: instruments
                    .iter()
                    .map(|instrument| TradeInstrument {
                        figi: String::new(),
                        instrument_id: instrument.to_string(),
                    })
                    .collect(),
                trade_source: *source as i32,
                with_open_interest: *with_open_interest,
            }),
            Self::Info { instruments } => Payload::SubscribeInfoRequest(SubscribeInfoRequest {
                subscription_action: action as i32,
                instruments: instruments
                    .iter()
                    .map(|instrument| InfoInstrument {
                        figi: String::new(),
                        instrument_id: instrument.to_string(),
                    })
                    .collect(),
            }),
            Self::LastPrice { instruments } => {
                Payload::SubscribeLastPriceRequest(SubscribeLastPriceRequest {
                    subscription_action: action as i32,
                    instruments: instruments
                        .iter()
                        .map(|instrument| LastPriceInstrument {
                            figi: String::new(),
                            instrument_id: instrument.to_string(),
                        })
                        .collect(),
                })
            }
        };
        Ok(MarketDataRequest {
            payload: Some(payload),
        })
    }
}

fn validate_instruments(instruments: &[InstrumentId]) -> Result<()> {
    if instruments.is_empty() {
        Err(Error::InvalidArgument(
            "a subscription needs at least one instrument".to_owned(),
        ))
    } else {
        Ok(())
    }
}

enum MarketDataCommand {
    Subscribe(MarketDataSubscription),
    Unsubscribe(MarketDataSubscription),
    Shutdown,
}

#[derive(Clone)]
/// Командный handle background-задачи market data stream.
///
/// Clone handle позволяет менять подписки из другой async-задачи, не передавая
/// туда сам event receiver.
pub struct MarketDataStreamHandle {
    commands: mpsc::Sender<MarketDataCommand>,
    readiness: watch::Receiver<std::result::Result<bool, String>>,
    connections: Arc<AtomicU64>,
}

impl MarketDataStreamHandle {
    /// Число успешно установленных соединений с начала работы потока.
    ///
    /// Рост значения означает, что был reconnect: свечи, закрывшиеся во время
    /// разрыва, поток не доставит, и потребитель должен догрузить их из истории.
    #[must_use]
    pub fn connection_count(&self) -> u64 {
        self.connections.load(Ordering::Acquire)
    }

    /// True only after all candle subscriptions in the current connection are confirmed.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.readiness.has_changed().is_ok() && matches!(*self.readiness.borrow(), Ok(true))
    }

    /// Waits for confirmed subscriptions, failing on rejection, closure or timeout.
    pub async fn wait_ready(&self, timeout: Duration) -> Result<()> {
        let mut receiver = self.readiness.clone();
        tokio::time::timeout(timeout, async move {
            loop {
                if receiver.has_changed().is_err() {
                    return Err(Error::StreamClosed(
                        "subscription readiness channel closed".to_owned(),
                    ));
                }
                match receiver.borrow().clone() {
                    Ok(true) => return Ok(()),
                    Err(message) => return Err(Error::StreamClosed(message)),
                    Ok(false) => {}
                }
                receiver.changed().await.map_err(|_| {
                    Error::StreamClosed("subscription readiness channel closed".to_owned())
                })?;
            }
        })
        .await
        .map_err(|_| Error::Timeout(timeout))?
    }

    /// Добавляет подписку в активное соединение и reconnect registry.
    pub async fn subscribe(&self, subscription: MarketDataSubscription) -> Result<()> {
        self.commands
            .send(MarketDataCommand::Subscribe(subscription))
            .await
            .map_err(|_| Error::StreamCommandClosed)
    }

    /// Удаляет подписку из активного соединения и reconnect registry.
    pub async fn unsubscribe(&self, subscription: MarketDataSubscription) -> Result<()> {
        self.commands
            .send(MarketDataCommand::Unsubscribe(subscription))
            .await
            .map_err(|_| Error::StreamCommandClosed)
    }

    /// Просит background task завершить stream без ожидания сетевой ошибки.
    pub async fn shutdown(&self) -> Result<()> {
        self.commands
            .send(MarketDataCommand::Shutdown)
            .await
            .map_err(|_| Error::StreamCommandClosed)
    }
}

/// Управление market stream и ограниченный поток входящих protobuf-сообщений.
pub struct MarketDataStream {
    /// Команды подписки, отписки и остановки.
    pub handle: MarketDataStreamHandle,
    /// Ответы API и ошибки background-задачи.
    pub events: ReceiverStream<Result<MarketDataResponse>>,
}

/// Запускает background-задачу market stream и немедленно возвращает handles.
///
/// Одинаковые initial subscriptions дедуплицируются по стабильному debug-key.
pub fn market_data_stream(
    client: MarketDataStreamApiClient,
    initial: Vec<MarketDataSubscription>,
    config: StreamConfig,
) -> Result<MarketDataStream> {
    let config = config.validate()?;
    let mut registry = HashMap::new();
    for subscription in initial {
        registry.insert(subscription.key(), subscription);
    }
    let (command_tx, command_rx) = mpsc::channel(config.capacity);
    let (event_tx, event_rx) = mpsc::channel(config.capacity);
    let (ready_tx, ready_rx) = watch::channel(Ok(false));
    let connections = Arc::new(AtomicU64::new(0));
    tokio::spawn(run_market_data_stream(
        client,
        registry,
        command_rx,
        event_tx,
        config,
        ready_tx,
        connections.clone(),
    ));
    Ok(MarketDataStream {
        handle: MarketDataStreamHandle {
            commands: command_tx,
            readiness: ready_rx,
            connections,
        },
        events: ReceiverStream::new(event_rx),
    })
}

async fn run_market_data_stream(
    client: MarketDataStreamApiClient,
    mut registry: HashMap<String, MarketDataSubscription>,
    mut commands: mpsc::Receiver<MarketDataCommand>,
    events: mpsc::Sender<Result<MarketDataResponse>>,
    config: StreamConfig,
    readiness: watch::Sender<std::result::Result<bool, String>>,
    connections: Arc<AtomicU64>,
) {
    // Сервер присылает ping с интервалом `ping_delay`. Тишина в три интервала
    // означает «полуоткрытое» соединение (сон компьютера, смена сети): его нужно
    // заменить, иначе стратегия будет ждать свечей, которые уже не придут.
    let silence_limit = config.ping_delay.saturating_mul(3);
    let mut reconnect_attempt = 0_u32;
    loop {
        let _ = readiness.send_replace(Ok(false));
        let mut pending = candle_subscription_keys(registry.values());
        // Каждый reconnect получает новый request channel. Registry живёт
        // снаружи цикла соединения и поэтому переживает разрыв.
        let (requests_tx, requests_rx) = mpsc::channel(config.capacity);
        let ping_settings = MarketDataRequest {
            payload: Some(Payload::PingSettings(PingDelaySettings {
                ping_delay_ms: config.ping_millis().ok(),
            })),
        };
        if requests_tx.send(ping_settings).await.is_err() {
            return;
        }
        for subscription in registry.values() {
            let Ok(request) = subscription.request(SubscriptionAction::Subscribe) else {
                continue;
            };
            if requests_tx.send(request).await.is_err() {
                return;
            }
        }

        // tonic начинает bidirectional RPC: requests_rx уходит на сервер, а
        // Streaming<MarketDataResponse> читается ниже в select loop.
        let response = client
            .clone()
            .market_data_stream(ReceiverStream::new(requests_rx))
            .await;
        let mut stream = match response {
            Ok(response) => {
                reconnect_attempt = 0;
                connections.fetch_add(1, Ordering::AcqRel);
                let _ = readiness.send_replace(Ok(pending.is_empty()));
                response.into_inner()
            }
            Err(status) => {
                tracing::warn!(code = ?status.code(), "market data connection failed");
                if !is_retryable_status(&status) {
                    let _ = events.send(Err(status.into())).await;
                    return;
                }
                reconnect_attempt = reconnect_attempt.saturating_add(1);
                if reconnect_exhausted(config, reconnect_attempt) {
                    let _ = events.send(Err(status.into())).await;
                    return;
                }
                tokio::time::sleep(reconnect_delay(config, reconnect_attempt)).await;
                continue;
            }
        };

        let mut heartbeat = tokio::time::interval(config.ping_delay);
        let acknowledgement_deadline = tokio::time::sleep(Duration::from_secs(15));
        tokio::pin!(acknowledgement_deadline);
        let silence_deadline = tokio::time::sleep(silence_limit);
        tokio::pin!(silence_deadline);
        let should_reconnect = loop {
            // Один select одновременно обслуживает команды приложения,
            // heartbeat и серверные сообщения, не блокируя остальные ветки.
            tokio::select! {
                () = &mut silence_deadline => {
                    tracing::warn!(silent_seconds = silence_limit.as_secs(), "market data stream is silent; reconnecting");
                    break true;
                }
                () = &mut acknowledgement_deadline, if !pending.is_empty() => {
                    tracing::warn!(pending = pending.len(), "subscription acknowledgement timeout; reconnecting");
                    break true;
                }
                command = commands.recv() => {
                    match command {
                        Some(MarketDataCommand::Subscribe(subscription)) => {
                            let key = subscription.key();
                            pending.extend(candle_subscription_keys(std::iter::once(&subscription)));
                            acknowledgement_deadline.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(15));
                            let _ = readiness.send_replace(Ok(pending.is_empty()));
                            match subscription.request(SubscriptionAction::Subscribe) {
                                Ok(request) => {
                                    if requests_tx.send(request).await.is_err() {
                                        break true;
                                    }
                                    registry.insert(key, subscription);
                                }
                                Err(error) => {
                                    if events.send(Err(error)).await.is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                        Some(MarketDataCommand::Unsubscribe(subscription)) => {
                            match subscription.request(SubscriptionAction::Unsubscribe) {
                                Ok(request) => {
                                    if requests_tx.send(request).await.is_err() {
                                        break true;
                                    }
                                    registry.remove(&subscription.key());
                                    for key in candle_subscription_keys(std::iter::once(&subscription)) { pending.remove(&key); }
                                    let _ = readiness.send_replace(Ok(pending.is_empty()));
                                }
                                Err(error) => {
                                    if events.send(Err(error)).await.is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                        Some(MarketDataCommand::Shutdown) | None => return,
                    }
                }
                _ = heartbeat.tick() => {
                    let ping = MarketDataRequest {
                        payload: Some(Payload::Ping(proto::PingRequest {
                            time: Some(prost_types::Timestamp::from(std::time::SystemTime::now())),
                        })),
                    };
                    if requests_tx.send(ping).await.is_err() {
                        break true;
                    }
                }
                message = stream.message() => {
                    match message {
                        Ok(Some(message)) => {
                            silence_deadline.as_mut().reset(tokio::time::Instant::now() + silence_limit);
                            tracing::debug!(payload = ?message.payload, "market data response");
                            if let Err(error) = confirm_candles(&message, &mut pending) {
                                let _ = readiness.send_replace(Err(error.to_string()));
                                let _ = events.send(Err(error)).await;
                                return;
                            }
                            if matches!(&message.payload, Some(proto::market_data_response::Payload::SubscribeCandlesResponse(_))) {
                                tracing::info!(pending = pending.len(), "candle subscriptions acknowledged");
                            }
                            let _ = readiness.send_replace(Ok(pending.is_empty()));
                            if events.send(Ok(message)).await.is_err() {
                                return;
                            }
                        }
                        Ok(None) => break true,
                        Err(status) if is_retryable_status(&status) => {
                            tracing::warn!(code = ?status.code(), "market data stream reconnecting");
                            break true;
                        },
                        Err(status) => {
                            let _ = events.send(Err(status.into())).await;
                            return;
                        }
                    }
                }
            }
        };

        if should_reconnect {
            let _ = readiness.send_replace(Ok(false));
            reconnect_attempt = reconnect_attempt.saturating_add(1);
            if reconnect_exhausted(config, reconnect_attempt) {
                let _ = events
                    .send(Err(Error::StreamClosed(
                        "market data reconnect attempts exhausted".to_owned(),
                    )))
                    .await;
                return;
            }
            tokio::time::sleep(reconnect_delay(config, reconnect_attempt)).await;
        }
    }
}

type CandleSubscriptionKey = (String, i32, bool);

fn candle_subscription_keys<'a>(
    subscriptions: impl Iterator<Item = &'a MarketDataSubscription>,
) -> HashSet<CandleSubscriptionKey> {
    subscriptions
        .flat_map(|subscription| match subscription {
            MarketDataSubscription::Candles {
                instruments,
                interval,
                waiting_close,
                ..
            } => instruments
                .iter()
                .map(|id| {
                    (
                        id.to_string(),
                        interval
                            .as_subscription_proto()
                            .expect("validated interval") as i32,
                        *waiting_close,
                    )
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect()
}

fn confirm_candles(
    response: &MarketDataResponse,
    pending: &mut HashSet<CandleSubscriptionKey>,
) -> Result<()> {
    if let Some(proto::market_data_response::Payload::SubscribeCandlesResponse(response)) =
        &response.payload
    {
        for subscription in &response.candles_subscriptions {
            if subscription.subscription_action == SubscriptionAction::Unsubscribe as i32 {
                continue;
            }
            if subscription.subscription_status != proto::SubscriptionStatus::Success as i32 {
                return Err(Error::StreamClosed(format!(
                    "candle subscription rejected: uid={} interval={} status={}",
                    subscription.instrument_uid,
                    subscription.interval,
                    subscription.subscription_status
                )));
            }
            pending.remove(&(
                subscription.instrument_uid.clone(),
                subscription.interval,
                subscription.waiting_close,
            ));
        }
    }
    Ok(())
}

/// Открывает server stream состояний заявок с reconnect.
pub fn order_state_stream(
    client: OrdersStreamApiClient,
    accounts: Vec<AccountId>,
    config: StreamConfig,
) -> Result<ReceiverStream<Result<OrderStateStreamResponse>>> {
    let request = OrderStateStreamRequest {
        accounts: account_strings(accounts)?,
        ping_delay_millis: Some(config.validate()?.ping_millis()?),
    };
    resilient_server_stream(
        move || {
            let mut client = client.clone();
            let request = request.clone();
            async move { Ok(client.order_state_stream(request).await?.into_inner()) }
        },
        config,
        "order state",
    )
}

/// Открывает server stream операций с reconnect на временных gRPC-ошибках.
pub fn operations_stream(
    client: OperationsStreamApiClient,
    accounts: Vec<AccountId>,
    config: StreamConfig,
) -> Result<ReceiverStream<Result<OperationsStreamResponse>>> {
    let request = OperationsStreamRequest {
        accounts: account_strings(accounts)?,
        ping_settings: Some(PingDelaySettings {
            ping_delay_ms: Some(config.validate()?.ping_millis()?),
        }),
    };
    resilient_server_stream(
        move || {
            let mut client = client.clone();
            let request = request.clone();
            async move { Ok(client.operations_stream(request).await?.into_inner()) }
        },
        config,
        "operations",
    )
}

/// Открывает server stream снимков/изменений портфеля с reconnect.
pub fn portfolio_stream(
    client: OperationsStreamApiClient,
    accounts: Vec<AccountId>,
    config: StreamConfig,
) -> Result<ReceiverStream<Result<PortfolioStreamResponse>>> {
    let request = PortfolioStreamRequest {
        accounts: account_strings(accounts)?,
        ping_settings: Some(PingDelaySettings {
            ping_delay_ms: Some(config.validate()?.ping_millis()?),
        }),
    };
    resilient_server_stream(
        move || {
            let mut client = client.clone();
            let request = request.clone();
            async move { Ok(client.portfolio_stream(request).await?.into_inner()) }
        },
        config,
        "portfolio",
    )
}

/// Открывает server stream позиций с reconnect.
pub fn positions_stream(
    client: OperationsStreamApiClient,
    accounts: Vec<AccountId>,
    with_initial_positions: bool,
    config: StreamConfig,
) -> Result<ReceiverStream<Result<PositionsStreamResponse>>> {
    let request = PositionsStreamRequest {
        accounts: account_strings(accounts)?,
        with_initial_positions,
        ping_settings: Some(PingDelaySettings {
            ping_delay_ms: Some(config.validate()?.ping_millis()?),
        }),
    };
    resilient_server_stream(
        move || {
            let mut client = client.clone();
            let request = request.clone();
            async move { Ok(client.positions_stream(request).await?.into_inner()) }
        },
        config,
        "positions",
    )
}

fn account_strings(accounts: Vec<AccountId>) -> Result<Vec<String>> {
    if accounts.is_empty() {
        return Err(Error::InvalidArgument(
            "a server stream needs at least one account".to_owned(),
        ));
    }
    Ok(accounts.into_iter().map(AccountId::into_inner).collect())
}

fn resilient_server_stream<T, Connect, ConnectFuture>(
    connect: Connect,
    config: StreamConfig,
    name: &'static str,
) -> Result<ReceiverStream<Result<T>>>
where
    T: Send + 'static,
    Connect: Fn() -> ConnectFuture + Clone + Send + Sync + 'static,
    ConnectFuture: Future<Output = Result<Streaming<T>>> + Send + 'static,
{
    let config = config.validate()?;
    let (sender, receiver) = mpsc::channel(config.capacity);
    // Общий runner используется четырьмя однонаправленными server streams.
    // В отличие от market data им не нужно восстанавливать command registry:
    // исходный request целиком клонируется функцией connect.
    tokio::spawn(async move {
        let mut reconnect_attempt = 0_u32;
        loop {
            let mut stream = match connect().await {
                Ok(stream) => {
                    reconnect_attempt = 0;
                    stream
                }
                Err(error) if error.is_transient() => {
                    tracing::warn!(stream = name, %error, "server stream connection failed; reconnecting");
                    reconnect_attempt = reconnect_attempt.saturating_add(1);
                    if reconnect_exhausted(config, reconnect_attempt) {
                        let _ = sender.send(Err(error)).await;
                        return;
                    }
                    tokio::time::sleep(reconnect_delay(config, reconnect_attempt)).await;
                    continue;
                }
                Err(error) => {
                    let _ = sender.send(Err(error)).await;
                    return;
                }
            };

            loop {
                match stream.message().await {
                    Ok(Some(message)) => {
                        if sender.send(Ok(message)).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => break,
                    Err(status) if is_retryable_status(&status) => break,
                    Err(status) => {
                        let _ = sender.send(Err(status.into())).await;
                        return;
                    }
                }
            }

            reconnect_attempt = reconnect_attempt.saturating_add(1);
            if reconnect_exhausted(config, reconnect_attempt) {
                let _ = sender
                    .send(Err(Error::StreamClosed(format!(
                        "{name} reconnect attempts exhausted"
                    ))))
                    .await;
                return;
            }
            tokio::time::sleep(reconnect_delay(config, reconnect_attempt)).await;
        }
    });
    Ok(ReceiverStream::new(receiver))
}

/// Stream переподключается при любой временной недоступности сети или API,
/// включая обрыв HTTP/2 (`Unknown`/`Internal`), а не завершает поток событий.
fn is_retryable_status(status: &tonic::Status) -> bool {
    crate::error::is_transient_code(status.code())
}

fn reconnect_exhausted(config: StreamConfig, attempt: u32) -> bool {
    config
        .max_reconnect_attempts
        .is_some_and(|maximum| attempt > maximum)
}

fn reconnect_delay(config: StreamConfig, attempt: u32) -> Duration {
    let multiplier = 1_u32 << attempt.saturating_sub(1).min(20);
    let base = config
        .initial_reconnect_delay
        .saturating_mul(multiplier)
        .min(config.max_reconnect_delay);
    let jitter_max = u64::try_from(base.as_millis() / 4).unwrap_or(u64::MAX);
    let jitter = if jitter_max == 0 {
        Duration::ZERO
    } else {
        Duration::from_millis(rand::thread_rng().gen_range(0..=jitter_max))
    };
    base.saturating_add(jitter).min(config.max_reconnect_delay)
}

#[cfg(test)]
mod tests {
    use crate::{
        domain::{CandleInterval, InstrumentId},
        proto,
    };

    use super::MarketDataSubscription;

    #[test]
    fn candle_acknowledgements_require_all_uids_and_reject_errors() {
        let mut pending =
            std::collections::HashSet::from([("a".to_owned(), 9, true), ("b".to_owned(), 9, true)]);
        let response = |uid: &str, status: i32| proto::MarketDataResponse {
            payload: Some(
                proto::market_data_response::Payload::SubscribeCandlesResponse(
                    proto::SubscribeCandlesResponse {
                        candles_subscriptions: vec![proto::CandleSubscription {
                            instrument_uid: uid.to_owned(),
                            interval: 9,
                            waiting_close: true,
                            subscription_status: status,
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                ),
            ),
        };
        super::confirm_candles(
            &response("a", proto::SubscriptionStatus::Success as i32),
            &mut pending,
        )
        .unwrap();
        assert_eq!(pending.len(), 1);
        assert!(super::confirm_candles(&response("b", 6), &mut pending).is_err());
        assert_eq!(pending.len(), 1);
        super::confirm_candles(
            &response("b", proto::SubscriptionStatus::Success as i32),
            &mut pending,
        )
        .unwrap();
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn readiness_is_revoked_on_reconnect_and_wait_is_bounded() {
        let (sender, receiver) = tokio::sync::watch::channel(Ok(true));
        let (commands, _commands) = tokio::sync::mpsc::channel(1);
        let handle = super::MarketDataStreamHandle {
            commands,
            readiness: receiver,
            connections: std::sync::Arc::default(),
        };
        assert!(handle.is_ready());
        let _ = sender.send_replace(Ok(false));
        assert!(!handle.is_ready());
        assert!(
            handle
                .wait_ready(std::time::Duration::from_millis(5))
                .await
                .is_err()
        );
        let _ = sender.send_replace(Ok(true));
        handle
            .wait_ready(std::time::Duration::from_secs(1))
            .await
            .unwrap();
        drop(sender);
        assert!(!handle.is_ready());
    }

    #[test]
    #[allow(deprecated)]
    fn subscription_uses_real_instrument_id() {
        let subscription = MarketDataSubscription::candles(
            vec![InstrumentId::new("BBG004730N88").expect("instrument id")],
            CandleInterval::OneMinute,
        )
        .expect("valid subscription");
        let request = subscription
            .request(crate::proto::SubscriptionAction::Subscribe)
            .expect("valid request");
        let payload = request.payload.expect("payload");
        let proto::market_data_request::Payload::SubscribeCandlesRequest(request) = payload else {
            panic!("unexpected subscription kind");
        };
        assert_eq!(request.instruments[0].instrument_id, "BBG004730N88");
        assert_eq!(request.instruments[0].figi, "");
    }
}
