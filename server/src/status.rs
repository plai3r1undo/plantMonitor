//! Current state of a plant: latest reading evaluated against its species.
//! Shared by the API and the alert loop.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::Serialize;

use crate::db::{Db, StoredReading};
use crate::eval::{self, Check, Conditions, Overall};
use crate::model::{Plant, Species};

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

#[derive(Serialize)]
pub struct ReadingOut {
    pub timestamp: i64,
    pub temperature: Option<f64>,
    pub humidity: Option<f64>,
    pub lux: Option<f64>,
    /// Calibrated percentage; only present for devices assigned to a plant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub soil_moisture: Option<f64>,
    pub soil_raw: Option<i64>,
    pub rssi: Option<i64>,
}

impl ReadingOut {
    pub fn new(r: &StoredReading, plant: Option<&Plant>) -> Self {
        let soil_moisture = plant.and_then(|p| {
            r.soil_raw
                .map(|raw| round1(eval::soil_percent(raw, p.soil_dry_raw, p.soil_wet_raw)))
        });
        ReadingOut {
            timestamp: r.ts,
            temperature: r.temperature,
            humidity: r.humidity,
            lux: r.lux,
            soil_moisture,
            soil_raw: r.soil_raw,
            rssi: r.rssi,
        }
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

#[derive(Serialize)]
pub struct PlantStatus {
    #[serde(flatten)]
    pub plant: Plant,
    pub species_info: Species,
    /// Evaluated against the species' dormancy ranges.
    pub dormancy_active: bool,
    pub status: Overall,
    pub last_seen: Option<i64>,
    pub latest: Option<ReadingOut>,
    pub checks: Vec<Check>,
}

pub fn plant_status(db: &Db, plant: Plant, species: Species, offline_after_secs: i64) -> Result<PlantStatus> {
    let now = now();
    let latest = db.latest(&plant.device_id)?.map(|r| ReadingOut::new(&r, Some(&plant)));
    let light = db
        .daylight_lux(&plant.device_id, now - eval::LIGHT_WINDOW_SECS, eval::DAYLIGHT_LUX)?
        .map(f64::round);
    let conditions = Conditions {
        temperature: latest.as_ref().and_then(|r| r.temperature),
        humidity: latest.as_ref().and_then(|r| r.humidity),
        light_lux: light,
        soil_moisture: latest.as_ref().and_then(|r| r.soil_moisture),
    };
    let checks = eval::evaluate(&species.ranges_for(&plant), &conditions);
    let dormancy_active = plant.dormant && species.dormancy.is_some();
    let last_seen = latest.as_ref().map(|r| r.timestamp);
    Ok(PlantStatus {
        status: eval::overall(last_seen, now, offline_after_secs, &checks),
        plant,
        species_info: species,
        dormancy_active,
        last_seen,
        latest,
        checks,
    })
}

/// Status of every plant.
pub fn all_plants(db: &Db, offline_after_secs: i64) -> Result<Vec<PlantStatus>> {
    let species = db.species_list()?;
    db.plants_list()?
        .into_iter()
        .filter_map(|p| {
            // species_id is a foreign key, so this only skips on a broken DB
            let s = species.iter().find(|s| s.id == p.species_id)?.clone();
            Some(plant_status(db, p, s, offline_after_secs))
        })
        .collect()
}
