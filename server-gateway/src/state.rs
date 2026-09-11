//! What every request needs: the config, the book, the key pools, and the
//! walk down the chain of models until one of them answers.

use std::collections::HashMap;
use std::sync::Arc;

use ed25519_dalek::VerifyingKey;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::broadcast;

use vd_llm::keypool::KeyPool;
use vd_llm::{ChatRequest, ChatResponse, LlmClient, LlmError};

use crate::config::GatewayConfig;
use crate::db::Db;
use vd_license::public_key_from_base64;

/// One room's loudspeaker: whatever any member says, the others hear, tagged
/// with who said it so nobody hears themselves.
pub type RoomChannel = broadcast::Sender<(u64, Vec<u8>)>;

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
    /// Rooms two paired devices meet in, by room name. The gateway forwards
    /// sealed frames between them and can read none of it: the key that opens
    /// a frame never leaves the devices that were paired.
    pub rooms: Arc<Mutex<HashMap<String, RoomChannel>>>,
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
            rooms: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The room's channel, opened the moment the first device asks for it.
    ///
    /// Capacity is generous because a device catching up after a week sends a
    /// burst of records, and a peer that reads a little slower than the other
    /// writes should not be dropped mid-sync.
    pub fn room(&self, name: &str) -> RoomChannel {
        let mut rooms = self.rooms.lock();
        rooms
            .entry(name.to_string())
            .or_insert_with(|| broadcast::channel(256).0)
            .clone()
    }

    /// Forget a room nobody is left in, so a long-running gateway does not
    /// collect one entry per pairing it has ever seen.
    pub fn drop_room_if_empty(&self, name: &str) {
        let mut rooms = self.rooms.lock();
        if rooms
            .get(name)
            .is_some_and(|sender| sender.receiver_count() == 0)
        {
            rooms.remove(name);
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
