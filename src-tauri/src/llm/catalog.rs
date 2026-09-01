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
            Some(ModelInfo {
                label: format!("{display} ({id})"),
                audio: accepts_audio(&id),
                id,
                chat,
            })
        })
        .collect();
    out.sort_by(|a, b| rank(&a.id).cmp(&rank(&b.id)).then(a.id.cmp(&b.id)));
    out
}

/// Model families that cannot take audio *in*, whatever they emit.
///
/// The provider lists image generators, TTS voices and embedders next to the
/// chat models, so the dictation picker has to weed them out explicitly.
const NOT_AUDIO_INPUT: &[&str] = &[
    "embedding",
    "embed",
    "aqa",
    "image",
    "banana",
    "imagen",
    "veo",
    "tts",
    "vision-only",
];

pub fn accepts_audio(id: &str) -> bool {
    let id = id.to_lowercase();
    if NOT_AUDIO_INPUT.iter().any(|bad| id.contains(bad)) {
        return false;
    }
    // Live models stream over WebSockets only, so a recorded clip cannot be
    // sent to them — this check must come before the transcribe one, because
    // `gemini-3.5-transcribe-live` matches both.
    if id.contains("live") {
        return false;
    }
    // Purpose-built speech-to-text models.
    if id.contains("transcribe") {
        return true;
    }
    // Everything else in the Gemini multimodal line accepts audio parts;
    // 1.0-era models did not.
    id.starts_with("gemini-") && !id.starts_with("gemini-1.0") && !id.starts_with("gemini-pro-")
}

/// True when the model must go through the Interactions API instead of
/// `generateContent` (the dedicated speech-to-text line).
pub fn is_transcribe_model(id: &str) -> bool {
    let id = id.to_lowercase();
    id.contains("transcribe") && !id.contains("live")
}

