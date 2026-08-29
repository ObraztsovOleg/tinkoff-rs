# Equal Risk Contribution — исследовательский разбор

Статус: метод распределения риска, а не прогноз доходности.

## Сигнал

Раз в месяц строится ковариационная матрица по 252 закрытым дневным доходностям.
Перед оптимизацией применяется 10% diagonal shrinkage:

```text
Sigma_shrunk = 0.9 * Sigma + 0.1 * diag(Sigma)
risk_contribution_i = w_i * (Sigma_shrunk * w)_i / portfolio_volatility
```

Детерминированный solver ищет положительные веса с одинаковыми risk
contributions, суммой весов 1 и максимумом 0.5 на инструмент. Портфель long-only,
gross 1. Нужны минимум два пригодных инструмента; при singular/invalid matrix
новый target не формируется.

## Основание

Maillard, Roncalli и Teiletche анализируют свойства equally weighted risk
contribution portfolios и их положение между equal-weight и minimum-variance.
Подход не обещает excess return: он только перераспределяет капитал на основе
оценённого риска.

Anderson, Bianchi и Goldberg подчёркивают, что исторические результаты risk
parity могут зависеть от leverage, bond exposure и выбранного периода. В этой
реализации нет облигационной multi-asset диверсификации оригинальных практик;
используется корзина MOEX equities.

## Риски

- Ковариации нестабильны, особенно во время regime shift.
- Акции одного рынка становятся сильнее коррелированы именно в кризис.
- Низкая историческая volatility автоматически повышает вес инструмента.
- Long-only ERC сохраняет общий рыночный риск и способен существенно падать.
- Лоты и малый бюджет могут сильно отклонить реализованный портфель от решения.

## Источники

- Основной оригинал: `../originals/maillard-roncalli-teiletche-2010.pdf`.
- Критический контекст: `../originals/anderson-bianchi-goldberg-2012.pdf`.
- Метаданные и адреса: `../manifest.toml`.
