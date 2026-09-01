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
    /// Number of keys stored for this provider (mirrored from secrets).
    #[serde(default, skip_deserializing)]
    pub key_count: usize,
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
