//! Telegram bot: sends the alerts and answers commands such as /status.
//!
//! Uses long polling (`getUpdates`), so only outgoing HTTPS to
//! api.telegram.org is needed; nothing has to be reachable from outside.
//! Only chats listed in `telegram.chat_ids` get answers or alerts.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;

use crate::alerts;
use crate::eval::{Overall, Status};
use crate::soil::Verdict;
use crate::status::{self, PlantStatus};
use crate::AppState;

const API: &str = "https://api.telegram.org";
/// How long one getUpdates call waits for messages.
const POLL_SECS: u64 = 50;
/// Messages older than this (sent while the server was down) are ignored.
const MAX_AGE_SECS: i64 = 600;
/// Telegram's limit is 4096 characters; bytes are a safe upper bound.
const MAX_MESSAGE: usize = 4000;

const HELP: &str = "\
🌱 <b>Plant monitor</b>

/status — all plants at a glance
/plant <i>name</i> — readings and advice for one plant
/mute <i>name</i> — stop alerts for a plant
/unmute <i>name</i> — resume alerts for a plant

You can also just send a plant's name.";

// ---------------------------------------------------------------- Bot API

#[derive(Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
    error_code: Option<u16>,
    parameters: Option<ResponseParameters>,
}

#[derive(Deserialize)]
struct ResponseParameters {
    retry_after: Option<u64>,
}

#[derive(Debug)]
struct ApiError {
    method: String,
    code: Option<u16>,
    description: String,
    retry_after: Option<u64>,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Telegram {}: {}", self.method, self.description)?;
        if let Some(code) = self.code {
            write!(f, " ({code})")?;
        }
        Ok(())
    }
}

impl std::error::Error for ApiError {}

#[derive(Deserialize)]
struct Update {
    update_id: i64,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    chat: Chat,
    date: i64,
    text: Option<String>,
}

