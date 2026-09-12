//! What the gateway may spend money on, as it stands right now.
//!
//! The config file seeds this once; after that the database is the truth and
//! the admin page edits it. A key that stopped working at two in the morning
//! is replaced from a browser, not from a deploy, and the pool it belongs to
//! is rebuilt in place — the other upstreams do not even notice.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use vd_llm::keypool::KeyPool;
use vd_llm::provider::{ProviderConfig, ProviderKind};

use crate::config::{GatewayConfig, Tier};
use crate::db::Db;

/// One place the gateway can send a request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamRow {
    pub id: String,
    #[serde(default)]
    pub label: String,
    pub kind: ProviderKind,
    pub base_url: String,
    #[serde(default = "default_api_version")]
    pub api_version: String,
    #[serde(default = "default_dialect")]
    pub reasoning_dialect: String,
    #[serde(default)]
    pub extra_headers: Vec<(String, String)>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub position: i64,
    /// How many keys it has. Read-only: keys are added and removed one at a
    /// time, and never travel back out of the gateway.
    #[serde(default)]
    pub key_count: usize,
}

/// One model, with what it costs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRow {
    pub name: String,
    pub upstream_id: String,
    /// What the upstream calls it, when that differs from what clients ask
    /// for — `deepseek/deepseek-chat` on OpenRouter, `deepseek-chat` here.
    #[serde(default)]
    pub upstream_name: String,
    #[serde(default)]
    pub price_in: f64,
    #[serde(default)]
    pub price_cached: Option<f64>,
    #[serde(default)]
    pub price_out: f64,
    #[serde(default)]
    pub context_tokens: Option<u32>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub position: i64,
    /// Serves dictation instead of chat: it is tried for transcription and
    /// never offered as a chat model.
    #[serde(default)]
    pub voice: bool,
    /// Dollars per clip, for voice models. A transcription answers with text
    /// and no token count, so there is nothing else to bill it by.
    #[serde(default)]
    pub price_request: f64,
}

/// One key, as the admin page is allowed to see it.
#[derive(Debug, Clone, Serialize)]
pub struct KeyRow {
    pub id: i64,
    /// `AIzaS...9fA` — enough to tell two keys apart, not enough to use one.
    pub masked: String,
    pub added_at: i64,
}

/// A licence the gateway has issued.
#[derive(Debug, Clone, Serialize)]
pub struct LicenseRow {
    pub license_id: String,
    pub tier: String,
    pub expires_at: i64,
    pub max_peers: u32,
    pub issued_at: i64,
    pub note: String,
    pub revoked: bool,
}

fn default_api_version() -> String {
    "v1beta".into()
}

fn default_dialect() -> String {
    "auto".into()
}

fn yes() -> bool {
    true
}

impl ModelRow {
    pub fn cached_price(&self) -> f64 {
        self.price_cached.unwrap_or(self.price_in)
    }

    pub fn upstream_name(&self) -> &str {
        if self.upstream_name.trim().is_empty() {
            &self.name
        } else {
            &self.upstream_name
        }
    }
}

/// Everything above, plus the live key pools.
pub struct Registry {
    pub upstreams: Vec<UpstreamRow>,
    pub models: Vec<ModelRow>,
    pub tiers: HashMap<String, Tier>,
    pools: HashMap<String, Arc<KeyPool>>,
}

impl Registry {
    /// Read the whole picture from the database.
    ///
    /// `previous` carries the pools already in use: a pool whose keys have not
    /// changed is kept, cooldowns and all, so editing one upstream does not
    /// hand every other one a fresh set of keys that have "never failed".
    pub fn load(db: &Db, previous: Option<&Registry>) -> rusqlite::Result<Registry> {
        let mut upstreams = db.list_upstreams()?;
        let models = db.list_models()?;
        let tiers = db.list_tiers()?;

        let mut pools = HashMap::new();
        for upstream in &mut upstreams {
            let keys = db.upstream_keys(&upstream.id)?;
            upstream.key_count = keys.len();
            let kept = previous
                .and_then(|old| old.pools.get(&upstream.id))
                .filter(|pool| pool.keys_are(&keys))
                .cloned();
            pools.insert(
                upstream.id.clone(),
                kept.unwrap_or_else(|| Arc::new(KeyPool::new(keys))),
            );
        }

        Ok(Registry {
            upstreams,
            models,
            tiers,
            pools,
        })
    }

