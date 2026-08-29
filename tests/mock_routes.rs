use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use secrecy::SecretString;
use tinkoff_rs::{
    PRODUCTION_ENDPOINT, ProductionApi, SANDBOX_ENDPOINT, SandboxApi,
    broker::{Accounts, ProductionBroker, SandboxBroker},
    proto,
};
use tokio::{net::TcpListener, sync::oneshot};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Response, Status, transport::Server};

macro_rules! unimplemented_rpc {
    ($method:ident, $request:ty, $response:ty) => {
        fn $method<'life0, 'async_trait>(
            &'life0 self,
            _request: Request<$request>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = std::result::Result<Response<$response>, Status>>
                    + Send
                    + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move { Err(Status::unimplemented(stringify!($method))) })
        }
    };
}

struct MockUsers {
    calls: Arc<AtomicUsize>,
}

#[tonic::async_trait]
impl proto::users_service_server::UsersService for MockUsers {
    async fn get_accounts(
        &self,
        request: Request<proto::GetAccountsRequest>,
    ) -> std::result::Result<Response<proto::GetAccountsResponse>, Status> {
        assert!(request.metadata().contains_key("authorization"));
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut response = Response::new(proto::GetAccountsResponse {
            accounts: vec![proto::Account {
                id: "production-account".to_owned(),
                name: "production route".to_owned(),
                ..proto::Account::default()
            }],
        });
        response.metadata_mut().insert(
            "x-tracking-id",
            "production-users".parse().expect("metadata"),
        );
        Ok(response)
    }

    unimplemented_rpc!(
        get_margin_attributes,
        proto::GetMarginAttributesRequest,
        proto::GetMarginAttributesResponse
    );
    unimplemented_rpc!(
        get_user_tariff,
        proto::GetUserTariffRequest,
        proto::GetUserTariffResponse
    );
    unimplemented_rpc!(get_info, proto::GetInfoRequest, proto::GetInfoResponse);
    unimplemented_rpc!(
        get_bank_accounts,
        proto::GetBankAccountsRequest,
        proto::GetBankAccountsResponse
    );
    unimplemented_rpc!(
        currency_transfer,
        proto::CurrencyTransferRequest,
        proto::CurrencyTransferResponse
    );
    unimplemented_rpc!(pay_in, proto::PayInRequest, proto::PayInResponse);
    unimplemented_rpc!(
        get_account_values,
        proto::GetAccountValuesRequest,
        proto::GetAccountValuesResponse
    );
}

struct MockSandbox {
    calls: Arc<AtomicUsize>,
}

#[tonic::async_trait]
impl proto::sandbox_service_server::SandboxService for MockSandbox {
    async fn get_sandbox_accounts(
        &self,
        request: Request<proto::GetAccountsRequest>,
    ) -> std::result::Result<Response<proto::GetAccountsResponse>, Status> {
        assert!(request.metadata().contains_key("authorization"));
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut response = Response::new(proto::GetAccountsResponse {
            accounts: vec![proto::Account {
                id: "sandbox-account".to_owned(),
                name: "sandbox route".to_owned(),
                ..proto::Account::default()
            }],
        });
        response.metadata_mut().insert(
            "x-tracking-id",
            "sandbox-service".parse().expect("metadata"),
        );
        Ok(response)
    }

