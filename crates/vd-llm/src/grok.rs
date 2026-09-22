//! Subscription proxy at cli-chat-proxy.grok.com.
//!
//! The proxy refuses a call whose `x-grok-client-version` is missing or older
//! than the build it currently accepts. The version is whatever the stable
//! channel pointer says right now, cached for half an hour, not a number
//! compiled into this crate.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::RequestBuilder;

use crate::provider::ProviderConfig;

const VERSION_TTL: Duration = Duration::from_secs(30 * 60);
const POINTERS: &[&str] = &[
    "https://x.ai/cli/stable",
    "https://storage.googleapis.com/grok-build-public-artifacts/cli/stable",
];

struct CachedVersion {
    value: String,
    at: Instant,
}

static VERSION: Mutex<Option<CachedVersion>> = Mutex::new(None);

pub fn is_subscription(provider: &ProviderConfig) -> bool {
    provider.id == "grok" || provider.base_url.contains("cli-chat-proxy.grok.com")
}

/// Headers this crate owns for the subscription proxy. A copy saved in
/// settings would go stale the next time the CLI ships.
pub fn is_managed_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("x-xai-token-auth")
        || name.eq_ignore_ascii_case("x-grok-client-version")
}

pub async fn apply(
    http: &reqwest::Client,
    provider: &ProviderConfig,
    req: RequestBuilder,
) -> RequestBuilder {
    if !is_subscription(provider) {
        return req;
    }
    let version = client_version(http).await;
    req.header("X-XAI-Token-Auth", "xai-grok-cli")
        .header("x-grok-client-version", version)
}

/// Stable-channel version the CLI installer itself publishes.
///
/// A failed lookup keeps the previous answer. With nothing cached yet, the
/// local `grok --version` is the last resort.
pub async fn client_version(http: &reqwest::Client) -> String {
    if let Some(hit) = fresh() {
        return hit;
    }
    for url in POINTERS {
        if let Some(version) = fetch_pointer(http, url).await {
            store(version.clone());
            return version;
        }
    }
    if let Some(version) = local_grok_version() {
        store(version.clone());
        return version;
    }
    fresh().unwrap_or_else(|| "0.1.202".to_string())
}

fn fresh() -> Option<String> {
    let guard = VERSION.lock().ok()?;
    let hit = guard.as_ref()?;
    (hit.at.elapsed() < VERSION_TTL).then(|| hit.value.clone())
}

fn store(value: String) {
    if let Ok(mut guard) = VERSION.lock() {
        *guard = Some(CachedVersion {
            value,
            at: Instant::now(),
        });
    }
}

async fn fetch_pointer(http: &reqwest::Client, url: &str) -> Option<String> {
    let response = http
        .get(url)
        .timeout(Duration::from_secs(8))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body = response.text().await.ok()?;
    parse_version(body.trim())
}

fn parse_version(raw: &str) -> Option<String> {
    let version = raw.split_whitespace().next()?.trim();
    let version = version.strip_prefix('v').unwrap_or(version);
    let ok = !version.is_empty()
        && version.len() < 32
        && version
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == '-');
    ok.then(|| version.to_string())
}

fn local_grok_version() -> Option<String> {
    let output = std::process::Command::new("grok")
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // `grok 1.0.40 (hash) [stable]`
    parse_version(text.split_whitespace().nth(1).unwrap_or(""))
}

#[cfg(test)]
mod tests {
    use super::parse_version;

    #[test]
    fn pointer_is_a_bare_version() {
        assert_eq!(parse_version("1.0.40"), Some("1.0.40".into()));
        assert_eq!(parse_version("v1.0.40\n"), Some("1.0.40".into()));
        assert_eq!(parse_version("not a version"), None);
    }
}