#[derive(Deserialize)]
struct Chat {
    id: i64,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct User {
    username: Option<String>,
}

async fn call<T: DeserializeOwned>(
    state: &AppState,
    method: &str,
    body: &serde_json::Value,
    timeout: Option<Duration>,
) -> Result<T> {
    let tg = state.config.telegram.as_ref().context("telegram is not configured")?;
    let url = format!("{API}/bot{}/{method}", tg.bot_token);
    let mut req = state.http.post(&url).json(body);
    if let Some(t) = timeout {
        req = req.timeout(t);
    }
    // without_url(): the URL contains the bot token, keep it out of logs.
    let res = req.send().await.map_err(reqwest::Error::without_url)?;
    let status = res.status();
    let r: ApiResponse<T> = res
        .json()
        .await
        .map_err(reqwest::Error::without_url)
        .with_context(|| format!("Telegram {method}: HTTP {status}"))?;
    if !r.ok {
        return Err(ApiError {
            method: method.to_string(),
            code: r.error_code,
            description: r.description.unwrap_or_else(|| "request failed".into()),
            retry_after: r.parameters.and_then(|p| p.retry_after),
        }
        .into());
    }
    r.result.with_context(|| format!("Telegram {method}: response without result"))
}

/// Sends `html` to one chat, split into several messages if it is long.
async fn send_message(state: &AppState, chat_id: i64, html: &str, silent: bool) -> Result<()> {
    for chunk in split_message(html) {
        let body = json!({
            "chat_id": chat_id,
            "text": chunk,
            "parse_mode": "HTML",
            "disable_notification": silent,
            "link_preview_options": { "is_disabled": true },
        });
        call::<serde_json::Value>(state, "sendMessage", &body, None).await?;
    }
    Ok(())
}

/// Sends an alert to every configured chat. Fails only if no chat got it.
pub async fn notify(state: &AppState, title: &str, message: &str, silent: bool) -> Result<()> {
    let Some(tg) = &state.config.telegram else { bail!("telegram is not configured") };
    if tg.chat_ids.is_empty() {
        bail!("telegram.chat_ids is empty");
    }
    let text = format!("<b>{}</b>\n{}", esc(title), esc(message));
    let mut last_err = None;
    let mut delivered = false;
    for &chat in &tg.chat_ids {
        match send_message(state, chat, &text, silent).await {
            Ok(()) => delivered = true,
            Err(e) => {
                tracing::warn!(chat, "sending Telegram alert: {e:#}");
                last_err = Some(e);
            }
        }
    }
    match last_err {
        Some(e) if !delivered => Err(e),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------- polling

/// An invalid or revoked token: retrying won't help.
fn is_fatal(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ApiError>().is_some_and(|e| matches!(e.code, Some(401 | 404)))
}

fn retry_delay(e: &anyhow::Error, backoff: u64) -> Duration {
    let wait = e.downcast_ref::<ApiError>().and_then(|e| e.retry_after).unwrap_or(backoff);
    Duration::from_secs(wait.clamp(1, 300))
}

async fn setup(state: &AppState) -> Result<String> {
    let me: User = call(state, "getMe", &json!({}), None).await?;
    // getUpdates doesn't work while a webhook is set.
    call::<bool>(state, "deleteWebhook", &json!({}), None).await?;
    let commands = json!({ "commands": [
        { "command": "status", "description": "All plants at a glance" },
        { "command": "plant", "description": "Readings and advice for one plant" },
        { "command": "mute", "description": "Stop alerts for a plant" },
        { "command": "unmute", "description": "Resume alerts for a plant" },
        { "command": "help", "description": "What this bot can do" },
    ]});
    call::<bool>(state, "setMyCommands", &commands, None).await?;
    Ok(me.username.unwrap_or_default())
}

pub async fn run(state: Arc<AppState>) {
    let Some(tg) = &state.config.telegram else { return };
    let mut backoff = 1;
    loop {
        match setup(&state).await {
            Ok(name) => {
                tracing::info!("Telegram bot @{name} ready");
                break;
            }
            Err(e) if is_fatal(&e) => {
                tracing::error!("Telegram bot disabled, check telegram.bot_token: {e:#}");
                return;
            }
            Err(e) => {
                tracing::warn!("starting Telegram bot: {e:#}");
                tokio::time::sleep(retry_delay(&e, backoff)).await;
                backoff = (backoff * 2).min(60);
            }
        }
    }

    let mut offset = 0;
    let mut backoff = 1;
    // Chats outside chat_ids that were already told their ID.
    let mut told = HashSet::new();
    loop {
        let body = json!({ "offset": offset, "timeout": POLL_SECS, "allowed_updates": ["message"] });
        let updates: Vec<Update> =
            match call(&state, "getUpdates", &body, Some(Duration::from_secs(POLL_SECS + 15))).await {
                Ok(u) => {
                    backoff = 1;
                    u
                }
                Err(e) if is_fatal(&e) => {
                    tracing::error!("Telegram bot stopped, check telegram.bot_token: {e:#}");
                    return;
                }
                Err(e) => {
                    tracing::warn!("polling Telegram: {e:#}");
                    tokio::time::sleep(retry_delay(&e, backoff)).await;
                    backoff = (backoff * 2).min(60);
                    continue;
                }
            };
        for u in updates {
            offset = offset.max(u.update_id + 1);
            let Some(m) = u.message else { continue };
            if let Err(e) = handle(&state, tg, m, &mut told).await {
                tracing::warn!("answering Telegram message: {e:#}");
            }
        }
    }
}

async fn handle(
    state: &AppState,
    tg: &crate::config::Telegram,
    m: Message,
    told: &mut HashSet<i64>,
) -> Result<()> {
    let Some(text) = m.text else { return Ok(()) };
    if status::now() - m.date > MAX_AGE_SECS {
        return Ok(());
    }
    let chat = m.chat.id;
    if !tg.chat_ids.contains(&chat) {
        tracing::info!(chat, "Telegram message from a chat not in telegram.chat_ids");
        // Answer once per chat, and never in groups, so the bot can't be
        // used to spam.
        if m.chat.kind == "private" && told.len() < 1000 && told.insert(chat) {
            let reply = format!(
                "This bot is private. If it's yours, add <code>{chat}</code> to <code>chat_ids</code> \
                 in the <code>[telegram]</code> section of config.toml and restart the server."
            );
            send_message(state, chat, &reply, false).await?;
        }
        return Ok(());
    }
    let reply = respond(state, &text)?;
    send_message(state, chat, &reply, false).await
}

// ---------------------------------------------------------------- commands

/// Splits "/cmd@bot args" into ("cmd", "args"); plain text gives ("", text).
fn parse_command(text: &str) -> (String, &str) {
    let text = text.trim();
    match text.strip_prefix('/') {
        Some(rest) => {
            let (cmd, arg) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            let cmd = cmd.split('@').next().unwrap_or_default();
            (cmd.to_lowercase(), arg.trim())
        }
        None => (String::new(), text),
    }
}

fn respond(state: &AppState, text: &str) -> Result<String> {
    let (cmd, arg) = parse_command(text);
    let plants = status::all_plants(&state.db, state.config.offline_after_minutes * 60)?;
    let names: Vec<&str> = plants.iter().map(|p| p.plant.name.as_str()).collect();
    Ok(match cmd.as_str() {
        "start" | "help" => HELP.to_string(),
        "status" => status_text(&plants),
        "plant" | "mute" | "unmute" if arg.is_empty() => {
            format!("Which plant? Send <code>/{cmd} name</code>.\n\n{}", plant_names(&names))
        }
        "plant" | "" => match find(&names, arg) {
            Ok(i) => plant_text(&plants[i], state.config.public_url.as_deref()),
            Err(msg) => msg,
        },
        "mute" | "unmute" => match find(&names, arg) {
            Ok(i) => {
                let on = cmd == "unmute";
                let id = plants[i].plant.id;
                let mut plant = state.db.plant_get(id)?.context("plant was deleted")?;
                plant.alerts = on;
                state.db.plant_update(id, &plant, false)?;
                tracing::info!(plant = %plant.name, alerts = on, "alerts changed from Telegram");
                if on {
                    format!("🔔 Alerts on for <b>{}</b>.", esc(&plant.name))
                } else {
                    format!("🔕 Alerts off for <b>{}</b>. /unmute to turn them back on.", esc(&plant.name))
                }
            }
            Err(msg) => msg,
        },
        _ => format!("Unknown command.\n\n{HELP}"),
    })
}

fn plant_names(names: &[&str]) -> String {
    if names.is_empty() {
        "No plants yet. Add them in the web UI.".into()
    } else {
        names.iter().map(|n| format!("• {}", esc(n))).collect::<Vec<_>>().join("\n")
    }
}

/// Index of the plant matching `query`: an exact name (any case), or the
/// only name containing it.
fn find(names: &[&str], query: &str) -> Result<usize, String> {
    let q = query.trim().to_lowercase();
    if let Some(i) = names.iter().position(|n| n.to_lowercase() == q) {
        return Ok(i);
    }
    let hits: Vec<usize> = (0..names.len()).filter(|&i| names[i].to_lowercase().contains(&q)).collect();
    match hits.as_slice() {
        [i] => Ok(*i),
        [] => Err(format!(
            "No plant called “{}”. Your plants:\n\n{}\n\n/help lists the commands.",
            esc(query),
            plant_names(names)
        )),
        _ => {
            let several: Vec<&str> = hits.iter().map(|&i| names[i]).collect();
            Err(format!("Which one?\n\n{}", plant_names(&several)))
        }
    }
}

fn status_icon(s: Overall) -> &'static str {
    match s {
        Overall::Ok => "✅",
        Overall::Attention => "⚠️",
        Overall::Offline => "📴",
        Overall::NoData => "⏳",
        Overall::NoSensor => "➖",
    }
}

fn metric_icon(metric: &str) -> &'static str {
    match metric {
        "soil_moisture" => "💧",
        "light_lux" => "☀️",
        "temperature" => "🌡️",
        "humidity" => "💨",
        _ => "•",
    }
}

fn value(v: f64, unit: &str) -> String {
    if unit == "lx" {
        format!("{v:.0} {unit}")
    } else {
        format!("{v:.1} {unit}")
    }
}

fn ago(ts: i64, now: i64) -> String {
    let s = (now - ts).max(0);
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86400 => format!("{} h ago", s / 3600),
        _ => format!("{} d ago", s / 86400),
    }
}

