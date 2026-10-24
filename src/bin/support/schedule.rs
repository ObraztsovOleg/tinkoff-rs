//! Официальное расписание MOEX: дата текущей сессии и моменты, в которые
//! стратегия просила её вызвать (открытие, аукцион закрытия, конец сессии).

use std::collections::HashSet;

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use tinkoff_rs::{
    Error, Result,
    domain::{datetime_to_timestamp, timestamp_to_datetime},
    engine::{
        EventPhase, ScheduleAnchor, ScheduleEvent, ScheduleRequirement, StrategyEvent,
        StrategyEventKind,
    },
    proto::TradingSchedulesRequest,
};

use crate::{OPEN_RETRY_MARGIN_MINUTES, SCHEDULE_GRACE_SECONDS, venue::VenueApi};

/// Одно рассчитанное срабатывание относительно официальной сессии MOEX.
#[derive(Clone)]
pub(super) struct ScheduledTrigger {
    pub(super) anchor: ScheduleAnchor,
    pub(super) session_date: NaiveDate,
    pub(super) at: DateTime<Utc>,
    pub(super) offset_seconds: i64,
    /// Конец основной сессии из официального расписания.
    pub(super) session_end: Option<DateTime<Utc>>,
}

impl ScheduledTrigger {
    /// До какого момента повторять отложенное решение. Повторяется только
    /// основное открытие сессии: offset-события intraday-стратегий привязаны к
    /// конкретному времени и после него теряют смысл.
    pub(super) fn retry_until(&self) -> Option<DateTime<Utc>> {
        (self.anchor == ScheduleAnchor::SessionOpen && self.offset_seconds == 0)
            .then_some(self.session_end?)
            .map(|end| end - Duration::minutes(OPEN_RETRY_MARGIN_MINUTES))
    }

    /// Строит стабильный ID, чтобы одно расписание нельзя было исполнить дважды.
    pub(super) fn id(&self) -> String {
        format!(
            "schedule:{:?}:{}:{}",
            self.anchor,
            self.session_date,
            self.at.to_rfc3339()
        )
    }

    /// Преобразует внутренний trigger в live-событие для стратегии и risk manager.
    pub(super) fn event(self) -> StrategyEvent {
        let id = self.id();
        StrategyEvent {
            id,
            phase: EventPhase::Live,
            occurred_at: self.at,
            payload: StrategyEventKind::Schedule(ScheduleEvent {
                anchor: self.anchor,
                session_date: self.session_date,
                scheduled_at: self.at,
            }),
        }
    }
}

