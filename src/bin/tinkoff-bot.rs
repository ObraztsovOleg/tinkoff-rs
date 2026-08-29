//! CLI и исполняемый цикл для портфельных стратегий T-Invest Sandbox.
//!
//! Инвариант процесса: один экземпляр программы обслуживает один взвешенный ансамбль
//! и один выделенный Sandbox-счёт. Ансамбль может содержать одну или несколько
//! стратегий. Production-контур здесь намеренно отсутствует, поэтому ошибка в
//! конфигурации или логике примера не может отправить заявку на реальный счёт.
//!
//! Жизненный цикл команды `run`:
//! 1. проверить TOML, счёт и инструменты;
//! 2. восстановить устойчивое состояние риска и незавершённые пакеты заявок;
//! 3. прогреть стратегию только закрытыми свечами без права выставлять заявки;
//! 4. объединить поток свечей, официальное расписание MOEX и периодические risk-check;
//! 5. перед каждым решением заново прочитать портфель и проверить внешние операции.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration as StdDuration,
};

use chrono::{DateTime, Duration, NaiveDate, Utc};
use clap::{Parser, Subcommand};
use futures::StreamExt;
use rust_decimal::Decimal;
use secrecy::SecretString;
use serde::Deserialize;
use tinkoff_rs::{
    Error, Result, SandboxApi,
    broker::{Accounts, MarketData, Portfolio, SandboxBroker},
    domain::{
        AccountId, Candle, CandleHistoryRequest, CandleInterval, InstrumentId, MarketDataEvent,
        Money, PortfolioSnapshot, Price, StrategyId, datetime_to_timestamp, timestamp_to_datetime,
    },
    engine::{
        AccountRiskState, BatchExecutionOutcome, BatchExecutor, DataRequirement, EventPhase,
        InstrumentInfo, PlanningContext, PortfolioEngine, RiskAnchor, RiskConfig, RiskManager,
        RiskMode, RiskObservation, ScheduleAnchor, ScheduleEvent, ScheduleRequirement,
        StrategyEvent, StrategyEventKind,
    },
    proto::{
        GetOperationsByCursorRequest, GetOrdersRequest, InstrumentIdType, InstrumentRequest,
        OperationState, OperationType, RealExchange, TradingSchedulesRequest,
    },
    storage::{PostgresStorage, Storage},
    strategies::{BuiltinStrategySpec, WeightedEnsembleStrategy, WeightedStrategySpec},
    stream::{MarketDataSubscription, StreamConfig, market_event},
};
use tokio::time::MissedTickBehavior;
use tracing::{info, warn};

const DEFAULT_APP_NAME: &str = "ObraztsovOleg.tinkoff-rs.strategies";
const DEFAULT_DATABASE_CONNECTIONS: u32 = 5;
/// Небольшое окно допуска для срабатывания события расписания после задержки процесса.
/// Более старое событие пропускается: исполнять запоздалый сигнал опаснее, чем не исполнить его.
const SCHEDULE_GRACE_SECONDS: i64 = 120;

/// Общие аргументы CLI. Путь к TOML можно передать флагом или переменной окружения.
#[derive(Parser)]
#[command(version, about = "T-Invest Sandbox portfolio-strategy runtime")]
struct Cli {
    /// Путь к TOML одного счёта/ансамбля.
    #[arg(long, env = "TINKOFF_BOT_CONFIG", default_value = "bot.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

/// Верхнеуровневые операции над конфигурацией, runtime и риском.
#[derive(Subcommand)]
enum Command {
    /// Показать стабильные идентификаторы всех встроенных стратегий.
    ListStrategies,
    /// Разобрать и проверить TOML без токена, базы данных и сетевых запросов.
    CheckConfig,
    /// Применить миграции `PostgreSQL`; незавершённые legacy-intents блокируют миграцию.
    Migrate,
    /// Прогреть настроенный ансамбль и запустить его только в Sandbox.
    Run,
    /// Вывести устойчивое состояние risk epoch счёта в JSON.
    RiskStatus,
    /// Начать новую risk epoch после проверок портфеля и заявок.
    RiskReset {
        /// Обязательная аудиторская причина ручного сброса.
        #[arg(long)]
        reason: String,
    },
}

/// Допустимый торговый контур.
///
/// Единственный вариант перечисления делает `contour = "production"` ошибкой TOML,
/// а не скрытым переключателем, который можно случайно активировать.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Contour {
    #[default]
    Sandbox,
}

/// Роль инструмента в universe. Значение сохраняется в конфигурации стратегии,
/// хотя общие проверки runtime одинаковы для обеих ролей.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum UniverseGroup {
    #[default]
    Tradable,
    PairCandidate,
}

/// Одна запись `[[universe]]`; `uid` — стабильный UID инструмента T-Invest.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UniverseEntry {
    /// UID, а не ticker: ticker может быть неоднозначным или измениться.
    uid: String,
    #[serde(default, rename = "group")]
    _group: UniverseGroup,
}

/// Корневая TOML-конфигурация одного процесса торгового ансамбля.
///
/// `deny_unknown_fields` защищает от опечаток: неизвестное поле не будет молча
/// проигнорировано. Бюджет и процентные стопы находятся внутри `risk`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BotConfig {
    /// Всегда `sandbox`; иное значение не десериализуется.
    #[serde(default, rename = "contour")]
    _contour: Contour,
    /// Отдельный Sandbox-счёт, закреплённый за этим ансамблем.
    account_id: String,
    /// Уникальный ID конкретного экземпляра ансамбля.
    strategy_id: String,
    /// Идентификатор приложения для T-Invest API.
    #[serde(default = "default_app_name")]
    app_name: String,
    /// Максимальный размер пула соединений `PostgreSQL`.
    #[serde(default = "default_database_connections")]
    database_connections: u32,
    /// Бюджет, опциональные процентные стопы и параметры risk epoch.
    risk: RiskConfig,
    /// Инструменты, которыми имеет право владеть этот процесс.
    universe: Vec<UniverseEntry>,
    /// Одна или несколько встроенных стратегий и их относительные веса.
    strategies: Vec<WeightedStrategySpec>,
}

/// Значение `app_name`, используемое при отсутствии одноимённого поля в TOML.
fn default_app_name() -> String {
    DEFAULT_APP_NAME.to_owned()
}

/// Размер пула `PostgreSQL` по умолчанию.
const fn default_database_connections() -> u32 {
    DEFAULT_DATABASE_CONNECTIONS
}

