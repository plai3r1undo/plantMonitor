use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use crate::model::{self, Plant, Species};

/// A reading as sent by a sensor node. Fields are optional so a single
/// failed sensor doesn't drop the whole measurement.
#[derive(Debug, Deserialize)]
pub struct NewReading {
    pub device_id: String,
    pub temperature: Option<f64>,
    pub humidity: Option<f64>,
    pub lux: Option<f64>,
    pub soil_raw: Option<i64>,
    pub rssi: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct StoredReading {
    pub ts: i64,
    pub temperature: Option<f64>,
    pub humidity: Option<f64>,
    pub lux: Option<f64>,
    pub soil_raw: Option<i64>,
    pub rssi: Option<i64>,
}

pub struct DeviceSummary {
    pub device_id: String,
    pub readings: i64,
    pub latest: StoredReading,
}

pub struct User {
    pub id: i64,
    pub name: String,
    pub password_hash: String,
    /// Set with a password shorter than auth::MIN_PASSWORD_LEN (CLI only).
    pub weak_password: bool,
}

#[derive(Serialize)]
pub struct TokenInfo {
    pub id: i64,
    pub name: String,
    pub created: i64,
    pub last_used: Option<i64>,
}

pub struct AlertState {
    pub status: String,
    pub since: i64,
    pub notified_at: Option<i64>,
}

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS readings (
        id          INTEGER PRIMARY KEY,
        device_id   TEXT    NOT NULL,
        ts          INTEGER NOT NULL,
        temperature REAL,
        humidity    REAL,
        lux         REAL,
        soil_raw    INTEGER,
        rssi        INTEGER
    );
    CREATE INDEX IF NOT EXISTS readings_device_ts ON readings (device_id, ts);

    CREATE TABLE IF NOT EXISTS species (
        id        INTEGER PRIMARY KEY,
        name      TEXT NOT NULL UNIQUE,
        temp_min  REAL NOT NULL, temp_max  REAL NOT NULL,
        hum_min   REAL NOT NULL, hum_max   REAL NOT NULL,
        light_min REAL NOT NULL, light_max REAL NOT NULL,
        soil_min  REAL NOT NULL, soil_max  REAL NOT NULL,
        notes     TEXT NOT NULL DEFAULT ''
    );

    CREATE TABLE IF NOT EXISTS plants (
        id           INTEGER PRIMARY KEY,
        name         TEXT    NOT NULL,
        location     TEXT    NOT NULL DEFAULT '',
        device_id    TEXT    NOT NULL UNIQUE,
        species_id   INTEGER NOT NULL REFERENCES species (id),
        soil_dry_raw INTEGER NOT NULL,
        soil_wet_raw INTEGER NOT NULL,
        alerts       INTEGER NOT NULL DEFAULT 1
    );

    CREATE TABLE IF NOT EXISTS users (
        id            INTEGER PRIMARY KEY,
        name          TEXT NOT NULL UNIQUE,
        password_hash TEXT NOT NULL,
        weak_password INTEGER NOT NULL DEFAULT 0
    );

