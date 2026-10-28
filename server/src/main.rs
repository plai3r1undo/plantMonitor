mod alerts;
mod api;
mod auth;
mod config;
mod db;
mod eval;
mod model;
mod soil;
mod status;

use std::io::IsTerminal;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tracing_subscriber::EnvFilter;

pub struct AppState {
    pub config: config::Config,
    pub db: db::Db,
    pub limiter: auth::LoginLimiter,
    /// Verified against for unknown usernames, so timing doesn't leak them.
    pub dummy_hash: String,
    pub hash_permits: tokio::sync::Semaphore,
    pub http: reqwest::Client,
}

const USAGE: &str = "\
Usage: plant-server [-c CONFIG] [COMMAND]

Options:
  -c, --config FILE   Config file (default: $PLANT_CONFIG or ./config.toml)

Commands:
  serve               Run the server (default)
  user-add NAME       Create a web UI user (prompts for the password)
  user-passwd NAME    Change a user's password and log them out everywhere
  user-del NAME       Delete a user
  user-list           List users
";

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("plant_server=info")),
        )
        .with_ansi(std::io::stdout().is_terminal())
        .init();

    let mut config_path = std::env::var("PLANT_CONFIG").unwrap_or_else(|_| "config.toml".into());
    let mut rest = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-c" | "--config" => config_path = args.next().context("--config needs a value")?,
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            _ => rest.push(arg),
        }
    }

    let config = config::Config::load(&config_path).with_context(|| format!("loading config {config_path}"))?;
    let db = db::Db::open(&config.database)
        .with_context(|| format!("opening database {}", config.database.display()))?;

    match rest.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] | ["serve"] => serve(config, db),
        ["user-add", name] => user_add(&db, name),
        ["user-passwd", name] => user_passwd(&db, name),
        ["user-del", name] => {
            if !db.user_delete(name)? {
                bail!("no user '{name}'");
            }
            println!("deleted user '{name}'");
            Ok(())
        }
        ["user-list"] => {
            for name in db.user_names()? {
                println!("{name}");
            }
            Ok(())
        }
        _ => {
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    }
}

/// Returns the password and whether it is weak (the admin confirmed using
/// one shorter than auth::MIN_PASSWORD_LEN).
fn prompt_new_password() -> Result<(String, bool)> {
    let pw = rpassword::prompt_password("New password: ")?;
    auth::check_password_size(&pw).map_err(anyhow::Error::msg)?;
    let weak = auth::is_weak_password(&pw);
    if weak {
        eprintln!(
            "warning: this password is shorter than {} characters and easy to guess.\n\
             The server will log a warning and the web UI will ask you to change it.",
            auth::MIN_PASSWORD_LEN
        );
        eprint!("Use it anyway? [y/N] ");
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            bail!("password must be at least {} characters", auth::MIN_PASSWORD_LEN);
        }
    }
    if rpassword::prompt_password("Repeat password: ")? != pw {
        bail!("passwords don't match");
    }
    Ok((pw, weak))
}

fn user_add(db: &db::Db, name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 || name.chars().any(|c| c.is_control() || c.is_whitespace()) {
        bail!("user name must be 1-64 characters without spaces");
    }
    if db.user_by_name(name)?.is_some() {
        bail!("user '{name}' already exists");
    }
    let (pw, weak) = prompt_new_password()?;
    db.user_create(name, &auth::hash_password(&pw)?, weak)?;
    println!("created user '{name}'");
    Ok(())
}

fn user_passwd(db: &db::Db, name: &str) -> Result<()> {
    let user = db.user_by_name(name)?.with_context(|| format!("no user '{name}'"))?;
    let (pw, weak) = prompt_new_password()?;
    db.user_set_password(user.id, &auth::hash_password(&pw)?, weak, None)?;
    println!("password changed; all sessions of '{name}' ended");
    Ok(())
}

#[tokio::main]
async fn serve(config: config::Config, db: db::Db) -> Result<()> {
    if db.user_names()?.is_empty() {
        tracing::warn!("no users yet; create one with: plant-server user-add <name>");
    }
    for name in db.weak_password_users()? {
        tracing::warn!(
            user = %name,
            "user has a weak password (under {} characters); change it in Settings",
            auth::MIN_PASSWORD_LEN
        );
    }
    if !config.secure_cookies {
        tracing::warn!("secure_cookies = false: only use this on a trusted network without HTTPS");
    }

    let listen = config.listen.clone();
    let state = Arc::new(AppState {
        config,
        db,
        limiter: auth::LoginLimiter::default(),
        dummy_hash: auth::hash_password("not-a-real-password")?,
        hash_permits: tokio::sync::Semaphore::new(2),
        http: alerts::client(),
    });
    tokio::spawn(alerts::run(state.clone()));

    let app = api::router(state);
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    tracing::info!("listening on http://{listen}");

    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
        })
        .await?;
    Ok(())
}
