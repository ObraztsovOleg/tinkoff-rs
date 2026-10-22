# Руководство tinkoff-rs 0.3

Версия 0.3 предоставляет portfolio-target runtime для T-Invest Sandbox. Старые
`Strategy`, `StrategyEngine`, `StrategyRegistry`, `SmaStrategy`, SMA 10/30 и
feature `strategy-sma` удалены без compatibility layer.
Текущий durable runtime записывает `runtime_version = 2`; обозначение v1 ниже
относится только к legacy state и intents, оставленным для аудита и миграции.

## 1. Модель запуска

Один процесс владеет ровно одним sandbox-счётом и одним взвешенным ансамблем.
Ансамбль содержит одну или несколько независимых стратегий:

```text
closed candles / schedule
        -> несколько BuiltinStrategy
        -> WeightedEnsembleStrategy
        -> полный TargetPortfolio
        -> RiskManager
        -> TargetPlanner
        -> reductions-first PlannedBatch
        -> BatchExecutor
        -> T-Invest Sandbox
```

Компоненты не создают заявки. Каждая возвращает полные signed weights по всему
торгуемому universe, а ансамбль формирует один общий target. Planner переводит его
в signed lots относительно
фактических позиций. Благодаря этому отклонённая заявка не делает внутреннее
состояние стратегии источником истины: истина всегда находится у брокера.

## 2. Требования

- Rust 1.85 или новее;
- PostgreSQL;
- T-Invest sandbox token;
- отдельный sandbox account ID для каждого запущенного процесса;
- явные UID всех торгуемых и reference-инструментов.

Счёт можно создать и пополнить примером:

```bash
export TINKOFF_TOKEN='sandbox-token'
export TINKOFF_SANDBOX_ACCOUNT_NAME='strategy account'
export TINKOFF_SANDBOX_INITIAL_RUB='30000'
cargo run --no-default-features --example sandbox-bootstrap
```

Runtime сам счёт не создаёт, не пополняет и не закрывает.

## 3. Конфигурация

Минимальная форма:

```toml
contour = "sandbox"
account_id = "sandbox-account-id"
strategy_id = "ma-main"
app_name = "my-company.tinkoff-rs.bot"
database_connections = 5

[risk]
budget_rub = "30000.00"
daily_loss_percent = "5.0"
max_drawdown_percent = "25.0"

[[universe]]
uid = "instrument-uid-1"

[[universe]]
uid = "instrument-uid-2"

[[strategies]]
signal_weight = "1.0"
kind = "moving-average-trend"
fast_window = 50
slow_window = 200
band_percent = "1.0"
```

Неизвестные поля отклоняются. `budget_rub` обязателен. Чтобы отключить один из
стопов, полностью удалите соответствующее поле; значение `0` не означает
отключение.

`contour = "production"` переводит процесс на выделенный реальный счёт. Тогда
оба стопа обязательны, токен берётся из `TINKOFF_TOKEN_PRODUCTION` (Sandbox — из
`TINKOFF_TOKEN`), счёт должен быть открыт, а токен — иметь полный доступ. При
аварийной остановке (`live_settings.halted`) команды `run`, `run --service-only`
и `cleanup` не стартуют; `run --startup-only` только читает счёт и рынок.
Пополнение и ручные заявки на реальном счёте недоступны; каждый план требует
подтверждения. Подробности — в [руководстве по реальным счетам](LIVE-TRADING.ru.md).

Стратегии residual momentum, short-term reversal и betting against beta имеют
дополнительный `benchmark_uid` внутри соответствующей `[[strategies]]`. Reference UID не становится
торгуемой позицией.

Чтобы объединить сигналы, добавьте другие секции:

```toml
[[strategies]]
signal_weight = "0.6"
kind = "moving-average-trend"
fast_window = 50
slow_window = 200
band_percent = "1.0"

[[strategies]]
signal_weight = "0.4"
kind = "range-breakout"
lookback_days = 50
filter_percent = "1.0"
```

Вес положителен и относителен. Ансамбль делит каждый `signal_weight` на сумму
всех весов, затем складывает targets по UID. Благодаря такой выпуклой смеси
итог не превышает максимальные gross/net/per-instrument ограничения компонент.
Финальную проверку всё равно выполняет `TargetPlanner`.

Последний target каждой компоненты входит в durable state и продолжает действовать
до её следующего сигнала. Поэтому месячный сигнал не исчезает, когда дневная
компонента инициирует новую ребалансировку. Компонента без первого target даёт
нулевой вклад. Нулевой общий вес закрывает позицию, а смена знака выполняется
через отдельные reduction и opening legs.

Готовые конфигурации находятся в `examples/configs/`.

`strategy_id` идентифицирует неизменяемую конфигурацию ансамбля и его durable
state. Задавайте новый ID после изменения:

- состава или порядка `[[universe]]`;
- количества или порядка `[[strategies]]`;
- любого `signal_weight`, `kind` или параметра компоненты;
- старого одиночного `[strategy]` на новый формат `[[strategies]]`.

