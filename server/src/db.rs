use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use crate::model::{self, Plant, Range, Ranges, Species};

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
        -- The plant the device was in when the reading arrived; NULL when the
        -- device wasn't assigned or was being calibrated.
        plant_id    INTEGER REFERENCES plants (id) ON DELETE SET NULL,
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
        notes     TEXT NOT NULL DEFAULT '',
        -- Dormancy ranges: all NULL when the species has no dormancy.
        dorm_temp_min  REAL, dorm_temp_max  REAL,
        dorm_hum_min   REAL, dorm_hum_max   REAL,
        dorm_light_min REAL, dorm_light_max REAL,
        dorm_soil_min  REAL, dorm_soil_max  REAL,
        -- Expected drying time in days: both NULL when not set.
        dry_days_min   REAL, dry_days_max   REAL
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

/// Column definitions of the plants table, also used to rebuild it.
const PLANTS_TABLE: &str = "(
        id                INTEGER PRIMARY KEY,
        name              TEXT    NOT NULL,
        location          TEXT    NOT NULL DEFAULT '',
        device_id         TEXT    UNIQUE,
        species_id        INTEGER NOT NULL REFERENCES species (id),
        soil_dry_raw      INTEGER NOT NULL,
        soil_wet_raw      INTEGER NOT NULL,
        alerts            INTEGER NOT NULL DEFAULT 1,
        dormant           INTEGER NOT NULL DEFAULT 0,
        calibrating_until INTEGER
    )";

const READING_COLUMNS: &str = "ts, temperature, humidity, lux, soil_raw, rssi";
const SPECIES_COLUMNS: &str =
    "id, name, temp_min, temp_max, hum_min, hum_max, light_min, light_max, soil_min, soil_max, notes,
     dorm_temp_min, dorm_temp_max, dorm_hum_min, dorm_hum_max,
     dorm_light_min, dorm_light_max, dorm_soil_min, dorm_soil_max, dry_days_min, dry_days_max";
const PLANT_COLUMNS: &str =
    "id, name, location, device_id, species_id, soil_dry_raw, soil_wet_raw, alerts, dormant, calibrating_until";

/// Columns added after the first release, as (table, column, definition).
const ADDED_COLUMNS: &[(&str, &str, &str)] = &[
    ("users", "weak_password", "INTEGER NOT NULL DEFAULT 0"),
    ("plants", "dormant", "INTEGER NOT NULL DEFAULT 0"),
    ("species", "dorm_temp_min", "REAL"),
    ("species", "dorm_temp_max", "REAL"),
    ("species", "dorm_hum_min", "REAL"),
    ("species", "dorm_hum_max", "REAL"),
    ("species", "dorm_light_min", "REAL"),
    ("species", "dorm_light_max", "REAL"),
    ("species", "dorm_soil_min", "REAL"),
    ("species", "dorm_soil_max", "REAL"),
    ("species", "dry_days_min", "REAL"),
    ("species", "dry_days_max", "REAL"),
    ("plants", "calibrating_until", "INTEGER"),
    ("readings", "plant_id", "INTEGER REFERENCES plants (id) ON DELETE SET NULL"),
];

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
        dormancy: map_dormancy(row, 11)?,
        drying_days: match (row.get(19)?, row.get(20)?) {
            (Some(lo), Some(hi)) => Some([lo, hi]),
            _ => None,
        },
    })
}

/// Dormancy ranges stored as 8 nullable columns starting at `offset`.
fn map_dormancy(row: &Row, offset: usize) -> rusqlite::Result<Option<Ranges>> {
    let v: Vec<Option<f64>> = (offset..offset + 8).map(|i| row.get(i)).collect::<rusqlite::Result<_>>()?;
    let Some(v) = v.into_iter().collect::<Option<Vec<f64>>>() else { return Ok(None) };
    Ok(Some(Ranges {
        temperature: [v[0], v[1]],
        humidity: [v[2], v[3]],
        light_lux: [v[4], v[5]],
        soil_moisture: [v[6], v[7]],
    }))
}

fn drying_params(d: Option<Range>) -> [Option<f64>; 2] {
    d.map_or([None; 2], |[lo, hi]| [Some(lo), Some(hi)])
}

