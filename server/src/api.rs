use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use tower_http::timeout::TimeoutLayer;

use crate::auth::{self, constant_time_eq, require_user, Principal};
use crate::db::NewReading;
use crate::model::{valid_device_id, validate_calibration, Plant, Species};
use crate::status::{self, now, ReadingOut};
use crate::{alerts, AppState};

pub type AppResult<T> = Result<T, AppError>;

pub fn router(state: Arc<AppState>) -> Router {
    let private = Router::new()
        .route("/api/me", get(auth::me))
        .route("/api/logout", post(auth::logout))
        .route("/api/password", post(auth::change_password))
        .route("/api/plants", get(list_plants).post(create_plant))
        .route("/api/plants/{id}", get(get_plant).put(update_plant).delete(delete_plant))
        .route("/api/plants/{id}/readings", get(plant_readings))
        .route(
            "/api/plants/{id}/calibration",
            post(start_calibration).put(finish_calibration).delete(cancel_calibration),
        )
        .route("/api/species", get(list_species).post(create_species))
        .route("/api/species/{id}", put(update_species).delete(delete_species))
        .route("/api/devices", get(list_devices))
        .route("/api/devices/{id}", delete(delete_device))
        .route("/api/tokens", get(list_tokens).post(create_token))
        .route("/api/tokens/{id}", delete(delete_token))
        .route("/api/notifications", get(notification_status))
        .route("/api/notifications/test", post(test_notification))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_auth));

    Router::new()
        .route("/", get(|| static_file("text/html; charset=utf-8", include_str!("../static/index.html"))))
        .route("/style.css", get(|| static_file("text/css; charset=utf-8", include_str!("../static/style.css"))))
        .route("/app.js", get(|| static_file("text/javascript; charset=utf-8", include_str!("../static/app.js"))))
        .route("/api/health", get(|| async { Json(serde_json::json!({ "status": "ok" })) }))
        .route("/api/login", post(auth::login))
        .route("/api/readings", post(post_reading))
        .merge(private)
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, Duration::from_secs(30)))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

