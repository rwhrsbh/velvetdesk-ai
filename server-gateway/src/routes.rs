//! The doors: who may come in, what it costs them, and the two dialects they
//! may speak on the way.

use std::collections::HashMap;
use std::convert::Infallible;

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::config::Tier;
use crate::quota::{allowance, charge, charge_flat, request_credits, Allowance, Bill};
use crate::state::AppState;
use crate::translate;
use vd_license::{verify, License, LicenseError};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/v1/usage", get(usage))
        .route("/v1/chat/completions", post(chat_completions))
        // Dictation, in the shape every OpenAI client already speaks, so the
        // desktop needs no special case for it.
        .route("/v1/audio/transcriptions", post(transcriptions))
        // Gemini puts the action after a colon, which is not a path segment,
        // so the whole tail is taken and split here.
        .route("/v1beta/models/{*tail}", post(gemini_generate))
        // Two paired devices meeting. The gateway moves sealed bytes between
        // them and reads none of it.
        .route("/sync/ws", get(sync_ws))
        // The same exchange for devices that are never awake together: one
        // leaves sealed records, the other collects them whenever it starts.
        .route("/sync/push", post(sync_push))
        .route("/sync/pull", get(sync_pull))
        // Buying more credits, and hearing that the money arrived.
        .route("/pay/checkout", post(checkout))
        // Buying a plan. Open: the buyer has no licence yet, which is what
        // they are here to get.
        .route("/pay/plans", get(plans))
        .route("/pay/coins", get(coins))
        .route("/pay/subscribe", post(subscribe))
        .route("/pay/order/{order}", get(order_status))
        .route("/pay/orders", get(orders_for_device))
        // Payment notifications. Open by necessity — the provider posts here
        // with no credentials of ours — and trusted only by signature.
        .route("/pay/ipn", post(payment_notice))
        .merge(crate::admin::router())
        .with_state(state)
}

// ------------------------------------------------------------------- errors

pub enum ApiError {
    Unauthorized(String),
    Forbidden(String),
    BadRequest(String),
    OutOfCredits(Allowance),
    Upstream(String),
    /// The gateway is full. Not the caller's fault and not permanent, so it
    /// travels as 503 with a Retry-After rather than as a failure.
    Busy {
        message: String,
        retry_after: u64,
    },
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, body) = match self {
            ApiError::Unauthorized(message) => (
                StatusCode::UNAUTHORIZED,
                json!({ "error": { "type": "unauthorized", "message": message } }),
            ),
            ApiError::Forbidden(message) => (
                StatusCode::FORBIDDEN,
                json!({ "error": { "type": "forbidden", "message": message } }),
            ),
            ApiError::BadRequest(message) => (
                StatusCode::BAD_REQUEST,
                json!({ "error": { "type": "invalid_request", "message": message } }),
            ),
            // The client shows a time, not an error: the operator has not done
            // anything wrong, the window has.
            ApiError::OutOfCredits(state) => (
                StatusCode::TOO_MANY_REQUESTS,
                json!({
                    "error": {
                        "type": "rate_limit",
                        "reason": "window",
                        "message": "this licence has spent its credits for now —                                     wait for the window to reopen, or buy more",
                        "reset_at": state.reset_at,
                        "credits_left": state.left(),
                    }
                }),
            ),
            // What went wrong upstream is the gateway's business, not the
            // client's. The raw text names the model, the provider behind it
            // and sometimes a link to their console — which together tell a
            // paying customer exactly what they are really talking to, and
            // tell anybody else how the service is assembled. It is logged
            // here in full and answered for in one sentence.
            ApiError::Upstream(message) => {
                log::warn!("upstream failed: {message}");
                let (code, text) = classify_upstream(&message);
                (
                    code,
                    json!({ "error": { "type": "upstream", "message": text } }),
                )
            }
            ApiError::Busy {
                message,
                retry_after,
            } => {
                let body = json!({
                    "error": {
                        "type": "overloaded",
                        "message": message,
                        "retry_after": retry_after,
                    }
                });
                let mut response = (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response();
                if let Ok(value) = HeaderValue::from_str(&retry_after.to_string()) {
                    response.headers_mut().insert("retry-after", value);
                }
                return response;
            }
        };
        (status, Json(body)).into_response()
    }
}

/// Turn an upstream failure into something safe to say out loud.
///
/// The distinctions worth keeping are the ones a client can act on: wait and
/// try again, or stop and tell somebody. Everything else is one sentence.
fn classify_upstream(detail: &str) -> (StatusCode, &'static str) {
    let lower = detail.to_ascii_lowercase();
    let rate_limited = lower.contains("429")
        || lower.contains("rate-limit")
        || lower.contains("rate limit")
        || lower.contains("quota")
        || lower.contains("overload")
        || lower.contains("parked");
    if rate_limited {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "the service is busy right now — try again in a minute",
        );
    }
    if lower.contains("declined") || lower.contains("blocked") || lower.contains("safety") {
        return (
            StatusCode::BAD_REQUEST,
            "the model declined to answer this request",
        );
    }
    if lower.contains("timed out") || lower.contains("timeout") || lower.contains("transport") {
        return (
            StatusCode::GATEWAY_TIMEOUT,
            "the service did not answer in time — try again",
        );
    }
    (
        StatusCode::BAD_GATEWAY,
        "the service could not answer this request — try again shortly",
    )
}

impl From<rusqlite::Error> for ApiError {
    fn from(value: rusqlite::Error) -> Self {
        ApiError::Upstream(format!("gateway database: {value}"))
    }
}

// --------------------------------------------------------------------- auth

pub struct Caller {
    pub license: License,
    pub tier: Tier,
    /// Devices this licence may pair, after the server has had its say.
    pub peers: u32,
}

/// The licence, whichever way the client carries it: an OpenAI-style bearer
/// token, Gemini's own header, or the query parameter its SDKs still use.
fn token(headers: &HeaderMap, query: &HashMap<String, String>) -> Option<String> {
    if let Some(value) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        let trimmed = value.trim();
        let token = trimmed
            .strip_prefix("Bearer ")
            .or_else(|| trimmed.strip_prefix("bearer "))
            .unwrap_or(trimmed);
        if !token.is_empty() {
            return Some(token.to_string());
        }
    }
    if let Some(value) = headers.get("x-goog-api-key").and_then(|v| v.to_str().ok()) {
        if !value.trim().is_empty() {
            return Some(value.trim().to_string());
        }
    }
    query.get("key").filter(|k| !k.is_empty()).cloned()
}

fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
) -> Result<Caller, ApiError> {
    let Some(verifier) = state.verifier.as_ref() else {
        return Err(ApiError::Forbidden(LicenseError::NoKey.to_string()));
    };
    let token = token(headers, query)
        .ok_or_else(|| ApiError::Unauthorized("no licence key was sent".into()))?;
    let license =
        verify(&token, verifier).map_err(|err| ApiError::Unauthorized(err.to_string()))?;
    if license.expired_at(now()) {
        return Err(ApiError::Forbidden(LicenseError::Expired.to_string()));
    }
    if state.db.is_revoked(&license.license_id)? {
        return Err(ApiError::Forbidden(LicenseError::Revoked.to_string()));
    }
    let tier = state.registry.read().tier(&license.tier);
    // Three numbers can say how many devices are allowed, and the server's
    // own entry wins: the licence carries what it was sold with, the tier
    // carries the plan's default, and the ledger carries whatever the
    // operator has been granted since — a team that asked for more gets it
    // without being issued a new key.
    let peers = state
        .db
        .peers_for(&license.license_id)?
        .unwrap_or_else(|| license.max_peers.max(tier.max_peers))
        .max(1);
    Ok(Caller {
        license,
        tier,
        peers,
    })
}

/// Wait for a slot in the gateway, or turn the caller away politely.
///
/// Every call that costs an upstream request passes through here, so a burst
/// of operators becomes a queue instead of a wall of refusals from the
/// provider behind it.
async fn admit(state: &AppState, license_id: &str) -> Result<crate::queue::Slot, ApiError> {
    state
        .queue
        .admit(license_id)
        .await
        .map_err(|rejected| ApiError::Busy {
            message: rejected.message().to_string(),
            retry_after: state.queue.retry_after(),
        })
}

/// Which machine is calling, if it says.
///
/// Our own app always says; a third-party client pointed at the gateway may
/// not, and is treated as one shared, nameless device rather than refused —
/// the licence still limits what it can spend.
fn device_id(headers: &HeaderMap) -> String {
    headers
        .get("x-vd-device")
        .and_then(|value| value.to_str().ok())
        .map(|id| id.trim())
        .filter(|id| !id.is_empty() && id.len() <= 64)
        .unwrap_or("unnamed")
        .to_string()
}

