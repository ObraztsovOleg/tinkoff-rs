//! Индикаторы по свечам для инструмента `get_indicators`.
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tinkoff_rs::domain::Candle;

pub(crate) fn round(value: f64) -> Value {
    if value.is_finite() {
        json!((value * 10_000.0).round() / 10_000.0)
    } else {
        Value::Null
    }
}

pub(crate) fn number(price: Decimal) -> f64 {
    price.to_string().parse().unwrap_or(f64::NAN)
}

pub(crate) fn sma(values: &[f64], n: usize) -> Option<f64> {
    (n > 0 && values.len() >= n).then(|| values[values.len() - n..].iter().sum::<f64>() / n as f64)
}

pub(crate) fn ema_series(values: &[f64], n: usize) -> Vec<f64> {
    if n == 0 || values.len() < n {
        return Vec::new();
    }
    let k = 2.0 / (n as f64 + 1.0);
    let mut current = values[..n].iter().sum::<f64>() / n as f64;
    let mut out = vec![current];
    for value in &values[n..] {
        current = value * k + current * (1.0 - k);
        out.push(current);
    }
    out
}

pub(crate) fn rsi(closes: &[f64], n: usize) -> Option<f64> {
    if n == 0 || closes.len() <= n {
        return None;
    }
    let changes: Vec<f64> = closes.windows(2).map(|w| w[1] - w[0]).collect();
    let mut gain = changes[..n].iter().map(|c| c.max(0.0)).sum::<f64>() / n as f64;
    let mut loss = changes[..n].iter().map(|c| (-c).max(0.0)).sum::<f64>() / n as f64;
    for change in &changes[n..] {
        gain = (gain * (n as f64 - 1.0) + change.max(0.0)) / n as f64;
        loss = (loss * (n as f64 - 1.0) + (-change).max(0.0)) / n as f64;
    }
    Some(if loss == 0.0 {
        100.0
    } else {
        100.0 - 100.0 / (1.0 + gain / loss)
    })
}

pub(crate) fn stdev(values: &[f64]) -> f64 {
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    (values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64).sqrt()
}

