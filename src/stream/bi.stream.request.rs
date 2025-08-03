use prost_types::Timestamp;

use crate::tcs::{
    market_data_request::Payload, CandleInstrument, CandleSource, GetMySubscriptions, InfoInstrument, LastPriceInstrument, MarketDataRequest, OrderBookInstrument, OrderBookType, PingRequest, SubscribeCandlesRequest, SubscribeInfoRequest, SubscribeLastPriceRequest, SubscribeOrderBookRequest, SubscribeTradesRequest, SubscriptionAction, SubscriptionInterval, TradeInstrument, TradeSourceType
};


pub trait BiStreamRequest<Req> {
    fn set_subscription_action(&mut self, action: SubscriptionAction);
    fn ping() -> Req;
}

impl BiStreamRequest<MarketDataRequest> for MarketDataRequest {
    fn set_subscription_action(&mut self, action: SubscriptionAction) {
        if let Some(payload) = &mut self.payload {
            match payload {
                Payload::SubscribeCandlesRequest(candles_request) => {
                    candles_request.subscription_action = action as i32;
                }
                Payload::SubscribeOrderBookRequest(order_book_request) => {
                    order_book_request.subscription_action = action as i32;
                }
                Payload::SubscribeTradesRequest(trades_request) => {
                    trades_request.subscription_action = action as i32;
                }
                Payload::SubscribeInfoRequest(info_request) => {
                    info_request.subscription_action = action as i32;
                }
                Payload::SubscribeLastPriceRequest(last_price_request) => {
                    last_price_request.subscription_action = action as i32;
                }
                _ => {
                    // Игнорируем остальные варианты
                }
            }
        }
    }

    fn ping() -> Self {
        MarketDataRequest {
            payload: Some(Payload::Ping(
                PingRequest {
                    time: Some(Timestamp::from(std::time::SystemTime::now())),
                }
            ))
        }
    }
}

impl MarketDataRequest {
    pub fn candles(
        figi: &str,
        interval: SubscriptionInterval,
    ) -> Self {
        MarketDataRequest {
            payload: Some(Payload::SubscribeCandlesRequest(
                SubscribeCandlesRequest {
                    subscription_action: SubscriptionAction::Unspecified as i32,
                    instruments: vec![CandleInstrument {
                        figi: figi.to_string(),
                        interval: interval as i32, 
                        instrument_id: "figi".to_string(),
                    }],
                    waiting_close: false,
                    candle_source_type: Some(CandleSource::Unspecified as i32), 
                }
            ))
        }
    }

    pub fn order_book(
        figi: String,
        depth: i32,
    ) -> Self {
        MarketDataRequest {
            payload: Some(Payload::SubscribeOrderBookRequest(
                SubscribeOrderBookRequest {
                    subscription_action: SubscriptionAction::Unspecified as i32,
                    instruments: vec![OrderBookInstrument {
                        figi: figi.to_string(),
                        depth: depth,
                        instrument_id: "figi".to_string(),
                        order_book_type: OrderBookType::OrderbookTypeAll as i32
                    }]
                }
            ))
        }
    }

    pub fn trades(
        figi: String,
    ) -> Self {
        MarketDataRequest {
            payload: Some(Payload::SubscribeTradesRequest(
                SubscribeTradesRequest {
                    subscription_action: SubscriptionAction::Unspecified as i32,
                    instruments: vec![TradeInstrument {
                        figi: figi.to_string(),
                        instrument_id: "figi".to_string(),
                    }],
                    trade_source: TradeSourceType::TradeSourceAll as i32
                }
            ))
        }
    }

    pub fn info(
        figi: &str,
    ) -> Self {
        MarketDataRequest {
            payload: Some(Payload::SubscribeInfoRequest(
                SubscribeInfoRequest {
                    subscription_action: SubscriptionAction::Unspecified as i32,
                    instruments: vec![InfoInstrument {
                        figi: figi.to_string(),
                        instrument_id: "figi".to_string(),
                    }],
                }
            ))
        }
    }

    pub fn last_price(
        figi: String,
    ) -> Self {
        MarketDataRequest {
            payload: Some(Payload::SubscribeLastPriceRequest(
                SubscribeLastPriceRequest {
                    subscription_action: SubscriptionAction::Unspecified as i32,
                    instruments: vec![LastPriceInstrument {
                        figi: figi.to_string(),
                        instrument_id: "figi".to_string(),
                    }],
                }
            ))
        }
    }

    pub fn my_subscriptions() -> Self {
        MarketDataRequest {
            payload: Some(Payload::GetMySubscriptions(
                GetMySubscriptions {}
            ))
        }
    }
}
