use std::{pin::Pin, sync::Arc, time::Duration};

use futures::{Stream, StreamExt};
use secrecy::SecretString;
use tinkoff_rs::{
    ProductionApi,
    domain::{AccountId, InstrumentId},
    proto,
    stream::{
        MarketDataSubscription, StreamConfig, operations_stream, order_state_stream,
        portfolio_stream, positions_stream,
    },
};
use tokio::{
    net::TcpListener,
    sync::oneshot,
    time::{sleep, timeout},
};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status, transport::Server};

type BoxStream<T> = Pin<Box<dyn Stream<Item = std::result::Result<T, Status>> + Send + 'static>>;

fn reconnecting<T>(counter: &std::sync::atomic::AtomicUsize) -> BoxStream<T>
where
    T: Default + Send + 'static,
{
    let call = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    if call == 0 {
        Box::pin(tokio_stream::iter(vec![
            Ok(T::default()),
            Err(Status::unavailable("force reconnect")),
        ]))
    } else {
        Box::pin(tokio_stream::once(Ok(T::default())).chain(tokio_stream::pending()))
    }
}

#[derive(Default)]
struct MockMarketStream {
    calls: std::sync::atomic::AtomicUsize,
    restored_subscriptions: std::sync::atomic::AtomicUsize,
}

#[tonic::async_trait]
impl proto::market_data_stream_service_server::MarketDataStreamService for MockMarketStream {
    type MarketDataStreamStream = BoxStream<proto::MarketDataResponse>;
    type MarketDataServerSideStreamStream = BoxStream<proto::MarketDataResponse>;

    async fn market_data_stream(
        &self,
        request: Request<tonic::Streaming<proto::MarketDataRequest>>,
    ) -> std::result::Result<Response<Self::MarketDataStreamStream>, Status> {
        let mut requests = request.into_inner();
        let first = requests
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("market stream omitted ping configuration"))?;
        assert!(matches!(
            first.payload,
            Some(proto::market_data_request::Payload::PingSettings(_))
        ));
        let second = requests
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("market stream omitted subscription"))?;
        let Some(proto::market_data_request::Payload::SubscribeInfoRequest(subscription)) =
            second.payload
        else {
            return Err(Status::invalid_argument("unexpected market subscription"));
        };
        assert_eq!(subscription.instruments[0].instrument_id, "instrument-uid");
        self.restored_subscriptions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Response::new(reconnecting(&self.calls)))
    }

    async fn market_data_server_side_stream(
        &self,
        _request: Request<proto::MarketDataServerSideStreamRequest>,
    ) -> std::result::Result<Response<Self::MarketDataServerSideStreamStream>, Status> {
        Err(Status::unimplemented("not used"))
    }
}

#[derive(Default)]
struct MockOrdersStream {
    order_calls: std::sync::atomic::AtomicUsize,
}

#[tonic::async_trait]
impl proto::orders_stream_service_server::OrdersStreamService for MockOrdersStream {
    type TradesStreamStream = BoxStream<proto::TradesStreamResponse>;
    type OrderStateStreamStream = BoxStream<proto::OrderStateStreamResponse>;

    async fn trades_stream(
        &self,
        _request: Request<proto::TradesStreamRequest>,
    ) -> std::result::Result<Response<Self::TradesStreamStream>, Status> {
        Err(Status::unimplemented("not used"))
    }

    async fn order_state_stream(
        &self,
        request: Request<proto::OrderStateStreamRequest>,
    ) -> std::result::Result<Response<Self::OrderStateStreamStream>, Status> {
        assert_eq!(request.into_inner().accounts, ["account"]);
        Ok(Response::new(reconnecting(&self.order_calls)))
    }
}

#[derive(Default)]
#[allow(clippy::struct_field_names)]
struct MockOperationsStream {
    portfolio_reconnects: std::sync::atomic::AtomicUsize,
    positions_reconnects: std::sync::atomic::AtomicUsize,
    operations_reconnects: std::sync::atomic::AtomicUsize,
}

#[tonic::async_trait]
impl proto::operations_stream_service_server::OperationsStreamService for MockOperationsStream {
    type PortfolioStreamStream = BoxStream<proto::PortfolioStreamResponse>;
    type PositionsStreamStream = BoxStream<proto::PositionsStreamResponse>;
    type OperationsStreamStream = BoxStream<proto::OperationsStreamResponse>;

    async fn portfolio_stream(
        &self,
        request: Request<proto::PortfolioStreamRequest>,
    ) -> std::result::Result<Response<Self::PortfolioStreamStream>, Status> {
        assert_eq!(request.into_inner().accounts, ["account"]);
        Ok(Response::new(reconnecting(&self.portfolio_reconnects)))
    }

