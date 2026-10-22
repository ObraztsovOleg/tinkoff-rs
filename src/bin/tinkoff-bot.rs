//! CLI и исполняемый цикл для портфельных стратегий T-Invest.
//!
//! Инвариант процесса: один экземпляр программы обслуживает один взвешенный ансамбль
//! и один выделенный счёт. Ансамбль может содержать одну или несколько
//! стратегий. По умолчанию счёт виртуальный (Sandbox). Реальный счёт
//! (`contour = "production"`) требует обоих процентных стопов в TOML и токена
//! `TINKOFF_TOKEN_PRODUCTION` с полным доступом к открытому счёту; аварийная
//! остановка в базе не даёт запустить процесс реального контура.
//!
//! Жизненный цикл команды `run`:
//! 1. проверить TOML, счёт и инструменты;
//! 2. восстановить устойчивое состояние риска и незавершённые пакеты заявок;
//! 3. прогреть стратегию только закрытыми свечами без права выставлять заявки;
//! 4. объединить поток свечей, официальное расписание MOEX и периодические risk-check;
//! 5. перед каждым решением заново прочитать портфель и проверить внешние операции.
//!
//! Отсутствие сети не завершает процесс. Решение, для которого нет связи с API,
//! пропускается без торговли; поток данных переподключается сам и догружает
//! пропущенные свечи; при долгом простое или обрыве во время запуска сессия
//! создаётся заново с задержкой до минуты. Устаревшая котировка тоже лишь
//! пропускает решение. Процесс завершают сигнал остановки и ошибки, которые
//! повтор не исправит: конфигурация, авторизация, занятый другим процессом счёт.

#[path = "support/ai/mod.rs"]
mod ai;
#[path = "support/cleanup.rs"]
mod cleanup;
#[path = "support/control.rs"]
mod control;
#[path = "support/documents/mod.rs"]
mod documents;
#[path = "support/history.rs"]
mod history;
#[path = "support/instruments.rs"]
mod instruments;
#[path = "support/resilience.rs"]
mod resilience;
#[path = "support/venue.rs"]
mod venue;

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
use serde::Deserialize;
use tinkoff_rs::{
    Error, Result,
    broker::{MarketData, Portfolio},
    domain::{
        AccountId, Candle, CandleHistoryRequest, CandleInterval, InstrumentId, MarketDataEvent,
        Money, PortfolioSnapshot, Price, StrategyId, datetime_to_timestamp, timestamp_to_datetime,
    },
    engine::{
        AccountRiskState, BatchExecutionOutcome, BatchExecutor, DataRequirement, DividendEvent,
        EventPhase, InstrumentFundamentals, InstrumentInfo, InstrumentKind, PlanningContext,
        PortfolioEngine, PortfolioStrategy, ReferenceData, ReferenceRequirement, RiskAnchor,
        RiskConfig, RiskManager, RiskMode, RiskObservation, ScheduleAnchor, ScheduleEvent,
        ScheduleRequirement, StrategyEvent, StrategyEventKind,
    },
    proto::{
        GetAssetFundamentalsRequest, GetDividendsRequest, GetOperationsByCursorRequest,
        InstrumentIdType, InstrumentRequest, OperationState, OperationType, RealExchange,
        TradingSchedulesRequest,
    },
    storage::{PostgresStorage, RuntimeLease, Storage},
    strategies::{BuiltinStrategySpec, WeightedEnsembleStrategy, WeightedStrategySpec},
    stream::{
        MarketDataStream, MarketDataStreamHandle, MarketDataSubscription, StreamConfig,
        market_event,
    },
};
use tokio::{sync::watch, time::MissedTickBehavior};
use tracing::{info, warn};
use venue::{Contour, VenueApi, VenueBroker};

const DEFAULT_APP_NAME: &str = "ObraztsovOleg.tinkoff-rs.strategies";
const DEFAULT_DATABASE_CONNECTIONS: u32 = 5;
/// Небольшое окно допуска для срабатывания события расписания после задержки процесса.
/// Более старое событие пропускается: исполнять запоздалый сигнал опаснее, чем не исполнить его.
const SCHEDULE_GRACE_SECONDS: i64 = 120;
/// Решение открытия сессии, отложенное из-за сна компьютера, сети, аукциона или
/// незавершённого пакета, повторяется с этим интервалом до конца той же сессии.
const OPEN_RETRY_SECONDS: i64 = 60;
/// Последний повтор — не позже чем за столько минут до конца основной сессии.
const OPEN_RETRY_MARGIN_MINUTES: i64 = 10;
/// Сколько раз за сессию повторять неудачный запрос к ИИ-модели.
const AI_ATTEMPTS: u32 = 3;

/// Общие аргументы CLI. Путь к TOML можно передать флагом или переменной окружения.
#[derive(Parser)]
#[command(
    version,
    about = "T-Invest portfolio-strategy runtime (Sandbox or a dedicated real account)"
)]
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
    /// Прогреть настроенный ансамбль и запустить его в контуре из TOML.
    Run {
        /// Validate live connectivity and persist warmup, then exit before any order recovery or live signal.
        #[arg(long)]
        startup_only: bool,
        /// Serve account commands and protective risk checks without starting a strategy.
        #[arg(long, conflicts_with = "startup_only")]
        service_only: bool,
    },
    /// Reconcile/cancel orders and flatten this dedicated account; retry when trading opens.
    Cleanup {
        #[arg(long)]
        reason: String,
    },
    /// Вывести устойчивое состояние risk epoch счёта в JSON.
    RiskStatus,
    /// Начать новую risk epoch после проверок портфеля и заявок.
    RiskReset {
        /// Обязательная аудиторская причина ручного сброса.
        #[arg(long)]
        reason: String,
    },
    /// Показать профили ИИ-моделей из `AI_MODELS_FILE` в JSON (без токенов).
    AiProfiles,
    /// Проверить подключение к ИИ-модели коротким запросом без торговли.
    AiCheck {
        /// Имя профиля; по умолчанию профиль `default`.
        #[arg(long)]
        profile: Option<String>,
    },
    /// Пробный анализ ИИ-модели для `ai-model` компонент конфига: решение без заявок.
    AiPreview,
    /// Собирать документы для ИИ-стратегии до остановки: новости T-Invest с полными
    /// текстами и раскрытием эмитентов, деловые ленты, сообщения биржи и Банка России.
    /// Нужны `DATABASE_URL` и токен Sandbox `TINKOFF_TOKEN` (только чтение).
    Collect,
    /// Найти документы в хранилище: та же выдача, что получает модель.
    Documents {
        /// Слова запроса; пусто — свежие документы.
        #[arg(long, default_value = "")]
        query: String,
        /// Тикеры бумаг (можно несколько раз).
        #[arg(long)]
        ticker: Vec<String>,
        #[arg(long, default_value_t = 72)]
        hours: u32,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Как стенд видит инструмент или серию фьючерсов `futures:BR`: тип, лот,
    /// стоимость единицы цены, текущий контракт и дневная история (только чтение,
    /// токен Sandbox `TINKOFF_TOKEN`).
    Inspect {
        /// UID инструмента или `futures:<код>`.
        #[arg(long)]
        id: String,
        /// Глубина дневной истории.
        #[arg(long, default_value_t = 120)]
        days: i64,
    },
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

/// Одна запись `[[universe]]`: стабильный UID инструмента T-Invest или
/// непрерывная серия фьючерсов `futures:BR` (см. [`instruments`]).
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
    /// `sandbox` (по умолчанию) или `production` — реальный счёт.
    #[serde(default)]
    contour: Contour,
    /// Отдельный счёт выбранного контура, закреплённый за этим ансамблем.
    account_id: String,
    /// Уникальный ID конкретного экземпляра ансамбля.
    strategy_id: String,
    /// Идентификатор приложения для T-Invest API.
    #[serde(default = "default_app_name")]
    app_name: String,
    /// Максимальный размер рабочего пула `PostgreSQL`; account lease удерживает
    /// ещё одно отдельное session-соединение вне пула.
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
        // На реальном счёте стопы не опциональны: без них риск ограничен лишь бюджетом.
        if self.contour == Contour::Production
            && (self.risk.daily_loss_percent.is_none() || self.risk.max_drawdown_percent.is_none())
        {
            return Err(Error::Configuration(
                "a production config requires risk.daily_loss_percent and risk.max_drawdown_percent"
                    .to_owned(),
            ));
        }
        let account_id = AccountId::new(self.account_id.clone())?;
        let strategy_id = StrategyId::new(self.strategy_id.clone())?;
        let universe = self
            .universe
            .iter()
            .map(|entry| InstrumentId::new(entry.uid.clone()))
            .collect::<Result<Vec<_>>>()?;
        for id in &universe {
            if id.as_str().starts_with(instruments::SERIES_PREFIX)
                && instruments::series_code(id)
                    .is_none_or(|code| instruments::dated_code(&format!("{code}Z6")) != Some(code))
            {
                return Err(Error::Configuration(format!(
                    "`{id}` is not a futures series: expected futures:<two-character MOEX code>"
                )));
            }
        }
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
    match Box::pin(dispatch()).await {
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
                "valid: contour={} strategy={} components={} kinds={} instruments={} budget_rub={}",
                config.contour.label(),
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
        Command::AiProfiles => {
            let summary = match ai::Profiles::load() {
                Ok(profiles) => profiles.summary(),
                Err(error) => serde_json::json!({"error": error.to_string()}),
            };
            println!(
                "{}",
                serde_json::json!({"path": ai::Profiles::path(), "profiles": summary})
            );
            Ok(())
        }
        Command::AiCheck { profile } => {
            println!("{}", ai::check(profile.as_deref()).await?);
            Ok(())
        }
        Command::AiPreview => {
            let (config, ids) = BotConfig::load(&cli.config)?;
            ai_preview(&config, &ids).await
        }
        Command::Inspect { id, days } => {
            let (api, broker) = venue::connect(Contour::Sandbox, DEFAULT_APP_NAME).await?;
            let id = InstrumentId::new(id)?;
            let now = Utc::now();
            api.series()
                .load(api.instruments(), std::slice::from_ref(&id), now)
                .await?;
            let (info, tick) = instruments::describe(&api, &broker, &id).await?;
            let candles = history::load(
                broker.as_ref(),
                CandleHistoryRequest {
                    instrument_id: id.clone(),
                    interval: CandleInterval::OneDay,
                    from: now - Duration::days(days),
                    to: now,
                    limit: None,
                },
                None,
            )
            .await?;
            let contract =
                instruments::series_code(&id).and_then(|code| api.series().bound_contract(code));
            // Крупнейшие дневные движения: проверка склейки истории серии.
            let mut changes: Vec<(Decimal, DateTime<Utc>)> = candles
                .windows(2)
                .filter(|w| w[0].close.0 > Decimal::ZERO)
                .map(|w| {
                    (
                        (w[1].close.0 / w[0].close.0 - Decimal::ONE) * Decimal::ONE_HUNDRED,
                        w[1].time,
                    )
                })
                .collect();
            changes.sort_by_key(|(change, _)| std::cmp::Reverse(change.abs()));
            let moves: Vec<serde_json::Value> = changes
                .iter()
                .take(5)
                .map(|(change, time)| serde_json::json!({"time": time, "percent": change.round_dp(2).to_string()}))
                .collect();
            println!(
                "{}",
                serde_json::json!({
                    "info": info,
                    "price_step": tick.map(|t| t.to_string()),
                    "contract": contract.as_ref().map(|c| serde_json::json!({
                        "uid": c.uid, "ticker": c.ticker, "last_trade": c.last_trade, "roll_at": c.roll_at(),
                    })),
                    "candles": candles.len(),
                    "first": candles.first().map(|c| serde_json::json!({"time": c.time, "close": c.close.0.to_string()})),
                    "last": candles.last().map(|c| serde_json::json!({"time": c.time, "close": c.close.0.to_string()})),
                    "largest_daily_moves": moves,
                })
            );
            Ok(())
        }
        Command::Collect => {
            let storage = documents_storage().await?;
            // Без токена Sandbox сборщик работает с остальными источниками.
            let api = match venue::connect(Contour::Sandbox, DEFAULT_APP_NAME).await {
                Ok((api, _)) => Some(api),
                Err(Error::Configuration(message)) => {
                    warn!(%message, "T-Invest news are off: no sandbox token");
                    None
                }
                Err(error) => return Err(error),
            };
            documents::collect::run(api, &storage).await
        }
        Command::Documents {
            query,
            ticker,
            hours,
            limit,
        } => {
            let storage = documents_storage().await?;
            let (from, to) = documents::search::default_window(hours);
            let filter = documents::search::Filter {
                text: query,
                subjects: ticker
                    .iter()
                    .map(|t| documents::search::Subject {
                        uid: String::new(),
                        ticker: t.to_uppercase(),
                        name: String::new(),
                    })
                    .collect(),
                kinds: Vec::new(),
                sources: Vec::new(),
                topics: Vec::new(),
                min_importance: None,
                from,
                to,
                limit: limit.clamp(1, 100),
            };
            let sources = documents::sources::Sources::load()?;
            let vectors = documents::embed::Vectors::from_sources(&sources);
            let hits = documents::search::search(&storage, &filter, vectors.as_ref()).await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&hits).unwrap_or_default()
            );
            Ok(())
        }
        Command::RiskReset { reason } => {
            let (config, ids) = BotConfig::load(&cli.config)?;
            risk_reset(&config, &ids, &reason).await
        }
        Command::Cleanup { reason } => {
            let (config, ids) = BotConfig::load(&cli.config)?;
            cleanup::run(&config, &ids, &reason).await
        }
        Command::Run {
            startup_only,
            service_only,
        } => {
            let (config, ids) = BotConfig::load(&cli.config)?;
            if service_only {
                control::serve(config, ids).await
            } else {
                run_bot(config, ids, startup_only).await
            }
        }
    }
}