    CREATE TABLE IF NOT EXISTS sessions (
        token_hash TEXT    PRIMARY KEY,
        user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
        created    INTEGER NOT NULL,
        expires    INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS api_tokens (
        id         INTEGER PRIMARY KEY,
        name       TEXT    NOT NULL,
        token_hash TEXT    NOT NULL UNIQUE,
        created    INTEGER NOT NULL,
        last_used  INTEGER
    );

    CREATE TABLE IF NOT EXISTS alert_state (
        plant_id    INTEGER NOT NULL REFERENCES plants (id) ON DELETE CASCADE,
        metric      TEXT    NOT NULL,
        status      TEXT    NOT NULL,
        since       INTEGER NOT NULL,
        notified_at INTEGER,
        PRIMARY KEY (plant_id, metric)
    );
";

const READING_COLUMNS: &str = "ts, temperature, humidity, lux, soil_raw, rssi";
const SPECIES_COLUMNS: &str =
    "id, name, temp_min, temp_max, hum_min, hum_max, light_min, light_max, soil_min, soil_max, notes";
const PLANT_COLUMNS: &str =
    "id, name, location, device_id, species_id, soil_dry_raw, soil_wet_raw, alerts";

fn map_user(r: &Row) -> rusqlite::Result<User> {
    Ok(User { id: r.get(0)?, name: r.get(1)?, password_hash: r.get(2)?, weak_password: r.get(3)? })
}

fn map_reading(row: &Row, offset: usize) -> rusqlite::Result<StoredReading> {
    Ok(StoredReading {
        ts: row.get(offset)?,
        temperature: row.get(offset + 1)?,
        humidity: row.get(offset + 2)?,
        lux: row.get(offset + 3)?,
        soil_raw: row.get(offset + 4)?,
        rssi: row.get(offset + 5)?,
    })
}

fn map_species(row: &Row) -> rusqlite::Result<Species> {
    Ok(Species {
        id: row.get(0)?,
        name: row.get(1)?,
        temperature: [row.get(2)?, row.get(3)?],
        humidity: [row.get(4)?, row.get(5)?],
        light_lux: [row.get(6)?, row.get(7)?],
        soil_moisture: [row.get(8)?, row.get(9)?],
        notes: row.get(10)?,
    })
}

fn map_plant(row: &Row) -> rusqlite::Result<Plant> {
    Ok(Plant {
        id: row.get(0)?,
        name: row.get(1)?,
        location: row.get(2)?,
        device_id: row.get(3)?,
        species_id: row.get(4)?,
        soil_dry_raw: row.get(5)?,
        soil_wet_raw: row.get(6)?,
        alerts: row.get(7)?,
    })
}

/// SQLite storage. Queries are tiny, so a mutex-guarded connection is plenty.
pub struct Db(Mutex<Connection>);

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(SCHEMA)?;
        // Added after the first release; older databases lack the column.
        let has_weak: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('users') WHERE name = 'weak_password'",
            [],
            |r| r.get(0),
        )?;
        if !has_weak {
            conn.execute_batch("ALTER TABLE users ADD COLUMN weak_password INTEGER NOT NULL DEFAULT 0")?;
        }
        let db = Self(Mutex::new(conn));
        if db.species_list()?.is_empty() {
            for s in model::default_species() {
                db.species_insert(&s)?;
            }
        }
        Ok(db)
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ------------------------------------------------------------ readings

    pub fn insert_reading(&self, r: &NewReading, ts: i64) -> Result<()> {
        self.conn().execute(
            "INSERT INTO readings (device_id, ts, temperature, humidity, lux, soil_raw, rssi)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![r.device_id, ts, r.temperature, r.humidity, r.lux, r.soil_raw, r.rssi],
        )?;
        Ok(())
    }

    pub fn latest(&self, device_id: &str) -> Result<Option<StoredReading>> {
        let sql = format!(
            "SELECT {READING_COLUMNS} FROM readings WHERE device_id = ?1 ORDER BY ts DESC LIMIT 1"
        );
        Ok(self.conn().query_row(&sql, [device_id], |row| map_reading(row, 0)).optional()?)
    }

