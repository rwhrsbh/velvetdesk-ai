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
    /// What a credit is sold for, in dollars.
    ///
    /// `credit_usd` above is what one costs us; this is what one goes for.
    /// Kept apart on purpose: selling at cost is a way to lose money on
    /// every top-up while believing the plans are profitable. Four times
    /// cost is the default — in the same range as the subscriptions, which
    /// work out at two and a half to three times cost when spent in full,
    /// with a little more margin because a top-up is bought by somebody who
    /// has already run out and is not shopping around.
    #[serde(default = "default_credit_price")]
    pub credit_price_usd: f64,
    /// Where the app sends an operator who wants more credits: a payment
    /// page, a Telegram account, an email link. Empty hides the button
    /// rather than showing one that goes nowhere.
    #[serde(default)]
    pub topup_url: String,
    /// Upstreams in the order they are tried when the request names no model.
    pub upstreams: Vec<Upstream>,
    /// Budgets per tier, by the `tier` in the licence.
    #[serde(default)]
    pub tiers: HashMap<String, Tier>,
    /// How many upstream calls may run at once. The rest queue.
    #[serde(default = "default_inflight")]
    pub max_inflight: usize,
    /// How many of those one licence may hold, so a batch from one operator
    /// cannot fill the gateway.
    #[serde(default = "default_per_license")]
    pub max_per_license: usize,
    /// How many callers may wait. Past this the answer is an immediate
    /// "come back in a moment" rather than a held connection.
    #[serde(default = "default_queued")]
    pub max_queued: usize,
    /// Seconds a request may wait for a slot before it is turned away.
    #[serde(default = "default_queue_wait")]
    pub queue_wait_seconds: u64,
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
    // Loopback by default: a gateway on a VPS sits behind nginx or caddy, and
    // a fresh install should not be on the public internet by accident. The
    // container image sets VD_BIND to 0.0.0.0, where the isolation is the
    // network namespace rather than the interface.
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

fn default_credit_price() -> f64 {
    // Four tenths of a cent a credit: a thousand credits is $4 of sales
    // against $1 of cost.
    0.004
}

fn default_inflight() -> usize {
    crate::queue::DEFAULT_INFLIGHT
}

fn default_per_license() -> usize {
    crate::queue::DEFAULT_PER_LICENSE
}

fn default_queued() -> usize {
    crate::queue::DEFAULT_QUEUED
}

fn default_queue_wait() -> u64 {
    crate::queue::DEFAULT_WAIT_SECONDS
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
        let mut cfg: GatewayConfig = serde_json::from_str(&text).map_err(std::io::Error::other)?;
        cfg.apply_environment();
        Ok(cfg)
    }

    /// The config a container starts with when the volume is empty.
    ///
    /// A first run should not need a file to be written by hand on the host:
    /// the gateway comes up, the admin page is reachable, and providers,
    /// keys, models and tiers are added there. Everything it decides is
    /// written to the database in the same directory, so the whole service
    /// moves by copying one folder.
    pub fn starter() -> GatewayConfig {
        GatewayConfig {
            bind: default_bind(),
            db_path: default_db(),
            license_public_key: String::new(),
            credit_usd: default_credit_usd(),
            credit_price_usd: default_credit_price(),
            topup_url: String::new(),
            upstreams: vec![],
            // What is sold, as of now: pro is $10 a month for two people on
            // two machines, business is $150 a month or $1000 a year for a
            // team of ten.
            //
            // A credit is a tenth of a cent of upstream cost, so a weekly
            // budget is a monthly bill divided by 4.33 and multiplied by a
            // thousand. These numbers are chosen so that a subscription is
            // still profitable when it is used to the limit, which is the
            // only case worth planning for:
            //
            //   pro       1 500/week = $1.50 = $6.50 a month against $10
            //   business 20 000/week = $20   = $87   a month against $150
            //
            // Business is the better deal per seat — 2 000 credits a week
            // each against pro's 750 — which is the point: a pair who work
            // properly outgrow pro and move up rather than quietly costing
            // more than they pay.
            //
            // The five-hour window is deliberately loose compared with the
            // week. It exists to stop one runaway loop emptying a month in
            // an afternoon, not to pace anybody's shift; the week is the
            // real ceiling.
            tiers: HashMap::from([
                (
                    "pro".to_string(),
                    Tier {
                        credits_5h: 400.0,
                        credits_week: 1_500.0,
                        max_peers: 2,
                    },
                ),
                (
                    "business".to_string(),
                    Tier {
                        credits_5h: 5_000.0,
                        credits_week: 20_000.0,
                        max_peers: 10,
                    },
                ),
            ]),
            max_inflight: default_inflight(),
            max_per_license: default_per_license(),
            max_queued: default_queued(),
            queue_wait_seconds: default_queue_wait(),
        }
    }

    /// Let the environment win over the file, for the things a deployment
    /// decides rather than the operator: the address to listen on, where the
    /// database goes, and the licence key to check against.
    pub fn apply_environment(&mut self) {
        if let Ok(bind) = std::env::var("VD_BIND") {
            if !bind.trim().is_empty() {
                self.bind = bind;
            }
        }
        if let Ok(db) = std::env::var("VD_DB_PATH") {
            if !db.trim().is_empty() {
                self.db_path = db;
            }
        }
        if let Ok(key) = std::env::var("VD_LICENSE_PUBLIC_KEY") {
            if !key.trim().is_empty() {
                self.license_public_key = key.trim().to_string();
            }
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text)
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
