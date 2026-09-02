use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::storage::{read_json, write_json, Paths};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Google Generative Language API (Gemini), v1 or v1beta.
    Gemini,
    /// Any OpenAI-compatible /chat/completions endpoint.
    OpenaiCompatible,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    pub label: String,
    pub kind: ProviderKind,
    /// Gemini default: https://generativelanguage.googleapis.com
    /// OpenAI-compatible default: https://api.openai.com/v1
    pub base_url: String,
    /// Gemini only: v1 or v1beta.
    #[serde(default = "default_api_version")]
    pub api_version: String,
    pub model: String,
    #[serde(default)]
    pub extra_headers: Vec<(String, String)>,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// Model used for voice dictation. Empty means "same as `model`" for
    /// Gemini and `whisper-1` for OpenAI-compatible endpoints.
    #[serde(default)]
    pub transcribe_model: String,
    /// How hard the model should think. Empty means "provider default"; the
    /// rest are the levels every vendor now agrees on:
    /// none | minimal | low | medium | high | xhigh.
    #[serde(default)]
    pub thinking_effort: String,
    /// Thinking budget in tokens, for models that take a number instead of a
    /// level (Gemini 2.x, Qwen). `-1` asks Gemini to decide for itself.
    #[serde(default)]
    pub thinking_budget: Option<i32>,
    /// Which spelling of the reasoning knob this endpoint understands:
    /// auto | openai | openrouter | qwen. Ignored for Gemini, which is native.
    #[serde(default = "default_dialect")]
    pub reasoning_dialect: String,
    /// Context window in tokens. Empty falls back to a guess from the model
    /// name, which is what drives automatic compaction.
    #[serde(default)]
    pub context_tokens: Option<u32>,
    /// Number of keys stored for this provider (mirrored from secrets).
    #[serde(default, skip_deserializing)]
    pub key_count: usize,
}

fn default_dialect() -> String {
    "auto".into()
}

/// Rough context windows, by the part of the model name that gives it away.
/// Only used when the operator has not typed a number of their own.
const CONTEXT_GUESSES: &[(&str, u32)] = &[
    ("gemini-3", 1_048_576),
    ("gemini-2.5", 1_048_576),
    ("gemini-2.0", 1_048_576),
    ("gemini-1.5", 1_048_576),
    ("gemini", 32_768),
    ("gpt-5", 400_000),
    ("gpt-4.1", 1_047_576),
    ("gpt-4o", 128_000),
    ("o3", 200_000),
    ("o4", 200_000),
    ("claude", 200_000),
    ("deepseek", 128_000),
    ("qwen", 131_072),
    ("llama-4", 131_072),
    ("llama", 32_768),
    ("mistral", 32_768),
    ("kimi", 131_072),
];

impl ProviderConfig {
    /// Context window used for the "how full is it" figure and for deciding
    /// when to compact.
    pub fn context_window(&self) -> u32 {
        if let Some(explicit) = self.context_tokens.filter(|n| *n > 0) {
            return explicit;
        }
        let model = self.model.to_lowercase();
        CONTEXT_GUESSES
            .iter()
            .find(|(needle, _)| model.contains(needle))
            .map(|(_, size)| *size)
            .unwrap_or(128_000)
    }

    /// The reasoning spelling to use, inferred from the endpoint when the
    /// operator left it on "auto".
    pub fn dialect(&self) -> &str {
        if self.reasoning_dialect != "auto" && !self.reasoning_dialect.is_empty() {
            return &self.reasoning_dialect;
        }
        let url = self.base_url.to_lowercase();
        if url.contains("openrouter") {
            "openrouter"
        } else if url.contains("dashscope") || url.contains("aliyun") || url.contains("qwen") {
            "qwen"
        } else {
            "openai"
        }
    }
}

impl ProviderConfig {
    /// Model that handles audio input for this provider.
    pub fn speech_model(&self) -> String {
        if !self.transcribe_model.trim().is_empty() {
            return self.transcribe_model.trim().to_string();
        }
        match self.kind {
            ProviderKind::Gemini => self.model.clone(),
            ProviderKind::OpenaiCompatible => "whisper-1".to_string(),
        }
    }
}

fn default_api_version() -> String {
    "v1beta".to_string()
}

fn default_temperature() -> f32 {
    0.85
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    /// Model drives tools autonomously across several turns.
    Auto,
    /// One call: draft + memory patch.
    Act,
    /// One call: memory patch only, no visible reply.
    Memorize,
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
    /// Provider used for voice dictation. None means "same as the chat one",
    /// which lets an operator chat through a text-only endpoint and still
    /// dictate through Gemini, Groq or a local Whisper server.
    #[serde(default)]
    pub speech_provider: Option<String>,
    /// "provider" (cloud) or "local" (downloaded Whisper, offline).
    #[serde(default = "default_speech_engine")]
    pub speech_engine: String,
    /// Id of the downloaded model used when the engine is local.
    #[serde(default)]
    pub local_speech_model: String,
    /// Dictation language: "ru", "uk", "en" — or empty to follow the UI. An
    /// empty language makes Whisper fall back to English and quietly translate,
    /// which is never what an operator dictating Russian wants.
    #[serde(default)]
    pub speech_language: String,
    /// Compact the correspondence automatically once the prompt reaches this
    /// share of the context window.
    #[serde(default = "default_auto_compact")]
    pub auto_compact_at: f32,
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
            speech_provider: None,
            speech_engine: default_speech_engine(),
            local_speech_model: String::new(),
            speech_language: String::new(),
            auto_compact_at: default_auto_compact(),
        }
    }
}

impl Settings {
    pub fn load(paths: &Paths) -> Result<Settings> {
        Ok(read_json::<Settings>(&paths.settings_file())?.unwrap_or_default())
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

/// Mask a key for display: `AIzaS...9fA`.
pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 10 {
        return "*".repeat(chars.len());
    }
    let head: String = chars[..5].iter().collect();
    let tail: String = chars[chars.len() - 3..].iter().collect();
    format!("{head}...{tail}")
}

#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) {}
