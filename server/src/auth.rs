//! Authentication: password logins with session cookies for the web UI,
//! read-only bearer tokens for scripts, login rate limiting, CSRF checks.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::api::{AppError, AppResult};
use crate::config::Config;
use crate::status::now;
use crate::AppState;

/// Every state-changing request made with a session cookie must carry this
/// header. Browsers can't add custom headers to cross-site requests without a
/// CORS preflight (which we never allow), so this blocks CSRF.
pub const CSRF_HEADER: &str = "x-requested-with";
pub const CSRF_VALUE: &str = "plantmonitor";

pub const MIN_PASSWORD_LEN: usize = 12;
const MAX_PASSWORD_LEN: usize = 256;

const LOGIN_WINDOW_SECS: i64 = 15 * 60;
const LOGIN_FAILURES_PER_IP: u32 = 5;
const LOGIN_FAILURES_GLOBAL: u32 = 30;

/// Who made the request.
#[derive(Clone)]
pub enum Principal {
    User { id: i64, name: String, session_hash: String },
    /// Read-only API token.
    Token,
}

// ------------------------------------------------------------------ crypto

pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("hashing password: {e}"))
}

fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .is_ok_and(|h| Argon2::default().verify_password(password.as_bytes(), &h).is_ok())
}

/// 256-bit random token, hex encoded.
pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    hex(&bytes)
}

/// Tokens are stored hashed so a leaked database doesn't leak live sessions.
pub fn token_hash(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn is_weak_password(password: &str) -> bool {
    password.chars().count() < MIN_PASSWORD_LEN
}

pub fn check_new_password(password: &str) -> Result<(), String> {
    if is_weak_password(password) {
        return Err(format!("password must be at least {MIN_PASSWORD_LEN} characters"));
    }
    check_password_size(password)
}

/// Upper bound only. The CLI uses this when the admin explicitly accepts a
/// weak password; the account is then flagged and the UI nags about it.
pub fn check_password_size(password: &str) -> Result<(), String> {
    if password.is_empty() {
        return Err("password must not be empty".into());
    }
    if password.len() > MAX_PASSWORD_LEN {
        return Err(format!("password must be at most {MAX_PASSWORD_LEN} bytes"));
    }
    Ok(())
}

// ------------------------------------------------------------------ client IP / rate limiting

/// The client's IP: the TCP peer, or the last X-Forwarded-For hop when the
/// peer is our trusted reverse proxy.
pub fn client_ip(config: &Config, peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
    if config.trusted_proxy == Some(peer.ip()) {
        let forwarded = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .and_then(|v| v.trim().parse().ok());
        if let Some(ip) = forwarded {
            return ip;
        }
    }
    peer.ip()
}

/// IPv6 clients usually control a whole /64, so limit by prefix.
fn limit_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) if v6.to_ipv4_mapped().is_none() => {
            let s = v6.segments();
            IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
        }
        IpAddr::V6(v6) => IpAddr::V4(v6.to_ipv4_mapped().unwrap()),
        v4 => v4,
    }
}

#[derive(Default)]
struct Window {
    failures: u32,
    start: i64,
}

impl Window {
    fn current(&mut self, now: i64) -> &mut Self {
        if now - self.start >= LOGIN_WINDOW_SECS {
            *self = Window { failures: 0, start: now };
        }
        self
    }
}

/// Counts failed logins per client and globally in fixed 15-minute windows.
#[derive(Default)]
pub struct LoginLimiter {
    inner: Mutex<(HashMap<IpAddr, Window>, Window)>,
}

impl LoginLimiter {
    pub fn allowed(&self, ip: IpAddr, now: i64) -> bool {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let (per_ip, global) = &mut *guard;
        let ip_ok = per_ip
            .get_mut(&limit_key(ip))
            .is_none_or(|w| w.current(now).failures < LOGIN_FAILURES_PER_IP);
        ip_ok && global.current(now).failures < LOGIN_FAILURES_GLOBAL
    }

    pub fn failed(&self, ip: IpAddr, now: i64) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let (per_ip, global) = &mut *guard;
        if per_ip.len() > 10_000 {
            per_ip.retain(|_, w| now - w.start < LOGIN_WINDOW_SECS);
        }
        per_ip.entry(limit_key(ip)).or_default().current(now).failures += 1;
        global.current(now).failures += 1;
    }

    pub fn succeeded(&self, ip: IpAddr) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.0.remove(&limit_key(ip));
    }
}

// ------------------------------------------------------------------ cookies

