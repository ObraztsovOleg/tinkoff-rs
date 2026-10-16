//! Потеря сети не должна завершать поток рыночных данных стратегии.
//!
//! Оба сценария воспроизводят то, что видно в журналах живой кампании: обрыв
//! HTTP/2 приходит как gRPC `Unknown: transport error`, а при сне компьютера
//! сервер вообще недоступен. Поток обязан переподключиться и снова стать
//! готовым, не отдавая потребителю ошибку.

use std::{
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures::{Stream, StreamExt};
use secrecy::SecretString;
use tinkoff_rs::{
    ProductionApi,
    domain::{CandleInterval, InstrumentId},
    proto,
    stream::{MarketDataStream, MarketDataSubscription, StreamConfig},
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::timeout,
};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status, transport::Server};

type BoxStream<T> = Pin<Box<dyn Stream<Item = std::result::Result<T, Status>> + Send + 'static>>;

/// Первое соединение подтверждает подписку и обрывается транспортной ошибкой
/// (если `break_first`), последующие подтверждают подписку и остаются открытыми.
struct FlakyMarket {
    calls: Arc<AtomicUsize>,
    break_first: bool,
}

#[tonic::async_trait]
impl proto::market_data_stream_service_server::MarketDataStreamService for FlakyMarket {
    type MarketDataStreamStream = BoxStream<proto::MarketDataResponse>;
    type MarketDataServerSideStreamStream = BoxStream<proto::MarketDataResponse>;

