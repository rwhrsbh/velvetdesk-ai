//! What the gateway is told before it starts: where to listen, which
//! upstreams it may spend money on, what a model costs and what a tier is
//! allowed to spend.
//!
//! One JSON file, read at boot. Keys may sit in it (the file is the
//! operator's, `chmod 600`) or be named as environment variables, which is
//! what a deploy with secrets outside the repository wants.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use vd_llm::provider::{default_api_version, default_dialect, ProviderConfig, ProviderKind};

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

impl Model {
    pub fn cached_price(&self) -> f64 {
        self.price_cached.unwrap_or(self.price_in)
    }

    pub fn upstream_name(&self) -> &str {
        self.upstream_name.as_deref().unwrap_or(&self.name)
    }
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

    /// The provider shape `vd-llm` calls with, for one model of this upstream.
    pub fn provider(&self, model: &Model) -> ProviderConfig {
        ProviderConfig {
            id: self.id.clone(),
            label: if self.label.is_empty() {
                self.id.clone()
            } else {
                self.label.clone()
            },
            kind: self.kind,
            base_url: self.base_url.clone(),
            api_version: self.api_version.clone(),
            model: model.upstream_name().to_string(),
            extra_headers: self.extra_headers.clone(),
            temperature: 0.85,
            max_output_tokens: None,
            transcribe_model: String::new(),
            thinking_effort: String::new(),
            thinking_budget: None,
            reasoning_dialect: self.reasoning_dialect.clone(),
            // Fallback is chosen by the gateway across upstreams, not inside
            // one: a key that ran out here may still have a model there.
            model_chain: vec![],
            context_tokens: model.context_tokens,
            key_count: self.resolved_keys().len(),
        }
    }
}

impl GatewayConfig {
    pub fn load(path: &Path) -> std::io::Result<GatewayConfig> {
        let text = std::fs::read_to_string(path)?;
        serde_json::from_str(&text).map_err(std::io::Error::other)
    }

    /// Which upstream serves this model name, and the model's entry.
    pub fn find_model(&self, name: &str) -> Option<(&Upstream, &Model)> {
        for upstream in &self.upstreams {
            if let Some(model) = upstream.models.iter().find(|m| m.name == name) {
                return Some((upstream, model));
            }
        }
        None
    }

    /// The chain a request walks when the model it named is unavailable — or
    /// when it named nothing at all: every model of every upstream, in
    /// configured order, starting at the one asked for.
    pub fn chain_from(&self, name: &str) -> Vec<(&Upstream, &Model)> {
        let mut all: Vec<(&Upstream, &Model)> = vec![];
        for upstream in &self.upstreams {
            for model in &upstream.models {
                all.push((upstream, model));
            }
        }
        let Some(start) = all.iter().position(|(_, m)| m.name == name) else {
            return all;
        };
        all.rotate_left(start);
        all
    }

    pub fn tier(&self, name: &str) -> Tier {
        self.tiers.get(name).copied().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> GatewayConfig {
        serde_json::from_value(serde_json::json!({
            "upstreams": [
                {
                    "id": "openrouter",
                    "kind": "openai_compatible",
                    "base_url": "https://openrouter.ai/api/v1",
                    "keys": ["k1"],
                    "models": [
                        { "name": "deepseek-chat", "price_in": 0.28, "price_cached": 0.028, "price_out": 0.42 },
                        { "name": "qwen-max", "price_in": 1.6, "price_out": 6.4 }
                    ]
                },
                {
                    "id": "gemini",
                    "kind": "gemini",
                    "base_url": "https://generativelanguage.googleapis.com",
                    "keys": ["g1", "g2"],
                    "models": [{ "name": "gemini-2.5-flash", "price_in": 0.3, "price_out": 2.5 }]
                }
            ]
        }))
        .unwrap()
    }

    #[test]
    fn a_model_is_found_with_its_upstream() {
        let cfg = cfg();
        let (upstream, model) = cfg.find_model("gemini-2.5-flash").unwrap();
        assert_eq!(upstream.id, "gemini");
        assert_eq!(model.price_out, 2.5);
    }

    /// A model with no cache price is not secretly cheap: the cached rate
    /// falls back to the full one.
    #[test]
    fn an_uncached_model_prices_cache_hits_at_full_rate() {
        let cfg = cfg();
        let (_, model) = cfg.find_model("qwen-max").unwrap();
        assert_eq!(model.cached_price(), 1.6);
        let (_, cached) = cfg.find_model("deepseek-chat").unwrap();
        assert_eq!(cached.cached_price(), 0.028);
    }

    /// The chain starts at what was asked for and wraps round, so a request
    /// that names the last model still has somewhere to fall.
    #[test]
    fn the_chain_starts_where_it_was_asked_to() {
        let cfg = cfg();
        let chain: Vec<String> = cfg
            .chain_from("gemini-2.5-flash")
            .into_iter()
            .map(|(_, m)| m.name.clone())
            .collect();
        assert_eq!(chain, ["gemini-2.5-flash", "deepseek-chat", "qwen-max"]);
    }

    /// An unknown name is not an error: it falls to the configured order, the
    /// same as a request that named nothing.
    #[test]
    fn an_unknown_model_falls_back_to_the_whole_list() {
        let cfg = cfg();
        assert_eq!(cfg.chain_from("gpt-9").len(), 3);
    }

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
}