    unimplemented_rpc!(
        open_sandbox_account,
        proto::OpenSandboxAccountRequest,
        proto::OpenSandboxAccountResponse
    );
    unimplemented_rpc!(
        close_sandbox_account,
        proto::CloseSandboxAccountRequest,
        proto::CloseSandboxAccountResponse
    );
    unimplemented_rpc!(
        post_sandbox_order,
        proto::PostOrderRequest,
        proto::PostOrderResponse
    );
    unimplemented_rpc!(
        post_sandbox_order_async,
        proto::PostOrderAsyncRequest,
        proto::PostOrderAsyncResponse
    );
    unimplemented_rpc!(
        replace_sandbox_order,
        proto::ReplaceOrderRequest,
        proto::PostOrderResponse
    );
    unimplemented_rpc!(
        get_sandbox_orders,
        proto::GetOrdersRequest,
        proto::GetOrdersResponse
    );
    unimplemented_rpc!(
        cancel_sandbox_order,
        proto::CancelOrderRequest,
        proto::CancelOrderResponse
    );
    unimplemented_rpc!(
        get_sandbox_order_state,
        proto::GetOrderStateRequest,
        proto::OrderState
    );
    unimplemented_rpc!(
        get_sandbox_order_price,
        proto::GetOrderPriceRequest,
        proto::GetOrderPriceResponse
    );
    unimplemented_rpc!(
        get_sandbox_positions,
        proto::PositionsRequest,
        proto::PositionsResponse
    );
    unimplemented_rpc!(
        get_sandbox_operations,
        proto::OperationsRequest,
        proto::OperationsResponse
    );
    unimplemented_rpc!(
        get_sandbox_operations_by_cursor,
        proto::GetOperationsByCursorRequest,
        proto::GetOperationsByCursorResponse
    );
    unimplemented_rpc!(
        get_sandbox_portfolio,
        proto::PortfolioRequest,
        proto::PortfolioResponse
    );
    unimplemented_rpc!(
        sandbox_pay_in,
        proto::SandboxPayInRequest,
        proto::SandboxPayInResponse
    );
    unimplemented_rpc!(
        get_sandbox_withdraw_limits,
        proto::WithdrawLimitsRequest,
        proto::WithdrawLimitsResponse
    );
    unimplemented_rpc!(
        get_sandbox_max_lots,
        proto::GetMaxLotsRequest,
        proto::GetMaxLotsResponse
    );
    unimplemented_rpc!(
        post_sandbox_stop_order,
        proto::PostStopOrderRequest,
        proto::PostStopOrderResponse
    );
    unimplemented_rpc!(
        get_sandbox_stop_orders,
        proto::GetStopOrdersRequest,
        proto::GetStopOrdersResponse
    );
    unimplemented_rpc!(
        cancel_sandbox_stop_order,
        proto::CancelStopOrderRequest,
        proto::CancelStopOrderResponse
    );
}

#[tokio::test]
async fn contour_types_use_different_routes_and_current_endpoints() {
    assert_eq!(
        PRODUCTION_ENDPOINT,
        "https://invest-public-api.tbank.ru:443"
    );
    assert_eq!(
        SANDBOX_ENDPOINT,
        "https://sandbox-invest-public-api.tbank.ru:443"
    );

    let users_calls = Arc::new(AtomicUsize::new(0));
    let sandbox_calls = Arc::new(AtomicUsize::new(0));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let address = listener.local_addr().expect("mock address");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = Server::builder()
        .add_service(proto::users_service_server::UsersServiceServer::new(
            MockUsers {
                calls: users_calls.clone(),
            },
        ))
        .add_service(proto::sandbox_service_server::SandboxServiceServer::new(
            MockSandbox {
                calls: sandbox_calls.clone(),
            },
        ))
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
            let _ = shutdown_rx.await;
        });
    let server_task = tokio::spawn(server);
    let endpoint = format!("http://{address}");
    let token = SecretString::new("test-token".to_owned());

    let production = ProductionBroker::new(
        ProductionApi::builder(token.clone())
            .endpoint(endpoint.clone())
            .connect()
            .await
            .expect("production connects"),
    );
    let sandbox = SandboxBroker::new(
        SandboxApi::builder(token)
            .endpoint(endpoint)
            .connect()
            .await
            .expect("sandbox connects"),
    );
    let production_response = production.accounts().await.expect("production accounts");
    let sandbox_response = sandbox.accounts().await.expect("sandbox accounts");

    assert_eq!(
        production_response.data[0].id.as_str(),
        "production-account"
    );
    assert_eq!(
        production_response.metadata.tracking_id.as_deref(),
        Some("production-users")
    );
    assert_eq!(sandbox_response.data[0].id.as_str(), "sandbox-account");
    assert_eq!(
        sandbox_response.metadata.tracking_id.as_deref(),
        Some("sandbox-service")
    );
    assert_eq!(users_calls.load(Ordering::SeqCst), 1);
    assert_eq!(sandbox_calls.load(Ordering::SeqCst), 1);

    let _ = shutdown_tx.send(());
    server_task
        .await
        .expect("mock server task")
        .expect("mock server");
}
