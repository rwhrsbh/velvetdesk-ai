//! Device-code login for a Grok subscription upstream.
//!
//! The same public client the desktop uses. What we keep is the refresh
//! token; the access token is what the key pool sends, and it is replaced a
//! few minutes before it dies. The auth host is a parameter so a test can
//! stand in for auth.x.ai without changing the exchange.

use serde_json::{json, Value};

use crate::db::{Db, GrokSessionRow};

pub const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
pub const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// Refresh when the access token is missing or this close to dying.
const REFRESH_SKEW_SECS: i64 = 300;

/// Where chat for a subscription upstream is sent. Fixed: a pasted
/// console.x.ai key is a different product and is not accepted here.
pub const SUBSCRIPTION_BASE: &str = "https://cli-chat-proxy.grok.com/v1";

#[derive(Debug, Clone)]
pub struct AuthUrls {
    pub device_url: String,
    pub token_url: String,
}

impl AuthUrls {
    pub fn production() -> AuthUrls {
        AuthUrls {
            device_url: "https://auth.x.ai/oauth2/device/code".into(),
            token_url: "https://auth.x.ai/oauth2/token".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub interval: u64,
    pub expires_in: u64,
}

#[derive(Debug, Clone)]
pub struct SessionTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone)]
pub enum PollOutcome {
    Pending,
    Error(String),
    Approved(SessionTokens),
}

pub fn access_is_stale(access_token: &str, expires_at: i64, now: i64) -> bool {
    access_token.trim().is_empty() || expires_at.saturating_sub(now) <= REFRESH_SKEW_SECS
}

