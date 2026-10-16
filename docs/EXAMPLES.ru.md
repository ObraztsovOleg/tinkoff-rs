# Примеры конфигурации и запуска

Все команды торговли работают только с T-Invest Sandbox. Один процесс запускает
один ансамбль из одной или нескольких стратегий на одном выделенном счёте.
Перед запуском замените placeholder UID и `account_id`; независимым ансамблям
нужны разные sandbox-счета и разные `strategy_id`.

## Базовый сценарий

```bash
export TINKOFF_TOKEN="..."
export DATABASE_URL="postgres://user:password@localhost/tinkoff_bot"

cargo run --features bot --bin tinkoff-bot -- list-strategies
cargo run --features bot --bin tinkoff-bot -- \
  --config examples/configs/moving-average-trend.toml check-config
cargo run --features bot --bin tinkoff-bot -- \
  --config examples/configs/moving-average-trend.toml migrate
cargo run --features bot --bin tinkoff-bot -- \
  --config examples/configs/moving-average-trend.toml run
```

`check-config` не читает секреты и не подключается к сети. `run` сначала
восстанавливает state, загружает только нужный warmup закрытых свечей, запрещает
заявки во время warmup и затем слушает live stream и официальное расписание
MOEX.

## Одна или несколько стратегий

Одиночная стратегия использует тот же общий формат, что ансамбль:

```toml
[[strategies]]
signal_weight = "1.0"
kind = "moving-average-trend"
fast_window = 50
slow_window = 200
band_percent = "1.0"
```

Несколько стратегий перечисляются последовательными секциями. Готовый пример:

```bash
cargo run --features bot --bin tinkoff-bot -- \
  --config examples/configs/weighted-ensemble.toml check-config

cargo run --features bot --bin tinkoff-bot -- \
  --config examples/configs/weighted-ensemble.toml run
```

`signal_weight` является относительным: набор `5, 3, 2` эквивалентен
`0.5, 0.3, 0.2`. Единственная стратегия всегда получает эффективный вес 100%.
Стратегия, которая ещё не сформировала первый сигнал, временно вносит ноль.
После первого сигнала её последний полный target сохраняется до следующего
сигнала этой же стратегии.

Для каждого UID вычисляется:

```text
combined_weight = Σ(signal_weight / Σsignal_weights × component_weight)
```

Противоположные сигналы взаимно сокращаются. Нулевой итог закрывает позицию;
смена знака сначала закрывает прежнюю сторону и только затем открывает новую.
Все компоненты используют общий `[[universe]]`, поэтому он должен одновременно
удовлетворять минимальным требованиям каждого выбранного `kind`.

### Изменение конфигурации ансамбля

Считайте `strategy_id` идентификатором неизменяемой конфигурации. Свежий, ранее
не использованный `strategy_id` обязателен после любого из следующих изменений:

- добавление, удаление или перестановка `[[universe]]`;
- добавление, удаление или перестановка `[[strategies]]`;
- изменение `signal_weight`, `kind` или любого параметра стратегии.

Это правило действует и при переходе со старого одиночного `[strategy]` на
`[[strategies]]`. Оно не относится к процентным risk-порогам; изменение бюджета
управляется отдельной risk epoch, как описано ниже. `check-config` проверяет форму
нового TOML без БД и поэтому не может подтвердить совместимость с сохранённым
state старого `strategy_id`.

Для безопасной смены ID на том же Sandbox-счёте:

1. Остановите старый runtime.
2. Полностью ликвидируйте портфель и дождитесь terminal status всех durable и
   брокерских заявок.
3. Запишите новый `strategy_id` в TOML.
4. Перед первым `run` передайте владение счётом новой risk epoch:

   ```bash
   cargo run --features bot --bin tinkoff-bot -- \
     --config bot.toml risk-reset \
     --reason "replace immutable ensemble configuration"
   ```

5. Запустите новый runtime обычной командой `run`.

`risk-reset` не передаст счёт, пока старый runtime удерживает эксклюзивную lease,
портфель не пуст либо остаются незавершённые durable/активные Sandbox-заявки.

### Срок действия intraday-сигнала

Для обычной дневной, недельной или месячной компоненты `valid_until` ограничивает
время отправки конкретного target, но не закрывает уже набранную позицию. Для
`market-intraday-momentum` и `intraday-same-clock` тот же timestamp одновременно
является концом holding-интервала. После него ансамбль на следующем live event
обнуляет вклад просроченной компоненты и выпускает обновлённый общий target. Если
других активных компонент нет, это нулевой target для закрытия позиции.
Официальный `SessionClose` служит обязательным fallback-событием закрытия.
Официальный `SessionOpen` разрешает live-сигналы и отделяет регулярные свечи от
расширенной сессии. Поэтому первый запуск посреди торгового дня может намеренно
пропустить intraday-сигнал до следующего открытия MOEX.

### Незавершённый batch и новые сигналы

Runtime исполняет не более одного durable rebalance batch одновременно. Перед
каждым новым live event он сначала восстанавливает и сверяет незавершённые
intents/batches владельца; database single-flight дополнительно запрещает второй
незавершённый batch на всём выделенном `account_id`. Пока хотя бы один из них не
получил terminal status, новый торговый target и новый batch не создаются.

