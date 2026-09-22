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
use crate::quota::{allowance, WINDOW_5H, WINDOW_WEEK};
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
        .route("/admin/grok/status", get(grok_status_all))
        .route("/admin/upstreams/{id}/grok/login", post(grok_login_start))
        .route("/admin/upstreams/{id}/grok/poll", post(grok_login_poll))
        .route("/admin/upstreams/{id}/grok/logout", post(grok_logout))
        .route("/admin/upstreams/{id}/grok", get(grok_status_one))
        .route("/admin/models", get(list_models).post(save_model))
        .route("/admin/models/{name}", delete(delete_model))
        .route("/admin/models/{name}/providers", get(model_providers))
        .route("/admin/tiers", get(list_tiers).post(save_tier))
        .route("/admin/tiers/{name}", delete(delete_tier))
        .route("/admin/licenses", get(list_licenses).post(mint_license))
        .route("/admin/licenses/{id}/reissue", post(reissue_license))
        .route("/admin/licenses/{id}/peers", post(set_peers))
        .route("/admin/upstreams/{id}/catalog", get(catalog))
        .route("/admin/licenses/{id}/devices", get(list_devices))
        .route("/admin/licenses/{id}/detail", get(license_detail))
        .route("/admin/licenses/{id}/mailbox", delete(clear_mailbox))
        .route("/admin/licenses/{id}/credits", post(add_credits))
        .route(
            "/admin/licenses/{id}/devices/{device}",
            delete(forget_device),
        )
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
struct PollBody {
    device_code: String,
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
    #[serde(default)]
    days: Option<i64>,
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
        "credit_price_usd": state.cfg.credit_price_usd,
        "topup_url": state.cfg.topup_url,
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
    if body.id.trim().is_empty() || (!body.grok && body.base_url.trim().is_empty()) {
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
    if state.db.upstream_is_grok(&id)? {
        return Err(ApiError::BadRequest(
            "a Grok subscription signs in through the browser; a pasted key is not accepted"
                .into(),
        ));
    }
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

fn grok_status_value(state: &AppState, upstream_id: Option<&str>) -> Result<Value, ApiError> {
    crate::grok_auth::public_status(&state.db, upstream_id).map_err(ApiError::Upstream)
}

async fn grok_status_all(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    Ok(Json(grok_status_value(&state, None)?))
}

async fn grok_status_one(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    Ok(Json(grok_status_value(&state, Some(&id))?))
}

async fn grok_login_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    if !state.db.upstream_is_grok(&id)? {
        return Err(ApiError::BadRequest(
            "this upstream is not a Grok subscription".into(),
        ));
    }
    let code = crate::grok_auth::start_device(&state.llm.http, &state.grok_auth)
        .await
        .map_err(ApiError::Upstream)?;
    Ok(Json(json!({
        "device_code": code.device_code,
        "user_code": code.user_code,
        "verification_uri": code.verification_uri,
        "verification_uri_complete": code.verification_uri_complete,
        "interval": code.interval,
        "expires_in": code.expires_in,
    })))
}

async fn grok_login_poll(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: String,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    if !state.db.upstream_is_grok(&id)? {
        return Err(ApiError::BadRequest(
            "this upstream is not a Grok subscription".into(),
        ));
    }
    let body: PollBody = serde_json::from_str(&body)
        .map_err(|_| ApiError::BadRequest("device_code is required".into()))?;
    if body.device_code.trim().is_empty() {
        return Err(ApiError::BadRequest("device_code is required".into()));
    }
    match crate::grok_auth::poll_device(&state.llm.http, &state.grok_auth, &body.device_code)
        .await
        .map_err(ApiError::Upstream)?
    {
        crate::grok_auth::PollOutcome::Pending => {
            Ok(Json(json!({"status": "pending", "message": "", "expires_at": 0})))
        }
        crate::grok_auth::PollOutcome::Error(message) => {
            Ok(Json(json!({"status": "error", "message": message, "expires_at": 0})))
        }
        crate::grok_auth::PollOutcome::Approved(tokens) => {
            state.db.insert_grok_session(
                &id,
                &tokens.access_token,
                &tokens.refresh_token,
                tokens.expires_at,
                now(),
            )?;
            state.reload()?;
            Ok(Json(
                json!({"status": "ok", "message": "", "expires_at": tokens.expires_at}),
            ))
        }
    }
}

async fn grok_logout(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    state.db.delete_grok_sessions(&id)?;
    state.reload()?;
    Ok(Json(grok_status_value(&state, Some(&id))?))
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
    Json(raw): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    let mut body: ModelRow = serde_json::from_value(raw.clone())
        .map_err(|err| ApiError::BadRequest(format!("not a model: {err}")))?;
    // The add form knows nothing of routing: re-saving a model from it keeps
    // the hosts picked for it rather than quietly dropping them.
    if raw.get("routing").is_none() {
        if let Some(existing) = state
            .registry
            .read()
            .models
            .iter()
            .find(|model| model.name == body.name)
        {
            body.routing = existing.routing.clone();
        }
    }
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

/// What an upstream says it serves, with the prices it publishes.
///
/// Typing a model name and three prices by hand is how a pricing table ends
/// up quietly wrong — a decimal point in the wrong place bills a tenth of
/// what a model costs. OpenRouter publishes both; this fetches them with one
/// of the upstream's own keys and hands them to the form.
async fn catalog(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    if let Err(err) = state.prepare_grok().await {
        log::warn!("grok session was not refreshed: {err}");
    }
    let (provider, pool) = {
        let registry = state.registry.read();
        let upstream = registry
            .upstreams
            .iter()
            .find(|up| up.id == id)
            .ok_or_else(|| ApiError::BadRequest(format!("no upstream called {id}")))?;
        // Any model of this upstream will do: the catalogue is a property of
        // the endpoint, not of the model asked about.
        let sample = registry
            .models
            .iter()
            .find(|model| model.upstream_id == id)
            .cloned()
            .unwrap_or_else(|| crate::registry::ModelRow {
                name: String::new(),
                upstream_id: id.clone(),
                upstream_name: String::new(),
                price_in: 0.0,
                price_cached: None,
                price_out: 0.0,
                context_tokens: None,
                enabled: true,
                position: 0,
                voice: false,
                images: false,
                vision: false,
                price_request: 0.0,
                routing: Default::default(),
            });
        (
            crate::registry::provider_for(upstream, &sample),
            registry.pool(&id),
        )
    };
    let pool = pool.ok_or_else(|| ApiError::BadRequest("this upstream has no keys".into()))?;
    let lease = pool
        .acquire()
        .ok_or_else(|| ApiError::BadRequest("this upstream has no key that works".into()))?;
    match vd_llm::catalog::list_models(&state.llm.http, &provider, &lease.key).await {
        Ok(catalog) => {
            pool.report_success(lease.index);
            Ok(Json(json!({ "models": catalog.models })))
        }
        Err(err) => {
            pool.report_failure(lease.index, err.verdict());
            Err(ApiError::Upstream(err.message()))
        }
    }
}

/// The hosts OpenRouter serves one model through, with their prices.
///
/// Public on OpenRouter's side, so no key is spent on it. Prices come back
/// per million tokens, the unit the model table uses.
async fn model_providers(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    let (slug, routing) = {
        let registry = state.registry.read();
        let model = registry
            .models
            .iter()
            .find(|model| model.name == name)
            .ok_or_else(|| ApiError::BadRequest(format!("no model called {name}")))?;
        let upstream = registry
            .upstreams
            .iter()
            .find(|up| up.id == model.upstream_id)
            .ok_or_else(|| ApiError::BadRequest("the model's upstream is gone".into()))?;
        if crate::registry::provider_for(upstream, model).dialect() != "openrouter" {
            return Err(ApiError::BadRequest(
                "host routing exists only for models served through OpenRouter".into(),
            ));
        }
        // `:floor`, `:nitro` and the like are routing of their own; the list
        // is of the model itself.
        let slug = model
            .upstream_name()
            .split(':')
            .next()
            .unwrap_or("")
            .to_string();
        (slug, model.routing.clone())
    };
    // `~vendor/name-latest` is an alias that follows the vendor's newest
    // model; OpenRouter lists no hosts under the alias itself, only under
    // the model it currently points at.
    let target = if slug.starts_with('~') {
        let catalog = openrouter_json(&state, "https://openrouter.ai/api/v1/models").await?;
        catalog["data"]
            .as_array()
            .and_then(|models| models.iter().find(|m| m["id"] == slug.as_str()))
            .and_then(|m| m["alias_target"]["slug"].as_str())
            .unwrap_or(&slug)
            .to_string()
    } else {
        slug.clone()
    };
    let data = openrouter_json(
        &state,
        &format!("https://openrouter.ai/api/v1/models/{target}/endpoints"),
    )
    .await?;
    let per_million = |value: &Value| -> Option<f64> {
        let raw = match value {
            Value::String(text) => text.parse::<f64>().ok()?,
            Value::Number(number) => number.as_f64()?,
            _ => return None,
        };
        Some((raw * 1_000_000.0 * 10_000.0).round() / 10_000.0)
    };
    let hosts: Vec<Value> = data["data"]["endpoints"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|endpoint| {
            let pricing = &endpoint["pricing"];
            json!({
                "tag": endpoint["tag"].as_str().unwrap_or(""),
                "provider": endpoint["provider_name"].as_str().unwrap_or(""),
                "quantization": endpoint["quantization"].as_str().unwrap_or(""),
                "price_in": per_million(&pricing["prompt"]),
                "price_out": per_million(&pricing["completion"]),
                "price_cached": per_million(&pricing["input_cache_read"]),
                "discount": pricing["discount"].as_f64().unwrap_or(0.0),
                "context": endpoint["context_length"],
                "uptime": endpoint["uptime_last_30m"],
                // Tokens a second and seconds to first token over the last
                // half hour; a number or a percentile object, null when
                // OpenRouter has too little traffic to say.
                "throughput": endpoint["throughput_last_30m"],
                "latency": endpoint["latency_last_30m"],
                "status": endpoint["status"],
                "tools": endpoint["supported_parameters"]
                    .as_array()
                    .is_some_and(|params| params.iter().any(|p| p == "tools")),
            })
        })
        .collect();
    Ok(Json(json!({
        "model": slug,
        "target": target,
        "hosts": hosts,
        "routing": routing,
    })))
}

/// One public OpenRouter document, as JSON.
async fn openrouter_json(state: &AppState, url: &str) -> Result<Value, ApiError> {
    let response = state
        .llm
        .http
        .get(url)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|err| ApiError::Upstream(format!("OpenRouter: {err}")))?;
    if !response.status().is_success() {
        return Err(ApiError::Upstream(format!(
            "OpenRouter answered {} for {url}",
            response.status()
        )));
    }
    response
        .json()
        .await
        .map_err(|err| ApiError::Upstream(format!("OpenRouter: {err}")))
}

/// Empty a licence's sync mailbox.
///
/// Everything in it is sealed and unreadable here, so this is not a way to
/// look at anybody's correspondence — it is the way to reclaim the space
/// when a licence is finished with, or to force every device to start the
/// exchange again from what it holds locally.
async fn clear_mailbox(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    // The room is a hash of the licence token, which is never stored here;
    // what is stored is the room a device presented alongside this licence
    // the last time it pushed. A licence whose devices have never synced has
    // no mailbox to clear.
    let room = state.db.room_of(&id)?.ok_or_else(|| {
        ApiError::BadRequest(format!("{id} has no mailbox: no device has synced with it"))
    })?;
    let cleared = state.db.mailbox_clear(&room)?;
    Ok(Json(json!({ "cleared": cleared })))
}

/// The machines one licence is in use from.
/// Everything about one licence for its detail card: what it is, what it has
/// left (the plan's 5h and weekly windows, and the permanent wallet on top),
/// its devices, and its spend per day for the chart.
async fn license_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(window): Query<Window>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    let days = window.days.unwrap_or(30).clamp(1, 180);
    let since = now() - days * 86400;
    let row = state
        .db
        .list_licenses()?
        .into_iter()
        .find(|r| r.license_id == id);
    let tier_name = row.as_ref().map(|r| r.tier.clone()).unwrap_or_default();
    let tier = state.registry.read().tier(&tier_name);
    let allow = allowance(&state.db, &id, tier, now())?;
    let devices = state.db.devices(&id)?;
    let daily: Vec<Value> = state
        .db
        .usage_daily(&id, since)?
        .into_iter()
        .map(|(day, credits, calls)| json!({ "day": day, "credits": credits, "calls": calls }))
        .collect();
    Ok(Json(json!({
        "license": row,
        "tier": { "credits_5h": tier.credits_5h, "credits_week": tier.credits_week, "max_peers": tier.max_peers },
        "allowance": {
            "left_5h": allow.left_5h, "left_week": allow.left_week,
            "wallet": allow.wallet, "reset_at": allow.reset_at
        },
        "devices": devices,
        "daily": daily,
        "days": days,
        "credit_usd": state.cfg.credit_usd
    })))
}

