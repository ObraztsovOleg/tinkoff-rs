//! Справочные данные стратегий на отчётности и дивидендах: мультипликаторы
//! эмитентов и объявленные дивиденды.

use chrono::{Duration, NaiveDate, Utc};
use rust_decimal::Decimal;
use tinkoff_rs::{
    Error, Result,
    domain::{InstrumentId, Money, datetime_to_timestamp, timestamp_to_datetime},
    engine::{
        DividendEvent, EventPhase, InstrumentFundamentals, PortfolioEngine, ReferenceData,
        ReferenceRequirement, StrategyEvent, StrategyEventKind,
    },
    proto::{
        GetAssetFundamentalsRequest, GetDividendsRequest, InstrumentIdType, InstrumentRequest,
    },
    storage::PostgresStorage,
};
use tracing::info;

use crate::venue::VenueApi;

/// Загружает справочные данные, если они нужны стратегии, и сохраняет их в state.
///
/// Событие подаётся в фазе Warmup: стратегия обновляет снимок, но не может
/// выставить заявку. ID включает дату, поэтому повтор в тот же день не дублируется.
pub(super) async fn absorb_reference(
    api: &VenueApi,
    engine: &PortfolioEngine<PostgresStorage>,
    universe: &[InstrumentId],
    date: NaiveDate,
) -> Result<()> {
    let requirements = engine.reference_requirements().await;
    if requirements.is_empty() {
        return Ok(());
    }
    let reference = load_reference(api, universe, &requirements).await?;
    let (fundamentals, dividends) = (reference.fundamentals.len(), reference.dividends.len());
    let event = StrategyEvent {
        id: format!("reference:{date}"),
        phase: EventPhase::Warmup,
        occurred_at: reference.observed_at,
        payload: StrategyEventKind::Reference(reference),
    };
    let outcome = engine.process_event(&event, None).await?;
    if !outcome.duplicate {
        info!(fundamentals, dividends, "reference data loaded");
    }
    Ok(())
}

/// T-Invest кодирует отсутствующий показатель нулём: такой ноль не является данными.
pub(super) fn reported(value: f64) -> Option<Decimal> {
    (value.is_finite() && value != 0.0)
        .then(|| Decimal::try_from(value).ok())
        .flatten()
        .map(|value| value.round_dp(6))
}

pub(super) async fn load_reference(
    api: &VenueApi,
    universe: &[InstrumentId],
    requirements: &[ReferenceRequirement],
) -> Result<ReferenceData> {
    let now = Utc::now();
    let mut reference = ReferenceData {
        observed_at: now,
        ..ReferenceData::default()
    };
    if requirements.contains(&ReferenceRequirement::Fundamentals) {
        // Показатели привязаны к активу, а не к торгуемому UID.
        let mut assets = Vec::with_capacity(universe.len());
        for instrument_id in universe {
            let share = api
                .instruments()
                .share_by(InstrumentRequest {
                    id_type: InstrumentIdType::Uid as i32,
                    class_code: None,
                    id: instrument_id.to_string(),
                })
                .await?
                .into_inner()
                .instrument
                .ok_or_else(|| {
                    Error::Configuration(format!("share `{instrument_id}` not found"))
                })?;
            assets.push((instrument_id.clone(), share.asset_uid));
        }
        let response = api
            .instruments()
            .get_asset_fundamentals(GetAssetFundamentalsRequest {
                assets: assets.iter().map(|(_, asset)| asset.clone()).collect(),
            })
            .await?
            .into_inner();
        for row in response.fundamentals {
            let Some((instrument_id, _)) = assets.iter().find(|(_, asset)| *asset == row.asset_uid)
            else {
                continue;
            };
            reference.fundamentals.push(InstrumentFundamentals {
                instrument_id: instrument_id.clone(),
                price_to_book: reported(row.price_to_book_ttm),
                price_to_earnings: reported(row.pe_ratio_ttm),
                ev_to_ebitda: reported(row.ev_to_ebitda_mrq),
                return_on_equity: reported(row.roe),
                return_on_assets: reported(row.roa),
                net_margin: reported(row.net_margin_mrq),
                debt_to_equity: reported(row.total_debt_to_equity_mrq),
            });
        }
    }
    if requirements.contains(&ReferenceRequirement::Dividends) {
        for instrument_id in universe {
            let response = api
                .instruments()
                .get_dividends(GetDividendsRequest {
                    instrument_id: instrument_id.to_string(),
                    from: Some(datetime_to_timestamp(now - Duration::days(7))),
                    to: Some(datetime_to_timestamp(now + Duration::days(200))),
                    ..GetDividendsRequest::default()
                })
                .await?
                .into_inner();
            for dividend in response.dividends {
                let Some(last_buy) = dividend
                    .last_buy_date
                    .map(timestamp_to_datetime)
                    .transpose()?
                else {
                    continue;
                };
                reference.dividends.push(DividendEvent {
                    instrument_id: instrument_id.clone(),
                    last_buy_date: last_buy.date_naive(),
                    dividend_net: dividend
                        .dividend_net
                        .map(Money::from_proto)
                        .transpose()?
                        .map(|money| money.amount.0),
                });
            }
        }
    }
    Ok(reference)
}
