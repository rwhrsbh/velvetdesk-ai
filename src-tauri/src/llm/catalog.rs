//! Model discovery and voice transcription.
//!
//! Both live here because they share one trait: the operator should never have
//! to type a model id or an API version by hand — the app asks the provider.

use serde::Serialize;
use serde_json::{json, Value};

use super::CallError;
use crate::config::{ProviderConfig, ProviderKind};

#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub label: String,
    /// Model can be used for chat/completions.
    pub chat: bool,
    /// Model accepts audio input (voice dictation).
    pub audio: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelCatalog {
    /// API version that answered (Gemini: v1beta or v1).
    pub api_version: String,
    pub models: Vec<ModelInfo>,
}

/// Ask the provider which models this key may use.
pub async fn list_models(
    http: &reqwest::Client,
    provider: &ProviderConfig,
    api_key: &str,
) -> Result<ModelCatalog, CallError> {
    match provider.kind {
        ProviderKind::Gemini => list_gemini(http, provider, api_key).await,
        ProviderKind::OpenaiCompatible => list_openai(http, provider, api_key).await,
    }
}

async fn list_gemini(
    http: &reqwest::Client,
    provider: &ProviderConfig,
    api_key: &str,
) -> Result<ModelCatalog, CallError> {
    let base = provider.base_url.trim_end_matches('/');
    // Prefer the configured version, then fall back to the other one.
    let mut versions = vec![provider.api_version.trim().to_string()];
    for candidate in ["v1beta", "v1"] {
        if !versions.iter().any(|v| v == candidate) {
            versions.push(candidate.to_string());
        }
    }
    versions.retain(|v| !v.is_empty());

    let mut last: Option<CallError> = None;
    for version in versions {
        let url = format!("{base}/{version}/models?pageSize=200");
        let mut req = http.get(&url).header("x-goog-api-key", api_key);
        for (k, v) in &provider.extra_headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let response = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                last = Some(CallError::Transport(e.to_string()));
                continue;
            }
        };
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            last = Some(CallError::Status {
                code: status.as_u16(),
                body: text,
            });
            continue;
        }
        let value: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                last = Some(CallError::Parse(e.to_string()));
                continue;
            }
        };
        let models = parse_gemini_models(&value);
        if !models.is_empty() {
            return Ok(ModelCatalog {
                api_version: version,
                models,
            });
        }
    }
    Err(last.unwrap_or_else(|| CallError::Parse("no models returned".into())))
}

fn parse_gemini_models(value: &Value) -> Vec<ModelInfo> {
    let Some(list) = value.get("models").and_then(|m| m.as_array()) else {
        return vec![];
    };
    let mut out: Vec<ModelInfo> = list
        .iter()
        .filter_map(|entry| {
            let name = entry.get("name").and_then(|n| n.as_str())?;
            let id = name.strip_prefix("models/").unwrap_or(name).to_string();
            let methods: Vec<&str> = entry
                .get("supportedGenerationMethods")
                .and_then(|m| m.as_array())
                .map(|arr| arr.iter().filter_map(|m| m.as_str()).collect())
                .unwrap_or_default();
            let chat = methods.contains(&"generateContent");
            if !chat {
                return None;
            }
            let display = entry
                .get("displayName")
                .and_then(|d| d.as_str())
                .unwrap_or(&id)
                .to_string();
            // Gemini 1.5+ multimodal models accept audio parts.
            let audio = !id.contains("embedding") && !id.contains("aqa");
            Some(ModelInfo {
                label: format!("{display} ({id})"),
                id,
                chat,
                audio,
            })
        })
        .collect();
    out.sort_by(|a, b| rank(&a.id).cmp(&rank(&b.id)).then(a.id.cmp(&b.id)));
    out
}

/// Newest and most capable models first, deprecated ones last.
fn rank(id: &str) -> u8 {
    if id.contains("2.5-pro") {
        0
    } else if id.contains("2.5-flash") {
        1
    } else if id.contains("2.0") {
        2
    } else if id.contains("1.5-pro") {
        3
    } else if id.contains("1.5-flash") {
        4
    } else if id.contains("preview") || id.contains("exp") {
        6
    } else {
        5
    }
}

async fn list_openai(
    http: &reqwest::Client,
    provider: &ProviderConfig,
    api_key: &str,
) -> Result<ModelCatalog, CallError> {
    let base = provider.base_url.trim_end_matches('/');
    let url = format!("{base}/models");
    let mut req = http.get(&url);
    if !api_key.trim().is_empty() && api_key != "local" {
        req = req.header("authorization", format!("Bearer {api_key}"));
    }
    for (k, v) in &provider.extra_headers {
        req = req.header(k.as_str(), v.as_str());
    }

    let response = req
        .send()
        .await
        .map_err(|e| CallError::Transport(e.to_string()))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| CallError::Transport(e.to_string()))?;
    if !status.is_success() {
        return Err(CallError::Status {
            code: status.as_u16(),
            body: text,
        });
    }
    let value: Value =
        serde_json::from_str(&text).map_err(|e| CallError::Parse(format!("{e}: {text}")))?;

    let models = parse_openai_models(&value);
    if models.is_empty() {
        return Err(CallError::Parse(
            "endpoint returned an empty model list".into(),
        ));
    }
    Ok(ModelCatalog {
        api_version: provider.api_version.clone(),
        models,
    })
}

