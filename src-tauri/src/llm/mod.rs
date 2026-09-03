pub mod catalog;
pub mod gemini;
pub mod keypool;
pub mod openai;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

use crate::config::{ProviderConfig, ProviderKind};
use crate::error::{AppError, Result};
use keypool::{KeyPool, KeyVerdict};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
    /// Gemini 3 signs every function call it makes and refuses the next turn
    /// unless the signature comes back with it. Empty for other providers.
    #[serde(default)]
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: Role,
    #[serde(default)]
    pub content: String,
    /// Pictures the operator attached to this turn, sent inline.
    #[serde(default)]
    pub images: Vec<ImagePart>,
    /// Assistant turns may carry tool calls.
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// Tool turns answer a specific call.
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
}

/// An attached picture: its type, and its bytes base64-encoded without the
/// `data:` prefix. Both vendors take images this way — Gemini as `inlineData`,
/// OpenAI-compatible endpoints as a `data:` URL — so one shape serves both.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImagePart {
    pub mime: String,
    pub data: String,
}

impl LlmMessage {
    pub fn user(text: impl Into<String>) -> Self {
        LlmMessage {
            role: Role::User,
            content: text.into(),
            images: vec![],
            tool_calls: vec![],
            tool_call_id: None,
            tool_name: None,
        }
    }

    pub fn assistant(text: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        LlmMessage {
            role: Role::Assistant,
            content: text.into(),
            images: vec![],
            tool_calls,
            tool_call_id: None,
            tool_name: None,
        }
    }

    /// The operator's turn with attachments: the same message, plus what they
    /// pasted or picked, so the model looks at the screenshot it is asked about.
    pub fn user_with_images(text: impl Into<String>, images: Vec<ImagePart>) -> Self {
        LlmMessage {
            images,
            ..LlmMessage::user(text)
        }
    }

