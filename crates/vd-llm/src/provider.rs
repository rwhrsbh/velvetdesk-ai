//! What a provider is: where it lives, which model it answers with, and the
//! knobs both vendors spell differently.
//!
//! Shared by the desktop and the gateway — the desktop reads it from
//! `settings.json`, the gateway from its own table, and the call itself does
//! not care which one filled it in.

use serde::{Deserialize, Serialize};

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
    /// Models to try in order when the one above is unavailable. Quotas are
    /// per model, not per key, so a day spent on one model is not a day spent
    /// on the next.
    #[serde(default)]
    pub model_chain: Vec<String>,
    /// Context window in tokens. Empty falls back to a guess from the model
    /// name, which is what drives automatic compaction.
    #[serde(default)]
    pub context_tokens: Option<u32>,
    /// Number of keys stored for this provider (mirrored from secrets).
    #[serde(default, skip_deserializing)]
    pub key_count: usize,
}

/// A provider whose reasoning knob has not been named: work it out from the
/// endpoint at call time.
pub fn default_dialect() -> String {
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
    /// Every model this provider may answer with, in order: the chosen one
    /// first, then the fallbacks, without repeats.
    pub fn models(&self) -> Vec<String> {
        let mut models = vec![];
        // A provider whose model has not been chosen yet has none: an empty
        // name reaches the endpoint as a request for a model called "", and
        // the error it answers with explains nothing.
        if !self.model.trim().is_empty() {
            models.push(self.model.trim().to_string());
        }
        for fallback in &self.model_chain {
            let fallback = fallback.trim();
            if !fallback.is_empty() && !models.iter().any(|m| m == fallback) {
                models.push(fallback.to_string());
            }
        }
        models
    }

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
        } else if url.contains("api.nvidia.com") {
            "nvidia"
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

pub fn default_api_version() -> String {
    "v1beta".to_string()
}

pub fn default_temperature() -> f32 {
    0.85
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
