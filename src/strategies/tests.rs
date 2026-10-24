// Strategies added from the evidence review: value, quality, risk-managed
// momentum, return seasonality and the dividend-month premium.

use chrono::{NaiveDate, TimeZone};

use super::math::MAX_INSTRUMENT_WEIGHT;
use super::*;
use crate::{
    domain::Price,
    engine::{DividendEvent, InstrumentFundamentals, ReferenceData, ScheduleEvent},
};

fn uid(name: &str) -> InstrumentId {
    InstrumentId::new(name).unwrap()
}

fn index(value: usize) -> i64 {
    i64::try_from(value).unwrap()
}

fn ten() -> Vec<InstrumentId> {
    (0..10).map(|i| uid(&format!("s{i}"))).collect()
}

fn builtin(spec: BuiltinStrategySpec) -> BuiltinStrategy {
    BuiltinStrategy::new(
        StrategyId::new("new-kinds").unwrap(),
        AccountId::new("account").unwrap(),
        ten(),
        spec,
    )
    .unwrap()
}

fn candle(
    id: &InstrumentId,
    interval: CandleInterval,
    time: DateTime<Utc>,
    close: Decimal,
) -> Candle {
    Candle {
        instrument_id: id.clone(),
        interval,
        open: Price(close),
        high: Price(close),
        low: Price(close),
        close: Price(close),
        volume: 1,
        time,
        complete: true,
    }
}

fn fundamentals(id: &InstrumentId, pb: Option<i64>, roe: Option<i64>) -> InstrumentFundamentals {
    InstrumentFundamentals {
        instrument_id: id.clone(),
        price_to_book: pb.map(Decimal::from),
        price_to_earnings: None,
        ev_to_ebitda: None,
        return_on_equity: roe.map(Decimal::from),
        return_on_assets: None,
        net_margin: None,
        debt_to_equity: None,
    }
}

fn weight_of(weights: &[(InstrumentId, Decimal)], name: &str) -> Decimal {
    weights.iter().find(|(id, _)| id == &uid(name)).unwrap().1
}

#[test]
fn value_ranks_cheap_long_and_expensive_short_and_needs_coverage() {
    let mut strategy = builtin(BuiltinStrategySpec::ValueComposite(
        ValueCompositeConfig::default(),
    ));
    assert!(
        strategy
            .value_composite_weights(&ValueCompositeConfig::default())
            .is_none(),
        "no snapshot yet"
    );
    // P/B 1..10: s0 cheapest, s9 most expensive.
    strategy.state.fundamentals = ten()
        .iter()
        .enumerate()
        .map(|(i, id)| fundamentals(id, Some(index(i) + 1), None))
        .collect();
    strategy.state.fundamentals_observed_at = Some(Utc::now());
    let weights = strategy
        .value_composite_weights(&ValueCompositeConfig::default())
        .unwrap();
    assert!(weight_of(&weights, "s0") > Decimal::ZERO && weight_of(&weights, "s1") > Decimal::ZERO);
    assert!(weight_of(&weights, "s9") < Decimal::ZERO && weight_of(&weights, "s8") < Decimal::ZERO);
    assert_eq!(weight_of(&weights, "s5"), Decimal::ZERO);
    // Three of ten without any multiple: below the 80% coverage threshold.
    for row in strategy.state.fundamentals.iter_mut().take(3) {
        row.price_to_book = None;
    }
    assert!(
        strategy
            .value_composite_weights(&ValueCompositeConfig::default())
            .is_none()
    );
}

#[test]
fn quality_prefers_profitability_and_low_leverage() {
    let mut strategy = builtin(BuiltinStrategySpec::QualityProfitability(
        QualityProfitabilityConfig::default(),
    ));
    strategy.state.fundamentals = ten()
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let mut row = fundamentals(id, None, Some(index(i) * 3 - 5));
            row.debt_to_equity = Some(Decimal::from(100 - index(i) * 10));
            row
        })
        .collect();
    strategy.state.fundamentals_observed_at = Some(Utc::now());
    let weights = strategy
        .quality_profitability_weights(&QualityProfitabilityConfig::default())
        .unwrap();
    assert!(
        weight_of(&weights, "s9") > Decimal::ZERO,
        "highest ROE, lowest debt"
    );
    assert!(
        weight_of(&weights, "s0") < Decimal::ZERO,
        "negative ROE, highest debt"
    );
}

