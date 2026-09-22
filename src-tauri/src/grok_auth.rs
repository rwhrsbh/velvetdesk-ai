//! Device-code login for the Grok subscription.
//!
//! The access token lives a few hours. What we keep is the refresh token, and
//! every model call asks for a fresh access token a few minutes before the
//! old one dies. The CLI version header is applied later, at the request.

use serde::Serialize;
use serde_json::Value;

use crate::config::GrokSession;
use crate::error::{AppError, Result};
use crate::state::AppState;

const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const DEVICE_URL: &str = "https://auth.x.ai/oauth2/device/code";
const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
pub const PROVIDER_ID: &str = "grok";

#[derive(Debug, Clone, Serialize)]
pub struct GrokCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub interval: u64,
    pub expires_in: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GrokPoll {
    pub status: String,
    pub message: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GrokStatus {
    pub signed_in: bool,
    pub expires_at: i64,
}

/// Refresh a stored session when it is close to expiry. No session is fine.
pub async fn prepare(state: &AppState) -> Result<()> {
    let session = state.secrets.read().grok.clone();
    let Some(session) = session else {
        return Ok(());
    };
    if session.refresh_token.is_empty() {
        return Ok(());
    }
    let now = chrono::Utc::now().timestamp();
    if !session.access_token.is_empty() && session.expires_at.saturating_sub(now) > 300 {
        return Ok(());
    }
    let fresh = refresh(&state.llm.http, &session).await?;
    store(state, fresh, false)?;
    Ok(())
}

#[tauri::command]
pub async fn grok_login_start(state: tauri::State<'_, AppState>) -> Result<GrokCode> {
    let value = post_form(
        &state.llm.http,
        DEVICE_URL,
        &[("client_id", CLIENT_ID), ("scope", SCOPE)],
    )
    .await?;
    if let Some(err) = value.get("error").and_then(Value::as_str) {
        return Err(AppError::Provider(describe(&value, err)));
    }
    Ok(GrokCode {
        device_code: text(&value, "device_code"),
        user_code: text(&value, "user_code"),
        verification_uri: text(&value, "verification_uri"),
        verification_uri_complete: {
            let complete = text(&value, "verification_uri_complete");
            if complete.is_empty() {
                text(&value, "verification_uri")
            } else {
                complete
            }
        },
        interval: value
            .get("interval")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .max(2),
        expires_in: value
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(600),
    })
}

#[tauri::command]
pub async fn grok_login_poll(
    state: tauri::State<'_, AppState>,
    device_code: String,
) -> Result<GrokPoll> {
    let value = post_form(
        &state.llm.http,
        TOKEN_URL,
        &[
            ("grant_type", DEVICE_GRANT),
            ("client_id", CLIENT_ID),
            ("device_code", device_code.as_str()),
        ],
    )
    .await?;
    match value.get("error").and_then(Value::as_str) {
        Some("authorization_pending") | Some("slow_down") => Ok(GrokPoll {
            status: "pending".into(),
            message: String::new(),
            expires_at: 0,
        }),
        Some(err) => Ok(GrokPoll {
            status: "error".into(),
            message: describe(&value, err),
            expires_at: 0,
        }),
        None => {
            let session = session_from(&value, None)?;
            let expires_at = session.expires_at;
            store(&state, session, true)?;
            Ok(GrokPoll {
                status: "ok".into(),
                message: String::new(),
                expires_at,
            })
        }
    }
}

#[tauri::command]
pub fn grok_logout(state: tauri::State<'_, AppState>) -> Result<GrokStatus> {
    let mut secrets = state.secrets.read().clone();
    secrets.grok = None;
    secrets.keys.remove(PROVIDER_ID);
    state.save_secrets(secrets)?;
    Ok(GrokStatus {
        signed_in: false,
        expires_at: 0,
    })
}

#[tauri::command]
pub fn grok_status(state: tauri::State<'_, AppState>) -> Result<GrokStatus> {
    let secrets = state.secrets.read();
    let session = secrets.grok.as_ref();
    Ok(GrokStatus {
        signed_in: session.is_some_and(|s| !s.refresh_token.is_empty()),
        expires_at: session.map(|s| s.expires_at).unwrap_or(0),
    })
}

async fn refresh(http: &reqwest::Client, session: &GrokSession) -> Result<GrokSession> {
    let value = post_form(
        http,
        TOKEN_URL,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", session.refresh_token.as_str()),
        ],
    )
    .await?;
    if let Some(err) = value.get("error").and_then(Value::as_str) {
        return Err(AppError::Provider(describe(&value, err)));
    }
    session_from(&value, Some(session))
}

fn session_from(value: &Value, previous: Option<&GrokSession>) -> Result<GrokSession> {
    let access = text(value, "access_token");
    if access.is_empty() {
        return Err(AppError::Provider("вход Grok не вернул токен".into()));
    }
    let mut refresh_token = text(value, "refresh_token");
    if refresh_token.is_empty() {
        refresh_token = previous
            .map(|s| s.refresh_token.clone())
            .unwrap_or_default();
    }
    if refresh_token.is_empty() {
        return Err(AppError::Provider(
            "вход Grok не вернул refresh token — сессия не переживёт перезапуск".into(),
        ));
    }
    let expires_in = value
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(3600);
    Ok(GrokSession {
        access_token: access,
        refresh_token,
        expires_at: chrono::Utc::now().timestamp() + expires_in,
    })
}

fn store(state: &AppState, session: GrokSession, make_active: bool) -> Result<()> {
    let mut secrets = state.secrets.read().clone();
    secrets
        .keys
        .insert(PROVIDER_ID.to_string(), vec![session.access_token.clone()]);
    secrets.grok = Some(session);
    state.save_secrets(secrets)?;
    if make_active {
        let mut settings = state.settings.read().clone();
        settings.active_provider = Some(PROVIDER_ID.to_string());
        if let Some(provider) = settings.providers.iter_mut().find(|p| p.id == PROVIDER_ID) {
            if provider.model.trim().is_empty() {
                provider.model = "grok-4.7".into();
            }
        }
        state.save_settings(settings)?;
    }
    Ok(())
}

async fn post_form(http: &reqwest::Client, url: &str, fields: &[(&str, &str)]) -> Result<Value> {
    let mut form = std::collections::HashMap::new();
    for (key, value) in fields {
        form.insert(*key, *value);
    }
    let response = http
        .post(url)
        .form(&form)
        .send()
        .await
        .map_err(|e| AppError::Http(e.to_string()))?;
    let text = response
        .text()
        .await
        .map_err(|e| AppError::Http(e.to_string()))?;
    serde_json::from_str(&text).map_err(|_| AppError::Provider(text.chars().take(400).collect()))
}

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn describe(value: &Value, err: &str) -> String {
    let detail = value
        .get("error_description")
        .and_then(Value::as_str)
        .unwrap_or("");
    if detail.is_empty() {
        err.to_string()
    } else {
        format!("{err}: {detail}")
    }
}
