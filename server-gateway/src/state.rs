//! What every request needs: the config, the book, the live registry of what
//! the gateway may spend on, and the walk down the chain of models until one
//! of them answers.

use std::collections::HashMap;
use std::sync::Arc;

use ed25519_dalek::VerifyingKey;
use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use tokio::sync::broadcast;

use vd_llm::{ChatRequest, ChatResponse, LlmClient, LlmError, ProviderConfig};

use crate::config::GatewayConfig;
use crate::db::Db;
use crate::registry::{provider_for, Registry};
use vd_license::public_key_from_base64;

/// One room's loudspeaker: whatever any member says, the others hear, tagged
/// with who said it so nobody hears themselves.
pub type RoomChannel = broadcast::Sender<(u64, Vec<u8>)>;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<GatewayConfig>,
    pub db: Db,
    pub llm: LlmClient,
    /// Upstreams, models, tiers and the key pools, as they stand right now.
    ///
    /// Behind a lock because the admin page edits them while requests are in
    /// flight: a key added at two in the morning is in the pool for the next
    /// request, and the call already running finishes on what it started with.
    pub registry: Arc<RwLock<Registry>>,
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
    pub fn new(cfg: GatewayConfig, db: Db) -> rusqlite::Result<AppState> {
        // The config file seeds an empty database and is then consulted only
        // for what belongs to the box itself — the port, the database path,
        // the public key licences are checked against.
        Registry::seed_if_empty(&db, &cfg)?;
        let registry = Registry::load(&db, None)?;
        let verifier = public_key_from_base64(&cfg.license_public_key);
        Ok(AppState {
            cfg: Arc::new(cfg),
            db,
            llm: LlmClient::new(),
            registry: Arc::new(RwLock::new(registry)),
            verifier,
            admin_token: std::env::var("VD_ADMIN_TOKEN").unwrap_or_default(),
            rooms: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Read the registry again after an edit, keeping the pools whose keys did
    /// not change — along with which of their keys are cooling down and why.
    pub fn reload(&self) -> rusqlite::Result<()> {
        let next = {
            let current = self.registry.read();
            Registry::load(&self.db, Some(&current))?
        };
        *self.registry.write() = next;
        Ok(())
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
        self.registry.read().model_names()
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
        // The chain is copied out from under the lock: a round of calls takes
        // minutes, and the admin page must not wait that long to save a key.
        let attempts: Vec<(String, ProviderConfig)> = {
            let registry = self.registry.read();
            registry
                .chain_from(model)
                .into_iter()
                .map(|(upstream, entry)| (entry.name.clone(), provider_for(upstream, entry)))
                .collect()
        };
        if attempts.is_empty() {
            return Err(LlmError::Provider(
                "the gateway has no model switched on".into(),
            ));
        }

        let mut last = LlmError::Provider("no model was tried".into());
        for (model_name, provider) in attempts {
            let Some(pool) = self.registry.read().pool(&provider.id) else {
                continue;
            };
            if pool.is_empty() {
                // An upstream with no keys is a line in a table, not a place
                // to send anyone.
                continue;
            }

            match self.llm.chat(&provider, pool, request, on_event).await {
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
