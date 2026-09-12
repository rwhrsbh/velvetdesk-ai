//! The admin side: upstreams, keys, models, tiers, licences and what they
//! have cost.
//!
//! Everything here is behind one token in `VD_ADMIN_TOKEN`, and everything
//! here writes to the database and then reloads the registry — so a key added
//! in a browser is in the pool for the next request, without a restart.
//!
//! Keys are the one thing that only goes in. They are listed masked, deleted
//! by id, and never handed back out: an admin page that can read them is one
//! stolen token away from being the provider's problem too.

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Html;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ed25519_dalek::SigningKey;

use crate::config::Tier;
use crate::quota::{WINDOW_5H, WINDOW_WEEK};
use crate::registry::{now, ModelRow, UpstreamRow};
use crate::routes::ApiError;
use crate::state::AppState;
use vd_license::License;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin", get(page))
        .route("/admin/overview", get(overview))
        .route("/admin/upstreams", get(list_upstreams).post(save_upstream))
        .route("/admin/upstreams/{id}", delete(delete_upstream))
        .route("/admin/upstreams/{id}/keys", get(list_keys).post(add_key))
        .route("/admin/keys/{id}", delete(delete_key))
        .route("/admin/models", get(list_models).post(save_model))
        .route("/admin/models/{name}", delete(delete_model))
        .route("/admin/tiers", get(list_tiers).post(save_tier))
        .route("/admin/tiers/{name}", delete(delete_tier))
        .route("/admin/licenses", get(list_licenses).post(mint_license))
        .route("/admin/revoke", post(revoke))
        .route("/admin/unrevoke", post(unrevoke))
        .route("/admin/stats", get(stats))
}

/// The token check. An admin section with no token set is closed, not open:
/// forgetting to set one must not be the same as publishing the keys.
fn admin(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    if state.admin_token.is_empty() {
        return Err(ApiError::Forbidden(
            "the admin endpoints are closed: VD_ADMIN_TOKEN is not set".into(),
        ));
    }
    let sent = headers
        .get("x-vd-admin")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    // Compared in full every time rather than byte by byte with an early
    // exit, so the answer takes the same shape whatever was sent.
    if sent.len() != state.admin_token.len()
        || sent
            .bytes()
            .zip(state.admin_token.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            != 0
    {
        return Err(ApiError::Unauthorized("admin token does not match".into()));
    }
    Ok(())
}

async fn page() -> Html<&'static str> {
    Html(include_str!("admin.html"))
}

// ------------------------------------------------------------------ shapes

#[derive(Debug, Deserialize)]
struct KeyBody {
    key: String,
}

#[derive(Debug, Deserialize)]
struct TierBody {
    name: String,
    #[serde(flatten)]
    tier: Tier,
}

#[derive(Debug, Deserialize)]
struct MintBody {
    license_id: String,
    tier: String,
    #[serde(default)]
    days: i64,
    #[serde(default)]
    max_peers: u32,
    #[serde(default)]
    note: String,
}

#[derive(Debug, Deserialize)]
struct LicenseBody {
    license_id: String,
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Deserialize)]
struct Window {
    #[serde(default)]
    hours: Option<i64>,
}

// ------------------------------------------------------------------ routes

/// Everything the first screen needs, in one request.
async fn overview(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    let registry = state.registry.read();
    let upstreams: Vec<&UpstreamRow> = registry.upstreams.iter().collect();
    Ok(Json(json!({
        "credit_usd": state.cfg.credit_usd,
        "licensing": !state.cfg.license_public_key.trim().is_empty(),
        "upstreams": upstreams,
        "models": registry.models,
        "tiers": registry.tiers,
        // The waiting room, so an operator watching a slow evening can see
        // whether the gateway is queueing or the upstream is simply slow.
        "queue": {
            "inflight": state.queue.inflight(),
            "queued": state.queue.queued(),
            "max_inflight": state.queue.limits().inflight,
            "max_per_license": state.queue.limits().per_license,
            "max_queued": state.queue.limits().queued,
        },
    })))
}

async fn list_upstreams(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    Ok(Json(json!(state.registry.read().upstreams)))
}

async fn save_upstream(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<UpstreamRow>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    if body.id.trim().is_empty() || body.base_url.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "an id and a base_url are required".into(),
        ));
    }
    state.db.save_upstream(&body)?;
    state.reload()?;
    Ok(Json(json!({ "saved": body.id })))
}

async fn delete_upstream(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    state.db.delete_upstream(&id)?;
    state.reload()?;
    Ok(Json(json!({ "deleted": id })))
}

/// The keys of one upstream, with how each is faring.
///
/// The masks come from the database and the tallies from the live pool, in
/// the same order — the pool is built from that list. Without this the page
/// shows a row of identical-looking keys and no hint of which one the
/// provider has been refusing all morning.
async fn list_keys(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    let rows = state.db.list_keys(&id)?;
    let status = state
        .registry
        .read()
        .pool(&id)
        .map(|pool| pool.status())
        .unwrap_or_default();
    let merged: Vec<Value> = rows
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            let live = status.get(index);
            json!({
                "id": row.id,
                "masked": row.masked,
                "added_at": row.added_at,
                "successes": live.map(|s| s.successes).unwrap_or(0),
                "failures": live.map(|s| s.failures).unwrap_or(0),
                "cooling_seconds": live.map(|s| s.cooling_seconds).unwrap_or(0),
                "last_error": live.and_then(|s| s.last_error.clone()),
            })
        })
        .collect();
    Ok(Json(json!(merged)))
}

