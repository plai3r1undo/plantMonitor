//! Plants and species: the data edited from the web UI.

use serde::{Deserialize, Serialize};

/// Inclusive optimal range `[min, max]`.
pub type Range = [f64; 2];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Species {
    /// Assigned by the database; ignored on input.
    #[serde(default)]
    pub id: i64,
    pub name: String,
    /// °C
    pub temperature: Range,
    /// % relative humidity
    pub humidity: Range,
    /// Average lux while the plant is lit (see eval::DAYLIGHT_LUX)
    pub light_lux: Range,
    /// % (calibrated from the raw capacitive sensor value)
    pub soil_moisture: Range,
    #[serde(default)]
    pub notes: String,
    /// Optimal ranges while the plant rests (e.g. cactus winter dormancy).
    /// Used instead of the ranges above for plants marked dormant.
    #[serde(default)]
    pub dormancy: Option<Ranges>,
    /// Days the soil usually takes, after watering, to dry down to the
    /// minimum soil moisture. Used to judge whether pot and soil mix suit
    /// the plant (see soil.rs).
    #[serde(default)]
    pub drying_days: Option<Range>,
}

/// One set of optimal ranges, as used for evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ranges {
    pub temperature: Range,
    pub humidity: Range,
    pub light_lux: Range,
    pub soil_moisture: Range,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plant {
    /// Assigned by the database; ignored on input.
    #[serde(default)]
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub location: String,
    /// The sensor node in the pot; none after its sensor moved elsewhere.
    #[serde(default)]
    pub device_id: Option<String>,
    pub species_id: i64,
    /// Raw ADC value of the soil sensor in dry air (0 %)
    pub soil_dry_raw: i64,
    /// Raw ADC value of the soil sensor in water (100 %)
    pub soil_wet_raw: i64,
    /// Send notifications for this plant.
    #[serde(default = "yes")]
    pub alerts: bool,
    /// Resting: evaluate against the species' dormancy ranges, if it has any.
    #[serde(default)]
    pub dormant: bool,
    /// While set and in the future, the soil sensor is out of the pot being
    /// calibrated: its readings aren't added to the plant. Set by the server;
    /// ignored on input.
    #[serde(default, skip_deserializing)]
    pub calibrating_until: Option<i64>,
}

fn yes() -> bool {
    true
}

/// Species seeded into an empty database.
pub fn default_species() -> Vec<Species> {
    #[derive(Deserialize)]
    struct File {
        species: Vec<Species>,
    }
    let file: File = toml::from_str(include_str!("../default_species.toml"))
        .expect("default_species.toml is valid");
    file.species
}

pub fn valid_device_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn check_text(field: &str, value: &str, required: bool, max: usize) -> Result<(), String> {
    let len = value.chars().count();
    if required && value.trim().is_empty() {
        return Err(format!("{field} is required"));
    }
    if len > max {
        return Err(format!("{field} must be at most {max} characters"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{field} contains control characters"));
    }
    Ok(())
}

fn check_range(field: &str, [lo, hi]: Range, min: f64, max: f64) -> Result<(), String> {
    if !lo.is_finite() || !hi.is_finite() || lo < min || hi > max {
        return Err(format!("{field} must be within {min}..{max}"));
    }
    if lo > hi {
        return Err(format!("{field}: minimum is greater than maximum"));
    }
    Ok(())
}

fn check_ranges(prefix: &str, [temperature, humidity, light_lux, soil_moisture]: [Range; 4]) -> Result<(), String> {
    check_range(&format!("{prefix}temperature"), temperature, -30.0, 60.0)?;
    check_range(&format!("{prefix}humidity"), humidity, 0.0, 100.0)?;
    check_range(&format!("{prefix}light_lux"), light_lux, 0.0, 200_000.0)?;
    check_range(&format!("{prefix}soil_moisture"), soil_moisture, 0.0, 100.0)
}

impl Species {
    pub fn validate(&self) -> Result<(), String> {
        check_text("name", &self.name, true, 80)?;
        check_text("notes", &self.notes, false, 500)?;
        check_ranges("", [self.temperature, self.humidity, self.light_lux, self.soil_moisture])?;
        if let Some(d) = &self.dormancy {
            check_ranges("dormancy ", [d.temperature, d.humidity, d.light_lux, d.soil_moisture])?;
        }
        if let Some(d) = self.drying_days {
            check_range("drying_days", d, 0.5, 365.0)?;
        }
        Ok(())
    }

    /// The ranges that apply to `plant` right now: the dormancy ones while it
    /// is marked dormant and the species defines them.
    pub fn ranges_for(&self, plant: &Plant) -> Ranges {
        match &self.dormancy {
            Some(d) if plant.dormant => d.clone(),
            _ => Ranges {
                temperature: self.temperature,
                humidity: self.humidity,
                light_lux: self.light_lux,
                soil_moisture: self.soil_moisture,
            },
        }
    }
}

impl Plant {
    pub fn validate(&self) -> Result<(), String> {
        check_text("name", &self.name, true, 80)?;
        check_text("location", &self.location, false, 80)?;
        if self.device_id.as_deref().is_some_and(|d| !valid_device_id(d)) {
            return Err("device_id must be 1-64 characters of [A-Za-z0-9_-]".into());
        }
        validate_calibration(self.soil_dry_raw, self.soil_wet_raw)
    }
}

/// Raw soil sensor values for dry air and water.
pub fn validate_calibration(dry: i64, wet: i64) -> Result<(), String> {
    for (field, v) in [("soil_dry_raw", dry), ("soil_wet_raw", wet)] {
        if !(0..=4095).contains(&v) {
            return Err(format!("{field} must be between 0 and 4095"));
        }
    }
    if dry == wet {
        return Err("soil_dry_raw and soil_wet_raw must differ".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_species_are_valid() {
        let all = default_species();
        assert!(!all.is_empty());
        for s in all {
            s.validate().unwrap();
        }
    }

    #[test]
    fn dormancy_ranges_apply_only_to_dormant_plants() {
        let all = default_species();
        let cactus = all.iter().find(|s| s.name.starts_with("San Pedro")).unwrap();
        let dormancy = cactus.dormancy.as_ref().unwrap();
        let mut p = Plant {
            id: 0,
            name: "San Pedro".into(),
            location: String::new(),
            device_id: Some("plant-a1b2c3".into()),
            species_id: 1,
            soil_dry_raw: 2800,
            soil_wet_raw: 1200,
            alerts: true,
            dormant: false,
            calibrating_until: None,
        };
        assert_eq!(cactus.ranges_for(&p).temperature, cactus.temperature);
        p.dormant = true;
        assert_eq!(cactus.ranges_for(&p).temperature, dormancy.temperature);
        // A species without dormancy ignores the flag.
        let monstera = all.iter().find(|s| s.dormancy.is_none()).unwrap();
        assert_eq!(monstera.ranges_for(&p).temperature, monstera.temperature);
    }

    #[test]
    fn plant_validation() {
        let mut p = Plant {
            id: 0,
            name: "Monstera".into(),
            location: String::new(),
            device_id: Some("plant-a1b2c3".into()),
            species_id: 1,
            soil_dry_raw: 2800,
            soil_wet_raw: 1200,
            alerts: true,
            dormant: false,
            calibrating_until: None,
        };
        assert!(p.validate().is_ok());
        p.device_id = Some("bad id; drop".into());
        assert!(p.validate().is_err());
        p.device_id = None;
        assert!(p.validate().is_ok());
        p.name = "x\u{0007}".into();
        assert!(p.validate().is_err());
    }
}