fn parse_openai_models(value: &Value) -> Vec<ModelInfo> {
    let list = value
        .get("data")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out: Vec<ModelInfo> = list
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id").and_then(|i| i.as_str())?.to_string();
            let label = entry
                .get("name")
                .and_then(|n| n.as_str())
                .map(|n| format!("{n} ({id})"))
                .unwrap_or_else(|| id.clone());
            let audio = id.contains("whisper") || id.contains("transcribe") || id.contains("audio");
            Some(ModelInfo {
                id,
                label,
                chat: true,
                audio,
            })
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

// ---------------------------------------------------------------------------
// Voice dictation
// ---------------------------------------------------------------------------

const TRANSCRIBE_PROMPT: &str = "Transcribe this audio verbatim. Return only the \
transcript text, no commentary, no timestamps, no speaker labels. Keep the \
original language of the recording.";

/// Turn a recorded clip into text using the operator's own provider.
pub async fn transcribe(
    http: &reqwest::Client,
    provider: &ProviderConfig,
    api_key: &str,
    audio_base64: &str,
    mime: &str,
) -> Result<String, CallError> {
    match provider.kind {
        ProviderKind::Gemini => {
            transcribe_gemini(http, provider, api_key, audio_base64, mime).await
        }
        ProviderKind::OpenaiCompatible => {
            transcribe_openai(http, provider, api_key, audio_base64, mime).await
        }
    }
}

async fn transcribe_gemini(
    http: &reqwest::Client,
    provider: &ProviderConfig,
    api_key: &str,
    audio_base64: &str,
    mime: &str,
) -> Result<String, CallError> {
    let base = provider.base_url.trim_end_matches('/');
    let version = if provider.api_version.is_empty() {
        "v1beta"
    } else {
        provider.api_version.as_str()
    };
    let url = format!(
        "{base}/{version}/models/{}:generateContent",
        provider.speech_model()
    );
    let body = json!({
        "contents": [{
            "role": "user",
            "parts": [
                { "inline_data": { "mime_type": mime, "data": audio_base64 } },
                { "text": TRANSCRIBE_PROMPT }
            ]
        }],
        "generationConfig": { "temperature": 0.0 }
    });

    let response = http
        .post(&url)
        .header("x-goog-api-key", api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| CallError::Transport(e.to_string()))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| CallError::Transport(e.to_string()))?;
    if !status.is_success() {
        return Err(CallError::Status {
            code: status.as_u16(),
            body: text,
        });
    }
    let value: Value =
        serde_json::from_str(&text).map_err(|e| CallError::Parse(format!("{e}: {text}")))?;
    let parsed = super::gemini::parse_public(&value)?;
    Ok(parsed.text)
}

async fn transcribe_openai(
    http: &reqwest::Client,
    provider: &ProviderConfig,
    api_key: &str,
    audio_base64: &str,
    mime: &str,
) -> Result<String, CallError> {
    use base64::Engine;

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(audio_base64)
        .map_err(|e| CallError::Parse(format!("bad audio payload: {e}")))?;

    let extension = match mime {
        m if m.contains("webm") => "webm",
        m if m.contains("ogg") => "ogg",
        m if m.contains("mp4") || m.contains("m4a") => "m4a",
        m if m.contains("wav") => "wav",
        _ => "webm",
    };

    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name(format!("dictation.{extension}"))
        .mime_str(mime)
        .map_err(|e| CallError::Parse(e.to_string()))?;
    let form = reqwest::multipart::Form::new()
        .text("model", provider.speech_model())
        .part("file", part);

    let base = provider.base_url.trim_end_matches('/');
    let url = format!("{base}/audio/transcriptions");
    let mut req = http.post(&url);
    if !api_key.trim().is_empty() && api_key != "local" {
        req = req.header("authorization", format!("Bearer {api_key}"));
    }
    for (k, v) in &provider.extra_headers {
        req = req.header(k.as_str(), v.as_str());
    }

    let response = req
        .multipart(form)
        .send()
        .await
        .map_err(|e| CallError::Transport(e.to_string()))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| CallError::Transport(e.to_string()))?;
    if !status.is_success() {
        return Err(CallError::Status {
            code: status.as_u16(),
            body: text,
        });
    }
    let value: Value =
        serde_json::from_str(&text).map_err(|e| CallError::Parse(format!("{e}: {text}")))?;
    Ok(value
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .trim()
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_chat_models_and_ranks_them() {
        let raw = json!({
            "models": [
                { "name": "models/embedding-001", "supportedGenerationMethods": ["embedContent"] },
                { "name": "models/gemini-1.5-flash", "displayName": "Gemini 1.5 Flash",
                  "supportedGenerationMethods": ["generateContent"] },
                { "name": "models/gemini-2.5-pro", "displayName": "Gemini 2.5 Pro",
                  "supportedGenerationMethods": ["generateContent", "countTokens"] }
            ]
        });
        let models = parse_gemini_models(&raw);
        assert_eq!(models.len(), 2, "embedding models must be dropped");
        assert_eq!(models[0].id, "gemini-2.5-pro", "newest model comes first");
        assert!(models[0].label.starts_with("Gemini 2.5 Pro"));
        assert!(models[0].audio);
    }

    #[test]
    fn reads_openai_model_list() {
        let raw = json!({ "data": [{ "id": "gpt-4o-mini" }, { "id": "whisper-1" }] });
        let models = parse_openai_models(&raw);
        assert_eq!(models.len(), 2);
        assert!(models.iter().any(|m| m.id == "whisper-1" && m.audio));
    }
}
