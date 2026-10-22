//! Документы для ИИ-стратегии: ленты новостей с полными текстами, раскрытие
//! информации эмитентов, статьи, сообщения биржи и регулятора, пресс-релизы и
//! отчёты. Сбор (`collect`), хранение со склейкой повторов и поиск (`search`).
//!
//! Один и тот же факт приходит из нескольких лент: Интерфакс в T-Invest и в RSS,
//! перепечатки агентств. Точные повторы и почти одинаковые тексты (simhash)
//! попадают в один кластер: модель видит событие один раз, а число источников
//! повышает его вес.
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::json;
use tinkoff_rs::{Result, storage::PostgresStorage};

pub(crate) mod aliases;
pub(crate) mod cbr;
pub(crate) mod collect;
pub(crate) mod embed;
pub(crate) mod exposures;
pub(crate) mod facts;
pub(crate) mod issuers;
pub(crate) mod moex;
pub(crate) mod rss;
pub(crate) mod search;
pub(crate) mod sources;
pub(crate) mod tagging;
pub(crate) mod tinvest;
pub(crate) mod topics;
pub(crate) mod web;

/// Вид документа.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    /// Лента агентства (Интерфакс, РИА, ТАСС).
    News,
    /// Статья деловых изданий.
    Article,
    /// Раскрытие информации эмитентом: существенные факты, сообщения.
    Disclosure,
    /// Пресс-релиз с сайта эмитента.
    PressRelease,
    /// Отчётность эмитента.
    Report,
    /// Сообщение биржи.
    Exchange,
    /// Сообщение регулятора (Банк России).
    Regulator,
}

impl Kind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::News => "news",
            Self::Article => "article",
            Self::Disclosure => "disclosure",
            Self::PressRelease => "press_release",
            Self::Report => "report",
            Self::Exchange => "exchange",
            Self::Regulator => "regulator",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "news" => Self::News,
            "article" => Self::Article,
            "disclosure" => Self::Disclosure,
            "press_release" => Self::PressRelease,
            "report" => Self::Report,
            "exchange" => Self::Exchange,
            "regulator" => Self::Regulator,
            _ => return None,
        })
    }

    #[cfg(test)]
    pub(crate) const ALL: [Self; 7] = [
        Self::News,
        Self::Article,
        Self::Disclosure,
        Self::PressRelease,
        Self::Report,
        Self::Exchange,
        Self::Regulator,
    ];
}

/// Документ из источника до сохранения.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Document {
    /// Стабильный идентификатор в пределах источника: `tinvest:109974538`.
    pub id: String,
    /// Издатель: `interfax`, `eDisclosure`, `kommersant.ru`, `moex`, `cbr`.
    pub source: String,
    pub kind: Kind,
    pub url: String,
    pub title: String,
    pub lead: String,
    pub body: String,
    pub tables: Vec<String>,
    pub published: DateTime<Utc>,
    pub instruments: Vec<String>,
    pub tickers: Vec<String>,
    pub priority: bool,
}

impl Document {
    pub(crate) fn new(
        id: String,
        source: &str,
        kind: Kind,
        title: &str,
        published: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            source: source.to_owned(),
            kind,
            url: String::new(),
            title: clean(title, 500),
            lead: String::new(),
            body: String::new(),
            tables: Vec::new(),
            published,
            instruments: Vec::new(),
            tickers: Vec::new(),
            priority: false,
        }
    }
}

/// Стабильный 64-битный хэш FNV-1a.
pub(crate) fn fnv(parts: &[&str]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            // Разделитель частей: «ab»+«c» и «a»+«bc» дают разные хэши.
            hash ^= 0xff;
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        for byte in part.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    hash
}

/// Пробелы схлопнуты, длина ограничена.
pub(crate) fn clean(text: &str, limit: usize) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() <= limit {
        joined
    } else {
        joined.chars().take(limit).collect::<String>() + "…"
    }
}

/// Слова в нижнем регистре без знаков: основа для хэшей повторов.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Точный повтор: тот же заголовок и начало текста.
pub(crate) fn content_hash(document: &Document) -> String {
    let title = words(&document.title).join(" ");
    let start: String = words(&format!("{} {}", document.lead, document.body))
        .into_iter()
        .take(40)
        .collect::<Vec<_>>()
        .join(" ");
    format!("{:016x}", fnv(&[&title, &start]))
}

