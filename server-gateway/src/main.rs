//! The VelvetDesk gateway.
//!
//! `serve` runs it. `keygen` makes the key licences are signed with, and
//! `mint` writes one. Both of those touch the private half, so both are meant
//! to be run on the operator's own box and nowhere else.

mod config;
mod db;
mod quota;
mod routes;
mod state;
mod translate;

use std::path::PathBuf;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ed25519_dalek::SigningKey;

use crate::config::GatewayConfig;
use crate::db::Db;
use crate::state::AppState;
use vd_license::License;

fn main() -> std::io::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("keygen") => keygen(),
        Some("mint") => mint(&args[1..]),
        Some("serve") | None => serve(),
        Some(other) => {
            eprintln!("unknown command: {other}");
            eprintln!("usage: velvetdesk-gateway [serve|keygen|mint]");
            std::process::exit(2);
        }
    }
}

fn config_path() -> PathBuf {
    std::env::var("VD_GATEWAY_CONFIG")
        .unwrap_or_else(|_| "gateway.json".into())
        .into()
}

fn key_path() -> PathBuf {
    std::env::var("VD_LICENSE_KEY_FILE")
        .unwrap_or_else(|_| "license-signing.key".into())
        .into()
}

#[tokio::main]
async fn serve() -> std::io::Result<()> {
    let cfg = GatewayConfig::load(&config_path()).inspect_err(|err| {
        log::error!("cannot read {}: {err}", config_path().display());
    })?;
    let bind = cfg.bind.clone();
    let db = Db::open(&cfg.db_path).map_err(std::io::Error::other)?;

    for upstream in &cfg.upstreams {
        let keys = upstream.resolved_keys().len();
        if keys == 0 {
            log::warn!("upstream {} has no keys and will be skipped", upstream.id);
        } else {
            log::info!("upstream {} with {keys} key(s)", upstream.id);
        }
    }
    if cfg.license_public_key.trim().is_empty() {
        log::warn!("no license_public_key is configured: every request will be refused");
    }

    let state = AppState::new(cfg, db);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    log::info!("listening on {bind}");
    axum::serve(listener, routes::router(state)).await
}

/// Make the key licences are signed with. The private half goes to a file and
/// is never printed; the public half is what the config and the client carry.
fn keygen() -> std::io::Result<()> {
    let path = key_path();
    if path.exists() {
        // Overwriting it would silently invalidate every licence already
        // issued, and the first anyone would hear of it is a paying operator
        // locked out.
        eprintln!(
            "{} already exists — move it aside yourself if you really mean to replace it",
            path.display()
        );
        std::process::exit(1);
    }
    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    std::fs::write(&path, B64.encode(key.to_bytes()))?;
    restrict(&path);
    println!("private key written to {}", path.display());
    println!(
        "license_public_key: {}",
        B64.encode(key.verifying_key().to_bytes())
    );
    Ok(())
}

/// Write one licence: `mint <license-id> <tier> [days] [max-peers]`.
fn mint(args: &[String]) -> std::io::Result<()> {
    let mut args = args.iter();
    let (Some(license_id), Some(tier)) = (args.next(), args.next()) else {
        eprintln!("usage: velvetdesk-gateway mint <license-id> <tier> [days] [max-peers]");
        std::process::exit(2);
    };
    let days: i64 = args.next().and_then(|d| d.parse().ok()).unwrap_or(30);
    let max_peers: u32 = args.next().and_then(|p| p.parse().ok()).unwrap_or(2);

    let text = std::fs::read_to_string(key_path())?;
    let bytes = B64
        .decode(text.trim())
        .ok()
        .and_then(|raw| <[u8; 32]>::try_from(raw).ok())
        .ok_or_else(|| std::io::Error::other("the signing key file does not hold a key"))?;
    let key = SigningKey::from_bytes(&bytes);

    let license = License {
        license_id: license_id.clone(),
        tier: tier.clone(),
        // Zero days means a licence that does not expire — the operator's own
        // machine, not a customer's.
        expires_at: if days == 0 {
            0
        } else {
            chrono::Utc::now().timestamp() + days * 24 * 3600
        },
        max_peers,
    };
    println!("{}", vd_license::mint(&key, &license));
    Ok(())
}

#[cfg(unix)]
fn restrict(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict(_path: &std::path::Path) {}
