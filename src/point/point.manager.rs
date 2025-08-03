use crate::{tcs::{instruments_service_client::InstrumentsServiceClient, market_data_service_client::MarketDataServiceClient, operations_service_client::OperationsServiceClient, orders_service_client::OrdersServiceClient, signal_service_client::SignalServiceClient, stop_orders_service_client::StopOrdersServiceClient, users_service_client::UsersServiceClient, AssetRequest, AssetResponse, AssetsRequest, AssetsResponse, BondResponse, BondsResponse, Brand, BrokerReportRequest, BrokerReportResponse, CancelOrderRequest, CancelOrderResponse, CancelStopOrderRequest, CancelStopOrderResponse, CurrenciesResponse, CurrencyResponse, EditFavoritesRequest, EditFavoritesResponse, EtfResponse, EtfsResponse, FilterOptionsRequest, FindInstrumentRequest, FindInstrumentResponse, FutureResponse, FuturesResponse, GetAccountsRequest, GetAccountsResponse, GetAccruedInterestsRequest, GetAccruedInterestsResponse, GetAssetFundamentalsRequest, GetAssetFundamentalsResponse, GetAssetReportsRequest, GetAssetReportsResponse, GetBondCouponsRequest, GetBondCouponsResponse, GetBondEventsRequest, GetBondEventsResponse, GetBrandRequest, GetBrandsRequest, GetBrandsResponse, GetCandlesRequest, GetCandlesResponse, GetClosePricesRequest, GetClosePricesResponse, GetConsensusForecastsRequest, GetConsensusForecastsResponse, GetCountriesRequest, GetCountriesResponse, GetDividendsForeignIssuerRequest, GetDividendsForeignIssuerResponse, GetDividendsRequest, GetDividendsResponse, GetFavoritesRequest, GetFavoritesResponse, GetForecastRequest, GetForecastResponse, GetFuturesMarginRequest, GetFuturesMarginResponse, GetInfoRequest, GetInfoResponse, GetLastPricesRequest, GetLastPricesResponse, GetLastTradesRequest, GetLastTradesResponse, GetMarginAttributesRequest, GetMarginAttributesResponse, GetMaxLotsRequest, GetMaxLotsResponse, GetOperationsByCursorRequest, GetOperationsByCursorResponse, GetOrderBookRequest, GetOrderBookResponse, GetOrderPriceRequest, GetOrderPriceResponse, GetOrderStateRequest, GetOrdersRequest, GetOrdersResponse, GetSignalsRequest, GetSignalsResponse, GetStopOrdersRequest, GetStopOrdersResponse, GetStrategiesRequest, GetStrategiesResponse, GetTechAnalysisRequest, GetTechAnalysisResponse, GetTradingStatusRequest, GetTradingStatusResponse, GetTradingStatusesRequest, GetTradingStatusesResponse, GetUserTariffRequest, GetUserTariffResponse, IndicativesRequest, IndicativesResponse, InstrumentRequest, InstrumentResponse, InstrumentsRequest, OptionResponse, OptionsResponse, OrderState, PortfolioRequest, PortfolioResponse, PositionsRequest, PositionsResponse, PostOrderAsyncRequest, PostOrderAsyncResponse, PostOrderRequest, PostOrderResponse, PostStopOrderRequest, PostStopOrderResponse, ReplaceOrderRequest, ShareResponse, SharesResponse, TradingSchedulesRequest, TradingSchedulesResponse, WithdrawLimitsRequest, WithdrawLimitsResponse}, Investment, TSResult};

