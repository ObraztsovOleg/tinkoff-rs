# Встроенные портфельные стратегии

Проект содержит 15 показательных стратегий для T-Invest Sandbox. Они выбраны по
наличию формализованной исследовательской основы, а не по обещанию максимальной
прибыли. Бэктест и ранжирование на данных MOEX не входят в проект.

Каждый алгоритм можно использовать отдельно либо включить в один взвешенный
ансамбль через несколько секций `[[strategies]]`. Ансамбль сохраняет последний
полный target каждой компоненты и вычисляет по каждому UID взвешенную сумму.
Противоположные сигналы взаимно сокращаются; нулевой общий вес означает закрытие.
Рабочий пример находится в `examples/configs/weighted-ensemble.toml`.

| `kind` | Частота | Портфель | Исследовательский PDF |
|---|---:|---|---|
| `moving-average-trend` | день | long/short, SMA 50/200 | [разбор](research/briefs/moving-average-trend.ru.pdf) |
| `range-breakout` | день | long/short, range 50 | [разбор](research/briefs/range-breakout.ru.pdf) |
| `time-series-momentum` | месяц | own-return trend, inverse-vol | [разбор](research/briefs/time-series-momentum.ru.pdf) |
| `cross-sectional-momentum` | месяц | long/short квинтили 12–2 | [разбор](research/briefs/cross-sectional-momentum.ru.pdf) |
| `intermediate-momentum` | месяц | long/short квинтили 12–7 | [разбор](research/briefs/intermediate-momentum.ru.pdf) |
| `fifty-two-week-high` | месяц | long/short ранги к максимуму | [разбор](research/briefs/fifty-two-week-high.ru.pdf) |
| `residual-momentum` | месяц | benchmark residual ranks | [разбор](research/briefs/residual-momentum.ru.pdf) |
| `pairs-trading` | событие | одна market-neutral пара | [разбор](research/briefs/pairs-trading.ru.pdf) |
| `short-term-reversal` | неделя | long losers/short winners | [разбор](research/briefs/short-term-reversal.ru.pdf) |
| `market-intraday-momentum` | день | один benchmark, last 30 min | [разбор](research/briefs/market-intraday-momentum.ru.pdf) |
| `intraday-same-clock` | 30 минут | cross-sectional continuation | [разбор](research/briefs/intraday-same-clock.ru.pdf) |
| `low-volatility` | месяц | long-only low-vol quintile | [разбор](research/briefs/low-volatility.ru.pdf) |
| `betting-against-beta` | месяц | beta-balanced long/short | [разбор](research/briefs/betting-against-beta.ru.pdf) |
| `equal-risk-contribution` | месяц | long-only risk allocation | [разбор](research/briefs/equal-risk-contribution.ru.pdf) |
| `volatility-managed` | месяц | long/cash, gross до 2 | [разбор](research/briefs/volatility-managed.ru.pdf) |

## Единая семантика

- Сигналы используют только закрытые свечи. Дневное/недельное/месячное решение
  начинает исполняться не раньше следующей регулярной сессии.
- `valid_until` ограничивает срок отправки target. Только у
  `market-intraday-momentum` и `intraday-same-clock` он также ограничивает время
  удержания: на следующем live event просроченный вклад обнуляется, а ансамбль
  формирует обновлённый target для сохранения остальных сигналов или закрытия.
- Обычный long/short signal portfolio нормализуется по сторонам: до `+0.5` long
  и до `-0.5` short. Если сигналов одной стороны нет, её половина остаётся в
  кэше. Плечо до gross 2 используют только BAB и volatility-managed.
- Квинтильным стратегиям нужны минимум десять пригодных инструментов, pairs —
  два, volatility-managed — четыре.
- Инструмент без достаточной истории, с недоступным short/market order либо с
  одним лотом дороже разрешённого размера исключается до signal construction.
- Ограничения счёта и risk manager имеют приоритет над target стратегии.

## Где лежат статьи

Таблица ведёт на приложенные собственные PDF-разборы. Метаданные всех основных,
критических и контекстных публикаций находятся в
[research/manifest.toml](research/manifest.toml). Законно полученные полные
оригиналы складываются локально в `docs/research/originals/` и не коммитятся.
Инструкции: [research/README.ru.md](research/README.ru.md).

## Интерпретация

Academic evidence не переносится автоматически на MOEX: меняются universe,
ликвидность, short availability, торговое расписание, комиссии и режим рынка.
Особенно чувствительны к издержкам intraday same-clock и weekly reversal.
Sandbox упрощает margin и не воспроизводит полностью borrow/funding costs.

При бюджете 30 000 ₽ месячная фиксированная плата 390 ₽ равна примерно 1,3%
капитала ещё до комиссии и spread. Бюджет — база sizing, а не гарантированный
максимальный убыток; стратегия с gross 2 может держать номинальную экспозицию до
двух бюджетов.
