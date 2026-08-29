//! Cargo build script: генерирует Rust protobuf/gRPC-код до компиляции crate.
//!
//! Сгенерированный файл помещается в `OUT_DIR` и подключается из `src/proto.rs`
//! через `tonic::include_proto!`. Поэтому generated sources не хранятся в `src/`.

use std::path::PathBuf;

// Явный список делает состав контрактного snapshot проверяемым: случайно
// появившийся .proto не попадёт в публичный Rust API без изменения build.rs.
const CONTRACTS: &[&str] = &[
    "contracts/google/api/field_behavior.proto",
    "contracts/common.proto",
    "contracts/instruments.proto",
    "contracts/marketdata.proto",
    "contracts/operations.proto",
    "contracts/orders.proto",
    "contracts/sandbox.proto",
    "contracts/signals.proto",
    "contracts/stoporders.proto",
    "contracts/users.proto",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Vendored protoc обеспечивает одинаковую сборку без системной установки.
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    let include = protoc_bin_vendored::include_path()?;

    // SAFETY: build scripts run in their own process before any worker threads are started.
    unsafe {
        std::env::set_var("PROTOC", protoc);
    }

    for contract in CONTRACTS {
        println!("cargo:rerun-if-changed={contract}");
    }

    // Генерируются и client, и server stubs: server используется mock-тестами,
    // client — реальными ProductionApi/SandboxApi.
    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(CONTRACTS, &[PathBuf::from("contracts"), include])?;

    Ok(())
}
