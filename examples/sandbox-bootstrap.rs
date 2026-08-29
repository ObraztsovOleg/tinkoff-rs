//! Явно создаёт и пополняет виртуальный sandbox-счёт.
//!
//! Пример изменяет только состояние песочницы. Если пополнение не удалось,
//! только что открытый счёт закрывается как компенсационное действие. Runtime
//! бота не выполняет этот lifecycle автоматически.

use rust_decimal::Decimal;
use secrecy::SecretString;
use tinkoff_rs::{
    Error, Result, SandboxApi,
    broker::{SandboxAdmin, SandboxBroker},
    domain::{Money, Price},
};

#[tokio::main]
async fn main() -> Result<()> {
    // Сначала валидируются все локальные параметры, чтобы не создавать счёт при
    // заведомо некорректной сумме.
    let token = required_env("TINKOFF_TOKEN")?;
    let account_name = std::env::var("TINKOFF_SANDBOX_ACCOUNT_NAME")
        .unwrap_or_else(|_| "tinkoff-rs sandbox".to_owned());
    let initial_rubles = std::env::var("TINKOFF_SANDBOX_INITIAL_RUB")
        .unwrap_or_else(|_| "100000".to_owned())
        .parse::<Decimal>()
        .map_err(|error| {
            Error::Configuration(format!("invalid TINKOFF_SANDBOX_INITIAL_RUB: {error}"))
        })?;
    if initial_rubles <= Decimal::ZERO {
        return Err(Error::Configuration(
            "TINKOFF_SANDBOX_INITIAL_RUB must be positive".to_owned(),
        ));
    }

    let api = SandboxApi::builder(SecretString::new(token))
        .app_name("ObraztsovOleg.tinkoff-rs.bootstrap")
        .connect()
        .await?;
    let broker = SandboxBroker::new(api);
    // open_account и pay_in — отдельные mutation; атомарной API-транзакции между
    // ними нет, поэтому ветка ошибки ниже пытается закрыть созданный счёт.
    let account_id = broker.open_account(Some(account_name)).await?.data;
    let pay_in = broker
        .pay_in(
            &account_id,
            Money {
                currency: "rub".to_owned(),
                amount: Price::new(initial_rubles),
            },
        )
        .await;

    match pay_in {
        Ok(balance) => {
            println!("sandbox account_id={account_id}");
            println!("balance={} {}", balance.data.amount, balance.data.currency);
            println!("copy account_id into bot.toml");
            Ok(())
        }
        Err(error) => {
            let _ = broker.close_account(&account_id).await;
            Err(error)
        }
    }
}

fn required_env(name: &'static str) -> Result<String> {
    // Общая проверка отличает отсутствующую переменную от переданной пустой.
    let value =
        std::env::var(name).map_err(|_| Error::Configuration(format!("{name} is required")))?;
    if value.trim().is_empty() {
        Err(Error::Configuration(format!("{name} cannot be empty")))
    } else {
        Ok(value)
    }
}