/// Возвращает виды компонент в том же порядке, в котором они агрегируются.
fn strategy_kinds(config: &BotConfig) -> Vec<&'static str> {
    config
        .strategies
        .iter()
        .map(|component| component.strategy.kind())
        .collect()
}

/// Проверенные типизированные идентификаторы, с которыми работает runtime.
/// Отделение их от сырых строк гарантирует, что в основной цикл не попадут пустые UID.
#[derive(Clone)]
struct RuntimeIds {
    account_id: AccountId,
    strategy_id: StrategyId,
    universe: Vec<InstrumentId>,
}

impl BotConfig {
    /// Читает TOML, запрещает неизвестные поля и сразу выполняет смысловую проверку.
    fn load(path: &Path) -> Result<(Self, RuntimeIds)> {
        let raw = std::fs::read_to_string(path).map_err(|error| {
            Error::Configuration(format!("cannot read {}: {error}", path.display()))
        })?;
        let config: Self = toml::from_str(&raw).map_err(|error| {
            Error::Configuration(format!("invalid {}: {error}", path.display()))
        })?;
        let ids = config.validate()?;
        Ok((config, ids))
    }

    /// Проверяет общие ограничения и параметры всех компонент ансамбля.
    ///
    /// Здесь же удаляется неоднозначность universe: повторяющийся UID считается
    /// ошибкой, поскольку его двойное присутствие могло бы исказить веса.
    fn validate(&self) -> Result<RuntimeIds> {
        if self.app_name.trim().is_empty() {
            return Err(Error::Configuration("app_name cannot be empty".to_owned()));
        }
        if self.database_connections == 0 {
            return Err(Error::Configuration(
                "database_connections must be positive".to_owned(),
            ));
        }
        self.risk.validate()?;
        let account_id = AccountId::new(self.account_id.clone())?;
        let strategy_id = StrategyId::new(self.strategy_id.clone())?;
        let universe = self
            .universe
            .iter()
            .map(|entry| InstrumentId::new(entry.uid.clone()))
            .collect::<Result<Vec<_>>>()?;
        let unique: HashSet<_> = universe.iter().map(InstrumentId::as_str).collect();
        if unique.len() != universe.len() {
            return Err(Error::Configuration(
                "universe contains duplicate instrument UIDs".to_owned(),
            ));
        }
        // Конструктор ансамбля выполняет полную проверку: непустой список,
        // положительные signal_weight и требования каждого kind к universe.
        WeightedEnsembleStrategy::new(
            strategy_id.clone(),
            account_id.clone(),
            universe.clone(),
            self.strategies.clone(),
        )?;
        Ok(RuntimeIds {
            account_id,
            strategy_id,
            universe,
        })
    }
}

#[tokio::main]
/// Настраивает логирование, исполняет одну команду и преобразует ошибку в exit code.
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tinkoff_bot=info,tinkoff_rs=info".into()),
        )
        .init();
    match dispatch().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "command failed");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Разбирает команду и подключает только необходимые зависимости.
///
/// Например, `check-config` не требует токена или сети, а `list-strategies` не
/// открывает `PostgreSQL`. Это позволяет безопасно проверять файлы до запуска торговли.
async fn dispatch() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::ListStrategies => {
            for kind in BuiltinStrategySpec::all_kinds() {
                println!("{kind}");
            }
            Ok(())
        }
        Command::CheckConfig => {
            let (config, ids) = BotConfig::load(&cli.config)?;
            println!(
                "valid: contour=sandbox strategy={} components={} kinds={} instruments={} budget_rub={}",
                ids.strategy_id,
                config.strategies.len(),
                strategy_kinds(&config).join(","),
                ids.universe.len(),
                config.risk.budget_rub
            );
            Ok(())
        }
        Command::Migrate => {
            let (config, _) = BotConfig::load(&cli.config)?;
            let storage = connect_storage(&config).await?;
            storage.migrate().await?;
            info!("database migrations are current");
            Ok(())
        }
        Command::RiskStatus => {
            let (config, ids) = BotConfig::load(&cli.config)?;
            let storage = connect_storage(&config).await?;
            storage.migrate().await?;
            let state = storage
                .load_risk_state(&ids.account_id)
                .await?
                .ok_or_else(|| Error::Storage("risk state has not been initialized".to_owned()))?;
            ensure_risk_owner(&state, &ids)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&state).map_err(|error| {
                    Error::Storage(format!("cannot serialize risk state: {error}"))
                })?
            );
            Ok(())
        }
        Command::RiskReset { reason } => {
            let (config, ids) = BotConfig::load(&cli.config)?;
            risk_reset(&config, &ids, &reason).await
        }
        Command::Run => {
            let (config, ids) = BotConfig::load(&cli.config)?;
            run_bot(config, ids).await
        }
    }
}

/// Создаёт пул `PostgreSQL`. URL намеренно берётся из окружения, а не хранится в TOML.
async fn connect_storage(config: &BotConfig) -> Result<Arc<PostgresStorage>> {
    let database_url = std::env::var("DATABASE_URL")
        .map_err(|_| Error::Configuration("DATABASE_URL is required".to_owned()))?;
    Ok(Arc::new(
        PostgresStorage::connect(&database_url, config.database_connections).await?,
    ))
}

/// Создаёт исключительно Sandbox API и Sandbox-брокера.
///
/// В этой функции нет ветки Production; даже корректный боевой токен не меняет
/// тип клиента и не позволяет этому binary отправлять реальные заявки.
async fn connect_sandbox(config: &BotConfig) -> Result<(SandboxApi, Arc<SandboxBroker>)> {
    let token = std::env::var("TINKOFF_TOKEN")
        .map_err(|_| Error::Configuration("TINKOFF_TOKEN is required".to_owned()))?;
    if token.trim().is_empty() {
        return Err(Error::Configuration(
            "TINKOFF_TOKEN cannot be empty".to_owned(),
        ));
    }
    let api = SandboxApi::builder(SecretString::new(token))
        .app_name(config.app_name.clone())
        .connect()
        .await?;
    let broker = Arc::new(SandboxBroker::new(api.clone()));
    Ok((api, broker))
}