/// Определяет последнюю уже начавшуюся официальную торговую сессию MOEX.
///
/// Простая UTC-дата неверна ночью, в выходные и на специальных сессиях. Risk
/// manager использует эту дату до первой заявки, поэтому после restart дневной
/// stop не может случайно унаследовать вчерашнюю базу.
pub(super) async fn trading_schedules(
    api: &VenueApi,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<tinkoff_rs::proto::TradingSchedulesResponse> {
    let mut response = tinkoff_rs::proto::TradingSchedulesResponse::default();
    let mut cursor = schedule_start(from);
    while cursor < to {
        // The API counts calendar dates inclusively, so use a margin below fourteen days.
        let end = (cursor + Duration::days(12)).min(to);
        let page = api
            .instruments()
            .trading_schedules(TradingSchedulesRequest {
                exchange: Some("MOEX".to_owned()),
                from: Some(datetime_to_timestamp(cursor)),
                to: Some(datetime_to_timestamp(end)),
            })
            .await?
            .into_inner();
        response.exchanges.extend(page.exchanges);
        cursor = end;
    }
    Ok(response)
}

/// Начало запроса расписания. `TradingSchedules` отклоняет `from` с датой раньше
/// текущей московской («30003»): с 00:00 до 03:00 МСК дата `now` в UTC ещё
/// вчерашняя, поэтому запрос начинается с полуночи UTC московской даты.
pub(super) fn schedule_start(from: DateTime<Utc>) -> DateTime<Utc> {
    let moscow_date = (from + Duration::hours(3)).date_naive();
    from.max(moscow_date.and_time(NaiveTime::MIN).and_utc())
}

pub(super) async fn current_moex_session_date(
    api: &VenueApi,
    now: DateTime<Utc>,
    previous: Option<NaiveDate>,
) -> Result<NaiveDate> {
    // TradingSchedules rejects historical `from` dates. The durable risk session
    // is the authority across nights/weekends; a new flat account uses its next session.
    let response = trading_schedules(api, now, now + Duration::days(12)).await?;
    session_date_from_schedule(&response, now, previous)
}

pub(super) fn session_date_from_schedule(
    response: &tinkoff_rs::proto::TradingSchedulesResponse,
    now: DateTime<Utc>,
    previous: Option<NaiveDate>,
) -> Result<NaiveDate> {
    let mut sessions = Vec::new();
    for exchange in &response.exchanges {
        if !exchange.exchange.to_ascii_lowercase().contains("moex") {
            continue;
        }
        for day in exchange.days.iter().filter(|d| d.is_trading_day) {
            if let Some(start) = day.start_time {
                let start = timestamp_to_datetime(start)?;
                let date = day
                    .date
                    .map(timestamp_to_datetime)
                    .transpose()?
                    .map_or(start.date_naive(), |d| d.date_naive());
                sessions.push((start, date));
            }
        }
    }
    sessions.sort_unstable();
    sessions
        .iter()
        .rev()
        .find(|(start, _)| *start <= now)
        .map(|(_, date)| *date)
        .or(previous)
        .or_else(|| sessions.first().map(|(_, date)| *date))
        .ok_or_else(|| Error::Configuration("MOEX schedule contains no usable session".to_owned()))
}

/// Загружает официальное расписание MOEX и разворачивает требования стратегии
/// в абсолютные UTC-моменты на ближайшие две недели.
///
/// Время берётся из ответа биржевого календаря, а не вычисляется фиксированным
/// московским часом: так учитываются выходные, сокращённые дни и особые сессии.
pub(super) async fn load_schedule_triggers(
    api: &VenueApi,
    requirements: &[ScheduleRequirement],
    now: DateTime<Utc>,
) -> Result<Vec<ScheduledTrigger>> {
    if requirements.is_empty() {
        return Ok(Vec::new());
    }
    let response = trading_schedules(api, now, now + Duration::days(12)).await?;
    let mut triggers = Vec::new();
    for exchange in response.exchanges {
        if !exchange.exchange.to_ascii_lowercase().contains("moex") {
            continue;
        }
        for day in exchange.days.into_iter().filter(|day| day.is_trading_day) {
            let Some(date) = day
                .date
                .or(day.start_time)
                .map(timestamp_to_datetime)
                .transpose()?
                .map(|date| date.date_naive())
            else {
                continue;
            };
            for requirement in requirements {
                // Offset применяется после выбора конкретного якоря: открытия,
                // закрытия или начала аукциона закрытия этой сессии.
                let timestamp = match requirement.anchor {
                    ScheduleAnchor::SessionOpen => day.start_time,
                    ScheduleAnchor::SessionClose => day.end_time,
                    ScheduleAnchor::ClosingAuctionStart => day.closing_auction_start_time,
                };
                if let Some(timestamp) = timestamp {
                    triggers.push(ScheduledTrigger {
                        anchor: requirement.anchor,
                        session_date: date,
                        at: timestamp_to_datetime(timestamp)?
                            + Duration::seconds(requirement.offset_seconds),
                        offset_seconds: requirement.offset_seconds,
                        session_end: day.end_time.map(timestamp_to_datetime).transpose()?,
                    });
                }
            }
        }
    }
    triggers.sort_by_key(|trigger| trigger.at);
    Ok(triggers)
}

/// Окна основных сессий `(открытие, конец)` из официального расписания.
pub(super) fn session_windows(
    triggers: &[ScheduledTrigger],
) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    triggers
        .iter()
        .filter(|t| t.anchor == ScheduleAnchor::SessionOpen && t.offset_seconds == 0)
        .filter_map(|t| t.session_end.map(|end| (t.at, end)))
        .collect()
}

/// Извлекает наступившие события без дубликатов: `(trigger, late)`.
/// Будущие triggers остаются в векторе, обработанные удаляются через `retain`.
///
/// Событие старше двух минут (компьютер спал, процесс был занят) возвращается
/// с `late = true`: оно не исполняется сразу, но учитывается стратегией, а
/// решение открытия сессии ставится в очередь повтора до конца этой сессии.
pub(super) fn take_due_triggers(
    triggers: &mut Vec<ScheduledTrigger>,
    fired: &mut HashSet<String>,
    now: DateTime<Utc>,
) -> Vec<(ScheduledTrigger, bool)> {
    let mut due = Vec::new();
    triggers.retain(|trigger| {
        if trigger.at > now {
            return true;
        }
        let age = now.signed_duration_since(trigger.at).num_seconds();
        if fired.insert(trigger.id()) {
            due.push((trigger.clone(), age > SCHEDULE_GRACE_SECONDS));
        }
        false
    });
    due
}