async fn add_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<KeyBody>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    // One paste, many keys: a pool is usually assembled somewhere else and
    // arrives as a list.
    let added: Vec<&str> = body
        .key
        .split([',', '\n'])
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .collect();
    if added.is_empty() {
        return Err(ApiError::BadRequest("no key was sent".into()));
    }
    for key in &added {
        state.db.add_key(&id, key, now())?;
    }
    state.reload()?;
    Ok(Json(json!({ "added": added.len() })))
}

async fn delete_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    state.db.delete_key(id)?;
    state.reload()?;
    Ok(Json(json!({ "deleted": id })))
}

async fn list_models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    Ok(Json(json!(state.registry.read().models)))
}

async fn save_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ModelRow>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    if body.name.trim().is_empty() {
        return Err(ApiError::BadRequest("a model needs a name".into()));
    }
    let known = state
        .registry
        .read()
        .upstreams
        .iter()
        .any(|up| up.id == body.upstream_id);
    if !known {
        return Err(ApiError::BadRequest(format!(
            "there is no upstream called {}",
            body.upstream_id
        )));
    }
    state.db.save_model(&body)?;
    state.reload()?;
    Ok(Json(json!({ "saved": body.name })))
}

async fn delete_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    state.db.delete_model(&name)?;
    state.reload()?;
    Ok(Json(json!({ "deleted": name })))
}

async fn list_tiers(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    Ok(Json(json!(state.registry.read().tiers)))
}

async fn save_tier(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<TierBody>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    if body.name.trim().is_empty() {
        return Err(ApiError::BadRequest("a tier needs a name".into()));
    }
    state.db.save_tier(body.name.trim(), &body.tier)?;
    state.reload()?;
    Ok(Json(json!({ "saved": body.name })))
}

async fn delete_tier(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    state.db.delete_tier(&name)?;
    state.reload()?;
    Ok(Json(json!({ "deleted": name })))
}

async fn list_licenses(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    Ok(Json(json!(state.db.list_licenses()?)))
}

/// Issue a licence from the browser.
///
/// The signing key stays in its file on this machine; the page gets the
/// finished licence once, to hand to whoever it is for. It is not stored
/// anywhere afterwards — only the fact that it was issued is.
async fn mint_license(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<MintBody>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    if body.license_id.trim().is_empty() || body.tier.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "a licence needs an id and a tier".into(),
        ));
    }
    let path =
        std::env::var("VD_LICENSE_KEY_FILE").unwrap_or_else(|_| "license-signing.key".to_string());
    let text = std::fs::read_to_string(&path).map_err(|err| {
        ApiError::Upstream(format!("cannot read the signing key at {path}: {err}"))
    })?;
    let bytes = B64
        .decode(text.trim())
        .ok()
        .and_then(|raw| <[u8; 32]>::try_from(raw).ok())
        .ok_or_else(|| ApiError::Upstream("the signing key file does not hold a key".into()))?;
    let signer = SigningKey::from_bytes(&bytes);

    let max_peers = if body.max_peers == 0 {
        state.registry.read().tier(&body.tier).max_peers
    } else {
        body.max_peers
    };
    let license = License {
        license_id: body.license_id.trim().to_string(),
        tier: body.tier.trim().to_string(),
        // Zero days is a licence with no expiry — the operator's own machine.
        expires_at: if body.days == 0 {
            0
        } else {
            now() + body.days * 24 * 3600
        },
        max_peers,
    };
    let token = vd_license::mint(&signer, &license);
    state.db.record_license(
        &license.license_id,
        &license.tier,
        license.expires_at,
        license.max_peers,
        &body.note,
        now(),
    )?;
    // A freshly minted licence is not revoked, even if an older one with the
    // same id was.
    state.db.unrevoke(&license.license_id)?;
    Ok(Json(
        json!({ "license": token, "license_id": license.license_id }),
    ))
}

async fn revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LicenseBody>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    state
        .db
        .revoke(body.license_id.trim(), &body.reason, now())?;
    Ok(Json(json!({ "revoked": body.license_id })))
}

async fn unrevoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LicenseBody>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    state.db.unrevoke(body.license_id.trim())?;
    Ok(Json(json!({ "restored": body.license_id })))
}

/// What has been spent, by licence and by model, and how much of it came out
/// of a cache.
async fn stats(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(window): Query<Window>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    let hours = window.hours.unwrap_or(24).clamp(1, 24 * 90);
    let since = now() - hours * 3600;

    let by_license: Vec<Value> = state
        .db
        .usage_by_license(since)?
        .into_iter()
        .map(|(license_id, credits, calls)| {
            json!({
                "license_id": license_id,
                "credits": credits,
                "calls": calls,
            })
        })
        .collect();
    let by_model: Vec<Value> = state
        .db
        .usage_by_model(since)?
        .into_iter()
        .map(|(model, credits, prompt, cached)| {
            json!({
                "model": model,
                "credits": credits,
                "prompt_tokens": prompt,
                "cached_tokens": cached,
                "cache_share": if prompt > 0 { cached as f64 / prompt as f64 } else { 0.0 },
            })
        })
        .collect();

    Ok(Json(json!({
        "hours": hours,
        "credit_usd": state.cfg.credit_usd,
        // The number that says whether the prompt is built stable-first, or
        // whether the cache is being paid for and thrown away.
        "cache_share": state.db.cache_share_since(since)?,
        "by_license": by_license,
        "by_model": by_model,
        "window_5h_seconds": WINDOW_5H,
        "window_week_seconds": WINDOW_WEEK,
    })))
}