/// Запускает полный Sandbox-runtime после локальной и сетевой проверки окружения.
///
/// Порядок инициализации существенен: до завершения проверок и прогрева executor не
/// используется, поэтому исторические свечи не могут породить торговую заявку.
async fn run_bot(config: BotConfig, ids: RuntimeIds) -> Result<()> {
    let storage = connect_storage(&config).await?;
    storage.migrate().await?;
    // Lease удерживается до выхода из `run_bot`. Поэтому второй процесс не
    // успеет ни восстановить те же batch-и, ни отправить заявку с того же счёта.
    let _runtime_lease = storage.acquire_runtime_lease(&ids.account_id).await?;
    let (api, broker) = connect_sandbox(&config).await?;
    ensure_account_exists(&broker, &ids.account_id).await?;

    // Для engine ансамбль остаётся одной stateful PortfolioStrategy. Внутри него
    // компоненты независимо считают сигналы, а наружу выходит один полный target.
    let strategy = WeightedEnsembleStrategy::new(
        ids.strategy_id.clone(),
        ids.account_id.clone(),
        ids.universe.clone(),
        config.strategies.clone(),
    )?;
    let engine = PortfolioEngine::new(Box::new(strategy), storage.clone()).await?;
    let requirements = engine.data_requirements().await;
    let instruments = inspect_instruments(
        &api,
        &broker,
        &ids.universe,
        &requirements,
        config
            .strategies
            .iter()
            .any(|component| strategy_requires_short(component.strategy.kind())),
    )
    .await?;

    // Risk state восстанавливается до прогрева: несовпадение бюджета или владельца
    // счёта должно остановить процесс раньше любых торговых действий.
    let initial_portfolio = broker.portfolio(&ids.account_id).await?.data;
    let mut risk = initialize_risk(
        &api,
        storage.as_ref(),
        &config.risk,
        &ids,
        &initial_portfolio,
    )
    .await?;
    if outside_position(&initial_portfolio, &ids.universe).is_some()
        && risk.state().blocked.is_none()
    {
        validate_dedicated_portfolio(&initial_portfolio, &ids.universe)?;
    }
    // Warmup передаёт свечи как EventPhase::Warmup и без PlanningContext. Engine
    // может заполнить состояние индикаторов, но физически не способен создать batch.
    let latest_prices = warm_up(&engine, broker.as_ref(), &requirements).await?;

    // Незавершённые durable batches восстанавливаются до новых сигналов. При risk
    // lock разрешены лишь сокращения/закрытия, но не новые позиции.
    let executor = BatchExecutor::new(
        ids.account_id.clone(),
        ids.strategy_id.clone(),
        broker.clone(),
        broker.clone(),
        storage.clone(),
    );
    let recovered = if risk.state().blocked.is_some() {
        executor.recover_pending_batches_close_only().await?
    } else {
        executor.recover_pending_batches().await?
    };
    for (batch_id, outcome) in recovered {
        info!(%batch_id, ?outcome, "recovered durable rebalance batch");
    }

    let subscriptions = requirements
        .iter()
        .map(|requirement| {
            MarketDataSubscription::candles(
                vec![requirement.instrument_id.clone()],
                requirement.interval,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let mut market_stream = broker.market_data_stream(subscriptions, StreamConfig::default())?;
    let mut schedule_requirements = engine.schedule_requirements().await;
    // Открытие сессии нужно risk manager даже стратегии без собственного сигнала
    // на open: по нему корректно сменяется торговый день и дневной лимит.
    if !schedule_requirements.iter().any(|requirement| {
        requirement.anchor == ScheduleAnchor::SessionOpen && requirement.offset_seconds == 0
    }) {
        schedule_requirements.push(ScheduleRequirement {
            anchor: ScheduleAnchor::SessionOpen,
            offset_seconds: 0,
        });
    }
    let mut triggers = load_schedule_triggers(&api, &schedule_requirements, Utc::now()).await?;
    let mut schedule_loaded_on = Utc::now().date_naive();
    let mut fired = HashSet::new();
    // Delay предотвращает серию мгновенных catch-up тиков после подвисания процесса.
    let mut schedule_tick = tokio::time::interval(StdDuration::from_secs(15));
    schedule_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut risk_tick = tokio::time::interval(StdDuration::from_secs(30));
    risk_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut runtime = LiveRuntime {
        account_id: ids.account_id,
        universe: ids.universe,
        instruments,
        latest_prices,
        api,
        broker,
        storage,
        engine,
        executor,
        risk: &mut risk,
    };

    info!(
        strategy_id = %ids.strategy_id,
        components = config.strategies.len(),
        kinds = %strategy_kinds(&config).join(","),
        "warmup complete; Sandbox runtime is live"
    );
    // Все источники событий сходятся в LiveRuntime::process, поэтому свечи,
    // расписание и таймер риска проходят один и тот же safety pipeline.
    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal.map_err(|error| Error::Configuration(format!("signal handler failed: {error}")))?;
                market_stream.handle.shutdown().await?;
                info!("shutdown requested");
                return Ok(());
            }
            message = market_stream.events.next() => {
                let Some(response) = message else {
                    return Err(Error::StreamClosed("market data channel closed".to_owned()));
                };
                if let Some(MarketDataEvent::Candle(candle)) = market_event(response?, true)? {
                    // market_event(..., true) отбрасывает незакрытые свечи. Цена
                    // обновляется до вызова стратегии, чтобы planner видел close.
                    runtime.latest_prices.insert(candle.instrument_id.clone(), candle.close);
                    runtime.process(candle_event(candle, EventPhase::Live)).await?;
                }
            }
            _ = schedule_tick.tick() => {
                let now = Utc::now();
                // Расписание загружается заново при смене UTC-даты, а fired
                // очищается только вместе с новым набором официальных сессий MOEX.
                if now.date_naive() != schedule_loaded_on {
                    triggers = load_schedule_triggers(&runtime.api, &schedule_requirements, now).await?;
                    schedule_loaded_on = now.date_naive();
                    fired.clear();
                }
                for trigger in take_due_triggers(&mut triggers, &mut fired, now) {
                    runtime.process(trigger.event()).await?;
                }
            }
            _ = risk_tick.tick() => {
                let now = Utc::now();
                // Risk-check не просит стратегию менять сигнал; он обеспечивает
                // регулярный mark-to-market контроль даже при отсутствии свечей.
                runtime.process(StrategyEvent {
                    id: format!("risk-check:{}", now.timestamp()),
                    phase: EventPhase::Live,
                    occurred_at: now,
                    payload: StrategyEventKind::RiskCheck,
                }).await?;
            }
        }
    }
}