/// Take a seat on the licence, or explain that they are all taken.
///
/// A seat is held by the first machine that uses it and stays held: that is
/// what a ten-device licence means to whoever bought one. Nothing the
/// operator can do releases a seat — otherwise a licence for ten would be a
/// licence for however many people take turns — so a replaced laptop is
/// freed by whoever sells the licences, on the admin page.
fn seat(state: &AppState, caller: &Caller, headers: &HeaderMap) -> Result<(), ApiError> {
    let device = device_id(headers);
    match state
        .db
        .admit_device(&caller.license.license_id, &device, caller.peers, now())?
    {
        crate::registry::DeviceVerdict::Known
        | crate::registry::DeviceVerdict::Admitted { .. } => Ok(()),
        crate::registry::DeviceVerdict::NoSeats { taken } => Err(ApiError::Forbidden(format!(
            "this licence covers {} device(s) and {taken} are already registered - ask whoever sold it to release one",
            caller.peers
        ))),
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// What is left, on the way out, so the client can show it before the
/// operator runs into the wall rather than after.
fn credit_headers(state: &Allowance) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let left = HeaderValue::from_str(&format!("{:.2}", state.left())).ok();
    let reset = HeaderValue::from_str(&state.reset_at.to_string()).ok();
    if let Some(value) = left {
        headers.insert(HeaderName::from_static("x-vd-credits-left"), value);
    }
    if let Some(value) = reset {
        headers.insert(HeaderName::from_static("x-vd-window-reset"), value);
    }
    headers
}

/// Bill one answer, looking the price up in the registry as it stands now.
fn charge_one(
    state: &AppState,
    license_id: &str,
    tier: Tier,
    model: &str,
    response: &vd_llm::ChatResponse,
) -> rusqlite::Result<Allowance> {
    let priced = state
        .registry
        .read()
        .find_model(model)
        .map(|(_, row)| row.clone());
    charge(
        &state.db,
        &Bill {
            license_id,
            tier,
            model,
            priced: priced.as_ref(),
            usage: &response.usage,
            credit_usd: state.cfg.credit_usd,
            now: now(),
        },
    )
}

fn charge_for(
    state: &AppState,
    caller: &Caller,
    model: &str,
    response: &vd_llm::ChatResponse,
) -> rusqlite::Result<Allowance> {
    charge_one(
        state,
        &caller.license.license_id,
        caller.tier,
        model,
        response,
    )
}

// ------------------------------------------------------------------- routes

async fn health() -> Json<Value> {
    Json(json!({ "ok": true }))
}

async fn models(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    authenticate(&state, &headers, &query)?;
    let data: Vec<Value> = state
        .model_names()
        .into_iter()
        .map(|name| json!({ "id": name, "object": "model", "owned_by": "velvetdesk" }))
        .collect();
    Ok(Json(json!({ "object": "list", "data": data })))
}

async fn usage(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let caller = authenticate(&state, &headers, &query)?;
    // Asking what the licence has left is what the app does the moment a key
    // is entered, so this is where a machine claims its seat — and where a
    // licence with no seats left says so, before the operator has typed a
    // single reply.
    seat(&state, &caller, &headers)?;
    let state_now = allowance(&state.db, &caller.license.license_id, caller.tier, now())?;
    Ok(Json(json!({
        "license_id": caller.license.license_id,
        "tier": caller.license.tier,
        "expires_at": caller.license.expires_at,
        "max_peers": caller.peers,
        "devices_used": state.db.device_count(&caller.license.license_id)?,
        "credits_left_5h": state_now.left_5h,
        "credits_left_week": state_now.left_week,
        // The ceilings, so a client can say "92% left" instead of a number
        // nobody can place: 5988 credits means nothing without the 6000.
        "credits_5h": caller.tier.credits_5h,
        "credits_week": caller.tier.credits_week,
        // Bought on top of the plan, and spent only once the plan's windows
        // are empty.
        "credits_extra": state_now.wallet,
        // What more would cost, and where to get it. The app shows the way
        // to buy before anybody hits the wall, not after.
        "credit_price_usd": state.cfg.credit_price_usd,
        // Only business licences can buy more; pro that keeps running out is
        // pro that should move up, and the app says which.
        "topup_url": if caller.license.tier.eq_ignore_ascii_case("business") {
            state.cfg.topup_url.clone()
        } else {
            String::new()
        },
        "can_top_up": caller.license.tier.eq_ignore_ascii_case("business"),
        "reset_at": state_now.reset_at,
    })))
}

async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let caller = authenticate(&state, &headers, &query)?;
    let before = allowance(&state.db, &caller.license.license_id, caller.tier, now())?;
    if before.exhausted() {
        return Err(ApiError::OutOfCredits(before));
    }

    seat(&state, &caller, &headers)?;
    let slot = admit(&state, &caller.license.license_id).await?;

    let request: translate::OaiRequest = serde_json::from_value(body)
        .map_err(|err| ApiError::BadRequest(format!("cannot read the request: {err}")))?;
    let wanted = request.model.clone();
    let streaming = request.stream;
    let chat = request.to_chat_request();
    let id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
    let created = now();

    if !streaming {
        let (model, response) = state
            .call(&wanted, &chat, &|_| {})
            .await
            .map_err(|err| ApiError::Upstream(err.to_string()))?;
        let after = charge_for(&state, &caller, &model, &response)?;
        drop(slot);
        let body = translate::oai_completion(&id, created, &model, &response);
        return Ok((credit_headers(&after), Json(body)).into_response());
    }

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let task = state.clone();
    let license_id = caller.license.license_id.clone();
    let tier = caller.tier;
    tokio::spawn(async move {
        let sender = tx.clone();
        // The slot belongs to the call, not to the handler that started
        // it: a streamed answer occupies the gateway until its last token.
        let _slot = slot;
        let chunk_id = id.clone();
        let chunk_model = wanted.clone();
        // Every piece of text goes out the moment it arrives: the operator
        // reads the answer while it is still being written, which is the
        // whole reason the stream exists.
        let on_event = move |event: Value| {
            if event.get("kind").and_then(Value::as_str) != Some("delta") {
                return;
            }
            let Some(text) = event.get("text").and_then(Value::as_str) else {
                return;
            };
            let chunk = translate::oai_chunk(&chunk_id, created, &chunk_model, text);
            let _ = sender.send(Event::default().data(chunk.to_string()));
        };

        match task.call(&wanted, &chat, &on_event).await {
            Ok((model, response)) => {
                if let Err(err) = charge_one(&task, &license_id, tier, &model, &response) {
                    log::error!("could not record usage: {err}");
                }
                let last = translate::oai_final_chunk(&id, created, &model, &response);
                let _ = tx.send(Event::default().data(last.to_string()));
            }
            Err(err) => {
                // The stream has already started, so the failure travels as
                // an event rather than a status code nobody will see — and
                // it is sanitised the same way, for the same reason.
                log::warn!("upstream failed mid-stream: {err}");
                let (_, text) = classify_upstream(&err.to_string());
                let body = json!({ "error": { "type": "upstream", "message": text } });
                let _ = tx.send(Event::default().data(body.to_string()));
            }
        }
        let _ = tx.send(Event::default().data("[DONE]"));
    });

    let stream = UnboundedReceiverStream::new(rx).map(Ok::<Event, Infallible>);
    Ok((credit_headers(&before), Sse::new(stream)).into_response())
}

/// The biggest clip the gateway will take: about ten minutes of speech at the
/// bitrate the app records at, and small enough that a stuck upload cannot
/// hold a connection open all day.
const MAX_CLIP: usize = 24 * 1024 * 1024;

/// Turn a dictated clip into text.
///
/// The request is the OpenAI one — multipart, a `file` part, a `model` name
/// that this gateway ignores — because the subscription decides which voice
/// model runs, the same way it decides which chat model does.
async fn transcriptions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    mut form: axum::extract::Multipart,
) -> Result<Response, ApiError> {
    let caller = authenticate(&state, &headers, &query)?;
    let before = allowance(&state.db, &caller.license.license_id, caller.tier, now())?;
    if before.exhausted() {
        return Err(ApiError::OutOfCredits(before));
    }

    seat(&state, &caller, &headers)?;

    let mut clip: Vec<u8> = vec![];
    let mut mime = String::new();
    while let Some(field) = form
        .next_field()
        .await
        .map_err(|err| ApiError::BadRequest(format!("cannot read the upload: {err}")))?
    {
        if field.name() != Some("file") {
            continue;
        }
        mime = field
            .content_type()
            .unwrap_or("audio/webm")
            .split(';')
            .next()
            .unwrap_or("audio/webm")
            .trim()
            .to_string();
        let bytes = field
            .bytes()
            .await
            .map_err(|err| ApiError::BadRequest(format!("cannot read the clip: {err}")))?;
        clip = bytes.to_vec();
        break;
    }
    if clip.is_empty() {
        return Err(ApiError::BadRequest("the upload had no audio in it".into()));
    }
    if clip.len() > MAX_CLIP {
        return Err(ApiError::BadRequest(format!(
            "the clip is {} MB; {} MB is the limit",
            clip.len() / (1024 * 1024),
            MAX_CLIP / (1024 * 1024)
        )));
    }

    let _slot = admit(&state, &caller.license.license_id).await?;

    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&clip);
    let (model, price, text) = state
        .transcribe(&encoded, &mime)
        .await
        .map_err(|err| ApiError::Upstream(err.to_string()))?;
    let after = charge_flat(
        &state.db,
        &caller.license.license_id,
        caller.tier,
        &model,
        request_credits(price, state.cfg.credit_usd),
        now(),
    )?;
    Ok((credit_headers(&after), Json(json!({ "text": text }))).into_response())
}

