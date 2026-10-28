//! Compares measured conditions with a species' optimal ranges.

use serde::Serialize;

use crate::model::{Range, Ranges};

/// Readings below this many lux count as "dark" and are excluded from the
/// light average, so nights don't make every plant look under-lit.
pub const DAYLIGHT_LUX: f64 = 50.0;
/// Window over which light is averaged.
pub const LIGHT_WINDOW_SECS: i64 = 24 * 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Low,
    High,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Overall {
    Ok,
    Attention,
    Offline,
    NoData,
    /// No device assigned (its sensor moved to another plant).
    NoSensor,
}

#[derive(Debug, Serialize)]
pub struct Check {
    pub metric: &'static str,
    pub label: &'static str,
    pub unit: &'static str,
    pub value: Option<f64>,
    pub min: f64,
    pub max: f64,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advice: Option<&'static str>,
}

/// Values the evaluation runs on; light is the daylight average, not the
/// instantaneous reading.
pub struct Conditions {
    pub temperature: Option<f64>,
    pub humidity: Option<f64>,
    pub light_lux: Option<f64>,
    pub soil_moisture: Option<f64>,
}

/// Converts a raw capacitive sensor value to 0–100 %. The sensor reads
/// higher when dry, so `dry > wet` normally, but either order works.
pub fn soil_percent(raw: i64, dry: i64, wet: i64) -> f64 {
    let pct = (dry - raw) as f64 / (dry - wet) as f64 * 100.0;
    pct.clamp(0.0, 100.0)
}

fn check(
    metric: &'static str,
    label: &'static str,
    unit: &'static str,
    value: Option<f64>,
    [min, max]: Range,
    advice_low: &'static str,
    advice_high: &'static str,
) -> Check {
    let (status, advice) = match value {
        None => (Status::Unknown, None),
        Some(v) if v < min => (Status::Low, Some(advice_low)),
        Some(v) if v > max => (Status::High, Some(advice_high)),
        Some(_) => (Status::Ok, None),
    };
    Check { metric, label, unit, value, min, max, status, advice }
}

pub fn evaluate(ranges: &Ranges, c: &Conditions) -> Vec<Check> {
    vec![
        check(
            "soil_moisture",
            "Soil moisture",
            "%",
            c.soil_moisture,
            ranges.soil_moisture,
            "Soil is too dry — water the plant.",
            "Soil is too wet — hold off watering and check drainage.",
        ),
        check(
            "light_lux",
            "Light (daylight avg)",
            "lx",
            c.light_lux,
            ranges.light_lux,
            "Not enough light — move closer to a window or add a grow light.",
            "Too much light — move away from direct sun or add a sheer curtain.",
        ),
        check(
            "temperature",
            "Temperature",
            "°C",
            c.temperature,
            ranges.temperature,
            "Too cold — move away from drafts and cold windows.",
            "Too hot — move away from heaters or direct afternoon sun.",
        ),
        check(
            "humidity",
            "Humidity",
            "%",
            c.humidity,
            ranges.humidity,
            "Air is too dry — group plants, use a pebble tray or humidifier.",
            "Air is too humid — improve air circulation.",
        ),
    ]
}

pub fn overall(has_device: bool, last_seen: Option<i64>, now: i64, offline_after_secs: i64, checks: &[Check]) -> Overall {
    match last_seen {
        _ if !has_device => Overall::NoSensor,
        None => Overall::NoData,
        Some(ts) if now - ts > offline_after_secs => Overall::Offline,
        Some(_) if checks.iter().any(|c| matches!(c.status, Status::Low | Status::High)) => {
            Overall::Attention
        }
        Some(_) => Overall::Ok,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soil_percent_maps_and_clamps() {
        assert_eq!(soil_percent(2800, 2800, 1200), 0.0);
        assert_eq!(soil_percent(1200, 2800, 1200), 100.0);
        assert_eq!(soil_percent(2000, 2800, 1200), 50.0);
        assert_eq!(soil_percent(3500, 2800, 1200), 0.0);
        assert_eq!(soil_percent(500, 2800, 1200), 100.0);
    }

    #[test]
    fn check_statuses() {
        let r = [10.0, 20.0];
        assert_eq!(check("x", "x", "", Some(5.0), r, "", "").status, Status::Low);
        assert_eq!(check("x", "x", "", Some(25.0), r, "", "").status, Status::High);
        assert_eq!(check("x", "x", "", Some(20.0), r, "", "").status, Status::Ok);
        assert_eq!(check("x", "x", "", None, r, "", "").status, Status::Unknown);
    }
}