    pub fn history(&self, device_id: &str, since: i64) -> Result<Vec<StoredReading>> {
        let sql = format!(
            "SELECT {READING_COLUMNS} FROM readings WHERE device_id = ?1 AND ts >= ?2 ORDER BY ts"
        );
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![device_id, since], |row| map_reading(row, 0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Mean lux of readings at or above `threshold` since `since`, falling back
    /// to the maximum seen (i.e. the plant never got properly lit).
    pub fn daylight_lux(&self, device_id: &str, since: i64, threshold: f64) -> Result<Option<f64>> {
        let (avg, max): (Option<f64>, Option<f64>) = self.conn().query_row(
            "SELECT AVG(CASE WHEN lux >= ?3 THEN lux END), MAX(lux)
             FROM readings WHERE device_id = ?1 AND ts >= ?2",
            params![device_id, since, threshold],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(avg.or(max))
    }

    pub fn devices(&self) -> Result<Vec<DeviceSummary>> {
        let sql = "SELECT r.device_id, c.n, r.ts, r.temperature, r.humidity, r.lux, r.soil_raw, r.rssi
             FROM readings r
             JOIN (SELECT device_id, COUNT(*) AS n, MAX(id) AS last_id
                   FROM readings GROUP BY device_id) c ON r.id = c.last_id
             ORDER BY r.device_id";
        let conn = self.conn();
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], |row| {
            Ok(DeviceSummary {
                device_id: row.get(0)?,
                readings: row.get(1)?,
                latest: map_reading(row, 2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ------------------------------------------------------------ species

    pub fn species_list(&self) -> Result<Vec<Species>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!("SELECT {SPECIES_COLUMNS} FROM species ORDER BY name"))?;
        let rows = stmt.query_map([], map_species)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn species_get(&self, id: i64) -> Result<Option<Species>> {
        let sql = format!("SELECT {SPECIES_COLUMNS} FROM species WHERE id = ?1");
        Ok(self.conn().query_row(&sql, [id], map_species).optional()?)
    }

    pub fn species_insert(&self, s: &Species) -> Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO species (name, temp_min, temp_max, hum_min, hum_max,
                                  light_min, light_max, soil_min, soil_max, notes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                s.name, s.temperature[0], s.temperature[1], s.humidity[0], s.humidity[1],
                s.light_lux[0], s.light_lux[1], s.soil_moisture[0], s.soil_moisture[1], s.notes
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn species_update(&self, id: i64, s: &Species) -> Result<bool> {
        let n = self.conn().execute(
            "UPDATE species SET name = ?2, temp_min = ?3, temp_max = ?4, hum_min = ?5, hum_max = ?6,
                    light_min = ?7, light_max = ?8, soil_min = ?9, soil_max = ?10, notes = ?11
             WHERE id = ?1",
            params![
                id, s.name, s.temperature[0], s.temperature[1], s.humidity[0], s.humidity[1],
                s.light_lux[0], s.light_lux[1], s.soil_moisture[0], s.soil_moisture[1], s.notes
            ],
        )?;
        Ok(n > 0)
    }

    /// Fails with a constraint violation while plants still use the species.
    pub fn species_delete(&self, id: i64) -> Result<bool> {
        Ok(self.conn().execute("DELETE FROM species WHERE id = ?1", [id])? > 0)
    }

    // ------------------------------------------------------------ plants

    pub fn plants_list(&self) -> Result<Vec<Plant>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!("SELECT {PLANT_COLUMNS} FROM plants ORDER BY name"))?;
        let rows = stmt.query_map([], map_plant)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn plant_get(&self, id: i64) -> Result<Option<Plant>> {
        let sql = format!("SELECT {PLANT_COLUMNS} FROM plants WHERE id = ?1");
        Ok(self.conn().query_row(&sql, [id], map_plant).optional()?)
    }

    pub fn plant_insert(&self, p: &Plant) -> Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO plants (name, location, device_id, species_id, soil_dry_raw, soil_wet_raw, alerts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![p.name, p.location, p.device_id, p.species_id, p.soil_dry_raw, p.soil_wet_raw, p.alerts],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn plant_update(&self, id: i64, p: &Plant) -> Result<bool> {
        let n = self.conn().execute(
            "UPDATE plants SET name = ?2, location = ?3, device_id = ?4, species_id = ?5,
                    soil_dry_raw = ?6, soil_wet_raw = ?7, alerts = ?8
             WHERE id = ?1",
            params![id, p.name, p.location, p.device_id, p.species_id, p.soil_dry_raw, p.soil_wet_raw, p.alerts],
        )?;
        Ok(n > 0)
    }

    /// Readings are kept (they belong to the device, not the plant).
    pub fn plant_delete(&self, id: i64) -> Result<bool> {
        Ok(self.conn().execute("DELETE FROM plants WHERE id = ?1", [id])? > 0)
    }

    // ------------------------------------------------------------ users

    pub fn user_by_name(&self, name: &str) -> Result<Option<User>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id, name, password_hash, weak_password FROM users WHERE name = ?1",
                [name],
                map_user,
            )
            .optional()?)
    }

    pub fn user_by_id(&self, id: i64) -> Result<Option<User>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id, name, password_hash, weak_password FROM users WHERE id = ?1",
                [id],
                map_user,
            )
            .optional()?)
    }

    pub fn user_names(&self) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT name FROM users ORDER BY name")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Users whose password is shorter than auth::MIN_PASSWORD_LEN.
    pub fn weak_password_users(&self) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT name FROM users WHERE weak_password ORDER BY name")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn user_create(&self, name: &str, password_hash: &str, weak: bool) -> Result<()> {
        self.conn().execute(
            "INSERT INTO users (name, password_hash, weak_password) VALUES (?1, ?2, ?3)",
            params![name, password_hash, weak],
        )?;
        Ok(())
    }