async fn gemini_generate(
    State(state): State<AppState>,
    Path(tail): Path<String>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let caller = authenticate(&state, &headers, &query)?;
    let before = allowance(&state.db, &caller.license.license_id, caller.tier, now())?;
    if before.exhausted() {
        return Err(ApiError::OutOfCredits(before));
    }

    seat(&state, &caller, &headers)?;
    let slot = admit(&state, &caller.license.license_id).await?;

    // `gemini-2.5-flash:streamGenerateContent` — the model, then the verb.
    let (wanted, action) = tail
        .split_once(':')
        .map(|(model, action)| (model.to_string(), action.to_string()))
        .unwrap_or((tail.clone(), "generateContent".into()));
    let streaming = action.starts_with("stream");
    let chat = translate::gemini_to_chat_request(&body, streaming);

    if !streaming {
        let (model, response) = state
            .call(&wanted, &chat, &|_| {})
            .await
            .map_err(|err| ApiError::Upstream(err.to_string()))?;
        drop(slot);
        let after = charge_for(&state, &caller, &model, &response)?;
        let body = translate::gemini_response(&model, &response);
        return Ok((credit_headers(&after), Json(body)).into_response());
    }

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let task = state.clone();
    let license_id = caller.license.license_id.clone();
    let tier = caller.tier;
    tokio::spawn(async move {
        let sender = tx.clone();
        let on_event = move |event: Value| {
            if event.get("kind").and_then(Value::as_str) != Some("delta") {
                return;
            }
            let Some(text) = event.get("text").and_then(Value::as_str) else {
                return;
            };
            let _ = sender.send(Event::default().data(translate::gemini_chunk(text).to_string()));
        };

        match task.call(&wanted, &chat, &on_event).await {
            Ok((model, response)) => {
                if let Err(err) = charge_one(&task, &license_id, tier, &model, &response) {
                    log::error!("could not record usage: {err}");
                }
                // The closing message carries what the answer cost, which is
                // where a Gemini client looks for it.
                let last = json!({
                    "candidates": [{
                        "content": { "role": "model", "parts": [] },
                        "finishReason": if response.finish_reason.is_empty() {
                            "STOP".to_string()
                        } else {
                            response.finish_reason.to_uppercase()
                        },
                        "index": 0,
                    }],
                    "usageMetadata": translate::gemini_usage(&response),
                    "modelVersion": model,
                });
                let _ = tx.send(Event::default().data(last.to_string()));
            }
            Err(err) => {
                log::warn!("upstream failed mid-stream: {err}");
                let body = json!({
                    "error": {
                        "status": "UNAVAILABLE",
                        "message": classify_upstream(&err.to_string()).1,
                    }
                });
                let _ = tx.send(Event::default().data(body.to_string()));
            }
        }
    });

    let stream = UnboundedReceiverStream::new(rx).map(Ok::<Event, Infallible>);
    Ok((credit_headers(&before), Sse::new(stream)).into_response())
}

// ---------------------------------------------------------------------- sync

/// The most one device may leave in a single push.
const MAX_ITEMS: usize = 256;
/// The most one sealed record may weigh: a long correspondence with pictures
/// referenced rather than embedded, with room to spare.
const MAX_ITEM_BYTES: usize = 2 * 1024 * 1024;
/// How much one licence may keep waiting in the mailbox.
const MAX_ROOM_BYTES: i64 = 256 * 1024 * 1024;
/// Records nobody has collected in this long are dropped: a device that has
/// been gone three months will be rebuilt from its peer rather than from a
/// mailbox kept for it forever.
const MAILBOX_TTL: i64 = 90 * 24 * 3600;

#[derive(serde::Deserialize)]
struct PushBody {
    room: String,
    #[serde(default)]
    device: String,
    items: Vec<PushItem>,
}

#[derive(serde::Deserialize)]
struct PushItem {
    item: String,
    #[serde(default)]
    rev: i64,
    #[serde(default)]
    updated_at: i64,
    /// The record, sealed by the device and base64'd for the wire.
    sealed: String,
}

/// The room a licence is entitled to.
///
/// Derived from the licence itself, exactly as the devices derive it, so a
/// device cannot read or write anybody else's mailbox by naming their room:
/// the name is checked against the token that was presented, not taken on
/// trust.
fn room_for_token(headers: &HeaderMap, query: &HashMap<String, String>) -> Option<String> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
    use base64::Engine as _;
    use sha2::{Digest as _, Sha256};

    let token = token(headers, query)?;
    let mut secret = Sha256::new();
    secret.update(b"velvetdesk-sync-secret/v1");
    secret.update(token.trim().as_bytes());
    let secret = secret.finalize();

    let mut room = Sha256::new();
    room.update(b"velvetdesk-sync-room/v1");
    room.update(secret);
    Some(B64URL.encode(&room.finalize()[..16]))
}

/// Check the room the caller named is the one their licence gives them.
fn allowed_room(
    named: &str,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
) -> Result<String, ApiError> {
    let mine = room_for_token(headers, query)
        .ok_or_else(|| ApiError::Unauthorized("no licence key was sent".into()))?;
    // A pairing made by invite has a room of its own, which the licence
    // cannot predict; those devices still relay live and simply do not use
    // the mailbox. Anything else is somebody reaching into a room that is
    // not theirs.
    if named != mine {
        return Err(ApiError::Forbidden(
            "this room does not belong to this licence".into(),
        ));
    }
    Ok(mine)
}