fn status_text(plants: &[PlantStatus]) -> String {
    if plants.is_empty() {
        return "No plants yet. Add them in the web UI.".into();
    }
    let now = status::now();
    let attention = plants.iter().filter(|p| !matches!(p.status, Overall::Ok)).count();
    let mut out = vec![match attention {
        0 => "🌿 <b>All plants are fine</b>".to_string(),
        1 => "🌿 <b>1 plant needs a look</b>".to_string(),
        n => format!("🌿 <b>{n} plants need a look</b>"),
    }];
    out.push(String::new());
    for p in plants {
        let summary = match p.status {
            Overall::Ok => "ok".to_string(),
            Overall::Attention => p
                .checks
                .iter()
                .filter(|c| matches!(c.status, Status::Low | Status::High))
                .map(|c| {
                    let dir = if c.status == Status::Low { "low" } else { "high" };
                    format!("{} {dir} ({})", alerts::label(c.metric), value(c.value.unwrap_or_default(), c.unit))
                })
                .collect::<Vec<_>>()
                .join(", "),
            Overall::Offline => format!("offline, last seen {}", p.last_seen.map_or("never".into(), |t| ago(t, now))),
            Overall::NoData => "waiting for the first reading".to_string(),
            Overall::NoSensor => "no sensor assigned".to_string(),
        };
        let muted = if p.plant.alerts { "" } else { " 🔕" };
        out.push(format!("{} <b>{}</b>{muted} — {}", status_icon(p.status), esc(&p.plant.name), esc(&summary)));
    }
    out.join("\n")
}

