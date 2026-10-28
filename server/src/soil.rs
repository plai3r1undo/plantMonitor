//! Judges the pot and soil mix from how fast the soil dries after watering.
//!
//! Each watering shows up as a jump in soil moisture. From the peak after it
//! we measure how long the soil takes to fall to the species' minimum soil
//! moisture (when it should be watered again) and compare the typical time
//! with the species' expected drying time. A cactus mix still wet after
//! weeks, or a tropical mix dry within a day, is probably the wrong soil or
//! pot.

use serde::Serialize;

use crate::model::Range;

/// History the estimate looks at.
pub const WINDOW_SECS: i64 = 60 * DAY;
const DAY: i64 = 86_400;
/// A watering raises soil moisture by at least this many points…
const RISE: f64 = 15.0;
/// …within this time.
const RISE_SECS: i64 = 3 * 3600;
/// Rises this close together are one watering; the peak is the highest point.
const SETTLE_SECS: i64 = 6 * 3600;
/// Water draining from the pot right after watering isn't drying; the rate
/// fit skips this much after the peak.
const DRAIN_SECS: i64 = 6 * 3600;
/// A cycle that hasn't reached the minimum must be at least this long to say anything.
const MIN_CYCLE_SECS: i64 = DAY;
/// The peak must be this far above the minimum to count as a watering.
const MIN_HEADROOM: f64 = 5.0;
const MAX_DAYS: f64 = 365.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Dries as expected for the species.
    Ok,
    /// Stays wet much longer than expected: mix too dense, poor drainage.
    Slow,
    /// Dries much faster than expected: mix too coarse, pot too small.
    Fast,
    /// No judgement (not enough data, dormant, or no expected time set).
    Unknown,
}

#[derive(Debug, Serialize)]
pub struct Drying {
    pub verdict: Verdict,
    /// Typical days from watering until the soil reaches the minimum.
    pub days: Option<f64>,
    /// The species' expected drying time.
    pub expected_days: Option<Range>,
    /// Waterings the estimate is based on.
    pub waterings: usize,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    /// The soil reached the minimum.
    Measured,
    /// Watered again (or still drying): projected from the drying rate.
    Projected,
    /// Not drying measurably: the time so far is a lower bound.
    AtLeast,
}

#[derive(Debug, Clone, Copy)]
struct Cycle {
    days: f64,
    kind: Kind,
}

/// `points` are (timestamp, soil moisture %) in time order; `minimum` is the
/// species' minimum soil moisture.
pub fn estimate(points: &[(i64, f64)], minimum: f64, expected: Option<Range>, dormant: bool) -> Drying {
    let unknown = |days, waterings, message: &str| Drying {
        verdict: Verdict::Unknown,
        days,
        expected_days: expected,
        waterings,
        message: message.to_string(),
    };
    if dormant {
        return unknown(None, 0, "Not judged while dormant: resting plants use very little water.");
    }
    let cycles = cycles(points, minimum);
    if cycles.is_empty() {
        return unknown(
            None,
            0,
            "Not enough data yet. After a thorough watering it takes a few days of readings to see how fast the soil dries.",
        );
    }
    let days = median(cycles.iter().map(|c| c.days).collect());
    let n = cycles.len();
    let basis = format!(
        "Based on {n} watering{}{}.",
        if n == 1 { "" } else { "s" },
        if cycles.iter().all(|c| c.kind == Kind::Measured) { "" } else { ", partly projected" }
    );
    let Some([lo, hi]) = expected else {
        let msg = format!(
            "The soil takes about {} to dry after watering. Set the species' expected drying time to have it judged. {basis}",
            span(days)
        );
        return unknown(Some(days), n, &msg);
    };
    let typical = format!("{lo}–{hi} days is typical for this species");
    let (verdict, message) = if days > hi {
        (
            Verdict::Slow,
            format!(
                "The soil stays moist for about {} after watering; {typical}. The mix probably holds too much water or \
                 drains badly: use a more porous mix (for cacti and succulents mostly pumice, perlite or grit), make sure \
                 the pot has a drainage hole, or use a smaller or terracotta pot. Cold and low light also slow drying.",
                span(days)
            ),
        )
    } else if days < lo {
        (
            Verdict::Fast,
            format!(
                "The soil dries in about {} after watering; {typical}. The mix may be too coarse or the pot too small \
                 or root-bound: add material that holds water (coco coir, peat, compost) or repot into a larger pot. \
                 Heat and strong sun also speed drying.",
                span(days)
            ),
        )
    } else {
        (Verdict::Ok, format!("The soil dries in about {} after watering, as expected ({typical}).", span(days)))
    };
    Drying { verdict, days: Some(days), expected_days: expected, waterings: n, message: format!("{message} {basis}") }
}

