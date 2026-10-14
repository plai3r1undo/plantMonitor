//! Push notifications through ntfy when a plant needs attention.
//!
//! Every minute each plant (with alerts enabled) is evaluated. Per plant and
//! metric we remember the current problem and when it started; a problem is
//! notified once it has lasted `alert_after_minutes`, re-notified every
//! `remind_after_hours`, and optionally a recovery is sent when it clears.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use serde::Serialize;

use crate::config::Ntfy;
use crate::db::AlertState;
use crate::eval::{Overall, Status};
use crate::status::{self, PlantStatus};
use crate::AppState;

const TICK: Duration = Duration::from_secs(60);
const OFFLINE: &str = "offline";

#[derive(Serialize)]
pub struct Message<'a> {
    topic: &'a str,
    title: String,
    message: String,
    priority: u8,
    tags: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    click: Option<&'a str>,
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("building HTTP client")
}

pub async fn send(state: &AppState, title: String, message: String, priority: u8, tags: Vec<&'static str>) -> Result<()> {
    let Some(ntfy) = &state.config.ntfy else { bail!("ntfy is not configured") };
    let body = Message {
        topic: &ntfy.topic,
        title,
        message,
        priority,
        tags,
        click: state.config.public_url.as_deref(),
    };
    // JSON publishing (POST to the server root) handles non-ASCII titles.
    let mut req = state.http.post(&ntfy.server).json(&body);
    if let Some(token) = &ntfy.token {
        req = req.bearer_auth(token);
    }
    let res = req.send().await?;
    if !res.status().is_success() {
        bail!("ntfy responded {}", res.status());
    }
    Ok(())
}

pub async fn run(state: Arc<AppState>) {
    let Some(ntfy) = &state.config.ntfy else { return };
    tracing::info!("alerts enabled via {}", ntfy.server);
    let mut interval = tokio::time::interval(TICK);
    loop {
        interval.tick().await;
        if let Err(e) = tick(&state, ntfy).await {
            tracing::error!("alert check failed: {e:#}");
        }
    }
}

/// Current problems of a plant as (metric, label, status, detail).
fn problems(p: &PlantStatus) -> Vec<(&'static str, String, &'static str, String)> {
    let mut out = Vec::new();
    match p.status {
        Overall::NoData => {}
        Overall::Offline => out.push((
            OFFLINE,
            "Sensor offline".to_string(),
            "offline",
            "No reading received recently — check the sensor's power and WiFi.".to_string(),
        )),
        Overall::Ok | Overall::Attention => {
            for c in &p.checks {
                let status = match c.status {
                    Status::Low => "low",
                    Status::High => "high",
                    Status::Ok | Status::Unknown => continue,
                };
                let detail = format!(
                    "{} Now {:.1} {u}, optimal {}–{} {u}.",
                    c.advice.unwrap_or(""),
                    c.value.unwrap_or_default(),
                    c.min,
                    c.max,
                    u = c.unit
                );
                out.push((c.metric, format!("{} {status}", c.label), status, detail));
            }
        }
    }
    out
}

const METRICS: [&str; 5] = ["soil_moisture", "light_lux", "temperature", "humidity", OFFLINE];

async fn tick(state: &AppState, ntfy: &Ntfy) -> Result<()> {
    let now = status::now();
    let plants = status::all_plants(&state.db, state.config.offline_after_minutes * 60)?;
    for p in plants.iter().filter(|p| p.plant.alerts) {
        let current = problems(p);
        // While offline the readings are stale: only track the offline state
        // and leave the sensor metrics as they were.
        let metrics: &[&str] = match p.status {
            Overall::NoData => continue,
            Overall::Offline => &[OFFLINE],
            Overall::Ok | Overall::Attention => &METRICS,
        };
        for &metric in metrics {
            let problem = current.iter().find(|(m, ..)| *m == metric);
            let status = problem.map_or("ok", |(_, _, s, _)| *s);

            let mut st = match state.db.alert_get(p.plant.id, metric)? {
                Some(st) if st.status == status => st,
                prev => {
                    // Status changed (or first time seen): start a new period.
                    let was_notified = prev.is_some_and(|s| s.notified_at.is_some());
                    if was_notified && status == "ok" && ntfy.notify_recovery {
                        let title = format!("✅ {}: {} back to normal", p.plant.name, label(metric));
                        let msg = "Back within the optimal range.".to_string();
                        if let Err(e) = send(state, title, msg, 2, vec!["white_check_mark"]).await {
                            tracing::warn!("sending recovery notification: {e:#}");
                        }
                    }
                    let fresh = AlertState { status: status.into(), since: now, notified_at: None };
                    state.db.alert_set(p.plant.id, metric, &fresh)?;
                    continue;
                }
            };
            let Some((_, title, _, detail)) = problem else { continue };

            let due = match st.notified_at {
                None => now - st.since >= ntfy.alert_after_minutes * 60,
                Some(at) => ntfy.remind_after_hours > 0 && now - at >= ntfy.remind_after_hours * 3600,
            };
            if !due {
                continue;
            }
            let priority = if metric == "soil_moisture" || metric == OFFLINE { 4 } else { 3 };
            let title = format!("{}: {title}", p.plant.name);
            match send(state, title, detail.clone(), priority, vec!["potted_plant", "warning"]).await {
                Ok(()) => {
                    st.notified_at = Some(now);
                    state.db.alert_set(p.plant.id, metric, &st)?;
                    tracing::info!(plant = %p.plant.name, metric, "alert sent");
                }
                // Leave notified_at unset so the next tick retries.
                Err(e) => tracing::warn!("sending alert: {e:#}"),
            }
        }
    }
    Ok(())
}

fn label(metric: &str) -> &'static str {
    match metric {
        "soil_moisture" => "soil moisture",
        "light_lux" => "light",
        "temperature" => "temperature",
        "humidity" => "humidity",
        _ => "sensor",
    }
}