#[derive(Clone, Debug)]
pub enum PointRequest {
    Accounts(GetAccountsRequest),
    MarginAttributes(GetMarginAttributesRequest),
    UserTariff(GetUserTariffRequest),
    Info(GetInfoRequest),
    TradingSchedules(TradingSchedulesRequest),
    BondBy(InstrumentRequest),
    Bonds(InstrumentsRequest),
    BondEvents(GetBondEventsRequest),
    BondCoupons(GetBondCouponsRequest),
    CurrencyBy(InstrumentRequest),
    Currencies(InstrumentsRequest),
    EtfBy(InstrumentRequest),
    Etfs(InstrumentsRequest),
    FutureBy(InstrumentRequest),
    Futures(InstrumentsRequest),
    OptionBy(InstrumentRequest),
    Options(InstrumentsRequest),
    OptionsBy(FilterOptionsRequest),
    ShareBy(InstrumentRequest),
    Shares(InstrumentsRequest),
    Indicatives(IndicativesRequest),
    AccruedInterests(GetAccruedInterestsRequest),
    FuturesMargin(GetFuturesMarginRequest),
    InstrumentBy(InstrumentRequest),
    Dividends(GetDividendsRequest),
    AssetBy(AssetRequest),
    Assets(AssetsRequest),
    Favorites(GetFavoritesRequest),
    EditFavorites(EditFavoritesRequest),
    Countries(GetCountriesRequest),
    FindInstrument(FindInstrumentRequest),
    Brands(GetBrandsRequest),
    BrandBy(GetBrandRequest),
    AssetFundamentals(GetAssetFundamentalsRequest),
    AssetReports(GetAssetReportsRequest),
    ConsensusForecasts(GetConsensusForecastsRequest),
    ForecastBy(GetForecastRequest),
    PostOrder(PostOrderRequest),
    PostOrderAsync(PostOrderAsyncRequest),
    CancelOrder(CancelOrderRequest),
    OrderState(GetOrderStateRequest),
    Orders(GetOrdersRequest),
    ReplaceOrder(ReplaceOrderRequest),
    MaxLots(GetMaxLotsRequest),
    OrderPrice(GetOrderPriceRequest),
    Portfolio(PortfolioRequest),
    Positions(PositionsRequest),
    WithdrawLimits(WithdrawLimitsRequest),
    BrokerReport(BrokerReportRequest),
    DividendsForeignIssuer(GetDividendsForeignIssuerRequest),
    OperationsByCursor(GetOperationsByCursorRequest),
    Candles(GetCandlesRequest),
    LastPrices(GetLastPricesRequest),
    OrderBook(GetOrderBookRequest),
    TradingStatus(GetTradingStatusRequest),
    TradingStatuses(GetTradingStatusesRequest),
    LastTrades(GetLastTradesRequest),
    ClosePrices(GetClosePricesRequest),
    TechAnalysis(GetTechAnalysisRequest),
    PostStopOrder(PostStopOrderRequest),
    StopOrders(GetStopOrdersRequest),
    CancelStopOrder(CancelStopOrderRequest),
    Strategies(GetStrategiesRequest),
    Signals(GetSignalsRequest)
}