fn plant_text(p: &PlantStatus, public_url: Option<&str>) -> String {
    let now = status::now();
    let mut out = vec![format!("<b>{}</b>", esc(&p.plant.name))];
    let mut sub = vec![p.species_info.name.as_str()];
    if !p.plant.location.is_empty() {
        sub.push(&p.plant.location);
    }
    if p.dormancy_active {
        sub.push("Dormant");
    }
    out.push(format!("<i>{}</i>", esc(&sub.join(" · "))));
    let state = match p.status {
        Overall::Ok => "All good",
        Overall::Attention => "Needs attention",
        Overall::Offline => "Sensor offline",
        Overall::NoData => "No readings yet",
        Overall::NoSensor => "No sensor assigned",
    };
    let seen = p.last_seen.map(|t| format!(" · reading {}", ago(t, now))).unwrap_or_default();
    out.push(format!("{} {state}{seen}", status_icon(p.status)));
    out.push(String::new());

    if p.last_seen.is_some() {
        for c in &p.checks {
            let range = format!("{}–{} {}", c.min, c.max, c.unit);
            let line = match (c.status, c.value) {
                (Status::Off, _) => "switched off".to_string(),
                (_, None) => format!("— (optimal {range})"),
                (s, Some(v)) => {
                    let mark = match s {
                        Status::Low => " ⬇️ low",
                        Status::High => " ⬆️ high",
                        _ => "",
                    };
                    format!("{} (optimal {range}){mark}", value(v, c.unit))
                }
            };
            out.push(format!("{} {}: {}", metric_icon(c.metric), esc(c.label), esc(&line)));
        }
        let advice: Vec<String> =
            p.checks.iter().filter_map(|c| c.advice).map(|a| format!("• {}", esc(a))).collect();
        if !advice.is_empty() {
            out.push(String::new());
            out.extend(advice);
        }
    }
    if p.plant.soil_sensor && !p.soil_drying.message.is_empty() {
        let icon = if matches!(p.soil_drying.verdict, Verdict::Slow | Verdict::Fast) { "⚠️" } else { "🪴" };
        out.push(String::new());
        out.push(format!("{icon} Soil drying: {}", esc(&p.soil_drying.message)));
    }
    if !p.plant.alerts {
        out.push(String::new());
        out.push("🔕 Alerts are off for this plant (/unmute).".into());
    }
    if let Some(url) = public_url {
        out.push(String::new());
        out.push(format!("<a href=\"{}\">Open dashboard</a>", esc(url)));
    }
    out.join("\n")
}

