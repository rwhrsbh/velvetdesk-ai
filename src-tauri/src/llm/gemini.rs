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

    // Model names do not always tell the whole truth about which spelling of
    // the thinking control an endpoint accepts, so a rejection of the field is
    // retried once with the other one instead of surfacing as an error.
    if status.as_u16() == 400
        && rejected_thinking_field(&text)
        && flip_thinking(&mut body, provider, &request.thinking)
    {
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

/// Gemini spells the reasoning control two incompatible ways, and sending the
/// wrong one is a hard 400 ("Unknown name \"thinkingLevel\""):
///
/// * Gemini 3 and newer: `generationConfig.thinkingLevel`, a named level.
/// * Gemini 2.x: `generationConfig.thinkingConfig.thinkingBudget`, a number of
///   tokens — also the only way to switch thinking off, with 0.
///
/// The model name decides, and [`call`] retries with the other spelling if the
/// endpoint disagrees with that guess.
fn apply_thinking(config: &mut Value, provider: &ProviderConfig, thinking: &Thinking) {
    if thinking.is_default() {
        return;
    }
    if takes_thinking_level(&provider.model) {
        set_thinking_level(config, thinking);
    } else {
        set_thinking_budget(config, provider, thinking);
    }
}

/// Gemini 3 and later take a level; everything older takes a budget.
fn takes_thinking_level(model: &str) -> bool {
    let model = model.to_lowercase();
    let Some(rest) = model.strip_prefix("gemini-") else {
        return false;
    };
    rest.split(['.', '-'])
        .next()
        .and_then(|major| major.parse::<u32>().ok())
        .is_some_and(|major| major >= 3)
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
    // 2.5 Pro cannot stop thinking and rejects anything under 128.
    let budget = if provider.model.to_lowercase().contains("2.5-pro") && (0..128).contains(&budget)
    {
        128
    } else {
        budget
    };
    config["thinkingConfig"] = json!({ "thinkingBudget": budget });
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

/// True when a 400 is the endpoint rejecting the thinking field we picked.
fn rejected_thinking_field(body: &str) -> bool {
    let body = body.to_lowercase();
    body.contains("unknown name")
        && (body.contains("thinkinglevel") || body.contains("thinking_level"))
        || body.contains("unknown name") && body.contains("thinkingconfig")
        || body.contains("unknown name") && body.contains("thinking_config")
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
            transcribe_model: String::new(),
            thinking_effort: String::new(),
            thinking_budget: None,
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
            tool_calls: parsed.tool_calls.clone(),
            tool_name: None,
            tool_call_id: None,
        });
        let part = &build_body(&provider(), &req)["contents"][0]["parts"][0];
        assert_eq!(part["functionCall"]["name"], "list_men");
        assert_eq!(part["thoughtSignature"], "Cs4BAdHtim8abc");
    }

    /// A provider that signs nothing must not gain an empty signature field.
    #[test]
    fn unsigned_calls_stay_unsigned() {
        let mut req = ChatRequest::new("");
        req.messages.push(LlmMessage {
            role: Role::Assistant,
            content: String::new(),
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

    /// Gemini 3 takes a named level; 2.x rejects that field outright with
    /// `Unknown name "thinkingLevel"` and wants a token budget instead.
    #[test]
    fn each_generation_gets_the_field_it_accepts() {
        let high = Thinking {
            effort: "high".into(),
            budget_tokens: None,
        };

        for new_model in [
            "gemini-3.5-flash",
            "gemini-3-pro-preview",
            "gemini-4.0-flash",
            "models/gemini-3.5-flash",
        ] {
            let name = new_model.trim_start_matches("models/");
            let config = config_for(name, high.clone());
            assert_eq!(config["thinkingLevel"], "high", "{name}");
            assert!(config.get("thinkingConfig").is_none(), "{name}");
        }

        for old_model in ["gemini-2.5-flash", "gemini-2.5-pro", "gemini-2.0-flash"] {
            let config = config_for(old_model, high.clone());
            assert!(config.get("thinkingLevel").is_none(), "{old_model}");
            assert_eq!(
                config["thinkingConfig"]["thinkingBudget"], 24576,
                "{old_model}"
            );
        }
    }

    /// Switching thinking off means a zero budget on 2.x — except on 2.5 Pro,
    /// which cannot stop thinking and rejects anything below 128.
    #[test]
    fn thinking_off_respects_the_model_floor() {
        let off = Thinking {
            effort: "none".into(),
            budget_tokens: None,
        };
        assert_eq!(
            config_for("gemini-2.5-flash", off.clone())["thinkingConfig"]["thinkingBudget"],
            0
        );
        assert_eq!(
            config_for("gemini-2.5-pro", off.clone())["thinkingConfig"]["thinkingBudget"],
            128
        );
        // On Gemini 3 the nearest thing to "off" is the minimal level.
        assert_eq!(
            config_for("gemini-3.5-flash", off)["thinkingLevel"],
            "minimal"
        );
    }

    /// A budget typed by the operator is honoured as-is on 2.x, and mapped to
    /// the nearest level on 3.x rather than being sent as an unknown field.
    #[test]
    fn a_budget_is_translated_for_the_newer_models() {
        let budget = |n: i32| Thinking {
            effort: String::new(),
            budget_tokens: Some(n),
        };
        assert_eq!(
            config_for("gemini-2.5-flash", budget(4096))["thinkingConfig"]["thinkingBudget"],
            4096
        );
        assert_eq!(
            config_for("gemini-3.5-flash", budget(512))["thinkingLevel"],
            "minimal"
        );
        assert_eq!(
            config_for("gemini-3.5-flash", budget(20000))["thinkingLevel"],
            "high"
        );
        // -1 is Gemini's "decide for yourself".
        assert_eq!(
            config_for("gemini-3.5-flash", budget(-1))["thinkingLevel"],
            "medium"
        );
        assert_eq!(
            config_for("gemini-2.5-flash", budget(-1))["thinkingConfig"]["thinkingBudget"],
            -1
        );
    }

    /// The 400 that says the field is unknown is recognised, and the retry
    /// carries the other spelling.
    #[test]
    fn a_rejected_field_is_swapped_for_the_other_one() {
        let error = r#"{"error":{"code":400,"message":"Invalid JSON payload received. Unknown name \"thinkingLevel\" at 'generation_config': Cannot find field.","status":"INVALID_ARGUMENT"}}"#;
        assert!(rejected_thinking_field(error));
        assert!(!rejected_thinking_field(
            r#"{"error":{"message":"quota exceeded"}}"#
        ));

        let thinking = Thinking {
            effort: "high".into(),
            budget_tokens: None,
        };
        let mut req = ChatRequest::new("");
        req.thinking = thinking.clone();
        let provider = model("gemini-3.5-flash");
        let mut body = build_body(&provider, &req);
        assert_eq!(body["generationConfig"]["thinkingLevel"], "high");

        assert!(flip_thinking(&mut body, &provider, &thinking));
        assert!(body["generationConfig"].get("thinkingLevel").is_none());
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            24576
        );

        // And back the other way for an endpoint that only knows levels.
        let provider = model("gemini-2.5-flash");
        let mut body = build_body(&provider, &req);
        assert!(flip_thinking(&mut body, &provider, &thinking));
        assert_eq!(body["generationConfig"]["thinkingLevel"], "high");
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