/// Зависимости живого цикла. Тип брокера фиксирован как `SandboxBroker`.
struct LiveRuntime<'a> {
    account_id: AccountId,
    universe: Vec<InstrumentId>,
    instruments: Vec<InstrumentInfo>,
    latest_prices: HashMap<InstrumentId, Price>,
    api: SandboxApi,
    broker: Arc<SandboxBroker>,
    storage: Arc<PostgresStorage>,
    engine: PortfolioEngine<PostgresStorage>,
    executor: BatchExecutor<SandboxBroker, SandboxBroker, PostgresStorage>,
    risk: &'a mut RiskManager,
}

impl LiveRuntime<'_> {
    /// Обрабатывает одно live-событие через единый risk-first pipeline.
    ///
    /// Сначала читается свежий портфель и сохраняется решение риска. Затем
    /// восстанавливаются старые заявки; если они изменили позиции, risk observation
    /// строится повторно. Только после этого стратегия получает planning context.
    async fn process(&mut self, event: StrategyEvent) -> Result<()> {
        let mut portfolio = self.broker.portfolio(&self.account_id).await?.data;
        // Пополнения и выводы вычитаются из P&L с момента anchor. Неизвестный тип
        // внешнего движения нельзя безопасно нормализовать, поэтому он включает close-only.
        let (external_cash_flows, recognized) =
            scan_external_cash_flows(&self.api, self.risk.state()).await?;
        let mut decision = self.risk.observe(&RiskObservation {
            current_equity: rub_equity(&portfolio)?,
            session_date: event_session_date(&event, self.risk.state().session_date),
            observed_at: Utc::now(),
            external_cash_flows,
            external_cash_flows_recognized: recognized,
            portfolio_is_flat: portfolio_is_flat(&portfolio),
        })?;
        // Состояние стопов сохраняется до работы с заявками: перезапуск процесса
        // не должен забывать уже сработавшую блокировку.
        self.storage.save_risk_state(&decision.state).await?;
        let recovered = if decision.mode == RiskMode::CloseOnly {
            self.executor.recover_pending_batches_close_only().await?
        } else {
            self.executor.recover_pending_batches().await?
        };
        if !recovered.is_empty() {
            for (batch_id, outcome) in &recovered {
                info!(%batch_id, ?outcome, "polled pending rebalance batch");
            }
            // Исполненная старая заявка меняет equity и позиции. Использовать снимок,
            // сделанный до recovery, означало бы планировать от устаревшего количества лотов.
            portfolio = self.broker.portfolio(&self.account_id).await?.data;
            let (external_cash_flows, recognized) =
                scan_external_cash_flows(&self.api, self.risk.state()).await?;
            decision = self.risk.observe(&RiskObservation {
                current_equity: rub_equity(&portfolio)?,
                session_date: event_session_date(&event, self.risk.state().session_date),
                observed_at: Utc::now(),
                external_cash_flows,
                external_cash_flows_recognized: recognized,
                portfolio_is_flat: portfolio_is_flat(&portfolio),
            })?;
            self.storage.save_risk_state(&decision.state).await?;
        }

        // Single-flight barrier: новый target нельзя планировать, пока исход
        // предыдущего batch ещё не доказан. Закрытую свечу всё же применяем как
        // warmup, чтобы индикаторы не потеряли историю; warmup физически не может
        // создать target/batch. Schedule и RiskCheck будут повторены будущими
        // live-событиями, поэтому их безопаснее пропустить.
        let awaiting_recovery = recovered.iter().any(|(_, outcome)| {
            matches!(outcome, BatchExecutionOutcome::AwaitingFills)
        });
        if awaiting_recovery || self.storage.has_pending_orders(&self.account_id).await? {
            warn!(
                event_id = %event.id,
                "new trading decision deferred while a durable batch is unfinished"
            );
            if matches!(&event.payload, StrategyEventKind::Candle(_)) {
                let mut warmup_event = event;
                warmup_event.phase = EventPhase::Warmup;
                self.engine.process_event(&warmup_event, None).await?;
            }
            return Ok(());
        }
        prepare_planning_portfolio(&mut portfolio, &self.universe, decision.mode)?;
        if decision.mode == RiskMode::CloseOnly {
            warn!(?decision.state.blocked, "risk manager is in close-only mode");
        }
        let context = PlanningContext {
            portfolio,
            instruments: self.instruments.clone(),
            prices: self.latest_prices.clone(),
            risk: decision,
        };
        // Engine атомарно сохраняет target и legs. Executor получает только ID уже
        // сохранённого batch, поэтому процесс можно безопасно восстановить после сбоя.
        let outcome = self.engine.process_event(&event, Some(&context)).await?;
        if let Some(batch) = outcome.batch {
            let execution = if context.risk.mode == RiskMode::CloseOnly {
                self.executor.execute_batch_close_only(batch.id).await?
            } else {
                self.executor.execute_batch(batch.id).await?
            };
            info!(batch_id = %batch.id, ?execution, "rebalance batch processed");
        }
        Ok(())
    }
}

/// Определяет торговую дату для дневного stop.
///
/// У события официального расписания дата точная. Для свечи используется дата её
/// начала, а технический risk-check сохраняет уже известную runtime дату сессии.
fn event_session_date(event: &StrategyEvent, current: NaiveDate) -> NaiveDate {
    match &event.payload {
        StrategyEventKind::Schedule(schedule) => schedule.session_date,
        StrategyEventKind::Candle(candle) => candle.time.date_naive(),
        StrategyEventKind::Order(_) | StrategyEventKind::RiskCheck => current,
    }
}

/// Восстанавливает существующую risk epoch либо создаёт первую от текущей equity.
/// После создания сразу выполняется observation, чтобы внешний перевод между anchor
/// и запуском процесса не оказался ошибочно принят за торговую прибыль или убыток.
async fn initialize_risk(
    api: &SandboxApi,
    storage: &PostgresStorage,
    config: &RiskConfig,
    ids: &RuntimeIds,
    portfolio: &PortfolioSnapshot,
) -> Result<RiskManager> {
    let now = Utc::now();
    let mut manager = if let Some(state) = storage.load_risk_state(&ids.account_id).await? {
        ensure_risk_owner(&state, ids)?;
        RiskManager::from_state(config.clone(), state)?
    } else {
        RiskManager::new(
            config.clone(),
            RiskAnchor {
                account_id: ids.account_id.clone(),
                strategy_id: ids.strategy_id.clone(),
                account_equity: rub_equity(portfolio)?,
                session_date: now.date_naive(),
                observed_at: now,
            },
        )?
    };
    let (external_cash_flows, recognized) = scan_external_cash_flows(api, manager.state()).await?;
    let decision = manager.observe(&RiskObservation {
        current_equity: rub_equity(portfolio)?,
        session_date: manager.state().session_date,
        observed_at: now,
        external_cash_flows,
        external_cash_flows_recognized: recognized,
        portfolio_is_flat: portfolio_is_flat(portfolio),
    })?;
    storage.save_risk_state(&decision.state).await?;
    Ok(manager)
}