    pub fn tool_result(call: &ToolCall, content: impl Into<String>) -> Self {
        LlmMessage {
            role: Role::Tool,
            content: content.into(),
            images: vec![],
            tool_calls: vec![],
            tool_call_id: Some(call.id.clone()),
            tool_name: Some(call.name.clone()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON-schema object describing the parameters.
    pub parameters: Value,
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub system: String,
    pub messages: Vec<LlmMessage>,
    pub tools: Vec<ToolDef>,
    pub temperature: f32,
    pub max_output_tokens: Option<u32>,
    /// Ask the provider for a raw JSON object (used by ACT / MEMORIZE).
    pub force_json: bool,
    /// How hard to think on this call.
    pub thinking: Thinking,
    /// Stream the answer as it is written. Falls back to a single response if
    /// the endpoint does not support it.
    pub stream: bool,
}

/// Reasoning controls, in the two shapes vendors offer: a level, or a number
/// of tokens. Empty and `None` mean "leave the provider on its default".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Thinking {
    #[serde(default)]
    pub effort: String,
    #[serde(default)]
    pub budget_tokens: Option<i32>,
}

impl Thinking {
    pub fn is_default(&self) -> bool {
        self.effort.trim().is_empty() && self.budget_tokens.is_none()
    }

    /// Levels every vendor understands, normalised.
    pub fn level(&self) -> Option<&str> {
        match self.effort.trim() {
            "" => None,
            other => Some(other),
        }
    }
}

impl ChatRequest {
    pub fn new(system: impl Into<String>) -> Self {
        ChatRequest {
            system: system.into(),
            messages: vec![],
            tools: vec![],
            temperature: 0.85,
            max_output_tokens: None,
            force_json: false,
            thinking: Thinking::default(),
            stream: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub text: String,
    /// The provider's answer exactly as it arrived, capped. Kept so an empty
    /// or surprising reply can be read rather than guessed at.
    #[serde(default)]
    pub raw: String,
    /// Which model answered — the first of the chain, or a fallback.
    #[serde(default)]
    pub model: String,
    /// The model's summary of its own reasoning, when it reports one.
    #[serde(default)]
    pub thoughts: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default)]
    pub finish_reason: String,
    /// Index of the API key that produced this answer.
    #[serde(default)]
    pub key_index: usize,
    #[serde(default)]
    pub attempts: usize,
}

/// Provider-level failure with enough context for the rotation policy.
#[derive(Debug)]
pub enum CallError {
    Status { code: u16, body: String },
    Transport(String),
    Parse(String),
}

impl CallError {
    pub fn message(&self) -> String {
        match self {
            CallError::Status { code, body } => {
                let short: String = body.chars().take(400).collect();
                format!("HTTP {code}: {short}")
            }
            CallError::Transport(e) => format!("transport: {e}"),
            CallError::Parse(e) => format!("parse: {e}"),
        }
    }

    fn verdict(&self) -> KeyVerdict {
        match self {
            CallError::Status { code, .. } => match code {
                429 => KeyVerdict::RateLimited,
                401 | 403 => KeyVerdict::QuotaOrAuth,
                408 | 409 | 425 => KeyVerdict::Transient,
                c if *c >= 500 => KeyVerdict::ServerError,
                _ => KeyVerdict::Fatal,
            },
            CallError::Transport(_) => KeyVerdict::Transient,
            CallError::Parse(_) => KeyVerdict::Fatal,
        }
    }
}

#[derive(Clone)]
pub struct LlmClient {
    pub http: reqwest::Client,
}

/// Payloads are kept for inspection, not for storage: enough to see what came
/// back, not enough to bloat a chat log.
pub const RAW_LIMIT: usize = 24_000;

/// Cut a payload to something a person can open in a dialog.
pub fn cap_raw(text: &str) -> String {
    if text.len() <= RAW_LIMIT {
        return text.to_string();
    }
    let mut cut = RAW_LIMIT;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… ({} more characters)", &text[..cut], text.len() - cut)
}

/// True when a failure looks like "this endpoint does not stream" rather than
/// a problem with the request itself.
fn cannot_stream(result: &std::result::Result<ChatResponse, CallError>) -> bool {
    let Err(CallError::Status { code, body }) = result else {
        return false;
    };
    let body = body.to_lowercase();
    *code == 404 || (*code == 400 && (body.contains("stream") || body.contains("not supported")))
}

impl LlmClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .user_agent("VelvetDesk/0.1")
            .build()
            .unwrap_or_default();
        LlmClient { http }
    }

    /// Send a chat request, working down the provider's list of models and
    /// rotating its keys within each one.
    ///
    /// Quotas are per model, not per key: when every key has been refused by
    /// one model, the next model in the list starts again with the first key.
    /// Cooldowns are dropped at that point for the same reason — a key parked
    /// for quota on one model has a fresh quota on the next.
    pub async fn chat(
        &self,
        provider: &ProviderConfig,
        pool: Arc<KeyPool>,
        request: &ChatRequest,
        on_event: &(dyn Fn(Value) + Send + Sync),
    ) -> Result<ChatResponse> {
        let models = provider.models();
        let mut last_error = AppError::Provider("no model was tried".into());

        for (index, model) in models.iter().enumerate() {
            let mut attempt_provider = provider.clone();
            attempt_provider.model = model.clone();

            match self
                .chat_one_model(&attempt_provider, pool.clone(), request, on_event)
                .await
            {
                Ok(mut response) => {
                    response.model = model.clone();
                    return Ok(response);
                }
                Err(err) => {
                    last_error = err;
                    let Some(next) = models.get(index + 1) else {
                        break;
                    };
                    on_event(serde_json::json!({
                        "kind": "model_switch",
                        "from": model,
                        "to": next,
                        "reason": last_error.to_string(),
                    }));
                    pool.clear_cooldowns();
                }
            }
        }

        Err(last_error)
    }