    /// Changes the password and ends every session of the user except `keep_session`.
    pub fn user_set_password(&self, id: i64, password_hash: &str, weak: bool, keep_session: Option<&str>) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE users SET password_hash = ?2, weak_password = ?3 WHERE id = ?1",
            params![id, password_hash, weak],
        )?;
        conn.execute(
            "DELETE FROM sessions WHERE user_id = ?1 AND token_hash IS NOT ?2",
            params![id, keep_session],
        )?;
        Ok(())
    }

    pub fn user_delete(&self, name: &str) -> Result<bool> {
        Ok(self.conn().execute("DELETE FROM users WHERE name = ?1", [name])? > 0)
    }

    // ------------------------------------------------------------ sessions

    pub fn session_create(&self, token_hash: &str, user_id: i64, now: i64, expires: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute("DELETE FROM sessions WHERE expires < ?1", [now])?;
        conn.execute(
            "INSERT INTO sessions (token_hash, user_id, created, expires) VALUES (?1, ?2, ?3, ?4)",
            params![token_hash, user_id, now, expires],
        )?;
        Ok(())
    }

    /// Returns (user id, user name) for a live session.
    pub fn session_user(&self, token_hash: &str, now: i64) -> Result<Option<(i64, String)>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT u.id, u.name FROM sessions s JOIN users u ON u.id = s.user_id
                 WHERE s.token_hash = ?1 AND s.expires >= ?2",
                params![token_hash, now],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub fn session_delete(&self, token_hash: &str) -> Result<()> {
        self.conn().execute("DELETE FROM sessions WHERE token_hash = ?1", [token_hash])?;
        Ok(())
    }

    // ------------------------------------------------------------ API tokens

    pub fn token_create(&self, name: &str, token_hash: &str, now: i64) -> Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO api_tokens (name, token_hash, created) VALUES (?1, ?2, ?3)",
            params![name, token_hash, now],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn token_list(&self) -> Result<Vec<TokenInfo>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT id, name, created, last_used FROM api_tokens ORDER BY created")?;
        let rows = stmt.query_map([], |r| {
            Ok(TokenInfo { id: r.get(0)?, name: r.get(1)?, created: r.get(2)?, last_used: r.get(3)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn token_delete(&self, id: i64) -> Result<bool> {
        Ok(self.conn().execute("DELETE FROM api_tokens WHERE id = ?1", [id])? > 0)
    }

    /// Checks a token and records its use.
    pub fn token_use(&self, token_hash: &str, now: i64) -> Result<bool> {
        let n = self.conn().execute(
            "UPDATE api_tokens SET last_used = ?2 WHERE token_hash = ?1",
            params![token_hash, now],
        )?;
        Ok(n > 0)
    }

    // ------------------------------------------------------------ alert state

    pub fn alert_get(&self, plant_id: i64, metric: &str) -> Result<Option<AlertState>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT status, since, notified_at FROM alert_state WHERE plant_id = ?1 AND metric = ?2",
                params![plant_id, metric],
                |r| Ok(AlertState { status: r.get(0)?, since: r.get(1)?, notified_at: r.get(2)? }),
            )
            .optional()?)
    }

    pub fn alert_set(&self, plant_id: i64, metric: &str, s: &AlertState) -> Result<()> {
        self.conn().execute(
            "INSERT INTO alert_state (plant_id, metric, status, since, notified_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (plant_id, metric) DO UPDATE
             SET status = excluded.status, since = excluded.since, notified_at = excluded.notified_at",
            params![plant_id, metric, s.status, s.since, s.notified_at],
        )?;
        Ok(())
    }
}
