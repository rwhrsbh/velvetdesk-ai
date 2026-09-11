use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::storage::{read_json, write_json, Paths};
pub use vd_llm::provider::{default_dialect, mask_key, ProviderConfig, ProviderKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    /// Model drives tools autonomously across several turns.
    Auto,
    /// One call: draft + memory patch.
    Act,
    /// One call: memory patch only, no visible reply.
    Memorize,
    /// One call per man: a letter in her voice, nothing written to disk.
    Letters,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecurityLevel {
    /// Every mutation waits for operator approval.
    Ask,
    /// Additive mutations run automatically, destructive ones wait.
    Safe,
    /// Everything runs inside the sandbox, no dialogs.
    Yolo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub active_provider: Option<String>,
    #[serde(default = "default_mode")]
    pub agent_mode: AgentMode,
    #[serde(default = "default_security")]
    pub security_level: SecurityLevel,
    #[serde(default)]
    pub active_model_id: Option<String>,
    #[serde(default = "default_history_limit")]
    pub history_limit: usize,
    #[serde(default = "default_max_tool_turns")]
    pub max_tool_turns: usize,
    /// Extra house rules appended to every system prompt.
    #[serde(default)]
    pub global_style_rules: String,
    #[serde(default = "default_true")]
    pub telemetry_disabled: bool,
    /// UI language: "ru" or "en".
    #[serde(default = "default_language")]
    pub ui_language: String,
    /// Set once the operator has been through the guided tour, or skipped it.
    /// A fresh install opens it by itself; the button in the top bar opens it
    /// again whenever it is wanted.
    #[serde(default)]
    pub tour_done: bool,
    /// Provider used for voice dictation. None means "same as the chat one",
    /// which lets an operator chat through a text-only endpoint and still
    /// dictate through Gemini, Groq or a local Whisper server.
    #[serde(default)]
    pub speech_provider: Option<String>,
    /// "provider" (cloud) or "local" (downloaded Whisper, offline).
    #[serde(default = "default_speech_engine")]
    pub speech_engine: String,
    /// Ed25519 public key, base64, that a VelvetDesk Cloud licence is checked
    /// against. Empty until the operator pastes theirs, and an empty one means
    /// the licence is taken on trust by the gateway alone.
    #[serde(default)]
    pub cloud_public_key: String,
    /// Id of the downloaded model used when the engine is local.
    #[serde(default)]
    pub local_speech_model: String,
    /// `deviceId` of the microphone to record from. Empty means the system
    /// default, which is not always the one that actually works.
    #[serde(default)]
    pub speech_device: String,
    /// Compact the correspondence automatically once the prompt reaches this
    /// share of the context window.
    #[serde(default = "default_auto_compact")]
    pub auto_compact_at: f32,
    /// Folders an agent may read and write outside its own data directory.
    /// Empty by default: an agent starts isolated and has to ask.
    #[serde(default)]
    pub trusted_roots: Vec<crate::workspace::TrustedRoot>,
    /// Look for a newer release on start. The check only reads the release
    /// list; nothing is downloaded or installed without the operator saying so.
    #[serde(default = "default_true")]
    pub update_check: bool,
    /// A version the operator has already been offered and turned down.
    #[serde(default)]
    pub update_skipped: String,
}

fn default_auto_compact() -> f32 {
    0.85
}

fn default_mode() -> AgentMode {
    AgentMode::Auto
}

fn default_security() -> SecurityLevel {
    SecurityLevel::Safe
}

fn default_history_limit() -> usize {
    40
}

fn default_max_tool_turns() -> usize {
    8
}

fn default_true() -> bool {
    true
}

fn default_language() -> String {
    "ru".to_string()
}

fn default_speech_engine() -> String {
    "provider".to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            providers: vec![
                ProviderConfig {
                    id: "gemini".into(),
                    label: "Google Gemini".into(),
                    kind: ProviderKind::Gemini,
                    base_url: "https://generativelanguage.googleapis.com".into(),
                    api_version: "v1beta".into(),
                    model: "gemini-2.5-pro".into(),
                    extra_headers: vec![],
                    temperature: 0.85,
                    max_output_tokens: None,
                    transcribe_model: String::new(),
                    thinking_effort: String::new(),
                    thinking_budget: None,
                    model_chain: vec![],
                    reasoning_dialect: default_dialect(),
                    context_tokens: None,
                    key_count: 0,
                },
                ProviderConfig {
                    id: "openai-compatible".into(),
                    label: "OpenAI-compatible".into(),
                    kind: ProviderKind::OpenaiCompatible,
                    base_url: "https://openrouter.ai/api/v1".into(),
                    api_version: "v1".into(),
                    model: "deepseek/deepseek-chat".into(),
                    extra_headers: vec![],
                    temperature: 0.85,
                    max_output_tokens: None,
                    transcribe_model: String::new(),
                    thinking_effort: String::new(),
                    thinking_budget: None,
                    model_chain: vec![],
                    reasoning_dialect: default_dialect(),
                    context_tokens: None,
                    key_count: 0,
                },
                ProviderConfig {
                    id: "nvidia".into(),
                    label: "NVIDIA NIM".into(),
                    kind: ProviderKind::OpenaiCompatible,
                    base_url: "https://integrate.api.nvidia.com/v1".into(),
                    api_version: "v1".into(),
                    // Nothing is picked for the operator: the endpoint
                    // publishes its whole catalogue, and naming one model here
                    // would only be a guess at which of them they want.
                    model: String::new(),
                    extra_headers: vec![],
                    temperature: 0.85,
                    max_output_tokens: None,
                    transcribe_model: String::new(),
                    thinking_effort: String::new(),
                    thinking_budget: None,
                    model_chain: vec![],
                    reasoning_dialect: default_dialect(),
                    context_tokens: None,
                    key_count: 0,
                },
                ProviderConfig {
                    id: "velvetdesk-cloud".into(),
                    label: "VelvetDesk Cloud".into(),
                    kind: ProviderKind::OpenaiCompatible,
                    // The operator's own gateway. Nothing is sent anywhere by
                    // default: without a licence key this provider is inert,
                    // exactly like the others without their keys.
                    base_url: "https://cloud.velvetdesk.ai/v1".into(),
                    api_version: "v1".into(),
                    model: "deepseek-chat".into(),
                    extra_headers: vec![],
                    temperature: 0.85,
                    max_output_tokens: None,
                    transcribe_model: String::new(),
                    thinking_effort: String::new(),
                    thinking_budget: None,
                    model_chain: vec![],
                    reasoning_dialect: default_dialect(),
                    context_tokens: None,
                    key_count: 0,
                },
                ProviderConfig {
                    id: "groq".into(),
                    label: "Groq / Whisper".into(),
                    kind: ProviderKind::OpenaiCompatible,
                    base_url: "https://api.groq.com/openai/v1".into(),
                    api_version: "v1".into(),
                    model: "llama-3.3-70b-versatile".into(),
                    extra_headers: vec![],
                    temperature: 0.85,
                    max_output_tokens: None,
                    transcribe_model: "whisper-large-v3-turbo".into(),
                    thinking_effort: String::new(),
                    thinking_budget: None,
                    model_chain: vec![],
                    reasoning_dialect: default_dialect(),
                    context_tokens: None,
                    key_count: 0,
                },
            ],
            active_provider: Some("gemini".into()),
            agent_mode: AgentMode::Auto,
            security_level: SecurityLevel::Safe,
            active_model_id: None,
            history_limit: 40,
            max_tool_turns: 8,
            global_style_rules: String::new(),
            telemetry_disabled: true,
            ui_language: default_language(),
            tour_done: false,
            speech_provider: None,
            speech_engine: default_speech_engine(),
            cloud_public_key: String::new(),
            local_speech_model: String::new(),
            trusted_roots: vec![],
            update_check: true,
            update_skipped: String::new(),
            speech_device: String::new(),
            auto_compact_at: default_auto_compact(),
        }
    }
}