#[test]
fn composite_scores_average_available_percentiles_with_ties() {
    let rows = vec![
        (uid("a"), vec![Some(Decimal::ONE), Some(Decimal::from(3))]),
        (uid("b"), vec![Some(Decimal::ONE), None]),
        (
            uid("c"),
            vec![Some(Decimal::from(2)), Some(Decimal::from(1))],
        ),
        (uid("d"), vec![None, None]),
        (
            uid("e"),
            vec![Some(Decimal::from(4)), Some(Decimal::from(2))],
        ),
    ];
    let scores = composite_scores(&rows, 5).unwrap();
    let score = |name: &str| scores.iter().find(|(id, _)| id == &uid(name)).unwrap().1;
    // a and b tie on metric 1 (ranks 0 and 1 → 0.5 / 3).
    assert_eq!(score("b"), Decimal::new(5, 1) / Decimal::from(3));
    assert_eq!(
        score("e"),
        (Decimal::ONE + Decimal::new(5, 1)) / Decimal::from(2)
    );
    assert!(!scores.iter().any(|(id, _)| id == &uid("d")));
    assert!(composite_scores(&rows, 6).is_none(), "4 of 6 is below 80%");
}

#[test]
fn risk_managed_momentum_scales_to_target_volatility_with_cap() {
    let config = RiskManagedMomentumConfig {
        formation_days: 30,
        skip_days: 5,
        volatility_days: 20,
        ..RiskManagedMomentumConfig::default()
    };
    let mut strategy = builtin(BuiltinStrategySpec::RiskManagedMomentum(config.clone()));
    let start = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    for (i, id) in ten().iter().enumerate() {
        for day in 0..=40_i64 {
            // Distinct trends with an alternating wiggle so the long/short book has variance.
            let wiggle = if day % 2 == 0 {
                Decimal::ONE
            } else {
                -Decimal::ONE
            };
            let close = Decimal::from(100)
                + Decimal::from(day) * Decimal::from(index(i)) / Decimal::from(10)
                + wiggle * Decimal::from(index(i) % 3);
            strategy.state.insert_candle(
                candle(
                    id,
                    CandleInterval::OneDay,
                    start + chrono::Duration::days(day),
                    close,
                ),
                100,
            );
        }
    }
    let gross =
        |weights: &[(InstrumentId, Decimal)]| weights.iter().map(|(_, w)| w.abs()).sum::<Decimal>();
    let scaled = strategy
        .risk_managed_momentum_weights(&config)
        .unwrap()
        .unwrap();
    assert!(gross(&scaled) > Decimal::ZERO && gross(&scaled) <= Decimal::ONE + Decimal::new(1, 9));
    assert!(weight_of(&scaled, "s9") > Decimal::ZERO && weight_of(&scaled, "s0") < Decimal::ZERO);
    // A tiny target leaves the cap and scales weights proportionally.
    let low = RiskManagedMomentumConfig {
        target_annual_volatility_percent: Decimal::new(1, 2),
        ..config.clone()
    };
    let lower = RiskManagedMomentumConfig {
        target_annual_volatility_percent: Decimal::new(2, 2),
        ..config
    };
    let a = gross(
        &strategy
            .risk_managed_momentum_weights(&low)
            .unwrap()
            .unwrap(),
    );
    let b = gross(
        &strategy
            .risk_managed_momentum_weights(&lower)
            .unwrap()
            .unwrap(),
    );
    assert!(a > Decimal::ZERO && (b / a - Decimal::from(2)).abs() < Decimal::new(1, 6));
}

#[test]
fn seasonality_ranks_same_calendar_month_returns_of_past_years() {
    let config = ReturnSeasonalityConfig {
        years: 3,
        ..ReturnSeasonalityConfig::default()
    };
    let mut strategy = builtin(BuiltinStrategySpec::ReturnSeasonality(config.clone()));
    for (i, id) in ten().iter().enumerate() {
        let mut close = Decimal::from(100);
        for year in 2022..=2026 {
            for month in 1..=12_u32 {
                // October return grows with the index; other months are flat.
                if month == 10 {
                    close *= Decimal::ONE + Decimal::from(index(i) - 5) / Decimal::from(100);
                }
                let time = Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0).unwrap();
                strategy
                    .state
                    .insert_candle(candle(id, CandleInterval::OneMonth, time, close), 200);
            }
        }
    }
    let october = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
    let weights = strategy
        .return_seasonality_weights(&config, october)
        .unwrap()
        .unwrap();
    assert!(weight_of(&weights, "s9") > Decimal::ZERO && weight_of(&weights, "s0") < Decimal::ZERO);
    // Not enough years of history for a 20-year average.
    let long = ReturnSeasonalityConfig {
        years: 20,
        ..config
    };
    assert!(
        strategy
            .return_seasonality_weights(&long, october)
            .unwrap()
            .is_none()
    );
}

