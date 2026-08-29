//! Сериализуемое состояние встроенных стратегий.
//!
//! Состояние не содержит broker client или runtime-объектов: его можно целиком
//! записать в storage после события и восстановить после рестарта процесса.

use chrono::{DateTime, Datelike, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::domain::{Candle, CandleInterval, InstrumentId};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
/// Общий снимок истории и маркеров ребалансировки одной встроенной стратегии.
pub(crate) struct BuiltinState {
    /// Отсортированные временные ряды по паре `(instrument_id, interval)`.
    series: Vec<SeriesState>,
    /// Последний год и месяц, для которого был успешно сформирован monthly target.
    pub(crate) last_month: Option<(i32, u32)>,
    /// ISO-год и номер недели последнего weekly target.
    pub(crate) last_week: Option<(i32, u32)>,
    /// Время свечи, уже породившей intraday-same-clock target.
    pub(crate) last_intraday_source: Option<DateTime<Utc>>,
    /// Выбранная пара и её открытое/закрытое состояние между сессиями.
    pub(crate) pair: Option<PairState>,
}

impl BuiltinState {
    /// Вставляет или заменяет закрытую свечу и оставляет только свежий хвост ряда.
    ///
    /// Повтор с тем же timestamp заменяет существующее значение, поэтому replay
    /// данных идемпотентен. `maximum_bars` ограничивает размер durable state.
    pub(crate) fn insert_candle(&mut self, candle: Candle, maximum_bars: usize) {
        if let Some(series) = self.series.iter_mut().find(|series| {
            series.instrument_id == candle.instrument_id && series.interval == candle.interval
        }) {
            series.insert(candle, maximum_bars);
            return;
        }
        let mut series = SeriesState {
            instrument_id: candle.instrument_id.clone(),
            interval: candle.interval,
            candles: Vec::new(),
        };
        series.insert(candle, maximum_bars);
        self.series.push(series);
        self.series.sort_by(|left, right| {
            left.instrument_id
                .as_str()
                .cmp(right.instrument_id.as_str())
                .then_with(|| interval_order(left.interval).cmp(&interval_order(right.interval)))
        });
    }

    /// Возвращает свечи инструмента и интервала в хронологическом порядке.
    ///
    /// Для ещё не прогретой комбинации возвращается пустой срез.
    pub(crate) fn candles(
        &self,
        instrument_id: &InstrumentId,
        interval: CandleInterval,
    ) -> &[Candle] {
        self.series
            .iter()
            .find(|series| &series.instrument_id == instrument_id && series.interval == interval)
            .map_or(&[], |series| series.candles.as_slice())
    }

    /// Возвращает наиболее поздний timestamp среди всех сохранённых рядов.
    ///
    /// Это значение записывается в `TargetPortfolio::source_as_of`, чтобы target
    /// можно было связать с конкретным срезом рыночных данных.
    pub(crate) fn newest_source_time(&self) -> Option<DateTime<Utc>> {
        self.series
            .iter()
            .filter_map(|series| series.candles.last().map(|candle| candle.time))
            .max()
    }

    /// Проверяет, не создавался ли target в календарном месяце этой сессии.
    pub(crate) fn is_new_month(&self, session: chrono::NaiveDate) -> bool {
        self.last_month != Some((session.year(), session.month()))
    }

    /// Помечает месяц успешно созданного target.
    pub(crate) fn mark_month(&mut self, session: chrono::NaiveDate) {
        self.last_month = Some((session.year(), session.month()));
    }

    /// Проверяет, не создавался ли target в ISO-неделе этой сессии.
    pub(crate) fn is_new_week(&self, session: chrono::NaiveDate) -> bool {
        let week = session.iso_week();
        self.last_week != Some((week.year(), week.week()))
    }

    /// Помечает ISO-неделю успешно созданного target.
    pub(crate) fn mark_week(&mut self, session: chrono::NaiveDate) {
        let week = session.iso_week();
        self.last_week = Some((week.year(), week.week()));
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
/// Один свечной ряд, однозначно заданный UID и интервалом.
struct SeriesState {
    instrument_id: InstrumentId,
    interval: CandleInterval,
    candles: Vec<Candle>,
}

impl SeriesState {
    /// Поддерживает хронологическую сортировку и bounded history.
    fn insert(&mut self, candle: Candle, maximum_bars: usize) {
        // binary_search одновременно обеспечивает быструю дедупликацию timestamp и
        // правильную вставку запоздавшей свечи в середину ряда.
        match self
            .candles
            .binary_search_by_key(&candle.time, |existing| existing.time)
        {
            Ok(index) => self.candles[index] = candle,
            Err(index) => self.candles.insert(index, candle),
        }
        // Удаляются самые старые свечи: все сигналы используют свежий хвост истории.
        if self.candles.len() > maximum_bars {
            let remove = self.candles.len() - maximum_bars;
            self.candles.drain(..remove);
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
/// Текущее направление позиции относительно спреда выбранной пары.
pub(crate) enum PairPosition {
    /// Обе ноги должны быть закрыты.
    Flat,
    /// Long левой нормированной цены и short правой.
    LongSpread,
    /// Short левой нормированной цены и long правой.
    ShortSpread,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
/// Параметры выбранной пары, зафиксированные на formation-периоде.
pub(crate) struct PairState {
    /// UID левой ноги формулы спреда.
    pub(crate) left: InstrumentId,
    /// UID правой ноги формулы спреда.
    pub(crate) right: InstrumentId,
    /// Первое закрытие formation-окна левой ноги для нормирования к единице.
    pub(crate) left_base: Decimal,
    /// Первое закрытие formation-окна правой ноги для нормирования к единице.
    pub(crate) right_base: Decimal,
    /// Среднее `left / left_base - right / right_base` на formation-окне.
    pub(crate) mean_spread: Decimal,
    /// Выборочное стандартное отклонение formation-спреда.
    pub(crate) standard_deviation: Decimal,
    /// Число сессий до обязательного закрытия и нового выбора пары.
    pub(crate) sessions_remaining: usize,
    /// Текущая позиция, необходимая для корректных правил входа и выхода.
    pub(crate) position: PairPosition,
}

/// Задаёт стабильный порядок интервалов при сериализации состояния.
///
/// Порядок не влияет на сигнал, но делает JSON детерминированным и удобным для
/// аудита: сначала внутридневные интервалы, затем дневной, недельный и месячный.
const fn interval_order(interval: CandleInterval) -> u8 {
    match interval {
        CandleInterval::FiveSeconds => 0,
        CandleInterval::TenSeconds => 1,
        CandleInterval::ThirtySeconds => 2,
        CandleInterval::OneMinute => 3,
        CandleInterval::TwoMinutes => 4,
        CandleInterval::ThreeMinutes => 5,
        CandleInterval::FiveMinutes => 6,
        CandleInterval::TenMinutes => 7,
        CandleInterval::FifteenMinutes => 8,
        CandleInterval::ThirtyMinutes => 9,
        CandleInterval::OneHour => 10,
        CandleInterval::TwoHours => 11,
        CandleInterval::FourHours => 12,
        CandleInterval::OneDay => 13,
        CandleInterval::OneWeek => 14,
        CandleInterval::OneMonth => 15,
    }
}