    async fn positions_stream(
        &self,
        request: Request<proto::PositionsStreamRequest>,
    ) -> std::result::Result<Response<Self::PositionsStreamStream>, Status> {
        let request = request.into_inner();
        assert_eq!(request.accounts, ["account"]);
        assert!(request.with_initial_positions);
        Ok(Response::new(reconnecting(&self.positions_reconnects)))
    }

    async fn operations_stream(
        &self,
        request: Request<proto::OperationsStreamRequest>,
    ) -> std::result::Result<Response<Self::OperationsStreamStream>, Status> {
        assert_eq!(request.into_inner().accounts, ["account"]);
        Ok(Response::new(reconnecting(&self.operations_reconnects)))
    }
}

fn test_config() -> StreamConfig {
    StreamConfig {
        capacity: 8,
        ping_delay: Duration::from_secs(1),
        initial_reconnect_delay: Duration::from_millis(1),
        max_reconnect_delay: Duration::from_millis(5),
        max_reconnect_attempts: Some(5),
    }
}

async fn receive_two<T>(stream: &mut ReceiverStream<tinkoff_rs::Result<T>>) {
    for _ in 0..2 {
        timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("stream did not reconnect in time")
            .expect("stream ended")
            .expect("stream item");
    }
}

#[tokio::test]
async fn restores_market_orders_portfolio_positions_and_operations_streams() {
    let market = Arc::new(MockMarketStream::default());
    let orders = Arc::new(MockOrdersStream::default());
    let operations = Arc::new(MockOperationsStream::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let address = listener.local_addr().expect("mock address");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = Server::builder()
        .add_service(
            proto::market_data_stream_service_server::MarketDataStreamServiceServer::from_arc(
                market.clone(),
            ),
        )
        .add_service(
            proto::orders_stream_service_server::OrdersStreamServiceServer::from_arc(
                orders.clone(),
            ),
        )
        .add_service(
            proto::operations_stream_service_server::OperationsStreamServiceServer::from_arc(
                operations.clone(),
            ),
        )
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
            let _ = shutdown_rx.await;
        });
    let server_task = tokio::spawn(server);
    let api = ProductionApi::builder(SecretString::new("token".to_owned()))
        .endpoint(format!("http://{address}"))
        .connect()
        .await
        .expect("API connects");
    let raw = api.raw();
    let account = AccountId::new("account").expect("account id");

    let mut market_stream = tinkoff_rs::stream::market_data_stream(
        raw.market_data_stream(),
        vec![
            MarketDataSubscription::info(vec![
                InstrumentId::new("instrument-uid").expect("instrument id"),
            ])
            .expect("subscription"),
        ],
        test_config(),
    )
    .expect("market stream");
    for _ in 0..2 {
        timeout(Duration::from_secs(2), market_stream.events.next())
            .await
            .expect("market stream did not reconnect")
            .expect("market stream ended")
            .expect("market event");
    }

    let mut order_stream =
        order_state_stream(raw.orders_stream(), vec![account.clone()], test_config())
            .expect("orders stream");
    receive_two(&mut order_stream).await;
    let mut portfolio = portfolio_stream(
        raw.operations_stream(),
        vec![account.clone()],
        test_config(),
    )
    .expect("portfolio stream");
    receive_two(&mut portfolio).await;
    let mut positions = positions_stream(
        raw.operations_stream(),
        vec![account.clone()],
        true,
        test_config(),
    )
    .expect("positions stream");
    receive_two(&mut positions).await;
    let mut operation = operations_stream(raw.operations_stream(), vec![account], test_config())
        .expect("operations stream");
    receive_two(&mut operation).await;

    sleep(Duration::from_millis(10)).await;
    assert!(market.calls.load(std::sync::atomic::Ordering::SeqCst) >= 2);
    assert!(
        market
            .restored_subscriptions
            .load(std::sync::atomic::Ordering::SeqCst)
            >= 2
    );
    assert!(orders.order_calls.load(std::sync::atomic::Ordering::SeqCst) >= 2);
    assert!(
        operations
            .portfolio_reconnects
            .load(std::sync::atomic::Ordering::SeqCst)
            >= 2
    );
    assert!(
        operations
            .positions_reconnects
            .load(std::sync::atomic::Ordering::SeqCst)
            >= 2
    );
    assert!(
        operations
            .operations_reconnects
            .load(std::sync::atomic::Ordering::SeqCst)
            >= 2
    );

    market_stream.handle.shutdown().await.expect("shutdown");
    drop(order_stream);
    drop(portfolio);
    drop(positions);
    drop(operation);
    let _ = shutdown_tx.send(());
    server_task.abort();
    let _ = server_task.await;
}
