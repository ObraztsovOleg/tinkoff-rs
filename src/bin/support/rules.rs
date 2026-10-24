//! Правила стенда, общие с супервизором и дашбордом: `contracts/stand-rules.json`.
//! Константы генерирует `build.rs`, Python и дашборд читают тот же файл.

/// Тема рынка, через которую события влияют на бумаги не напрямую
/// (см. `documents::topics`).
pub(crate) struct Topic {
    pub id: &'static str,
    pub name: &'static str,
    /// Что значит +1.
    pub up: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/stand_rules.rs"));