/// Simhash заголовка и начала текста по тройкам слов: перепечатка с правками
/// отличается на несколько бит.
pub(crate) fn simhash(document: &Document) -> u64 {
    let text = format!("{} {} {}", document.title, document.lead, document.body);
    let tokens: Vec<String> = words(&text).into_iter().take(120).collect();
    let shingles: Vec<String> = if tokens.len() < 3 {
        tokens
    } else {
        tokens.windows(3).map(|w| w.join(" ")).collect()
    };
    let mut weights = [0_i32; 64];
    for shingle in &shingles {
        let hash = fnv(&[shingle]);
        for (bit, weight) in weights.iter_mut().enumerate() {
            *weight += if hash >> bit & 1 == 1 { 1 } else { -1 };
        }
    }
    weights
        .iter()
        .enumerate()
        .filter(|(_, weight)| **weight > 0)
        .fold(0, |hash, (bit, _)| hash | 1 << bit)
}

/// Порог расстояния Хэмминга для почти одинаковых текстов.
const NEAR_DUPLICATE_BITS: i32 = 3;
/// Окно поиска повторов вокруг времени публикации.
const DUPLICATE_WINDOW: Duration = Duration::hours(48);
/// Одинаковый заголовок считается тем же событием в этом окне.
const SAME_TITLE_WINDOW: Duration = Duration::hours(6);
/// Длина фрагмента для поиска.
const CHUNK_CHARS: usize = 1500;

