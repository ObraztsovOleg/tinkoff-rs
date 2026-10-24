# tinkoff-rs 0.3

Типизированный SDK для T-Invest API и portfolio-target runtime для 20
исследовательских стратегий MOEX: по умолчанию в Sandbox, после проверки — на
выделенном реальном счёте с подтверждением каждого плана. Версия 0.3 намеренно несовместима с
прежними `Strategy`, `StrategyEngine`, `StrategyRegistry` и SMA 10/30.

Стратегии не являются обещанием доходности. Это воспроизводимые торговые
гипотезы, перенесённые из академических работ на явно заданный набор UID.
Бэктест и сравнение результатов в проект не входят.

## Что входит

- `PortfolioStrategy -> TargetPortfolio -> TargetPlanner -> BatchExecutor`;
- один взвешенный ансамбль из одной или нескольких стратегий на отдельный
  T-Invest Sandbox-счёт и процесс;
- реальный контур (`contour = "production"`): токен `TINKOFF_TOKEN_PRODUCTION`,
  обязательные стопы, подтверждение каждого плана, аварийная остановка и
  перевод портфеля только после порога в Sandbox
  ([как подключить](docs/LIVE-TRADING.ru.md));
- 20 встроенных дневных, недельных, месячных и intraday стратегий, включая value,
  quality и дивидендную стратегию на справочных данных T-Invest
  ([отбор по силе доказательств](docs/research/top-10.ru.md));
- стратегия `ai-model`: любая ИИ-модель (Claude, OpenAI-совместимые, локальные)
  читает свежие новости, сама запрашивает индикаторы и выбирает доли бумаг
  ([как подключить](docs/AI-MODEL.ru.md));
- жёсткий конфигурируемый бюджет и опциональные loss/drawdown stop;
- single-flight, reductions-first исполнение многоинструментных ребалансировок;
- работа без сети: runtime пропускает решения без связи, сам переподключается,
  догружает пропущенные свечи и не завершается из-за обрыва;
- PostgreSQL persistence для state, risk epoch, batches, intents и fills;
- 20 русских PDF-разборов и локальный personal-use кэш оригинальных статей;
- отдельный TOML-пример для каждой встроенной стратегии и пример ансамбля.

## Быстрый старт

```bash
cp examples/configs/moving-average-trend.toml bot.toml
export TINKOFF_TOKEN='sandbox-token'
export DATABASE_URL='postgres://user:password@localhost/tinkoff_bot'

cargo run -- list-strategies
cargo run -- --config bot.toml check-config
cargo run -- --config bot.toml migrate
cargo run --release -- --config bot.toml run
```

Для нескольких сигналов используйте
`examples/configs/weighted-ensemble.toml`: каждая секция `[[strategies]]`
имеет относительный `signal_weight`, а наружу формируется одна итоговая позиция.
Поле `instruments` даёт стратегии собственный непересекающийся набор бумаг, чтобы
на одном счёте торговали несколько независимых блоков ([пример](docs/EXAMPLES.ru.md)).

`strategy_id` идентифицирует не только процесс, но и неизменяемую конфигурацию
ансамбля. После изменения общего universe, числа или порядка компонент, их
`signal_weight`, `kind` либо параметров задайте свежий, ранее не использованный
`strategy_id`.
Перед передачей того же счёта новому ID остановите старый runtime, ликвидируйте
позиции, дождитесь terminal status всех заявок и выполните `risk-reset` из нового
конфига; активная lease или незавершённый batch блокируют transfer.

Runtime не открывает и не пополняет sandbox-счёт автоматически. Для создания
отдельного счёта используйте `examples/sandbox-bootstrap.rs`.

## Документация

- [Руководство по runtime](docs/RUNBOOK.ru.md)
- [Примеры запуска](docs/EXAMPLES.ru.md)
- [Живая sandbox-кампания](docs/SANDBOX-CAMPAIGN.ru.md)
- [Запуск всего стенда через Docker Compose](docs/DOCKER.ru.md)
- [Торговля на реальных счетах](docs/LIVE-TRADING.ru.md)
- [Стратегия на ИИ-модели](docs/AI-MODEL.ru.md)
- [Алгоритмы стратегий](docs/STRATEGIES.ru.md)
- [PDF-архив исследований](docs/research/README.ru.md)
- [Манифест источников](docs/research/manifest.toml)

## Cargo features

- без features — SDK, protobuf, brokers, domain и streams;
- `engine` — portfolio engine, planner, risk и batch execution;
- `strategies` — все встроенные стратегии, включая конфигурацию `ai-model`;
- `postgres` — PostgreSQL storage и migrations;
- `cli` — готовый `tinkoff-bot`;
- `bot` — полный комплект и default feature.

## Статическая проверка

```bash
cargo fmt --all --check
cargo check --workspace --all-features
cargo check --workspace --all-features --examples
cargo clippy --workspace --all-features -- -D warnings
```

Регрессионные проверки транспорта и запуска выполняются через `cargo test`.
Проверка ценовых стратегий на истории — `tinkoff-bot backtest` (те же planner и
risk-лимиты, проскальзывание и комиссия; см. [управление стендом](docs/SANDBOX-CONTROLS.ru.md)).
Point-in-time состав индекса и гарантия прибыльности не входят в версию 0.3.

## Контракты API

Protobuf генерируется vendored `protoc` в Cargo `OUT_DIR` из зафиксированных
файлов `contracts/*.proto`. Происхождение snapshot и контрольная сумма исходного
архива сохранены в `contracts/SOURCE.toml`; обновление контрактов выполняется
вручную.

Правила, общие для бота, наблюдателя и дашборда, лежат в одном файле
`contracts/stand-rules.json`: перенос непрерывных фьючерсов (за сколько дней и
во сколько), стратегии «только акции», тарифы комиссий и пороги их измерения,
темы рынка ИИ-модели. `build.rs` превращает его в константы Rust, Python читает
тот же файл, дашборд получает правила от сервера; тест не даёт завести их копию.