/// Drying cycles: one per watering, from its peak to the minimum (or to the
/// next watering / the end of the data).
fn cycles(p: &[(i64, f64)], minimum: f64) -> Vec<Cycle> {
    let peaks = watering_peaks(p);
    let mut out = Vec::new();
    for (k, &start) in peaks.iter().enumerate() {
        let (t0, v0) = p[start];
        if v0 < minimum + MIN_HEADROOM {
            continue;
        }
        // Up to the next watering, without its rise.
        let end = match peaks.get(k + 1) {
            Some(&next) => p[..next].partition_point(|&(t, _)| t < p[next].0 - RISE_SECS).max(start + 1),
            None => p.len(),
        };
        let seg = &p[start..end];
        if let Some(&(t, _)) = seg.iter().find(|&&(_, v)| v <= minimum) {
            out.push(Cycle { days: (t - t0) as f64 / DAY as f64, kind: Kind::Measured });
            continue;
        }
        let Some(&(t_last, _)) = seg.last() else { continue };
        if t_last - t0 < MIN_CYCLE_SECS {
            continue;
        }
        let so_far = (t_last - t0) as f64 / DAY as f64;
        let fit: Vec<_> = seg.iter().filter(|&&(t, _)| t >= t0 + DRAIN_SECS).copied().collect();
        // Percentage points lost per day.
        let rate = -slope(if fit.len() >= 3 { &fit } else { seg }) * DAY as f64;
        out.push(if rate > 0.1 {
            let days = ((v0 - minimum) / rate).clamp(so_far, MAX_DAYS);
            Cycle { days, kind: Kind::Projected }
        } else {
            Cycle { days: so_far, kind: Kind::AtLeast }
        });
    }
    out
}

/// Index of the highest point after each watering.
fn watering_peaks(p: &[(i64, f64)]) -> Vec<usize> {
    let mut peaks: Vec<usize> = Vec::new();
    let mut from = 0;
    for i in 0..p.len() {
        let (t, v) = p[i];
        while p[from].0 < t - RISE_SECS {
            from += 1;
        }
        let low = p[from..i].iter().map(|&(_, v)| v).fold(f64::INFINITY, f64::min);
        if v - low < RISE {
            continue;
        }
        match peaks.last_mut() {
            Some(last) if t - p[*last].0 < SETTLE_SECS => {
                if v > p[*last].1 {
                    *last = i;
                }
            }
            _ => peaks.push(i),
        }
    }
    // Still rising after a rise: move each peak to the highest point that
    // follows within the settle time.
    for peak in &mut peaks {
        let t0 = p[*peak].0;
        for (j, &(t, v)) in p.iter().enumerate().skip(*peak + 1) {
            if t - t0 > SETTLE_SECS {
                break;
            }
            if v > p[*peak].1 {
                *peak = j;
            }
        }
    }
    peaks.dedup();
    peaks
}

/// Least-squares slope (per second).
fn slope(p: &[(i64, f64)]) -> f64 {
    let n = p.len() as f64;
    if n < 2.0 {
        return 0.0;
    }
    let t0 = p[0].0;
    let mt = p.iter().map(|&(t, _)| (t - t0) as f64).sum::<f64>() / n;
    let mv = p.iter().map(|&(_, v)| v).sum::<f64>() / n;
    let (mut num, mut den) = (0.0, 0.0);
    for &(t, v) in p {
        let dt = (t - t0) as f64 - mt;
        num += dt * (v - mv);
        den += dt * dt;
    }
    if den == 0.0 { 0.0 } else { num / den }
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    let m = v.len() / 2;
    if v.len() % 2 == 1 { v[m] } else { (v[m - 1] + v[m]) / 2.0 }
}

