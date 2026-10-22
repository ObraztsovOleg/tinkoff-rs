# Топ-10 стратегий по силе доказательств

Статус: отбор по качеству академической доказательной базы, а не по обещанию
прибыли. Ни одна стратегия не «доказанно прибыльна» в будущем: по McLean и
Pontiff доходности опубликованных аномалий после публикации в среднем на 58%
ниже, а в Sandbox на десяти бумагах за несколько сессий статистически значимого
результата не получить.

## Как отбирали

1. Репликация вне исходной выборки: другие периоды, страны и классы активов.
   Опора — Jensen, Kelly и Pedersen (153 фактора, 93 страны: большинство
   реплицируется) и более строгие Hou, Xue и Zhang (при весах по капитализации
   незначимы около 64% из 447 переменных; устойчивы в основном value, momentum,
   рентабельность и инвестиции, а также сезонность Heston–Sadka).
2. Свидетельства по российскому рынку или развивающимся рынкам.
3. Реализуемость на данных T-Invest: свечи, текущие мультипликаторы
   (`GetAssetFundamentals`), календарь дивидендов (`GetDividends`).
4. Комбинации — там, где факторы отрицательно коррелируют.

## Список

| # | Стратегия / комбинация | Главные доказательства | Россия / EM | В проекте |
|---|---|---|---|---|
| 1 | Cross-sectional momentum 12–1 | Jegadeesh–Titman; HXZ: реплицируется; JKP | Назарова–Лещёв (2019–2021): положительно после издержек | `cross-sectional-momentum` |
| 2 | Value + momentum | Asness–Moskowitz–Pedersen: корреляция ≈ −0,6, комбинация лучше каждого фактора во всех рынках | value на MOEX спорен | комбинация `value-composite` + `cross-sectional-momentum` |
| 3 | Quality / profitability | Novy-Marx (19 развитых рынков); QMJ: 23 из 24 стран; FF5 RMW | EM-премия рентабельности (практические исследования) | `quality-profitability` |
| 4 | Value composite | Fama–French, международно; JKP: value сильнее с контролями | Манушкин: в FF5 на России value избыточен | `value-composite` |
| 5 | Time-series momentum / тренд | Moskowitz–Ooi–Pedersen; критика Huang et al. | — | `time-series-momentum` |
| 6 | Risk-managed momentum | Barroso–Santa-Clara; Daniel–Moskowitz; критика Cederburg et al. | — | `risk-managed-momentum` |
| 7 | Низкий риск | Frazzini–Pedersen (BAB); Blitz–van Vliet; HXZ: idio-vol не реплицируется | low-vol подтверждён в EM | `low-volatility`, `betting-against-beta` |
| 8 | Сезонность доходностей | Heston–Sadka (2008, 2010); Keloharju et al.; HXZ: реплицируется | на EM слабее | `return-seasonality` |
| 9 | Мультифактор | JKP: 13 тем добавляют информацию друг к другу | — | комбинация value + quality + momentum + low-volatility |
| 10 | Дивидендная премия | Hartzmark–Solomon: 37–53 б.п. в месяц | БРИК: захват дивидендов; MOEX — противоречиво | `dividend-month` |

Новые в этой итерации: `value-composite`, `quality-profitability`,
`risk-managed-momentum`, `return-seasonality`, `dividend-month` и две комбинации
(value + momentum, мультифактор). Разборы — в `briefs/`.

## Рассмотрены и не включены

- **Turn-of-the-month** (McConnell–Xu: 31 из 35 стран). На индексе ММВБ за
  1997–2016 календарные аномалии исчезают после учёта спреда (Caporale–Zakirova).
- **Earnings announcement premium** (Frazzini–Lamont; Barber et al.: значим в 9
  из 20 стран). Даты отчётов в API есть не у всех эмитентов, а стратегия требует
  оборота около 100% в месяц.
- **MAX / лотерейные акции** (Bali–Cakici–Whitelaw). Международные репликации
  смешанные; категория trading frictions у HXZ реплицируется хуже всего.
- **Short-term reversal** уже есть в проекте, но у HXZ относится к
  нереплицируемым; в топ не включён.

## Что запущено в Sandbox

С 18 октября 2026 года в режиме «Постоянно, пока включена», по 1 000 000
виртуальных рублей на счёт, на одних и тех же десяти бумагах кампании (SBER,
GAZP, LKOH, GMKN, NVTK, ROSN, TATN, CHMF, MTSS, MAGN):

| Портфель | Счёт |
|---|---|
| `value-composite` | 715dff6b |
| `quality-profitability` | 2dabd44f |
| `value-momentum` | 27371c8d |
| `risk-managed-momentum` | 47f2bf59 |
| `return-seasonality` | 4fc99e36 |
| `dividend-month` | cb0ed81d |
| `multi-factor` | f68dfb94 |
| `cross-sectional-momentum` | b3c986cb |
| `time-series-momentum` | ca3b44d3 |

Прежние сценарии этих счетов остались в списке с историей. Low-volatility и BAB
отдельно не запущены — свободных счетов нет; low-volatility торгуется внутри
мультифактора.

## Источники

Полные библиографические записи и DOI — в `manifest.toml`; разборы новых
стратегий — в `briefs/`.
