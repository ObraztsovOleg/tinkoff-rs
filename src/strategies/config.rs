//! TOML-конфигурации встроенных стратегий.
//!
//! Числа дней означают количество **закрытых торговых свечей**, а не календарных
//! дней. Проценты передаются строками и разбираются через [`Decimal`], поэтому их
//! можно задавать без потери точности: например, `band_percent = "1.0"`.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::domain::InstrumentId;

// Значения по умолчанию вынесены в функции, потому что serde вызывает их при
// отсутствии соответствующего поля в TOML.
fn default_fast_window() -> usize {
    50
}

fn default_slow_window() -> usize {
    200
}

fn default_band_percent() -> Decimal {
    Decimal::ONE
}

fn default_range_lookback() -> usize {
    50
}

fn default_horizons() -> Vec<usize> {
    vec![21, 63, 252]
}

fn default_volatility_window() -> usize {
    63
}

fn default_formation_days() -> usize {
    252
}

fn default_skip_days() -> usize {
    21
}

fn default_quantile_percent() -> usize {
    20
}

fn default_intermediate_end_days() -> usize {
    126
}

fn default_pair_trading_days() -> usize {
    126
}

fn default_entry_z_score() -> Decimal {
    Decimal::from(2)
}

fn default_reversal_days() -> usize {
    5
}

fn default_thirty_minutes() -> u32 {
    30
}

fn default_same_clock_sessions() -> usize {
    20
}

fn default_volatility_months() -> usize {
    60
}

fn default_beta_shrinkage() -> Decimal {
    Decimal::new(6, 1)
}

fn default_covariance_shrinkage_percent() -> Decimal {
    Decimal::from(10)
}

fn default_erc_iterations() -> usize {
    100
}

fn default_erc_tolerance() -> Decimal {
    Decimal::new(1, 8)
}

fn default_realized_variance_days() -> usize {
    21
}

fn default_target_volatility_percent() -> Decimal {
    Decimal::from(20)
}