/// The 8 dormancy columns' values (all NULL without dormancy).
fn dormancy_params(d: &Option<Ranges>) -> [Option<f64>; 8] {
    match d {
        Some(d) => [
            d.temperature[0], d.temperature[1], d.humidity[0], d.humidity[1],
            d.light_lux[0], d.light_lux[1], d.soil_moisture[0], d.soil_moisture[1],
        ]
        .map(Some),
        None => [None; 8],
    }
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
        dormant: row.get(8)?,
        calibrating_until: row.get(9)?,
    })
}

/// Brings a database from an older release up to date.
fn migrate(conn: &mut Connection) -> Result<()> {
    let mut added = Vec::new();
    for &(table, column, definition) in ADDED_COLUMNS {
        let exists: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info(?1) WHERE name = ?2",
            [table, column],
            |r| r.get(0),
        )?;
        if !exists {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"))?;
            added.push(column);
        }
    }
    // Readings used to belong to devices only: give them to the plant their
    // device is assigned to.
    if added.contains(&"plant_id") {
        conn.execute(
            "UPDATE readings SET plant_id = (SELECT id FROM plants WHERE plants.device_id = readings.device_id)",
            [],
        )?;
    }
    // Default species get their expected drying time.
    if added.contains(&"dry_days_min") {
        for s in model::default_species() {
            let [lo, hi] = drying_params(s.drying_days);
            conn.execute(
                "UPDATE species SET dry_days_min = ?2, dry_days_max = ?3 WHERE name = ?1",
                params![s.name, lo, hi],
            )?;
        }
    }
    // Plants used to require a device. SQLite can't drop a NOT NULL
    // constraint, so the table is rebuilt.
    let device_required: bool = conn.query_row(
        "SELECT \"notnull\" FROM pragma_table_info('plants') WHERE name = 'device_id'",
        [],
        |r| r.get(0),
    )?;
    if device_required {
        let tx = conn.transaction()?;
        tx.execute_batch(&format!(
            "CREATE TABLE plants_new {PLANTS_TABLE};
             INSERT INTO plants_new ({PLANT_COLUMNS}) SELECT {PLANT_COLUMNS} FROM plants;
             DROP TABLE plants;
             ALTER TABLE plants_new RENAME TO plants;"
        ))?;
        tx.commit()?;
    }
    Ok(())
}

/// Frees `device_id` from whichever plant has it, except plant `keep`.
fn release_device(conn: &Connection, device_id: Option<&str>, keep: Option<i64>) -> Result<()> {
    if let Some(device_id) = device_id {
        conn.execute(
            "UPDATE plants SET device_id = NULL, calibrating_until = NULL WHERE device_id = ?1 AND id IS NOT ?2",
            params![device_id, keep],
        )?;
    }
    Ok(())
}