Так несовместимая история, cached targets и маркеры периодов не будут приняты
новой конфигурацией. `check-config` работает без БД и проверяет только сам TOML,
поэтому совместимость с уже сохранённым state выясняется лишь при `run`.
Изменения risk-полей имеют отдельные правила epoch, описанные в разделе 6.

Чтобы передать тот же Sandbox-счёт новому ID, сначала остановите старый runtime,
полностью ликвидируйте портфель и дождитесь terminal status всех durable и
брокерских заявок. Затем сохраните TOML со свежим, ранее не использованным
`strategy_id` и выполните обычный
`risk-reset --reason "replace immutable ensemble configuration"`. Команда создаст
новую risk epoch с новым владельцем. Она отклонит transfer, если старый процесс
ещё удерживает эксклюзивную lease счёта, портфель не пуст или остались
незавершённые durable/активные Sandbox-заявки. Только после успешного reset
запускайте новый `run`.

## 4. Команды

```bash
export TINKOFF_TOKEN='sandbox-token'
export DATABASE_URL='postgres://user:password@localhost/tinkoff_bot'

cargo run -- list-strategies
cargo run -- --config bot.toml check-config
cargo run -- --config bot.toml migrate
cargo run -- --config bot.toml run
cargo run -- --config bot.toml risk-status
cargo run -- --config bot.toml risk-reset --reason 'new risk epoch'
```

`list-strategies` не требует broker token или database URL.
`check-config` только разбирает и валидирует TOML. Остальные account-команды
подключаются исключительно к sandbox endpoint.

## 5. Warmup и события

Каждая стратегия объявляет `DataRequirement`: UID, роль, candle interval,
число warmup bars и необходимость окончательно закрытой свечи. Перед stream:

1. runtime загружает требуемую историю;
2. сортирует и дедуплицирует свечи;
3. восстанавливает сохранённый strategy state;
4. передаёт недостающие свечи как `EventPhase::Warmup`;
5. запрещает targets и заявки до перехода в `EventPhase::Live`.

Портфельные сигналы ждут полный timestamp-aligned набор инструментов. При
неполных данных ребалансировка пропускается. Внутридневные session anchors
строятся по `TradingSchedules`, а не по heartbeat stream.

`TargetPortfolio::valid_until` задаёт крайний срок отправки конкретного target.
Для дневных, недельных и месячных компонент он не является сроком удержания:
последний target остаётся в ensemble cache до следующего сигнала этой компоненты.
Для `market-intraday-momentum` и `intraday-same-clock` `valid_until` одновременно
завершает holding-интервал. На первом следующем live event ансамбль удаляет вклад
просроченной intraday-компоненты и выпускает обновлённый общий target; при
отсутствии других активных вкладов он будет нулевым и закроет позицию.
Официальный `SessionClose` всегда создаёт такое закрытие, даже если потоковая свеча
опоздала или не пришла.

Официальный `SessionOpen` одновременно включает live-сигналы обеих intraday-
стратегий. Свечи до него не считаются регулярной сессией. Если чистый первый
запуск произошёл уже после открытия и open-marker не был сохранён прежним
процессом, runtime намеренно пропускает intraday-торговлю до следующей сессии.

## 6. Risk

Бюджет — жёсткий allocation cap, а не команда пополнения и не гарантированный
максимальный убыток:

```text
allocated_principal = min(budget_rub, account_equity_at_anchor)
strategy_equity = allocated_principal
                + current_account_equity
                - account_equity_at_anchor
                - external_cash_flows_since_anchor
sizing_capital = min(budget_rub, max(strategy_equity, 0))
```

Прибыль сверх бюджета не реинвестируется. Убыток уменьшает размер последующих
позиций. Проценты рассчитываются по mark-to-market equity с комиссиями и
открытыми позициями.

Жёсткие ограничения runtime:

- gross exposure не больше 2 × sizing capital;
- абсолютная net exposure не больше 1 × sizing capital;
- один инструмент не больше 50% sizing capital;
- short и margin разрешаются только в Sandbox и только для short-enabled UID.

Дневной stop действует до следующей официальной сессии после полной
ликвидации. Maximum drawdown создаёт persistent close-only lock. Его снимает
только `risk-reset`, когда портфель flat и нет pending/uncertain orders.

Изменение бюджета требует новой risk epoch. Изменение процентов применяется к
существующей эпохе и не снимает уже сработавший lock.

## 7. Планирование и исполнение

```text
target_lots = trunc_toward_zero(
    sizing_capital * signed_weight / (price * lot_size)
)
```

Planner проверяет currency, exchange, lot size, trading availability, short
availability, gross/net/per-instrument limits и отсутствие посторонних позиций.

Batch исполняется фазами:

1. при close-only не отправлять ещё не поданные opening legs;
2. close/reduce позиции;
3. дождаться terminal broker status;
4. заново загрузить portfolio и сверить позиции;
5. только затем выполнить opening legs.

