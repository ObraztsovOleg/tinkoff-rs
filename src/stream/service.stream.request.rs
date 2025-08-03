use crate::tcs::{CandleInstrument, CandleSource, InfoInstrument, LastPriceInstrument, MarketDataServerSideStreamRequest, OrderBookInstrument, OrderBookType, OrderStateStreamRequest, PingDelaySettings, PortfolioStreamRequest, PositionsStreamRequest, SubscribeCandlesRequest, SubscribeInfoRequest, SubscribeLastPriceRequest, SubscribeOrderBookRequest, SubscribeTradesRequest, SubscriptionAction, SubscriptionInterval, TradeInstrument, TradeSourceType, TradesStreamRequest};

impl TradesStreamRequest {
    pub fn trades(accounts: Vec<String>) -> Self {
        TradesStreamRequest {
            accounts: accounts,
            ping_delay_ms: Some(120000)
        }
    }
}

impl OrderStateStreamRequest {
    pub fn order_state(accounts: Vec<String>) -> Self {
        OrderStateStreamRequest {
            accounts: accounts,
            ping_delay_ms: Some(120000)
        }
    }
}

impl PositionsStreamRequest {
    pub fn positions(accounts: Vec<String>, with_initial_positions: bool) -> Self {
        PositionsStreamRequest {
            accounts: accounts,
            with_initial_positions: with_initial_positions,
            ping_settings: Some(PingDelaySettings{
                ping_delay_ms: Some(120000)
            })
        }
    }
}

impl PortfolioStreamRequest {
    pub fn portfolio(accounts: Vec<String>) -> Self {
        PortfolioStreamRequest {
            accounts: accounts,
            ping_settings: Some(PingDelaySettings{
                ping_delay_ms: Some(120000)
            })
        }
    }
}

impl MarketDataServerSideStreamRequest {
    pub fn candles(
        figi: &str,
        interval: SubscriptionInterval
    ) -> Self {
        MarketDataServerSideStreamRequest {
            subscribe_candles_request: Some(
                SubscribeCandlesRequest {
                    subscription_action: SubscriptionAction::Subscribe as i32,
                    instruments: vec![CandleInstrument {
                        figi: figi.to_string(),
                        interval: interval as i32, 
                        instrument_id: "figi".to_string(),
                    }],
                    waiting_close: false,
                    candle_source_type: Some(CandleSource::Unspecified as i32),
                }
            ),
            subscribe_order_book_request: None,
            subscribe_trades_request: None,
            subscribe_info_request: None,
            subscribe_last_price_request: None,
            ping_settings: Some(PingDelaySettings {
                ping_delay_ms: Some(120000)
            })
        }
    }
    
    pub fn order_book(
        figi: &str,
        depth: i32,
    ) -> Self {
        MarketDataServerSideStreamRequest {
            subscribe_candles_request: None,
            subscribe_order_book_request: Some(
                SubscribeOrderBookRequest {
                    subscription_action: SubscriptionAction::Subscribe as i32,
                    instruments: vec![OrderBookInstrument {
                        figi: figi.to_string(),
                        depth: depth,
                        instrument_id: "figi".to_string(),
                        order_book_type: OrderBookType::OrderbookTypeAll as i32
                    }]
                }
            ),
            subscribe_trades_request: None,
            subscribe_info_request: None,
            subscribe_last_price_request: None,
            ping_settings: Some(PingDelaySettings {
                ping_delay_ms: Some(120000)
            })
        }
    }

    pub fn trades(
        figi: &str,
    ) -> Self {
        MarketDataServerSideStreamRequest {
            subscribe_candles_request: None,
            subscribe_order_book_request: None,
            subscribe_trades_request: Some(
                SubscribeTradesRequest {
                    subscription_action: SubscriptionAction::Subscribe as i32,
                    instruments: vec![TradeInstrument {
                        figi: figi.to_string(),
                        instrument_id: "figi".to_string(),
                    }],
                    trade_source: TradeSourceType::TradeSourceAll as i32
                }
            ),
            subscribe_info_request: None,
            subscribe_last_price_request: None,
            ping_settings: Some(PingDelaySettings {
                ping_delay_ms: Some(120000)
            })
        }
    }

    pub fn info(
        figi: &str,
    ) -> Self {
        MarketDataServerSideStreamRequest {
            subscribe_candles_request: None,
            subscribe_order_book_request: None,
            subscribe_trades_request: None,
            subscribe_info_request: Some(
                SubscribeInfoRequest {
                    subscription_action: SubscriptionAction::Subscribe as i32,
                    instruments: vec![InfoInstrument {
                        figi: figi.to_string(),
                        instrument_id: "figi".to_string(),
                    }],
                }
            ),
            subscribe_last_price_request: None,
            ping_settings: Some(PingDelaySettings {
                ping_delay_ms: Some(120000)
            })
        }
    }

    pub fn last_price(
        figi: &str,
    ) -> Self {
        MarketDataServerSideStreamRequest {
            subscribe_candles_request: None,
            subscribe_order_book_request: None,
            subscribe_trades_request: None,
            subscribe_info_request: None,
            subscribe_last_price_request: Some(
                SubscribeLastPriceRequest {
                    subscription_action: SubscriptionAction::Subscribe as i32,
                    instruments: vec![LastPriceInstrument {
                        figi: figi.to_string(),
                        instrument_id: "figi".to_string(),
                    }],
                }
            ),
            ping_settings: Some(PingDelaySettings {
                ping_delay_ms: Some(120000)
            })
        }
    }
}