    /// One model, every key it has.
    async fn chat_one_model(
        &self,
        provider: &ProviderConfig,
        pool: Arc<KeyPool>,
        request: &ChatRequest,
        on_event: &(dyn Fn(Value) + Send + Sync),
    ) -> Result<ChatResponse> {
        let key_total = pool.len();
        if key_total == 0 {
            return Err(AppError::NoKeys(format!(
                "provider {} has no API keys configured",
                provider.id
            )));
        }
        let max_attempts = (key_total * 2).clamp(2, 8);
        let mut last_error = String::from("unknown error");

        for attempt in 0..max_attempts {
            let lease = match pool.acquire() {
                Some(lease) => lease,
                None => {
                    let wait = pool.shortest_cooldown().unwrap_or(Duration::from_secs(2));
                    on_event(serde_json::json!({
                        "kind": "llm_wait",
                        "message": format!("all keys cooling down, waiting {}s", wait.as_secs().max(1)),
                    }));
                    tokio::time::sleep(wait.min(Duration::from_secs(30))).await;
                    continue;
                }
            };

            let mut result = match (request.stream, provider.kind) {
                (true, ProviderKind::Gemini) => {
                    gemini::call_streaming(&self.http, provider, &lease.key, request, on_event)
                        .await
                }
                (true, ProviderKind::OpenaiCompatible) => {
                    openai::call_streaming(&self.http, provider, &lease.key, request, on_event)
                        .await
                }
                (false, ProviderKind::Gemini) => {
                    gemini::call(&self.http, provider, &lease.key, request).await
                }
                (false, ProviderKind::OpenaiCompatible) => {
                    openai::call(&self.http, provider, &lease.key, request).await
                }
            };

            // Not every endpoint streams — a local server or an old proxy may
            // answer 404 or 400 for it. One plain call settles that, instead of
            // the operator losing the turn.
            if request.stream && cannot_stream(&result) {
                on_event(serde_json::json!({ "kind": "no_stream" }));
                result = match provider.kind {
                    ProviderKind::Gemini => {
                        gemini::call(&self.http, provider, &lease.key, request).await
                    }
                    ProviderKind::OpenaiCompatible => {
                        openai::call(&self.http, provider, &lease.key, request).await
                    }
                };
            }

            match result {
                Ok(mut response) => {
                    pool.report_success(lease.index);
                    response.key_index = lease.index;
                    response.attempts = attempt + 1;
                    return Ok(response);
                }
                Err(err) => {
                    let verdict = err.verdict();
                    last_error = err.message();
                    pool.report_failure(lease.index, verdict);
                    on_event(serde_json::json!({
                        "kind": "llm_retry",
                        "attempt": attempt + 1,
                        "key_index": lease.index,
                        "verdict": format!("{:?}", verdict),
                        "message": last_error,
                    }));
                    if matches!(verdict, KeyVerdict::Fatal) && key_total == 1 {
                        return Err(AppError::Provider(last_error));
                    }
                    // Exponential backoff 1s -> 2s -> 4s (capped at 8s).
                    let backoff = 1u64 << attempt.min(3);
                    tokio::time::sleep(Duration::from_secs(backoff.min(8))).await;
                }
            }
        }

        Err(AppError::Provider(format!(
            "all {key_total} key(s) failed after {max_attempts} attempts: {last_error}"
        )))
    }
}

impl Default for LlmClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract the first balanced JSON object from a model answer that may be
/// wrapped in prose or a ```json fence.
pub fn extract_json_object(text: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(text.trim()) {
        return Some(v);
    }
    let cleaned = text
        .replace("```json", "```")
        .split("```")
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    for chunk in cleaned {
        if let Some(v) = scan_object(&chunk) {
            return Some(v);
        }
    }
    scan_object(text)
}

fn scan_object(text: &str) -> Option<Value> {
    let bytes: Vec<char> = text.chars().collect();
    let start = bytes.iter().position(|c| *c == '{')?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in bytes.iter().enumerate().skip(start) {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    let slice: String = bytes[start..=i].iter().collect();
                    return serde_json::from_str::<Value>(&slice).ok();
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_fenced_json() {
        let text = "Sure, here:\n```json\n{\"reply\": \"hi {there}\", \"patch\": {}}\n```\nDone.";
        let v = extract_json_object(text).unwrap();
        assert_eq!(v["reply"], "hi {there}");
    }

    #[test]
    fn extracts_bare_json() {
        let v = extract_json_object("{\"a\":1}").unwrap();
        assert_eq!(v["a"], 1);
    }
}