/// Индикатор по имени: `sma_50`, `ema_20`, `rsi_14`, `macd`, `bollinger_20`,
/// `atr_14`, `volatility_20`, `return_20d`, `high_52w`, `low_52w`,
/// `distance_from_high_52w`, `volume_avg_20`, `max_drawdown_60`, `price`.
pub(crate) fn indicator(name: &str, candles: &[Candle]) -> Value {
    let closes: Vec<f64> = candles.iter().map(|c| number(c.close.0)).collect();
    let last = closes.last().copied().unwrap_or(f64::NAN);
    let window = |prefix: &str, default: usize| -> Option<usize> {
        let rest = name.strip_prefix(prefix)?;
        if rest.is_empty() {
            return Some(default);
        }
        rest.strip_prefix('_')?
            .trim_end_matches('d')
            .parse()
            .ok()
            .filter(|n| (1..=250).contains(n))
    };
    let missing = || json!({"error": "not enough history"});
    if name == "price" {
        return round(last);
    }
    if let Some(n) = window("sma", 20) {
        return sma(&closes, n).map_or_else(missing, round);
    }
    if let Some(n) = window("ema", 20) {
        return ema_series(&closes, n)
            .last()
            .copied()
            .map_or_else(missing, round);
    }
    if let Some(n) = window("rsi", 14) {
        return rsi(&closes, n).map_or_else(missing, round);
    }
    if name == "macd" {
        let (fast, slow) = (ema_series(&closes, 12), ema_series(&closes, 26));
        if slow.len() < 9 {
            return missing();
        }
        let line: Vec<f64> = slow
            .iter()
            .zip(&fast[fast.len() - slow.len()..])
            .map(|(s, f)| f - s)
            .collect();
        let signal = ema_series(&line, 9);
        let (macd, signal) = (
            *line.last().unwrap_or(&f64::NAN),
            *signal.last().unwrap_or(&f64::NAN),
        );
        return json!({"macd": round(macd), "signal": round(signal), "histogram": round(macd - signal)});
    }
    if let Some(n) = window("bollinger", 20) {
        if closes.len() < n {
            return missing();
        }
        let tail = &closes[closes.len() - n..];
        let (middle, deviation) = (tail.iter().sum::<f64>() / n as f64, stdev(tail));
        let (upper, lower) = (middle + 2.0 * deviation, middle - 2.0 * deviation);
        return json!({"upper": round(upper), "middle": round(middle), "lower": round(lower),
                      "percent_b": round(if upper > lower { (last - lower) / (upper - lower) } else { 0.5 })});
    }
    if let Some(n) = window("atr", 14) {
        if candles.len() <= n {
            return missing();
        }
        let ranges: Vec<f64> = candles
            .windows(2)
            .map(|w| {
                let (high, low, previous) = (
                    number(w[1].high.0),
                    number(w[1].low.0),
                    number(w[0].close.0),
                );
                (high - low)
                    .max((high - previous).abs())
                    .max((low - previous).abs())
            })
            .collect();
        let mut atr = ranges[..n].iter().sum::<f64>() / n as f64;
        for range in &ranges[n..] {
            atr = (atr * (n as f64 - 1.0) + range) / n as f64;
        }
        return json!({"atr": round(atr), "atr_percent": round(atr / last * 100.0)});
    }
    if let Some(n) = window("volatility", 20) {
        if closes.len() <= n {
            return missing();
        }
        let returns: Vec<f64> = closes[closes.len() - n - 1..]
            .windows(2)
            .map(|w| (w[1] / w[0]).ln())
            .collect();
        return round(stdev(&returns) * 252f64.sqrt() * 100.0);
    }
    if let Some(n) = window("return", 20) {
        return (closes.len() > n)
            .then(|| (last / closes[closes.len() - 1 - n] - 1.0) * 100.0)
            .map_or_else(missing, round);
    }
    if matches!(name, "high_52w" | "low_52w" | "distance_from_high_52w") {
        let tail = &candles[candles.len().saturating_sub(252)..];
        let high = tail
            .iter()
            .map(|c| number(c.high.0))
            .fold(f64::MIN, f64::max);
        let low = tail
            .iter()
            .map(|c| number(c.low.0))
            .fold(f64::MAX, f64::min);
        return match name {
            "high_52w" => round(high),
            "low_52w" => round(low),
            _ => round((last / high - 1.0) * 100.0),
        };
    }
    if let Some(n) = window("volume_avg", 20) {
        let tail = &candles[candles.len().saturating_sub(n)..];
        return round(tail.iter().map(|c| c.volume as f64).sum::<f64>() / tail.len().max(1) as f64);
    }
    if let Some(n) = window("max_drawdown", 60) {
        let tail = &closes[closes.len().saturating_sub(n)..];
        let (mut peak, mut worst) = (f64::MIN, 0.0_f64);
        for close in tail {
            peak = peak.max(*close);
            worst = worst.min(close / peak - 1.0);
        }
        return round(worst * 100.0);
    }
    json!({"error": "unknown indicator", "supported": SUPPORTED_INDICATORS})
}

pub(crate) const SUPPORTED_INDICATORS: &str = "price, sma_N, ema_N, rsi_N, macd, bollinger_N, atr_N, volatility_N, return_Nd, high_52w, low_52w, distance_from_high_52w, volume_avg_N, max_drawdown_N (N от 1 до 250)";

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    use rust_decimal::prelude::FromPrimitive;
    use tinkoff_rs::domain::{CandleInterval, InstrumentId, Price};

    fn candle(day: i64, close: f64, volume: i64) -> Candle {
        let price = |v: f64| Price(Decimal::from_f64(v).unwrap());
        Candle {
            instrument_id: InstrumentId::new("uid".to_owned()).unwrap(),
            interval: CandleInterval::OneDay,
            open: price(close - 1.0),
            high: price(close + 1.0),
            low: price(close - 2.0),
            close: price(close),
            volume,
            time: DateTime::from_timestamp(1_700_000_000 + day * 86_400, 0).unwrap(),
            complete: true,
        }
    }

    #[test]
    fn indicators_follow_standard_definitions() {
        let rising: Vec<Candle> = (0..60)
            .map(|i| candle(i, 100.0 + i as f64, 1000 + i))
            .collect();
        assert_eq!(indicator("price", &rising), json!(159.0));
        assert_eq!(indicator("sma_5", &rising), json!(157.0));
        assert_eq!(indicator("rsi_14", &rising), json!(100.0), "only gains");
        assert_eq!(
            indicator("return_10d", &rising),
            round((159.0 / 149.0 - 1.0) * 100.0)
        );
        assert_eq!(indicator("max_drawdown_30", &rising), json!(0.0));
        assert!(indicator("macd", &rising)["macd"].as_f64().unwrap() > 0.0);
        let bands = indicator("bollinger_20", &rising);
        assert!(bands["upper"].as_f64() > bands["lower"].as_f64());
        assert!(
            indicator("sma_100", &rising)["error"].is_string(),
            "not enough history"
        );
        assert!(indicator("magic", &rising)["supported"].is_string());
    }
}