/// "18 hours", "1 day", "9 days".
fn span(days: f64) -> String {
    if days < 1.0 {
        let h = (days * 24.0).round().max(1.0);
        format!("{h} hour{}", if h == 1.0 { "" } else { "s" })
    } else {
        let d = days.round();
        format!("{d} day{}", if d == 1.0 { "" } else { "s" })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3600;

    /// Readings every 10 min: dry at `before`, watered at `water_at` (hours)
    /// up to `peak`, then losing `per_day` points a day, floored at 0.
    fn series(hours: i64, before: f64, waterings: &[i64], peak: f64, per_day: f64) -> Vec<(i64, f64)> {
        let mut v = before;
        let mut out = Vec::new();
        for i in 0..hours * 6 {
            let t = i * 600;
            if waterings.contains(&(t / H)) && t % H == 0 {
                v = peak;
            } else {
                v = (v - per_day / 144.0).max(0.0);
            }
            out.push((t, v));
        }
        out
    }

    #[test]
    fn cactus_mix_drying_in_a_week_is_ok() {
        // 60 points to 10 % at 7 points/day: about 7 days.
        let p = series(24 * 20, 8.0, &[24], 60.0, 7.0);
        let d = estimate(&p, 10.0, Some([4.0, 14.0]), false);
        assert_eq!(d.verdict, Verdict::Ok, "{d:?}");
        assert!((d.days.unwrap() - 7.1).abs() < 0.3, "{d:?}");
        assert_eq!(d.waterings, 1);
    }

    #[test]
    fn cactus_soil_still_wet_after_weeks_is_slow() {
        // Loses 1 point a day: never reaches 10 % within the data.
        let p = series(24 * 25, 8.0, &[24], 60.0, 1.0);
        let d = estimate(&p, 10.0, Some([4.0, 14.0]), false);
        assert_eq!(d.verdict, Verdict::Slow, "{d:?}");
        assert!(d.days.unwrap() > 14.0);
    }

    #[test]
    fn basil_drying_in_hours_is_fast() {
        let p = series(24 * 6, 40.0, &[10, 40, 70], 80.0, 60.0);
        let d = estimate(&p, 45.0, Some([1.0, 4.0]), false);
        assert_eq!(d.verdict, Verdict::Fast, "{d:?}");
        assert_eq!(d.waterings, 3);
        assert!(d.days.unwrap() < 1.0);
    }

    #[test]
    fn watered_early_is_projected_from_the_rate() {
        // Monstera watered every 4 days at 5 points/day: reaches 35 % after
        // 6 days if left alone.
        let p = series(24 * 14, 40.0, &[5, 101, 197, 293], 65.0, 5.0);
        let d = estimate(&p, 35.0, Some([5.0, 12.0]), false);
        assert_eq!(d.verdict, Verdict::Ok, "{d:?}");
        assert!((d.days.unwrap() - 6.0).abs() < 0.6, "{d:?}");
        assert!(d.message.contains("projected"));
    }

    #[test]
    fn no_watering_or_dormant_is_unknown() {
        let flat = series(24 * 10, 30.0, &[], 0.0, 1.0);
        assert_eq!(estimate(&flat, 10.0, Some([4.0, 14.0]), false).verdict, Verdict::Unknown);
        let p = series(24 * 20, 8.0, &[24], 60.0, 1.0);
        assert_eq!(estimate(&p, 10.0, Some([4.0, 14.0]), true).verdict, Verdict::Unknown);
        let d = estimate(&p, 10.0, None, false);
        assert_eq!(d.verdict, Verdict::Unknown);
        assert!(d.days.is_some());
    }

    #[test]
    fn noise_is_not_a_watering() {
        let p: Vec<_> = (0..1000).map(|i| (i * 600, 30.0 + if i % 2 == 0 { 4.0 } else { -4.0 })).collect();
        assert!(watering_peaks(&p).is_empty());
    }
}