pub async fn start_device(http: &reqwest::Client, auth: &AuthUrls) -> Result<DeviceCode, String> {
    let value = post_form(
        http,
        &auth.device_url,
        &[("client_id", CLIENT_ID), ("scope", SCOPE)],
    )
    .await?;
    if let Some(err) = value.get("error").and_then(Value::as_str) {
        return Err(describe(&value, err));
    }
    let verification_uri = text(&value, "verification_uri");
    let complete = text(&value, "verification_uri_complete");
    Ok(DeviceCode {
        device_code: text(&value, "device_code"),
        user_code: text(&value, "user_code"),
        verification_uri: verification_uri.clone(),
        verification_uri_complete: if complete.is_empty() {
            verification_uri
        } else {
            complete
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

pub async fn poll_device(
    http: &reqwest::Client,
    auth: &AuthUrls,
    device_code: &str,
) -> Result<PollOutcome, String> {
    let value = post_form(
        http,
        &auth.token_url,
        &[
            ("grant_type", DEVICE_GRANT),
            ("client_id", CLIENT_ID),
            ("device_code", device_code),
        ],
    )
    .await?;
    match value.get("error").and_then(Value::as_str) {
        Some("authorization_pending") | Some("slow_down") => Ok(PollOutcome::Pending),
        Some(err) => Ok(PollOutcome::Error(describe(&value, err))),
        None => Ok(PollOutcome::Approved(tokens_from(&value, None)?)),
    }
}

pub async fn refresh_session(
    http: &reqwest::Client,
    auth: &AuthUrls,
    session: &GrokSessionRow,
) -> Result<SessionTokens, String> {
    let value = post_form(
        http,
        &auth.token_url,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", session.refresh_token.as_str()),
        ],
    )
    .await?;
    if let Some(err) = value.get("error").and_then(Value::as_str) {
        return Err(describe(&value, err));
    }
    tokens_from(&value, Some(session.refresh_token.as_str()))
}

/// Replace access tokens that are missing or close to expiry.
///
/// A refresh that succeeds is written before the next one is attempted, so a
/// later failure does not throw away a token already in hand. `Ok(true)`
/// means the pool should be rebuilt. One failure with nothing saved is `Err`.
pub async fn refresh_due(
    db: &Db,
    http: &reqwest::Client,
    auth: &AuthUrls,
    now: i64,
) -> Result<bool, String> {
    let sessions = db.grok_sessions().map_err(|err| err.to_string())?;
    let mut changed = false;
    let mut failure: Option<String> = None;
    for session in sessions {
        if session.refresh_token.trim().is_empty()
            || !access_is_stale(&session.access_token, session.expires_at, now)
        {
            continue;
        }
        match refresh_session(http, auth, &session).await {
            Ok(fresh) => {
                db.update_grok_session(
                    session.id,
                    &fresh.access_token,
                    &fresh.refresh_token,
                    fresh.expires_at,
                )
                .map_err(|err| err.to_string())?;
                changed = true;
                log::info!(
                    "refreshed grok session {} on {}",
                    session.id,
                    session.upstream_id
                );
            }
            Err(err) => {
                log::warn!(
                    "grok session {} on {} was not refreshed: {err}",
                    session.id,
                    session.upstream_id
                );
                failure = Some(err);
            }
        }
    }
    if changed {
        return Ok(true);
    }
    if let Some(err) = failure {
        return Err(err);
    }
    Ok(false)
}

/// What the admin page is allowed to see. Tokens stay in the database.
pub fn public_status(db: &Db, upstream_id: Option<&str>) -> Result<Value, String> {
    let sessions = db.grok_sessions().map_err(|err| err.to_string())?;
    let sessions: Vec<Value> = sessions
        .into_iter()
        .filter(|row| match upstream_id {
            Some(id) => row.upstream_id == id,
            None => true,
        })
        .map(|row| {
            json!({
                "id": row.id,
                "upstream_id": row.upstream_id,
                "masked": vd_llm::mask_key(&row.access_token),
                "expires_at": row.expires_at,
            })
        })
        .collect();
    let signed_in = !sessions.is_empty();
    Ok(json!({
        "signed_in": signed_in,
        "status": if signed_in { "signed in" } else { "not signed in" },
        "sessions": sessions,
    }))
}

fn tokens_from(value: &Value, previous_refresh: Option<&str>) -> Result<SessionTokens, String> {
    let access = text(value, "access_token");
    if access.is_empty() {
        return Err("вход Grok не вернул токен".into());
    }
    let mut refresh_token = text(value, "refresh_token");
    if refresh_token.is_empty() {
        refresh_token = previous_refresh.unwrap_or("").to_string();
    }
    if refresh_token.is_empty() {
        return Err("вход Grok не вернул refresh token — сессия не переживёт перезапуск".into());
    }
    let expires_in = value
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(3600);
    Ok(SessionTokens {
        access_token: access,
        refresh_token,
        expires_at: chrono::Utc::now().timestamp() + expires_in,
    })
}

async fn post_form(
    http: &reqwest::Client,
    url: &str,
    fields: &[(&str, &str)],
) -> Result<Value, String> {
    let response = http
        .post(url)
        .form(fields)
        .send()
        .await
        .map_err(|err| err.to_string())?;
    let text = response.text().await.map_err(|err| err.to_string())?;
    serde_json::from_str(&text).map_err(|_| text.chars().take(400).collect())
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    use axum::extract::State;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use serde_json::{json, Value};

    use super::*;
    use crate::config::GatewayConfig;
    use crate::registry::{provider_for, ModelRow, Routing, UpstreamRow};
    use crate::state::AppState;
    use vd_llm::provider::ProviderKind;

    struct StandIn {
        base: String,
        refreshes: Arc<AtomicU32>,
    }

    fn decode_component(raw: &str) -> String {
        let mut out = Vec::new();
        let bytes = raw.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'+' => {
                    out.push(b' ');
                    index += 1;
                }
                b'%' if index + 2 < bytes.len() => {
                    let hex = &raw[index + 1..index + 3];
                    if let Ok(byte) = u8::from_str_radix(hex, 16) {
                        out.push(byte);
                    }
                    index += 3;
                }
                byte => {
                    out.push(byte);
                    index += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    fn parse_form(body: &str) -> HashMap<String, String> {
        let mut form = HashMap::new();
        for pair in body.split('&') {
            if pair.is_empty() {
                continue;
            }
            let mut parts = pair.splitn(2, '=');
            let key = decode_component(parts.next().unwrap_or(""));
            let value = decode_component(parts.next().unwrap_or(""));
            form.insert(key, value);
        }
        form
    }

    fn field<'a>(form: &'a HashMap<String, String>, key: &str) -> &'a str {
        form.get(key).map(String::as_str).unwrap_or("")
    }

    async fn device(body: String) -> Json<Value> {
        let form = parse_form(&body);
        if field(&form, "client_id") != CLIENT_ID || field(&form, "scope") != SCOPE {
            return Json(json!({
                "error": "invalid_client",
                "error_description": "client or scope",
            }));
        }
        Json(json!({
            "device_code": "approved-code",
            "user_code": "WORD-CODE",
            "verification_uri": "https://auth.example/device",
            "verification_uri_complete": "https://auth.example/device?user_code=WORD-CODE",
            "interval": 1,
            "expires_in": 600
        }))
    }

    async fn token(State(hits): State<Arc<AtomicU32>>, body: String) -> Json<Value> {
        let form = parse_form(&body);
        if field(&form, "client_id") != CLIENT_ID {
            return Json(json!({"error": "invalid_client"}));
        }
        let grant = field(&form, "grant_type");
        if grant == "refresh_token" {
            let n = hits.fetch_add(1, Ordering::SeqCst) + 1;
            // No refresh_token: the previous one has to be kept.
            return Json(json!({
                "access_token": format!("fresh-access-token-{n:04}-0123456789abcdef"),
                "expires_in": 3600,
                "token_type": "Bearer"
            }));
        }
        if grant != DEVICE_GRANT {
            return Json(json!({
                "error": "unsupported_grant",
                "error_description": grant
            }));
        }
        let body = match field(&form, "device_code") {
            "pending-code" => json!({"error": "authorization_pending"}),
            "slow-code" => json!({"error": "slow_down"}),
            "denied-code" => json!({
                "error": "access_denied",
                "error_description": "the operator said no"
            }),
            "approved-code" => json!({
                "access_token": "approved-access-token-0123456789abcdef",
                "refresh_token": "approved-refresh-token-0123456789abcdef",
                "expires_in": 3600
            }),
            other => json!({"error": "invalid_request", "error_description": other}),
        };
        Json(body)
    }

    async fn health() -> &'static str {
        "ok"
    }

    async fn stand_in() -> StandIn {
        let refreshes = Arc::new(AtomicU32::new(0));
        let app = Router::new()
            .route("/health", get(health))
            .route("/device", post(device))
            .route("/token", post(token))
            .with_state(refreshes.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let base = format!("http://{address}");
        let client = reqwest::Client::new();
        for _ in 0..50 {
            if client
                .get(format!("{base}/health"))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        StandIn { base, refreshes }
    }

    fn auth(base: &str) -> AuthUrls {
        AuthUrls {
            device_url: format!("{base}/device"),
            token_url: format!("{base}/token"),
        }
    }

    fn temp_db_path() -> std::path::PathBuf {
        let name = format!("vd-grok-{}.db", rand::random::<u64>());
        std::env::temp_dir().join(name)
    }

    fn remove_db(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
        let text = path.display().to_string();
        let _ = std::fs::remove_file(format!("{text}-wal"));
        let _ = std::fs::remove_file(format!("{text}-shm"));
    }

    fn grok_upstream() -> UpstreamRow {
        UpstreamRow {
            id: "my-grok".into(),
            label: "Grok".into(),
            kind: ProviderKind::OpenaiCompatible,
            base_url: "https://evil.example/v1".into(),
            api_version: "v1".into(),
            reasoning_dialect: "auto".into(),
            extra_headers: vec![
                ("X-XAI-Token-Auth".into(), "stolen".into()),
                ("x-grok-client-version".into(), "0.0.1".into()),
                ("X-Custom".into(), "keep".into()),
            ],
            enabled: true,
            position: 0,
            grok: true,
            key_count: 0,
        }
    }

    fn openai_upstream() -> UpstreamRow {
        UpstreamRow {
            id: "openai-main".into(),
            label: "OpenAI".into(),
            kind: ProviderKind::OpenaiCompatible,
            base_url: "https://api.openai.com/v1".into(),
            api_version: "v1".into(),
            reasoning_dialect: "auto".into(),
            extra_headers: vec![("X-XAI-Token-Auth".into(), "leave".into())],
            enabled: true,
            position: 1,
            grok: false,
            key_count: 0,
        }
    }

    fn model(name: &str, upstream_id: &str) -> ModelRow {
        ModelRow {
            name: name.into(),
            upstream_id: upstream_id.into(),
            upstream_name: String::new(),
            price_in: 1.0,
            price_cached: None,
            price_out: 2.0,
            context_tokens: Some(500_000),
            enabled: true,
            position: 0,
            voice: false,
            images: false,
            vision: false,
            price_request: 0.0,
            routing: Routing::default(),
        }
    }

    #[tokio::test]
    async fn device_code_is_pending_until_approved_or_refused() {
        let stand = stand_in().await;
        let http = reqwest::Client::new();
        let urls = auth(&stand.base);

        let started = start_device(&http, &urls).await.unwrap();
        assert_eq!(started.user_code, "WORD-CODE");
        assert!(started.verification_uri_complete.contains("WORD-CODE"));
        assert!(!started.device_code.is_empty());

        // The stand-in keys off the device code, so these three are the
        // states the admin page has to sit in.
        let urls_pending = urls.clone();
        let pending = poll_device(&http, &urls_pending, "pending-code")
            .await
            .unwrap();
        assert!(matches!(pending, PollOutcome::Pending));
        let slow = poll_device(&http, &urls, "slow-code").await.unwrap();
        assert!(matches!(slow, PollOutcome::Pending));
        let denied = poll_device(&http, &urls, "denied-code").await.unwrap();
        match denied {
            PollOutcome::Error(message) => assert!(message.contains("the operator said no")),
            other => panic!("expected an error, got {other:?}"),
        }
        let approved = poll_device(&http, &urls, "approved-code").await.unwrap();
        match approved {
            PollOutcome::Approved(tokens) => {
                assert_eq!(
                    tokens.access_token,
                    "approved-access-token-0123456789abcdef"
                );
                assert_eq!(
                    tokens.refresh_token,
                    "approved-refresh-token-0123456789abcdef"
                );
            }
            other => panic!("expected approval, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_stale_session_is_refreshed_before_the_pool_spends_it() {
        let stand = stand_in().await;
        let path = temp_db_path();
        let db = Db::open(path.to_str().unwrap()).unwrap();
        db.save_upstream(&grok_upstream()).unwrap();
        db.save_upstream(&openai_upstream()).unwrap();
        db.save_model(&model("grok-4.7", "my-grok")).unwrap();
        db.save_model(&model("gpt-4o", "openai-main")).unwrap();
        db.add_key("openai-main", "sk-openai-plain", 1).unwrap();

        let now = crate::registry::now();
        let refresh = "old-refresh-token-that-must-stay";
        db.insert_grok_session(
            "my-grok",
            "still-good-access-token-zzzzzzzzzzzz",
            refresh,
            now + 7200,
            now,
        )
        .unwrap();

        let cfg = GatewayConfig::starter();
        let mut state = AppState::new(cfg, db).unwrap();
        state.grok_auth = auth(&stand.base);

        // Far from expiry: the stand-in must not be asked, or the access
        // token below would be replaced.
        state.prepare_grok().await.unwrap();
        assert_eq!(stand.refreshes.load(Ordering::SeqCst), 0);
        let kept = state.db.grok_sessions().unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].access_token, "still-good-access-token-zzzzzzzzzzzz");
        assert_eq!(kept[0].refresh_token, refresh);

        // Missing access token, even with a distant expiry.
        state
            .db
            .update_grok_session(kept[0].id, "", refresh, now + 7200)
            .unwrap();
        state.prepare_grok().await.unwrap();
        assert_eq!(stand.refreshes.load(Ordering::SeqCst), 1);
        let missing = state.db.grok_sessions().unwrap();
        assert_eq!(
            missing[0].access_token,
            "fresh-access-token-0001-0123456789abcdef"
        );
        assert_eq!(missing[0].refresh_token, refresh, "omitted refresh is kept");

        // Within five minutes of expiry.
        state
            .db
            .update_grok_session(
                missing[0].id,
                "stale-access-token-aaaaaaaaaaaaaa",
                refresh,
                crate::registry::now() + 30,
            )
            .unwrap();
        state.prepare_grok().await.unwrap();
        assert_eq!(stand.refreshes.load(Ordering::SeqCst), 2);
        let fresh = state.db.grok_sessions().unwrap();
        let access = "fresh-access-token-0002-0123456789abcdef";
        assert_eq!(fresh[0].access_token, access);
        assert_eq!(fresh[0].refresh_token, refresh);

        // The new access token is not close to expiry, so a second pass
        // leaves it alone.
        state.prepare_grok().await.unwrap();
        assert_eq!(stand.refreshes.load(Ordering::SeqCst), 2);

        let sent = state
            .registry
            .read()
            .pool("my-grok")
            .unwrap()
            .acquire()
            .unwrap()
            .key;
        assert_eq!(sent, access);
        let plain = state
            .registry
            .read()
            .pool("openai-main")
            .unwrap()
            .acquire()
            .unwrap()
            .key;
        assert_eq!(plain, "sk-openai-plain");

        {
            // The read guard must end before the next `.await`: clippy treats
            // a guard kept in this async test as held across that point.
            let registry = state.registry.read();
            let (grok_up, grok_model) = registry.find_model("grok-4.7").unwrap();
            assert_eq!(grok_up.id, "my-grok");
            assert!(grok_up.grok);
            assert_eq!(grok_up.base_url, SUBSCRIPTION_BASE);
            assert_eq!(
                grok_up.extra_headers,
                vec![("X-Custom".into(), "keep".into())]
            );
            let grok_provider = provider_for(grok_up, grok_model);
            assert!(vd_llm::grok::is_subscription(&grok_provider));
            assert_eq!(grok_provider.base_url, SUBSCRIPTION_BASE);
            assert!(grok_provider
                .extra_headers
                .iter()
                .all(|(name, _)| !vd_llm::grok::is_managed_header(name)));

            let (openai_up, openai_model) = registry.find_model("gpt-4o").unwrap();
            let openai_provider = provider_for(openai_up, openai_model);
            assert!(!vd_llm::grok::is_subscription(&openai_provider));
            assert_eq!(openai_provider.base_url, "https://api.openai.com/v1");
        }

        // A hand-edited row still goes to the subscription proxy.
        let mut edited = grok_upstream();
        edited.base_url = "https://console.x.ai/v1".into();
        let forced = provider_for(&edited, &model("grok-4.7", "my-grok"));
        assert!(vd_llm::grok::is_subscription(&forced));
        assert_eq!(forced.base_url, SUBSCRIPTION_BASE);

        let status = public_status(&state.db, Some("my-grok")).unwrap();
        let text = status.to_string();
        assert!(status["signed_in"].as_bool().unwrap());
        assert_eq!(status["status"], "signed in");
        assert!(status["sessions"][0].get("access_token").is_none());
        assert!(status["sessions"][0].get("refresh_token").is_none());
        assert!(status["sessions"][0].get("api_key").is_none());
        assert!(!text.contains(access));
        assert!(!text.contains(refresh));
        let listed = serde_json::to_string(&state.db.list_keys("my-grok").unwrap()).unwrap();
        assert!(!listed.contains(access));
        assert!(!listed.contains(refresh));
        assert!(!listed.contains("approved-refresh-token"));

        let urls = state.grok_auth.clone();
        let http = state.llm.http.clone();
        drop(state);
        let reloaded = Db::open(path.to_str().unwrap()).unwrap();
        let survived = reloaded.grok_sessions().unwrap();
        assert_eq!(
            survived.len(),
            1,
            "a reloaded database still has the session"
        );
        assert_eq!(survived[0].access_token, access);
        assert_eq!(survived[0].refresh_token, refresh);
        let registry = crate::registry::Registry::load(&reloaded, None).unwrap();
        assert_eq!(
            registry.pool("my-grok").unwrap().acquire().unwrap().key,
            access
        );
        drop(registry);

        let mut state = AppState::new(GatewayConfig::starter(), reloaded).unwrap();
        state.grok_auth = urls;
        // A second approval is another credential in the same pool.
        let approved = poll_device(&http, &state.grok_auth, "approved-code")
            .await
            .unwrap();
        let PollOutcome::Approved(tokens) = approved else {
            panic!("expected a second approval");
        };
        state
            .db
            .insert_grok_session(
                "my-grok",
                &tokens.access_token,
                &tokens.refresh_token,
                tokens.expires_at,
                crate::registry::now(),
            )
            .unwrap();
        state.reload().unwrap();
        assert_eq!(state.db.grok_sessions().unwrap().len(), 2);
        let pool = state.registry.read().pool("my-grok").unwrap();
        assert_eq!(pool.len(), 2);
        let mut sent_keys = [pool.acquire().unwrap().key, pool.acquire().unwrap().key];
        sent_keys.sort();
        assert!(sent_keys.contains(&access.to_string()));
        assert!(sent_keys.contains(&tokens.access_token));
        assert!(sent_keys.iter().all(|key| !key.contains("refresh")));

        state.db.delete_grok_sessions("my-grok").unwrap();
        state.reload().unwrap();
        let after = public_status(&state.db, None).unwrap();
        assert_eq!(after["status"], "not signed in");
        assert!(!after["signed_in"].as_bool().unwrap());
        assert!(state.registry.read().pool("my-grok").unwrap().is_empty());
        drop(state);

        let signed_out = Db::open(path.to_str().unwrap()).unwrap();
        assert!(signed_out.grok_sessions().unwrap().is_empty());
        assert_eq!(
            signed_out.upstream_keys("openai-main").unwrap(),
            vec!["sk-openai-plain".to_string()]
        );
        assert!(signed_out
            .list_upstreams()
            .unwrap()
            .iter()
            .any(|up| up.id == "my-grok" && up.grok));
        drop(signed_out);
        remove_db(&path);
    }

    #[test]
    fn status_before_any_login_is_not_signed_in() {
        let db = Db::memory().unwrap();
        let status = public_status(&db, None).unwrap();
        assert_eq!(status["status"], "not signed in");
        assert_eq!(status["signed_in"], false);
        assert!(status["sessions"].as_array().unwrap().is_empty());
    }

    /// A database written before session columns existed still opens, and
    /// what it already held is still there.
    #[test]
    fn an_existing_database_keeps_its_upstreams_keys_models_and_licences() {
        let path = temp_db_path();
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE upstream (
                     id TEXT PRIMARY KEY,
                     label TEXT NOT NULL DEFAULT '',
                     kind TEXT NOT NULL,
                     base_url TEXT NOT NULL,
                     api_version TEXT NOT NULL DEFAULT 'v1beta',
                     reasoning_dialect TEXT NOT NULL DEFAULT 'auto',
                     extra_headers TEXT NOT NULL DEFAULT '[]',
                     enabled INTEGER NOT NULL DEFAULT 1,
                     position INTEGER NOT NULL DEFAULT 0
                 );
                 CREATE TABLE upstream_key (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     upstream_id TEXT NOT NULL,
                     api_key TEXT NOT NULL,
                     added_at INTEGER NOT NULL,
                     UNIQUE(upstream_id, api_key)
                 );
                 CREATE TABLE model (
                     name TEXT PRIMARY KEY,
                     upstream_id TEXT NOT NULL,
                     upstream_name TEXT NOT NULL DEFAULT '',
                     price_in REAL NOT NULL DEFAULT 0,
                     price_cached REAL,
                     price_out REAL NOT NULL DEFAULT 0,
                     context_tokens INTEGER,
                     enabled INTEGER NOT NULL DEFAULT 1,
                     position INTEGER NOT NULL DEFAULT 0,
                     voice INTEGER NOT NULL DEFAULT 0,
                     price_request REAL NOT NULL DEFAULT 0
                 );
                 CREATE TABLE tier (
                     name TEXT PRIMARY KEY,
                     credits_5h REAL NOT NULL,
                     credits_week REAL NOT NULL,
                     max_peers INTEGER NOT NULL DEFAULT 2
                 );
                 CREATE TABLE license (
                     license_id TEXT PRIMARY KEY,
                     tier TEXT NOT NULL,
                     expires_at INTEGER NOT NULL DEFAULT 0,
                     max_peers INTEGER NOT NULL DEFAULT 2,
                     issued_at INTEGER NOT NULL,
                     note TEXT NOT NULL DEFAULT ''
                 );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO upstream (id, label, kind, base_url, api_version, reasoning_dialect, extra_headers, enabled, position)
                 VALUES ('openrouter', 'OpenRouter', 'openai_compatible', 'https://openrouter.ai/api/v1', 'v1', 'auto', '[]', 1, 0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO upstream_key (upstream_id, api_key, added_at) VALUES ('openrouter', 'sk-or-existing-key', 10)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO model (name, upstream_id, upstream_name, price_in, price_out, enabled, position)
                 VALUES ('deepseek-chat', 'openrouter', '', 0.2, 0.8, 1, 0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tier (name, credits_5h, credits_week, max_peers) VALUES ('pro', 400, 1500, 2)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO license (license_id, tier, expires_at, max_peers, issued_at, note)
                 VALUES ('VD-PRO-0001', 'pro', 0, 2, 10, 'kept')",
                [],
            )
            .unwrap();
        }

        let db = Db::open(path.to_str().unwrap()).unwrap();
        let upstreams = db.list_upstreams().unwrap();
        assert_eq!(upstreams.len(), 1);
        assert_eq!(upstreams[0].id, "openrouter");
        assert!(!upstreams[0].grok);
        assert_eq!(
            db.upstream_keys("openrouter").unwrap(),
            vec!["sk-or-existing-key".to_string()]
        );
        let keys = db.list_keys("openrouter").unwrap();
        assert_eq!(keys.len(), 1);
        assert!(!keys[0].session);
        let listed = serde_json::to_string(&keys).unwrap();
        assert!(!listed.contains("sk-or-existing-key"));
        let models = db.list_models().unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "deepseek-chat");
        assert!(db.list_tiers().unwrap().contains_key("pro"));
        let licenses = db.list_licenses().unwrap();
        assert_eq!(licenses.len(), 1);
        assert_eq!(licenses[0].license_id, "VD-PRO-0001");
        assert!(licenses[0].note == "kept");
        assert!(db.grok_sessions().unwrap().is_empty());
        let status = public_status(&db, None).unwrap();
        assert_eq!(status["status"], "not signed in");

        let registry = crate::registry::Registry::load(&db, None).unwrap();
        let (up, model) = registry.find_model("deepseek-chat").unwrap();
        assert_eq!(up.id, "openrouter");
        assert!(!vd_llm::grok::is_subscription(&provider_for(up, model)));
        assert_eq!(
            registry.pool("openrouter").unwrap().acquire().unwrap().key,
            "sk-or-existing-key"
        );
        drop(registry);
        drop(db);
        remove_db(&path);
    }
}