async fn list_devices(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    Ok(Json(json!(state.db.devices(&id)?)))
}

/// Release a seat.
///
/// Only from here: an operator who could free their own seat could work
/// through a ten-device licence with a whole floor of people. A laptop that
/// was replaced, sold or reinstalled is unbound by whoever sold the licence,
/// and the next machine to call takes the seat.
async fn forget_device(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, device)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    state.db.forget_device(&id, &device)?;
    Ok(Json(json!({ "released": device })))
}

#[derive(Deserialize)]
struct CreditsBody {
    /// Credits to sell. Negative takes them back — a refund, or a mistake
    /// being undone.
    credits: f64,
}

/// Sell credits against a licence key.
///
/// These sit outside the plan's windows and none of them is spent while the
/// included allowance still has room: an operator who tops up on Thursday
/// keeps what they bought until the week's budget is actually gone.
async fn add_credits(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<CreditsBody>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    if !body.credits.is_finite() || body.credits.abs() > 10_000_000.0 {
        return Err(ApiError::BadRequest(
            "that is not a number of credits".into(),
        ));
    }
    let known = state
        .db
        .list_licenses()?
        .into_iter()
        .find(|row| row.license_id == id)
        .ok_or_else(|| ApiError::BadRequest(format!("no licence called {id}")))?;
    // Top-ups are a business feature, deliberately. A pro licence that keeps
    // running out is a pro licence that has outgrown its plan, and the answer
    // to that is the bigger plan — which is also the cheaper one per seat.
    // Selling credits into pro would let somebody stay on the small plan
    // forever at the price of the large one.
    if !known.tier.eq_ignore_ascii_case("business") {
        return Err(ApiError::BadRequest(format!(
            "credits are sold on business licences; {id} is on {}",
            known.tier
        )));
    }
    let left = state
        .db
        .wallet_add(&id, body.credits, crate::registry::now())?;
    Ok(Json(json!({ "license_id": id, "credits_left": left })))
}