#[derive(Clone, Debug)]
pub enum PointResponse {
    Accounts(GetAccountsResponse),
    MarginAttributes(GetMarginAttributesResponse),
    UserTariff(GetUserTariffResponse),
    Info(GetInfoResponse),
    TradingSchedules(TradingSchedulesResponse),
    BondBy(BondResponse),
    Bonds(BondsResponse),
    BondEvents(GetBondEventsResponse),
    BondCoupons(GetBondCouponsResponse),
    CurrencyBy(CurrencyResponse),
    Currencies(CurrenciesResponse),
    EtfBy(EtfResponse),
    Etfs(EtfsResponse),
    FutureBy(FutureResponse),
    Futures(FuturesResponse),
    OptionBy(OptionResponse),
    Options(OptionsResponse),
    OptionsBy(OptionsResponse),
    ShareBy(ShareResponse),
    Shares(SharesResponse),
    Indicatives(IndicativesResponse),
    AccruedInterests(GetAccruedInterestsResponse),
    FuturesMargin(GetFuturesMarginResponse),
    InstrumentBy(InstrumentResponse),
    Dividends(GetDividendsResponse),
    AssetBy(AssetResponse),
    Assets(AssetsResponse),
    Favorites(GetFavoritesResponse),
    EditFavorites(EditFavoritesResponse),
    Countries(GetCountriesResponse),
    FindInstrument(FindInstrumentResponse),
    Brands(GetBrandsResponse),
    BrandBy(Brand),
    AssetFundamentals(GetAssetFundamentalsResponse),
    AssetReports(GetAssetReportsResponse),
    ConsensusForecasts(GetConsensusForecastsResponse),
    ForecastBy(GetForecastResponse),
    PostOrder(PostOrderResponse),
    PostOrderAsync(PostOrderAsyncResponse),
    CancelOrder(CancelOrderResponse),
    OrderState(OrderState),
    Orders(GetOrdersResponse),
    ReplaceOrder(PostOrderResponse),
    MaxLots(GetMaxLotsResponse),
    OrderPrice(GetOrderPriceResponse),
    Portfolio(PortfolioResponse),
    Positions(PositionsResponse),
    WithdrawLimits(WithdrawLimitsResponse),
    BrokerReport(BrokerReportResponse),
    DividendsForeignIssuer(GetDividendsForeignIssuerResponse),
    OperationsByCursor(GetOperationsByCursorResponse),
    Candles(GetCandlesResponse),
    LastPrices(GetLastPricesResponse),
    OrderBook(GetOrderBookResponse),
    TradingStatus(GetTradingStatusResponse),
    TradingStatuses(GetTradingStatusesResponse),
    LastTrades(GetLastTradesResponse),
    ClosePrices(GetClosePricesResponse),
    TechAnalysis(GetTechAnalysisResponse),
    PostStopOrder(PostStopOrderResponse),
    StopOrders(GetStopOrdersResponse),
    CancelStopOrder(CancelStopOrderResponse),
    Strategies(GetStrategiesResponse),
    Signals(GetSignalsResponse)
}