Закрытая свеча при этом не теряется: runtime передаёт её стратегии как
неторговый `EventPhase::Warmup` и атомарно сохраняет обновлённую индикаторную
историю, но target и заявки в этой фазе запрещены. Schedule events и risk-check
до стратегии не доходят, пока предыдущий batch не завершён. Пропущенный торговый
сигнал не откладывается и не исполняется позже по устаревшей цене; после terminal
исхода следующий новый live event снова может сформировать target. Если risk manager
уже включил close-only, recovery продолжает сокращения, но не разрешает новые
opening legs.

## Бюджет и процентные стопы

Бюджет меняется в TOML как точное десятичное значение:

```toml
[risk]
budget_rub = "75000.00"
daily_loss_percent = "3.5"
max_drawdown_percent = "18.0"
```

Рост equity выше 75 000 ₽ не реинвестируется. При убытке sizing capital
снижается. BAB может иметь gross до `2 × sizing_capital`, сохраняя
net не выше 1. У long-only volatility-managed формула имеет cap 2,
но выходной target ограничивается общим hard net-limit 1. Бюджет в любом
случае не является гарантией максимального убытка.

Чтобы отключить только дневной стоп, удалите его поле:

```toml
[risk]
budget_rub = "75000.00"
max_drawdown_percent = "18.0"
```

Чтобы отключить оба процентных стопа, оставьте только бюджет:

```toml
[risk]
budget_rub = "75000.00"
```

Изменение процентов не снимает уже установленный lock. Изменение бюджета
требует нового risk epoch:

```bash
cargo run --features bot --bin tinkoff-bot -- \
  --config examples/configs/moving-average-trend.toml risk-status

cargo run --features bot --bin tinkoff-bot -- \
  --config examples/configs/moving-average-trend.toml \
  risk-reset --reason "approved budget change to 75000 RUB"
```

`risk-reset` отклоняется, если портфель не ликвидирован, есть durable intent или
активная sandbox-заявка. Дневной stop нельзя сбросить в той же MOEX-сессии.

## Все одиночные конфигурации и ансамбль

Локально проверить весь каталог без токена и базы:

```bash
for config in examples/configs/*.toml; do
  cargo run --features bot --bin tinkoff-bot -- \
    --config "$config" check-config || exit 1
done
```

Каждый файл запускается одинаково; выбирайте один файл на процесс:

```bash
cargo run --features bot --bin tinkoff-bot -- --config examples/configs/moving-average-trend.toml run
cargo run --features bot --bin tinkoff-bot -- --config examples/configs/range-breakout.toml run
cargo run --features bot --bin tinkoff-bot -- --config examples/configs/time-series-momentum.toml run
cargo run --features bot --bin tinkoff-bot -- --config examples/configs/cross-sectional-momentum.toml run
cargo run --features bot --bin tinkoff-bot -- --config examples/configs/intermediate-momentum.toml run
cargo run --features bot --bin tinkoff-bot -- --config examples/configs/fifty-two-week-high.toml run
cargo run --features bot --bin tinkoff-bot -- --config examples/configs/residual-momentum.toml run
cargo run --features bot --bin tinkoff-bot -- --config examples/configs/pairs-trading.toml run
cargo run --features bot --bin tinkoff-bot -- --config examples/configs/short-term-reversal.toml run
cargo run --features bot --bin tinkoff-bot -- --config examples/configs/market-intraday-momentum.toml run
cargo run --features bot --bin tinkoff-bot -- --config examples/configs/intraday-same-clock.toml run
cargo run --features bot --bin tinkoff-bot -- --config examples/configs/low-volatility.toml run
cargo run --features bot --bin tinkoff-bot -- --config examples/configs/betting-against-beta.toml run
cargo run --features bot --bin tinkoff-bot -- --config examples/configs/equal-risk-contribution.toml run
cargo run --features bot --bin tinkoff-bot -- --config examples/configs/volatility-managed.toml run
cargo run --features bot --bin tinkoff-bot -- --config examples/configs/weighted-ensemble.toml run
```

Квинтильные примеры содержат минимум десять UID, pairs — минимум два,
volatility-managed — минимум четыре. Benchmark UID у residual momentum,
short-term reversal и BAB является reference series и не дублируется в
`[[universe]]`.

## PDF-разборы и оригиналы

Основные вложения документации — 15 локальных PDF в
`docs/research/briefs/`. Сетевые адреса и DOI находятся в
`docs/research/manifest.toml`.

Полные сторонние статьи не нужны ни для сборки, ни для запуска стратегий.
Если пользователь законно получил оригинал, его можно вручную поместить
в `docs/research/originals/` под именем из поля `expected_filename` соответствующей
записи `[[paper]]` в manifest. Каталог `originals/` исключён из Git.

## Пользовательская Rust-стратегия без сети

[portfolio-strategy-demo.rs](../examples/portfolio-strategy-demo.rs) реализует
собственный `PortfolioStrategy`, создаёт полный target и планирует batch в
`MemoryStorage`. В примере нет API client и `BatchExecutor`, поэтому отправка
заявки технически невозможна:

```bash
cargo run --features engine --example portfolio-strategy-demo
```