/// Создаёт новую risk epoch после строгих проверок безопасного состояния счёта.
///
/// Reset разрешён только для пустого выделенного портфеля без локальных pending legs
/// и без активных Sandbox-заявок. Это исключает смену anchor посреди исполнения.
async fn risk_reset(config: &BotConfig, ids: &RuntimeIds, reason: &str) -> Result<()> {
    let storage = connect_storage(config).await?;
    storage.migrate().await?;
    // Тот же account-level lock, что у `run`, делает reset и смену владельца
    // невозможными, пока старый процесс ещё работает.
    let _runtime_lease = storage.acquire_runtime_lease(&ids.account_id).await?;
    let (api, broker) = connect_sandbox(config).await?;
    ensure_account_exists(&broker, &ids.account_id).await?;
    let portfolio = broker.portfolio(&ids.account_id).await?.data;
    validate_dedicated_portfolio(&portfolio, &ids.universe)?;
    if !portfolio_is_flat(&portfolio) {
        return Err(Error::RiskRejected(
            "risk-reset requires a fully liquidated portfolio".to_owned(),
        ));
    }
    if storage.has_pending_orders(&ids.account_id).await? {
        return Err(Error::RiskRejected(
            "risk-reset requires no pending durable orders".to_owned(),
        ));
    }
    let active_orders = api
        .raw()
        .sandbox()
        .get_sandbox_orders(GetOrdersRequest {
            account_id: ids.account_id.to_string(),
            advanced_filters: None,
        })
        .await?
        .into_inner()
        .orders;
    if !active_orders.is_empty() {
        return Err(Error::RiskRejected(
            "risk-reset requires no active Sandbox orders".to_owned(),
        ));
    }
    let state = storage
        .load_risk_state(&ids.account_id)
        .await?
        .ok_or_else(|| Error::Storage("risk state has not been initialized".to_owned()))?;
    if state.account_id != ids.account_id {
        return Err(Error::Storage(
            "risk state was loaded for a different account".to_owned(),
        ));
    }
    // from_state должен увидеть бюджет старой epoch. Новый бюджет применяется только
    // внутри reset — иначе обычное изменение TOML могло бы обойти требование risk-reset.
    let previous_config = RiskConfig {
        budget_rub: state.budget_rub,
        daily_loss_percent: config.risk.daily_loss_percent,
        max_drawdown_percent: config.risk.max_drawdown_percent,
    };
    let previous_strategy_id = state.strategy_id.clone();
    let previous_epoch = state.epoch;
    let mut manager = RiskManager::from_state(previous_config, state)?;
    let now = Utc::now();
    let decision = manager.reset(
        config.risk.clone(),
        RiskAnchor {
            account_id: ids.account_id.clone(),
            strategy_id: ids.strategy_id.clone(),
            account_equity: rub_equity(&portfolio)?,
            session_date: manager.state().session_date,
            observed_at: now,
        },
        reason,
        false,
    )?;
    // CAS по прежним owner/epoch разрешает аудируемую передачу пустого счёта
    // новому strategy_id, не ослабляя обычный save_risk_state runtime-а.
    storage
        .replace_risk_state_for_reset(&previous_strategy_id, previous_epoch, &decision.state)
        .await?;
    println!("new risk epoch: {}", decision.state.epoch);
    Ok(())
}

/// Не позволяет другой стратегии присвоить уже закреплённый за epoch счёт.
fn ensure_risk_owner(state: &AccountRiskState, ids: &RuntimeIds) -> Result<()> {
    if state.account_id != ids.account_id || state.strategy_id != ids.strategy_id {
        return Err(Error::RiskRejected(format!(
            "account {} is already owned by strategy {} in risk epoch {}",
            state.account_id, state.strategy_id, state.epoch
        )));
    }
    Ok(())
}

/// Проверяет, что указанный `account_id` действительно является доступным Sandbox-счётом.
async fn ensure_account_exists(broker: &SandboxBroker, account_id: &AccountId) -> Result<()> {
    let accounts = broker.accounts().await?.data;
    if accounts.iter().any(|account| &account.id == account_id) {
        Ok(())
    } else {
        Err(Error::Configuration(format!(
            "Sandbox account `{account_id}` is unavailable to this token"
        )))
    }
}

/// Загружает метаданные инструментов и проверяет их до начала live-цикла.
///
/// Торговля разрешается только RUB-инструментами MOEX с корректным размером лота,
/// доступными API market orders и, для long/short стратегий, разрешённым шортом.
async fn inspect_instruments(
    api: &SandboxApi,
    broker: &SandboxBroker,
    universe: &[InstrumentId],
    requirements: &[DataRequirement],
    requires_short: bool,
) -> Result<Vec<InstrumentInfo>> {
    let mut seen = HashSet::new();
    let mut raw = HashMap::new();
    // Один инструмент может встречаться в нескольких требованиях к таймфреймам;
    // сетевые метаданные для него достаточно запросить один раз.
    for requirement in requirements {
        if !seen.insert(requirement.instrument_id.clone()) {
            continue;
        }
        let response = api
            .raw()
            .instruments()
            .get_instrument_by(InstrumentRequest {
                id_type: InstrumentIdType::Uid as i32,
                class_code: None,
                id: requirement.instrument_id.to_string(),
            })
            .await?
            .into_inner();
        let instrument = response.instrument.ok_or_else(|| {
            Error::Configuration(format!(
                "instrument `{}` was not returned by T-Invest",
                requirement.instrument_id
            ))
        })?;
        if instrument.uid != requirement.instrument_id.as_str()
            || instrument.real_exchange != RealExchange::Moex as i32
            || !instrument.currency.eq_ignore_ascii_case("rub")
        {
            return Err(Error::Configuration(format!(
                "instrument `{}` must be a RUB instrument executed on MOEX",
                requirement.instrument_id
            )));
        }
        raw.insert(requirement.instrument_id.clone(), instrument);
    }

    let mut result = Vec::with_capacity(universe.len());
    // Проверяем именно торгуемый universe: benchmark может быть нужен как источник
    // данных, но не обязан превращаться в InstrumentInfo для planner.
    for instrument_id in universe {
        let instrument = raw.get(instrument_id).ok_or_else(|| {
            Error::Configuration(format!(
                "tradable instrument `{instrument_id}` has no data requirement"
            ))
        })?;
        let availability = broker.trading_availability(instrument_id).await?.data;
        if instrument.lot <= 0
            || !instrument.api_trade_available_flag
            || !availability.api_trade_available
            || !availability.market_order_available
            || (requires_short && !instrument.short_enabled_flag)
        {
            return Err(Error::Configuration(format!(
                "tradable instrument `{instrument_id}` lacks lot, API market-order or required short availability"
            )));
        }
        result.push(InstrumentInfo {
            instrument_id: instrument_id.clone(),
            lot_size: i64::from(instrument.lot),
            currency: instrument.currency.clone(),
            exchange: "moex".to_owned(),
            short_enabled: instrument.short_enabled_flag,
            api_trade_available: instrument.api_trade_available_flag
                && availability.api_trade_available,
            market_order_available: availability.market_order_available,
        });
    }
    Ok(result)
}