impl Settings {
    pub fn load(paths: &Paths) -> Result<Settings> {
        let mut settings = read_json::<Settings>(&paths.settings_file())?.unwrap_or_default();
        // A provider added in a later version would otherwise never appear for
        // anyone who has used the app before: the settings file on disk holds
        // the list it was written with. Ones the operator has edited are left
        // exactly as they are — only what is missing is added.
        for provider in Settings::default().providers {
            if !settings.providers.iter().any(|p| p.id == provider.id) {
                settings.providers.push(provider);
            }
        }
        Ok(settings)
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        write_json(&paths.settings_file(), self)
    }

    pub fn provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|p| p.id == id)
    }

    /// Provider that handles dictation: the dedicated one when set, else the
    /// chat provider.
    pub fn speech(&self) -> Option<&ProviderConfig> {
        match &self.speech_provider {
            Some(id) => self.provider(id).or_else(|| self.active()),
            None => self.active(),
        }
    }

    pub fn active(&self) -> Option<&ProviderConfig> {
        match &self.active_provider {
            Some(id) => self.provider(id),
            None => self.providers.first(),
        }
    }
}

/// API keys live in a separate file so the settings blob can be shared safely.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Secrets {
    /// provider id -> ordered key pool
    #[serde(default)]
    pub keys: std::collections::HashMap<String, Vec<String>>,
}

