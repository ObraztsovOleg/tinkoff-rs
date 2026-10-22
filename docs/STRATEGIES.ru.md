# Встроенные портфельные стратегии

Проект содержит 20 показательных стратегий и стратегию на внешней ИИ-модели. Они выбраны по
наличию формализованной исследовательской основы, а не по обещанию максимальной
прибыли. Бэктест и ранжирование на данных MOEX не входят в проект. Отбор пяти
последних и двух комбинаций по силе доказательств описан в
[research/top-10.ru.md](research/top-10.ru.md).

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
| `volatility-managed` | месяц | long/cash, formula cap 2; executable net cap 1 | [разбор](research/briefs/volatility-managed.ru.pdf) |
| `value-composite` | месяц | long/short ранг B/P, E/P, EBITDA/EV | [разбор](research/briefs/value-composite.ru.pdf) |
| `quality-profitability` | месяц | long/short ранг ROE, ROA, маржи, долга | [разбор](research/briefs/quality-profitability.ru.pdf) |
| `risk-managed-momentum` | месяц | momentum 12–1, целевая волатильность 12% | [разбор](research/briefs/risk-managed-momentum.ru.pdf) |
| `return-seasonality` | месяц | long/short ранг того же месяца за 5 лет | [разбор](research/briefs/return-seasonality.ru.pdf) |
| `dividend-month` | день | long за 30 дней до последнего дня покупки | [разбор](research/briefs/dividend-month.ru.pdf) |
| `ai-model` | день | доли от ИИ-модели по новостям и индикаторам | [руководство](AI-MODEL.ru.md) |

## Единая семантика

- Сигналы используют только закрытые свечи. Дневное/недельное/месячное решение
  начинает исполняться не раньше следующей регулярной сессии.
- Решение на открытии сессии, которое не удалось принять вовремя (компьютер
  спал, нет сети, ещё идёт аукцион, исполняется прошлый пакет заявок), повторяется
  раз в минуту, но не позже чем за 10 минут до конца этой сессии. Повтор не
  включает intraday-сигналы посреди сессии. Месячная стратегия свою первую цель
  строит на первом открытии после запуска, дальше — раз в месяц.
- `valid_until` ограничивает срок отправки target. Только у
  `market-intraday-momentum` и `intraday-same-clock` он также ограничивает время
  удержания: на следующем live event просроченный вклад обнуляется, а ансамбль
  формирует обновлённый target для сохранения остальных сигналов или закрытия.
  Официальный `SessionOpen` включает приём регулярных intraday-свечей, а
  `SessionClose` в любом случае удаляет intraday-вклад; бары расширенной сессии и
  запоздавшая финальная свеча не могут открыть overnight-позицию. При первом
  запуске посреди сессии без сохранённого open-marker intraday-сигналы безопасно
  ждут следующего официального открытия.
- Обычный long/short signal portfolio нормализуется по сторонам: до `+0.5` long
  и до `-0.5` short. Если сигналов одной стороны нет, её половина остаётся в
  кэше. BAB может достигать gross 2 при net не выше 1.
  Формула volatility-managed также имеет cap 2, но long-only
  target исполняется с cap 1, так как для него net равен gross.
- Квинтильным стратегиям нужны минимум десять пригодных инструментов, pairs —
  два, volatility-managed — четыре.
- `value-composite`, `quality-profitability` и `dividend-month` используют
  справочные данные брокера: текущие мультипликаторы (`GetAssetFundamentals`) и
  календарь дивидендов (`GetDividends`). Runtime загружает их при старте и перед
  каждым открытием сессии и подаёт стратегии событием `Reference` в фазе Warmup:
  оно обновляет state, но не может создать заявку. Ноль T-Invest считается
  пропуском показателя. При сбое загрузки стратегия работает на прошлом снимке, а
  без снимка откладывает решение. Истории мультипликаторов на дату решения API не
  даёт, поэтому эти сигналы проверяются только вперёд.
- `ai-model` не основан на исследовании: решение принимает подключённая модель
  на открытии сессии, лимиты стратегии и risk manager применяются после неё
  ([подробнее](AI-MODEL.ru.md)).
- Недостаточная история откладывает общий сигнал, а недоступный обязательный
  short/market order останавливает preflight. Если максимальный вес `0.5` не
  позволяет купить даже один лот при текущем `sizing_capital`, planner получает
  для новой позиции ноль лотов и заявку по ней не создаёт.
- Ограничения счёта и risk manager имеют приоритет над target стратегии.
- Инструменты — любые рублёвые инструменты Мосбиржи: акции, фонды, облигации,
  фьючерсы, валюта. Вес — доля капитала по рублёвой стоимости: у облигации цена —
  процент номинала плюс НКД, у фьючерса — номинал (цена в пунктах × стоимость
  пункта). Непрерывный фьючерс `futures:<код>` стенд переносит в следующий
  контракт за 5 дней до последнего дня торгов; история сигналов склеивается с
  поправкой на разницу цен контрактов. `value-composite`, `quality-profitability`,
  `value-momentum`, `multi-factor` и `dividend-month` принимают только акции.
- Комиссия учитывается при переводе весов в лоты: лот стоит вместе с комиссией за
  сделку (ставка контура и типа инструмента — тариф брокера, уточнённый по
  фактическим списаниям). Изменение позиции меньше порога сделки (по умолчанию
  0,5% капитала) не исполняется; закрытие и разворот — всегда.

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