fn cookie_name(config: &Config) -> &'static str {
    // The __Host- prefix makes browsers reject the cookie unless it is
    // Secure, host-only and Path=/.
    if config.secure_cookies {
        "__Host-session"
    } else {
        "session"
    }
}

fn session_cookie(config: &Config, value: &str, max_age: i64) -> HeaderValue {
    let secure = if config.secure_cookies { "; Secure" } else { "" };
    let cookie = format!(
        "{}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}",
        cookie_name(config)
    );
    HeaderValue::from_str(&cookie).expect("cookie is ASCII")
}

fn session_token<'a>(config: &Config, headers: &'a HeaderMap) -> Option<&'a str> {
    let name = cookie_name(config);
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

fn has_csrf_header(headers: &HeaderMap) -> bool {
    headers.get(CSRF_HEADER).is_some_and(|v| v == CSRF_VALUE)
}

// ------------------------------------------------------------------ middleware

fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<Option<Principal>> {
    let now = now();
    if let Some(auth) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        let Some(token) = auth.strip_prefix("Bearer ") else { return Ok(None) };
        return Ok(state.db.token_use(&token_hash(token.trim()), now)?.then_some(Principal::Token));
    }
    let Some(token) = session_token(&state.config, headers) else { return Ok(None) };
    let hash = token_hash(token);
    Ok(state
        .db
        .session_user(&hash, now)?
        .map(|(id, name)| Principal::User { id, name, session_hash: hash }))
}

/// Guards every private route.
pub async fn require_auth(State(state): State<Arc<AppState>>, mut req: Request, next: Next) -> Response {
    let principal = match authenticate(&state, req.headers()) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return AppError::Status(StatusCode::UNAUTHORIZED, "not logged in".into()).into_response()
        }
        Err(e) => return AppError::Internal(e).into_response(),
    };
    let read_only = matches!(*req.method(), Method::GET | Method::HEAD);
    if !read_only {
        match principal {
            Principal::Token => {
                return AppError::Status(StatusCode::FORBIDDEN, "API tokens are read-only".into())
                    .into_response()
            }
            Principal::User { .. } if !has_csrf_header(req.headers()) => {
                return AppError::Status(StatusCode::FORBIDDEN, "missing CSRF header".into())
                    .into_response()
            }
            Principal::User { .. } => {}
        }
    }
    req.extensions_mut().insert(principal);
    next.run(req).await
}

/// Endpoints that only a logged-in user (not an API token) may call.
pub fn require_user(p: &Principal) -> AppResult<(i64, &str, &str)> {
    match p {
        Principal::User { id, name, session_hash } => Ok((*id, name, session_hash)),
        Principal::Token => Err(AppError::Status(StatusCode::FORBIDDEN, "needs a logged-in user".into())),
    }
}

// ------------------------------------------------------------------ handlers

