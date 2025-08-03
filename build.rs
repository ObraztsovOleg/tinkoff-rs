fn main()->Result<(),Box<dyn std::error::Error>>{
    tonic_build::configure()
        .build_client(true)
        .build_server(false)
        .out_dir("src")
        .client_attribute(".", "#[derive(derive_more::From, derive_more::Into)]")
        .compile_protos(
            &[
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
            ],
            &["contracts/"],
        )?;

    std::fs::rename(
        "src/tinkoff.public.invest.api.contract.v1.rs",
        "src/tcs.rs",
    )?;
    Ok(())
}