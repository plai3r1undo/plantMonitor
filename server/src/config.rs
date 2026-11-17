use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// Server settings. Plants and species live in the database and are edited
/// from the web UI.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_database")]
    pub database: PathBuf,
    /// Shared secret the sensor nodes send in the `X-Api-Key` header.
    pub device_key: String,
    /// A plant whose last reading is older than this is reported as offline.
    #[serde(default = "default_offline_after")]
    pub offline_after_minutes: i64,
    /// Mark the session cookie `Secure`. Must be true when served over HTTPS;
    /// set false only for plain-HTTP use on a trusted LAN.
    #[serde(default = "yes")]
    pub secure_cookies: bool,
    #[serde(default = "default_session_days")]
    pub session_days: i64,
    /// Address of the reverse proxy. Requests from it have their client IP
    /// taken from `X-Forwarded-For` (used for login rate limiting and logs).
    #[serde(default)]
    pub trusted_proxy: Option<IpAddr>,
    /// Networks (CIDR) allowed to submit sensor readings. Defaults to private
    /// LAN ranges, so the device endpoint is unusable from the internet even
    /// if the web UI is exposed.
    #[serde(default = "default_device_networks")]
    pub device_networks: Vec<Cidr>,
    /// External URL of the web UI, used as the link in notifications.
    #[serde(default)]
    pub public_url: Option<String>,
    /// When to alert; applies to every notification channel. Always set
    /// after `load` (filled from defaults or the legacy `[ntfy]` keys).
    #[serde(default)]
    alerts: Option<Alerts>,
    #[serde(default)]
    pub ntfy: Option<Ntfy>,
    #[serde(default)]
    pub telegram: Option<Telegram>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Alerts {
    /// A problem must persist this long before it is notified (avoids
    /// alerts from short spikes).
    #[serde(default = "default_alert_after")]
    pub alert_after_minutes: i64,
    /// Repeat the alert while the problem persists; 0 = never.
    #[serde(default = "default_remind_after")]
    pub remind_after_hours: i64,
    /// Also notify when a problem clears.
    #[serde(default = "yes")]
    pub notify_recovery: bool,
}

impl Default for Alerts {
    fn default() -> Self {
        Alerts {
            alert_after_minutes: default_alert_after(),
            remind_after_hours: default_remind_after(),
            notify_recovery: true,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ntfy {
    #[serde(default = "default_ntfy_server")]
    pub server: String,
    pub topic: String,
    /// Access token for protected topics / self-hosted servers.
    #[serde(default)]
    pub token: Option<String>,
    // Older configs set the alert timing here; it now lives in [alerts].
    #[serde(default)]
    alert_after_minutes: Option<i64>,
    #[serde(default)]
    remind_after_hours: Option<i64>,
    #[serde(default)]
    notify_recovery: Option<bool>,
}

/// A Telegram bot (create one with @BotFather) that sends the alerts and
/// answers commands such as /status.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Telegram {
    /// Bot token from @BotFather, like `123456789:AAH...`.
    pub bot_token: String,
    /// Chats that receive alerts and may use the bot. Everyone else is
    /// only told their chat ID, so you can add it here.
    #[serde(default)]
    pub chat_ids: Vec<i64>,
}

/// An IP network such as `192.168.1.0/24` or `fd00::/8`. A bare address
/// means a single host.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

impl Cidr {
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = u32::MAX.checked_shl(32 - u32::from(self.prefix)).unwrap_or(0);
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = u128::MAX.checked_shl(128 - u32::from(self.prefix)).unwrap_or(0);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

impl std::str::FromStr for Cidr {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let (addr, prefix) = s.split_once('/').unwrap_or((s, ""));
        let addr: IpAddr = addr.trim().parse().map_err(|_| format!("invalid network address: {s}"))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = if prefix.is_empty() {
            max
        } else {
            prefix.parse().ok().filter(|p| *p <= max).ok_or_else(|| format!("invalid prefix length: {s}"))?
        };
        Ok(Cidr { addr: addr.to_canonical(), prefix })
    }
}

impl<'de> Deserialize<'de> for Cidr {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

fn default_device_networks() -> Vec<Cidr> {
    ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "127.0.0.0/8", "fc00::/7", "fe80::/10", "::1"]
        .iter()
        .map(|s| s.parse().unwrap())
        .collect()
}

fn yes() -> bool {
    true
}
fn default_listen() -> String {
    "127.0.0.1:8080".into()
}
fn default_database() -> PathBuf {
    "plants.db".into()
}
fn default_offline_after() -> i64 {
    30
}
fn default_session_days() -> i64 {
    14
}
fn default_ntfy_server() -> String {
    "https://ntfy.sh".into()
}
fn default_alert_after() -> i64 {
    30
}
fn default_remind_after() -> i64 {
    24
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path.as_ref())?;
        let mut config: Config = toml::from_str(&text).context("parsing TOML")?;
        if let Some(n) = &mut config.ntfy {
            if n.token.as_deref().is_some_and(str::is_empty) {
                n.token = None;
            }
            n.server = n.server.trim_end_matches('/').to_string();
        }
        if config.alerts.is_none() {
            let mut a = Alerts::default();
            if let Some(n) = &config.ntfy {
                a.alert_after_minutes = n.alert_after_minutes.unwrap_or(a.alert_after_minutes);
                a.remind_after_hours = n.remind_after_hours.unwrap_or(a.remind_after_hours);
                a.notify_recovery = n.notify_recovery.unwrap_or(a.notify_recovery);
            }
            config.alerts = Some(a);
        } else if config
            .ntfy
            .as_ref()
            .is_some_and(|n| n.alert_after_minutes.is_some() || n.remind_after_hours.is_some() || n.notify_recovery.is_some())
        {
            bail!("alert timing is set in both [alerts] and [ntfy]; keep it in [alerts] only");
        }
        if let Some(t) = &mut config.telegram {
            t.bot_token = t.bot_token.trim().to_string();
        }
        config.validate()?;
        Ok(config)
    }