/// Сообщает preflight, требуется ли стратегии возможность открывать короткие позиции.
fn strategy_requires_short(kind: &str) -> bool {
    !matches!(
        kind,
        "low-volatility" | "equal-risk-contribution" | "volatility-managed"
    )
}

/// Загружает минимальную историю закрытых свечей и прогревает состояние стратегии.
///
/// Свечи сортируются глобально по времени, чтобы многомерные стратегии увидели их
/// в причинном порядке. `PlanningContext = None` и `EventPhase::Warmup` запрещают
/// планирование/исполнение заявок во время воспроизведения истории.
async fn warm_up(
    engine: &PortfolioEngine<PostgresStorage>,
    broker: &SandboxBroker,
    requirements: &[DataRequirement],
) -> Result<HashMap<InstrumentId, Price>> {
    let now = Utc::now();
    let mut candles = Vec::new();
    for requirement in requirements {
        if requirement.warmup_bars == 0 || requirement.warmup_bars > 2_500 {
            return Err(Error::Configuration(format!(
                "warmup for `{}` must contain 1..=2500 bars",
                requirement.instrument_id
            )));
        }
        let mut received = broker
            .candles(CandleHistoryRequest {
                instrument_id: requirement.instrument_id.clone(),
                interval: requirement.interval,
                from: now - warmup_span(requirement.interval, requirement.warmup_bars)?,
                to: now,
                limit: Some(
                    i32::try_from(requirement.warmup_bars.saturating_add(8).min(2_500)).map_err(
                        |_| {
                            Error::Configuration(
                                "warmup bar count does not fit API limit".to_owned(),
                            )
                        },
                    )?,
                ),
            })
            .await?
            .data;
        received.retain(|candle| candle.complete);
        received.sort_by_key(|candle| candle.time);
        if received.len() < requirement.warmup_bars {
            return Err(Error::Configuration(format!(
                "instrument `{}` returned {} complete {:?} candles, but {} are required",
                requirement.instrument_id,
                received.len(),
                requirement.interval,
                requirement.warmup_bars
            )));
        }
        // API может вернуть небольшой запас. Стратегии нужны ровно последние N
        // завершённых баров, поэтому более старую часть отбрасываем.
        let keep_from = received.len() - requirement.warmup_bars;
        candles.extend(received.drain(keep_from..));
    }
    // При одинаковом timestamp UID задаёт стабильный порядок воспроизведения.
    candles.sort_by(|left, right| {
        left.time.cmp(&right.time).then_with(|| {
            left.instrument_id
                .as_str()
                .cmp(right.instrument_id.as_str())
        })
    });
    let mut latest_prices = HashMap::new();
    for candle in candles {
        latest_prices.insert(candle.instrument_id.clone(), candle.close);
        engine
            .process_event(&candle_event(candle, EventPhase::Warmup), None)
            .await?;
    }
    Ok(latest_prices)
}

/// Оценивает календарное окно, достаточное для получения `bars` торговых свечей.
///
/// Множители больше длины самого интервала оставляют запас на ночь, выходные и
/// праздники MOEX. Это только границы запроса; после ответа берутся ровно последние
/// завершённые бары, поэтому запас не меняет вычисляемый сигнал.
fn warmup_span(interval: CandleInterval, bars: usize) -> Result<Duration> {
    let bars = i64::try_from(bars)
        .map_err(|_| Error::Configuration("warmup bar count is too large".to_owned()))?;
    let duration = match interval {
        CandleInterval::OneMonth => Duration::days(bars.saturating_mul(35)),
        CandleInterval::OneWeek => Duration::days(bars.saturating_mul(10)),
        CandleInterval::OneDay => Duration::days(bars.saturating_mul(3).saturating_add(14)),
        CandleInterval::FourHours => Duration::hours(bars.saturating_mul(16)),
        CandleInterval::TwoHours => Duration::hours(bars.saturating_mul(8)),
        CandleInterval::OneHour => Duration::hours(bars.saturating_mul(4)),
        CandleInterval::ThirtyMinutes => Duration::minutes(bars.saturating_mul(30 * 8)),
        CandleInterval::FifteenMinutes => Duration::minutes(bars.saturating_mul(15 * 8)),
        CandleInterval::TenMinutes => Duration::minutes(bars.saturating_mul(10 * 8)),
        CandleInterval::FiveMinutes => Duration::minutes(bars.saturating_mul(5 * 8)),
        CandleInterval::ThreeMinutes => Duration::minutes(bars.saturating_mul(3 * 8)),
        CandleInterval::TwoMinutes => Duration::minutes(bars.saturating_mul(2 * 8)),
        CandleInterval::OneMinute => Duration::minutes(bars.saturating_mul(8)),
        CandleInterval::ThirtySeconds => Duration::seconds(bars.saturating_mul(30 * 8)),
        CandleInterval::TenSeconds => Duration::seconds(bars.saturating_mul(10 * 8)),
        CandleInterval::FiveSeconds => Duration::seconds(bars.saturating_mul(5 * 8)),
    };
    Ok(duration)
}