impl PointRequest {
    pub async fn request(self, service: Investment) -> TSResult<PointResponse> {
        match self {
            PointRequest::Accounts(req) => Ok(
                PointResponse::Accounts(
                    UsersServiceClient::from(service.into_inner()).get_accounts(req).await?.into_inner()
                )
            ),
            PointRequest::MarginAttributes(req) => Ok(
                PointResponse::MarginAttributes(
                    UsersServiceClient::from(service.into_inner()).get_margin_attributes(req).await?.into_inner()
                )
            ),
            PointRequest::UserTariff(req) => Ok(
                PointResponse::UserTariff(
                    UsersServiceClient::from(service.into_inner()).get_user_tariff(req).await?.into_inner()
                )
            ),
            PointRequest::Info(req) => Ok(
                PointResponse::Info(
                    UsersServiceClient::from(service.into_inner()).get_info(req).await?.into_inner()
                )
            ),
            PointRequest::TradingSchedules(req) => Ok(
                PointResponse::TradingSchedules(
                    InstrumentsServiceClient::from(service.into_inner()).trading_schedules(req).await?.into_inner()
                )
            ),
            PointRequest::BondBy(req) => Ok(
                PointResponse::BondBy(
                    InstrumentsServiceClient::from(service.into_inner()).bond_by(req).await?.into_inner()
                )
            ),
            PointRequest::Bonds(req) => Ok(
                PointResponse::Bonds(
                    InstrumentsServiceClient::from(service.into_inner()).bonds(req).await?.into_inner()
                )
            ),
            PointRequest::BondEvents(req) => Ok(
                PointResponse::BondEvents(
                    InstrumentsServiceClient::from(service.into_inner()).get_bond_events(req).await?.into_inner()
                )
            ),
            PointRequest::BondCoupons(req) => Ok(
                PointResponse::BondCoupons(
                    InstrumentsServiceClient::from(service.into_inner()).get_bond_coupons(req).await?.into_inner()
                )
            ),
            PointRequest::CurrencyBy(req) => Ok(
                PointResponse::CurrencyBy(
                    InstrumentsServiceClient::from(service.into_inner()).currency_by(req).await?.into_inner()
                )
            ),
            PointRequest::Currencies(req) => Ok(
                PointResponse::Currencies(
                    InstrumentsServiceClient::from(service.into_inner()).currencies(req).await?.into_inner()
                )
            ),
            PointRequest::EtfBy(req) => Ok(
                PointResponse::EtfBy(
                    InstrumentsServiceClient::from(service.into_inner()).etf_by(req).await?.into_inner()
                )
            ),
            PointRequest::Etfs(req) => Ok(
                PointResponse::Etfs(
                    InstrumentsServiceClient::from(service.into_inner()).etfs(req).await?.into_inner()
                )
            ),
            PointRequest::FutureBy(req) => Ok(
                PointResponse::FutureBy(
                    InstrumentsServiceClient::from(service.into_inner()).future_by(req).await?.into_inner()
                )
            ),
            PointRequest::Futures(req) => Ok(
                PointResponse::Futures(
                    InstrumentsServiceClient::from(service.into_inner()).futures(req).await?.into_inner()
                )
            ),
            PointRequest::OptionBy(req) => Ok(
                PointResponse::OptionBy(
                    InstrumentsServiceClient::from(service.into_inner()).option_by(req).await?.into_inner()
                )
            ),
            PointRequest::Options(req) => Ok(
                PointResponse::Options(
                    InstrumentsServiceClient::from(service.into_inner()).options(req).await?.into_inner()
                )
            ),
            PointRequest::OptionsBy(req) => Ok(
                PointResponse::OptionsBy(
                    InstrumentsServiceClient::from(service.into_inner()).options_by(req).await?.into_inner()
                )
            ),
            PointRequest::ShareBy(req) => Ok(
                PointResponse::ShareBy(
                    InstrumentsServiceClient::from(service.into_inner()).share_by(req).await?.into_inner()
                )
            ),
            PointRequest::Shares(req) => Ok(
                PointResponse::Shares(
                    InstrumentsServiceClient::from(service.into_inner()).shares(req).await?.into_inner()
                )
            ),
            PointRequest::Indicatives(req) => Ok(
                PointResponse::Indicatives(
                    InstrumentsServiceClient::from(service.into_inner()).indicatives(req).await?.into_inner()
                )
            ),
            PointRequest::AccruedInterests(req) => Ok(
                PointResponse::AccruedInterests(
                    InstrumentsServiceClient::from(service.into_inner()).get_accrued_interests(req).await?.into_inner()
                )
            ),
            PointRequest::FuturesMargin(req) => Ok(
                PointResponse::FuturesMargin(
                    InstrumentsServiceClient::from(service.into_inner()).get_futures_margin(req).await?.into_inner()
                )
            ),
            PointRequest::InstrumentBy(req) => Ok(
                PointResponse::InstrumentBy(
                    InstrumentsServiceClient::from(service.into_inner()).get_instrument_by(req).await?.into_inner()
                )
            ),
            PointRequest::Dividends(req) => Ok(
                PointResponse::Dividends(
                    InstrumentsServiceClient::from(service.into_inner()).get_dividends(req).await?.into_inner()
                )
            ),
            PointRequest::AssetBy(req) => Ok(
                PointResponse::AssetBy(
                    InstrumentsServiceClient::from(service.into_inner()).get_asset_by(req).await?.into_inner()
                )
            ),
            PointRequest::Assets(req) => Ok(
                PointResponse::Assets(
                    InstrumentsServiceClient::from(service.into_inner()).get_assets(req).await?.into_inner()
                )
            ),
            PointRequest::Favorites(req) => Ok(
                PointResponse::Favorites(
                    InstrumentsServiceClient::from(service.into_inner()).get_favorites(req).await?.into_inner()
                )
            ),
            PointRequest::EditFavorites(req) => Ok(
                PointResponse::EditFavorites(
                    InstrumentsServiceClient::from(service.into_inner()).edit_favorites(req).await?.into_inner()
                )
            ),
            PointRequest::Countries(req) => Ok(
                PointResponse::Countries(
                    InstrumentsServiceClient::from(service.into_inner()).get_countries(req).await?.into_inner()
                )
            ),
            PointRequest::FindInstrument(req) => Ok(
                PointResponse::FindInstrument(
                    InstrumentsServiceClient::from(service.into_inner()).find_instrument(req).await?.into_inner()
                )
            ),
            PointRequest::Brands(req) => Ok(
                PointResponse::Brands(
                    InstrumentsServiceClient::from(service.into_inner()).get_brands(req).await?.into_inner()
                )
            ),
            PointRequest::BrandBy(req) => Ok(
                PointResponse::BrandBy(
                    InstrumentsServiceClient::from(service.into_inner()).get_brand_by(req).await?.into_inner()
                )
            ),
            PointRequest::AssetFundamentals(req) => Ok(
                PointResponse::AssetFundamentals(
                    InstrumentsServiceClient::from(service.into_inner()).get_asset_fundamentals(req).await?.into_inner()
                )
            ),
            PointRequest::AssetReports(req) => Ok(
                PointResponse::AssetReports(
                    InstrumentsServiceClient::from(service.into_inner()).get_asset_reports(req).await?.into_inner()
                )
            ),
            PointRequest::ConsensusForecasts(req) => Ok(
                PointResponse::ConsensusForecasts(
                    InstrumentsServiceClient::from(service.into_inner()).get_consensus_forecasts(req).await?.into_inner()
                )
            ),
            PointRequest::ForecastBy(req) => Ok(
                PointResponse::ForecastBy(
                    InstrumentsServiceClient::from(service.into_inner()).get_forecast_by(req).await?.into_inner()
                )
            ),
            PointRequest::PostOrder(req) => Ok(
                PointResponse::PostOrder(
                    OrdersServiceClient::from(service.into_inner()).post_order(req).await?.into_inner()
                )
            ),
            PointRequest::PostOrderAsync(req) => Ok(
                PointResponse::PostOrderAsync(
                    OrdersServiceClient::from(service.into_inner()).post_order_async(req).await?.into_inner()
                )
            ),
            PointRequest::CancelOrder(req) => Ok(
                PointResponse::CancelOrder(
                    OrdersServiceClient::from(service.into_inner()).cancel_order(req).await?.into_inner()
                )
            ),
            PointRequest::OrderState(req) => Ok(
                PointResponse::OrderState(
                    OrdersServiceClient::from(service.into_inner()).get_order_state(req).await?.into_inner()
                )
            ),
            PointRequest::Orders(req) => Ok(
                PointResponse::Orders(
                    OrdersServiceClient::from(service.into_inner()).get_orders(req).await?.into_inner()
                )
            ),
            PointRequest::ReplaceOrder(req) => Ok(
                PointResponse::ReplaceOrder(
                    OrdersServiceClient::from(service.into_inner()).replace_order(req).await?.into_inner()
                )
            ),
            PointRequest::MaxLots(req) => Ok(
                PointResponse::MaxLots(
                    OrdersServiceClient::from(service.into_inner()).get_max_lots(req).await?.into_inner()
                )
            ),
            PointRequest::OrderPrice(req) => Ok(
                PointResponse::OrderPrice(
                    OrdersServiceClient::from(service.into_inner()).get_order_price(req).await?.into_inner()
                )
            ),
            PointRequest::Portfolio(req) => Ok(
                PointResponse::Portfolio(
                    OperationsServiceClient::from(service.into_inner()).get_portfolio(req).await?.into_inner()
                )
            ),
            PointRequest::Positions(req) => Ok(
                PointResponse::Positions(
                    OperationsServiceClient::from(service.into_inner()).get_positions(req).await?.into_inner()
                )
            ),
            PointRequest::WithdrawLimits(req) => Ok(
                PointResponse::WithdrawLimits(
                    OperationsServiceClient::from(service.into_inner()).get_withdraw_limits(req).await?.into_inner()
                )
            ),
            PointRequest::BrokerReport(req) => Ok(
                PointResponse::BrokerReport(
                    OperationsServiceClient::from(service.into_inner()).get_broker_report(req).await?.into_inner()
                )
            ),
            PointRequest::DividendsForeignIssuer(req) => Ok(
                PointResponse::DividendsForeignIssuer(
                    OperationsServiceClient::from(service.into_inner()).get_dividends_foreign_issuer(req).await?.into_inner()
                )
            ),
            PointRequest::OperationsByCursor(req) => Ok(
                PointResponse::OperationsByCursor(
                    OperationsServiceClient::from(service.into_inner()).get_operations_by_cursor(req).await?.into_inner()
                )
            ),
            PointRequest::Candles(req) => Ok(
                PointResponse::Candles(
                    MarketDataServiceClient::from(service.into_inner()).get_candles(req).await?.into_inner()
                )
            ),
            PointRequest::LastPrices(req) => Ok(
                PointResponse::LastPrices(
                    MarketDataServiceClient::from(service.into_inner()).get_last_prices(req).await?.into_inner()
                )
            ),
            PointRequest::OrderBook(req) => Ok(
                PointResponse::OrderBook(
                    MarketDataServiceClient::from(service.into_inner()).get_order_book(req).await?.into_inner()
                )
            ),
            PointRequest::TradingStatus(req) => Ok(
                PointResponse::TradingStatus(
                    MarketDataServiceClient::from(service.into_inner()).get_trading_status(req).await?.into_inner()
                )
            ),
            PointRequest::TradingStatuses(req) => Ok(
                PointResponse::TradingStatuses(
                    MarketDataServiceClient::from(service.into_inner()).get_trading_statuses(req).await?.into_inner()
                )
            ),
            PointRequest::LastTrades(req) => Ok(
                PointResponse::LastTrades(
                    MarketDataServiceClient::from(service.into_inner()).get_last_trades(req).await?.into_inner()
                )
            ),
            PointRequest::ClosePrices(req) => Ok(
                PointResponse::ClosePrices(
                    MarketDataServiceClient::from(service.into_inner()).get_close_prices(req).await?.into_inner()
                )
            ),
            PointRequest::TechAnalysis(req) => Ok(
                PointResponse::TechAnalysis(
                    MarketDataServiceClient::from(service.into_inner()).get_tech_analysis(req).await?.into_inner()
                )
            ),
            PointRequest::PostStopOrder(req) => Ok(
                PointResponse::PostStopOrder(
                    StopOrdersServiceClient::from(service.into_inner()).post_stop_order(req).await?.into_inner()
                )
            ),
            PointRequest::StopOrders(req) => Ok(
                PointResponse::StopOrders(
                    StopOrdersServiceClient::from(service.into_inner()).get_stop_orders(req).await?.into_inner()
                )
            ),
            PointRequest::CancelStopOrder(req) => Ok(
                PointResponse::CancelStopOrder(
                    StopOrdersServiceClient::from(service.into_inner()).cancel_stop_order(req).await?.into_inner()
                )
            ),
            PointRequest::Strategies(req) => Ok(
                PointResponse::Strategies(
                    SignalServiceClient::from(service.into_inner()).get_strategies(req).await?.into_inner()
                )
            ),
            PointRequest::Signals(req) => Ok(
                PointResponse::Signals(
                    SignalServiceClient::from(service.into_inner()).get_signals(req).await?.into_inner()
                )
            ),
        }
    }
}