    /// Seed an empty database from the config file, so a first run with a
    /// hand-written `gateway.json` works exactly as it did before the admin
    /// page existed.
    pub fn seed_if_empty(db: &Db, cfg: &GatewayConfig) -> rusqlite::Result<()> {
        if !db.list_upstreams()?.is_empty() {
            return Ok(());
        }
        for (index, upstream) in cfg.upstreams.iter().enumerate() {
            db.save_upstream(&UpstreamRow {
                id: upstream.id.clone(),
                label: upstream.label.clone(),
                kind: upstream.kind,
                base_url: upstream.base_url.clone(),
                api_version: upstream.api_version.clone(),
                reasoning_dialect: upstream.reasoning_dialect.clone(),
                extra_headers: upstream.extra_headers.clone(),
                enabled: true,
                position: index as i64,
                key_count: 0,
            })?;
            for key in upstream.resolved_keys() {
                db.add_key(&upstream.id, &key, now())?;
            }
            for (place, model) in upstream.models.iter().enumerate() {
                db.save_model(&ModelRow {
                    name: model.name.clone(),
                    upstream_id: upstream.id.clone(),
                    upstream_name: model.upstream_name.clone().unwrap_or_default(),
                    price_in: model.price_in,
                    price_cached: model.price_cached,
                    price_out: model.price_out,
                    context_tokens: model.context_tokens,
                    enabled: true,
                    position: place as i64,
                    voice: model.voice,
                    price_request: model.price_request,
                })?;
            }
        }
        for (name, tier) in &cfg.tiers {
            db.save_tier(name, tier)?;
        }
        Ok(())
    }

    pub fn tier(&self, name: &str) -> Tier {
        self.tiers.get(name).copied().unwrap_or_default()
    }

    pub fn pool(&self, upstream_id: &str) -> Option<Arc<KeyPool>> {
        self.pools.get(upstream_id).cloned()
    }

    /// Which upstream serves this model, if it is switched on and its upstream
    /// is too.
    pub fn find_model(&self, name: &str) -> Option<(&UpstreamRow, &ModelRow)> {
        let model = self
            .models
            .iter()
            .find(|model| model.name == name && model.enabled)?;
        let upstream = self
            .upstreams
            .iter()
            .find(|up| up.id == model.upstream_id && up.enabled)?;
        Some((upstream, model))
    }

    /// The models to try, starting at the one asked for and wrapping round.
    ///
    /// Anything switched off is not in the list at all, which is what the
    /// switch is for: an upstream out of credit is turned off and stops being
    /// tried, without deleting what is known about it.
    pub fn chain_from(&self, name: &str) -> Vec<(&UpstreamRow, &ModelRow)> {
        let mut all: Vec<(&UpstreamRow, &ModelRow)> = vec![];
        for model in &self.models {
            if !model.enabled || model.voice {
                continue;
            }
            if let Some(upstream) = self
                .upstreams
                .iter()
                .find(|up| up.id == model.upstream_id && up.enabled)
            {
                all.push((upstream, model));
            }
        }
        if let Some(start) = all.iter().position(|(_, model)| model.name == name) {
            all.rotate_left(start);
        }
        all
    }

    /// The dictation models, in the order they should be tried.
    ///
    /// Separate from the chat chain because the two fail for different
    /// reasons and a text model asked to transcribe simply cannot: a clip
    /// goes to whichever voice model answers, and the client never has to
    /// know which one that was.
    pub fn voice_chain(&self) -> Vec<(&UpstreamRow, &ModelRow)> {
        self.models
            .iter()
            .filter(|model| model.enabled && model.voice)
            .filter_map(|model| {
                self.upstreams
                    .iter()
                    .find(|up| up.id == model.upstream_id && up.enabled)
                    .map(|upstream| (upstream, model))
            })
            .collect()
    }

    /// Every model a client may ask for.
    pub fn model_names(&self) -> Vec<String> {
        self.chain_from("")
            .into_iter()
            .map(|(_, model)| model.name.clone())
            .collect()
    }
}