impl Secrets {
    pub fn load(paths: &Paths) -> Result<Secrets> {
        Ok(read_json::<Secrets>(&paths.secrets_file())?.unwrap_or_default())
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        write_json(&paths.secrets_file(), self)?;
        restrict_permissions(&paths.secrets_file());
        Ok(())
    }

    pub fn for_provider(&self, provider_id: &str) -> Vec<String> {
        self.keys.get(provider_id).cloned().unwrap_or_default()
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A provider that ships without a model — the operator picks one from
    /// the endpoint's own catalogue — has no models until they do, and an
    /// empty name must never be sent as if it were one.
    #[test]
    fn a_provider_with_no_model_chosen_has_none() {
        let mut provider = Settings::default()
            .providers
            .into_iter()
            .find(|p| p.id == "nvidia")
            .unwrap();
        assert!(provider.models().is_empty());

        provider.model = "deepseek-ai/deepseek-v4-flash-0731".into();
        assert_eq!(provider.models(), vec![provider.model.clone()]);
    }

    /// A provider added in a later version has to reach the people already
    /// running the app: their settings file lists the providers it was written
    /// with, and nothing would ever add the new one to it.
    #[test]
    fn a_new_provider_reaches_an_existing_install() {
        let dir = std::env::temp_dir().join(format!("velvet-settings-{}", crate::models::new_id()));
        let paths = crate::storage::Paths::new(dir).unwrap();

        let mut old = Settings::default();
        old.providers.retain(|p| p.id != "nvidia");
        old.save(&paths).unwrap();

        let loaded = Settings::load(&paths).unwrap();
        assert!(loaded.providers.iter().any(|p| p.id == "nvidia"));
        assert_eq!(loaded.providers.len(), Settings::default().providers.len());
    }

    /// The chain is "this one, then these", with no repeats: a fallback that
    /// names the primary again would waste a whole round of keys on it.
    #[test]
    fn the_model_chain_starts_with_the_chosen_model() {
        let mut provider = Settings::default().providers[0].clone();
        provider.model = "gemini-3.5-flash".into();
        provider.model_chain = vec![
            "gemini-3.5-flash".into(),
            " gemini-3.5-flash-lite ".into(),
            String::new(),
            "gemini-2.5-flash".into(),
        ];

        assert_eq!(
            provider.models(),
            vec![
                "gemini-3.5-flash".to_string(),
                "gemini-3.5-flash-lite".to_string(),
                "gemini-2.5-flash".to_string(),
            ]
        );

        provider.model_chain.clear();
        assert_eq!(provider.models(), vec!["gemini-3.5-flash".to_string()]);
    }
}