#[test]
fn dividend_month_holds_until_the_last_buy_day() {
    let config = DividendMonthConfig::default();
    let mut strategy = builtin(BuiltinStrategySpec::DividendMonth(config.clone()));
    let day = |d: u32| NaiveDate::from_ymd_opt(2026, 11, d).unwrap();
    assert!(
        strategy.dividend_month_weights(&config, day(1)).is_none(),
        "no calendar yet"
    );
    strategy.state.dividends = vec![DividendEvent {
        instrument_id: uid("s3"),
        last_buy_date: day(20),
        dividend_net: None,
    }];
    strategy.state.dividends_observed_at = Some(Utc::now());
    let inside = strategy.dividend_month_weights(&config, day(1)).unwrap();
    assert_eq!(weight_of(&inside, "s3"), MAX_INSTRUMENT_WEIGHT);
    assert_eq!(
        weight_of(
            &strategy.dividend_month_weights(&config, day(20)).unwrap(),
            "s3"
        ),
        Decimal::ZERO,
        "exit before the ex-dividend gap"
    );
    let early = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
    assert!(
        strategy
            .dividend_month_weights(&config, early)
            .unwrap()
            .iter()
            .all(|(_, w)| w.is_zero())
    );
}

#[test]
fn ai_model_config_of_running_portfolios_keeps_its_stored_form() {
    // strategy_instances.config is compared as JSON: options added later must
    // not appear with their default values, or a running portfolio would be
    // refused as a different immutable configuration.
    let value = serde_json::to_value(AiModelConfig::default()).unwrap();
    for key in [
        "material_importance",
        "market_move_percent",
        "commodity_move_percent",
        "currency_move_percent",
    ] {
        assert!(
            value.get(key).is_none(),
            "{key} must not be serialized by default"
        );
    }
    let custom = AiModelConfig {
        material_importance: 5,
        market_move_percent: Decimal::from(3),
        ..AiModelConfig::default()
    };
    let value = serde_json::to_value(custom).unwrap();
    assert_eq!(
        (
            value["material_importance"].as_u64(),
            value["market_move_percent"].as_str()
        ),
        (Some(5), Some("3"))
    );
    let back: AiModelConfig = serde_json::from_value(value).unwrap();
    assert_eq!(back.material_importance, 5);
}

#[tokio::test]
async fn an_ai_component_takes_only_advice_of_its_own_profile() {
    use crate::engine::ModelAdvice;
    let open = Utc.with_ymd_and_hms(2026, 10, 26, 7, 0, 0).unwrap();
    let advice = |model: &str| StrategyEvent {
        id: format!("advice:{model}"),
        phase: EventPhase::Warmup,
        occurred_at: open,
        payload: StrategyEventKind::Advice(ModelAdvice {
            observed_at: open,
            session_date: open.date_naive(),
            universe: ten(),
            model: model.to_owned(),
            weights: Vec::new(),
            rationale: String::new(),
        }),
    };
    let spec = |profile: &str| -> BuiltinStrategySpec {
        toml::from_str::<WeightedStrategySpec>(&format!(
            "signal_weight = \"1\"\nkind = \"ai-model\"\n{profile}"
        ))
        .unwrap()
        .strategy
    };
    let mut deepseek = builtin(spec("model_profile = \"deepseek\""));
    deepseek
        .on_event(&advice("qwen122:Qwen3.5-122B-A10B"))
        .await
        .unwrap();
    assert!(
        deepseek.state.ai_advice.is_none(),
        "another model's decision is not taken"
    );
    deepseek
        .on_event(&advice("deepseek:DeepSeek-V4-Flash"))
        .await
        .unwrap();
    assert_eq!(
        deepseek.state.ai_advice.as_ref().unwrap().model,
        "deepseek:DeepSeek-V4-Flash"
    );
    // Без своего профиля (единственная ИИ-компонента) — как раньше, любое решение.
    let mut single = builtin(spec(""));
    single
        .on_event(&advice("claude:claude-opus-5-5"))
        .await
        .unwrap();
    assert!(single.state.ai_advice.is_some());
}

