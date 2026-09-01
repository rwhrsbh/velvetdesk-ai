//! Google Generative Language API (Gemini) provider: v1 and v1beta,
//! system instructions and native function calling.

use serde_json::{json, Value};

use super::{CallError, ChatRequest, ChatResponse, Role, ToolCall, Usage};
use crate::config::ProviderConfig;

pub async fn call(
    http: &reqwest::Client,
    provider: &ProviderConfig,
    api_key: &str,
    request: &ChatRequest,
) -> Result<ChatResponse, CallError> {
    let version = if provider.api_version.is_empty() {
        "v1beta"
    } else {
        provider.api_version.as_str()
    };
    let base = provider.base_url.trim_end_matches('/');
    let url = format!(
        "{base}/{version}/models/{}:generateContent",
        provider.model.trim()
    );

    let body = build_body(provider, request);

    let mut req = http
        .post(&url)
        .header("x-goog-api-key", api_key)
        .header("content-type", "application/json");
    for (k, v) in &provider.extra_headers {
        req = req.header(k.as_str(), v.as_str());
    }

    let response = req
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
    parse_response(&value)
}

fn build_body(provider: &ProviderConfig, request: &ChatRequest) -> Value {
    let mut contents: Vec<Value> = vec![];

    for msg in &request.messages {
        match msg.role {
            Role::User => contents.push(json!({
                "role": "user",
                "parts": [{ "text": msg.content }],
            })),
            Role::Assistant => {
                let mut parts: Vec<Value> = vec![];
                if !msg.content.trim().is_empty() {
                    parts.push(json!({ "text": msg.content }));
                }
                for call in &msg.tool_calls {
                    parts.push(json!({
                        "functionCall": { "name": call.name, "args": call.args }
                    }));
                }
                if parts.is_empty() {
                    parts.push(json!({ "text": "" }));
                }
                contents.push(json!({ "role": "model", "parts": parts }));
            }
            Role::Tool => {
                let name = msg.tool_name.clone().unwrap_or_else(|| "tool".into());
                let payload: Value = serde_json::from_str(&msg.content)
                    .unwrap_or_else(|_| json!({ "result": msg.content }));
                contents.push(json!({
                    "role": "user",
                    "parts": [{
                        "functionResponse": {
                            "name": name,
                            "response": { "result": payload }
                        }
                    }],
                }));
            }
        }
    }

    let mut generation_config = json!({
        "temperature": request.temperature,
    });
    if let Some(max) = request.max_output_tokens.or(provider.max_output_tokens) {
        generation_config["maxOutputTokens"] = json!(max);
    }
    if request.force_json {
        generation_config["responseMimeType"] = json!("application/json");
    }

    let mut body = json!({
        "contents": contents,
        "generationConfig": generation_config,
        "safetySettings": [
            { "category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_ONLY_HIGH" },
            { "category": "HARM_CATEGORY_HATE_SPEECH", "threshold": "BLOCK_ONLY_HIGH" },
            { "category": "HARM_CATEGORY_SEXUALLY_EXPLICIT", "threshold": "BLOCK_ONLY_HIGH" },
            { "category": "HARM_CATEGORY_DANGEROUS_CONTENT", "threshold": "BLOCK_ONLY_HIGH" }
        ]
    });

    if !request.system.trim().is_empty() {
        body["systemInstruction"] = json!({ "parts": [{ "text": request.system }] });
    }

    if !request.tools.is_empty() {
        let declarations: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": sanitize_schema(&t.parameters),
                })
            })
            .collect();
        body["tools"] = json!([{ "functionDeclarations": declarations }]);
    }

    body
}

/// Gemini rejects a few JSON-schema keywords that OpenAI tolerates.
fn sanitize_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                if matches!(
                    k.as_str(),
                    "additionalProperties" | "$schema" | "examples" | "default" | "title"
                ) {
                    continue;
                }
                out.insert(k.clone(), sanitize_schema(v));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sanitize_schema).collect()),
        other => other.clone(),
    }
}

fn parse_response(value: &Value) -> Result<ChatResponse, CallError> {
    if let Some(err) = value.get("error") {
        return Err(CallError::Parse(err.to_string()));
    }

    let candidate = value
        .get("candidates")
        .and_then(|c| c.get(0))
        .ok_or_else(|| {
            let reason = value
                .get("promptFeedback")
                .map(|f| f.to_string())
                .unwrap_or_else(|| "no candidates returned".into());
            CallError::Parse(reason)
        })?;

    let mut text = String::new();
    let mut tool_calls = vec![];

    if let Some(parts) = candidate
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
    {
        for part in parts {
            if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                text.push_str(t);
            }
            if let Some(fc) = part.get("functionCall") {
                let name = fc
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or_default()
                    .to_string();
                let args = fc
                    .get("args")
                    .cloned()
                    .unwrap_or(Value::Object(Default::default()));
                tool_calls.push(ToolCall {
                    id: format!("gem-{}-{}", name, tool_calls.len()),
                    name,
                    args,
                });
            }
        }
    }

    let usage_meta = value.get("usageMetadata");
    let usage = Usage {
        prompt_tokens: usage_meta
            .and_then(|u| u.get("promptTokenCount"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        completion_tokens: usage_meta
            .and_then(|u| u.get("candidatesTokenCount"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        total_tokens: usage_meta
            .and_then(|u| u.get("totalTokenCount"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
    };

    Ok(ChatResponse {
        text: text.trim().to_string(),
        tool_calls,
        usage,
        finish_reason: candidate
            .get("finishReason")
            .and_then(|f| f.as_str())
            .unwrap_or_default()
            .to_string(),
        key_index: 0,
        attempts: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderKind;
    use crate::llm::{LlmMessage, ToolDef};

    fn provider() -> ProviderConfig {
        ProviderConfig {
            id: "gemini".into(),
            label: "Gemini".into(),
            kind: ProviderKind::Gemini,
            base_url: "https://generativelanguage.googleapis.com".into(),
            api_version: "v1beta".into(),
            model: "gemini-2.5-pro".into(),
            extra_headers: vec![],
            temperature: 0.8,
            max_output_tokens: None,
            key_count: 1,
        }
    }

    #[test]
    fn builds_system_and_tools() {
        let mut req = ChatRequest::new("be warm");
        req.messages.push(LlmMessage::user("hi"));
        req.tools.push(ToolDef {
            name: "get_man".into(),
            description: "read dossier".into(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": { "man_id": { "type": "string" } },
                "required": ["man_id"]
            }),
        });
        let body = build_body(&provider(), &req);
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be warm");
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["name"],
            "get_man"
        );
        assert!(body["tools"][0]["functionDeclarations"][0]["parameters"]
            .get("additionalProperties")
            .is_none());
    }

    #[test]
    fn parses_text_and_function_call() {
        let raw = json!({
            "candidates": [{
                "content": { "parts": [
                    { "text": "ok" },
                    { "functionCall": { "name": "get_man", "args": { "man_id": "42" } } }
                ]},
                "finishReason": "STOP"
            }],
            "usageMetadata": { "promptTokenCount": 10, "candidatesTokenCount": 5, "totalTokenCount": 15 }
        });
        let parsed = parse_response(&raw).unwrap();
        assert_eq!(parsed.text, "ok");
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].args["man_id"], "42");
        assert_eq!(parsed.usage.total_tokens, 15);
    }
}