#[derive(Deserialize)]
pub struct LoginRequest {
    username: String,
    password: String,
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> AppResult<Response> {
    if !has_csrf_header(&headers) {
        return Err(AppError::Status(StatusCode::FORBIDDEN, "missing CSRF header".into()));
    }
    let ip = client_ip(&state.config, peer, &headers);
    let now = now();
    if !state.limiter.allowed(ip, now) {
        tracing::warn!(%ip, "login rate limit hit");
        return Err(AppError::Status(
            StatusCode::TOO_MANY_REQUESTS,
            "too many failed logins, try again in 15 minutes".into(),
        ));
    }
    if req.username.len() > 64 || req.password.len() > MAX_PASSWORD_LEN {
        return Err(AppError::Status(StatusCode::BAD_REQUEST, "input too long".into()));
    }

    let user = state.db.user_by_name(&req.username)?;
    // Verify against a dummy hash for unknown users so response time doesn't
    // reveal which usernames exist.
    let hash = user.as_ref().map_or(state.dummy_hash.clone(), |u| u.password_hash.clone());
    let ok = {
        // Argon2 is deliberately expensive; cap concurrent verifications.
        let _permit = state.hash_permits.acquire().await?;
        tokio::task::spawn_blocking(move || verify_password(&req.password, &hash)).await?
    };
    let Some(user) = user.filter(|_| ok) else {
        state.limiter.failed(ip, now);
        tracing::warn!(%ip, username = ?req.username, "failed login");
        return Err(AppError::Status(StatusCode::UNAUTHORIZED, "invalid username or password".into()));
    };
    state.limiter.succeeded(ip);

    let token = new_token();
    let max_age = state.config.session_days * 86400;
    state.db.session_create(&token_hash(&token), user.id, now, now + max_age)?;
    tracing::info!(%ip, user = %user.name, "login");
    let mut res =
        Json(serde_json::json!({ "user": user.name, "weak_password": user.weak_password })).into_response();
    res.headers_mut().insert(header::SET_COOKIE, session_cookie(&state.config, &token, max_age));
    Ok(res)
}

pub async fn logout(
    State(state): State<Arc<AppState>>,
    Extension(principal): Extension<Principal>,
) -> AppResult<Response> {
    let (_, _, session_hash) = require_user(&principal)?;
    state.db.session_delete(session_hash)?;
    let mut res = StatusCode::NO_CONTENT.into_response();
    res.headers_mut().insert(header::SET_COOKIE, session_cookie(&state.config, "", 0));
    Ok(res)
}

pub async fn me(
    State(state): State<Arc<AppState>>,
    Extension(principal): Extension<Principal>,
) -> AppResult<Json<serde_json::Value>> {
    Ok(Json(match principal {
        Principal::User { id, name, .. } => {
            let weak = state.db.user_by_id(id)?.is_some_and(|u| u.weak_password);
            serde_json::json!({ "user": name, "via": "session", "weak_password": weak })
        }
        Principal::Token => serde_json::json!({ "user": null, "via": "token" }),
    }))
}

#[derive(Deserialize)]
pub struct PasswordChange {
    current: String,
    new: String,
}

pub async fn change_password(
    State(state): State<Arc<AppState>>,
    Extension(principal): Extension<Principal>,
    Json(req): Json<PasswordChange>,
) -> AppResult<StatusCode> {
    let (user_id, name, session_hash) = require_user(&principal)?;
    check_new_password(&req.new).map_err(|e| AppError::Status(StatusCode::BAD_REQUEST, e))?;
    if req.current.len() > MAX_PASSWORD_LEN {
        return Err(AppError::Status(StatusCode::BAD_REQUEST, "input too long".into()));
    }
    let user = state.db.user_by_id(user_id)?.ok_or_else(|| AppError::Status(StatusCode::UNAUTHORIZED, "user gone".into()))?;
    let ok = {
        let _permit = state.hash_permits.acquire().await?;
        let hash = user.password_hash;
        tokio::task::spawn_blocking(move || verify_password(&req.current, &hash)).await?
    };
    if !ok {
        return Err(AppError::Status(StatusCode::FORBIDDEN, "current password is wrong".into()));
    }
    let new_hash = tokio::task::spawn_blocking(move || hash_password(&req.new)).await??;
    // Log out everywhere else.
    state.db.user_set_password(user_id, &new_hash, false, Some(session_hash))?;
    tracing::info!(user = %name, "password changed");
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_roundtrip() {
        let h = hash_password("correct horse battery").unwrap();
        assert!(verify_password("correct horse battery", &h));
        assert!(!verify_password("wrong", &h));
        assert!(!verify_password("x", "not a hash"));
    }

    #[test]
    fn weak_passwords() {
        assert!(is_weak_password("1234"));
        assert!(!is_weak_password("twelve chars"));
        assert!(check_new_password("1234").is_err());
        assert!(check_password_size("1234").is_ok());
        assert!(check_password_size("").is_err());
    }

    #[test]
    fn limiter_blocks_after_failures() {
        let l = LoginLimiter::default();
        let ip: IpAddr = "203.0.113.7".parse().unwrap();
        for _ in 0..LOGIN_FAILURES_PER_IP {
            assert!(l.allowed(ip, 1000));
            l.failed(ip, 1000);
        }
        assert!(!l.allowed(ip, 1000));
        assert!(l.allowed("203.0.113.8".parse().unwrap(), 1000));
        assert!(l.allowed(ip, 1000 + LOGIN_WINDOW_SECS));
    }

    #[test]
    fn limiter_groups_ipv6_by_prefix() {
        let l = LoginLimiter::default();
        for i in 0..LOGIN_FAILURES_PER_IP {
            l.failed(format!("2001:db8::{i}").parse().unwrap(), 1000);
        }
        assert!(!l.allowed("2001:db8::ffff".parse().unwrap(), 1000));
    }

    #[test]
    fn cookie_parsing() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, "a=1; __Host-session=abc; b=2".parse().unwrap());
        let cfg: Config = toml::from_str("device_key = \"0123456789abcdef\"").unwrap();
        assert_eq!(session_token(&cfg, &h), Some("abc"));
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
