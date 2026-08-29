# tinkoff-rs 0.3

Типизированный SDK для T-Invest API и sandbox-only portfolio-target runtime для
15 исследовательских стратегий MOEX. Версия 0.3 намеренно несовместима с
прежними `Strategy`, `StrategyEngine`, `StrategyRegistry` и SMA 10/30.

Стратегии не являются обещанием доходности. Это воспроизводимые торговые
гипотезы, перенесённые из академических работ на явно заданный набор UID.
Бэктест и сравнение результатов в проект не входят.

## Что входит

- `PortfolioStrategy -> TargetPortfolio -> TargetPlanner -> BatchExecutor`;
- один взвешенный ансамбль из одной или нескольких стратегий на отдельный
  T-Invest Sandbox-счёт и процесс;
- 15 встроенных дневных, недельных, месячных и intraday стратегий;
- жёсткий конфигурируемый бюджет и опциональные loss/drawdown stop;
- single-flight, reductions-first исполнение многоинструментных ребалансировок;
- PostgreSQL persistence для state, risk epoch, batches, intents и fills;
- 15 русских PDF-разборов и локальный personal-use кэш оригинальных статей;
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

`strategy_id` идентифицирует не только процесс, но и неизменяемую конфигурацию
ансамбля. После изменения общего universe, числа или порядка компонент, их
`signal_weight`, `kind` либо параметров задайте новый `strategy_id`.
Перед передачей того же счёта новому ID остановите старый runtime, ликвидируйте
позиции, дождитесь terminal status всех заявок и выполните `risk-reset` из нового
конфига; активная lease или незавершённый batch блокируют transfer.

Runtime не открывает и не пополняет sandbox-счёт автоматически. Для создания
отдельного счёта используйте `examples/sandbox-bootstrap.rs`.

## Документация

- [Руководство по runtime](docs/RUNBOOK.ru.md)
- [Примеры запуска](docs/EXAMPLES.ru.md)
- [Алгоритмы стратегий](docs/STRATEGIES.ru.md)
- [PDF-архив исследований](docs/research/README.ru.md)
- [Манифест источников](docs/research/manifest.toml)

## Cargo features

- без features — SDK, protobuf, brokers, domain и streams;
- `engine` — portfolio engine, planner, risk и batch execution;
- `strategies` — все 15 встроенных стратегий;
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

Автоматические тесты, бэктест, slippage-модель, point-in-time состав индекса и
гарантия прибыльности намеренно не входят в версию 0.3.

## Контракты API

Protobuf генерируется vendored `protoc` в Cargo `OUT_DIR` из зафиксированных
файлов `contracts/*.proto`. Происхождение snapshot и контрольная сумма исходного
архива сохранены в `contracts/SOURCE.toml`; обновление контрактов выполняется
вручную.