/// Хранилище документов: та же база, без конфигурации стратегии.
async fn documents_storage() -> Result<PostgresStorage> {
    let database_url = std::env::var("DATABASE_URL")
        .map_err(|_| Error::Configuration("DATABASE_URL is required".to_owned()))?;
    let storage = PostgresStorage::connect(&database_url, 4).await?;
    storage.migrate().await?;
    Ok(storage)
}

/// Создаёт пул `PostgreSQL`. URL намеренно берётся из окружения, а не хранится в TOML.
async fn connect_storage(config: &BotConfig) -> Result<Arc<PostgresStorage>> {
    let database_url = std::env::var("DATABASE_URL")
        .map_err(|_| Error::Configuration("DATABASE_URL is required".to_owned()))?;
    Ok(Arc::new(
        PostgresStorage::connect(&database_url, config.database_connections).await?,
    ))
}

/// Подключает контур из TOML: Sandbox — токеном `TINKOFF_TOKEN`, реальный счёт —
/// токеном `TINKOFF_TOKEN_PRODUCTION`.
async fn connect_venue(config: &BotConfig) -> Result<(VenueApi, Arc<VenueBroker>)> {
    let (api, broker) = venue::connect(config.contour, &config.app_name).await?;
    // Непрерывные фьючерсы universe привязываются к текущим контрактам.
    let ids = config
        .universe
        .iter()
        .map(|entry| InstrumentId::new(entry.uid.clone()))
        .collect::<Result<Vec<_>>>()?;
    api.series()
        .load(api.instruments(), &ids, Utc::now())
        .await?;
    Ok((api, broker))
}

/// Запускает полный runtime выбранного контура после локальной и сетевой проверки окружения.
///
/// Временная потеря сети не завершает процесс: при обрыве во время запуска или
/// потере потока данных сессия создаётся заново с задержкой до минуты, а в живом
/// цикле отдельные события без сети пропускаются без торговли. Процесс завершается
/// только по сигналу остановки либо из-за ошибки, которую повтор не исправит
/// (конфигурация, авторизация, занятый lease, нарушение инвариантов хранилища).
async fn run_bot(config: BotConfig, ids: RuntimeIds, startup_only: bool) -> Result<()> {
    if startup_only {
        // Режим проверки запуска должен честно сообщить о недоступной сети.
        return start_session(&config, &ids, true).await.map(drop);
    }
    resilience::run_with_reconnect(
        "strategy",
        resilience::shutdown_signal(),
        resilience::Backoff::session(),
        |mut shutdown| {
            let config = &config;
            let ids = &ids;
            async move {
                let session = tokio::select! {
                    session = start_session(config, ids, false) => session?,
                    () = resilience::shutdown_requested(&mut shutdown) => {
                        info!("shutdown requested during startup");
                        return Ok(());
                    }
                };
                match session {
                    Some(session) => session.run(config, &mut shutdown).await,
                    None => Ok(()),
                }
            }
        },
    )
    .await
}

/// Подготовленная живая сессия: lease, прогретая стратегия, подписки и расписание.
struct LiveSession {
    runtime: LiveRuntime,
    market_stream: MarketDataStream,
    requirements: Vec<DataRequirement>,
    schedule_requirements: Vec<ScheduleRequirement>,
    triggers: Vec<ScheduledTrigger>,
}