Исполнение single-flight: до обработки каждого нового live event runtime сначала
recover/reconcile-ит незавершённые intents и batches текущего владельца, а
database constraint запрещает более одного unfinished batch на всём `account_id`.
Если после recovery остаётся хотя бы один нетерминальный объект,
новый торговый target и новый batch не создаются. Закрытая свеча всё же передаётся
как неторговый `EventPhase::Warmup` и атомарно обновляет индикаторную историю;
target и заявки в этой фазе невозможны. Schedule events и risk-check в стратегию
до terminal исхода предыдущего batch не передаются. Эти сигналы не откладываются:
исполнять их позже по устаревшей цене запрещено. Следующий новый live event после
завершения batch снова допускает планирование. В close-only recovery продолжает
сокращения, но не отправляет новые opening legs.

T-Invest не предоставляет атомарную корзину. Частичное исполнение или отказ на
фазе reduction переводит batch в incomplete и не запускает openings.
Если opening уже успел попасть к брокеру до срабатывания stop, runtime помечает
его uncertain, продолжает периодическую сверку и ликвидирует появившуюся
позицию; автоматической подмены client UUID не происходит.

## 8. Persistence и restart

PostgreSQL хранит strategy state, processed events, candles, risk state,
rebalance batches, intents, orders и fills. State стратегии, processed event,
target, risk snapshot и legs фиксируются одной транзакцией.

Новая migration не восстанавливает v1 SMA state. Если у legacy runtime есть
`pending`, `submitting`, `accepted` или `uncertain` intents, migration
останавливается: сначала их нужно сверить с Sandbox. Терминальные v1 строки
остаются audit-only.

После restart `BatchExecutor` сначала восстанавливает pending batches текущего
владельца и reconcile-ит request UUID. Нетерминальный исход включает тот же
single-flight барьер; события свечей сохраняют историю без торговли до следующей
успешной сверки.

### Работа без сети

Временная недоступность сети или T-Invest API не завершает `run` и
`run --service-only`. К временным относятся обрыв соединения (в том числе gRPC
`Unknown: transport error` и `Internal` при разрыве HTTP/2), таймауты, отмена
запроса по времени, `Unavailable` и ограничение частоты.

- Событие, для которого нет связи, обрабатывается без торговли: свеча и
  границы сессии попадают в состояние стратегии как `Warmup`, торговое решение
  пропускается. Следующее событие заново читает портфель и сверяет незавершённые
  batches, поэтому неоднозначно отправленная заявка не повторяется вслепую.
- Отказ risk-проверки при исполнении (устаревшая или отсутствующая котировка)
  тоже лишь пропускает решение.
- Market data stream переподключается без ограничения числа попыток. Если
  сервер молчит три интервала ping (90 с), соединение считается мёртвым и
  заменяется. После переподписки свечи, закрывшиеся во время разрыва,
  догружаются из истории по порядку; решения по расписанию ждут подтверждения
  подписок.
- Если простой длится больше трёх минут, сеть пропала во время запуска или
  потерян lease-connection PostgreSQL, сессия создаётся заново: новые
  соединения, захват lease, recovery и прогрев. Пауза между попытками растёт
  2 → 4 → … → 60 с.
- Журнал показывает начало простоя, напоминание раз в минуту и
  `network restored; live decisions resumed` после восстановления.

Процесс завершают только SIGINT/SIGTERM и ошибки, которые повтор не исправит:
конфигурация, авторизация, отказ сервера в подписке, счёт занят другим
процессом, нарушение инвариантов хранилища. `run --startup-only` по-прежнему
сразу сообщает о недоступной сети: это проверка окружения.

## 9. Исследования и PDF

Манифест: `docs/research/manifest.toml`. Русские PDF-разборы отслеживаются Git,
полные сторонние статьи находятся только в gitignored `originals/`. URL в
manifest нужны для указания происхождения, а пользовательская документация ссылается
прежде всего на локальные PDF-разборы. Законно полученный оригинал пользователь
вручную кладёт в `docs/research/originals/` под именем из `expected_filename`
соответствующей записи `[[paper]]` в manifest. Оригиналы не участвуют в сборке или
работе runtime.

## 10. Пользовательская стратегия

Новая стратегия реализует `PortfolioStrategy` и не получает API, `Trading` или
`Storage`. Она объявляет data/schedule requirements, сериализуемое state и
возвращает полный `TargetPortfolio`. Offline пример:

```bash
cargo run --no-default-features --features engine \
  --example portfolio-strategy-demo
```

## 11. Ограничения

- только T-Invest Sandbox;
- fixed UID universe без point-in-time состава индекса;
- нет бэктеста, сравнения доходности и slippage-модели;
- исторические свечи не обещают полную corporate-action adjustment;
- Sandbox упрощает комиссии, margin и перенос short;
- стратегии являются гипотезами, а не гарантией прибыли на MOEX.

## 12. Статические проверки

```bash
cargo fmt --all --check
cargo check --workspace --all-features
cargo check --workspace --all-features --examples
cargo clippy --workspace --all-features -- -D warnings
```

Регрессионные проверки транспорта, прогрева и данных исполнения запускаются
через `cargo test --workspace --all-features`. Бэктест не входит в версию 0.3.
Живой прогон описан в [sandbox-кампании](SANDBOX-CAMPAIGN.ru.md).