/// Leave sealed records for the other devices on this licence.
async fn sync_push(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(body): Json<PushBody>,
) -> Result<Json<Value>, ApiError> {
    let caller = authenticate(&state, &headers, &query)?;
    seat(&state, &caller, &headers)?;
    let room = allowed_room(&body.room, &headers, &query)?;
    state.db.note_room(&caller.license.license_id, &room)?;
    if body.items.len() > MAX_ITEMS {
        return Err(ApiError::BadRequest(format!(
            "at most {MAX_ITEMS} records at a time"
        )));
    }

    let (_, bytes) = state.db.mailbox_size(&room)?;
    if bytes > MAX_ROOM_BYTES {
        return Err(ApiError::Forbidden(
            "this licence's sync store is full; the oldest records expire on their own".into(),
        ));
    }

    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;
    let mut stored = 0usize;
    for item in &body.items {
        let sealed = B64
            .decode(item.sealed.as_bytes())
            .map_err(|_| ApiError::BadRequest("a record was not readable base64".into()))?;
        if sealed.len() > MAX_ITEM_BYTES {
            return Err(ApiError::BadRequest(format!(
                "a record is larger than {} MB",
                MAX_ITEM_BYTES / (1024 * 1024)
            )));
        }
        if item.item.is_empty() || item.item.len() > 200 {
            return Err(ApiError::BadRequest("a record name is out of range".into()));
        }
        if state.db.mailbox_put(&crate::registry::MailDrop {
            room: &room,
            item: &item.item,
            rev: item.rev,
            updated_at: item.updated_at,
            device: &body.device,
            sealed: &sealed,
            now: now(),
        })? {
            stored += 1;
        }
    }

    // Housekeeping on the way past: cheap, and it keeps a long-abandoned
    // room from being somebody else's problem later.
    let _ = state.db.mailbox_expire(now() - MAILBOX_TTL);
    Ok(Json(json!({ "stored": stored, "seen": body.items.len() })))
}

/// Collect everything left since the sequence number this device last saw.
async fn sync_pull(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let caller = authenticate(&state, &headers, &query)?;
    seat(&state, &caller, &headers)?;
    let named = query
        .get("room")
        .cloned()
        .ok_or_else(|| ApiError::BadRequest("a room is required".into()))?;
    let room = allowed_room(&named, &headers, &query)?;
    let since: i64 = query.get("since").and_then(|s| s.parse().ok()).unwrap_or(0);
    let limit: usize = query
        .get("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(64)
        .clamp(1, MAX_ITEMS);

    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;
    let rows = state.db.mailbox_since(&room, since, limit)?;
    let cursor = rows.last().map(|row| row.seq).unwrap_or(since);
    let items: Vec<Value> = rows
        .into_iter()
        .map(|row| {
            json!({
                "seq": row.seq,
                "item": row.item,
                "rev": row.rev,
                "updated_at": row.updated_at,
                "device": row.device,
                "sealed": B64.encode(row.sealed),
            })
        })
        .collect();
    Ok(Json(json!({
        "items": items,
        "cursor": cursor,
        // True when there is more behind this page: the device comes straight
        // back rather than waiting for the next round.
        "more": items.len() == limit,
    })))
}

/// What is on sale, and for how much.
async fn plans(State(state): State<AppState>) -> Json<Value> {
    let mut sold: Vec<Value> = state
        .cfg
        .plan_prices
        .iter()
        .filter_map(|(key, price)| {
            let (tier, months) = key.split_once(':')?;
            let months: i64 = months.parse().ok()?;
            let allowance = state.registry.read().tier(tier);
            Some(json!({
                "tier": tier,
                "months": months,
                "price_usd": price,
                "devices": allowance.max_peers,
                "credits_week": allowance.credits_week,
                "credits_5h": allowance.credits_5h,
            }))
        })
        .collect();
    // Cheapest first, and a tier's month before its year.
    sold.sort_by(|a, b| {
        a["price_usd"]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&b["price_usd"].as_f64().unwrap_or(0.0))
    });
    Json(json!({
        "plans": sold,
        "credit_price_usd": state.cfg.credit_price_usd,
        "selling": !state.cfg.nowpayments_key.trim().is_empty()
            && !state.cfg.public_url.trim().is_empty(),
    }))
}

/// The coins the provider will take, with the icons it publishes for them.
///
/// Asked of the provider rather than kept in a list here: they add and
/// suspend coins constantly, and a list of our own would send somebody to pay
/// in something that is not being accepted this week.
async fn coins(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    if state.cfg.nowpayments_key.trim().is_empty() {
        return Err(ApiError::Forbidden("this gateway sells nothing".into()));
    }
    let response = state
        .llm
        .http
        .get("https://api.nowpayments.io/v1/full-currencies")
        .header("x-api-key", state.cfg.nowpayments_key.trim())
        .send()
        .await
        .map_err(|err| ApiError::Upstream(format!("the payment provider: {err}")))?;
    let body: Value = response
        .json()
        .await
        .map_err(|err| ApiError::Upstream(format!("the payment provider: {err}")))?;

    let empty = vec![];
    let list = body
        .get("currencies")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let coins: Vec<Value> = list
        .iter()
        .filter(|coin| coin.get("enable").and_then(Value::as_bool).unwrap_or(true))
        .filter_map(|coin| {
            let code = coin
                .get("code")
                .or_else(|| coin.get("currency"))
                .or_else(|| coin.get("ticker"))
                .and_then(Value::as_str)?
                .to_lowercase();
            if code.is_empty() {
                return None;
            }
            // The logo comes back as a path on their site as often as a URL.
            let logo = coin
                .get("logo_url")
                .or_else(|| coin.get("logoUrl"))
                .or_else(|| coin.get("image"))
                .and_then(Value::as_str)
                .map(|url| {
                    if url.starts_with("http") {
                        url.to_string()
                    } else {
                        format!(
                            "https://nowpayments.io{}{}",
                            if url.starts_with('/') { "" } else { "/" },
                            url
                        )
                    }
                });
            Some(json!({
                "code": code,
                "name": coin.get("name").and_then(Value::as_str).unwrap_or(&code),
                "logo": logo,
                "network": coin.get("network").and_then(Value::as_str),
            }))
        })
        .collect();
    Ok(Json(json!({ "coins": coins })))
}

#[derive(serde::Deserialize)]
struct SubscribeBody {
    tier: String,
    months: i64,
    pay_currency: String,
    /// Whatever the buyer wants written on the licence — a company, a
    /// nickname, an email. Kept for support and nothing else.
    #[serde(default)]
    note: String,
}

