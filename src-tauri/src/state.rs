use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::agent::tools::PendingAction;
use crate::config::{ProviderConfig, Secrets, Settings};
use crate::error::{AppError, Result};
use crate::llm::keypool::KeyPool;
use crate::llm::LlmClient;
use crate::storage::Paths;

pub struct AppState {
    pub paths: Paths,
    pub settings: RwLock<Settings>,
    pub secrets: RwLock<Secrets>,
    pools: RwLock<HashMap<String, Arc<KeyPool>>>,
    pub pending: RwLock<Vec<PendingAction>>,
    pub llm: LlmClient,
    /// The stop switch of every run in flight, by run id.
    ///
    /// A run is a long thing — several calls to the provider and a tool or two
    /// between them — and the operator watching it write the wrong letter
    /// should not have to sit through the rest of it.
    cancels: RwLock<HashMap<String, Arc<AtomicBool>>>,
}

impl AppState {
    pub fn new(paths: Paths) -> Result<Self> {
        let settings = Settings::load(&paths)?;
        let secrets = Secrets::load(&paths)?;
        let mut pools = HashMap::new();
        for provider in &settings.providers {
            pools.insert(
                provider.id.clone(),
                Arc::new(KeyPool::new(secrets.for_provider(&provider.id))),
            );
        }
        Ok(AppState {
            paths,
            settings: RwLock::new(settings),
            secrets: RwLock::new(secrets),
            pools: RwLock::new(pools),
            pending: RwLock::new(vec![]),
            llm: LlmClient::new(),
            cancels: RwLock::new(HashMap::new()),
        })
    }

    /// The stop switch for a run, made on the spot if the run is new.
    ///
    /// Stop can arrive before the run has registered itself — the operator
    /// presses it the instant they see the spinner — so a flag asked for by
    /// either side is the same flag.
    pub fn cancel_flag(&self, run_id: &str) -> Arc<AtomicBool> {
        let mut cancels = self.cancels.write();
        cancels
            .entry(run_id.to_string())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone()
    }

    /// Raise the stop switch of a run.
    pub fn cancel_run(&self, run_id: &str) {
        self.cancel_flag(run_id).store(true, Ordering::Relaxed);
    }

    /// Forget a finished run's switch.
    pub fn drop_cancel(&self, run_id: &str) {
        self.cancels.write().remove(run_id);
    }

    /// Settings snapshot with live key counts filled in.
    pub fn settings_view(&self) -> Settings {
        let mut settings = self.settings.read().clone();
        let secrets = self.secrets.read();
        for provider in &mut settings.providers {
            provider.key_count = secrets.for_provider(&provider.id).len();
        }
        settings
    }

    pub fn save_settings(&self, next: Settings) -> Result<()> {
        let mut next = next;
        next.pin_cloud();
        next.save(&self.paths)?;
        *self.settings.write() = next;
        self.reload_pools();
        self.refresh_entitlement();
        Ok(())
    }

    pub fn save_secrets(&self, next: Secrets) -> Result<()> {
        next.save(&self.paths)?;
        *self.secrets.write() = next;
        self.reload_pools();
        self.refresh_entitlement();
        Ok(())
    }

    /// Re-read the licence and publish what it allows.
    ///
    /// Called wherever the licence can change — startup, a saved key, a saved
    /// settings file — so the caps the agent's tools consult are never a
    /// version behind the token the operator just pasted in.
    pub fn refresh_entitlement(&self) -> crate::entitlement::Entitlement {
        let token = self
            .secrets
            .read()
            .for_provider(crate::entitlement::CLOUD_PROVIDER)
            .first()
            .cloned()
            .unwrap_or_default();
        let entitlement = crate::entitlement::read(&token);
        crate::entitlement::set_limits(entitlement.limits);
        entitlement
    }

    pub fn reload_pools(&self) {
        let settings = self.settings.read().clone();
        let secrets = self.secrets.read().clone();
        let mut pools = self.pools.write();
        pools.clear();
        for provider in &settings.providers {
            pools.insert(
                provider.id.clone(),
                Arc::new(KeyPool::new(secrets.for_provider(&provider.id))),
            );
        }
    }

    pub fn pool(&self, provider_id: &str) -> Arc<KeyPool> {
        if let Some(pool) = self.pools.read().get(provider_id) {
            return pool.clone();
        }
        let keys = self.secrets.read().for_provider(provider_id);
        let pool = Arc::new(KeyPool::new(keys));
        self.pools
            .write()
            .insert(provider_id.to_string(), pool.clone());
        pool
    }

    /// The provider currently selected in settings.
    pub fn active_provider(&self) -> Result<ProviderConfig> {
        let settings = self.settings.read();
        settings
            .active()
            .cloned()
            .ok_or_else(|| AppError::Invalid("no LLM provider configured".into()))
    }
}