/// Фрагменты текста по абзацам, длинные абзацы — по предложениям. Первый
/// фрагмент начинается с заголовка и анонса.
pub(crate) fn chunks(document: &Document) -> Vec<String> {
    let mut pieces: Vec<String> = Vec::new();
    let head = [document.title.as_str(), document.lead.as_str()]
        .iter()
        .filter(|p| !p.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let mut current = head;
    let mut parts: Vec<String> = Vec::new();
    for paragraph in document
        .body
        .split('\n')
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        if paragraph.chars().count() <= CHUNK_CHARS {
            parts.push(paragraph.to_owned());
            continue;
        }
        let mut sentence = String::new();
        for piece in paragraph.split_inclusive(['.', '!', '?']) {
            if sentence.chars().count() + piece.chars().count() > CHUNK_CHARS
                && !sentence.is_empty()
            {
                parts.push(std::mem::take(&mut sentence));
            }
            sentence.push_str(piece);
        }
        if !sentence.trim().is_empty() {
            parts.push(sentence.trim().to_owned());
        }
    }
    for table in &document.tables {
        parts.push(clean(table, CHUNK_CHARS * 2));
    }
    for part in parts {
        if !current.is_empty() && current.chars().count() + part.chars().count() + 1 > CHUNK_CHARS {
            pieces.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(&part);
    }
    if !current.trim().is_empty() {
        pieces.push(current);
    }
    // Очень длинный кусок (таблица без абзацев) режется по символам.
    pieces
        .into_iter()
        .flat_map(|piece| {
            let chars: Vec<char> = piece.chars().collect();
            chars
                .chunks(CHUNK_CHARS * 2)
                .map(|c| c.iter().collect::<String>())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Подпись фрагмента: о каком документе он.
fn chunk_context(document: &Document) -> String {
    format!(
        "{} · {} · {}",
        document.title,
        document.source,
        (document.published + Duration::hours(3)).format("%Y-%m-%d")
    )
}

/// Кластер для нового документа: точный повтор или почти одинаковый текст рядом
/// по времени; иначе документ открывает свой кластер.
async fn cluster_for(
    storage: &PostgresStorage,
    document: &Document,
    hash: &str,
    simhash: i64,
) -> Result<String> {
    let from = document.published - DUPLICATE_WINDOW;
    let to = document.published + DUPLICATE_WINDOW;
    // Тот же заголовок в пределах нескольких часов — то же событие (уточнение
    // сообщения, перепечатка с другим текстом).
    let found: Option<String> = sqlx::query_scalar(
        "SELECT cluster_id FROM documents WHERE published_at BETWEEN $1 AND $2 \
         AND (content_hash = $3 OR bit_count((simhash # $4)::bit(64)) <= $5 \
              OR (lower(title) = lower($6) AND published_at BETWEEN $7 AND $8)) \
         ORDER BY (content_hash = $3) DESC, published_at LIMIT 1",
    )
    .bind(from)
    .bind(to)
    .bind(hash)
    .bind(simhash)
    .bind(NEAR_DUPLICATE_BITS)
    .bind(&document.title)
    .bind(document.published - SAME_TITLE_WINDOW)
    .bind(document.published + SAME_TITLE_WINDOW)
    .fetch_optional(storage.pool())
    .await?;
    Ok(found.unwrap_or_else(|| document.id.clone()))
}

/// Сохраняет новые документы с фрагментами; известные пропускает. Возвращает
/// число новых.
pub(crate) async fn store(storage: &PostgresStorage, documents: &[Document]) -> Result<usize> {
    if documents.is_empty() {
        return Ok(0);
    }
    let ids: Vec<&str> = documents.iter().map(|d| d.id.as_str()).collect();
    let known: Vec<String> = sqlx::query_scalar("SELECT id FROM documents WHERE id = ANY($1)")
        .bind(&ids)
        .fetch_all(storage.pool())
        .await?;
    let mut added = 0;
    for document in documents.iter().filter(|d| !known.contains(&d.id)) {
        if document.title.trim().is_empty() {
            continue;
        }
        let hash = content_hash(document);
        // Знаковое представление тех же 64 бит: в PostgreSQL нет беззнакового BIGINT.
        let simhash = i64::from_ne_bytes(simhash(document).to_ne_bytes());
        let cluster = cluster_for(storage, document, &hash, simhash).await?;
        let mut transaction = storage.pool().begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO documents(id,source,kind,url,title,lead,body,tables,published_at,instruments,tickers,priority,content_hash,simhash,cluster_id) \
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15) ON CONFLICT (id) DO NOTHING",
        )
        .bind(&document.id)
        .bind(&document.source)
        .bind(document.kind.as_str())
        .bind(&document.url)
        .bind(&document.title)
        .bind(&document.lead)
        .bind(&document.body)
        .bind(json!(document.tables))
        .bind(document.published)
        .bind(&document.instruments)
        .bind(&document.tickers)
        .bind(document.priority)
        .bind(&hash)
        .bind(simhash)
        .bind(&cluster)
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        if inserted == 0 {
            transaction.rollback().await?;
            continue;
        }
        let pieces = chunks(document);
        let ords: Vec<i32> = (0..i32::try_from(pieces.len()).unwrap_or(i32::MAX)).collect();
        sqlx::query(
            "INSERT INTO document_chunks(document_id,ord,text,context,published_at) \
             SELECT $1, ord, text, $4, $5 FROM UNNEST($2::int[], $3::text[]) AS t(ord, text)",
        )
        .bind(&document.id)
        .bind(&ords)
        .bind(&pieces)
        .bind(chunk_context(document))
        .bind(document.published)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        added += 1;
    }
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(title: &str, body: &str) -> Document {
        let mut document = Document::new(
            format!("test:{}", fnv(&[title])),
            "test",
            Kind::News,
            title,
            DateTime::from_timestamp(1_792_390_000, 0).unwrap(),
        );
        document.body = body.to_owned();
        document
    }

    #[test]
    fn reprints_share_hashes_and_near_copies_differ_by_a_few_bits() {
        let body = "Совет директоров ПАО Сбербанк рекомендовал выплатить дивиденды за 2026 год в размере 37 рублей на акцию. \
                    Дата закрытия реестра назначена на 18 июля. Решение примет годовое собрание акционеров.";
        let original = document("Сбербанк рекомендовал дивиденды 37 рублей", body);
        let reprint = document("СБЕРБАНК рекомендовал дивиденды — 37 рублей!", body);
        assert_eq!(
            content_hash(&original),
            content_hash(&reprint),
            "case and punctuation do not matter"
        );
        let edited = document(
            "Сбербанк рекомендовал дивиденды 37 рублей",
            &body.replace("18 июля", "19 июля"),
        );
        let distance = (simhash(&original) ^ simhash(&edited)).count_ones();
        assert!(distance <= 12, "a small edit keeps most bits: {distance}");
        let other = document(
            "Нефть Brent подорожала до 110 долларов",
            "Котировки нефти растут на фоне сообщений о перебоях поставок через пролив.",
        );
        assert!((simhash(&original) ^ simhash(&other)).count_ones() > 15);
    }

    #[test]
    fn chunks_start_with_the_title_and_split_long_texts() {
        let paragraph = "Предложение о выручке компании. ".repeat(80);
        let mut long = document(
            "Отчёт МСФО",
            &format!("Первый абзац.\n{paragraph}\nПоследний абзац."),
        );
        long.lead = "Выручка выросла".to_owned();
        long.tables.push("Показатель | 2026 | 2025".to_owned());
        let pieces = chunks(&long);
        assert!(pieces[0].starts_with("Отчёт МСФО\nВыручка выросла\nПервый абзац."));
        assert!(pieces.len() >= 2);
        assert!(pieces.iter().all(|p| p.chars().count() <= CHUNK_CHARS * 2));
        assert!(pieces.last().unwrap().contains("Показатель | 2026"));
        assert_eq!(
            chunks(&document("Только заголовок", "")),
            vec!["Только заголовок"]
        );
    }

    #[test]
    fn kinds_round_trip() {
        for kind in Kind::ALL {
            assert_eq!(Kind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(Kind::parse("rumor"), None);
    }
}