/// Оборачивает свечу в идемпотентное событие стратегии.
///
/// `candle.time` обозначает начало интервала, поэтому `occurred_at` сдвигается к
/// закрытию. Так target не выглядит созданным раньше данных, на которых он рассчитан.
fn candle_event(candle: Candle, phase: EventPhase) -> StrategyEvent {
    let interval = candle_interval_key(candle.interval);
    let occurred_at = candle.time + candle_duration(candle.interval);
    StrategyEvent {
        id: format!(
            "candle:{}:{interval}:{}",
            candle.instrument_id,
            candle.time.to_rfc3339()
        ),
        phase,
        occurred_at,
        payload: StrategyEventKind::Candle(candle),
    }
}

/// Возвращает номинальную длительность свечи для вычисления времени её закрытия.
const fn candle_duration(interval: CandleInterval) -> Duration {
    match interval {
        CandleInterval::FiveSeconds => Duration::seconds(5),
        CandleInterval::TenSeconds => Duration::seconds(10),
        CandleInterval::ThirtySeconds => Duration::seconds(30),
        CandleInterval::OneMinute => Duration::minutes(1),
        CandleInterval::TwoMinutes => Duration::minutes(2),
        CandleInterval::ThreeMinutes => Duration::minutes(3),
        CandleInterval::FiveMinutes => Duration::minutes(5),
        CandleInterval::TenMinutes => Duration::minutes(10),
        CandleInterval::FifteenMinutes => Duration::minutes(15),
        CandleInterval::ThirtyMinutes => Duration::minutes(30),
        CandleInterval::OneHour => Duration::hours(1),
        CandleInterval::TwoHours => Duration::hours(2),
        CandleInterval::FourHours => Duration::hours(4),
        CandleInterval::OneDay => Duration::days(1),
        CandleInterval::OneWeek => Duration::weeks(1),
        CandleInterval::OneMonth => Duration::days(31),
    }
}

/// Стабильное короткое имя интервала, входящее в идемпотентный event ID.
const fn candle_interval_key(interval: CandleInterval) -> &'static str {
    match interval {
        CandleInterval::OneMinute => "1m",
        CandleInterval::FiveMinutes => "5m",
        CandleInterval::FifteenMinutes => "15m",
        CandleInterval::OneHour => "1h",
        CandleInterval::OneDay => "1d",
        CandleInterval::TwoMinutes => "2m",
        CandleInterval::ThreeMinutes => "3m",
        CandleInterval::TenMinutes => "10m",
        CandleInterval::ThirtyMinutes => "30m",
        CandleInterval::TwoHours => "2h",
        CandleInterval::FourHours => "4h",
        CandleInterval::OneWeek => "1w",
        CandleInterval::OneMonth => "1mo",
        CandleInterval::FiveSeconds => "5s",
        CandleInterval::TenSeconds => "10s",
        CandleInterval::ThirtySeconds => "30s",
    }
}

/// Одно рассчитанное срабатывание относительно официальной сессии MOEX.
#[derive(Clone)]
struct ScheduledTrigger {
    anchor: ScheduleAnchor,
    session_date: NaiveDate,
    at: DateTime<Utc>,
}

impl ScheduledTrigger {
    /// Строит стабильный ID, чтобы одно расписание нельзя было исполнить дважды.
    fn id(&self) -> String {
        format!(
            "schedule:{:?}:{}:{}",
            self.anchor,
            self.session_date,
            self.at.to_rfc3339()
        )
    }

    /// Преобразует внутренний trigger в live-событие для стратегии и risk manager.
    fn event(self) -> StrategyEvent {
        let id = self.id();
        StrategyEvent {
            id,
            phase: EventPhase::Live,
            occurred_at: self.at,
            payload: StrategyEventKind::Schedule(ScheduleEvent {
                anchor: self.anchor,
                session_date: self.session_date,
                scheduled_at: self.at,
            }),
        }
    }
}

/// Загружает официальное расписание MOEX и разворачивает требования стратегии
/// в абсолютные UTC-моменты на ближайшие две недели.
///
/// Время берётся из ответа биржевого календаря, а не вычисляется фиксированным
/// московским часом: так учитываются выходные, сокращённые дни и особые сессии.
async fn load_schedule_triggers(
    api: &SandboxApi,
    requirements: &[ScheduleRequirement],
    now: DateTime<Utc>,
) -> Result<Vec<ScheduledTrigger>> {
    if requirements.is_empty() {
        return Ok(Vec::new());
    }
    let response = api
        .raw()
        .instruments()
        .trading_schedules(TradingSchedulesRequest {
            exchange: Some("MOEX".to_owned()),
            from: Some(datetime_to_timestamp(now - Duration::days(1))),
            to: Some(datetime_to_timestamp(now + Duration::days(14))),
        })
        .await?
        .into_inner();
    let mut triggers = Vec::new();
    for exchange in response.exchanges {
        if !exchange.exchange.to_ascii_lowercase().contains("moex") {
            continue;
        }
        for day in exchange.days.into_iter().filter(|day| day.is_trading_day) {
            let Some(date) = day
                .start_time
                .or(day.date)
                .map(timestamp_to_datetime)
                .transpose()?
                .map(|date| date.date_naive())
            else {
                continue;
            };
            for requirement in requirements {
                // Offset применяется после выбора конкретного якоря: открытия,
                // закрытия или начала аукциона закрытия этой сессии.
                let timestamp = match requirement.anchor {
                    ScheduleAnchor::SessionOpen => day.start_time,
                    ScheduleAnchor::SessionClose => day.end_time,
                    ScheduleAnchor::ClosingAuctionStart => day.closing_auction_start_time,
                };
                if let Some(timestamp) = timestamp {
                    triggers.push(ScheduledTrigger {
                        anchor: requirement.anchor,
                        session_date: date,
                        at: timestamp_to_datetime(timestamp)?
                            + Duration::seconds(requirement.offset_seconds),
                    });
                }
            }
        }
    }
    triggers.sort_by_key(|trigger| trigger.at);
    Ok(triggers)
}