/// Проверяет окружение, восстанавливает риск и прогревает стратегию.
///
/// Порядок инициализации существенен: до завершения проверок и прогрева executor не
/// используется, поэтому исторические свечи не могут породить торговую заявку.
/// При `startup_only` функция выходит до восстановления заявок и возвращает `None`.
async fn start_session(
    config: &BotConfig,
    ids: &RuntimeIds,
    startup_only: bool,
) -> Result<Option<LiveSession>> {
    let storage = connect_storage(config).await?;
    storage.migrate().await?;
    // Lease удерживается до конца сессии. Поэтому второй процесс не
    // успеет ни восстановить те же batch-и, ни отправить заявку с того же счёта.
    let mut runtime_lease = storage.acquire_runtime_lease(&ids.account_id).await?;
    // Прогрев только читает; торговый цикл реального счёта при аварийной остановке не стартует.
    if !startup_only {
        venue::ensure_not_halted(&storage, config.contour).await?;
    }
    let (api, broker) = connect_venue(config).await?;
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
    control::register(&storage, ids, config.contour).await?;
    let requirements = engine.data_requirements().await;
    let mut instruments = inspect_instruments(
        &api,
        &broker,
        &ids.universe,
        &requirements,
        config
            .strategies
            .iter()
            .any(|component| strategy_requires_short(&component.strategy)),
        &share_only_instruments(config, ids),
    )
    .await?;
    // Комиссия контура закладывается в расчёт лотов, мелкие изменения не торгуются.
    let costs = instruments::ExecutionCosts::load(&storage, config.contour.label()).await;
    costs.apply(&mut instruments);

    // Подписка запускается до исторических запросов. События временно буферизуются
    // stream runner-ом, поэтому свеча, закрывшаяся во время последовательного
    // warmup нескольких инструментов, не попадает в окно без источника данных.
    let subscriptions = requirements
        .iter()
        .map(|requirement| {
            MarketDataSubscription::candles(
                vec![api.series().contract(&requirement.instrument_id)],
                requirement.interval,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let market_stream = broker.market_data_stream(subscriptions, StreamConfig::default())?;
    info!(strategy_id = %ids.strategy_id, subscriptions = requirements.len(), "waiting for candle subscription acknowledgements");
    market_stream
        .handle
        .wait_ready(StdDuration::from_secs(60))
        .await?;
    let warmup_cutoff = Utc::now();

    // Risk state восстанавливается до прогрева: несовпадение бюджета или владельца
    // счёта должно остановить процесс раньше любых торговых действий.
    let initial_portfolio = broker.portfolio(&ids.account_id).await?.data;
    let current_session_date = current_moex_session_date(
        &api,
        Utc::now(),
        storage
            .load_risk_state(&ids.account_id)
            .await?
            .map(|s| s.session_date),
    )
    .await?;
    let risk = initialize_risk(
        &api,
        storage.as_ref(),
        &config.risk,
        ids,
        &initial_portfolio,
        current_session_date,
    )
    .await?;
    if outside_position(&initial_portfolio, &ids.universe).is_some()
        && risk.state().blocked.is_none()
    {
        let managed = control::managed_universe(&storage, &ids.account_id, &ids.universe).await?;
        validate_dedicated_portfolio(&initial_portfolio, &managed)?;
    }
    // Warmup передаёт свечи как EventPhase::Warmup и без PlanningContext. Engine
    // может заполнить состояние индикаторов, но физически не способен создать batch.
    let mut latest_prices = warm_up(
        &engine,
        broker.as_ref(),
        &requirements,
        warmup_cutoff,
        storage.as_ref(),
        &ids.strategy_id,
    )
    .await?;

    // Мультипликаторы и дивиденды нужны до первого решения. В preflight их
    // недоступность — ошибка проверки; в обычном запуске стратегия дождётся
    // следующего открытия сессии, где загрузка повторится.
    if let Err(error) = absorb_reference(
        &api,
        &engine,
        &share_ids(&instruments),
        Utc::now().date_naive(),
    )
    .await
    {
        if startup_only {
            return Err(error);
        }
        warn!(%error, "reference data unavailable at startup; retrying at the next session open");
    }

    if startup_only {
        market_stream
            .handle
            .wait_ready(StdDuration::from_secs(60))
            .await?;
        load_schedule_triggers(&api, &engine.schedule_requirements().await, Utc::now()).await?;
        market_stream.handle.shutdown().await?;
        info!(strategy_id = %ids.strategy_id, "startup checks complete; no live decisions or order recovery performed");
        return Ok(None);
    }

    // Незавершённые durable batches восстанавливаются до новых сигналов. При risk
    // lock разрешены лишь сокращения/закрытия, но не новые позиции.
    let executor = BatchExecutor::new(
        ids.account_id.clone(),
        ids.strategy_id.clone(),
        broker.clone(),
        broker.clone(),
        storage.clone(),
    );
    // С момента следующего вызова executor способен обращаться к брокеру.
    // Heartbeat доказывает, что advisory lock не исчез во время долгого warmup.
    market_stream
        .handle
        .wait_ready(StdDuration::from_secs(60))
        .await?;
    runtime_lease.check().await?;
    let recovered = if risk.state().blocked.is_some() {
        executor.recover_pending_batches_close_only().await?
    } else {
        executor.recover_pending_batches().await?
    };
    for (batch_id, outcome) in recovered {
        info!(%batch_id, ?outcome, "recovered durable rebalance batch");
    }
    let own_unfinished = storage
        .pending_batch_ids(&ids.account_id, &ids.strategy_id)
        .await?;
    if storage.has_unfinished_batches(&ids.account_id).await? && own_unfinished.is_empty() {
        return Err(Error::Storage(format!(
            "account `{}` has an unfinished batch owned by another strategy; run cannot safely recover it",
            ids.account_id
        )));
    }
    // Повторный короткий запрос закрывает редкое окно до фактического принятия
    // stream-подписки. Эти startup-свечи только дополняют state: догонять их
    // торговые сигналы после инициализации запрещено.
    catch_up_warmup(
        &engine,
        broker.as_ref(),
        &requirements,
        warmup_cutoff,
        &mut latest_prices,
    )
    .await?;
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
    let triggers = load_schedule_triggers(&api, &schedule_requirements, Utc::now()).await?;
    market_stream
        .handle
        .wait_ready(StdDuration::from_secs(60))
        .await?;

    Ok(Some(LiveSession {
        runtime: LiveRuntime {
            account_id: ids.account_id.clone(),
            strategy_id: ids.strategy_id.clone(),
            universe: ids.universe.clone(),
            instruments,
            latest_prices,
            api,
            broker,
            storage,
            engine,
            executor,
            runtime_lease,
            risk,
            health: resilience::NetworkHealth::default(),
            pending_session_date: None,
            pending_open: None,
            ai_jobs: ai_jobs(config, ids),
            ai_done: HashSet::new(),
            ai_failures: HashMap::new(),
            ai_watch: ai::Watch::default(),
            costs,
        },
        market_stream,
        requirements,
        schedule_requirements,
        triggers,
    }))
}

impl LiveSession {
    /// Живой цикл. Все источники событий сходятся в `LiveRuntime::handle`, поэтому
    /// свечи, расписание и таймер риска проходят один и тот же safety pipeline.
    ///
    /// Пока подписки переподтверждаются после обрыва, свечи в стратегию не
    /// подаются, а торговые решения по расписанию пропускаются: решение на
    /// неполных данных опаснее отсутствия решения. После переподключения
    /// пропущенный интервал догружается из истории по порядку, как Warmup.
    async fn run(self, config: &BotConfig, shutdown: &mut watch::Receiver<bool>) -> Result<()> {
        let Self {
            mut runtime,
            mut market_stream,
            requirements,
            schedule_requirements,
            mut triggers,
        } = self;
        let mut continuity = StreamContinuity::new(&market_stream.handle);
        let mut schedule_loaded_on = Utc::now().date_naive();
        let mut fired = HashSet::new();
        // Delay предотвращает серию мгновенных catch-up тиков после подвисания процесса.
        let mut schedule_tick = tokio::time::interval(StdDuration::from_secs(15));
        schedule_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut risk_tick = tokio::time::interval(StdDuration::from_secs(30));
        risk_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut control_tick = tokio::time::interval(StdDuration::from_secs(1));
        control_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // ИИ-модель: сторож скачков цен и всплесков новостей, только в основную сессию.
        let mut ai_tick = tokio::time::interval(StdDuration::from_secs(60));
        ai_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut sessions = session_windows(&triggers);
        let mut stream_ready = true;

        info!(
            strategy_id = %runtime.strategy_id,
            components = config.strategies.len(),
            kinds = %strategy_kinds(config).join(","),
            "warmup complete; {} runtime is live",
            runtime.api.contour().title()
        );
        loop {
            tokio::select! {
                () = resilience::shutdown_requested(shutdown) => {
                    let _ = market_stream.handle.shutdown().await;
                    info!("shutdown requested");
                    return Ok(());
                }
                message = market_stream.events.next() => {
                    // Закрытый канал или неустранимая ошибка потока завершают сессию;
                    // временные причины приводят к её пересозданию в run_bot.
                    let Some(response) = message else {
                        return Err(Error::StreamClosed("market data channel closed".to_owned()));
                    };
                    if let Some(MarketDataEvent::Candle(mut candle)) = market_event(response?, true)? {
                        candle.instrument_id = runtime.api.series().of_broker(candle.instrument_id);
                        // market_event(..., true) отбрасывает незакрытые свечи. Цена
                        // обновляется до вызова стратегии, чтобы planner видел close.
                        runtime.latest_prices.insert(candle.instrument_id.clone(), candle.close);
                        // До подтверждения подписок свеча не подаётся в стратегию:
                        // после него весь пропущенный интервал догружается по порядку.
                        if market_stream.handle.is_ready() {
                            continuity.restore(&mut runtime, &requirements, &market_stream.handle).await?;
                            runtime.handle(candle_event(candle, EventPhase::Live)).await?;
                        }
                    }
                }
                _ = schedule_tick.tick() => {
                    let ready = market_stream.handle.is_ready();
                    if ready != stream_ready {
                        stream_ready = ready;
                        if ready {
                            info!("market subscriptions confirmed after reconnect");
                        } else {
                            warn!("market data stream is reconnecting; scheduled decisions wait for confirmed subscriptions");
                        }
                    }
                    let now = Utc::now();
                    // Расписание загружается заново при смене UTC-даты, а fired
                    // очищается только вместе с новым набором официальных сессий MOEX.
                    // Без сети сохраняется прежнее расписание; загрузка повторится.
                    if now.date_naive() != schedule_loaded_on {
                        match load_schedule_triggers(&runtime.api, &schedule_requirements, now).await {
                            Ok(loaded) => {
                                sessions = session_windows(&loaded);
                                triggers = loaded;
                                schedule_loaded_on = now.date_naive();
                                fired.clear();
                            }
                            Err(error) if error.is_transient() => {
                                runtime.health.failure("schedule refresh", &error);
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    if ready {
                        continuity.restore(&mut runtime, &requirements, &market_stream.handle).await?;
                    }
                    for (trigger, late) in take_due_triggers(&mut triggers, &mut fired, now) {
                        let retry_until = trigger.retry_until();
                        // Сессия уже закончилась (процесс запущен позже): одно сообщение
                        // об открытии, плановые пересмотры прошедших часов — без шума.
                        let quiet = late
                            && trigger.offset_seconds != 0
                            && trigger.session_end.is_some_and(|end| end <= now);
                        let event = trigger.event();
                        // После перезапуска уже увиденное сегодня открытие не повторяется.
                        if late && runtime.event_seen(&event.id).await? {
                            continue;
                        }
                        let handled = if quiet {
                            tracing::debug!(event_id = %event.id, "review of a finished session skipped");
                            runtime.note_missed(event.clone()).await?;
                            Handled::Skipped
                        } else if late {
                            warn!(event_id = %event.id, "session event missed: the computer was asleep or the process was busy");
                            runtime.note_missed(event.clone()).await?;
                            Handled::Deferred
                        } else if !ready {
                            warn!(event_id = %event.id, "decision uncovered: market data stream is reconnecting");
                            runtime.absorb(event.clone()).await?;
                            Handled::Deferred
                        } else {
                            runtime.handle(event.clone()).await?
                        };
                        if handled == Handled::Deferred {
                            if let Some(until) = retry_until {
                                runtime.defer_open(event, until, now);
                            }
                        }
                    }
                    if ready {
                        runtime.retry_deferred_open(now).await?;
                    }
                }
                _ = risk_tick.tick() => {
                    let now = Utc::now();
                    // Risk-check не просит стратегию менять сигнал; он обеспечивает
                    // регулярный mark-to-market контроль даже при отсутствии свечей.
                    runtime.handle(StrategyEvent {
                        id: format!("risk-check:{}", now.timestamp()),
                        phase: EventPhase::Live,
                        occurred_at: now,
                        payload: StrategyEventKind::RiskCheck,
                    }).await?;
                }
                _ = ai_tick.tick(), if !runtime.ai_jobs.is_empty() => {
                    let now = Utc::now();
                    if market_stream.handle.is_ready() && sessions.iter().any(|(start, end)| *start <= now && now < *end) {
                        runtime.watch_models(now).await?;
                    }
                }
                _ = control_tick.tick() => {
                    match runtime.poll_controls().await {
                        Ok(()) => {}
                        Err(error) if error.is_transient() => runtime.health.failure("account commands", &error),
                        Err(error) => return Err(error),
                    }
                }
            }
        }
    }
}

/// Следит за разрывами market data stream и закрывает пробелы в данных стратегии.
///
/// Поток не доставляет свечи, закрывшиеся во время разрыва. Когда число
/// соединений выросло, свечи от последнего непрерывного момента загружаются
/// из истории и подаются как Warmup; повторный event id engine отбрасывает.
struct StreamContinuity {
    connections: u64,
    continuous_until: DateTime<Utc>,
}

impl StreamContinuity {
    fn new(handle: &MarketDataStreamHandle) -> Self {
        Self {
            connections: handle.connection_count(),
            continuous_until: Utc::now(),
        }
    }

    async fn restore(
        &mut self,
        runtime: &mut LiveRuntime,
        requirements: &[DataRequirement],
        handle: &MarketDataStreamHandle,
    ) -> Result<()> {
        let connections = handle.connection_count();
        if connections != self.connections {
            // Ошибка догрузки завершает сессию: она будет пересоздана с полным
            // прогревом, а не продолжит торговать на данных с пробелом.
            catch_up_warmup(
                &runtime.engine,
                runtime.broker.as_ref(),
                requirements,
                self.continuous_until,
                &mut runtime.latest_prices,
            )
            .await?;
            info!(gap_started = %self.continuous_until, "market data reconnected; missed candles restored from history");
            self.connections = connections;
        }
        self.continuous_until = Utc::now();
        Ok(())
    }
}

/// Итог обработки события: нужен ли повтор решения открытия сессии.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Handled {
    /// Стратегия получила planning context; повтор не нужен.
    Decided,
    /// Решение не принято по временной причине: торги, сеть, незавершённый пакет.
    Deferred,
    /// Решение не нужно: стратегия выключена или счёт закрывает позиции.
    Skipped,
}

/// Отложенное решение открытия сессии, которое повторяется до конца этой сессии.
struct PendingOpen {
    event: StrategyEvent,
    until: DateTime<Utc>,
    attempts: u32,
    next_at: DateTime<Utc>,
}

/// Зависимости живого цикла. Тип брокера фиксирован как `VenueBroker`.
struct LiveRuntime {
    account_id: AccountId,
    strategy_id: StrategyId,
    universe: Vec<InstrumentId>,
    instruments: Vec<InstrumentInfo>,
    latest_prices: HashMap<InstrumentId, Price>,
    api: VenueApi,
    broker: Arc<VenueBroker>,
    storage: Arc<PostgresStorage>,
    engine: PortfolioEngine<PostgresStorage>,
    executor: BatchExecutor<VenueBroker, VenueBroker, PostgresStorage>,
    /// Отдельное PostgreSQL-соединение, удерживающее account advisory lock.
    runtime_lease: RuntimeLease,
    risk: RiskManager,
    /// Учёт простоя сети: решения без сети пропускаются, процесс продолжает работу.
    health: resilience::NetworkHealth,
    /// Торговая дата из события расписания, которое не удалось обработать без сети.
    /// Следующее событие применит её, чтобы дневной лимит не остался во вчерашнем дне.
    pending_session_date: Option<NaiveDate>,
    /// Открытие сессии, решение которого ещё не принято.
    pending_open: Option<PendingOpen>,
    /// Компоненты `ai-model` и сессии, по которым решение модели уже получено.
    ai_jobs: Vec<ai::AiJob>,
    /// Пересмотры по расписанию (компонента, время), по которым решение уже получено.
    ai_done: HashSet<(usize, DateTime<Utc>)>,
    ai_failures: HashMap<DateTime<Utc>, u32>,
    /// Сторож внеочередных решений: скачки цен и всплески новостей.
    ai_watch: ai::Watch,
    /// Комиссии контура и порог минимальной сделки; обновляются на открытии сессии.
    costs: instruments::ExecutionCosts,
}

impl LiveRuntime {
    /// Обрабатывает live-событие и переживает недоступность сети.
    ///
    /// Временная ошибка сети/API или отказ риск-проверки (например, устаревшая
    /// котировка) не завершает runtime: торговое решение этого события
    /// пропускается, свеча и границы сессии сохраняются в состоянии стратегии,
    /// а следующее событие заново читает портфель и восстанавливает незавершённые
    /// пакеты заявок. Остальные ошибки по-прежнему останавливают процесс.
    async fn handle(&mut self, event: StrategyEvent) -> Result<Handled> {
        if let StrategyEventKind::Schedule(ScheduleEvent {
            anchor: ScheduleAnchor::SessionOpen,
            session_date,
            scheduled_at,
        }) = &event.payload
        {
            // Ставки комиссии наблюдатель уточняет по фактическим списаниям.
            self.costs =
                instruments::ExecutionCosts::load(&self.storage, self.api.contour().label()).await;
            self.costs.apply(&mut self.instruments);
            // Свежий снимок справочных данных подаётся до решения этой сессии;
            // при сбое стратегия работает на прошлом снимке.
            if let Err(error) = absorb_reference(
                &self.api,
                &self.engine,
                &share_ids(&self.instruments),
                *session_date,
            )
            .await
            {
                warn!(%error, "reference data unavailable; strategy keeps the previous snapshot");
            }
            // Решение ИИ-модели нужно до решения сессии; сбой повторяется через минуту.
            if !self.consult_models(*session_date, *scheduled_at).await? {
                self.pending_session_date = self.pending_session_date.max(Some(*session_date));
                return Ok(Handled::Deferred);
            }
        }
        let result = self.process(event.clone()).await;
        if result.is_err() {
            if let StrategyEventKind::Schedule(schedule) = &event.payload {
                self.pending_session_date =
                    self.pending_session_date.max(Some(schedule.session_date));
            }
        } else if self
            .pending_session_date
            .is_some_and(|date| date <= self.risk.state().session_date)
        {
            self.pending_session_date = None;
        }
        match result {
            Ok(handled) => {
                self.health.success();
                Ok(handled)
            }
            Err(error) if error.is_transient() => {
                self.health.failure(&event.id, &error);
                if self.health.needs_rebuild() {
                    // Долгий простой: соединения пересоздаются вместе с сессией,
                    // а после возврата сети стратегия заново прогревается.
                    return Err(error);
                }
                self.absorb(event).await?;
                Ok(Handled::Deferred)
            }
            Err(Error::RiskRejected(reason)) => {
                // Плановая проверка риска — не торговое решение: её отказ не шумит в журнале.
                if is_risk_check(&event) {
                    tracing::debug!(event_id = %event.id, %reason, "risk check skipped execution data");
                } else {
                    warn!(event_id = %event.id, %reason, "decision uncovered: risk check rejected execution; runtime continues");
                }
                self.absorb(event).await?;
                Ok(Handled::Deferred)
            }
            Err(error) => Err(error),
        }
    }

    /// Учитывает пропущенное событие расписания. Открытие только переносит
    /// торговую дату риска: intraday-сигналы не включаются посреди сессии.
    /// Закрытие подаётся как Warmup и снимает intraday-вклад.
    async fn note_missed(&mut self, event: StrategyEvent) -> Result<()> {
        if let StrategyEventKind::Schedule(ScheduleEvent {
            anchor: ScheduleAnchor::SessionOpen,
            session_date,
            ..
        }) = &event.payload
        {
            self.pending_session_date = self.pending_session_date.max(Some(*session_date));
            return Ok(());
        }
        self.absorb(event).await
    }

    /// Получает решения моделей для компонент `ai-model` к пересмотру `scheduled_at`.
    ///
    /// Каждая компонента опрашивается один раз за пересмотр (открытие или плановый
    /// час). Возвращает `false`, если решение нужно повторить: сбой модели или сети,
    /// не больше `AI_ATTEMPTS` попыток. Затем компонента сохраняет позиции. Решение
    /// «держать» не подаётся стратегии: цель на этом пересмотре не появится.
    async fn consult_models(
        &mut self,
        session_date: NaiveDate,
        scheduled_at: DateTime<Utc>,
    ) -> Result<bool> {
        if self.ai_jobs.is_empty() || !self.control_settings().await?.enabled {
            return Ok(true);
        }
        let mut ready = true;
        for index in 0..self.ai_jobs.len() {
            let job = self.ai_jobs[index].clone();
            let now = Utc::now();
            if self.ai_done.contains(&(index, scheduled_at))
                || !self.ai_watch.allowed(index, &job, now, false)
            {
                continue;
            }
            // Первый пересмотр торговой даты — открытие сессии, остальные — плановые.
            let opening = !self
                .ai_done
                .iter()
                .any(|(_, time)| time.date_naive() == session_date);
            if opening {
                self.review_memory(&job).await;
            }
            let trigger = ai::Trigger::new(if opening { "session_open" } else { "scheduled" }, "");
            if let Some(run) = self.ask_model(index, &job, session_date, trigger).await? {
                if !run.hold {
                    let event = StrategyEvent {
                        id: format!(
                            "advice:{session_date}:{index}:{}",
                            Utc::now().timestamp_millis()
                        ),
                        phase: EventPhase::Warmup,
                        occurred_at: run.advice.observed_at,
                        payload: StrategyEventKind::Advice(run.advice.clone()),
                    };
                    self.engine.process_event(&event, None).await?;
                }
                self.ai_done.insert((index, scheduled_at));
            } else {
                let failures = self.ai_failures.entry(scheduled_at).or_default();
                *failures += 1;
                if *failures < AI_ATTEMPTS {
                    ready = false;
                } else {
                    warn!(
                        "ai model decision failed; the ai component keeps its positions until the next review"
                    );
                    self.ai_done.insert((index, scheduled_at));
                }
            }
        }
        Ok(ready)
    }

    /// Перед первым решением дня: итоги прошлых решений против индекса,
    /// измеренная чувствительность бумаг к темам и, раз в неделю, разбор с
    /// выводами. Сбой не мешает самому решению.
    async fn review_memory(&self, job: &ai::AiJob) {
        match ai::compute_outcomes(
            &self.storage,
            self.broker.as_ref(),
            self.strategy_id.as_str(),
        )
        .await
        {
            Ok(count) if count > 0 => info!(count, "ai decision outcomes computed"),
            Ok(_) => {}
            Err(error) => warn!(%error, "ai decision outcomes skipped"),
        }
        match ai::calibrate(&self.storage, self.broker.as_ref()).await {
            Ok(count) if count > 0 => info!(count, "exposure calibration suggestions computed"),
            Ok(_) => {}
            Err(error) => warn!(%error, "exposure calibration skipped"),
        }
        let meter = ai::usage::meter();
        let result = ai::review_if_due(&self.storage, self.strategy_id.as_str(), job, &meter).await;
        if let Err(error) = ai::usage::store(
            &self.storage,
            "reflection",
            Some(self.strategy_id.as_str()),
            &ai::usage::snapshot(&meter),
        )
        .await
        {
            warn!(%error, "ai review usage was not recorded");
        }
        match result {
            Ok(Some(summary)) => info!(%summary, "ai model reviewed its results"),
            Ok(None) => {}
            Err(error) => warn!(%error, "ai model review skipped"),
        }
    }

    /// Один запрос к модели с записью в журнал. `None` — модель не ответила.
    async fn ask_model(
        &mut self,
        index: usize,
        job: &ai::AiJob,
        session_date: NaiveDate,
        trigger: ai::Trigger,
    ) -> Result<Option<ai::AdviceRun>> {
        let started = std::time::Instant::now();
        let context = ai::Context {
            api: &self.api,
            broker: self.broker.as_ref(),
            account_id: &self.account_id,
            storage: Some(self.storage.as_ref()),
            strategy_id: self.strategy_id.as_str(),
            persist_memory: true,
        };
        let meter = ai::usage::meter();
        ai::activity_started(&self.storage, self.strategy_id.as_str(), job, &trigger).await;
        let result = ai::advise(&context, job, session_date, trigger.clone(), &meter).await;
        ai::activity_finished(
            &self.storage,
            self.strategy_id.as_str(),
            if result.is_ok() { "ok" } else { "failed" },
        )
        .await;
        self.ai_watch.decided(index, Utc::now());
        let spend = ai::usage::snapshot(&meter);
        ai::record(
            &self.storage,
            &ai::Entry {
                strategy_id: self.strategy_id.as_str(),
                account_id: self.account_id.as_str(),
                kind: "decision",
                session_date: Some(session_date),
                job,
                trigger: &trigger,
                duration: started.elapsed(),
                spend: &spend,
            },
            result.as_ref(),
        )
        .await?;
        match result {
            Ok(run) => {
                let summary = run
                    .weights
                    .iter()
                    .map(|w| {
                        format!(
                            "{}={}%",
                            w["ticker"].as_str().unwrap_or("?"),
                            w["weight_percent"]
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                info!(model = %run.advice.model, trigger = trigger.kind, hold = run.hold, weights = %summary, tools = run.tool_calls.len(), "ai model decision received");
                Ok(Some(run))
            }
            Err(error) => {
                warn!(%error, trigger = trigger.kind, "ai model decision failed");
                Ok(None)
            }
        }
    }

    /// Раз в минуту в торговую сессию: всплеск новостей или скачок цены вызывает
    /// внеочередное решение модели, которое исполняется сразу через обычный
    /// risk pipeline (`Advice` в live-фазе).
    async fn watch_models(&mut self, now: DateTime<Utc>) -> Result<()> {
        if self.ai_jobs.is_empty() || !self.control_settings().await?.enabled {
            return Ok(());
        }
        let context = ai::Context {
            api: &self.api,
            broker: self.broker.as_ref(),
            account_id: &self.account_id,
            storage: Some(self.storage.as_ref()),
            strategy_id: self.strategy_id.as_str(),
            persist_memory: true,
        };
        let triggers = match self.ai_watch.check(&context, &self.ai_jobs, now).await {
            Ok(triggers) => triggers,
            Err(error) if error.is_transient() => {
                self.health.failure("ai watch", &error);
                return Ok(());
            }
            Err(error) => {
                warn!(%error, "ai watch skipped this minute");
                return Ok(());
            }
        };
        let session_date = self.known_session_date();
        // Дневной лимит расходов останавливает только внеочередные решения:
        // плановые пересмотры продолжаются.
        let budget = if triggers.is_empty() {
            None
        } else {
            Some(ai::usage::budget(&self.storage).await?)
        };
        for (index, trigger) in triggers {
            let job = self.ai_jobs[index].clone();
            if let Some(budget) = budget.filter(ai::usage::BudgetState::exhausted) {
                // Пауза как после решения: повод не повторяется каждую минуту.
                self.ai_watch.decided(index, now);
                let error = Error::Configuration(format!(
                    "daily AI budget exhausted: {:.0} of {:.0} RUB spent; out-of-schedule decisions resume tomorrow",
                    budget.spent_rub,
                    budget.limit_rub.unwrap_or_default()
                ));
                warn!(%error, trigger = trigger.kind, "ai model not asked out of schedule");
                ai::record(
                    &self.storage,
                    &ai::Entry {
                        strategy_id: self.strategy_id.as_str(),
                        account_id: self.account_id.as_str(),
                        kind: "decision",
                        session_date: Some(session_date),
                        job: &job,
                        trigger: &trigger,
                        duration: std::time::Duration::ZERO,
                        spend: &ai::usage::Spend::default(),
                    },
                    Err(&error),
                )
                .await?;
                continue;
            }
            info!(trigger = trigger.kind, details = %trigger.details, "ai model asked out of schedule");
            let Some(run) = self
                .ask_model(index, &job, session_date, trigger.clone())
                .await?
            else {
                continue;
            };
            if run.hold {
                continue;
            }
            let event = StrategyEvent {
                id: format!("advice:{}:{index}:{}", trigger.kind, now.timestamp_millis()),
                phase: EventPhase::Live,
                occurred_at: run.advice.observed_at,
                payload: StrategyEventKind::Advice(run.advice),
            };
            self.handle(event).await?;
        }
        Ok(())
    }

    /// Было ли событие уже учтено стратегией (до перезапуска процесса).
    async fn event_seen(&self, event_id: &str) -> Result<bool> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM processed_events WHERE strategy_id=$1 AND event_id=$2)",
        )
        .bind(self.strategy_id.as_str())
        .bind(event_id)
        .fetch_one(self.storage.pool())
        .await?)
    }

    /// Запоминает открытие сессии, решение по которому нужно повторить.
    fn defer_open(&mut self, event: StrategyEvent, until: DateTime<Utc>, now: DateTime<Utc>) {
        if now >= until {
            warn!(event_id = %event.id, "session-open decision missed; the next decision waits for the next session open");
            return;
        }
        if self
            .pending_open
            .as_ref()
            .is_some_and(|pending| pending.event.id == event.id)
        {
            return;
        }
        info!(event_id = %event.id, %until, "session-open decision deferred; retrying every minute until the session closes");
        self.pending_open = Some(PendingOpen {
            event,
            until,
            attempts: 0,
            next_at: now + Duration::seconds(OPEN_RETRY_SECONDS),
        });
    }

    /// Повторяет отложенное решение открытия, пока идёт та же сессия.
    ///
    /// Повтор получает новый id, а payload сохраняет дату сессии: месячная
    /// стратегия отмечает месяц только после реального target, поэтому успешный
    /// повтор не исполнит решение дважды.
    async fn retry_deferred_open(&mut self, now: DateTime<Utc>) -> Result<()> {
        let Some(pending) = self.pending_open.as_mut() else {
            return Ok(());
        };
        if now >= pending.until {
            warn!(event_id = %pending.event.id, attempts = pending.attempts, "deferred session-open decision expired; the next decision waits for the next session open");
            self.pending_open = None;
            return Ok(());
        }
        if now < pending.next_at {
            return Ok(());
        }
        pending.attempts += 1;
        pending.next_at = now + Duration::seconds(OPEN_RETRY_SECONDS);
        let event = StrategyEvent {
            id: format!("{}:retry:{}", pending.event.id, pending.attempts),
            phase: EventPhase::Live,
            occurred_at: now,
            payload: pending.event.payload.clone(),
        };
        info!(event_id = %event.id, "retrying deferred session-open decision");
        match self.handle(event).await? {
            Handled::Deferred => {}
            handled => {
                info!(?handled, "deferred session-open decision completed");
                self.pending_open = None;
            }
        }
        Ok(())
    }

    /// Текущая торговая дата риска с учётом пропущенного из-за сети открытия сессии.
    fn known_session_date(&self) -> NaiveDate {
        let current = self.risk.state().session_date;
        self.pending_session_date
            .map_or(current, |date| date.max(current))
    }

    /// Сохраняет событие в состоянии стратегии без права торговать.
    ///
    /// Свечи и границы сессии подаются как Warmup, чтобы индикаторы и intraday
    /// gate не потеряли историю. Engine отбрасывает повторный event id, поэтому
    /// событие, уже зафиксированное до сбоя, не будет применено дважды.
    async fn absorb(&self, event: StrategyEvent) -> Result<()> {
        if matches!(
            &event.payload,
            StrategyEventKind::Candle(_)
                | StrategyEventKind::Schedule(ScheduleEvent {
                    anchor: ScheduleAnchor::SessionOpen | ScheduleAnchor::SessionClose,
                    ..
                })
        ) {
            let mut warmup = event;
            warmup.phase = EventPhase::Warmup;
            self.engine.process_event(&warmup, None).await?;
        }
        Ok(())
    }

    /// Обрабатывает одно live-событие через единый risk-first pipeline.
    ///
    /// Сначала читается свежий портфель и сохраняется решение риска. Затем
    /// восстанавливаются старые заявки; если они изменили позиции, risk observation
    /// строится повторно. Только после этого стратегия получает planning context.
    async fn process(&mut self, event: StrategyEvent) -> Result<Handled> {
        // Проверка выполняется до чтения портфеля, recovery и любой новой
        // отправки. Ошибка heartbeat завершает run, потому что эксклюзивное
        // владение счётом больше нельзя доказать.
        self.runtime_lease.check().await?;
        let mut portfolio = self.broker.portfolio(&self.account_id).await?.data;
        // Пополнения и выводы вычитаются из P&L с момента anchor. Неизвестный тип
        // внешнего движения нельзя безопасно нормализовать, поэтому он включает close-only.
        let (external_cash_flows, recognized) =
            scan_external_cash_flows(&self.api, self.risk.state()).await?;
        let mut decision = self.risk.observe(&RiskObservation {
            current_equity: rub_equity(&portfolio)?,
            session_date: event_session_date(&event, self.known_session_date()),
            observed_at: Utc::now(),
            external_cash_flows,
            external_cash_flows_recognized: recognized,
            portfolio_is_flat: portfolio_is_flat(&portfolio),
        })?;
        // Состояние стопов сохраняется до работы с заявками: перезапуск процесса
        // не должен забывать уже сработавшую блокировку.
        self.storage.save_risk_state(&decision.state).await?;
        self.sample_performance(&portfolio, &decision).await?;
        self.apply_execution_budget(&mut decision).await?;
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
                session_date: event_session_date(&event, self.known_session_date()),
                observed_at: Utc::now(),
                external_cash_flows,
                external_cash_flows_recognized: recognized,
                portfolio_is_flat: portfolio_is_flat(&portfolio),
            })?;
            self.storage.save_risk_state(&decision.state).await?;
            self.apply_execution_budget(&mut decision).await?;
        }

        let controls = self.control_settings().await?;
        if decision.mode == RiskMode::CloseOnly || controls.cleanup_requested {
            self.cancel_batches(None).await?;
            match self.protective_close(controls.cleanup_requested).await {
                Ok(()) => {}
                Err(Error::RiskRejected(error)) => {
                    warn!(%error, "protective close remains pending");
                }
                Err(error) => return Err(error),
            }
            return Ok(Handled::Skipped);
        }
        if !controls.enabled {
            let mut warmup = event;
            warmup.phase = EventPhase::Warmup;
            self.engine.process_event(&warmup, None).await?;
            return Ok(Handled::Skipped);
        }

        // Single-flight barrier: новый target нельзя планировать, пока исход
        // предыдущего batch ещё не доказан. Свечи и session boundaries всё же
        // применяем как Warmup: индикаторы не теряют историю, а intraday open/close
        // gate не остаётся в прошлом дне. Warmup физически не может создать batch.
        // Торговый сигнал этого события намеренно не откладывается: после fill он
        // уже мог устареть. Остальные Schedule/RiskCheck пропускаются.
        let awaiting_recovery = recovered
            .iter()
            .any(|(_, outcome)| matches!(outcome, BatchExecutionOutcome::AwaitingFills));
        let own_unfinished = self
            .storage
            .pending_batch_ids(&self.account_id, &self.strategy_id)
            .await?;
        let account_unfinished = self
            .storage
            .has_unfinished_batches(&self.account_id)
            .await?;
        if account_unfinished && own_unfinished.is_empty() {
            return Err(Error::Storage(format!(
                "account `{}` has an unfinished batch owned by another strategy",
                self.account_id
            )));
        }
        if awaiting_recovery
            || account_unfinished
            || self.storage.has_pending_orders(&self.account_id).await?
        {
            warn!(
                event_id = %event.id,
                "new trading decision deferred while a durable batch is unfinished"
            );
            if matches!(
                &event.payload,
                StrategyEventKind::Candle(_)
                    | StrategyEventKind::Schedule(ScheduleEvent {
                        anchor: ScheduleAnchor::SessionOpen | ScheduleAnchor::SessionClose,
                        ..
                    })
            ) {
                let mut warmup_event = event;
                warmup_event.phase = EventPhase::Warmup;
                self.engine.process_event(&warmup_event, None).await?;
            }
            return Ok(Handled::Deferred);
        }
        // Непрерывные фьючерсы переходят на следующий контракт до решения; после
        // переноса сессия пересоздаётся и привязывается к новому контракту.
        if self.roll_futures().await? {
            return Err(Error::StreamClosed(
                "futures series moved to the next contract; the session restarts on it".to_owned(),
            ));
        }
        self.latest_prices = refresh_execution_data(&self.api, &mut self.instruments).await?;
        if self
            .instruments
            .iter()
            .any(|i| !i.api_trade_available || !i.market_order_available)
        {
            // Вне торгов это нормально: проверка риска не шумит, решение ждёт повтора.
            if is_risk_check(&event) {
                tracing::debug!(event_id = %event.id, "risk check outside trading hours");
            } else {
                warn!(event_id = %event.id, "decision uncovered: market orders unavailable");
            }
            let mut warmup = event;
            warmup.phase = EventPhase::Warmup;
            self.engine.process_event(&warmup, None).await?;
            return Ok(Handled::Deferred);
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
            min_trade_fraction: self.costs.min_trade_fraction,
        };
        // Engine атомарно сохраняет target и legs. Executor получает только ID уже
        // сохранённого batch, поэтому процесс можно безопасно восстановить после сбоя.
        let outcome = self.engine.process_event(&event, Some(&context)).await?;
        if is_risk_check(&event) && outcome.target.is_none() && outcome.intents.is_empty() {
            tracing::debug!(event_id = %event.id, "risk check passed; no target change");
        } else {
            info!(event_id = %event.id, duplicate = outcome.duplicate, target = outcome.target.is_some(), legs = outcome.intents.len(), "live decision observed");
        }
        if let Some(batch) = outcome.batch {
            let execution = if context.risk.mode == RiskMode::CloseOnly {
                self.executor.execute_batch_close_only(batch.id).await?
            } else {
                self.executor.execute_batch(batch.id).await?
            };
            info!(batch_id = %batch.id, ?execution, "rebalance batch processed");
        }
        Ok(Handled::Decided)
    }
}

/// Плановая проверка риска раз в 30 секунд: не торговое решение стратегии.
fn is_risk_check(event: &StrategyEvent) -> bool {
    matches!(event.payload, StrategyEventKind::RiskCheck)
}

/// Execution prices are queried independently from daily/monthly signal history.
async fn refresh_execution_data(
    api: &VenueApi,
    instruments: &mut [InstrumentInfo],
) -> Result<HashMap<InstrumentId, Price>> {
    // Серия котируется по привязанному контракту.
    let ids = instruments
        .iter()
        .map(|i| api.series().contract(&i.instrument_id).to_string())
        .collect::<Vec<_>>();
    let mut response = api
        .market_data()
        .get_last_prices(tinkoff_rs::proto::GetLastPricesRequest {
            instrument_id: ids.clone(),
            ..Default::default()
        })
        .await?
        .into_inner();
    for price in &mut response.last_prices {
        if let Ok(uid) = InstrumentId::new(price.instrument_uid.clone()) {
            price.instrument_uid = api.series().of_broker(uid).to_string();
        }
    }
    let statuses = api
        .market_data()
        .get_trading_statuses(tinkoff_rs::proto::GetTradingStatusesRequest { instrument_id: ids })
        .await?
        .into_inner()
        .trading_statuses;
    for instrument in instruments.iter_mut() {
        let contract = api.series().contract(&instrument.instrument_id);
        let status = statuses
            .iter()
            .find(|s| s.instrument_uid == contract.as_str());
        instrument.api_trade_available = status.is_some_and(|s| s.api_trade_available_flag);
        instrument.market_order_available = status.is_some_and(|s| s.market_order_available_flag);
    }
    if instruments
        .iter()
        .any(|i| !i.api_trade_available || !i.market_order_available)
    {
        return Ok(HashMap::new());
    }
    execution_prices(&response.last_prices, instruments)
}

fn execution_prices(
    prices: &[tinkoff_rs::proto::LastPrice],
    instruments: &[InstrumentInfo],
) -> Result<HashMap<InstrumentId, Price>> {
    instruments
        .iter()
        .map(|instrument| {
            let value = prices
                .iter()
                .find(|p| p.instrument_uid == instrument.instrument_id.as_str())
                .ok_or_else(|| {
                    Error::RiskRejected(format!(
                        "current price missing for {}",
                        instrument.instrument_id
                    ))
                })?;
            let price = Price::from_recent_trade(value, Utc::now())?;
            Ok((instrument.instrument_id.clone(), price))
        })
        .collect()
}

/// Определяет торговую дату для дневного stop.
///
/// Только официальное событие расписания меняет торговую дату. UTC-дата свечи
/// не подходит для вечерних/специальных сессий MOEX; candle и risk-check поэтому
/// сохраняют уже определённую календарём runtime дату.
fn event_session_date(event: &StrategyEvent, current: NaiveDate) -> NaiveDate {
    match &event.payload {
        StrategyEventKind::Schedule(schedule) => schedule.session_date,
        StrategyEventKind::Candle(_)
        | StrategyEventKind::Order(_)
        | StrategyEventKind::RiskCheck
        | StrategyEventKind::Reference(_)
        | StrategyEventKind::Advice(_) => current,
    }
}

/// Восстанавливает существующую risk epoch либо создаёт первую от текущей equity.
/// После создания сразу выполняется observation, чтобы внешний перевод между anchor
/// и запуском процесса не оказался ошибочно принят за торговую прибыль или убыток.
async fn initialize_risk(
    api: &VenueApi,
    storage: &PostgresStorage,
    config: &RiskConfig,
    ids: &RuntimeIds,
    portfolio: &PortfolioSnapshot,
    current_session_date: NaiveDate,
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
                session_date: current_session_date,
                observed_at: now,
            },
        )?
    };
    let (external_cash_flows, recognized) = scan_external_cash_flows(api, manager.state()).await?;
    let decision = manager.observe(&RiskObservation {
        current_equity: rub_equity(portfolio)?,
        // После ночного restart нельзя сравнивать equity с базой вчерашней
        // сессии. Официальный календарь обновляет day_start до recovery/orders.
        session_date: current_session_date,
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
/// и без активных заявок у брокера. Это исключает смену anchor посреди исполнения.
async fn risk_reset(config: &BotConfig, ids: &RuntimeIds, reason: &str) -> Result<()> {
    let storage = connect_storage(config).await?;
    storage.migrate().await?;
    // Тот же account-level lock, что у `run`, делает reset и смену владельца
    // невозможными, пока старый процесс ещё работает.
    let mut runtime_lease = storage.acquire_runtime_lease(&ids.account_id).await?;
    let (api, broker) = connect_venue(config).await?;
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
    if !api.active_orders(&ids.account_id).await?.is_empty() {
        return Err(Error::RiskRejected(
            "risk-reset requires no active broker orders".to_owned(),
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
    if storage.has_unfinished_batches(&ids.account_id).await? {
        return Err(Error::RiskRejected(
            "risk-reset requires no unfinished rebalance batches for the account".to_owned(),
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
    if previous_strategy_id != ids.strategy_id
        && storage
            .load_portfolio_strategy_state(&ids.strategy_id)
            .await?
            .is_some()
    {
        return Err(Error::RiskRejected(format!(
            "strategy_id `{}` already has durable state; use a fresh strategy_id for ownership transfer",
            ids.strategy_id
        )));
    }
    let mut manager = RiskManager::from_state(previous_config, state)?;
    let now = Utc::now();
    let current_session_date =
        current_moex_session_date(&api, now, Some(manager.state().session_date)).await?;
    let decision = manager.reset(
        config.risk.clone(),
        RiskAnchor {
            account_id: ids.account_id.clone(),
            strategy_id: ids.strategy_id.clone(),
            account_equity: rub_equity(&portfolio)?,
            session_date: current_session_date,
            observed_at: now,
        },
        reason,
        false,
    )?;
    // `account_risk_state.strategy_id` является FK на strategy_instances. Новый
    // immutable ID регистрируется только после всех risk-проверок, но до CAS
    // передачи владельца. Повтор с тем же ID допустим лишь для того же config.
    let replacement_strategy = WeightedEnsembleStrategy::new(
        ids.strategy_id.clone(),
        ids.account_id.clone(),
        ids.universe.clone(),
        config.strategies.clone(),
    )?;
    // Сетевые проверки выше могли занять достаточно времени для разрыва idle
    // session. Перед изменением владельца ещё раз доказываем владение lease.
    runtime_lease.check().await?;
    storage
        .register_portfolio_strategy(
            &ids.strategy_id,
            &ids.account_id,
            replacement_strategy.kind(),
            &replacement_strategy.config()?,
        )
        .await?;
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

/// Проверяет, что счёт доступен токену; реальный счёт — открыт и с полным доступом.
async fn ensure_account_exists(broker: &VenueBroker, account_id: &AccountId) -> Result<()> {
    venue::ensure_account(broker, account_id).await
}

/// Загружает метаданные инструментов и проверяет их до начала live-цикла.
///
/// Торговля разрешается RUB-инструментами MOEX поддерживаемых типов (акции,
/// фонды, облигации, фьючерсы, валюта) с корректным размером лота, доступными
/// API market orders и, для long/short стратегий, разрешённым шортом. Стратегии
/// на отчётности и дивидендах получают только акции.
async fn inspect_instruments(
    api: &VenueApi,
    broker: &VenueBroker,
    universe: &[InstrumentId],
    requirements: &[DataRequirement],
    requires_short: bool,
    shares_only: &HashSet<InstrumentId>,
) -> Result<Vec<InstrumentInfo>> {
    // Инструменты только для данных (ориентир рынка) тоже должны быть рублёвыми MOEX.
    let mut seen = HashSet::new();
    for requirement in requirements {
        if universe.contains(&requirement.instrument_id)
            || !seen.insert(requirement.instrument_id.clone())
        {
            continue;
        }
        let contract = api.series().contract(&requirement.instrument_id);
        let instrument = api
            .instruments()
            .get_instrument_by(InstrumentRequest {
                id_type: InstrumentIdType::Uid as i32,
                class_code: None,
                id: contract.to_string(),
            })
            .await?
            .into_inner()
            .instrument
            .ok_or_else(|| {
                Error::Configuration(format!(
                    "instrument `{}` was not returned by T-Invest",
                    requirement.instrument_id
                ))
            })?;
        if instrument.uid != contract.as_str()
            || instrument.real_exchange != RealExchange::Moex as i32
            || !instrument.currency.eq_ignore_ascii_case("rub")
        {
            return Err(Error::Configuration(format!(
                "instrument `{}` must be a RUB instrument executed on MOEX",
                requirement.instrument_id
            )));
        }
    }
    let mut result = Vec::with_capacity(universe.len());
    for instrument_id in universe {
        if !requirements
            .iter()
            .any(|r| &r.instrument_id == instrument_id)
        {
            return Err(Error::Configuration(format!(
                "tradable instrument `{instrument_id}` has no data requirement"
            )));
        }
        let (info, _) = instruments::describe(api, broker, instrument_id).await?;
        if requires_short && !info.short_enabled {
            return Err(Error::Configuration(format!(
                "tradable instrument `{instrument_id}` lacks the required short availability"
            )));
        }
        if shares_only.contains(instrument_id) && info.kind != InstrumentKind::Share {
            return Err(Error::Configuration(format!(
                "`{instrument_id}`: strategies computed from company reports and dividends trade shares only"
            )));
        }
        result.push(info);
    }
    Ok(result)
}

/// Инструменты компонент, которые считают сигнал по отчётности и дивидендам.
fn share_only_instruments(config: &BotConfig, ids: &RuntimeIds) -> HashSet<InstrumentId> {
    config
        .strategies
        .iter()
        .filter(|component| {
            matches!(
                component.strategy.kind(),
                "value-composite"
                    | "quality-profitability"
                    | "value-momentum"
                    | "multi-factor"
                    | "dividend-month"
            )
        })
        .flat_map(|component| {
            component
                .instruments
                .clone()
                .unwrap_or_else(|| ids.universe.clone())
        })
        .collect()
}

/// Акции universe: только для них есть мультипликаторы и дивиденды.
fn share_ids(instruments: &[InstrumentInfo]) -> Vec<InstrumentId> {
    instruments
        .iter()
        .filter(|info| info.kind == InstrumentKind::Share)
        .map(|info| info.instrument_id.clone())
        .collect()
}

/// Сообщает preflight, требуется ли стратегии возможность открывать короткие позиции.
fn strategy_requires_short(strategy: &BuiltinStrategySpec) -> bool {
    match strategy {
        // Модель открывает short только с явным allow_short.
        BuiltinStrategySpec::AiModel(config) => config.allow_short,
        _ => !matches!(
            strategy.kind(),
            "low-volatility" | "equal-risk-contribution" | "volatility-managed"
        ),
    }
}

/// Компоненты `ai-model` конфига и их инструменты.
fn ai_jobs(config: &BotConfig, ids: &RuntimeIds) -> Vec<ai::AiJob> {
    config
        .strategies
        .iter()
        .filter_map(|component| match &component.strategy {
            BuiltinStrategySpec::AiModel(model) => Some(ai::AiJob {
                universe: component
                    .instruments
                    .clone()
                    .unwrap_or_else(|| ids.universe.clone()),
                config: model.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// `ai-preview`: решение модели без заявок; при `DATABASE_URL` пишется в журнал.
async fn ai_preview(config: &BotConfig, ids: &RuntimeIds) -> Result<()> {
    let jobs = ai_jobs(config, ids);
    if jobs.is_empty() {
        return Err(Error::Configuration(
            "the config has no ai-model strategy".to_owned(),
        ));
    }
    let storage = if std::env::var("DATABASE_URL").is_ok() {
        let storage = connect_storage(config).await?;
        storage.migrate().await?;
        Some(storage)
    } else {
        None
    };
    let (api, broker) = connect_venue(config).await?;
    let context = ai::Context {
        api: &api,
        broker: broker.as_ref(),
        account_id: &ids.account_id,
        storage: storage.as_deref(),
        strategy_id: ids.strategy_id.as_str(),
        persist_memory: false,
    };
    let session_date = Utc::now().date_naive();
    let mut out = Vec::new();
    let mut failed = None;
    for job in &jobs {
        let started = std::time::Instant::now();
        let trigger = ai::Trigger::new("preview", "");
        let meter = ai::usage::meter();
        let result = ai::advise(&context, job, session_date, trigger.clone(), &meter).await;
        if let Some(storage) = &storage {
            ai::record(
                storage,
                &ai::Entry {
                    strategy_id: ids.strategy_id.as_str(),
                    account_id: ids.account_id.as_str(),
                    kind: "preview",
                    session_date: Some(session_date),
                    job,
                    trigger: &trigger,
                    duration: started.elapsed(),
                    spend: &ai::usage::snapshot(&meter),
                },
                result.as_ref(),
            )
            .await?;
        }
        match result {
            Ok(run) => out.push(serde_json::to_value(&run).map_err(|error| {
                Error::Configuration(format!("cannot serialize preview: {error}"))
            })?),
            Err(error) => {
                out.push(serde_json::json!({"error": error.to_string()}));
                failed = Some(error);
            }
        }
    }
    println!("{}", serde_json::json!({"previews": out}));
    failed.map_or(Ok(()), Err)
}

/// Загружает минимальную историю закрытых свечей и прогревает состояние стратегии.
///
/// Свечи сортируются глобально по времени, чтобы многомерные стратегии увидели их
/// в причинном порядке. `PlanningContext = None` и `EventPhase::Warmup` запрещают
/// планирование/исполнение заявок во время воспроизведения истории.
async fn warm_up(
    engine: &PortfolioEngine<PostgresStorage>,
    broker: &VenueBroker,
    requirements: &[DataRequirement],
    cutoff: DateTime<Utc>,
    storage: &PostgresStorage,
    strategy_id: &StrategyId,
) -> Result<HashMap<InstrumentId, Price>> {
    // Each event marker and strategy state were committed atomically. Replaying
    // known warmup events after restore only serializes the full state repeatedly.
    let processed: HashSet<String> = sqlx::query_scalar::<_, String>(
        "SELECT event_id FROM processed_events WHERE strategy_id=$1 AND event_id LIKE 'candle:%'",
    )
    .bind(strategy_id.as_str())
    .fetch_all(storage.pool())
    .await?
    .into_iter()
    .collect();
    let mut candles = Vec::new();
    for requirement in requirements {
        if requirement.warmup_bars == 0 || requirement.warmup_bars > 2_500 {
            return Err(Error::Configuration(format!(
                "warmup for `{}` must contain 1..=2500 bars",
                requirement.instrument_id
            )));
        }
        let mut received = history::load(
            broker,
            CandleHistoryRequest {
                instrument_id: requirement.instrument_id.clone(),
                interval: requirement.interval,
                from: cutoff - warmup_span(requirement.interval, requirement.warmup_bars)?,
                to: cutoff,
                limit: None,
            },
            Some(requirement.warmup_bars),
        )
        .await?;
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
    let mut events = Vec::new();
    for candle in candles {
        latest_prices.insert(candle.instrument_id.clone(), candle.close);
        let event = candle_event(candle, EventPhase::Warmup);
        if !processed.contains(&event.id) {
            events.push(event);
        }
    }
    let total = events.len();
    for (index, chunk) in events.chunks(250).enumerate() {
        if !engine.process_warmup_batch(chunk).await? {
            return Err(Error::Storage(
                "warmup checkpoint conflicted with another event writer".to_owned(),
            ));
        }
        info!(
            completed = ((index + 1) * 250).min(total),
            total, "warmup progress"
        );
    }
    Ok(latest_prices)
}

/// Дополняет warmup свечами, закрывшимися во время startup-последовательности.
///
/// Нижняя граница сдвинута на один интервал назад, потому что API индексирует
/// свечу временем открытия: бар мог открыться до `from`, но закрыться после него.
/// Повторы безопасны благодаря event ID, а `Warmup` не разрешает торговый target.
async fn catch_up_warmup(
    engine: &PortfolioEngine<PostgresStorage>,
    broker: &VenueBroker,
    requirements: &[DataRequirement],
    from: DateTime<Utc>,
    latest_prices: &mut HashMap<InstrumentId, Price>,
) -> Result<()> {
    let to = Utc::now();
    let mut candles = Vec::new();
    for requirement in requirements {
        let mut received = history::load(
            broker,
            CandleHistoryRequest {
                instrument_id: requirement.instrument_id.clone(),
                interval: requirement.interval,
                from: from - candle_duration(requirement.interval),
                to,
                limit: None,
            },
            None,
        )
        .await?;
        received.retain(|candle| {
            candle.complete
                // `time` — начало бара. Для daily/monthly номинальная длина
                // интервала не совпадает с фактическим закрытием биржи, поэтому
                // authoritative флаг `complete` определяет верхнюю границу.
                // Условие ниже нужно лишь для короткого пересечения с cutoff;
                // лишний старый бар безвреден благодаря event-id дедупликации.
                && candle.time + candle_duration(candle.interval) > from
                && candle.time <= to
        });
        candles.extend(received);
    }
    candles.sort_by(|left, right| {
        left.time.cmp(&right.time).then_with(|| {
            left.instrument_id
                .as_str()
                .cmp(right.instrument_id.as_str())
        })
    });
    candles.dedup_by(|left, right| {
        left.time == right.time
            && left.interval == right.interval
            && left.instrument_id == right.instrument_id
    });
    for candle in candles {
        latest_prices.insert(candle.instrument_id.clone(), candle.close);
        engine
            .process_event(&candle_event(candle, EventPhase::Warmup), None)
            .await?;
    }
    Ok(())
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
/// номинальному закрытию. Дневная свеча может стать complete до `open + 24h`,
/// поэтому будущее номинальное время ограничивается фактом её получения.
fn candle_event(candle: Candle, phase: EventPhase) -> StrategyEvent {
    let interval = candle_interval_key(candle.interval);
    let occurred_at = (candle.time + candle_duration(candle.interval)).min(Utc::now());
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

/// Загружает справочные данные, если они нужны стратегии, и сохраняет их в state.
///
/// Событие подаётся в фазе Warmup: стратегия обновляет снимок, но не может
/// выставить заявку. ID включает дату, поэтому повтор в тот же день не дублируется.
async fn absorb_reference(
    api: &VenueApi,
    engine: &PortfolioEngine<PostgresStorage>,
    universe: &[InstrumentId],
    date: NaiveDate,
) -> Result<()> {
    let requirements = engine.reference_requirements().await;
    if requirements.is_empty() {
        return Ok(());
    }
    let reference = load_reference(api, universe, &requirements).await?;
    let (fundamentals, dividends) = (reference.fundamentals.len(), reference.dividends.len());
    let event = StrategyEvent {
        id: format!("reference:{date}"),
        phase: EventPhase::Warmup,
        occurred_at: reference.observed_at,
        payload: StrategyEventKind::Reference(reference),
    };
    let outcome = engine.process_event(&event, None).await?;
    if !outcome.duplicate {
        info!(fundamentals, dividends, "reference data loaded");
    }
    Ok(())
}

/// T-Invest кодирует отсутствующий показатель нулём: такой ноль не является данными.
fn reported(value: f64) -> Option<Decimal> {
    (value.is_finite() && value != 0.0)
        .then(|| Decimal::try_from(value).ok())
        .flatten()
        .map(|value| value.round_dp(6))
}

async fn load_reference(
    api: &VenueApi,
    universe: &[InstrumentId],
    requirements: &[ReferenceRequirement],
) -> Result<ReferenceData> {
    let now = Utc::now();
    let mut reference = ReferenceData {
        observed_at: now,
        ..ReferenceData::default()
    };
    if requirements.contains(&ReferenceRequirement::Fundamentals) {
        // Показатели привязаны к активу, а не к торгуемому UID.
        let mut assets = Vec::with_capacity(universe.len());
        for instrument_id in universe {
            let share = api
                .instruments()
                .share_by(InstrumentRequest {
                    id_type: InstrumentIdType::Uid as i32,
                    class_code: None,
                    id: instrument_id.to_string(),
                })
                .await?
                .into_inner()
                .instrument
                .ok_or_else(|| {
                    Error::Configuration(format!("share `{instrument_id}` not found"))
                })?;
            assets.push((instrument_id.clone(), share.asset_uid));
        }
        let response = api
            .instruments()
            .get_asset_fundamentals(GetAssetFundamentalsRequest {
                assets: assets.iter().map(|(_, asset)| asset.clone()).collect(),
            })
            .await?
            .into_inner();
        for row in response.fundamentals {
            let Some((instrument_id, _)) = assets.iter().find(|(_, asset)| *asset == row.asset_uid)
            else {
                continue;
            };
            reference.fundamentals.push(InstrumentFundamentals {
                instrument_id: instrument_id.clone(),
                price_to_book: reported(row.price_to_book_ttm),
                price_to_earnings: reported(row.pe_ratio_ttm),
                ev_to_ebitda: reported(row.ev_to_ebitda_mrq),
                return_on_equity: reported(row.roe),
                return_on_assets: reported(row.roa),
                net_margin: reported(row.net_margin_mrq),
                debt_to_equity: reported(row.total_debt_to_equity_mrq),
            });
        }
    }
    if requirements.contains(&ReferenceRequirement::Dividends) {
        for instrument_id in universe {
            let response = api
                .instruments()
                .get_dividends(GetDividendsRequest {
                    instrument_id: instrument_id.to_string(),
                    from: Some(datetime_to_timestamp(now - Duration::days(7))),
                    to: Some(datetime_to_timestamp(now + Duration::days(200))),
                    ..GetDividendsRequest::default()
                })
                .await?
                .into_inner();
            for dividend in response.dividends {
                let Some(last_buy) = dividend
                    .last_buy_date
                    .map(timestamp_to_datetime)
                    .transpose()?
                else {
                    continue;
                };
                reference.dividends.push(DividendEvent {
                    instrument_id: instrument_id.clone(),
                    last_buy_date: last_buy.date_naive(),
                    dividend_net: dividend
                        .dividend_net
                        .map(Money::from_proto)
                        .transpose()?
                        .map(|money| money.amount.0),
                });
            }
        }
    }
    Ok(reference)
}

/// Одно рассчитанное срабатывание относительно официальной сессии MOEX.
#[derive(Clone)]
struct ScheduledTrigger {
    anchor: ScheduleAnchor,
    session_date: NaiveDate,
    at: DateTime<Utc>,
    offset_seconds: i64,
    /// Конец основной сессии из официального расписания.
    session_end: Option<DateTime<Utc>>,
}

impl ScheduledTrigger {
    /// До какого момента повторять отложенное решение. Повторяется только
    /// основное открытие сессии: offset-события intraday-стратегий привязаны к
    /// конкретному времени и после него теряют смысл.
    fn retry_until(&self) -> Option<DateTime<Utc>> {
        (self.anchor == ScheduleAnchor::SessionOpen && self.offset_seconds == 0)
            .then_some(self.session_end?)
            .map(|end| end - Duration::minutes(OPEN_RETRY_MARGIN_MINUTES))
    }

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

/// Определяет последнюю уже начавшуюся официальную торговую сессию MOEX.
///
/// Простая UTC-дата неверна ночью, в выходные и на специальных сессиях. Risk
/// manager использует эту дату до первой заявки, поэтому после restart дневной
/// stop не может случайно унаследовать вчерашнюю базу.
async fn trading_schedules(
    api: &VenueApi,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<tinkoff_rs::proto::TradingSchedulesResponse> {
    let mut response = tinkoff_rs::proto::TradingSchedulesResponse::default();
    let mut cursor = from;
    while cursor < to {
        // The API counts calendar dates inclusively, so use a margin below fourteen days.
        let end = (cursor + Duration::days(12)).min(to);
        let page = api
            .instruments()
            .trading_schedules(TradingSchedulesRequest {
                exchange: Some("MOEX".to_owned()),
                from: Some(datetime_to_timestamp(cursor)),
                to: Some(datetime_to_timestamp(end)),
            })
            .await?
            .into_inner();
        response.exchanges.extend(page.exchanges);
        cursor = end;
    }
    Ok(response)
}

async fn current_moex_session_date(
    api: &VenueApi,
    now: DateTime<Utc>,
    previous: Option<NaiveDate>,
) -> Result<NaiveDate> {
    // TradingSchedules rejects historical `from` dates. The durable risk session
    // is the authority across nights/weekends; a new flat account uses its next session.
    let response = trading_schedules(api, now, now + Duration::days(12)).await?;
    session_date_from_schedule(&response, now, previous)
}

fn session_date_from_schedule(
    response: &tinkoff_rs::proto::TradingSchedulesResponse,
    now: DateTime<Utc>,
    previous: Option<NaiveDate>,
) -> Result<NaiveDate> {
    let mut sessions = Vec::new();
    for exchange in &response.exchanges {
        if !exchange.exchange.to_ascii_lowercase().contains("moex") {
            continue;
        }
        for day in exchange.days.iter().filter(|d| d.is_trading_day) {
            if let Some(start) = day.start_time {
                let start = timestamp_to_datetime(start)?;
                let date = day
                    .date
                    .map(timestamp_to_datetime)
                    .transpose()?
                    .map_or(start.date_naive(), |d| d.date_naive());
                sessions.push((start, date));
            }
        }
    }
    sessions.sort_unstable();
    sessions
        .iter()
        .rev()
        .find(|(start, _)| *start <= now)
        .map(|(_, date)| *date)
        .or(previous)
        .or_else(|| sessions.first().map(|(_, date)| *date))
        .ok_or_else(|| Error::Configuration("MOEX schedule contains no usable session".to_owned()))
}

/// Загружает официальное расписание MOEX и разворачивает требования стратегии
/// в абсолютные UTC-моменты на ближайшие две недели.
///
/// Время берётся из ответа биржевого календаря, а не вычисляется фиксированным
/// московским часом: так учитываются выходные, сокращённые дни и особые сессии.
async fn load_schedule_triggers(
    api: &VenueApi,
    requirements: &[ScheduleRequirement],
    now: DateTime<Utc>,
) -> Result<Vec<ScheduledTrigger>> {
    if requirements.is_empty() {
        return Ok(Vec::new());
    }
    let response = trading_schedules(api, now, now + Duration::days(12)).await?;
    let mut triggers = Vec::new();
    for exchange in response.exchanges {
        if !exchange.exchange.to_ascii_lowercase().contains("moex") {
            continue;
        }
        for day in exchange.days.into_iter().filter(|day| day.is_trading_day) {
            let Some(date) = day
                .date
                .or(day.start_time)
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
                        offset_seconds: requirement.offset_seconds,
                        session_end: day.end_time.map(timestamp_to_datetime).transpose()?,
                    });
                }
            }
        }
    }
    triggers.sort_by_key(|trigger| trigger.at);
    Ok(triggers)
}

/// Окна основных сессий `(открытие, конец)` из официального расписания.
fn session_windows(triggers: &[ScheduledTrigger]) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    triggers
        .iter()
        .filter(|t| t.anchor == ScheduleAnchor::SessionOpen && t.offset_seconds == 0)
        .filter_map(|t| t.session_end.map(|end| (t.at, end)))
        .collect()
}

/// Извлекает наступившие события без дубликатов: `(trigger, late)`.
/// Будущие triggers остаются в векторе, обработанные удаляются через `retain`.
///
/// Событие старше двух минут (компьютер спал, процесс был занят) возвращается
/// с `late = true`: оно не исполняется сразу, но учитывается стратегией, а
/// решение открытия сессии ставится в очередь повтора до конца этой сессии.
fn take_due_triggers(
    triggers: &mut Vec<ScheduledTrigger>,
    fired: &mut HashSet<String>,
    now: DateTime<Utc>,
) -> Vec<(ScheduledTrigger, bool)> {
    let mut due = Vec::new();
    triggers.retain(|trigger| {
        if trigger.at > now {
            return true;
        }
        let age = now.signed_duration_since(trigger.at).num_seconds();
        if fired.insert(trigger.id()) {
            due.push((trigger.clone(), age > SCHEDULE_GRACE_SECONDS));
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
    api: &VenueApi,
    state: &AccountRiskState,
) -> Result<(Decimal, bool)> {
    let mut cursor = None;
    let mut total = Decimal::ZERO;
    let mut recognized = true;
    loop {
        let response = api
            .operations_page(GetOperationsByCursorRequest {
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
            .await?;
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

#[cfg(test)]
mod campaign_tests {
    use super::{InstrumentId, InstrumentInfo, execution_prices};

    fn trigger(
        anchor: super::ScheduleAnchor,
        at: chrono::DateTime<chrono::Utc>,
        offset_seconds: i64,
    ) -> super::ScheduledTrigger {
        super::ScheduledTrigger {
            anchor,
            session_date: at.date_naive(),
            at,
            offset_seconds,
            session_end: Some(at + chrono::Duration::hours(9)),
        }
    }

    #[test]
    fn missed_triggers_are_reported_late_once_and_only_the_open_is_retried() {
        use super::ScheduleAnchor::{ClosingAuctionStart, SessionOpen};
        use chrono::{Duration, Utc};
        let now = Utc::now();
        let mut triggers = vec![
            trigger(SessionOpen, now - Duration::hours(5), 0),
            trigger(SessionOpen, now - Duration::seconds(30), 0),
            trigger(ClosingAuctionStart, now - Duration::hours(1), -1800),
            trigger(SessionOpen, now + Duration::hours(1), 0),
        ];
        let mut fired = std::collections::HashSet::new();
        let due = super::take_due_triggers(&mut triggers, &mut fired, now);
        assert_eq!(
            due.iter().map(|(_, late)| *late).collect::<Vec<_>>(),
            [true, false, true]
        );
        assert_eq!(triggers.len(), 1, "only the future trigger stays");
        // The open of the session retries until 10 minutes before its end.
        let (open, _) = &due[0];
        assert_eq!(
            open.retry_until(),
            Some(open.at + Duration::hours(9) - Duration::minutes(10))
        );
        assert_eq!(due[2].0.retry_until(), None);
        assert!(
            super::take_due_triggers(
                &mut due.into_iter().map(|(t, _)| t).collect(),
                &mut fired,
                now
            )
            .is_empty()
        );
    }
    use rust_decimal::Decimal;
    use tinkoff_rs::proto::{LastPrice, Quotation};

    #[test]
    fn preopen_and_weekend_restore_keep_the_durable_risk_session() {
        use chrono::{Duration, Utc};
        use tinkoff_rs::{
            domain::datetime_to_timestamp,
            proto::{TradingDay, TradingSchedule, TradingSchedulesResponse},
        };
        let now = Utc::now();
        let previous = (now - Duration::days(3)).date_naive();
        let opening = now + Duration::hours(1);
        let response = TradingSchedulesResponse {
            exchanges: vec![TradingSchedule {
                exchange: "MOEX".to_owned(),
                days: vec![TradingDay {
                    date: Some(datetime_to_timestamp(opening)),
                    start_time: Some(datetime_to_timestamp(opening)),
                    is_trading_day: true,
                    ..Default::default()
                }],
            }],
        };
        assert_eq!(
            super::session_date_from_schedule(&response, now, Some(previous)).unwrap(),
            previous
        );
        assert_eq!(
            super::session_date_from_schedule(&response, now, None).unwrap(),
            opening.date_naive()
        );
        assert_eq!(
            super::session_date_from_schedule(
                &response,
                opening + Duration::seconds(1),
                Some(previous)
            )
            .unwrap(),
            opening.date_naive()
        );
        assert!(
            super::session_date_from_schedule(&TradingSchedulesResponse::default(), now, None)
                .is_err()
        );
    }

    #[test]
    fn execution_prices_require_current_positive_values_for_every_uid() {
        let info = InstrumentInfo {
            instrument_id: InstrumentId::new("uid").unwrap(),
            lot_size: 10,
            currency: "rub".to_owned(),
            exchange: "moex".to_owned(),
            short_enabled: true,
            api_trade_available: true,
            market_order_available: true,
            kind: tinkoff_rs::engine::InstrumentKind::Share,
            price_multiplier: Decimal::ONE,
            accrued_interest: Decimal::ZERO,
            commission_rate: Decimal::ZERO,
        };
        let mut price = LastPrice {
            instrument_uid: "uid".to_owned(),
            time: Some(super::datetime_to_timestamp(chrono::Utc::now())),
            price: Some(Quotation {
                units: 123,
                nano: 0,
            }),
            ..Default::default()
        };
        assert_eq!(
            execution_prices(&[price.clone()], std::slice::from_ref(&info)).unwrap()
                [&info.instrument_id]
                .0,
            Decimal::from(123)
        );
        assert!(execution_prices(&[], std::slice::from_ref(&info)).is_err());
        price.time = Some(super::datetime_to_timestamp(
            chrono::Utc::now() - chrono::Duration::minutes(6),
        ));
        assert!(execution_prices(&[price.clone()], std::slice::from_ref(&info)).is_err());
        price.time = None;
        assert!(execution_prices(&[price.clone()], std::slice::from_ref(&info)).is_err());
        price.time = Some(super::datetime_to_timestamp(chrono::Utc::now()));
        price.price = Some(Quotation { units: 0, nano: 0 });
        assert!(execution_prices(&[price], &[info]).is_err());
    }

    #[test]
    fn production_config_requires_both_stops_and_known_contour() {
        let base = |contour: &str, stops: &str| {
            format!(
                "contour = \"{contour}\"\naccount_id = \"2000000001\"\nstrategy_id = \"live-test\"\n\
                 [risk]\nbudget_rub = \"30000.00\"\n{stops}\n\
                 [[universe]]\nuid = \"uid-1\"\n\
                 [[strategies]]\nsignal_weight = \"1.0\"\nkind = \"moving-average-trend\"\n\
                 fast_window = 50\nslow_window = 200\nband_percent = \"1.0\"\n"
            )
        };
        let parse = |text: String| toml::from_str::<super::BotConfig>(&text);
        let both = "daily_loss_percent = \"2.0\"\nmax_drawdown_percent = \"10.0\"";
        let sandbox = parse(base("sandbox", "")).unwrap();
        assert!(sandbox.validate().is_ok());
        let live = parse(base("production", both)).unwrap();
        assert_eq!(live.contour, super::Contour::Production);
        assert!(live.validate().is_ok());
        for partial in [
            "",
            "daily_loss_percent = \"2.0\"",
            "max_drawdown_percent = \"10.0\"",
        ] {
            let Err(error) = parse(base("production", partial)).unwrap().validate() else {
                panic!("a production config without both stops must be rejected");
            };
            assert!(
                error
                    .to_string()
                    .contains("requires risk.daily_loss_percent")
            );
        }
        assert!(parse(base("live", both)).is_err());
    }
}
