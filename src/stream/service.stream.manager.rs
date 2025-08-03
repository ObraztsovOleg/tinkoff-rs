use tonic::{async_trait, Streaming};

use crate::{tcs::{market_data_stream_service_client::MarketDataStreamServiceClient, operations_stream_service_client::OperationsStreamServiceClient, orders_stream_service_client::OrdersStreamServiceClient, MarketDataResponse, MarketDataServerSideStreamRequest, OrderStateStreamRequest, OrderStateStreamResponse, PortfolioStreamRequest, PortfolioStreamResponse, PositionsStreamRequest, PositionsStreamResponse, TradesStreamRequest, TradesStreamResponse}, Investment, TSResult};

pub type ServerTrades = ServiceStreamManager<TradesStreamResponse>;
pub type ServerOrderState = ServiceStreamManager<OrderStateStreamResponse>;
pub type ServerMarketData = ServiceStreamManager<MarketDataResponse>;
pub type ServerPositions = ServiceStreamManager<PositionsStreamResponse>;
pub type ServerPortfolio = ServiceStreamManager<PortfolioStreamResponse>;

pub struct ServiceStreamManager<Res> {
    pub stream: Streaming<Res>
}

#[async_trait]
pub trait ServiceStream<Req, Res>: Sized {
    async fn new_stream(service: Investment, request: Req) -> TSResult<Self>;
}

macro_rules! stream {
    ([$(($request:ty, $response:ty, $stream_client:ident, $method:ident)),*]) => {
        $(
            #[async_trait]
            impl ServiceStream<$request, $response> for ServiceStreamManager<$response> {
                async fn new_stream(service: Investment, request: $request) -> TSResult<Self> {
                    let stream = $stream_client::from(service.into_inner())
                        .$method(request).await?.into_inner();
            
                    Ok(
                        Self {
                            stream: stream
                        }
                    )
                }
            }
        )*
    };
}

stream!([
    (TradesStreamRequest, TradesStreamResponse, OrdersStreamServiceClient, trades_stream),
    (OrderStateStreamRequest, OrderStateStreamResponse, OrdersStreamServiceClient, order_state_stream),
    (MarketDataServerSideStreamRequest, MarketDataResponse, MarketDataStreamServiceClient, market_data_server_side_stream),
    (PositionsStreamRequest, PositionsStreamResponse, OperationsStreamServiceClient, positions_stream),
    (PortfolioStreamRequest, PortfolioStreamResponse, OperationsStreamServiceClient, portfolio_stream)
]);