/// Start buying a plan.
///
/// Nobody is authenticated here: the whole point is that the buyer has no
/// licence yet. What comes back is an address to pay to and an order number,
/// and that number is the claim ticket — unguessable, theirs alone, and the
/// only thing that will hand over the key afterwards.
async fn subscribe(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SubscribeBody>,
) -> Result<Json<Value>, ApiError> {
    if state.cfg.nowpayments_key.trim().is_empty() || state.cfg.public_url.trim().is_empty() {
        return Err(ApiError::Forbidden("this gateway sells nothing".into()));
    }
    let tier = body.tier.trim().to_lowercase();
    let key = format!("{tier}:{}", body.months);
    let Some(price) = state.cfg.plan_prices.get(&key).copied() else {
        return Err(ApiError::BadRequest(format!("{key} is not on sale")));
    };

    // The order number is the credential, so it is made the way a credential
    // is: from the system's randomness, long enough that guessing one is not
    // a thing anybody tries twice.
    let order_id = format!("sub-{}", uuid::Uuid::new_v4().simple());
    state.db.open_purchase(&crate::registry::Purchase {
        order_id: order_id.clone(),
        tier: tier.clone(),
        months: body.months,
        devices: 0,
        note: body.note.chars().take(200).collect(),
        license_id: String::new(),
        license: String::new(),
        paid_at: 0,
        created_at: now(),
        device: device_id(&headers),
    })?;

    let callback = format!("{}/pay/ipn", state.cfg.public_url.trim_end_matches('/'));
    let response = state
        .llm
        .http
        .post("https://api.nowpayments.io/v1/payment")
        .header("x-api-key", state.cfg.nowpayments_key.trim())
        .json(&json!({
            "price_amount": price,
            "price_currency": "usd",
            "pay_currency": body.pay_currency.to_lowercase(),
            "order_id": order_id,
            "order_description": format!("VelvetDesk {tier}, {} month(s)", body.months),
            "ipn_callback_url": callback,
            "is_fee_paid_by_user": true,
        }))
        .send()
        .await
        .map_err(|err| ApiError::Upstream(format!("the payment provider: {err}")))?;

    let status = response.status();
    let answer: Value = response.json().await.unwrap_or_else(|_| json!({}));
    if !status.is_success() {
        let said = answer
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the payment provider refused the request");
        log::warn!("subscribe refused: {status} {answer}");
        return Err(ApiError::BadRequest(said.to_string()));
    }

    Ok(Json(json!({
        "order_id": order_id,
        "payment_id": answer.get("payment_id"),
        "pay_address": answer.get("pay_address"),
        "pay_amount": answer.get("pay_amount"),
        "pay_currency": answer.get("pay_currency"),
        "network": answer.get("network"),
        "price_usd": price,
        "tier": tier,
        "months": body.months,
    })))
}

/// Has it been paid, and what is the key?
///
/// The app asks this every few seconds while the operator is looking at the
/// payment screen. Until the money lands it says "waiting" and nothing else;
/// afterwards it hands over the licence, as many times as it is asked, to
/// whoever has the order number.
async fn order_status(
    State(state): State<AppState>,
    Path(order): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let found = state
        .db
        .purchase(&order)?
        .ok_or_else(|| ApiError::BadRequest("no such order".into()))?;
    Ok(Json(json!({
        "order_id": found.order_id,
        "tier": found.tier,
        "months": found.months,
        "paid": found.paid_at > 0,
        "license": found.license,
        "license_id": found.license_id,
    })))
}

/// Everything this machine has bought, with the keys it earned.
///
/// For the operator who closed the window before copying their key, and for
/// the one who reinstalled: the machine asks, and gets back what it paid for.
async fn orders_for_device(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let device = device_id(&headers);
    if device == "unnamed" {
        return Err(ApiError::BadRequest("no device was named".into()));
    }
    let orders: Vec<Value> = state
        .db
        .purchases_by_device(&device)?
        .into_iter()
        .map(|order| {
            json!({
                "order_id": order.order_id,
                "tier": order.tier,
                "months": order.months,
                "paid": order.paid_at > 0,
                "license": order.license,
                "license_id": order.license_id,
                "created_at": order.created_at,
            })
        })
        .collect();
    Ok(Json(json!({ "orders": orders })))
}

#[derive(serde::Deserialize)]
struct CheckoutBody {
    /// How many credits to buy.
    credits: f64,
    /// Which coin to pay in — the provider's own code, `usdttrc20` and the
    /// like. The app gets the list from the provider, so this is passed
    /// through rather than checked against anything here.
    pay_currency: String,
}

/// Open a payment for more credits.
///
/// The licence is the identity: whoever holds it is who gets the credits, so
/// there is no account to make and nothing to log into. The order carries the
/// licence id and the number of credits, and comes back untouched in the
/// notice — which is how the money finds its way to the right wallet.
///
/// The callback address is set per payment rather than in the provider's
/// dashboard. That is what lets several products share one payment account:
/// each tells the provider where its own notices go.
async fn checkout(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(body): Json<CheckoutBody>,
) -> Result<Json<Value>, ApiError> {
    let caller = authenticate(&state, &headers, &query)?;
    if !caller.license.tier.eq_ignore_ascii_case("business") {
        return Err(ApiError::Forbidden(
            "credits are sold on business licences".into(),
        ));
    }
    if state.cfg.nowpayments_key.trim().is_empty() || state.cfg.public_url.trim().is_empty() {
        return Err(ApiError::Forbidden(
            "this gateway is not set up to sell credits".into(),
        ));
    }
    if !body.credits.is_finite() || body.credits < 100.0 || body.credits > 1_000_000.0 {
        return Err(ApiError::BadRequest(
            "credits must be between 100 and 1 000 000".into(),
        ));
    }

    let dollars = (body.credits * state.cfg.credit_price_usd * 100.0).round() / 100.0;
    let order = format!("{}:{}", caller.license.license_id, body.credits.round());
    let callback = format!("{}/pay/ipn", state.cfg.public_url.trim_end_matches('/'));

    let response = state
        .llm
        .http
        .post("https://api.nowpayments.io/v1/payment")
        .header("x-api-key", state.cfg.nowpayments_key.trim())
        .json(&json!({
            "price_amount": dollars,
            "price_currency": "usd",
            "pay_currency": body.pay_currency.to_lowercase(),
            "order_id": order,
            "order_description": format!("VelvetDesk {} credits", body.credits.round()),
            "ipn_callback_url": callback,
            "is_fee_paid_by_user": true,
        }))
        .send()
        .await
        .map_err(|err| ApiError::Upstream(format!("the payment provider: {err}")))?;

    let status = response.status();
    let answer: Value = response.json().await.unwrap_or_else(|_| json!({}));
    if !status.is_success() {
        let said = answer
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the payment provider refused the request");
        log::warn!("checkout refused: {status} {answer}");
        return Err(ApiError::BadRequest(said.to_string()));
    }

    Ok(Json(json!({
        "payment_id": answer.get("payment_id"),
        "pay_address": answer.get("pay_address"),
        "pay_amount": answer.get("pay_amount"),
        "pay_currency": answer.get("pay_currency"),
        "network": answer.get("network"),
        "price_usd": dollars,
        "credits": body.credits.round(),
    })))
}

