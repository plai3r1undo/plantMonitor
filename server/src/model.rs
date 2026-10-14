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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plant {
    /// Assigned by the database; ignored on input.
    #[serde(default)]
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub location: String,
    pub device_id: String,
    pub species_id: i64,
    /// Raw ADC value of the soil sensor in dry air (0 %)
    pub soil_dry_raw: i64,
    /// Raw ADC value of the soil sensor in water (100 %)
    pub soil_wet_raw: i64,
    /// Send notifications for this plant.
    #[serde(default = "yes")]
    pub alerts: bool,
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

impl Species {
    pub fn validate(&self) -> Result<(), String> {
        check_text("name", &self.name, true, 80)?;
        check_text("notes", &self.notes, false, 500)?;
        check_range("temperature", self.temperature, -30.0, 60.0)?;
        check_range("humidity", self.humidity, 0.0, 100.0)?;
        check_range("light_lux", self.light_lux, 0.0, 200_000.0)?;
        check_range("soil_moisture", self.soil_moisture, 0.0, 100.0)?;
        Ok(())
    }
}

impl Plant {
    pub fn validate(&self) -> Result<(), String> {
        check_text("name", &self.name, true, 80)?;
        check_text("location", &self.location, false, 80)?;
        if !valid_device_id(&self.device_id) {
            return Err("device_id must be 1-64 characters of [A-Za-z0-9_-]".into());
        }
        for (field, v) in [("soil_dry_raw", self.soil_dry_raw), ("soil_wet_raw", self.soil_wet_raw)] {
            if !(0..=4095).contains(&v) {
                return Err(format!("{field} must be between 0 and 4095"));
            }
        }
        if self.soil_dry_raw == self.soil_wet_raw {
            return Err("soil_dry_raw and soil_wet_raw must differ".into());
        }
        Ok(())
    }
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
    fn plant_validation() {
        let mut p = Plant {
            id: 0,
            name: "Monstera".into(),
            location: String::new(),
            device_id: "plant-a1b2c3".into(),
            species_id: 1,
            soil_dry_raw: 2800,
            soil_wet_raw: 1200,
            alerts: true,
        };
        assert!(p.validate().is_ok());
        p.device_id = "bad id; drop".into();
        assert!(p.validate().is_err());
        p.device_id = "ok".into();
        p.name = "x\u{0007}".into();
        assert!(p.validate().is_err());
    }
}