async fn static_file(content_type: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

/// Defence-in-depth headers on every response. The CSP allows only our own
/// scripts and styles, so injected markup can't run code.
async fn security_headers(req: Request, next: Next) -> Response {
    let is_api = req.uri().path().starts_with("/api/");
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    let set = |h: &mut HeaderMap, k: &'static str, v: &'static str| {
        h.insert(k, HeaderValue::from_static(v));
    };
    set(
        h,
        "content-security-policy",
        "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
         connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
    );
    set(h, "x-content-type-options", "nosniff");
    set(h, "x-frame-options", "DENY");
    set(h, "referrer-policy", "no-referrer");
    set(h, "cross-origin-opener-policy", "same-origin");
    set(h, "cross-origin-resource-policy", "same-origin");
    set(h, "permissions-policy", "camera=(), microphone=(), geolocation=(), interest-cohort=()");
    if is_api {
        set(h, "cache-control", "no-store");
    }
    res
}

// ---------------------------------------------------------------- errors

pub enum AppError {
    Status(StatusCode, String),
    Internal(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        let e = e.into();
        // Unique / foreign-key violations are user errors, not server errors.
        if let Some(rusqlite::Error::SqliteFailure(f, _)) = e.downcast_ref::<rusqlite::Error>() {
            if f.code == rusqlite::ErrorCode::ConstraintViolation {
                return AppError::Status(
                    StatusCode::CONFLICT,
                    "conflicts with existing data (duplicate name/device, or still in use)".into(),
                );
            }
        }
        AppError::Internal(e)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (code, msg) = match self {
            AppError::Status(code, msg) => (code, msg),
            AppError::Internal(e) => {
                tracing::error!("{e:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

fn not_found(what: &str) -> AppError {
    AppError::Status(StatusCode::NOT_FOUND, format!("{what} not found"))
}

fn bad_request(msg: String) -> AppError {
    AppError::Status(StatusCode::BAD_REQUEST, msg)
}

// ---------------------------------------------------------------- ingest

async fn post_reading(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(reading): Json<NewReading>,
) -> AppResult<StatusCode> {
    let ip = auth::client_ip(&state.config, peer, &headers);
    if !state.config.device_networks.iter().any(|n| n.contains(ip)) {
        tracing::warn!(%ip, "reading rejected: address not in device_networks");
        return Err(AppError::Status(StatusCode::FORBIDDEN, "not allowed from this network".into()));
    }
    let sent = headers.get("x-api-key").map(|v| v.as_bytes()).unwrap_or_default();
    if !constant_time_eq(sent, state.config.device_key.as_bytes()) {
        return Err(AppError::Status(StatusCode::UNAUTHORIZED, "invalid device key".into()));
    }
    if !valid_device_id(&reading.device_id) {
        return Err(bad_request("device_id must be 1-64 chars of [A-Za-z0-9_-]".into()));
    }
    let finite = [reading.temperature, reading.humidity, reading.lux]
        .iter()
        .all(|v| v.is_none_or(f64::is_finite));
    if !finite {
        return Err(bad_request("values must be finite numbers".into()));
    }
    state.db.insert_reading(&reading, now())?;
    tracing::debug!(?reading, "stored reading");
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- plants

fn offline_secs(state: &AppState) -> i64 {
    state.config.offline_after_minutes * 60
}

async fn list_plants(State(state): State<Arc<AppState>>) -> AppResult<Response> {
    Ok(Json(status::all_plants(&state.db, offline_secs(&state))?).into_response())
}

fn load_plant(state: &AppState, id: i64) -> AppResult<(Plant, Species)> {
    let plant = state.db.plant_get(id)?.ok_or_else(|| not_found("plant"))?;
    let species = state.db.species_get(plant.species_id)?.ok_or_else(|| not_found("species"))?;
    Ok((plant, species))
}

async fn get_plant(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult<Response> {
    let (plant, species) = load_plant(&state, id)?;
    Ok(Json(status::plant_status(&state.db, plant, species, offline_secs(&state))?).into_response())
}

#[derive(Deserialize)]
struct PlantInput {
    #[serde(flatten)]
    plant: Plant,
    /// Take the device from the plant that has it. Its history stays with
    /// that plant, which is left without a sensor.
    #[serde(default)]
    move_device: bool,
}

/// `id` is the plant being edited, if any.
fn validate_plant(state: &AppState, input: &PlantInput, id: Option<i64>) -> AppResult<()> {
    let p = &input.plant;
    p.validate().map_err(bad_request)?;
    if state.db.species_get(p.species_id)?.is_none() {
        return Err(bad_request("unknown species".into()));
    }
    if let Some(device) = &p.device_id {
        let owner = state.db.plants_list()?.into_iter().find(|o| o.device_id.as_ref() == Some(device));
        if let Some(owner) = owner.filter(|o| Some(o.id) != id && !input.move_device) {
            return Err(AppError::Status(
                StatusCode::CONFLICT,
                format!("device {device} is in plant “{}”; move it from there", owner.name),
            ));
        }
    }
    Ok(())
}

async fn create_plant(State(state): State<Arc<AppState>>, Json(input): Json<PlantInput>) -> AppResult<Response> {
    validate_plant(&state, &input, None)?;
    let id = state.db.plant_insert(&input.plant, input.move_device)?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "id": id }))).into_response())
}

async fn update_plant(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(input): Json<PlantInput>,
) -> AppResult<StatusCode> {
    validate_plant(&state, &input, Some(id))?;
    if !state.db.plant_update(id, &input.plant, input.move_device)? {
        return Err(not_found("plant"));
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- calibration

/// How long a calibration may run before readings go to the plant again.
const CALIBRATION_SECS: i64 = 30 * 60;

/// Takes the soil sensor out of service: until calibration ends (or times
/// out), its readings don't reach the plant's history, status or alerts.
async fn start_calibration(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult<Response> {
    let plant = state.db.plant_get(id)?.ok_or_else(|| not_found("plant"))?;
    if plant.device_id.is_none() {
        return Err(bad_request("this plant has no sensor".into()));
    }
    let until = now() + CALIBRATION_SECS;
    state.db.plant_set_calibrating(id, Some(until))?;
    Ok(Json(serde_json::json!({ "until": until })).into_response())
}

#[derive(Deserialize)]
struct Calibration {
    soil_dry_raw: i64,
    soil_wet_raw: i64,
}

async fn finish_calibration(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(c): Json<Calibration>,
) -> AppResult<StatusCode> {
    validate_calibration(c.soil_dry_raw, c.soil_wet_raw).map_err(bad_request)?;
    if !state.db.plant_set_calibration(id, c.soil_dry_raw, c.soil_wet_raw)? {
        return Err(not_found("plant"));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn cancel_calibration(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult<StatusCode> {
    if !state.db.plant_set_calibrating(id, None)? {
        return Err(not_found("plant"));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_plant(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult<StatusCode> {
    if !state.db.plant_delete(id)? {
        return Err(not_found("plant"));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct HistoryQuery {
    /// How far back to go (default 24, max one year).
    hours: Option<i64>,
    /// `json` (default) or `csv`.
    format: Option<String>,
}

async fn plant_readings(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Query(q): Query<HistoryQuery>,
) -> AppResult<Response> {
    let plant = state.db.plant_get(id)?.ok_or_else(|| not_found("plant"))?;
    let hours = q.hours.unwrap_or(24).clamp(1, 24 * 366);
    let rows = state.db.history(plant.id, now() - hours * 3600)?;
    let readings: Vec<_> = rows.iter().map(|r| ReadingOut::new(r, Some(&plant))).collect();

    match q.format.as_deref() {
        None | Some("json") => Ok(Json(readings).into_response()),
        Some("csv") => {
            let mut out = String::from("timestamp,temperature,humidity,lux,soil_moisture,soil_raw,rssi\n");
            let f = |v: Option<f64>| v.map(|v| v.to_string()).unwrap_or_default();
            let i = |v: Option<i64>| v.map(|v| v.to_string()).unwrap_or_default();
            for r in &readings {
                out += &format!(
                    "{},{},{},{},{},{},{}\n",
                    r.timestamp,
                    f(r.temperature),
                    f(r.humidity),
                    f(r.lux),
                    f(r.soil_moisture),
                    i(r.soil_raw),
                    i(r.rssi)
                );
            }
            Ok(([(header::CONTENT_TYPE, "text/csv; charset=utf-8")], out).into_response())
        }
        Some(_) => Err(bad_request("unknown format, use json or csv".into())),
    }
}

// ---------------------------------------------------------------- species

async fn list_species(State(state): State<Arc<AppState>>) -> AppResult<Response> {
    Ok(Json(state.db.species_list()?).into_response())
}

async fn create_species(State(state): State<Arc<AppState>>, Json(s): Json<Species>) -> AppResult<Response> {
    s.validate().map_err(bad_request)?;
    let id = state.db.species_insert(&s)?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "id": id }))).into_response())
}

async fn update_species(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(s): Json<Species>,
) -> AppResult<StatusCode> {
    s.validate().map_err(bad_request)?;
    if !state.db.species_update(id, &s)? {
        return Err(not_found("species"));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_species(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult<StatusCode> {
    if !state.db.species_delete(id)? {
        return Err(not_found("species"));
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- devices

#[derive(Serialize)]
struct DeviceOut {
    device_id: String,
    /// Plant this device is assigned to, if any.
    plant_id: Option<i64>,
    plant_name: Option<String>,
    readings: i64,
    last_seen: i64,
    /// Reported within offline_after_minutes.
    online: bool,
    latest: ReadingOut,
}

async fn list_devices(State(state): State<Arc<AppState>>) -> AppResult<Response> {
    let plants = state.db.plants_list()?;
    let now = now();
    let devices: Vec<_> = state
        .db
        .devices()?
        .into_iter()
        .map(|d| {
            let plant = plants.iter().find(|p| p.device_id.as_ref() == Some(&d.device_id));
            DeviceOut {
                plant_id: plant.map(|p| p.id),
                plant_name: plant.map(|p| p.name.clone()),
                readings: d.readings,
                last_seen: d.latest.ts,
                online: now - d.latest.ts <= offline_secs(&state),
                latest: ReadingOut::new(&d.latest, plant),
                device_id: d.device_id,
            }
        })
        .collect();
    Ok(Json(devices).into_response())
}

/// Forgets an offline device (e.g. a broken or retired sensor). Its plant
/// keeps its history; readings that belong to no plant are deleted. If it
/// reports again, it shows up as a new unassigned device.
async fn delete_device(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> AppResult<StatusCode> {
    let device = state.db.devices()?.into_iter().find(|d| d.device_id == id).ok_or_else(|| not_found("device"))?;
    if now() - device.latest.ts <= offline_secs(&state) {
        return Err(AppError::Status(
            StatusCode::CONFLICT,
            "this device is still sending readings; only offline devices can be deleted".into(),
        ));
    }
    state.db.device_delete(&id)?;
    tracing::info!(device = %id, "device deleted");
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- API tokens

async fn list_tokens(
    State(state): State<Arc<AppState>>,
    Extension(p): Extension<Principal>,
) -> AppResult<Response> {
    require_user(&p)?;
    Ok(Json(state.db.token_list()?).into_response())
}

#[derive(Deserialize)]
struct NewToken {
    name: String,
}

async fn create_token(
    State(state): State<Arc<AppState>>,
    Extension(p): Extension<Principal>,
    Json(req): Json<NewToken>,
) -> AppResult<Response> {
    let (_, user, _) = require_user(&p)?;
    let name = req.name.trim();
    if name.is_empty() || name.chars().count() > 60 || name.chars().any(char::is_control) {
        return Err(bad_request("token name must be 1-60 characters".into()));
    }
    let token = auth::new_token();
    let id = state.db.token_create(name, &auth::token_hash(&token), now())?;
    tracing::info!(%user, token = name, "API token created");
    // The only time the token is ever shown.
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "id": id, "token": token }))).into_response())
}

async fn delete_token(
    State(state): State<Arc<AppState>>,
    Extension(p): Extension<Principal>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    require_user(&p)?;
    if !state.db.token_delete(id)? {
        return Err(not_found("token"));
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- notifications

async fn notification_status(
    State(state): State<Arc<AppState>>,
    Extension(p): Extension<Principal>,
) -> AppResult<Json<serde_json::Value>> {
    require_user(&p)?;
    Ok(Json(match &state.config.ntfy {
        None => serde_json::json!({ "enabled": false }),
        Some(n) => serde_json::json!({
            "enabled": true,
            "server": n.server,
            "topic": n.topic,
            "alert_after_minutes": n.alert_after_minutes,
            "remind_after_hours": n.remind_after_hours,
        }),
    }))
}

async fn test_notification(State(state): State<Arc<AppState>>) -> AppResult<StatusCode> {
    if state.config.ntfy.is_none() {
        return Err(bad_request("notifications are not configured ([ntfy] in config.toml)".into()));
    }
    alerts::send(
        &state,
        "🌱 Plant monitor test".into(),
        "Notifications are working.".into(),
        3,
        vec!["potted_plant"],
    )
    .await
    .map_err(|e| AppError::Status(StatusCode::BAD_GATEWAY, format!("sending failed: {e}")))?;
    Ok(StatusCode::NO_CONTENT)
}
