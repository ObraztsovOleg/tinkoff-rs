use std::ops::Deref;

use async_channel::Sender;
use tonic::Streaming;

use crate::{
    bi_stream_request::BiStreamRequest,
    service::{Investment, TSResult},
    tcs::{
        market_data_stream_service_client::MarketDataStreamServiceClient,
        MarketDataRequest, MarketDataResponse,
        SubscriptionAction
    }
};

pub type BiMarketData = BiStreamManager<MarketDataRequest, MarketDataResponse>;

pub struct BiStreamManager<Req, Res> {
    pub sender: Sender<Req>,
    pub stream: Streaming<Res>
}

impl<Req, Res> BiStreamManager<Req, Res> 
where
    Req: BiStreamRequest<Req> 
{
    pub async fn subscribe(&self, mut req: Req) -> TSResult<()> {
        req.set_subscription_action(SubscriptionAction::Subscribe);
        self.sender.send(req).await?;
        
        Ok(())
    }

    pub async fn unsubscribe(&self, mut req: Req) -> TSResult<()> {
        req.set_subscription_action(SubscriptionAction::Unsubscribe);
        self.sender.send(req).await?;

        Ok(())
    }

    pub async fn ping(&self) -> TSResult<()> {
        self.sender.send(Req::ping()).await?;

        Ok(())
    }
}

pub trait BiStream<Req, Res>: Sized {
    async fn new_stream(service: Investment) -> TSResult<BiStreamManager<Req, Res>>;
}

macro_rules! stream {
    ([$(($request:ty, $response:ty, $stream_client:ident, $method:ident)),*]) => {
        $(
            impl BiStream<$request, $response> for BiStreamManager<$request, $response> {
                async fn new_stream(service: Investment) -> TSResult<Self> {
                    let (request, response) = async_channel::unbounded();
                    let ping = <$request>::ping();
                    
                    request.send(ping).await?;
            
                    let stream = $stream_client::from(service.into_inner())
                        .$method(response).await?.into_inner();
                    Ok(
                        Self {
                            sender: request,
                            stream: stream
                        }
                    )
                }
            }
        )*
    };
}

stream!([
    (MarketDataRequest, MarketDataResponse, MarketDataStreamServiceClient, market_data_stream)
]);

