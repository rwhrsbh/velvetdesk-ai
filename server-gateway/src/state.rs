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
    /// Admission control: how many upstream calls run at once, and who is
    /// waiting for a turn.
    pub queue: Arc<crate::queue::Queue>,
}

impl AppState {
    pub fn new(cfg: GatewayConfig, db: Db) -> rusqlite::Result<AppState> {
        // The config file seeds an empty database and is then consulted only
        // for what belongs to the box itself — the port, the database path,
        // the public key licences are checked against.
        Registry::seed_if_empty(&db, &cfg)?;
        let limits = (
            cfg.max_inflight,
            cfg.max_per_license,
            cfg.max_queued,
            cfg.queue_wait_seconds,
        );
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
            queue: Arc::new(crate::queue::Queue::new(crate::queue::Limits {
                inflight: limits.0,
                per_license: limits.1,
                queued: limits.2,
                wait: std::time::Duration::from_secs(limits.3),
            })),
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

    /// Turn a dictated clip into text, trying each voice model in turn.
    ///
    /// The client asks for no model here and is told none back: dictation is
    /// a thing the subscription does, and which of the gateway's keys did it
    /// is the gateway's business. Returns the model that answered and what a
    /// clip on it costs, so the caller can bill it.
    pub async fn transcribe(
        &self,
        audio_base64: &str,
        mime: &str,
    ) -> Result<(String, f64, String), LlmError> {
        let attempts: Vec<(String, f64, ProviderConfig)> = {
            let registry = self.registry.read();
            registry
                .voice_chain()
                .into_iter()
                .map(|(upstream, entry)| {
                    let mut provider = provider_for(upstream, entry);
                    // For voice the model name belongs in the speech slot:
                    // the chat field is what a transcription ignores.
                    provider.transcribe_model = entry.upstream_name().to_string();
                    (entry.name.clone(), entry.price_request, provider)
                })
                .collect()
        };
        if attempts.is_empty() {
            return Err(LlmError::Provider(
                "the gateway has no voice model switched on".into(),
            ));
        }

        let mut last = LlmError::Provider("no voice model was tried".into());
        for (index, (model_name, price, provider)) in attempts.into_iter().enumerate() {
            let Some(pool) = self.registry.read().pool(&provider.id) else {
                continue;
            };
            if pool.is_empty() {
                continue;
            }
            if index > 0 {
                pool.clear_cooldowns();
            }
            let Some(lease) = pool.acquire() else {
                continue;
            };
            match vd_llm::catalog::transcribe(
                &self.llm.http,
                &provider,
                &lease.key,
                audio_base64,
                mime,
            )
            .await
            {
                Ok(text) => {
                    pool.report_success(lease.index);
                    return Ok((model_name, price, text));
                }
                Err(err) => {
                    pool.report_failure(lease.index, err.verdict());
                    log::warn!("{model_name} could not transcribe: {}", err.message());
                    last = LlmError::Provider(err.message());
                }
            }
        }
        Err(last)
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
        for (index, (model_name, provider)) in attempts.into_iter().enumerate() {
            let Some(pool) = self.registry.read().pool(&provider.id) else {
                continue;
            };
            if pool.is_empty() {
                // An upstream with no keys is a line in a table, not a place
                // to send anyone.
                continue;
            }

            // Quotas are per model, not per key. A key parked because one
            // model refused it has a fresh quota on the next one, so moving
            // down the chain forgives the cooldowns first — which is the whole
            // reason a pool of free keys is worth having.
            if index > 0 {
                pool.clear_cooldowns();
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
