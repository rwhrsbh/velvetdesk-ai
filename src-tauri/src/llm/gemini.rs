//! Google Generative Language API (Gemini) provider: v1 and v1beta,
//! system instructions and native function calling.

use serde_json::{json, Value};

use super::{CallError, ChatRequest, ChatResponse, Role, Thinking, ToolCall, Usage};
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

    let mut body = build_body(provider, request);

    let (mut status, mut text) = post(http, &url, api_key, provider, &body).await?;

    // Model names do not tell the whole truth about which fields an endpoint
    // accepts. When one is rejected by name, a thinking control is retried in
    // its other spelling and anything else is dropped — twice at most, which
    // covers "no thought summaries here" followed by "no levels either".
    for _ in 0..2 {
        if status.as_u16() != 400 {
            break;
        }
        let Some(field) = rejected_field(&text) else {
            break;
        };
        let recovered = if is_thinking_field(&field) {
            flip_thinking(&mut body, provider, &request.thinking)
        } else {
            drop_field(&mut body, &field)
        };
        if !recovered {
            break;
        }
        let retry = post(http, &url, api_key, provider, &body).await?;
        status = retry.0;
        text = retry.1;
    }

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

/// Exactly how many tokens a request is, counted by the model's own tokenizer.
///
/// A local estimate can only ever approximate this — a character count is off
/// by half on Cyrillic alone — and `countTokens` neither generates anything nor
/// spends the generation quota.
pub async fn count_tokens(
    http: &reqwest::Client,
    provider: &ProviderConfig,
    api_key: &str,
    request: &ChatRequest,
) -> Result<u32, CallError> {
    let version = if provider.api_version.is_empty() {
        "v1beta"
    } else {
        provider.api_version.as_str()
    };
    let base = provider.base_url.trim_end_matches('/');
    let url = format!(
        "{base}/{version}/models/{}:countTokens",
        provider.model.trim()
    );

    // The endpoint takes the request itself, minus the settings that do not
    // affect its size — and it insists on the model name and at least one
    // message, even when what is being measured is the system prompt.
    let mut body = build_body(provider, request);
    if let Some(map) = body.as_object_mut() {
        map.remove("generationConfig");
        map.remove("safetySettings");
        map.insert(
            "model".into(),
            json!(format!("models/{}", provider.model.trim())),
        );
    }
    // `is_none_or` would read better, but the project builds on 1.77.
    if body["contents"]
        .as_array()
        .map(|c| c.is_empty())
        .unwrap_or(true)
    {
        body["contents"] = json!([{ "role": "user", "parts": [{ "text": "" }] }]);
    }
    let payload = json!({ "generateContentRequest": body });

    let (status, text) = post(http, &url, api_key, provider, &payload).await?;
    if !status.is_success() {
        return Err(CallError::Status {
            code: status.as_u16(),
            body: text,
        });
    }
    let value: Value =
        serde_json::from_str(&text).map_err(|e| CallError::Parse(format!("{e}: {text}")))?;
    value["totalTokens"]
        .as_u64()
        .map(|n| n as u32)
        .ok_or_else(|| CallError::Parse(format!("no totalTokens in {text}")))
}

async fn post(
    http: &reqwest::Client,
    url: &str,
    api_key: &str,
    provider: &ProviderConfig,
    body: &Value,
) -> Result<(reqwest::StatusCode, String), CallError> {
    let mut req = http
        .post(url)
        .header("x-goog-api-key", api_key)
        .header("content-type", "application/json");
    for (k, v) in &provider.extra_headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let response = req
        .json(body)
        .send()
        .await
        .map_err(|e| CallError::Transport(e.to_string()))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| CallError::Transport(e.to_string()))?;
    Ok((status, text))
}

