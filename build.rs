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

/// Правила стенда, общие для бота, супервизора и дашборда.
const RULES: &str = "contracts/stand-rules.json";

/// Константы Rust из `contracts/stand-rules.json`: тот же файл читают Python и
/// дашборд, поэтому дни переноса фьючерсов, стратегии «только акции» и темы
/// рынка не могут разойтись между частями стенда.
fn stand_rules(out: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    use std::fmt::Write as _;

    println!("cargo:rerun-if-changed={RULES}");
    let rules: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(RULES)?)?;
    let text = |value: &serde_json::Value, what: &str| -> Result<String, String> {
        value
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("{RULES}: {what} must be a string"))
    };
    let futures = &rules["futures"];
    let roll_days = futures["roll_days"]
        .as_i64()
        .ok_or("futures.roll_days must be an integer")?;
    let roll_time = text(&futures["roll_time_utc"], "futures.roll_time_utc")?;
    let (hour, minute) = roll_time
        .split_once(':')
        .ok_or("futures.roll_time_utc must be HH:MM")?;
    let (hour, minute): (u32, u32) = (hour.parse()?, minute.parse()?);
    let mut code = String::from("// Сгенерировано build.rs из contracts/stand-rules.json.\n");
    writeln!(
        code,
        "pub(crate) const SERIES_PREFIX: &str = {:?};",
        text(&futures["series_prefix"], "futures.series_prefix")?
    )?;
    writeln!(code, "pub(crate) const ROLL_DAYS: i64 = {roll_days};")?;
    writeln!(
        code,
        "pub(crate) const ROLL_TIME_UTC: (u32, u32) = ({hour}, {minute});"
    )?;
    writeln!(
        code,
        "pub(crate) const MONTHS: &str = {:?};",
        text(&futures["month_letters"], "futures.month_letters")?
    )?;
    code.push_str("pub(crate) const SHARE_ONLY_KINDS: &[&str] = &[");
    for kind in rules["share_only_kinds"]
        .as_array()
        .ok_or("share_only_kinds must be a list")?
    {
        write!(code, "{:?}, ", text(kind, "share_only_kinds[]")?)?;
    }
    code.push_str("];\npub(crate) const TOPICS: &[Topic] = &[\n");
    for topic in rules["topics"].as_array().ok_or("topics must be a list")? {
        writeln!(
            code,
            "    Topic {{ id: {:?}, name: {:?}, up: {:?} }},",
            text(&topic["id"], "topics[].id")?,
            text(&topic["name"], "topics[].name")?,
            text(&topic["up"], "topics[].up")?
        )?;
    }
    code.push_str("];\n");
    std::fs::write(out.join("stand_rules.rs"), code)?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // sqlx::migrate! embeds the directory at compile time. Track additions too,
    // otherwise an incremental build can silently omit a new migration file.
    println!("cargo:rerun-if-changed=migrations");
    stand_rules(&PathBuf::from(std::env::var("OUT_DIR")?))?;
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
    let sources: Vec<PathBuf> = CONTRACTS.iter().map(PathBuf::from).collect();
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&sources, &[PathBuf::from("contracts"), include])?;

    Ok(())
}
