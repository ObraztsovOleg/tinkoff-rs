//! Темы рынка, через которые события влияют на бумаги не напрямую.
//!
//! Знак темы в разметке документа — куда событие двигает её «показатель»:
//! для нефти +1 — рост цен и спроса, для санкций +1 — ослабление, для ставки
//! +1 — повышение. Чувствительность бумаги к теме (−2…+2) говорит, хорошо ли ей
//! движение темы вверх. Произведение знака, силы и чувствительности — ожидаемое
//! влияние события на бумагу.

pub(crate) use crate::rules::{TOPICS, Topic};

/// Тема по идентификатору.
pub(crate) fn find(id: &str) -> Option<&'static Topic> {
    TOPICS.iter().find(|topic| topic.id == id)
}

/// Список тем для промптов разметки и профилей.
pub(crate) fn describe() -> String {
    TOPICS
        .iter()
        .map(|topic| format!("- {} — {}: +1 = {}", topic.id, topic.name, topic.up))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Идентификаторы тем для схем инструментов модели.
pub(crate) fn ids() -> Vec<&'static str> {
    TOPICS.iter().map(|topic| topic.id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_are_unique_and_described() {
        let mut ids = ids();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), TOPICS.len());
        assert!(describe().contains("- key_rate — ключевая ставка"));
        assert_eq!(find("oil").map(|t| t.name), Some("нефть и нефтепродукты"));
        assert!(find("weather").is_none());
    }
}