/// Stream a reply token by token.
///
/// Everything the non-streaming path does still applies — the body is the same
/// and the thinking-field retry is the same — but the answer is emitted as it
/// arrives, so the operator watches it being written instead of a spinner.
pub async fn call_streaming(
    http: &reqwest::Client,
    provider: &ProviderConfig,
    api_key: &str,
    request: &ChatRequest,
    on_event: &(dyn Fn(Value) + Send + Sync),
) -> Result<ChatResponse, CallError> {
    use futures_util::StreamExt;

    let version = if provider.api_version.is_empty() {
        "v1beta"
    } else {
        provider.api_version.as_str()
    };
    let base = provider.base_url.trim_end_matches('/');
    let url = format!(
        "{base}/{version}/models/{}:streamGenerateContent?alt=sse",
        provider.model.trim()
    );

    let mut body = build_body(provider, request);
    // Ask for the model's own summary of its reasoning when it is thinking.
    if !request.thinking.is_default() {
        request_thoughts(&mut body, provider);
    }

    let mut response = send(http, &url, api_key, provider, &body).await?;

    // Same recovery as the plain call. Thought summaries in particular are not
    // offered by every model, and losing the summary beats losing the answer.
    for _ in 0..2 {
        if response.status().as_u16() != 400 {
            break;
        }
        let text = response
            .text()
            .await
            .map_err(|e| CallError::Transport(e.to_string()))?;
        let recovered = match rejected_field(&text) {
            Some(field) if is_thinking_field(&field) => {
                flip_thinking(&mut body, provider, &request.thinking)
            }
            Some(field) => drop_field(&mut body, &field),
            None => false,
        };
        if !recovered {
            return Err(CallError::Status {
                code: 400,
                body: text,
            });
        }
        response = send(http, &url, api_key, provider, &body).await?;
    }

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(CallError::Status {
            code: status.as_u16(),
            body,
        });
    }

    let mut text = String::new();
    let mut thoughts = String::new();
    let mut tool_calls: Vec<ToolCall> = vec![];
    let mut usage = Usage::default();
    let mut finish_reason = String::new();
    let mut buffer = String::new();
    // The events as they arrived, so an empty or surprising answer can be read
    // rather than guessed at.
    let mut seen = String::new();
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| CallError::Transport(e.to_string()))?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        for value in take_events(&mut buffer) {
            if seen.len() < super::RAW_LIMIT {
                seen.push_str(&value.to_string());
                seen.push('\n');
            }
            if let Some(reason) = value["candidates"][0]["finishReason"].as_str() {
                finish_reason = reason.to_string();
            }
            if let Some(meta) = value.get("usageMetadata") {
                usage = read_usage(Some(meta));
            }
            let Some(parts) = value["candidates"][0]["content"]["parts"].as_array() else {
                continue;
            };
            for part in parts {
                if let Some(piece) = part.get("text").and_then(|t| t.as_str()) {
                    // A part marked `thought` is the model reasoning aloud, not
                    // the answer: it belongs behind the spoiler, not in the text.
                    if part.get("thought").and_then(|t| t.as_bool()) == Some(true) {
                        thoughts.push_str(piece);
                        on_event(json!({ "kind": "thought", "text": piece }));
                    } else {
                        text.push_str(piece);
                        on_event(json!({ "kind": "delta", "text": piece }));
                    }
                }
                if let Some(call) = part.get("functionCall") {
                    tool_calls.push(ToolCall {
                        id: format!("gem-{}", tool_calls.len()),
                        name: call["name"].as_str().unwrap_or_default().to_string(),
                        args: call
                            .get("args")
                            .cloned()
                            .unwrap_or(Value::Object(Default::default())),
                        signature: part
                            .get("thoughtSignature")
                            .and_then(|s| s.as_str())
                            .unwrap_or_default()
                            .to_string(),
                    });
                }
            }
        }
    }

    Ok(ChatResponse {
        text: text.trim().to_string(),
        raw: super::cap_raw(&seen),
        // The caller fills this in: it knows which model of the chain this was.
        model: String::new(),
        thoughts: thoughts.trim().to_string(),
        tool_calls,
        usage,
        finish_reason,
        key_index: 0,
        attempts: 0,
    })
}

