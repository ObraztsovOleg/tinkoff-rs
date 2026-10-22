//! Как пресса называет эмитентов: тикер, название из T-Invest и привычные
//! имена («Норникель», «Сбер»). Из них строится полнотекстовый запрос,
//! находящий документы о бумаге в любом падеже.

/// Привычные названия ликвидных бумаг MOEX. Остальные бумаги ищутся по тикеру
/// и названию из справочника T-Invest.
const KNOWN: &[(&str, &[&str])] = &[
    ("SBER", &["сбербанк", "сбер"]),
    ("SBERP", &["сбербанк", "сбер"]),
    ("GAZP", &["газпром"]),
    ("LKOH", &["лукойл"]),
    ("GMKN", &["норникель", "норильский никель"]),
    ("NVTK", &["новатэк"]),
    ("ROSN", &["роснефть"]),
    ("TATN", &["татнефть"]),
    ("TATNP", &["татнефть"]),
    ("SIBN", &["газпром нефть"]),
    ("SNGS", &["сургутнефтегаз"]),
    ("SNGSP", &["сургутнефтегаз"]),
    ("TRNFP", &["транснефть"]),
    ("CHMF", &["северсталь"]),
    ("MAGN", &["ммк", "магнитогорский металлургический"]),
    ("NLMK", &["нлмк", "новолипецкий"]),
    ("PLZL", &["полюс"]),
    ("ALRS", &["алроса"]),
    ("RUAL", &["русал"]),
    ("PHOR", &["фосагро"]),
    ("MTSS", &["мтс"]),
    ("RTKM", &["ростелеком"]),
    ("MOEX", &["мосбиржа", "московская биржа"]),
    ("VTBR", &["втб"]),
    ("T", &["т-банк", "тинькофф", "т-технологии"]),
    ("YDEX", &["яндекс"]),
    ("OZON", &["озон"]),
    ("MGNT", &["магнит"]),
    ("X5", &["x5", "пятерочка"]),
    ("AFLT", &["аэрофлот"]),
    ("IRAO", &["интер рао"]),
    ("HYDR", &["русгидро"]),
    ("FEES", &["россети"]),
    ("AFKS", &["афк система"]),
    ("POSI", &["позитив"]),
    ("SMLT", &["самолет"]),
    ("PIKK", &["группа пик"]),
    ("MTLR", &["мечел"]),
    ("RASP", &["распадская"]),
    ("UPRO", &["юнипро"]),
    ("ENPG", &["эн+"]),
    ("BSPB", &["банк санкт-петербург"]),
    ("SVCB", &["совкомбанк"]),
    ("CBOM", &["московский кредитный банк", "мкб"]),
    ("HEAD", &["хэдхантер", "headhunter"]),
    ("BELU", &["новабев", "белуга"]),
    ("SELG", &["селигдар"]),
    ("UGLD", &["южуралзолото"]),
    ("FLOT", &["совкомфлот"]),
];

/// Слова названия без организационно-правовой формы и кавычек.
fn name_words(name: &str) -> Vec<String> {
    name.split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '+'))
        .map(str::to_lowercase)
        .filter(|w| {
            !w.is_empty()
                && !matches!(
                    w.as_str(),
                    "пао" | "оао" | "ао" | "ап" | "ап." | "group" | "plc" | "ltd"
                )
        })
        .collect()
}

/// Названия бумаги для поиска.
pub(crate) fn names(ticker: &str, name: &str) -> Vec<String> {
    let mut out: Vec<String> = KNOWN
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(ticker))
        .map(|(_, aliases)| aliases.iter().map(|a| (*a).to_owned()).collect())
        .unwrap_or_default();
    let words = name_words(name);
    if !words.is_empty() {
        out.push(words.join(" "));
    }
    out.push(ticker.to_lowercase());
    out.sort();
    out.dedup();
    out
}

/// Слово для `to_tsquery`: только буквы и цифры.
fn lexeme(word: &str) -> String {
    word.chars().filter(|c| c.is_alphanumeric()).collect()
}

/// Часть запроса `to_tsquery('russian', …)` для одного названия: слово или
/// фраза. Падежи покрывает русская морфология («Газпрома» → «газпром»); префиксы
/// не используются — иначе «газпром» находил бы «Газпромбанк».
fn term(alias: &str) -> Option<String> {
    let words: Vec<String> = alias
        .split_whitespace()
        .map(lexeme)
        .filter(|w| !w.is_empty())
        .collect();
    match words.as_slice() {
        [] => None,
        [word] => Some(word.clone()),
        many => Some(format!("({})", many.join(" <-> "))),
    }
}

/// Запрос `to_tsquery('russian', …)`: любое из названий любой из бумаг.
pub(crate) fn tsquery<'a>(instruments: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    let mut terms: Vec<String> = instruments
        .into_iter()
        .flat_map(|(ticker, name)| names(ticker, name))
        .filter_map(|alias| term(&alias))
        .collect();
    terms.sort();
    terms.dedup();
    terms.join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_names_tickers_and_phrases_build_a_query() {
        assert_eq!(
            names("GMKN", "Норильский никель"),
            vec!["gmkn", "норильский никель", "норникель"]
        );
        assert_eq!(
            tsquery([("GMKN", "Норильский никель"), ("SBER", "Сбер Банк")]),
            "(норильский <-> никель) | (сбер <-> банк) | gmkn | sber | норникель | сбер | сбербанк"
        );
        assert_eq!(
            tsquery([("XYZ", "ПАО «Новая Компания»")]),
            "(новая <-> компания) | xyz"
        );
        assert_eq!(term("эн+"), Some("эн".to_owned()));
    }
}