#[tokio::test]
async fn ai_model_trades_only_fresh_advice_within_its_limits() {
    use crate::engine::{AdviceWeight, ModelAdvice};
    let spec: WeightedStrategySpec = toml::from_str(
        "signal_weight = \"1\"\nkind = \"ai-model\"\nmodel_profile = \"claude\"\nmax_weight_percent = \"30\"\ninstructions = \"Покупай на новостях\"",
    )
    .unwrap();
    let BuiltinStrategySpec::AiModel(config) = spec.strategy.clone() else {
        panic!("ai-model expected");
    };
    assert_eq!(
        (
            config.model_profile.as_deref(),
            config.news_hours,
            config.allow_short
        ),
        (Some("claude"), 24, false)
    );
    assert!(
        toml::from_str::<WeightedStrategySpec>(
            "signal_weight = \"1\"\nkind = \"ai-model\"\ntoken = \"x\""
        )
        .is_err(),
        "tokens never live in a strategy"
    );
    let mut strategy = builtin(spec.strategy);
    let open = Utc.with_ymd_and_hms(2026, 10, 20, 6, 10, 0).unwrap();
    let session = open.date_naive();
    let advice = |universe: Vec<InstrumentId>, date: chrono::NaiveDate| StrategyEvent {
        id: format!("advice:{date}:{}", universe.len()),
        phase: EventPhase::Warmup,
        occurred_at: open,
        payload: StrategyEventKind::Advice(ModelAdvice {
            observed_at: open,
            session_date: date,
            universe,
            model: "claude:claude-opus-5-5".to_owned(),
            weights: [
                ("s0", "0.6"),
                ("s1", "-0.2"),
                ("s2", "0.3"),
                ("s3", "0.3"),
                ("outside", "0.5"),
            ]
            .into_iter()
            .map(|(id, weight)| AdviceWeight {
                instrument_id: uid(id),
                weight: weight.parse().unwrap(),
            })
            .collect(),
            rationale: "test".to_owned(),
        }),
    };
    let open_event = StrategyEvent {
        id: "schedule:open".to_owned(),
        phase: EventPhase::Live,
        occurred_at: open,
        payload: StrategyEventKind::Schedule(ScheduleEvent {
            anchor: ScheduleAnchor::SessionOpen,
            session_date: session,
            scheduled_at: open,
        }),
    };
    // Advice for another block or another session never trades.
    strategy
        .on_event(&advice(ten()[..3].to_vec(), session))
        .await
        .unwrap();
    assert!(strategy.state.ai_advice.is_none());
    strategy
        .on_event(&advice(ten(), session.pred_opt().unwrap()))
        .await
        .unwrap();
    for i in 0..10 {
        strategy
            .on_event(&StrategyEvent {
                id: format!("c{i}"),
                phase: EventPhase::Warmup,
                occurred_at: open,
                payload: StrategyEventKind::Candle(candle(
                    &uid(&format!("s{i}")),
                    CandleInterval::OneDay,
                    open - Duration::days(1),
                    Decimal::from(100),
                )),
            })
            .await
            .unwrap();
    }
    assert!(
        strategy.on_event(&open_event).await.unwrap().is_none(),
        "stale advice"
    );
    strategy.on_event(&advice(ten(), session)).await.unwrap();
    let target = strategy.on_event(&open_event).await.unwrap().unwrap();
    let weights: Vec<(InstrumentId, Decimal)> = target
        .weights
        .iter()
        .map(|w| (w.instrument_id.clone(), w.weight))
        .collect();
    // 0.6 → cap 0.3, short → 0, outside → dropped; gross 0.9 stays below 100%.
    assert_eq!(weight_of(&weights, "s0"), "0.3".parse::<Decimal>().unwrap());
    assert_eq!(weight_of(&weights, "s1"), Decimal::ZERO);
    assert_eq!(weights.len(), 10);
    assert_eq!(
        weights.iter().map(|(_, w)| w.abs()).sum::<Decimal>(),
        "0.9".parse::<Decimal>().unwrap()
    );
    // An hourly review an hour later ignores the advice of the open.
    let hour = StrategyEvent {
        id: "schedule:hour".to_owned(),
        phase: EventPhase::Live,
        occurred_at: open + Duration::hours(1),
        payload: StrategyEventKind::Schedule(ScheduleEvent {
            anchor: ScheduleAnchor::SessionOpen,
            session_date: session,
            scheduled_at: open + Duration::hours(1),
        }),
    };
    assert!(
        strategy.on_event(&hour).await.unwrap().is_none(),
        "stale hourly advice"
    );
    // An out-of-schedule (live) decision trades at once.
    let mut live = advice(ten(), session);
    live.phase = EventPhase::Live;
    live.id = "advice:news_burst".to_owned();
    let target = strategy
        .on_event(&live)
        .await
        .unwrap()
        .expect("live advice trades");
    assert_eq!(target.event_id, "advice:news_burst");
}