/// Take the complete server-sent events out of a buffer.
///
/// A chunk off the wire can end in the middle of an event, so whatever is left
/// unterminated stays in the buffer for the next one.
pub(crate) fn take_events(buffer: &mut String) -> Vec<Value> {
    let mut events = vec![];
    // Servers differ on line endings and Gemini sends CRLF: looking only
    // for a bare blank line found no separator at all, and the answer came
    // back empty with nothing to explain it.
    if buffer.contains('\r') {
        buffer.retain(|c| c != '\r');
    }
    while let Some(cut) = buffer.find("\n\n") {
        let event = buffer[..cut].to_string();
        buffer.drain(..cut + 2);
        let Some(payload) = event
            .lines()
            .find_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
        else {
            continue;
        };
        if payload == "[DONE]" {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(payload) {
            events.push(value);
        }
    }
    events
}

async fn send(
    http: &reqwest::Client,
    url: &str,
    api_key: &str,
    provider: &ProviderConfig,
    body: &Value,
) -> Result<reqwest::Response, CallError> {
    let mut req = http
        .post(url)
        .header("x-goog-api-key", api_key)
        .header("content-type", "application/json");
    for (k, v) in &provider.extra_headers {
        req = req.header(k.as_str(), v.as_str());
    }
    req.json(body)
        .send()
        .await
        .map_err(|e| CallError::Transport(e.to_string()))
}

/// Ask for thought summaries in whichever way this generation spells it.
fn request_thoughts(body: &mut Value, _provider: &ProviderConfig) {
    let config = &mut body["generationConfig"];
    if let Some(thinking) = config.get_mut("thinkingConfig") {
        thinking["includeThoughts"] = json!(true);
    } else if config.get("thinkingLevel").is_some() {
        config["thinkingSummaries"] = json!("auto");
    }
}

fn build_body(provider: &ProviderConfig, request: &ChatRequest) -> Value {
    let mut contents: Vec<Value> = vec![];

    for msg in &request.messages {
        match msg.role {
            Role::User => {
                let mut parts: Vec<Value> = vec![json!({ "text": msg.content })];
                for image in &msg.images {
                    parts.push(json!({
                        "inlineData": { "mimeType": image.mime, "data": image.data }
                    }));
                }
                contents.push(json!({ "role": "user", "parts": parts }));
            }
            Role::Assistant => {
                let mut parts: Vec<Value> = vec![];
                if !msg.content.trim().is_empty() {
                    parts.push(json!({ "text": msg.content }));
                }
                for call in &msg.tool_calls {
                    let mut part = json!({
                        "functionCall": { "name": call.name, "args": call.args }
                    });
                    // Without the signature Gemini 3 rejects the whole request:
                    // "Function call is missing a thought_signature in
                    // functionCall parts" (HTTP 400).
                    if !call.signature.is_empty() {
                        part["thoughtSignature"] = json!(call.signature);
                    }
                    parts.push(part);
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
    apply_thinking(&mut generation_config, provider, &request.thinking);

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

/// Gemini spells the reasoning control two ways, and sending the one an
/// endpoint does not know is a hard 400 ("Unknown name \"thinkingLevel\""):
///
/// * `generationConfig.thinkingConfig.thinkingBudget` — a number of tokens.
/// * `generationConfig.thinkingLevel` — a named level.
///
/// The budget is what actually works: probing this API found `thinkingLevel`
/// rejected by every model it serves, 3.x included, while a budget was accepted
/// everywhere. So the budget goes out by default and [`call`] retries with the
/// level if some endpoint ever asks for it.
fn apply_thinking(config: &mut Value, provider: &ProviderConfig, thinking: &Thinking) {
    if thinking.is_default() {
        return;
    }
    set_thinking_budget(config, provider, thinking);
}

/// Major version of a Gemini model name, when it has one.
fn generation(model: &str) -> Option<u32> {
    model
        .to_lowercase()
        .strip_prefix("gemini-")?
        .split(['.', '-'])
        .next()?
        .parse::<u32>()
        .ok()
}

/// Budgets a model actually accepts, which the documentation does not say and
/// the API answers with a 400. Measured, per family:
///
/// * `2.5-pro` — 128 … 32768, and it cannot stop thinking.
/// * `2.5-flash-lite` — 0, or 512 … 24576; 128 is refused.
/// * `3.x` lite models — no 0 either; the smallest accepted is 128.
/// * everything else — 0 … 24576.
///
/// `-1` is Gemini's "decide for yourself" and is passed through untouched.
fn clamp_budget(model: &str, budget: i32) -> i32 {
    if budget < 0 {
        return budget;
    }
    let name = model.to_lowercase();
    if name.contains("2.5-pro") {
        return budget.clamp(128, 32768);
    }
    if name.contains("lite") {
        return match generation(&name) {
            Some(major) if major >= 3 => budget.max(128),
            _ if budget == 0 => 0,
            _ => budget.clamp(512, 24576),
        };
    }
    budget.clamp(0, 24576)
}

fn set_thinking_level(config: &mut Value, thinking: &Thinking) {
    let level = match (thinking.level(), thinking.budget_tokens) {
        (Some(level), _) => level.to_string(),
        // A budget carried over from another provider still has to say
        // something here; map it onto the nearest level.
        (None, Some(budget)) => budget_to_level(budget).to_string(),
        (None, None) => return,
    };
    let level = match level.as_str() {
        // The API publishes minimal / low / medium / high only.
        "xhigh" | "max" => "high",
        "off" => "minimal",
        "none" => "minimal",
        other => other,
    };
    config["thinkingLevel"] = json!(level);
}

fn set_thinking_budget(config: &mut Value, provider: &ProviderConfig, thinking: &Thinking) {
    let budget = match (thinking.budget_tokens, thinking.level()) {
        (Some(budget), _) => budget,
        (None, Some(level)) => level_to_budget(level),
        (None, None) => return,
    };
    config["thinkingConfig"] = json!({ "thinkingBudget": clamp_budget(&provider.model, budget) });
}

/// Levels as token budgets, within the 0–24576 range Flash accepts.
fn level_to_budget(level: &str) -> i32 {
    match level {
        "none" | "off" => 0,
        "minimal" => 512,
        "low" => 2048,
        "medium" => 8192,
        _ => 24576,
    }
}

fn budget_to_level(budget: i32) -> &'static str {
    match budget {
        0 => "minimal",
        // -1 asks the model to decide, which is what "medium" means here.
        b if b < 0 => "medium",
        b if b <= 1024 => "minimal",
        b if b <= 4096 => "low",
        b if b <= 12288 => "medium",
        _ => "high",
    }
}

/// The field an "Unknown name" rejection is complaining about.
///
/// Gemini's generation config differs between models and API versions, and
/// being told is the only way to learn which fields a given endpoint knows.
/// The error names one, which is enough to drop it and try again.
fn rejected_field(body: &str) -> Option<String> {
    if !body.to_lowercase().contains("unknown name") {
        return None;
    }
    let after_label = &body[body.find("Unknown name")? + "Unknown name".len()..];
    let quoted = after_label.trim_start().trim_start_matches('\\');
    let mut chars = quoted.char_indices();
    let (_, quote) = chars.next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let rest = &quoted[quote.len_utf8()..];
    let end = rest.find(quote)?;
    let name = rest[..end].trim().trim_end_matches('\\').to_string();
    (!name.is_empty()).then_some(name)
}

/// True when the named field is a thinking control, which has a second
/// spelling worth trying before giving up on it.
fn is_thinking_field(name: &str) -> bool {
    matches!(
        name.to_lowercase().replace('_', "").as_str(),
        "thinkinglevel" | "thinkingconfig"
    )
}

/// Remove a field the endpoint does not know, wherever it sits in the
/// generation config. False when there was nothing to remove.
fn drop_field(body: &mut Value, name: &str) -> bool {
    fn remove(value: &mut Value, name: &str) -> bool {
        let Some(map) = value.as_object_mut() else {
            return false;
        };
        if map.remove(name).is_some() {
            return true;
        }
        map.values_mut().any(|nested| remove(nested, name))
    }
    remove(&mut body["generationConfig"], name)
}

/// Swap `thinkingLevel` for `thinkingConfig` or the other way round.
fn flip_thinking(body: &mut Value, provider: &ProviderConfig, thinking: &Thinking) -> bool {
    let config = &mut body["generationConfig"];
    if config.get("thinkingLevel").is_some() {
        config.as_object_mut().map(|c| c.remove("thinkingLevel"));
        set_thinking_budget(config, provider, thinking);
        true
    } else if config.get("thinkingConfig").is_some() {
        config.as_object_mut().map(|c| c.remove("thinkingConfig"));
        set_thinking_level(config, thinking);
        true
    } else {
        false
    }
}

/// Gemini rejects a few JSON-schema keywords that OpenAI tolerates.
///
/// Only *keywords* are dropped. Inside `properties` the keys are property
/// names chosen by us, so a tool taking an argument called `title` or
/// `default` must keep it — dropping one there leaves `required` pointing at a
/// property that no longer exists, which Gemini rejects with HTTP 400.
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
                let value = match (k.as_str(), v) {
                    ("properties", Value::Object(props)) => Value::Object(
                        props
                            .iter()
                            .map(|(name, sub)| (name.clone(), sanitize_schema(sub)))
                            .collect(),
                    ),
                    _ => sanitize_schema(v),
                };
                out.insert(k.clone(), value);
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sanitize_schema).collect()),
        other => other.clone(),
    }
}

fn read_usage(meta: Option<&Value>) -> Usage {
    let field = |name: &str| {
        meta.and_then(|m| m.get(name))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32
    };
    Usage {
        prompt_tokens: field("promptTokenCount"),
        completion_tokens: field("candidatesTokenCount"),
        total_tokens: field("totalTokenCount"),
    }
}

/// Parse a raw generateContent payload (used by the transcription path too).
pub fn parse_public(value: &Value) -> Result<ChatResponse, CallError> {
    parse_response(value)
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
    let mut thoughts = String::new();
    let mut tool_calls = vec![];

    if let Some(parts) = candidate
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
    {
        for part in parts {
            if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                if part.get("thought").and_then(|v| v.as_bool()) == Some(true) {
                    thoughts.push_str(t);
                } else {
                    text.push_str(t);
                }
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
                let signature = part
                    .get("thoughtSignature")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .to_string();
                tool_calls.push(ToolCall {
                    id: format!("gem-{}-{}", name, tool_calls.len()),
                    name,
                    args,
                    signature,
                });
            }
        }
    }

    let usage = read_usage(value.get("usageMetadata"));

    Ok(ChatResponse {
        text: text.trim().to_string(),
        // Filled in by the caller, which has the payload and the model name.
        raw: String::new(),
        model: String::new(),
        thoughts: thoughts.trim().to_string(),
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
            transcribe_model: String::new(),
            thinking_effort: String::new(),
            thinking_budget: None,
            model_chain: vec![],
            reasoning_dialect: "auto".into(),
            context_tokens: None,
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

    /// A property may legitimately be called `title` or `default`. Stripping it
    /// as if it were a schema keyword used to leave `required` dangling, which
    /// Gemini answered with
    /// `parameters.required[1]: property is not defined` (HTTP 400).
    #[test]
    fn keeps_properties_named_like_schema_keywords() {
        let mut req = ChatRequest::new("");
        req.tools.push(ToolDef {
            name: "add_gift".into(),
            description: "record a gift".into(),
            parameters: json!({
                "type": "object",
                "title": "dropped, this one is a keyword",
                "properties": {
                    "man_id": { "type": "string" },
                    "title": { "type": "string", "description": "what he sent" },
                    "default": { "type": "string" }
                },
                "required": ["man_id", "title"]
            }),
        });
        let params = build_body(&provider(), &req)["tools"][0]["functionDeclarations"][0]
            ["parameters"]
            .clone();
        assert!(params.get("title").is_none(), "keyword must be stripped");
        assert!(params["properties"]["title"].is_object());
        assert!(params["properties"]["default"].is_object());
    }

    /// Every declared tool must survive sanitising with `required` still fully
    /// covered by `properties` — otherwise the whole request is rejected.
    #[test]
    fn every_tool_stays_consistent_after_sanitising() {
        let mut req = ChatRequest::new("");
        req.tools = crate::agent::tools::tool_defs();
        let body = build_body(&provider(), &req);
        let declarations = body["tools"][0]["functionDeclarations"].as_array().unwrap();
        assert_eq!(declarations.len(), req.tools.len());
        for declaration in declarations {
            let params = &declaration["parameters"];
            let properties = params["properties"].as_object().unwrap();
            for name in params["required"].as_array().unwrap_or(&vec![]) {
                let name = name.as_str().unwrap();
                assert!(
                    properties.contains_key(name),
                    "{}: required property `{name}` is not defined",
                    declaration["name"]
                );
            }
        }
    }

    /// Gemini 3 signs each function call and rejects the follow-up turn unless
    /// the signature is echoed back: "Function call is missing a
    /// thought_signature in functionCall parts" (HTTP 400). Parse it, then put
    /// it back on the wire.
    #[test]
    fn thought_signatures_survive_the_round_trip() {
        let raw = json!({
            "candidates": [{
                "content": { "parts": [{
                    "functionCall": { "name": "list_men", "args": {} },
                    "thoughtSignature": "Cs4BAdHtim8abc"
                }] },
                "finishReason": "STOP"
            }]
        });
        let parsed = parse_response(&raw).unwrap();
        assert_eq!(parsed.tool_calls[0].signature, "Cs4BAdHtim8abc");

        let mut req = ChatRequest::new("");
        req.messages.push(LlmMessage {
            role: Role::Assistant,
            content: String::new(),
            images: vec![],
            tool_calls: parsed.tool_calls.clone(),
            tool_name: None,
            tool_call_id: None,
        });
        let part = &build_body(&provider(), &req)["contents"][0]["parts"][0];
        assert_eq!(part["functionCall"]["name"], "list_men");
        assert_eq!(part["thoughtSignature"], "Cs4BAdHtim8abc");
    }

    /// A screenshot goes out beside the words that ask about it.
    #[test]
    fn attachments_ride_with_the_operators_turn() {
        let mut req = ChatRequest::new("");
        req.messages.push(LlmMessage::user_with_images(
            "who is this",
            vec![crate::llm::ImagePart {
                mime: "image/png".into(),
                data: "AAAB".into(),
            }],
        ));
        let parts = &build_body(&provider(), &req)["contents"][0]["parts"];
        assert_eq!(parts[0]["text"], "who is this");
        assert_eq!(parts[1]["inlineData"]["mimeType"], "image/png");
        assert_eq!(parts[1]["inlineData"]["data"], "AAAB");
    }

    /// A provider that signs nothing must not gain an empty signature field.
    #[test]
    fn unsigned_calls_stay_unsigned() {
        let mut req = ChatRequest::new("");
        req.messages.push(LlmMessage {
            role: Role::Assistant,
            content: String::new(),
            images: vec![],
            tool_calls: vec![ToolCall {
                id: "1".into(),
                name: "get_man".into(),
                args: json!({}),
                signature: String::new(),
            }],
            tool_name: None,
            tool_call_id: None,
        });
        let part = &build_body(&provider(), &req)["contents"][0]["parts"][0];
        assert!(part.get("thoughtSignature").is_none());
    }

    fn model(name: &str) -> ProviderConfig {
        let mut p = provider();
        p.model = name.into();
        p
    }

    fn config_for(name: &str, thinking: Thinking) -> Value {
        let mut req = ChatRequest::new("");
        req.thinking = thinking;
        build_body(&model(name), &req)["generationConfig"].clone()
    }

    /// Probing this API found `thinkingLevel` refused by every model it serves,
    /// 3.x included, while a budget was accepted everywhere. So a budget is
    /// what goes out, for every generation.
    #[test]
    fn every_generation_is_given_a_budget() {
        let high = Thinking {
            effort: "high".into(),
            budget_tokens: None,
        };

        for name in [
            "gemini-3.5-flash",
            "gemini-3.5-flash-lite",
            "gemini-2.5-flash",
            "gemini-2.0-flash",
        ] {
            let config = config_for(name, high.clone());
            assert!(
                config.get("thinkingLevel").is_none(),
                "{name} must not be sent a level"
            );
            assert_eq!(config["thinkingConfig"]["thinkingBudget"], 24576, "{name}");
        }
    }

    /// Each family refuses a different set of numbers — measured, not
    /// documented — and the app has to stay inside them.
    #[test]
    fn a_budget_stays_inside_what_the_model_accepts() {
        let off = Thinking {
            effort: "none".into(),
            budget_tokens: None,
        };

        // Flash takes a zero budget and stops thinking.
        assert_eq!(
            config_for("gemini-2.5-flash", off.clone())["thinkingConfig"]["thinkingBudget"],
            0
        );
        // 2.5 Pro cannot stop, and refuses anything under 128.
        assert_eq!(
            config_for("gemini-2.5-pro", off.clone())["thinkingConfig"]["thinkingBudget"],
            128
        );
        // The 3.x lite models answer 0 with "invalid argument".
        assert_eq!(
            config_for("gemini-3.5-flash-lite", off.clone())["thinkingConfig"]["thinkingBudget"],
            128
        );
        // 2.5 flash-lite takes 0, but nothing between 1 and 511.
        assert_eq!(
            config_for("gemini-2.5-flash-lite", off)["thinkingConfig"]["thinkingBudget"],
            0
        );
        assert_eq!(
            config_for(
                "gemini-2.5-flash-lite",
                Thinking {
                    effort: String::new(),
                    budget_tokens: Some(128),
                }
            )["thinkingConfig"]["thinkingBudget"],
            512
        );
        // "Decide for yourself" passes through untouched.
        assert_eq!(
            config_for(
                "gemini-2.5-flash",
                Thinking {
                    effort: String::new(),
                    budget_tokens: Some(-1),
                }
            )["thinkingConfig"]["thinkingBudget"],
            -1
        );
    }

    /// Levels are the operator's vocabulary; they become numbers on the wire.
    #[test]
    fn levels_become_budgets() {
        let budget_for = |name: &str| {
            config_for(
                "gemini-2.5-flash",
                Thinking {
                    effort: name.into(),
                    budget_tokens: None,
                },
            )["thinkingConfig"]["thinkingBudget"]
                .clone()
        };

        assert_eq!(budget_for("none"), 0);
        assert_eq!(budget_for("minimal"), 512);
        assert_eq!(budget_for("low"), 2048);
        assert_eq!(budget_for("medium"), 8192);
        assert_eq!(budget_for("high"), 24576);
        assert_eq!(budget_for("xhigh"), 24576);
    }

    /// The 400 that says the field is unknown is recognised, and the retry
    /// carries the other spelling.
    #[test]
    fn a_rejected_field_is_swapped_for_the_other_one() {
        let error = r#"{"error":{"code":400,"message":"Invalid JSON payload received. Unknown name \"thinkingLevel\" at 'generation_config': Cannot find field.","status":"INVALID_ARGUMENT"}}"#;
        assert_eq!(rejected_field(error).as_deref(), Some("thinkingLevel"));
        assert!(is_thinking_field("thinkingLevel"));
        assert!(rejected_field(r#"{"error":{"message":"quota exceeded"}}"#).is_none());

        let thinking = Thinking {
            effort: "high".into(),
            budget_tokens: None,
        };
        let mut req = ChatRequest::new("");
        req.thinking = thinking.clone();
        let provider = model("gemini-3.5-flash");
        let mut body = build_body(&provider, &req);
        // A budget goes out by default, since that is what the API accepts.
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            24576
        );

        // An endpoint that only knows levels gets one on the retry.
        assert!(flip_thinking(&mut body, &provider, &thinking));
        assert!(body["generationConfig"].get("thinkingConfig").is_none());
        assert_eq!(body["generationConfig"]["thinkingLevel"], "high");

        // And back again, if that spelling is refused too.
        assert!(flip_thinking(&mut body, &provider, &thinking));
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            24576
        );
    }

    /// An explicit budget still wins over a level on the models that take one,
    /// and choosing nothing must not add a field at all.
    #[test]
    fn an_explicit_budget_wins_and_silence_adds_nothing() {
        let config = config_for(
            "gemini-2.5-flash",
            Thinking {
                effort: "high".into(),
                budget_tokens: Some(2048),
            },
        );
        assert_eq!(config["thinkingConfig"]["thinkingBudget"], 2048);

        let config = config_for("gemini-3.5-flash", Thinking::default());
        assert!(config.get("thinkingLevel").is_none());
        assert!(config.get("thinkingConfig").is_none());
    }

    /// A stream arrives in arbitrary pieces: an event split across two chunks
    /// must still be read exactly once, and only when it is whole.
    /// Not every model offers thought summaries, and the ones that do not say
    /// so by name. The field is dropped and the request goes through.
    #[test]
    fn an_unknown_field_is_dropped_and_the_request_retried() {
        let error = r#"{"error":{"code":400,"message":"Invalid JSON payload received. Unknown name \"thinkingSummaries\" at 'generation_config': Cannot find field.","status":"INVALID_ARGUMENT"}}"#;
        let field = rejected_field(error).expect("the rejection names its field");
        assert_eq!(field, "thinkingSummaries");
        assert!(!is_thinking_field(&field), "it has no second spelling");

        let mut req = ChatRequest::new("");
        req.thinking = Thinking {
            effort: "high".into(),
            budget_tokens: None,
        };
        let provider = model("gemini-3.5-flash");
        let mut body = build_body(&provider, &req);
        // Summaries ride along with whichever control is in use.
        body["generationConfig"]["thinkingSummaries"] = json!("auto");

        assert!(drop_field(&mut body, &field));
        assert!(body["generationConfig"].get("thinkingSummaries").is_none());
        // Only the rejected field goes; what was asked for stays.
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            24576
        );
        assert!(!drop_field(&mut body, &field), "nothing left to drop");
    }

    /// `includeThoughts` sits inside `thinkingConfig` on the older models, so
    /// the search has to go a level down.
    #[test]
    fn a_nested_field_is_dropped() {
        let mut req = ChatRequest::new("");
        req.thinking = Thinking {
            effort: "medium".into(),
            budget_tokens: None,
        };
        let provider = model("gemini-2.5-flash");
        let mut body = build_body(&provider, &req);
        request_thoughts(&mut body, &provider);
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["includeThoughts"],
            true
        );

        assert!(drop_field(&mut body, "includeThoughts"));
        assert!(body["generationConfig"]["thinkingConfig"]
            .get("includeThoughts")
            .is_none());
        assert!(body["generationConfig"]["thinkingConfig"]["thinkingBudget"].is_number());
    }

    /// Gemini separates its events with CRLF, and the whole answer went
    /// missing until that was handled.
    #[test]
    fn carriage_returns_do_not_hide_events() {
        let mut buffer = String::from("data: {\"a\":1}\r\n\r\ndata: {\"b\":2}\r\n\r\n");
        let events = take_events(&mut buffer);
        assert_eq!(events.len(), 2, "CRLF events were missed");
        assert_eq!(events[0]["a"], 1);
        assert_eq!(events[1]["b"], 2);
        assert!(buffer.is_empty());
    }

    #[test]
    fn events_are_only_taken_when_complete() {
        let mut buffer = String::from("data: {\"a\":1}\n\ndata: {\"b\"");
        let first = take_events(&mut buffer);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0]["a"], 1);
        assert_eq!(buffer, "data: {\"b\"");

        buffer.push_str(":2}\n\n");
        let second = take_events(&mut buffer);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0]["b"], 2);
        assert!(buffer.is_empty());

        // Keep-alives and the terminator carry nothing to parse.
        let mut buffer = String::from(": ping\n\ndata: [DONE]\n\n");
        assert!(take_events(&mut buffer).is_empty());
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