    pub fn alerts(&self) -> &Alerts {
        self.alerts.as_ref().expect("set by Config::load")
    }

    /// Whether any notification channel is configured.
    pub fn notifications_enabled(&self) -> bool {
        self.ntfy.is_some() || self.telegram.is_some()
    }

    fn validate(&self) -> Result<()> {
        if self.device_key.len() < 16 {
            bail!("device_key must be at least 16 characters (generate one with: openssl rand -hex 24)");
        }
        if !(1..=365).contains(&self.session_days) {
            bail!("session_days must be between 1 and 365");
        }
        if let Some(n) = &self.ntfy {
            if n.topic.is_empty()
                || n.topic.len() > 64
                || !n.topic.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                bail!("ntfy.topic must be 1-64 characters of [A-Za-z0-9_-]");
            }
            if n.topic.len() < 16 && n.token.is_none() {
                tracing::warn!(
                    "ntfy.topic is short; on public servers anyone who guesses it can read your alerts"
                );
            }
            if !n.server.starts_with("https://") && !n.server.starts_with("http://") {
                bail!("ntfy.server must be an http(s) URL");
            }
        }
        let a = self.alerts();
        if a.alert_after_minutes < 0 || a.remind_after_hours < 0 {
            bail!("alert_after_minutes and remind_after_hours must not be negative");
        }
        if let Some(t) = &self.telegram {
            if !valid_bot_token(&t.bot_token) {
                bail!("telegram.bot_token doesn't look like a token from @BotFather (digits:letters)");
            }
            if t.chat_ids.is_empty() {
                tracing::warn!(
                    "telegram.chat_ids is empty: message your bot, it replies with your chat ID to add to config.toml"
                );
            }
        }
        Ok(())
    }
}

fn valid_bot_token(t: &str) -> bool {
    t.split_once(':').is_some_and(|(id, secret)| {
        !id.is_empty()
            && id.bytes().all(|b| b.is_ascii_digit())
            && secret.len() >= 20
            && secret.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(extra: &str) -> Result<Config> {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("plant-config-test-{}-{n}.toml", std::process::id()));
        std::fs::write(&dir, format!("device_key = \"0123456789abcdef0123\"\n{extra}"))?;
        let c = Config::load(&dir);
        std::fs::remove_file(&dir).ok();
        c
    }

    #[test]
    fn alert_settings() {
        let c = parse("").unwrap();
        assert_eq!(c.alerts().alert_after_minutes, 30);
        assert!(!c.notifications_enabled());

        // Legacy keys in [ntfy] still work.
        let c = parse("[ntfy]\ntopic = \"plants-0123456789abcdef\"\nremind_after_hours = 0\n").unwrap();
        assert_eq!(c.alerts().remind_after_hours, 0);
        assert!(c.notifications_enabled());

        let c = parse("[alerts]\nalert_after_minutes = 5\n").unwrap();
        assert_eq!(c.alerts().alert_after_minutes, 5);
        assert!(parse("[alerts]\n[ntfy]\ntopic = \"plants-0123456789abcdef\"\nremind_after_hours = 1\n").is_err());
    }

    #[test]
    fn telegram_settings() {
        let c = parse("[telegram]\nbot_token = \"123456:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw\"\nchat_ids = [42]\n").unwrap();
        assert_eq!(c.telegram.unwrap().chat_ids, [42]);
        assert!(c.alerts.is_some());
        assert!(parse("[telegram]\nbot_token = \"CHANGE-ME\"\n").is_err());
        assert!(!valid_bot_token("abc:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw"));
        assert!(!valid_bot_token("123456:short"));
    }

    #[test]
    fn cidr_matching() {
        let lan: Cidr = "192.168.1.0/24".parse().unwrap();
        assert!(lan.contains("192.168.1.42".parse().unwrap()));
        assert!(lan.contains("::ffff:192.168.1.42".parse().unwrap()));
        assert!(!lan.contains("192.168.2.1".parse().unwrap()));
        assert!(!lan.contains("fd00::1".parse().unwrap()));
        let host: Cidr = "10.0.0.5".parse().unwrap();
        assert!(host.contains("10.0.0.5".parse().unwrap()));
        assert!(!host.contains("10.0.0.6".parse().unwrap()));
        let all: Cidr = "0.0.0.0/0".parse().unwrap();
        assert!(all.contains("8.8.8.8".parse().unwrap()));
        assert!("10.0.0.0/33".parse::<Cidr>().is_err());
        assert!("nonsense/8".parse::<Cidr>().is_err());

        let defaults = default_device_networks();
        let allowed = |ip: &str| defaults.iter().any(|n| n.contains(ip.parse().unwrap()));
        assert!(allowed("192.168.0.10") && allowed("10.1.2.3") && allowed("fd12::1"));
        assert!(!allowed("8.8.8.8") && !allowed("2a01:4f8::1") && !allowed("172.32.0.1"));
    }
}
