use std::{fs, path::Path};

use tinkoff_rs::{
    ProductionRawApi, SandboxRawApi,
    broker::{
        Accounts, MarketData, OperationsStream, OrdersStream, Portfolio, ProductionBroker,
        ProductionTrading, Reports, SandboxAdmin, SandboxBroker, Trading,
    },
};

const CONTRACTS: &[&str] = &[
    "common.proto",
    "instruments.proto",
    "marketdata.proto",
    "operations.proto",
    "orders.proto",
    "sandbox.proto",
    "signals.proto",
    "stoporders.proto",
    "users.proto",
];

#[test]
fn snapshot_contains_exactly_107_rpc_and_new_methods() {
    let contracts = Path::new(env!("CARGO_MANIFEST_DIR")).join("contracts");
    let text = CONTRACTS
        .iter()
        .map(|name| fs::read_to_string(contracts.join(name)).expect("contract is readable"))
        .collect::<Vec<_>>()
        .join("\n");
    let rpc_count = text
        .lines()
        .filter(|line| line.trim_start().starts_with("rpc "))
        .count();
    assert_eq!(rpc_count, 107);
    for method in [
        "rpc DfaBy ",
        "rpc Dfas ",
        "rpc News(",
        "rpc PayIn(",
        "rpc GetAccountValues(",
        "rpc OperationsStream(",
        "rpc GetSandboxOrderPrice(",
    ] {
        assert!(text.contains(method), "missing {method}");
    }
}

#[test]
fn contour_types_expose_their_positive_capabilities() {
    fn production<
        T: MarketData + Accounts + Portfolio + OrdersStream + OperationsStream + Reports,
    >() {
    }
    fn production_trading<T: Trading>() {}
    fn sandbox<
        T: MarketData
            + Accounts
            + Portfolio
            + Trading
            + OrdersStream
            + OperationsStream
            + SandboxAdmin,
    >() {
    }

    production::<ProductionBroker>();
    production_trading::<ProductionTrading>();
    sandbox::<SandboxBroker>();
}

// This is never called; compiling it verifies access to newly generated RPCs.
#[allow(dead_code)]
async fn generated_clients_expose_new_rpc(production: &ProductionRawApi, sandbox: &SandboxRawApi) {
    let _ = production
        .instruments()
        .dfa_by(tinkoff_rs::proto::InstrumentRequest::default())
        .await;
    let _ = production
        .instruments()
        .dfas(tinkoff_rs::proto::DfasRequest::default())
        .await;
    let _ = production
        .instruments()
        .news(tinkoff_rs::proto::NewsRequest::default())
        .await;
    let _ = production
        .users()
        .pay_in(tinkoff_rs::proto::PayInRequest::default())
        .await;
    let _ = production
        .users()
        .get_account_values(tinkoff_rs::proto::GetAccountValuesRequest::default())
        .await;
    let _ = production
        .operations_stream()
        .operations_stream(tinkoff_rs::proto::OperationsStreamRequest::default())
        .await;
    let _ = sandbox
        .sandbox()
        .get_sandbox_order_price(tinkoff_rs::proto::GetOrderPriceRequest::default())
        .await;
}