    async fn market_data_stream(
        &self,
        request: Request<tonic::Streaming<proto::MarketDataRequest>>,
    ) -> std::result::Result<Response<Self::MarketDataStreamStream>, Status> {
        let mut requests = request.into_inner();
        let _ping_settings = requests.message().await?;
        let subscribe = requests
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("missing subscription"))?;
        let Some(proto::market_data_request::Payload::SubscribeCandlesRequest(subscription)) =
            subscribe.payload
        else {
            return Err(Status::invalid_argument("unexpected request"));
        };
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let ack = proto::MarketDataResponse {
            payload: Some(
                proto::market_data_response::Payload::SubscribeCandlesResponse(
                    proto::SubscribeCandlesResponse {
                        candles_subscriptions: vec![proto::CandleSubscription {
                            instrument_uid: subscription.instruments[0].instrument_id.clone(),
                            interval: subscription.instruments[0].interval,
                            waiting_close: subscription.waiting_close,
                            subscription_action: proto::SubscriptionAction::Subscribe as i32,
                            subscription_status: proto::SubscriptionStatus::Success as i32,
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                ),
            ),
        };
        let break_now = self.break_first && call == 0;
        let (sender, receiver) = mpsc::channel(4);
        tokio::spawn(async move {
            let _requests = requests;
            if sender.send(Ok(ack)).await.is_err() {
                return;
            }
            if break_now {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let _ = sender.send(Err(Status::unknown("transport error"))).await;
            } else {
                sender.closed().await;
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }

    async fn market_data_server_side_stream(
        &self,
        _request: Request<proto::MarketDataServerSideStreamRequest>,
    ) -> std::result::Result<Response<Self::MarketDataServerSideStreamStream>, Status> {
        Err(Status::unimplemented("not used"))
    }
}

struct RunningServer {
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

fn serve(listener: TcpListener, calls: Arc<AtomicUsize>, break_first: bool) -> RunningServer {
    let (shutdown, shutdown_rx) = oneshot::channel();
    let service =
        proto::market_data_stream_service_server::MarketDataStreamServiceServer::new(FlakyMarket {
            calls,
            break_first,
        });
    let task = tokio::spawn(async move {
        Server::builder()
            .add_service(service)
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                let _ = shutdown_rx.await;
            })
            .await
            .expect("mock server");
    });
    RunningServer { shutdown, task }
}

/// TCP-посредник между клиентом и сервером, которым тест «выключает сеть»:
/// все соединения рвутся, новые отклоняются, пока посредник не запущен снова.
struct Network {
    task: JoinHandle<()>,
    cut: watch::Sender<bool>,
}

fn network(listener: TcpListener, upstream: SocketAddr) -> Network {
    let (cut, cut_rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut stop = cut_rx.clone();
        loop {
            tokio::select! {
                _ = stop.changed() => return,
                accepted = listener.accept() => {
                    let Ok((mut client, _)) = accepted else { return };
                    let mut connection_cut = cut_rx.clone();
                    tokio::spawn(async move {
                        let Ok(mut server) = TcpStream::connect(upstream).await else { return };
                        tokio::select! {
                            _ = connection_cut.changed() => {}
                            _ = tokio::io::copy_bidirectional(&mut client, &mut server) => {}
                        }
                    });
                }
            }
        }
    });
    Network { task, cut }
}

impl Network {
    async fn cut(self) {
        let _ = self.cut.send(true);
        let _ = self.task.await;
    }
}

fn unlimited_reconnects() -> StreamConfig {
    StreamConfig {
        capacity: 8,
        ping_delay: Duration::from_secs(1),
        initial_reconnect_delay: Duration::from_millis(10),
        max_reconnect_delay: Duration::from_millis(100),
        max_reconnect_attempts: None,
    }
}

async fn open_stream(address: SocketAddr) -> MarketDataStream {
    let api = ProductionApi::builder(SecretString::new("token".to_owned()))
        .endpoint(format!("http://{address}"))
        .connect()
        .await
        .expect("API connects");
    tinkoff_rs::stream::market_data_stream(
        api.raw().market_data_stream(),
        vec![
            MarketDataSubscription::candles(
                vec![InstrumentId::new("instrument-uid").expect("instrument id")],
                CandleInterval::OneMinute,
            )
            .expect("subscription"),
        ],
        unlimited_reconnects(),
    )
    .expect("market stream")
}

/// Ни одна ошибка не должна дойти до потребителя: подтверждения допустимы.
async fn assert_no_error_items(stream: &mut MarketDataStream, window: Duration) {
    let deadline = tokio::time::Instant::now() + window;
    while let Ok(item) = tokio::time::timeout_at(deadline, stream.events.next()).await {
        match item {
            Some(Ok(_)) => {}
            Some(Err(error)) => panic!("network failure leaked to the strategy: {error}"),
            None => panic!("market data channel closed during a network failure"),
        }
    }
}

#[tokio::test]
async fn transport_error_reconnects_instead_of_closing_the_stream() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let calls = Arc::new(AtomicUsize::new(0));
    let server = serve(listener, calls.clone(), true);
    let mut stream = open_stream(address).await;

    assert_no_error_items(&mut stream, Duration::from_millis(500)).await;
    assert!(
        calls.load(Ordering::SeqCst) >= 2,
        "stream must reconnect after `Unknown: transport error`"
    );
    stream
        .handle
        .wait_ready(Duration::from_secs(2))
        .await
        .expect("subscriptions are confirmed again after reconnect");
    assert!(stream.handle.connection_count() >= 2);

    stream.handle.shutdown().await.expect("shutdown");
    let _ = server.shutdown.send(());
    let _ = server.task.await;
}

#[tokio::test]
async fn stream_survives_network_outage_and_recovers_when_network_returns() {
    let server_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
    let upstream = server_listener.local_addr().expect("server address");
    let calls = Arc::new(AtomicUsize::new(0));
    let server = serve(server_listener, calls.clone(), false);
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
    let address = proxy_listener.local_addr().expect("proxy address");
    let link = network(proxy_listener, upstream);

    let mut stream = open_stream(address).await;
    stream
        .handle
        .wait_ready(Duration::from_secs(2))
        .await
        .expect("initial acknowledgement");
    assert_eq!(stream.handle.connection_count(), 1);

    // «Сеть пропала»: соединения оборваны, новые подключения отклоняются.
    link.cut().await;
    assert_no_error_items(&mut stream, Duration::from_millis(800)).await;
    assert!(!stream.handle.is_ready());

    // «Сеть вернулась» на том же адресе: поток сам переподписывается.
    let listener = TcpListener::bind(address)
        .await
        .expect("rebind same address");
    let link = network(listener, upstream);
    timeout(
        Duration::from_secs(10),
        stream.handle.wait_ready(Duration::from_secs(10)),
    )
    .await
    .expect("readiness wait finished")
    .expect("subscriptions restored after the outage");
    assert!(stream.handle.connection_count() >= 2);
    assert!(calls.load(Ordering::SeqCst) >= 2);

    stream.handle.shutdown().await.expect("shutdown");
    link.cut().await;
    let _ = server.shutdown.send(());
    let _ = server.task.await;
}
