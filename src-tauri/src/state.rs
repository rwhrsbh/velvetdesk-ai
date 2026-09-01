use parking_lot::RwLock;
use std::collections::HashMap;
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
        })
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
        next.save(&self.paths)?;
        *self.settings.write() = next;
        self.reload_pools();
        Ok(())
    }

    pub fn save_secrets(&self, next: Secrets) -> Result<()> {
        next.save(&self.paths)?;
        *self.secrets.write() = next;
        self.reload_pools();
        Ok(())
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
