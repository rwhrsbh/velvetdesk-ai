//! What the gateway is told before it starts: where to listen, where the
//! database is, and the public key licences are checked against.
//!
//! The upstream and model sections of this file seed an empty database on the
//! first run and are never read again — after that the admin page edits the
//! database, because a key that stopped working at two in the morning should
//! not need a deploy. Keys may sit in the file (the operator's own, `chmod
//! 600`) or be named as environment variables.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use vd_llm::provider::{default_api_version, default_dialect, ProviderKind};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GatewayConfig {
    /// Address to listen on. TLS is terminated in front of this by nginx or
    /// caddy — the gateway speaks plain HTTP to the reverse proxy.
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_db")]
    pub db_path: String,
    /// Ed25519 public key, base64, that licences are checked against.
    #[serde(default)]
    pub license_public_key: String,
    /// What one credit costs in dollars. Prices below are per million tokens,
    /// so a credit is a unit of money, and the tier limits are budgets.
    #[serde(default = "default_credit_usd")]
    pub credit_usd: f64,
    /// Upstreams in the order they are tried when the request names no model.
    pub upstreams: Vec<Upstream>,
    /// Budgets per tier, by the `tier` in the licence.
    #[serde(default)]
    pub tiers: HashMap<String, Tier>,
}

/// One provider the gateway may spend on: where it is, which keys open it,
/// and what each of its models costs.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Upstream {
    pub id: String,
    #[serde(default)]
    pub label: String,
    pub kind: ProviderKind,
    pub base_url: String,
    #[serde(default = "default_api_version")]
    pub api_version: String,
    /// Keys written into the config file.
    #[serde(default)]
    pub keys: Vec<String>,
    /// Names of environment variables holding keys, for deploys that keep
    /// secrets out of files.
    #[serde(default)]
    pub keys_env: Vec<String>,
    #[serde(default)]
    pub extra_headers: Vec<(String, String)>,
    #[serde(default = "default_dialect")]
    pub reasoning_dialect: String,
    /// Models this upstream serves, in fallback order.
    pub models: Vec<Model>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Model {
    /// Name the client asks for, which is also the name sent upstream unless
    /// `upstream_name` says otherwise.
    pub name: String,
    #[serde(default)]
    pub upstream_name: Option<String>,
    /// Dollars per million tokens.
    pub price_in: f64,
    /// Dollars per million prompt tokens served from the provider's cache.
    /// Equal to `price_in` when the provider has no cache — which is the
    /// honest default, not a discount nobody gave us.
    #[serde(default)]
    pub price_cached: Option<f64>,
    pub price_out: f64,
    #[serde(default)]
    pub context_tokens: Option<u32>,
    /// Takes dictation rather than chat.
    #[serde(default)]
    pub voice: bool,
    /// Dollars per clip, for voice models.
    #[serde(default)]
    pub price_request: f64,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct Tier {
    /// Credits allowed in any five-hour stretch.
    pub credits_5h: f64,
    /// Credits allowed in any seven-day stretch.
    pub credits_week: f64,
    #[serde(default = "default_peers")]
    pub max_peers: u32,
}

impl Default for Tier {
    fn default() -> Self {
        Tier {
            credits_5h: 1_000.0,
            credits_week: 20_000.0,
            max_peers: 2,
        }
    }
}

fn default_bind() -> String {
    "127.0.0.1:8787".into()
}

fn default_db() -> String {
    "gateway.db".into()
}

fn default_credit_usd() -> f64 {
    // A tenth of a cent a credit: a thousand credits is a dollar of cost, so
    // a tier's budget reads as money without a calculator.
    0.001
}

fn default_peers() -> u32 {
    2
}

impl Upstream {
    /// Keys from the file and from the environment, in that order, with the
    /// empty ones dropped.
    pub fn resolved_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.keys.iter().map(|k| k.trim().to_string()).collect();
        for name in &self.keys_env {
            if let Ok(value) = std::env::var(name) {
                // One variable may hold several keys, comma-separated: a host
                // that gives you one secret slot should not cost you a pool.
                keys.extend(value.split(',').map(|k| k.trim().to_string()));
            }
        }
        keys.retain(|k| !k.is_empty());
        keys
    }
}

impl GatewayConfig {
    pub fn load(path: &Path) -> std::io::Result<GatewayConfig> {
        let text = std::fs::read_to_string(path)?;
        serde_json::from_str(&text).map_err(std::io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One variable may hold a whole pool, because a host that gives you a
    /// single secret slot should not cost you one.
    #[test]
    fn keys_come_from_the_file_and_the_environment() {
        std::env::set_var("VD_TEST_KEYS", "a, b ,,c");
        let upstream: Upstream = serde_json::from_value(serde_json::json!({
            "id": "x",
            "kind": "gemini",
            "base_url": "https://example.invalid",
            "keys": ["file-key"],
            "keys_env": ["VD_TEST_KEYS"],
            "models": []
        }))
        .unwrap();
        assert_eq!(upstream.resolved_keys(), ["file-key", "a", "b", "c"]);
    }

    /// A seed file with no tiers is not a broken one: the defaults are
    /// sensible and the admin page edits them afterwards.
    #[test]
    fn a_missing_tier_falls_back_to_the_default() {
        let tier = Tier::default();
        assert!(tier.credits_5h > 0.0);
        assert!(tier.credits_week > tier.credits_5h);
        assert_eq!(tier.max_peers, 2);
    }
}
