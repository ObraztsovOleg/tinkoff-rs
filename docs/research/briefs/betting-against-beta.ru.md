# Betting Against Beta — исследовательский разбор

Статус: адаптированная long/short factor strategy с плечом; имеет повышенный
model, short и financing risk.

## Сигнал

Раз в месяц beta каждой акции к указанному MOEX benchmark оценивается по 252
закрытым дневным доходностям и стабилизируется:

```text
beta_shrunk = 0.6 * beta_ols + 0.4 * 1.0
```

Нижняя половина ranked universe образует long-leg, верхняя — short-leg. Веса
внутри legs обратно пропорциональны абсолютной shrunk beta, после чего legs
масштабируются к сопоставимой ex-ante beta. Итог ограничивается gross 2, net 1 и
50% strategy capital на бумагу. Требуются минимум десять пригодных акций,
benchmark и доступный short.

## Основание и адаптация

Frazzini и Pedersen связывают BAB premium с ограничениями на leverage: инвесторы
с ограниченным заёмным капиталом могут переплачивать за high-beta активы.
Оригинальный factor построен на широких международных данных и использует более
детальные beta estimates и financing assumptions. MOEX-реализация заметно уже.

Novy-Marx и Velikov показывают чувствительность BAB к portfolio construction,
стоимости исполнения и определению factor. Поэтому beta-neutral scaling не
трактуется как доказанная alpha.

## Риски

- Ошибка beta велика при короткой истории, halts и тонкой торговле.
- High-beta short может резко вырасти и стать недоступным для займа.
- Gross до 2 усиливает gap loss; бюджет не является максимальным убытком.
- Sector composition может объяснить значительную часть spread.
- Реальные financing и borrow costs в Sandbox представлены неполно.

## Источники

- Основной оригинал: `../originals/frazzini-pedersen-2014.pdf`.
- Критика: `../originals/novy-marx-velikov-2022.pdf`.
- Метаданные и адреса: `../manifest.toml`.