/// Newest and most capable models first, deprecated ones last.
fn rank(id: &str) -> u8 {
    if id.contains("transcribe") {
        // Speech-only models are useless for chat: keep them at the bottom.
        7
    } else if id.contains("2.5-pro") {
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
    // The dedicated speech models (gemini-*-transcribe) are not served by
    // generateContent: the clip has to be uploaded through the Files API and
    // then referenced from an Interactions request.
    if is_transcribe_model(&provider.speech_model()) {
        let uri = upload_audio(http, provider, api_key, audio_base64, mime).await?;
        return interactions_transcribe(http, provider, api_key, &uri, mime).await;
    }
    transcribe_gemini_inline(http, provider, api_key, audio_base64, mime).await
}

/// Multimodal chat models take the clip inline, in one request.
async fn transcribe_gemini_inline(
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

/// Resumable Files API upload; returns the `files/…` URI of the stored clip.
async fn upload_audio(
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
    let base = provider.base_url.trim_end_matches('/');
    let version = if provider.api_version.is_empty() {
        "v1beta"
    } else {
        provider.api_version.as_str()
    };

    // Step 1: announce the upload and collect the session URL.
    let start = http
        .post(format!("{base}/upload/{version}/files"))
        .header("x-goog-api-key", api_key)
        .header("X-Goog-Upload-Protocol", "resumable")
        .header("X-Goog-Upload-Command", "start")
        .header(
            "X-Goog-Upload-Header-Content-Length",
            bytes.len().to_string(),
        )
        .header("X-Goog-Upload-Header-Content-Type", mime)
        .json(&json!({ "file": { "display_name": "velvetdesk-dictation" } }))
        .send()
        .await
        .map_err(|e| CallError::Transport(e.to_string()))?;

    let status = start.status();
    let upload_url = start
        .headers()
        .get("x-goog-upload-url")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    if !status.is_success() {
        let body = start.text().await.unwrap_or_default();
        return Err(CallError::Status {
            code: status.as_u16(),
            body,
        });
    }
    let upload_url =
        upload_url.ok_or_else(|| CallError::Parse("files API returned no upload url".into()))?;

    // Step 2: send the bytes and finalise in one command.
    let finish = http
        .post(upload_url)
        .header("Content-Length", bytes.len().to_string())
        .header("X-Goog-Upload-Offset", "0")
        .header("X-Goog-Upload-Command", "upload, finalize")
        .body(bytes)
        .send()
        .await
        .map_err(|e| CallError::Transport(e.to_string()))?;

    let status = finish.status();
    let text = finish
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
    value
        .get("file")
        .and_then(|f| f.get("uri"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| CallError::Parse(format!("no file uri in upload response: {text}")))
}

/// Interactions API call for the dedicated speech-to-text models.
async fn interactions_transcribe(
    http: &reqwest::Client,
    provider: &ProviderConfig,
    api_key: &str,
    file_uri: &str,
    mime: &str,
) -> Result<String, CallError> {
    let base = provider.base_url.trim_end_matches('/');
    let version = if provider.api_version.is_empty() {
        "v1beta"
    } else {
        provider.api_version.as_str()
    };
    let body = json!({
        "model": provider.speech_model(),
        "input": [{ "type": "audio", "uri": file_uri, "mime_type": mime }],
        "generation_config": {
            "transcription_config": {
                "mode": { "type": "verbatim" }
            }
        }
    });

    let response = http
        .post(format!("{base}/{version}/interactions"))
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
    let transcript = extract_transcript(&value);
    if transcript.is_empty() {
        return Err(CallError::Parse(format!("empty transcript: {text}")));
    }
    Ok(transcript)
}

/// Collect transcript text from a response without hard-coding one shape:
/// the Interactions payload nests it differently from `generateContent`.
pub fn extract_transcript(value: &Value) -> String {
    let mut out = String::new();
    walk_text(value, &mut out);
    out.trim().to_string()
}

fn walk_text(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if matches!(key.as_str(), "text" | "transcript") {
                    if let Some(s) = child.as_str() {
                        if !s.trim().is_empty() {
                            if !out.is_empty() {
                                out.push(' ');
                            }
                            out.push_str(s.trim());
                        }
                        continue;
                    }
                }
                // Timestamps and word-level detail would duplicate the text.
                if matches!(key.as_str(), "words" | "usage" | "usageMetadata") {
                    continue;
                }
                walk_text(child, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| walk_text(item, out)),
        _ => {}
    }
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

    #[test]
    fn dictation_picker_rejects_non_speech_models() {
        for id in [
            "gemini-2.5-pro-preview-tts",
            "gemini-2.5-flash-image",
            "gemini-3.1-flash-image",
            "nano-banana-2",
            "text-embedding-004",
            "imagen-3.0-generate",
            "veo-3.0",
            "aqa",
            "gemini-3.5-transcribe-live",
            "gemini-3.1-flash-live-preview",
        ] {
            assert!(!accepts_audio(id), "{id} must not be offered for dictation");
        }
    }

    #[test]
    fn dictation_picker_keeps_audio_models() {
        for id in [
            "gemini-3.5-transcribe",
            "gemini-2.5-pro",
            "gemini-2.5-flash",
            "gemini-3.5-flash",
            "gemini-1.5-pro",
        ] {
            assert!(accepts_audio(id), "{id} should accept audio input");
        }
    }

    #[test]
    fn only_dedicated_models_use_the_interactions_api() {
        assert!(is_transcribe_model("gemini-3.5-transcribe"));
        assert!(!is_transcribe_model("gemini-3.5-transcribe-live"));
        assert!(!is_transcribe_model("gemini-2.5-flash"));
    }

    #[test]
    fn transcribe_models_rank_last_for_chat() {
        let raw = json!({
            "models": [
                { "name": "models/gemini-3.5-transcribe", "displayName": "Gemini 3.5 Transcribe",
                  "supportedGenerationMethods": ["generateContent"] },
                { "name": "models/gemini-2.5-pro", "displayName": "Gemini 2.5 Pro",
                  "supportedGenerationMethods": ["generateContent"] }
            ]
        });
        let models = parse_gemini_models(&raw);
        assert_eq!(models[0].id, "gemini-2.5-pro");
        assert_eq!(models[1].id, "gemini-3.5-transcribe");
        assert!(models[1].audio);
    }

    #[test]
    fn reads_transcript_from_interactions_shape() {
        let raw = json!({
            "output": [{
                "type": "transcription",
                "content": [{ "type": "text", "text": "привет, это тест" }],
                "words": [{ "text": "привет", "start": 0.1 }]
            }],
            "usageMetadata": { "text": "ignore me" }
        });
        assert_eq!(extract_transcript(&raw), "привет, это тест");
    }

    #[test]
    fn reads_transcript_from_generate_content_shape() {
        let raw = json!({
            "candidates": [{ "content": { "parts": [{ "text": "hello there" }] } }]
        });
        assert_eq!(extract_transcript(&raw), "hello there");
    }
}