/// A payment cleared: put the credits it bought on the licence.
///
/// The provider signs every notification with a shared secret, and that
/// signature is the whole of the trust here: the endpoint has to be reachable
/// without authentication, so anybody can post to it and only the signature
/// separates a payment from a wish. Unsigned, mis-signed, or arriving at a
/// gateway with no secret configured, it is refused without being read.
///
/// Which licence gets the credits comes from `order_id`, written when the
/// payment is created: `<licence id>:<credits>`. The provider echoes it back
/// untouched, and it is the only thing here we put there ourselves.
///
/// The same payment is announced several times as it moves through its
/// states, and a retry storm follows any answer that is not 2xx — so a
/// payment already credited is answered cheerfully and credited once.
async fn payment_notice(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    if state.cfg.ipn_secret.trim().is_empty() {
        return Err(ApiError::Forbidden(
            "payment notifications are not configured on this gateway".into(),
        ));
    }
    let sent = headers
        .get("x-nowpayments-sig")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    if sent.is_empty() {
        return Err(ApiError::Unauthorized(
            "the notice carried no signature".into(),
        ));
    }

    let parsed: Value = serde_json::from_slice(&body)
        .map_err(|err| ApiError::BadRequest(format!("cannot read the notice: {err}")))?;
    if !signature_matches(&parsed, &state.cfg.ipn_secret, &sent) {
        log::warn!("payment notice with a signature that does not match");
        return Err(ApiError::Unauthorized(
            "the signature does not match".into(),
        ));
    }

    let status = parsed
        .get("payment_status")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    // Only a payment that finished buys anything. The earlier states are
    // announced too, and answering them 200 stops the retries.
    if status != "finished" && status != "confirmed" {
        return Ok(Json(json!({ "ok": true, "ignored": status })));
    }

    let order = parsed
        .get("order_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    // A subscription being bought: mint the licence it paid for and leave it
    // where the buyer's order number will find it.
    if order.starts_with("sub-") {
        return deliver_subscription(&state, &order).await;
    }

    let (license_id, credits) = match order.split_once(':') {
        Some((id, credits)) => (id.trim().to_string(), credits.trim().parse::<f64>().ok()),
        None => (String::new(), None),
    };
    let Some(credits) = credits.filter(|amount| *amount > 0.0) else {
        log::warn!("payment {order} names no licence and amount to credit");
        return Ok(Json(json!({ "ok": true, "ignored": "order_id" })));
    };
    if license_id.is_empty() {
        return Ok(Json(json!({ "ok": true, "ignored": "order_id" })));
    }

    let payment = parsed
        .get("payment_id")
        .map(|id| id.to_string())
        .unwrap_or_else(|| order.clone());
    if state.db.payment_seen(&payment)? {
        // Already credited: the provider is repeating itself, which it does
        // by design, and paying twice for one payment is the one outcome
        // worth being careful about.
        return Ok(Json(json!({ "ok": true, "already": true })));
    }

    let left = state
        .db
        .wallet_add(&license_id, credits, now())
        .map_err(|err| ApiError::Upstream(err.to_string()))?;
    state
        .db
        .note_payment(&payment, &license_id, credits, now())?;
    log::info!("payment {payment}: {credits} credits to {license_id}, {left} left");
    Ok(Json(json!({ "ok": true, "credits_left": left })))
}

/// Mint the licence a paid subscription earned, and file it under the order.
///
/// Minting needs the private half of the signing key, which lives beside the
/// gateway because the admin page already mints with it. Nothing else in the
/// process reads it, and it never leaves the machine.
///
/// Called only from a notice whose signature has already been checked, and
/// only once: a purchase that already has a licence keeps the one it has,
/// however many times the provider repeats itself.
async fn deliver_subscription(state: &AppState, order: &str) -> Result<Json<Value>, ApiError> {
    let Some(purchase) = state.db.purchase(order)? else {
        log::warn!("payment for an order nobody opened: {order}");
        return Ok(Json(json!({ "ok": true, "ignored": "order" })));
    };
    if purchase.paid_at > 0 {
        return Ok(Json(json!({ "ok": true, "already": true })));
    }

    let path =
        std::env::var("VD_LICENSE_KEY_FILE").unwrap_or_else(|_| "license-signing.key".to_string());
    let text = std::fs::read_to_string(&path)
        .map_err(|err| ApiError::Upstream(format!("cannot read the signing key: {err}")))?;
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .ok()
        .and_then(|raw| <[u8; 32]>::try_from(raw).ok())
        .ok_or_else(|| ApiError::Upstream("the signing key file does not hold a key".into()))?;
    let signer = ed25519_dalek::SigningKey::from_bytes(&bytes);

    let devices = if purchase.devices > 0 {
        purchase.devices as u32
    } else {
        state.registry.read().tier(&purchase.tier).max_peers
    };
    // A licence id somebody could guess is a licence somebody could ask us to
    // extend, so it is random rather than sequential.
    let license_id = format!("VD-{}", uuid::Uuid::new_v4().simple());
    let license = vd_license::License {
        license_id: license_id.clone(),
        tier: purchase.tier.clone(),
        expires_at: now() + purchase.months.max(1) * 30 * 24 * 3600,
        max_peers: devices,
    };
    let token = vd_license::mint(&signer, &license);

    state.db.record_license(
        &license.license_id,
        &license.tier,
        license.expires_at,
        license.max_peers,
        &format!("bought {order}: {}", purchase.note),
        now(),
    )?;
    state
        .db
        .deliver_purchase(order, &license.license_id, &token, now())?;
    log::info!(
        "order {order} paid: {} for {} month(s), {} device(s)",
        license.tier,
        purchase.months,
        devices
    );
    Ok(Json(json!({ "ok": true, "issued": license.license_id })))
}

/// The provider's signature: HMAC-SHA512 over the JSON with its keys sorted,
/// nested objects included, compared as hex.
fn signature_matches(body: &Value, secret: &str, sent: &str) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha512;

    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                let mut out = serde_json::Map::new();
                for key in keys {
                    out.insert(key.clone(), sorted(&map[key]));
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }

    let Ok(mut mac) = Hmac::<Sha512>::new_from_slice(secret.trim().as_bytes()) else {
        return false;
    };
    let Ok(canonical) = serde_json::to_string(&sorted(body)) else {
        return false;
    };
    mac.update(canonical.as_bytes());
    let mine = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    // Compared in full, so the answer takes the same time whatever was sent.
    mine.len() == sent.len()
        && mine
            .bytes()
            .zip(sent.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

async fn sync_ws(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let caller = authenticate(&state, &headers, &query)?;
    let room = query
        .get("room")
        .filter(|room| !room.is_empty() && room.len() <= 64)
        .cloned()
        .ok_or_else(|| ApiError::BadRequest("a room is required".into()))?;

    // The seat register is the real limit — a sync round lasts seconds, so
    // counting who is in the room at this instant would let any number of
    // machines share a licence by never overlapping. What the room still
    // enforces is that a round is between two devices and not a crowd.
    seat(&state, &caller, &headers)?;
    let channel = state.room(&room);
    let here = channel.receiver_count();
    if here >= caller.peers.max(2) as usize {
        return Err(ApiError::Forbidden(format!(
            "{here} device(s) are already in this room"
        )));
    }

    Ok(upgrade.on_upgrade(move |socket| relay(socket, state, room)))
}

/// Forward frames between the members of one room.
///
/// Every connection gets a number so it does not receive its own frames back;
/// beyond that the gateway does not look inside, because it cannot.
async fn relay(socket: WebSocket, state: AppState, room: String) {
    use futures_util::{SinkExt, StreamExt};

    let channel = state.room(&room);
    let mut inbox = channel.subscribe();
    let me = NEXT_PEER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (mut sink, mut stream) = socket.split();

    loop {
        tokio::select! {
            incoming = stream.next() => match incoming {
                Some(Ok(WsMessage::Binary(bytes))) => {
                    // No receivers means nobody else is here yet; the sender
                    // will try again on its next round.
                    let _ = channel.send((me, bytes.to_vec()));
                }
                Some(Ok(WsMessage::Close(_))) | None => break,
                Some(Ok(_)) => continue,
                Some(Err(_)) => break,
            },
            relayed = inbox.recv() => match relayed {
                Ok((from, bytes)) if from != me => {
                    if sink.send(WsMessage::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                Ok(_) => continue,
                // Lagged behind the burst: the round is spoiled, and the next
                // one will start from a fresh digest anyway.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }

    drop(inbox);
    state.drop_room_if_empty(&room);
}

static NEXT_PEER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn the_licence_is_taken_from_any_of_the_three_places() {
        let empty = HashMap::new();
        assert_eq!(
            token(&headers(&[("authorization", "Bearer VD.a.b")]), &empty).as_deref(),
            Some("VD.a.b")
        );
        assert_eq!(
            token(&headers(&[("x-goog-api-key", "VD.a.b")]), &empty).as_deref(),
            Some("VD.a.b")
        );
        let query = HashMap::from([("key".to_string(), "VD.a.b".to_string())]);
        assert_eq!(token(&HeaderMap::new(), &query).as_deref(), Some("VD.a.b"));
        assert_eq!(token(&HeaderMap::new(), &empty), None);
    }

    /// A bearer header without the word "Bearer" is what several clients
    /// send, and refusing them would be pedantry with a support cost.
    #[test]
    fn a_bare_authorization_header_still_works() {
        assert_eq!(
            token(&headers(&[("authorization", "VD.a.b")]), &HashMap::new()).as_deref(),
            Some("VD.a.b")
        );
    }

    /// The provider's own example, signed with a known secret: the body is
    /// sorted by key — nested objects included — before it is hashed, and a
    /// body that has been tampered with does not match.
    #[test]
    fn a_payment_notice_is_trusted_only_when_it_is_signed() {
        use hmac::{Hmac, Mac};
        use sha2::Sha512;

        let secret = "ipn-secret";
        let body: Value = serde_json::json!({
            "payment_status": "finished",
            "order_id": "acme-1:5000",
            "payment_id": 123456789u64,
            "fee": { "serviceFee": 0.1, "currency": "btc" },
        });

        let mut sorted = serde_json::Map::new();
        sorted.insert(
            "fee".into(),
            serde_json::json!({ "currency": "btc", "serviceFee": 0.1 }),
        );
        sorted.insert("order_id".into(), serde_json::json!("acme-1:5000"));
        sorted.insert("payment_id".into(), serde_json::json!(123456789u64));
        sorted.insert("payment_status".into(), serde_json::json!("finished"));
        let mut mac = Hmac::<Sha512>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(
            serde_json::to_string(&Value::Object(sorted))
                .unwrap()
                .as_bytes(),
        );
        let expected = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();

        assert!(signature_matches(&body, secret, &expected));
        assert!(!signature_matches(&body, "another-secret", &expected));

        let tampered: Value = serde_json::json!({
            "payment_status": "finished",
            "order_id": "acme-1:500000",
            "payment_id": 123456789u64,
            "fee": { "serviceFee": 0.1, "currency": "btc" },
        });
        assert!(
            !signature_matches(&tampered, secret, &expected),
            "an order rewritten in flight must not pass"
        );
    }

    #[test]
    fn the_credit_headers_carry_the_tighter_window() {
        let state = Allowance {
            left_5h: 12.5,
            left_week: 900.0,
            wallet: 0.0,
            reset_at: 1_700_000_000,
        };
        let map = credit_headers(&state);
        assert_eq!(map["x-vd-credits-left"], "12.50");
        assert_eq!(map["x-vd-window-reset"], "1700000000");
    }
}