#[derive(Deserialize)]
struct PeersBody {
    max_peers: u32,
}

/// Change how many devices one licence pairs.
///
/// The key itself is not reissued and does not change: the number it was
/// sold with stays in the signature, and this is the server's own record,
/// which is what the gateway actually enforces. A team of twelve on a
/// ten-device licence is a line in this table, not a support ticket.
async fn set_peers(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PeersBody>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    let known = state
        .db
        .list_licenses()?
        .into_iter()
        .find(|row| row.license_id == id)
        .ok_or_else(|| ApiError::BadRequest(format!("no licence called {id}")))?;
    let peers = body.max_peers.max(1);
    state.db.record_license(
        &known.license_id,
        &known.tier,
        known.expires_at,
        peers,
        &known.note,
        crate::registry::now(),
    )?;
    Ok(Json(json!({ "license_id": id, "max_peers": peers })))
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

/// Re-print the key for a licence that already exists. A minted token is shown
/// once at issue; the operator who lost it need not lose the licence too. The
/// signature is deterministic, so re-minting the very same id/tier/expiry/seats
/// yields the identical key — this hands back exactly what was issued, without
/// changing anything the gateway enforces.
async fn reissue_license(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    admin(&state, &headers)?;
    let row = state
        .db
        .list_licenses()?
        .into_iter()
        .find(|r| r.license_id == id)
        .ok_or_else(|| ApiError::BadRequest("no licence with that id".into()))?;
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
    let license = License {
        license_id: row.license_id.clone(),
        tier: row.tier.clone(),
        expires_at: row.expires_at,
        max_peers: row.max_peers,
    };
    let token = vd_license::mint(&signer, &license);
    Ok(Json(
        json!({ "license": token, "license_id": row.license_id }),
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
    // `hours=0` is all time: the book is small enough to sum whole.
    let hours = window.hours.unwrap_or(24).clamp(0, 24 * 3650);
    let since = if hours == 0 { 0 } else { now() - hours * 3600 };

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