/// Извлекает наступившие события, исключая дубликаты и слишком старые сигналы.
/// Будущие triggers остаются в векторе, обработанные удаляются через `retain`.
fn take_due_triggers(
    triggers: &mut Vec<ScheduledTrigger>,
    fired: &mut HashSet<String>,
    now: DateTime<Utc>,
) -> Vec<ScheduledTrigger> {
    let mut due = Vec::new();
    triggers.retain(|trigger| {
        if trigger.at > now {
            return true;
        }
        let age = now.signed_duration_since(trigger.at).num_seconds();
        let id = trigger.id();
        // После паузы до двух минут сигнал ещё считается своевременным. Более
        // старый сигнал не догоняется, чтобы не торговать по утратившей силу цене.
        if age <= SCHEDULE_GRACE_SECONDS && fired.insert(id) {
            due.push(trigger.clone());
        }
        false
    });
    due
}

/// Суммирует внешние денежные потоки с начала risk epoch.
///
/// Возвращает `(rub_cash_flow, recognized)`. RUB-пополнения и выводы учитываются
/// со знаком `payment`, чтобы risk manager вычел их из торгового P&L. Перевод бумаг,
/// операция без суммы или движение в другой валюте делает `recognized = false`;
/// это переводит runtime в close-only, поскольку безопасно восстановить P&L нельзя.
async fn scan_external_cash_flows(
    api: &SandboxApi,
    state: &AccountRiskState,
) -> Result<(Decimal, bool)> {
    let mut cursor = None;
    let mut total = Decimal::ZERO;
    let mut recognized = true;
    loop {
        let response = api
            .raw()
            .sandbox()
            .get_sandbox_operations_by_cursor(GetOperationsByCursorRequest {
                account_id: state.account_id.to_string(),
                instrument_id: None,
                from: Some(datetime_to_timestamp(state.anchor_observed_at)),
                to: Some(datetime_to_timestamp(Utc::now())),
                cursor: cursor.clone(),
                limit: Some(1_000),
                operation_types: Vec::new(),
                state: Some(OperationState::Executed as i32),
                without_commissions: Some(false),
                without_trades: Some(false),
                without_overnights: Some(false),
            })
            .await?
            .into_inner();
        for item in response.items {
            let operation_type = OperationType::try_from(item.r#type).unwrap_or_default();
            match operation_type {
                OperationType::Input
                | OperationType::Output
                | OperationType::InputSwift
                | OperationType::OutputSwift
                | OperationType::InputAcquiring
                | OperationType::OutputAcquiring
                | OperationType::InpMulti
                | OperationType::OutMulti
                | OperationType::BuyCard
                | OperationType::SellCard => {
                    if let Some(payment) = item.payment {
                        let payment = Money::from_proto(payment)?;
                        if payment.currency.eq_ignore_ascii_case("rub") {
                            // API уже задаёт знак платежа: ввод положительный, вывод
                            // отрицательный. Повторно менять знак здесь не нужно.
                            total += payment.amount.0;
                        } else {
                            recognized = false;
                        }
                    } else {
                        recognized = false;
                    }
                }
                OperationType::InputSecurities
                | OperationType::OutputSecurities
                | OperationType::TransIisBs
                | OperationType::TransBsBs => recognized = false,
                OperationType::Unspecified | OperationType::Other
                    if item.instrument_uid.is_empty() =>
                {
                    recognized = false;
                }
                _ => {}
            }
        }
        if !response.has_next {
            break;
        }
        // Защита от бесконечного цикла при повреждённой пагинации API.
        if response.next_cursor.is_empty() || cursor.as_deref() == Some(&response.next_cursor) {
            return Err(Error::InvalidArgument(
                "operations cursor did not advance".to_owned(),
            ));
        }
        cursor = Some(response.next_cursor);
    }
    Ok((total, recognized))
}

/// Проверяет принцип «один ансамбль — один выделенный счёт».
/// Любая ненулевая позиция вне configured universe блокирует обычный запуск/reset.
fn validate_dedicated_portfolio(
    portfolio: &PortfolioSnapshot,
    universe: &[InstrumentId],
) -> Result<()> {
    if let Some(position) = outside_position(portfolio, universe) {
        return Err(Error::RiskRejected(format!(
            "dedicated account contains `{}` outside this ensemble universe",
            position.instrument_id
        )));
    }
    Ok(())
}

/// Находит первую ненулевую позицию, которой ансамбль не имеет права управлять.
fn outside_position<'a>(
    portfolio: &'a PortfolioSnapshot,
    universe: &[InstrumentId],
) -> Option<&'a tinkoff_rs::domain::PortfolioPosition> {
    let universe: HashSet<_> = universe.iter().collect();
    portfolio.positions.iter().find(|position| {
        !position.quantity.is_zero() && !universe.contains(&position.instrument_id)
    })
}

/// Готовит снимок для planner с учётом чужих позиций.
///
/// В normal mode чужая позиция является ошибкой. В close-only её нельзя автоматически
/// закрывать без метаданных и явного владения, поэтому она оставляется для ручного
/// удаления и исключается из снимка, по которому закрывается configured universe.
fn prepare_planning_portfolio(
    portfolio: &mut PortfolioSnapshot,
    universe: &[InstrumentId],
    mode: RiskMode,
) -> Result<()> {
    if let Some(position) = outside_position(portfolio, universe) {
        if mode == RiskMode::Normal {
            return Err(Error::RiskRejected(format!(
                "dedicated account contains `{}` outside this ensemble universe",
                position.instrument_id
            )));
        }
        warn!(
            instrument_id = %position.instrument_id,
            "external position requires manual removal; configured positions remain close-only"
        );
        let universe: HashSet<_> = universe.iter().collect();
        portfolio
            .positions
            .retain(|position| universe.contains(&position.instrument_id));
    }
    Ok(())
}

/// Извлекает полную mark-to-market equity и требует рублёвую валюту оценки.
/// Эта величина включает открытые позиции и используется обоими процентными стопами.
fn rub_equity(portfolio: &PortfolioSnapshot) -> Result<Decimal> {
    let total = portfolio.total.as_ref().ok_or_else(|| {
        Error::RiskRejected("portfolio response omitted mark-to-market equity".to_owned())
    })?;
    if !total.currency.eq_ignore_ascii_case("rub") {
        return Err(Error::RiskRejected(format!(
            "portfolio equity must be RUB, received {}",
            total.currency
        )));
    }
    Ok(total.amount.0)
}

/// Возвращает `true`, только если количество каждого инструмента равно нулю.
/// Денежный остаток не мешает считать портфель ликвидированным.
fn portfolio_is_flat(portfolio: &PortfolioSnapshot) -> bool {
    portfolio
        .positions
        .iter()
        .all(|position| position.quantity.is_zero())
}