#[tokio::test]
async fn late_session_open_does_not_enable_intraday_signals() {
    let open = Utc.with_ymd_and_hms(2026, 10, 19, 6, 10, 0).unwrap();
    let event = |occurred_at: DateTime<Utc>| StrategyEvent {
        id: format!("schedule:open:{occurred_at}"),
        phase: EventPhase::Live,
        occurred_at,
        payload: StrategyEventKind::Schedule(ScheduleEvent {
            anchor: ScheduleAnchor::SessionOpen,
            session_date: open.date_naive(),
            scheduled_at: open,
        }),
    };
    let mut strategy = builtin(BuiltinStrategySpec::IntradaySameClock(
        IntradaySameClockConfig::default(),
    ));
    // A retry after the computer slept through the open keeps intraday off.
    strategy
        .on_event(&event(open + Duration::minutes(30)))
        .await
        .unwrap();
    assert_eq!(strategy.state.active_intraday_session, None);
    strategy
        .on_event(&event(open + Duration::seconds(5)))
        .await
        .unwrap();
    assert_eq!(
        strategy.state.active_intraday_session,
        Some(open.date_naive())
    );
}

#[tokio::test]
async fn reference_events_update_only_requested_data_and_never_trade() {
    let reference = ReferenceData {
        observed_at: Utc::now(),
        fundamentals: ten()
            .iter()
            .map(|id| fundamentals(id, Some(2), Some(10)))
            .chain([fundamentals(&uid("outside"), Some(1), None)])
            .collect(),
        dividends: vec![DividendEvent {
            instrument_id: uid("s1"),
            last_buy_date: NaiveDate::from_ymd_opt(2026, 12, 1).unwrap(),
            dividend_net: None,
        }],
    };
    let event = StrategyEvent {
        id: "reference:test".to_owned(),
        phase: EventPhase::Warmup,
        occurred_at: Utc::now(),
        payload: StrategyEventKind::Reference(reference),
    };
    let mut value = builtin(BuiltinStrategySpec::ValueComposite(
        ValueCompositeConfig::default(),
    ));
    assert!(value.on_event(&event).await.unwrap().is_none());
    assert_eq!(
        value.state.fundamentals.len(),
        10,
        "instruments outside the universe are dropped"
    );
    assert_eq!(value.state.dividends.len(), 0);
    let mut dividends = builtin(BuiltinStrategySpec::DividendMonth(
        DividendMonthConfig::default(),
    ));
    dividends.on_event(&event).await.unwrap();
    assert_eq!(
        (
            dividends.state.fundamentals.len(),
            dividends.state.dividends.len()
        ),
        (0, 1)
    );
    let mut trend = builtin(BuiltinStrategySpec::MovingAverageTrend(
        MovingAverageTrendConfig::default(),
    ));
    trend.on_event(&event).await.unwrap();
    assert!(trend.state.fundamentals.is_empty() && trend.reference_requirements().is_empty());
    // The ensemble asks for the union of its components' data.
    let combination = WeightedEnsembleStrategy::new(
        StrategyId::new("combo").unwrap(),
        AccountId::new("account").unwrap(),
        ten(),
        vec![
            WeightedStrategySpec {
                signal_weight: Decimal::ONE,
                instruments: None,
                strategy: BuiltinStrategySpec::ValueComposite(ValueCompositeConfig::default()),
            },
            WeightedStrategySpec {
                signal_weight: Decimal::ONE,
                instruments: None,
                strategy: BuiltinStrategySpec::DividendMonth(DividendMonthConfig::default()),
            },
            WeightedStrategySpec {
                signal_weight: Decimal::ONE,
                instruments: None,
                strategy: BuiltinStrategySpec::CrossSectionalMomentum(
                    CrossSectionalMomentumConfig::default(),
                ),
            },
        ],
    )
    .unwrap();
    assert_eq!(
        combination.reference_requirements(),
        vec![
            ReferenceRequirement::Fundamentals,
            ReferenceRequirement::Dividends
        ]
    );
}
