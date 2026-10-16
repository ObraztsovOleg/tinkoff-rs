# Volatility-Managed Portfolio — исследовательский разбор

Статус: risk overlay для long-only benchmark basket, а не самостоятельный
cross-sectional predictor.

## Сигнал

Раз в месяц сначала формируется равновзвешенная long-корзина минимум из четырёх
пригодных инструментов. По её дневным доходностям за предыдущий календарный
месяц оценивается annualized realized variance `RV`. Целевая экспозиция:

```text
exposure = clamp((0.20^2) / RV, 0, 2)
```

При нулевой, отрицательной или неполной оценке RV новый target не
формируется. Формула исследования может запросить `exposure > 1`, но
в этой реализации long-only target дополнительно ограничивается значением 1:
иначе сумма знаковых весов нарушила бы фиксированный `net <= 1`. Веса внутри
корзины равные и ограничиваются лимитом 0.5 на инструмент.

## Основание и оговорки

Moreira и Muir показали, что уменьшение factor exposure при высокой прошлой
волатильности в их выборке повышало risk-adjusted результаты. Формула данной
реализации фиксирует 20% annual volatility target и использует простую MOEX
корзину, поэтому не воспроизводит все factor portfolios статьи.

Cederburg, O'Doherty, Wang и Yan обнаружили, что реализуемые out-of-sample
версии volatility management не систематически превосходят unmanaged portfolio.
Положительная regression alpha не обязательно превращается в полезный
real-time результат.

## Риски

- После спокойного месяца стратегия максимизирует плечо прямо перед shock.
- Monthly RV шумна и способна резко менять exposure.
- Без effective cap исследовательская формула допускала бы nominal gross 2;
  runtime не разрешает его для long-only target из-за `net <= 1`.
- Реальная стоимость funding, комиссии и margin rules отличаются от Sandbox.
- Equal-weight basket сохраняет sector, country и currency concentration.

## Источники

- Основной оригинал: `../originals/moreira-muir-2017.pdf`.
- Критика: `../originals/cederburg-odoherty-wang-yan-2020.pdf`.
- Метаданные и адреса: `../manifest.toml`.