/// The provider shape `vd-llm` calls with, for one model of one upstream.
pub fn provider_for(upstream: &UpstreamRow, model: &ModelRow) -> ProviderConfig {
    ProviderConfig {
        id: upstream.id.clone(),
        label: if upstream.label.is_empty() {
            upstream.id.clone()
        } else {
            upstream.label.clone()
        },
        kind: upstream.kind,
        base_url: upstream.base_url.clone(),
        api_version: upstream.api_version.clone(),
        model: model.upstream_name().to_string(),
        extra_headers: upstream.extra_headers.clone(),
        temperature: 0.85,
        max_output_tokens: None,
        transcribe_model: String::new(),
        thinking_effort: String::new(),
        thinking_budget: None,
        // The gateway walks its own chain across upstreams, so the provider
        // itself is given one model and no fallbacks.
        model_chain: vec![],
        reasoning_dialect: upstream.reasoning_dialect.clone(),
        context_tokens: model.context_tokens,
        key_count: upstream.key_count,
    }
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(id: &str, enabled: bool) -> UpstreamRow {
        UpstreamRow {
            id: id.into(),
            label: id.into(),
            kind: ProviderKind::OpenaiCompatible,
            base_url: "https://example.invalid/v1".into(),
            api_version: "v1".into(),
            reasoning_dialect: "auto".into(),
            extra_headers: vec![],
            enabled,
            position: 0,
            key_count: 1,
        }
    }

    fn model(name: &str, upstream_id: &str, enabled: bool) -> ModelRow {
        ModelRow {
            name: name.into(),
            upstream_id: upstream_id.into(),
            upstream_name: String::new(),
            price_in: 1.0,
            price_cached: None,
            price_out: 2.0,
            context_tokens: None,
            enabled,
            position: 0,
            voice: false,
            price_request: 0.0,
        }
    }

    fn registry() -> Registry {
        Registry {
            upstreams: vec![upstream("openrouter", true), upstream("gemini", false)],
            models: vec![
                model("deepseek-chat", "openrouter", true),
                model("qwen-max", "openrouter", false),
                model("gemini-2.5-flash", "gemini", true),
            ],
            tiers: HashMap::new(),
            pools: HashMap::new(),
        }
    }

    /// A model switched off, or one whose upstream is switched off, is not
    /// tried at all — that is what the switch is for when an account runs out
    /// of credit at midnight.
    #[test]
    fn the_chain_skips_what_is_switched_off() {
        let names: Vec<String> = registry()
            .chain_from("deepseek-chat")
            .into_iter()
            .map(|(_, model)| model.name.clone())
            .collect();
        assert_eq!(names, ["deepseek-chat"]);
    }

    #[test]
    fn a_disabled_model_cannot_be_found_by_name() {
        let registry = registry();
        assert!(registry.find_model("deepseek-chat").is_some());
        assert!(registry.find_model("qwen-max").is_none());
        assert!(
            registry.find_model("gemini-2.5-flash").is_none(),
            "its upstream is off"
        );
    }

    /// What clients ask for and what the upstream calls it are two names, and
    /// only one of them goes on the wire.
    #[test]
    fn the_upstream_name_is_used_when_there_is_one() {
        let mut model = model("deepseek-chat", "openrouter", true);
        assert_eq!(model.upstream_name(), "deepseek-chat");
        model.upstream_name = "deepseek/deepseek-chat".into();
        assert_eq!(model.upstream_name(), "deepseek/deepseek-chat");
        let provider = provider_for(&upstream("openrouter", true), &model);
        assert_eq!(provider.model, "deepseek/deepseek-chat");
    }

    /// Editing the registry must not quietly forgive a key the provider has
    /// been refusing: a pool whose keys did not change is the same pool,
    /// cooldowns and tallies included. A pool whose keys did change is new,
    /// because a key nobody has tried has nothing held against it.
    #[test]
    fn reloading_keeps_the_punishment_of_an_unchanged_pool() {
        use vd_llm::keypool::KeyVerdict;

        let db = crate::db::Db::memory().unwrap();
        db.save_upstream(&upstream("gemini", true)).unwrap();
        db.add_key("gemini", "k1", 0).unwrap();

        let first = Registry::load(&db, None).unwrap();
        let pool = first.pool("gemini").unwrap();
        pool.report_failure(0, KeyVerdict::QuotaOrAuth);
        assert!(pool.acquire().is_none(), "the only key is parked");

        let second = Registry::load(&db, Some(&first)).unwrap();
        assert!(
            second.pool("gemini").unwrap().acquire().is_none(),
            "an edit elsewhere does not hand a refused key a clean slate"
        );

        db.add_key("gemini", "k2", 0).unwrap();
        let third = Registry::load(&db, Some(&second)).unwrap();
        assert!(
            third.pool("gemini").unwrap().acquire().is_some(),
            "a pool that gained a key is a new pool"
        );
    }

    /// What the chain does between models: the same keys, forgiven, because
    /// the quota that stopped them belonged to the model and not to them.
    #[test]
    fn forgiving_cooldowns_revives_the_pool() {
        use vd_llm::keypool::KeyVerdict;

        let db = crate::db::Db::memory().unwrap();
        db.save_upstream(&upstream("gemini", true)).unwrap();
        db.add_key("gemini", "k1", 0).unwrap();
        let registry = Registry::load(&db, None).unwrap();
        let pool = registry.pool("gemini").unwrap();

        pool.report_failure(0, KeyVerdict::RateLimited);
        assert!(pool.acquire().is_none());
        pool.clear_cooldowns();
        assert!(pool.acquire().is_some());
    }

    /// A model with no cache price is not secretly cheap.
    #[test]
    fn cache_price_falls_back_to_the_full_one() {
        let mut model = model("m", "u", true);
        assert_eq!(model.cached_price(), 1.0);
        model.price_cached = Some(0.1);
        assert_eq!(model.cached_price(), 0.1);
    }
}
