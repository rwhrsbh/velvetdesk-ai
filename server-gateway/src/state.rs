//! What every request needs: the config, the book, the key pools, and the
//! walk down the chain of models until one of them answers.

use std::collections::HashMap;
use std::sync::Arc;

use ed25519_dalek::VerifyingKey;
use serde_json::Value;

use vd_llm::keypool::KeyPool;
use vd_llm::{ChatRequest, ChatResponse, LlmClient, LlmError};

use crate::config::GatewayConfig;
use crate::db::Db;
use crate::license::public_key_from_base64;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<GatewayConfig>,
    pub db: Db,
    pub llm: LlmClient,
    /// One pool per upstream, shared by every request that goes there, so a
    /// key cooling down after a 429 is cooling down for everyone.
    pub pools: Arc<HashMap<String, Arc<KeyPool>>>,
    pub verifier: Option<VerifyingKey>,
    /// Token for the admin endpoints, from `VD_ADMIN_TOKEN`. Empty means the
    /// admin endpoints are closed rather than open.
    pub admin_token: String,
}

impl AppState {
    pub fn new(cfg: GatewayConfig, db: Db) -> AppState {
        let pools = cfg
            .upstreams
            .iter()
            .map(|upstream| {
                (
                    upstream.id.clone(),
                    Arc::new(KeyPool::new(upstream.resolved_keys())),
                )
            })
            .collect();
        let verifier = public_key_from_base64(&cfg.license_public_key);
        AppState {
            cfg: Arc::new(cfg),
            db,
            llm: LlmClient::new(),
            pools: Arc::new(pools),
            verifier,
            admin_token: std::env::var("VD_ADMIN_TOKEN").unwrap_or_default(),
        }
    }

    /// Every model the gateway will answer with, in configured order.
    pub fn model_names(&self) -> Vec<String> {
        self.cfg
            .upstreams
            .iter()
            .flat_map(|upstream| upstream.models.iter().map(|model| model.name.clone()))
            .collect()
    }

    /// Send the request, walking down the chain from the model that was asked
    /// for until one answers.
    ///
    /// Within one model the crate rotates the upstream's keys and waits out
    /// their cooldowns; between models the gateway moves on, because a quota
    /// is per model and a refusal is per model — neither is cured by trying
    /// the same one again with a different key.
    ///
    /// Returns the model that actually answered, which is not always the one
    /// that was asked for, and the client is told so in the response.
    pub async fn call(
        &self,
        model: &str,
        request: &ChatRequest,
        on_event: &(dyn Fn(Value) + Send + Sync),
    ) -> Result<(String, ChatResponse), LlmError> {
        let chain: Vec<(String, String)> = self
            .cfg
            .chain_from(model)
            .into_iter()
            .map(|(upstream, model)| (upstream.id.clone(), model.name.clone()))
            .collect();
        if chain.is_empty() {
            return Err(LlmError::Provider("the gateway has no models".into()));
        }

        let mut last = LlmError::Provider("no model was tried".into());
        for (upstream_id, model_name) in chain {
            let Some((upstream, entry)) = self.cfg.find_model(&model_name) else {
                continue;
            };
            if upstream.id != upstream_id {
                continue;
            }
            let Some(pool) = self.pools.get(&upstream_id) else {
                continue;
            };
            if pool.is_empty() {
                // An upstream with no keys is a line in the config file, not
                // a place to send anyone.
                continue;
            }

            let provider = upstream.provider(entry);
            match self
                .llm
                .chat(&provider, pool.clone(), request, on_event)
                .await
            {
                Ok(mut response) => {
                    response.model = model_name.clone();
                    return Ok((model_name, response));
                }
                Err(err) => {
                    log::warn!("{model_name} failed: {err}");
                    on_event(serde_json::json!({
                        "kind": "model_switch",
                        "from": model_name,
                        "reason": err.to_string(),
                    }));
                    last = err;
                }
            }
        }
        Err(last)
    }
}