fn default_max_leverage() -> Decimal {
    Decimal::from(2)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// Параметры тренда по пересечению быстрой и медленной скользящих средних.
pub struct MovingAverageTrendConfig {
    /// Число дневных закрытий в быстрой SMA (по умолчанию 50).
    #[serde(default = "default_fast_window")]
    pub fast_window: usize,
    /// Число дневных закрытий в медленной SMA (по умолчанию 200).
    #[serde(default = "default_slow_window")]
    pub slow_window: usize,
    /// Нейтральная полоса вокруг медленной SMA, в процентах.
    ///
    /// Long возникает при `fast > slow * (1 + band / 100)`, short — при
    /// `fast < slow * (1 - band / 100)`; внутри полосы целевой вес равен нулю.
    #[serde(default = "default_band_percent", with = "rust_decimal::serde::str")]
    pub band_percent: Decimal,
}

impl Default for MovingAverageTrendConfig {
    fn default() -> Self {
        Self {
            fast_window: default_fast_window(),
            slow_window: default_slow_window(),
            band_percent: default_band_percent(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// Параметры пробоя недавнего ценового диапазона.
pub struct RangeBreakoutConfig {
    /// Длина диапазона из предыдущих дневных свечей (текущая свеча не входит).
    #[serde(default = "default_range_lookback")]
    pub lookback_days: usize,
    /// Дополнительный отступ за максимумом/минимумом диапазона, в процентах.
    #[serde(default = "default_band_percent", with = "rust_decimal::serde::str")]
    pub filter_percent: Decimal,
}

impl Default for RangeBreakoutConfig {
    fn default() -> Self {
        Self {
            lookback_days: default_range_lookback(),
            filter_percent: default_band_percent(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// Параметры momentum каждой отдельной временной серии.
pub struct TimeSeriesMomentumConfig {
    /// Горизонты доходности в дневных свечах; по умолчанию 21, 63 и 252.
    /// Знаки доходностей по горизонтам суммируются.
    #[serde(default = "default_horizons")]
    pub horizons_days: Vec<usize>,
    /// Окно дневных доходностей для оценки волатильности и inverse-vol веса.
    #[serde(default = "default_volatility_window")]
    pub volatility_window_days: usize,
}

impl Default for TimeSeriesMomentumConfig {
    fn default() -> Self {
        Self {
            horizons_days: default_horizons(),
            volatility_window_days: default_volatility_window(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// Параметры кросс-секционного momentum «12–2 месяца».
pub struct CrossSectionalMomentumConfig {
    /// Начало окна формирования, в дневных свечах назад от последнего закрытия.
    #[serde(default = "default_formation_days")]
    pub formation_days: usize,
    /// Число последних дневных свечей, исключённых из сигнала.
    #[serde(default = "default_skip_days")]
    pub skip_days: usize,
    /// Размер каждого крайнего хвоста ранга, в процентах от вселенной.
    #[serde(default = "default_quantile_percent")]
    pub quantile_percent: usize,
}

impl Default for CrossSectionalMomentumConfig {
    fn default() -> Self {
        Self {
            formation_days: default_formation_days(),
            skip_days: default_skip_days(),
            quantile_percent: default_quantile_percent(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// Параметры промежуточного momentum (по умолчанию месяцы 12–7).
pub struct IntermediateMomentumConfig {
    /// Более далёкая граница окна доходности, в дневных свечах назад.
    #[serde(default = "default_formation_days")]
    pub start_days_ago: usize,
    /// Более близкая граница окна доходности, в дневных свечах назад.
    #[serde(default = "default_intermediate_end_days")]
    pub end_days_ago: usize,
    /// Размер long- и short-хвоста ранга, в процентах.
    #[serde(default = "default_quantile_percent")]
    pub quantile_percent: usize,
}

impl Default for IntermediateMomentumConfig {
    fn default() -> Self {
        Self {
            start_days_ago: default_formation_days(),
            end_days_ago: default_intermediate_end_days(),
            quantile_percent: default_quantile_percent(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// Параметры стратегии близости к 52-недельному максимуму.
pub struct FiftyTwoWeekHighConfig {
    /// Окно поиска максимальной цены (`high`) в дневных свечах.
    #[serde(default = "default_formation_days")]
    pub lookback_days: usize,
    /// Размер крайних хвостов ранга `последнее закрытие / максимум`.
    #[serde(default = "default_quantile_percent")]
    pub quantile_percent: usize,
}

impl Default for FiftyTwoWeekHighConfig {
    fn default() -> Self {
        Self {
            lookback_days: default_formation_days(),
            quantile_percent: default_quantile_percent(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// Параметры momentum доходности, не объяснённой рыночным бенчмарком.
pub struct ResidualMomentumConfig {
    /// UID неторгуемого инструмента-бенчмарка для однофакторной регрессии.
    pub benchmark_uid: InstrumentId,
    /// Полное окно регрессии и формирования сигнала, в дневных свечах.
    #[serde(default = "default_formation_days")]
    pub formation_days: usize,
    /// Число последних наблюдений, исключённых из суммы остатков.
    #[serde(default = "default_skip_days")]
    pub skip_days: usize,
    /// Размер крайних хвостов ранга стандартизованного residual momentum.
    #[serde(default = "default_quantile_percent")]
    pub quantile_percent: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// Параметры парной торговли нормированным ценовым спредом.
pub struct PairsTradingConfig {
    /// Число дневных закрытий для выбора пары и оценки спреда.
    #[serde(default = "default_formation_days")]
    pub formation_days: usize,
    /// Максимальная жизнь выбранной пары до обязательного закрытия/перевыбора.
    #[serde(default = "default_pair_trading_days")]
    pub trading_days: usize,
    /// Абсолютный z-score, при котором открывается позиция на возврат к среднему.
    #[serde(default = "default_entry_z_score", with = "rust_decimal::serde::str")]
    pub entry_z_score: Decimal,
}

impl Default for PairsTradingConfig {
    fn default() -> Self {
        Self {
            formation_days: default_formation_days(),
            trading_days: default_pair_trading_days(),
            entry_z_score: default_entry_z_score(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// Параметры недельного разворота относительно рынка.
pub struct ShortTermReversalConfig {
    /// UID неторгуемого рыночного бенчмарка.
    pub benchmark_uid: InstrumentId,
    /// Горизонт краткосрочной доходности в дневных свечах (по умолчанию 5).
    #[serde(default = "default_reversal_days")]
    pub lookback_days: usize,
    /// Размер хвостов: проигравшие идут в long, победители — в short.
    #[serde(default = "default_quantile_percent")]
    pub quantile_percent: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// Параметры внутридневного momentum рынка.
pub struct MarketIntradayMomentumConfig {
    /// Длительность окна после открытия, задающего направление позиции.
    /// В portfolio runtime v2 поддерживается ровно 30 минут.
    #[serde(default = "default_thirty_minutes")]
    pub opening_window_minutes: u32,
    /// За сколько минут до завершающего аукциона сформировать цель.
    /// В portfolio runtime v2 поддерживается ровно 30 минут.
    #[serde(default = "default_thirty_minutes")]
    pub final_window_minutes: u32,
}

impl Default for MarketIntradayMomentumConfig {
    fn default() -> Self {
        Self {
            opening_window_minutes: default_thirty_minutes(),
            final_window_minutes: default_thirty_minutes(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// Параметры предсказания следующего внутридневного слота по тому же времени.
pub struct IntradaySameClockConfig {
    /// Число прошлых сессий для среднего возврата того же временного слота.
    #[serde(default = "default_same_clock_sessions")]
    pub lookback_sessions: usize,
    /// Размер long- и short-хвоста ранга прогнозов.
    #[serde(default = "default_quantile_percent")]
    pub quantile_percent: usize,
    /// Размер внутридневной свечи; portfolio runtime v2 поддерживает ровно 30 минут.
    #[serde(default = "default_thirty_minutes")]
    pub interval_minutes: u32,
}

impl Default for IntradaySameClockConfig {
    fn default() -> Self {
        Self {
            lookback_sessions: default_same_clock_sessions(),
            quantile_percent: default_quantile_percent(),
            interval_minutes: default_thirty_minutes(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// Параметры long-only портфеля акций с низкой волатильностью.
pub struct LowVolatilityConfig {
    /// Окно стандартного отклонения месячных доходностей.
    #[serde(default = "default_volatility_months")]
    pub lookback_months: usize,
    /// Доля инструментов с наименьшей волатильностью, в процентах.
    #[serde(default = "default_quantile_percent")]
    pub quantile_percent: usize,
}

impl Default for LowVolatilityConfig {
    fn default() -> Self {
        Self {
            lookback_months: default_volatility_months(),
            quantile_percent: default_quantile_percent(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// Параметры Betting Against Beta (BAB).
pub struct BettingAgainstBetaConfig {
    /// UID неторгуемого рыночного бенчмарка, относительно которого считается beta.
    pub benchmark_uid: InstrumentId,
    /// Окно OLS-оценки beta в дневных свечах.
    #[serde(default = "default_formation_days")]
    pub lookback_days: usize,
    /// Доля сырой beta в shrunk-оценке `s * beta + (1 - s) * 1`.
    /// Значение `0` полностью притягивает beta к единице, `1` не сглаживает её.
    #[serde(default = "default_beta_shrinkage", with = "rust_decimal::serde::str")]
    pub shrinkage: Decimal,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// Параметры long-only Equal Risk Contribution (ERC).
pub struct EqualRiskContributionConfig {
    /// Окно оценки ковариационной матрицы дневных доходностей.
    #[serde(default = "default_formation_days")]
    pub lookback_days: usize,
    /// Сила диагонального shrinkage ковариаций, в процентах.
    /// При `100` внедиагональные ковариации обнуляются.
    #[serde(
        default = "default_covariance_shrinkage_percent",
        with = "rust_decimal::serde::str"
    )]
    pub diagonal_shrinkage_percent: Decimal,
    /// Верхняя граница числа итераций решателя равных вкладов в риск.
    #[serde(default = "default_erc_iterations")]
    pub max_iterations: usize,
    /// Допустимая относительная ошибка вклада инструмента в дисперсию портфеля.
    #[serde(default = "default_erc_tolerance", with = "rust_decimal::serde::str")]
    pub tolerance: Decimal,
}

impl Default for EqualRiskContributionConfig {
    fn default() -> Self {
        Self {
            lookback_days: default_formation_days(),
            diagonal_shrinkage_percent: default_covariance_shrinkage_percent(),
            max_iterations: default_erc_iterations(),
            tolerance: default_erc_tolerance(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// Параметры управления экспозицией по реализованной дисперсии рынка.
pub struct VolatilityManagedConfig {
    /// Окно реализованной дневной дисперсии равновзвешенной корзины.
    #[serde(default = "default_realized_variance_days")]
    pub realized_variance_days: usize,
    /// Целевая годовая волатильность в процентах; в формуле используется её квадрат.
    #[serde(
        default = "default_target_volatility_percent",
        with = "rust_decimal::serde::str"
    )]
    pub target_annual_volatility_percent: Decimal,
    /// Максимальный gross портфеля; валидатор ограничивает значение сверху двумя.
    #[serde(default = "default_max_leverage", with = "rust_decimal::serde::str")]
    pub max_leverage: Decimal,
}

impl Default for VolatilityManagedConfig {
    fn default() -> Self {
        Self {
            realized_variance_days: default_realized_variance_days(),
            target_annual_volatility_percent: default_target_volatility_percent(),
            max_leverage: default_max_leverage(),
        }
    }
}

fn default_seasonality_years() -> usize {
    5
}

fn default_momentum_volatility_days() -> usize {
    126
}

fn default_momentum_target_volatility_percent() -> Decimal {
    Decimal::from(12)
}

fn default_one() -> Decimal {
    Decimal::ONE
}

fn default_ai_news_feeds() -> Vec<String> {
    [
        "https://www.interfax.ru/rss",
        "https://www.kommersant.ru/RSS/section-business.xml",
        "https://www.kommersant.ru/RSS/section-finance.xml",
        "https://www.vedomosti.ru/rss/rubric/finance",
        "https://www.vedomosti.ru/rss/rubric/business",
        "https://www.finam.ru/analysis/nslent/rsspoint/",
        "https://www.finam.ru/analysis/conews/rsspoint/",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

const fn default_ai_news_hours() -> u32 {
    24
}

const fn default_ai_tool_rounds() -> u32 {
    8
}

fn default_ai_max_weight_percent() -> Decimal {
    Decimal::from(25)
}

const fn default_ai_interval_minutes() -> u32 {
    60
}

const fn default_ai_news_burst_items() -> u32 {
    3
}

const fn default_ai_news_burst_minutes() -> u32 {
    30
}

fn default_ai_price_jump_percent() -> Decimal {
    Decimal::from(2)
}

fn default_ai_fast_jump_percent() -> Decimal {
    Decimal::ONE
}

const fn default_ai_cooldown_minutes() -> u32 {
    20
}

const fn default_ai_max_decisions() -> u32 {
    24
}

const fn default_ai_material_importance() -> u8 {
    4
}

fn default_ai_market_move_percent() -> Decimal {
    Decimal::TWO
}

fn default_ai_commodity_move_percent() -> Decimal {
    Decimal::from(3)
}

fn default_ai_currency_move_percent() -> Decimal {
    Decimal::new(15, 1)
}

// Поля, добавленные после первых запусков, не сериализуются со значением по
// умолчанию: сохранённая конфигурация стратегии (`strategy_instances.config`)
// остаётся прежней, и уже работающий портфель запускается без нового id.
#[allow(clippy::trivially_copy_pass_by_ref)] // serde передаёт поле по ссылке
fn is_default_material_importance(value: &u8) -> bool {
    *value == default_ai_material_importance()
}

fn is_default_market_move(value: &Decimal) -> bool {
    *value == default_ai_market_move_percent()
}

fn is_default_commodity_move(value: &Decimal) -> bool {
    *value == default_ai_commodity_move_percent()
}

fn is_default_currency_move(value: &Decimal) -> bool {
    *value == default_ai_currency_move_percent()
}

const fn default_true() -> bool {
    true
}

fn default_dividend_entry_days() -> u32 {
    30
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// Параметры value-ранга по текущим мультипликаторам T-Invest.
pub struct ValueCompositeConfig {
    /// Размер дешёвого (long) и дорогого (short) хвоста ранга, в процентах.
    #[serde(default = "default_quantile_percent")]
    pub quantile_percent: usize,
}

impl Default for ValueCompositeConfig {
    fn default() -> Self {
        Self {
            quantile_percent: default_quantile_percent(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// Параметры ранга качества: рентабельность и низкая долговая нагрузка.
pub struct QualityProfitabilityConfig {
    /// Размер качественного (long) и слабого (short) хвоста ранга, в процентах.
    #[serde(default = "default_quantile_percent")]
    pub quantile_percent: usize,
}

impl Default for QualityProfitabilityConfig {
    fn default() -> Self {
        Self {
            quantile_percent: default_quantile_percent(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// Параметры momentum 12–1 с масштабированием до целевой волатильности.
pub struct RiskManagedMomentumConfig {
    /// Дальняя граница окна доходности, в дневных свечах.
    #[serde(default = "default_formation_days")]
    pub formation_days: usize,
    /// Число последних дневных свечей, исключённых из окна.
    #[serde(default = "default_skip_days")]
    pub skip_days: usize,
    /// Размер long- и short-хвоста ранга, в процентах.
    #[serde(default = "default_quantile_percent")]
    pub quantile_percent: usize,
    /// Окно реализованной волатильности long/short портфеля, в дневных свечах.
    #[serde(default = "default_momentum_volatility_days")]
    pub volatility_days: usize,
    /// Целевая годовая волатильность портфеля в процентах.
    #[serde(
        default = "default_momentum_target_volatility_percent",
        with = "rust_decimal::serde::str"
    )]
    pub target_annual_volatility_percent: Decimal,
    /// Максимальный множитель экспозиции; валидатор ограничивает его диапазоном (0, 2].
    #[serde(default = "default_one", with = "rust_decimal::serde::str")]
    pub max_leverage: Decimal,
}

impl Default for RiskManagedMomentumConfig {
    fn default() -> Self {
        Self {
            formation_days: default_formation_days(),
            skip_days: default_skip_days(),
            quantile_percent: default_quantile_percent(),
            volatility_days: default_momentum_volatility_days(),
            target_annual_volatility_percent: default_momentum_target_volatility_percent(),
            max_leverage: default_one(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// Параметры сезонности доходностей одного календарного месяца.
pub struct ReturnSeasonalityConfig {
    /// Сколько прошлых лет того же месяца усредняется.
    #[serde(default = "default_seasonality_years")]
    pub years: usize,
    /// Размер long- и short-хвоста ранга, в процентах.
    #[serde(default = "default_quantile_percent")]
    pub quantile_percent: usize,
}

impl Default for ReturnSeasonalityConfig {
    fn default() -> Self {
        Self {
            years: default_seasonality_years(),
            quantile_percent: default_quantile_percent(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// Параметры премии месяца выплаты дивиденда.
pub struct DividendMonthConfig {
    /// За сколько календарных дней до последнего дня покупки открывается позиция.
    #[serde(default = "default_dividend_entry_days")]
    pub entry_days: u32,
}

impl Default for DividendMonthConfig {
    fn default() -> Self {
        Self {
            entry_days: default_dividend_entry_days(),
        }
    }
}

/// Стратегия на внешней ИИ-модели: модель читает свежие новости, сама запрашивает
/// свечи, индикаторы и мультипликаторы и выдаёт целевые веса инструментов блока.
///
/// Подключение к модели (провайдер, URL, модель, токен) задаётся не здесь, а в
/// файле профилей `AI_MODELS_FILE`; стратегия ссылается на профиль по имени.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AiModelConfig {
    /// Имя профиля из файла моделей; `None` — профиль `default` этого файла.
    #[serde(default)]
    pub model_profile: Option<String>,
    /// Собственная идея стратегии: что модели учитывать и как торговать.
    #[serde(default)]
    pub instructions: String,
    /// RSS/Atom-ленты новостей.
    #[serde(default = "default_ai_news_feeds")]
    pub news_feeds: Vec<String>,
    /// Насколько свежие новости (в часах) видит модель.
    #[serde(default = "default_ai_news_hours")]
    pub news_hours: u32,
    /// Сколько раз модель может вызвать инструменты до итогового решения.
    #[serde(default = "default_ai_tool_rounds")]
    pub max_tool_rounds: u32,
    /// Предел доли одного инструмента, %.
    #[serde(
        default = "default_ai_max_weight_percent",
        with = "rust_decimal::serde::str"
    )]
    pub max_weight_percent: Decimal,
    /// Разрешены ли короткие позиции; по умолчанию только long.
    #[serde(default)]
    pub allow_short: bool,
    /// Плановый пересмотр после открытия, минуты; 0 — только на открытии.
    #[serde(default = "default_ai_interval_minutes")]
    pub decision_interval_minutes: u32,
    /// Внеочередное решение, если о бумагах блока вышло столько новостей…
    #[serde(default = "default_ai_news_burst_items")]
    pub news_burst_items: u32,
    /// …за столько минут. `news_burst_items = 0` выключает этот триггер.
    #[serde(default = "default_ai_news_burst_minutes")]
    pub news_burst_minutes: u32,
    /// Внеочередное решение при изменении цены с прошлого решения, %; 0 — выключено.
    #[serde(
        default = "default_ai_price_jump_percent",
        with = "rust_decimal::serde::str"
    )]
    pub price_jump_percent: Decimal,
    /// …или за последние 10 минут, %; 0 — выключено.
    #[serde(
        default = "default_ai_fast_jump_percent",
        with = "rust_decimal::serde::str"
    )]
    pub fast_jump_percent: Decimal,
    /// Минимальный интервал между внеочередными решениями, минуты.
    #[serde(default = "default_ai_cooldown_minutes")]
    pub min_minutes_between_decisions: u32,
    /// Предел решений модели за торговый день (расход API).
    #[serde(default = "default_ai_max_decisions")]
    pub max_decisions_per_day: u32,
    /// Долгосрочная память: заметки модели и её прошлые решения в каждом разговоре.
    #[serde(default = "default_true")]
    pub memory: bool,
    /// Внеочередное решение при событии о бумагах блока с такой важностью
    /// разметки (1–5): отчётность, дивиденды, сделка, решение регулятора; 0 — выключено.
    #[serde(
        default = "default_ai_material_importance",
        skip_serializing_if = "is_default_material_importance"
    )]
    pub material_importance: u8,
    /// Внеочередное решение при движении индекса Мосбиржи с открытия, %; 0 — выключено.
    #[serde(
        default = "default_ai_market_move_percent",
        with = "rust_decimal::serde::str",
        skip_serializing_if = "is_default_market_move"
    )]
    pub market_move_percent: Decimal,
    /// …нефти Brent с открытия, %; 0 — выключено.
    #[serde(
        default = "default_ai_commodity_move_percent",
        with = "rust_decimal::serde::str",
        skip_serializing_if = "is_default_commodity_move"
    )]
    pub commodity_move_percent: Decimal,
    /// …юаня или доллара к рублю с открытия, %; 0 — выключено.
    #[serde(
        default = "default_ai_currency_move_percent",
        with = "rust_decimal::serde::str",
        skip_serializing_if = "is_default_currency_move"
    )]
    pub currency_move_percent: Decimal,
}

impl AiModelConfig {
    /// Смещения плановых пересмотров от открытия сессии, секунды (не позже 9,5 ч).
    #[must_use]
    pub fn decision_offsets_seconds(&self) -> Vec<i64> {
        let mut offsets = vec![0];
        if self.decision_interval_minutes > 0 {
            let step = i64::from(self.decision_interval_minutes) * 60;
            let mut offset = step;
            while offset <= 9 * 3600 + 30 * 60 {
                offsets.push(offset);
                offset += step;
            }
        }
        offsets
    }
}

impl Default for AiModelConfig {
    fn default() -> Self {
        Self {
            model_profile: None,
            instructions: String::new(),
            news_feeds: default_ai_news_feeds(),
            news_hours: default_ai_news_hours(),
            max_tool_rounds: default_ai_tool_rounds(),
            max_weight_percent: default_ai_max_weight_percent(),
            allow_short: false,
            decision_interval_minutes: default_ai_interval_minutes(),
            news_burst_items: default_ai_news_burst_items(),
            news_burst_minutes: default_ai_news_burst_minutes(),
            price_jump_percent: default_ai_price_jump_percent(),
            fast_jump_percent: default_ai_fast_jump_percent(),
            min_minutes_between_decisions: default_ai_cooldown_minutes(),
            max_decisions_per_day: default_ai_max_decisions(),
            memory: true,
            material_importance: default_ai_material_importance(),
            market_move_percent: default_ai_market_move_percent(),
            commodity_move_percent: default_ai_commodity_move_percent(),
            currency_move_percent: default_ai_currency_move_percent(),
        }
    }
}

#[derive(Clone, Debug)]
/// Типобезопасная спецификация одной из встроенных стратегий.
///
/// При сериализации используется TOML/JSON-поле `kind` в kebab-case. В enum хранится
/// ровно один набор параметров, поэтому несовместимые поля разных стратегий нельзя
/// случайно смешать после десериализации.
pub enum BuiltinStrategySpec {
    /// Тренд по SMA 50/200 с нейтральной полосой.
    MovingAverageTrend(MovingAverageTrendConfig),
    /// Пробой верхней или нижней границы недавнего диапазона.
    RangeBreakout(RangeBreakoutConfig),
    /// Momentum временного ряда с inverse-vol масштабированием.
    TimeSeriesMomentum(TimeSeriesMomentumConfig),
    /// Кросс-секционный momentum 12–2 месяца.
    CrossSectionalMomentum(CrossSectionalMomentumConfig),
    /// Momentum на промежуточном участке 12–7 месяцев.
    IntermediateMomentum(IntermediateMomentumConfig),
    /// Ранг близости к 52-недельному максимуму.
    FiftyTwoWeekHigh(FiftyTwoWeekHighConfig),
    /// Momentum остатков однофакторной рыночной регрессии.
    ResidualMomentum(ResidualMomentumConfig),
    /// Возврат нормированного спреда выбранной пары к среднему.
    PairsTrading(PairsTradingConfig),
    /// Long недавних проигравших и short недавних победителей.
    ShortTermReversal(ShortTermReversalConfig),
    /// Направление последнего получаса по движению первых 30 минут.
    MarketIntradayMomentum(MarketIntradayMomentumConfig),
    /// Ранг среднего возврата следующего слота в прошлых сессиях.
    IntradaySameClock(IntradaySameClockConfig),
    /// Long-only портфель нижнего квинтиля волатильности.
    LowVolatility(LowVolatilityConfig),
    /// Beta-нейтральная ставка long low-beta / short high-beta.
    BettingAgainstBeta(BettingAgainstBetaConfig),
    /// Long-only портфель с равными вкладами активов в риск.
    EqualRiskContribution(EqualRiskContributionConfig),
    /// Равновзвешенная корзина с экспозицией, обратной дисперсии.
    VolatilityManaged(VolatilityManagedConfig),
    /// Long дешёвых / short дорогих по P/B, P/E и EV/EBITDA.
    ValueComposite(ValueCompositeConfig),
    /// Long рентабельных и малозакредитованных / short слабых компаний.
    QualityProfitability(QualityProfitabilityConfig),
    /// Momentum 12–1 с масштабированием до целевой волатильности.
    RiskManagedMomentum(RiskManagedMomentumConfig),
    /// Ранг средней доходности того же календарного месяца прошлых лет.
    ReturnSeasonality(ReturnSeasonalityConfig),
    /// Long бумаг перед последним днём покупки под объявленный дивиденд.
    DividendMonth(DividendMonthConfig),
    /// Решение внешней ИИ-модели по новостям и индикаторам.
    AiModel(AiModelConfig),
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
// Вспомогательное wire-представление нужно для internally tagged serde enum:
// публичный enum выше остаётся удобным для Rust API, а каждая TOML-секция
// [[strategies]] имеет плоские поля kind и параметров конкретного алгоритма.
enum WireStrategySpec {
    MovingAverageTrend {
        #[serde(default = "default_fast_window")]
        fast_window: usize,
        #[serde(default = "default_slow_window")]
        slow_window: usize,
        #[serde(default = "default_band_percent", with = "rust_decimal::serde::str")]
        band_percent: Decimal,
    },
    RangeBreakout {
        #[serde(default = "default_range_lookback")]
        lookback_days: usize,
        #[serde(default = "default_band_percent", with = "rust_decimal::serde::str")]
        filter_percent: Decimal,
    },
    TimeSeriesMomentum {
        #[serde(default = "default_horizons")]
        horizons_days: Vec<usize>,
        #[serde(default = "default_volatility_window")]
        volatility_window_days: usize,
    },
    CrossSectionalMomentum {
        #[serde(default = "default_formation_days")]
        formation_days: usize,
        #[serde(default = "default_skip_days")]
        skip_days: usize,
        #[serde(default = "default_quantile_percent")]
        quantile_percent: usize,
    },
    IntermediateMomentum {
        #[serde(default = "default_formation_days")]
        start_days_ago: usize,
        #[serde(default = "default_intermediate_end_days")]
        end_days_ago: usize,
        #[serde(default = "default_quantile_percent")]
        quantile_percent: usize,
    },
    FiftyTwoWeekHigh {
        #[serde(default = "default_formation_days")]
        lookback_days: usize,
        #[serde(default = "default_quantile_percent")]
        quantile_percent: usize,
    },
    ResidualMomentum {
        benchmark_uid: InstrumentId,
        #[serde(default = "default_formation_days")]
        formation_days: usize,
        #[serde(default = "default_skip_days")]
        skip_days: usize,
        #[serde(default = "default_quantile_percent")]
        quantile_percent: usize,
    },
    PairsTrading {
        #[serde(default = "default_formation_days")]
        formation_days: usize,
        #[serde(default = "default_pair_trading_days")]
        trading_days: usize,
        #[serde(default = "default_entry_z_score", with = "rust_decimal::serde::str")]
        entry_z_score: Decimal,
    },
    ShortTermReversal {
        benchmark_uid: InstrumentId,
        #[serde(default = "default_reversal_days")]
        lookback_days: usize,
        #[serde(default = "default_quantile_percent")]
        quantile_percent: usize,
    },
    MarketIntradayMomentum {
        #[serde(default = "default_thirty_minutes")]
        opening_window_minutes: u32,
        #[serde(default = "default_thirty_minutes")]
        final_window_minutes: u32,
    },
    IntradaySameClock {
        #[serde(default = "default_same_clock_sessions")]
        lookback_sessions: usize,
        #[serde(default = "default_quantile_percent")]
        quantile_percent: usize,
        #[serde(default = "default_thirty_minutes")]
        interval_minutes: u32,
    },
    LowVolatility {
        #[serde(default = "default_volatility_months")]
        lookback_months: usize,
        #[serde(default = "default_quantile_percent")]
        quantile_percent: usize,
    },
    BettingAgainstBeta {
        benchmark_uid: InstrumentId,
        #[serde(default = "default_formation_days")]
        lookback_days: usize,
        #[serde(default = "default_beta_shrinkage", with = "rust_decimal::serde::str")]
        shrinkage: Decimal,
    },
    EqualRiskContribution {
        #[serde(default = "default_formation_days")]
        lookback_days: usize,
        #[serde(
            default = "default_covariance_shrinkage_percent",
            with = "rust_decimal::serde::str"
        )]
        diagonal_shrinkage_percent: Decimal,
        #[serde(default = "default_erc_iterations")]
        max_iterations: usize,
        #[serde(default = "default_erc_tolerance", with = "rust_decimal::serde::str")]
        tolerance: Decimal,
    },
    VolatilityManaged {
        #[serde(default = "default_realized_variance_days")]
        realized_variance_days: usize,
        #[serde(
            default = "default_target_volatility_percent",
            with = "rust_decimal::serde::str"
        )]
        target_annual_volatility_percent: Decimal,
        #[serde(default = "default_max_leverage", with = "rust_decimal::serde::str")]
        max_leverage: Decimal,
    },
    ValueComposite {
        #[serde(default = "default_quantile_percent")]
        quantile_percent: usize,
    },
    QualityProfitability {
        #[serde(default = "default_quantile_percent")]
        quantile_percent: usize,
    },
    RiskManagedMomentum {
        #[serde(default = "default_formation_days")]
        formation_days: usize,
        #[serde(default = "default_skip_days")]
        skip_days: usize,
        #[serde(default = "default_quantile_percent")]
        quantile_percent: usize,
        #[serde(default = "default_momentum_volatility_days")]
        volatility_days: usize,
        #[serde(
            default = "default_momentum_target_volatility_percent",
            with = "rust_decimal::serde::str"
        )]
        target_annual_volatility_percent: Decimal,
        #[serde(default = "default_one", with = "rust_decimal::serde::str")]
        max_leverage: Decimal,
    },
    ReturnSeasonality {
        #[serde(default = "default_seasonality_years")]
        years: usize,
        #[serde(default = "default_quantile_percent")]
        quantile_percent: usize,
    },
    DividendMonth {
        #[serde(default = "default_dividend_entry_days")]
        entry_days: u32,
    },
    AiModel(AiModelConfig),
}

impl From<WireStrategySpec> for BuiltinStrategySpec {
    fn from(value: WireStrategySpec) -> Self {
        match value {
            WireStrategySpec::MovingAverageTrend {
                fast_window,
                slow_window,
                band_percent,
            } => Self::MovingAverageTrend(MovingAverageTrendConfig {
                fast_window,
                slow_window,
                band_percent,
            }),
            WireStrategySpec::RangeBreakout {
                lookback_days,
                filter_percent,
            } => Self::RangeBreakout(RangeBreakoutConfig {
                lookback_days,
                filter_percent,
            }),
            WireStrategySpec::TimeSeriesMomentum {
                horizons_days,
                volatility_window_days,
            } => Self::TimeSeriesMomentum(TimeSeriesMomentumConfig {
                horizons_days,
                volatility_window_days,
            }),
            WireStrategySpec::CrossSectionalMomentum {
                formation_days,
                skip_days,
                quantile_percent,
            } => Self::CrossSectionalMomentum(CrossSectionalMomentumConfig {
                formation_days,
                skip_days,
                quantile_percent,
            }),
            WireStrategySpec::IntermediateMomentum {
                start_days_ago,
                end_days_ago,
                quantile_percent,
            } => Self::IntermediateMomentum(IntermediateMomentumConfig {
                start_days_ago,
                end_days_ago,
                quantile_percent,
            }),
            WireStrategySpec::FiftyTwoWeekHigh {
                lookback_days,
                quantile_percent,
            } => Self::FiftyTwoWeekHigh(FiftyTwoWeekHighConfig {
                lookback_days,
                quantile_percent,
            }),
            WireStrategySpec::ResidualMomentum {
                benchmark_uid,
                formation_days,
                skip_days,
                quantile_percent,
            } => Self::ResidualMomentum(ResidualMomentumConfig {
                benchmark_uid,
                formation_days,
                skip_days,
                quantile_percent,
            }),
            WireStrategySpec::PairsTrading {
                formation_days,
                trading_days,
                entry_z_score,
            } => Self::PairsTrading(PairsTradingConfig {
                formation_days,
                trading_days,
                entry_z_score,
            }),
            WireStrategySpec::ShortTermReversal {
                benchmark_uid,
                lookback_days,
                quantile_percent,
            } => Self::ShortTermReversal(ShortTermReversalConfig {
                benchmark_uid,
                lookback_days,
                quantile_percent,
            }),
            WireStrategySpec::MarketIntradayMomentum {
                opening_window_minutes,
                final_window_minutes,
            } => Self::MarketIntradayMomentum(MarketIntradayMomentumConfig {
                opening_window_minutes,
                final_window_minutes,
            }),
            WireStrategySpec::IntradaySameClock {
                lookback_sessions,
                quantile_percent,
                interval_minutes,
            } => Self::IntradaySameClock(IntradaySameClockConfig {
                lookback_sessions,
                quantile_percent,
                interval_minutes,
            }),
            WireStrategySpec::LowVolatility {
                lookback_months,
                quantile_percent,
            } => Self::LowVolatility(LowVolatilityConfig {
                lookback_months,
                quantile_percent,
            }),
            WireStrategySpec::BettingAgainstBeta {
                benchmark_uid,
                lookback_days,
                shrinkage,
            } => Self::BettingAgainstBeta(BettingAgainstBetaConfig {
                benchmark_uid,
                lookback_days,
                shrinkage,
            }),
            WireStrategySpec::EqualRiskContribution {
                lookback_days,
                diagonal_shrinkage_percent,
                max_iterations,
                tolerance,
            } => Self::EqualRiskContribution(EqualRiskContributionConfig {
                lookback_days,
                diagonal_shrinkage_percent,
                max_iterations,
                tolerance,
            }),
            WireStrategySpec::VolatilityManaged {
                realized_variance_days,
                target_annual_volatility_percent,
                max_leverage,
            } => Self::VolatilityManaged(VolatilityManagedConfig {
                realized_variance_days,
                target_annual_volatility_percent,
                max_leverage,
            }),
            WireStrategySpec::ValueComposite { quantile_percent } => {
                Self::ValueComposite(ValueCompositeConfig { quantile_percent })
            }
            WireStrategySpec::QualityProfitability { quantile_percent } => {
                Self::QualityProfitability(QualityProfitabilityConfig { quantile_percent })
            }
            WireStrategySpec::RiskManagedMomentum {
                formation_days,
                skip_days,
                quantile_percent,
                volatility_days,
                target_annual_volatility_percent,
                max_leverage,
            } => Self::RiskManagedMomentum(RiskManagedMomentumConfig {
                formation_days,
                skip_days,
                quantile_percent,
                volatility_days,
                target_annual_volatility_percent,
                max_leverage,
            }),
            WireStrategySpec::ReturnSeasonality {
                years,
                quantile_percent,
            } => Self::ReturnSeasonality(ReturnSeasonalityConfig {
                years,
                quantile_percent,
            }),
            WireStrategySpec::DividendMonth { entry_days } => {
                Self::DividendMonth(DividendMonthConfig { entry_days })
            }
            WireStrategySpec::AiModel(config) => Self::AiModel(config),
        }
    }
}

impl From<&BuiltinStrategySpec> for WireStrategySpec {
    fn from(value: &BuiltinStrategySpec) -> Self {
        match value {
            BuiltinStrategySpec::MovingAverageTrend(config) => Self::MovingAverageTrend {
                fast_window: config.fast_window,
                slow_window: config.slow_window,
                band_percent: config.band_percent,
            },
            BuiltinStrategySpec::RangeBreakout(config) => Self::RangeBreakout {
                lookback_days: config.lookback_days,
                filter_percent: config.filter_percent,
            },
            BuiltinStrategySpec::TimeSeriesMomentum(config) => Self::TimeSeriesMomentum {
                horizons_days: config.horizons_days.clone(),
                volatility_window_days: config.volatility_window_days,
            },
            BuiltinStrategySpec::CrossSectionalMomentum(config) => Self::CrossSectionalMomentum {
                formation_days: config.formation_days,
                skip_days: config.skip_days,
                quantile_percent: config.quantile_percent,
            },
            BuiltinStrategySpec::IntermediateMomentum(config) => Self::IntermediateMomentum {
                start_days_ago: config.start_days_ago,
                end_days_ago: config.end_days_ago,
                quantile_percent: config.quantile_percent,
            },
            BuiltinStrategySpec::FiftyTwoWeekHigh(config) => Self::FiftyTwoWeekHigh {
                lookback_days: config.lookback_days,
                quantile_percent: config.quantile_percent,
            },
            BuiltinStrategySpec::ResidualMomentum(config) => Self::ResidualMomentum {
                benchmark_uid: config.benchmark_uid.clone(),
                formation_days: config.formation_days,
                skip_days: config.skip_days,
                quantile_percent: config.quantile_percent,
            },
            BuiltinStrategySpec::PairsTrading(config) => Self::PairsTrading {
                formation_days: config.formation_days,
                trading_days: config.trading_days,
                entry_z_score: config.entry_z_score,
            },
            BuiltinStrategySpec::ShortTermReversal(config) => Self::ShortTermReversal {
                benchmark_uid: config.benchmark_uid.clone(),
                lookback_days: config.lookback_days,
                quantile_percent: config.quantile_percent,
            },
            BuiltinStrategySpec::MarketIntradayMomentum(config) => Self::MarketIntradayMomentum {
                opening_window_minutes: config.opening_window_minutes,
                final_window_minutes: config.final_window_minutes,
            },
            BuiltinStrategySpec::IntradaySameClock(config) => Self::IntradaySameClock {
                lookback_sessions: config.lookback_sessions,
                quantile_percent: config.quantile_percent,
                interval_minutes: config.interval_minutes,
            },
            BuiltinStrategySpec::LowVolatility(config) => Self::LowVolatility {
                lookback_months: config.lookback_months,
                quantile_percent: config.quantile_percent,
            },
            BuiltinStrategySpec::BettingAgainstBeta(config) => Self::BettingAgainstBeta {
                benchmark_uid: config.benchmark_uid.clone(),
                lookback_days: config.lookback_days,
                shrinkage: config.shrinkage,
            },
            BuiltinStrategySpec::EqualRiskContribution(config) => Self::EqualRiskContribution {
                lookback_days: config.lookback_days,
                diagonal_shrinkage_percent: config.diagonal_shrinkage_percent,
                max_iterations: config.max_iterations,
                tolerance: config.tolerance,
            },
            BuiltinStrategySpec::VolatilityManaged(config) => Self::VolatilityManaged {
                realized_variance_days: config.realized_variance_days,
                target_annual_volatility_percent: config.target_annual_volatility_percent,
                max_leverage: config.max_leverage,
            },
            BuiltinStrategySpec::ValueComposite(config) => Self::ValueComposite {
                quantile_percent: config.quantile_percent,
            },
            BuiltinStrategySpec::QualityProfitability(config) => Self::QualityProfitability {
                quantile_percent: config.quantile_percent,
            },
            BuiltinStrategySpec::RiskManagedMomentum(config) => Self::RiskManagedMomentum {
                formation_days: config.formation_days,
                skip_days: config.skip_days,
                quantile_percent: config.quantile_percent,
                volatility_days: config.volatility_days,
                target_annual_volatility_percent: config.target_annual_volatility_percent,
                max_leverage: config.max_leverage,
            },
            BuiltinStrategySpec::ReturnSeasonality(config) => Self::ReturnSeasonality {
                years: config.years,
                quantile_percent: config.quantile_percent,
            },
            BuiltinStrategySpec::DividendMonth(config) => Self::DividendMonth {
                entry_days: config.entry_days,
            },
            BuiltinStrategySpec::AiModel(config) => Self::AiModel(config.clone()),
        }
    }
}

impl<'de> Deserialize<'de> for BuiltinStrategySpec {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        WireStrategySpec::deserialize(deserializer).map(Self::from)
    }
}

impl Serialize for BuiltinStrategySpec {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        WireStrategySpec::from(self).serialize(serializer)
    }
}

impl BuiltinStrategySpec {
    /// Возвращает стабильное значение поля `kind`, используемое в TOML, CLI и storage.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::MovingAverageTrend(_) => "moving-average-trend",
            Self::RangeBreakout(_) => "range-breakout",
            Self::TimeSeriesMomentum(_) => "time-series-momentum",
            Self::CrossSectionalMomentum(_) => "cross-sectional-momentum",
            Self::IntermediateMomentum(_) => "intermediate-momentum",
            Self::FiftyTwoWeekHigh(_) => "fifty-two-week-high",
            Self::ResidualMomentum(_) => "residual-momentum",
            Self::PairsTrading(_) => "pairs-trading",
            Self::ShortTermReversal(_) => "short-term-reversal",
            Self::MarketIntradayMomentum(_) => "market-intraday-momentum",
            Self::IntradaySameClock(_) => "intraday-same-clock",
            Self::LowVolatility(_) => "low-volatility",
            Self::BettingAgainstBeta(_) => "betting-against-beta",
            Self::EqualRiskContribution(_) => "equal-risk-contribution",
            Self::VolatilityManaged(_) => "volatility-managed",
            Self::ValueComposite(_) => "value-composite",
            Self::QualityProfitability(_) => "quality-profitability",
            Self::RiskManagedMomentum(_) => "risk-managed-momentum",
            Self::ReturnSeasonality(_) => "return-seasonality",
            Self::DividendMonth(_) => "dividend-month",
            Self::AiModel(_) => "ai-model",
        }
    }

    /// Возвращает полный перечень поддерживаемых значений `kind` в порядке CLI.
    #[must_use]
    pub const fn all_kinds() -> &'static [&'static str] {
        &[
            "moving-average-trend",
            "range-breakout",
            "time-series-momentum",
            "cross-sectional-momentum",
            "intermediate-momentum",
            "fifty-two-week-high",
            "residual-momentum",
            "pairs-trading",
            "short-term-reversal",
            "market-intraday-momentum",
            "intraday-same-clock",
            "low-volatility",
            "betting-against-beta",
            "equal-risk-contribution",
            "volatility-managed",
            "value-composite",
            "quality-profitability",
            "risk-managed-momentum",
            "return-seasonality",
            "dividend-month",
            "ai-model",
        ]
    }
}