/// Escapes text for Telegram's HTML parse mode.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Splits on line breaks into messages of at most MAX_MESSAGE bytes. Our
/// HTML tags never span lines, so every chunk stays valid.
fn split_message(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for line in text.lines() {
        let mut line = line;
        // A single overlong line (unlikely): cut it at a char boundary.
        while line.len() > MAX_MESSAGE {
            let mut cut = MAX_MESSAGE;
            while !line.is_char_boundary(cut) {
                cut -= 1;
            }
            if !cur.is_empty() {
                chunks.push(std::mem::take(&mut cur));
            }
            chunks.push(line[..cut].to_string());
            line = &line[cut..];
        }
        if !cur.is_empty() && cur.len() + 1 + line.len() > MAX_MESSAGE {
            chunks.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push('\n');
        }
        cur.push_str(line);
    }
    if !cur.trim().is_empty() {
        chunks.push(cur);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands() {
        assert_eq!(parse_command("/status"), ("status".into(), ""));
        assert_eq!(parse_command("/Plant@my_bot  Big Ficus "), ("plant".into(), "Big Ficus"));
        assert_eq!(parse_command(" ficus "), ("".into(), "ficus"));
    }

    #[test]
    fn plant_lookup() {
        let names = ["Ficus", "Ficus small", "Monstera"];
        assert_eq!(find(&names, "ficus"), Ok(0));
        assert_eq!(find(&names, "mon"), Ok(2));
        assert_eq!(find(&names, "SMALL"), Ok(1));
        assert!(find(&names, "cus").unwrap_err().starts_with("Which one?"));
        assert!(find(&names, "<cactus>").unwrap_err().contains("&lt;cactus&gt;"));
    }

    #[test]
    fn formatting() {
        assert_eq!(esc("a<b> & \"c\""), "a&lt;b&gt; &amp; &quot;c&quot;");
        assert_eq!(ago(1000, 1030), "just now");
        assert_eq!(ago(0, 7200), "2 h ago");
        assert_eq!(value(1234.4, "lx"), "1234 lx");
        assert_eq!(value(21.25, "°C"), "21.2 °C");
    }

    #[test]
    fn splitting() {
        assert_eq!(split_message("a\nb"), ["a\nb"]);
        let line = "x".repeat(3000);
        let text = format!("{line}\n{line}\n{line}");
        let chunks = split_message(&text);
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| c.len() <= MAX_MESSAGE));
        let long = "é".repeat(5000);
        assert!(split_message(&long).iter().all(|c| c.len() <= MAX_MESSAGE));
        assert_eq!(split_message(&long).concat(), long);
    }

    #[test]
    fn api_errors() {
        let e: anyhow::Error = ApiError {
            method: "getUpdates".into(),
            code: Some(401),
            description: "Unauthorized".into(),
            retry_after: None,
        }
        .into();
        assert!(is_fatal(&e));
        let e: anyhow::Error = ApiError {
            method: "sendMessage".into(),
            code: Some(429),
            description: "Too Many Requests".into(),
            retry_after: Some(7),
        }
        .into();
        assert!(!is_fatal(&e));
        assert_eq!(retry_delay(&e, 1), Duration::from_secs(7));
    }
}
