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
                        "message": "the licence has spent its credits for this window",
                        "reset_at": state.reset_at,
                        "credits_left": state.left(),
                    }
                }),
            ),
            ApiError::Upstream(message) => (
                StatusCode::BAD_GATEWAY,
                json!({ "error": { "type": "upstream", "message": message } }),
            ),
        };
        (status, Json(body)).into_response()
    }
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
    Ok(Caller { license, tier })
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
    let state_now = allowance(&state.db, &caller.license.license_id, caller.tier, now())?;
    Ok(Json(json!({
        "license_id": caller.license.license_id,
        "tier": caller.license.tier,
        "expires_at": caller.license.expires_at,
        "max_peers": caller.license.max_peers.max(caller.tier.max_peers),
        "credits_left_5h": state_now.left_5h,
        "credits_left_week": state_now.left_week,
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
        let body = translate::oai_completion(&id, created, &model, &response);
        return Ok((credit_headers(&after), Json(body)).into_response());
    }

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let task = state.clone();
    let license_id = caller.license.license_id.clone();
    let tier = caller.tier;
    tokio::spawn(async move {
        let sender = tx.clone();
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
                // an event rather than a status code nobody will see.
                let body = json!({ "error": { "type": "upstream", "message": err.to_string() } });
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
                let body =
                    json!({ "error": { "status": "UNAVAILABLE", "message": err.to_string() } });
                let _ = tx.send(Event::default().data(body.to_string()));
            }
        }
    });

    let stream = UnboundedReceiverStream::new(rx).map(Ok::<Event, Infallible>);
    Ok((credit_headers(&before), Sse::new(stream)).into_response())
}

// ---------------------------------------------------------------------- sync

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

    // The licence says how many devices may be paired, and the room is where
    // that is actually enforced: a third laptop is turned away at the door
    // rather than discovering later that nothing arrived.
    let channel = state.room(&room);
    let peers = channel.receiver_count();
    let allowed = caller.license.max_peers.max(caller.tier.max_peers).max(1) as usize;
    if peers >= allowed {
        return Err(ApiError::Forbidden(format!(
            "this licence pairs {allowed} device(s), and {peers} are already connected"
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

    #[test]
    fn the_credit_headers_carry_the_tighter_window() {
        let state = Allowance {
            left_5h: 12.5,
            left_week: 900.0,
            reset_at: 1_700_000_000,
        };
        let map = credit_headers(&state);
        assert_eq!(map["x-vd-credits-left"], "12.50");
        assert_eq!(map["x-vd-window-reset"], "1700000000");
    }
}