/// SQLite storage. Queries are tiny, so a mutex-guarded connection is plenty.
pub struct Db(Mutex<Connection>);

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let mut conn = Connection::open(path)?;
        // Foreign keys stay off while migrating, so rebuilding the plants
        // table doesn't cascade into readings and alert state.
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = OFF;")?;
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(&format!("CREATE TABLE IF NOT EXISTS plants {PLANTS_TABLE}"))?;
        migrate(&mut conn)?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS readings_plant_ts ON readings (plant_id, ts);
             PRAGMA foreign_keys = ON;",
        )?;
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

    /// Stores a reading for the device and, unless its sensor is being
    /// calibrated, for the plant the device is in.
    pub fn insert_reading(&self, r: &NewReading, ts: i64) -> Result<()> {
        self.conn().execute(
            "INSERT INTO readings (device_id, plant_id, ts, temperature, humidity, lux, soil_raw, rssi)
             VALUES (?1, (SELECT id FROM plants WHERE device_id = ?1
                          AND (calibrating_until IS NULL OR calibrating_until < ?2)),
                     ?2, ?3, ?4, ?5, ?6, ?7)",
            params![r.device_id, ts, r.temperature, r.humidity, r.lux, r.soil_raw, r.rssi],
        )?;
        Ok(())
    }

    pub fn latest(&self, plant_id: i64) -> Result<Option<StoredReading>> {
        let sql = format!(
            "SELECT {READING_COLUMNS} FROM readings WHERE plant_id = ?1 ORDER BY ts DESC LIMIT 1"
        );
        Ok(self.conn().query_row(&sql, [plant_id], |row| map_reading(row, 0)).optional()?)
    }

    pub fn history(&self, plant_id: i64, since: i64) -> Result<Vec<StoredReading>> {
        let sql = format!(
            "SELECT {READING_COLUMNS} FROM readings WHERE plant_id = ?1 AND ts >= ?2 ORDER BY ts"
        );
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![plant_id, since], |row| map_reading(row, 0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// (timestamp, raw soil value) of a plant's readings since `since`.
    pub fn soil_history(&self, plant_id: i64, since: i64) -> Result<Vec<(i64, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT ts, soil_raw FROM readings
             WHERE plant_id = ?1 AND ts >= ?2 AND soil_raw IS NOT NULL ORDER BY ts",
        )?;
        let rows = stmt.query_map(params![plant_id, since], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Mean lux of readings at or above `threshold` since `since`, falling back
    /// to the maximum seen (i.e. the plant never got properly lit).
    pub fn daylight_lux(&self, plant_id: i64, since: i64, threshold: f64) -> Result<Option<f64>> {
        let (avg, max): (Option<f64>, Option<f64>) = self.conn().query_row(
            "SELECT AVG(CASE WHEN lux >= ?3 THEN lux END), MAX(lux)
             FROM readings WHERE plant_id = ?1 AND ts >= ?2",
            params![plant_id, since, threshold],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(avg.or(max))
    }

    /// Devices that are assigned to a plant or have readings of their own.
    /// A device whose readings all belong to plants it has left isn't listed.
    pub fn devices(&self) -> Result<Vec<DeviceSummary>> {
        let sql = "WITH ids AS (SELECT device_id FROM plants WHERE device_id IS NOT NULL
                             UNION SELECT device_id FROM readings WHERE plant_id IS NULL)
             SELECT r.device_id, c.n, r.ts, r.temperature, r.humidity, r.lux, r.soil_raw, r.rssi
             FROM ids
             JOIN (SELECT device_id, COUNT(*) AS n, MAX(id) AS last_id
                   FROM readings GROUP BY device_id) c ON c.device_id = ids.device_id
             JOIN readings r ON r.id = c.last_id
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

    /// Forgets a device: removes it from its plant and deletes its readings
    /// that don't belong to a plant. Plants keep their history.
    pub fn device_delete(&self, device_id: &str) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        release_device(&tx, Some(device_id), None)?;
        tx.execute("DELETE FROM readings WHERE device_id = ?1 AND plant_id IS NULL", [device_id])?;
        tx.commit()?;
        Ok(())
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
        let d = dormancy_params(&s.dormancy);
        let dry = drying_params(s.drying_days);
        let conn = self.conn();
        conn.execute(
            "INSERT INTO species (name, temp_min, temp_max, hum_min, hum_max,
                                  light_min, light_max, soil_min, soil_max, notes,
                                  dorm_temp_min, dorm_temp_max, dorm_hum_min, dorm_hum_max,
                                  dorm_light_min, dorm_light_max, dorm_soil_min, dorm_soil_max,
                                  dry_days_min, dry_days_max)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
            params![
                s.name, s.temperature[0], s.temperature[1], s.humidity[0], s.humidity[1],
                s.light_lux[0], s.light_lux[1], s.soil_moisture[0], s.soil_moisture[1], s.notes,
                d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7], dry[0], dry[1]
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn species_update(&self, id: i64, s: &Species) -> Result<bool> {
        let d = dormancy_params(&s.dormancy);
        let dry = drying_params(s.drying_days);
        let n = self.conn().execute(
            "UPDATE species SET name = ?2, temp_min = ?3, temp_max = ?4, hum_min = ?5, hum_max = ?6,
                    light_min = ?7, light_max = ?8, soil_min = ?9, soil_max = ?10, notes = ?11,
                    dorm_temp_min = ?12, dorm_temp_max = ?13, dorm_hum_min = ?14, dorm_hum_max = ?15,
                    dorm_light_min = ?16, dorm_light_max = ?17, dorm_soil_min = ?18, dorm_soil_max = ?19,
                    dry_days_min = ?20, dry_days_max = ?21
             WHERE id = ?1",
            params![
                id, s.name, s.temperature[0], s.temperature[1], s.humidity[0], s.humidity[1],
                s.light_lux[0], s.light_lux[1], s.soil_moisture[0], s.soil_moisture[1], s.notes,
                d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7], dry[0], dry[1]
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

    /// With `move_device`, the device is first taken from the plant using it.
    pub fn plant_insert(&self, p: &Plant, move_device: bool) -> Result<i64> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if move_device {
            release_device(&tx, p.device_id.as_deref(), None)?;
        }
        tx.execute(
            "INSERT INTO plants (name, location, device_id, species_id, soil_dry_raw, soil_wet_raw, alerts, dormant)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![p.name, p.location, p.device_id, p.species_id, p.soil_dry_raw, p.soil_wet_raw, p.alerts, p.dormant],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    /// With `move_device`, the device is first taken from the plant using it.
    /// Changing the device ends a running calibration.
    pub fn plant_update(&self, id: i64, p: &Plant, move_device: bool) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if move_device {
            release_device(&tx, p.device_id.as_deref(), Some(id))?;
        }
        let n = tx.execute(
            "UPDATE plants SET name = ?2, location = ?3, device_id = ?4, species_id = ?5,
                    soil_dry_raw = ?6, soil_wet_raw = ?7, alerts = ?8, dormant = ?9,
                    calibrating_until = CASE WHEN device_id IS ?4 THEN calibrating_until END
             WHERE id = ?1",
            params![id, p.name, p.location, p.device_id, p.species_id, p.soil_dry_raw, p.soil_wet_raw, p.alerts, p.dormant],
        )?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// Starts (`Some(until)`) or ends (`None`) soil sensor calibration.
    pub fn plant_set_calibrating(&self, id: i64, until: Option<i64>) -> Result<bool> {
        let n = self.conn().execute(
            "UPDATE plants SET calibrating_until = ?2 WHERE id = ?1",
            params![id, until],
        )?;
        Ok(n > 0)
    }

    /// Saves new soil sensor calibration values and ends calibration.
    pub fn plant_set_calibration(&self, id: i64, dry: i64, wet: i64) -> Result<bool> {
        let n = self.conn().execute(
            "UPDATE plants SET soil_dry_raw = ?2, soil_wet_raw = ?3, calibrating_until = NULL WHERE id = ?1",
            params![id, dry, wet],
        )?;
        Ok(n > 0)
    }

    /// The plant's readings are kept, no longer linked to a plant; they show
    /// up under their device again.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("plant-test-{name}-{}.db", std::process::id()));
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
        }
        path
    }

    fn reading(device: &str) -> NewReading {
        NewReading {
            device_id: device.into(),
            temperature: Some(20.0),
            humidity: None,
            lux: None,
            soil_raw: Some(2000),
            rssi: None,
        }
    }

    fn plant(name: &str, device: Option<&str>) -> Plant {
        Plant {
            id: 0,
            name: name.into(),
            location: String::new(),
            device_id: device.map(Into::into),
            species_id: 1,
            soil_dry_raw: 2800,
            soil_wet_raw: 1200,
            alerts: true,
            dormant: false,
            calibrating_until: None,
        }
    }

    #[test]
    fn upgrades_a_database_from_the_first_release() {
        let path = temp_db("upgrade");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE readings (id INTEGER PRIMARY KEY, device_id TEXT NOT NULL, ts INTEGER NOT NULL,
                     temperature REAL, humidity REAL, lux REAL, soil_raw INTEGER, rssi INTEGER);
                 CREATE TABLE species (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE,
                     temp_min REAL NOT NULL, temp_max REAL NOT NULL, hum_min REAL NOT NULL, hum_max REAL NOT NULL,
                     light_min REAL NOT NULL, light_max REAL NOT NULL, soil_min REAL NOT NULL, soil_max REAL NOT NULL,
                     notes TEXT NOT NULL DEFAULT '');
                 CREATE TABLE plants (id INTEGER PRIMARY KEY, name TEXT NOT NULL, location TEXT NOT NULL DEFAULT '',
                     device_id TEXT NOT NULL UNIQUE, species_id INTEGER NOT NULL REFERENCES species (id),
                     soil_dry_raw INTEGER NOT NULL, soil_wet_raw INTEGER NOT NULL, alerts INTEGER NOT NULL DEFAULT 1);
                 CREATE TABLE alert_state (plant_id INTEGER NOT NULL REFERENCES plants (id) ON DELETE CASCADE,
                     metric TEXT NOT NULL, status TEXT NOT NULL, since INTEGER NOT NULL, notified_at INTEGER,
                     PRIMARY KEY (plant_id, metric));
                 INSERT INTO species VALUES (1, 'Basil', 18, 30, 40, 70, 10000, 40000, 45, 75, '');
                 INSERT INTO plants VALUES (7, 'Pot', '', 'dev-a', 1, 2800, 1200, 1);
                 INSERT INTO alert_state VALUES (7, 'soil_moisture', 'low', 100, NULL);
                 INSERT INTO readings (device_id, ts, soil_raw) VALUES ('dev-a', 100, 2000), ('dev-b', 100, 2000);",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        // Readings of the assigned device now belong to its plant.
        assert_eq!(db.history(7, 0).unwrap().len(), 1);
        assert!(db.alert_get(7, "soil_moisture").unwrap().is_some(), "alert state survived the rebuild");
        let basil = db.species_get(1).unwrap().unwrap();
        assert_eq!(basil.drying_days, Some([1.0, 4.0]));
        // Plants may now be without a device.
        let mut p = db.plant_get(7).unwrap().unwrap();
        p.device_id = None;
        assert!(db.plant_update(7, &p, false).unwrap());
        let conn = db.conn();
        let fk: bool = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0)).unwrap();
        assert!(fk);
    }

    #[test]
    fn moving_a_device_keeps_history_with_the_old_plant() {
        let db = Db::open(&temp_db("move")).unwrap();
        let a = db.plant_insert(&plant("A", Some("dev")), false).unwrap();
        db.insert_reading(&reading("dev"), 100).unwrap();
        // Without move_device the second plant can't take it.
        assert!(db.plant_insert(&plant("B", Some("dev")), false).is_err());
        let b = db.plant_insert(&plant("B", Some("dev")), true).unwrap();
        db.insert_reading(&reading("dev"), 200).unwrap();
        assert_eq!(db.plant_get(a).unwrap().unwrap().device_id, None);
        assert_eq!(db.history(a, 0).unwrap().iter().map(|r| r.ts).collect::<Vec<_>>(), [100]);
        assert_eq!(db.history(b, 0).unwrap().iter().map(|r| r.ts).collect::<Vec<_>>(), [200]);
        assert_eq!(db.devices().unwrap().len(), 1);
    }

    #[test]
    fn calibration_readings_stay_out_of_the_plant() {
        let db = Db::open(&temp_db("calib")).unwrap();
        let a = db.plant_insert(&plant("A", Some("dev")), false).unwrap();
        db.plant_set_calibrating(a, Some(1000)).unwrap();
        db.insert_reading(&reading("dev"), 500).unwrap();
        assert!(db.history(a, 0).unwrap().is_empty());
        assert_eq!(db.devices().unwrap()[0].latest.ts, 500, "the device still shows the reading");
        db.plant_set_calibration(a, 3300, 1400).unwrap();
        db.insert_reading(&reading("dev"), 600).unwrap();
        assert_eq!(db.history(a, 0).unwrap().len(), 1);
        assert_eq!(db.plant_get(a).unwrap().unwrap().soil_dry_raw, 3300);
    }

    #[test]
    fn deleting_a_device_keeps_plant_history() {
        let db = Db::open(&temp_db("delete")).unwrap();
        db.insert_reading(&reading("dev"), 50).unwrap(); // before it was assigned
        let a = db.plant_insert(&plant("A", Some("dev")), false).unwrap();
        db.insert_reading(&reading("dev"), 100).unwrap();
        db.device_delete("dev").unwrap();
        assert!(db.devices().unwrap().is_empty());
        assert_eq!(db.plant_get(a).unwrap().unwrap().device_id, None);
        assert_eq!(db.history(a, 0).unwrap().len(), 1);
    }
}
