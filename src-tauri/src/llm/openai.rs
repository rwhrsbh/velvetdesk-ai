//! Any OpenAI-compatible /chat/completions endpoint: OpenAI, OpenRouter,
//! DeepSeek, Together, LocalAI, Ollama (/v1), vLLM.

use serde_json::{json, Value};

use super::{CallError, ChatRequest, ChatResponse, Role, Thinking, ToolCall, Usage};
use crate::config::ProviderConfig;

pub async fn call(
    http: &reqwest::Client,
    provider: &ProviderConfig,
    api_key: &str,
    request: &ChatRequest,
) -> Result<ChatResponse, CallError> {
    let base = provider.base_url.trim_end_matches('/');
    let url = if base.ends_with("/chat/completions") {
        base.to_string()
    } else {
        format!("{base}/chat/completions")
    };

    let body = build_body(provider, request);

    let mut req = http.post(&url).header("content-type", "application/json");
    if !api_key.trim().is_empty() && api_key != "local" {
        req = req.header("authorization", format!("Bearer {api_key}"));
    }
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
    let mut messages: Vec<Value> = vec![];
    if !request.system.trim().is_empty() {
        messages.push(json!({ "role": "system", "content": request.system }));
    }

    for msg in &request.messages {
        match msg.role {
            Role::User => messages.push(json!({ "role": "user", "content": msg.content })),
            Role::Assistant => {
                let mut m = json!({ "role": "assistant", "content": msg.content });
                if !msg.tool_calls.is_empty() {
                    m["tool_calls"] = Value::Array(
                        msg.tool_calls
                            .iter()
                            .map(|c| {
                                json!({
                                    "id": c.id,
                                    "type": "function",
                                    "function": {
                                        "name": c.name,
                                        "arguments": c.args.to_string(),
                                    }
                                })
                            })
                            .collect(),
                    );
                }
                messages.push(m);
            }
            Role::Tool => messages.push(json!({
                "role": "tool",
                "tool_call_id": msg.tool_call_id.clone().unwrap_or_default(),
                "name": msg.tool_name.clone().unwrap_or_default(),
                "content": msg.content,
            })),
        }
    }

    let mut body = json!({
        "model": provider.model,
        "messages": messages,
        "temperature": request.temperature,
        "stream": false,
    });

    if let Some(max) = request.max_output_tokens.or(provider.max_output_tokens) {
        body["max_tokens"] = json!(max);
    }
    if request.force_json {
        body["response_format"] = json!({ "type": "json_object" });
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect(),
        );
        body["tool_choice"] = json!("auto");
    }

    apply_thinking(&mut body, provider, &request.thinking);

    body
}

/// OpenAI-compatible endpoints spell reasoning control three different ways,
/// and sending the wrong one is a 400 rather than a silent no-op — OpenAI
/// rejects unknown body parameters. The dialect comes from the provider
/// settings, inferred from the endpoint unless the operator picked one.
fn apply_thinking(body: &mut Value, provider: &ProviderConfig, thinking: &Thinking) {
    if thinking.is_default() {
        return;
    }
    match provider.dialect() {
        // OpenRouter's unified `reasoning` object, which it translates for
        // whichever upstream model is behind the request.
        "openrouter" => {
            let mut reasoning = json!({});
            if let Some(budget) = thinking.budget_tokens {
                reasoning["max_tokens"] = json!(budget);
            }
            match thinking.level() {
                Some("none") | Some("off") => reasoning["enabled"] = json!(false),
                Some(level) => reasoning["effort"] = json!(level),
                None => {}
            }
            body["reasoning"] = reasoning;
        }
        // Alibaba's models: an on/off switch plus a token budget.
        "qwen" => {
            match thinking.level() {
                Some("none") | Some("off") => {
                    body["enable_thinking"] = json!(false);
                }
                Some(_) => body["enable_thinking"] = json!(true),
                None => {}
            }
            if let Some(budget) = thinking.budget_tokens {
                body["enable_thinking"] = json!(budget != 0);
                body["thinking_budget"] = json!(budget);
            }
        }
        // Plain OpenAI (and Groq, Azure, most local servers): a single level.
        // A budget has no equivalent here, so it is left out rather than
        // guessed at.
        _ => {
            if let Some(level) = thinking.level() {
                body["reasoning_effort"] = json!(level);
            }
        }
    }
}

fn parse_response(value: &Value) -> Result<ChatResponse, CallError> {
    if let Some(err) = value.get("error") {
        return Err(CallError::Parse(err.to_string()));
    }
    let choice = value
        .get("choices")
        .and_then(|c| c.get(0))
        .ok_or_else(|| CallError::Parse("no choices returned".into()))?;
    let message = choice
        .get("message")
        .ok_or_else(|| CallError::Parse("choice without message".into()))?;

    let text = message
        .get("content")
        .and_then(|c| match c {
            Value::String(s) => Some(s.clone()),
            // Some gateways return content as an array of parts.
            Value::Array(parts) => Some(
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        })
        .unwrap_or_default();

    let mut tool_calls = vec![];
    if let Some(calls) = message.get("tool_calls").and_then(|c| c.as_array()) {
        for (i, call) in calls.iter().enumerate() {
            let func = call.get("function").unwrap_or(call);
            let name = func
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or_default()
                .to_string();
            let raw_args = func
                .get("arguments")
                .and_then(|a| a.as_str())
                .unwrap_or("{}");
            let args = serde_json::from_str::<Value>(raw_args)
                .unwrap_or_else(|_| json!({ "raw": raw_args }));
            tool_calls.push(ToolCall {
                id: call
                    .get("id")
                    .and_then(|i| i.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("call-{i}")),
                name,
                args,
                signature: String::new(),
            });
        }
    }

    let usage_raw = value.get("usage");
    let usage = Usage {
        prompt_tokens: usage_raw
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        completion_tokens: usage_raw
            .and_then(|u| u.get("completion_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        total_tokens: usage_raw
            .and_then(|u| u.get("total_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
    };

    Ok(ChatResponse {
        text: text.trim().to_string(),
        tool_calls,
        usage,
        finish_reason: choice
            .get("finish_reason")
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
    use crate::llm::LlmMessage;

    fn provider() -> ProviderConfig {
        ProviderConfig {
            id: "openai".into(),
            label: "OpenAI".into(),
            kind: ProviderKind::OpenaiCompatible,
            base_url: "https://api.openai.com/v1".into(),
            api_version: "v1".into(),
            model: "gpt-4o-mini".into(),
            extra_headers: vec![],
            temperature: 0.7,
            max_output_tokens: Some(2048),
            transcribe_model: String::new(),
            thinking_effort: String::new(),
            thinking_budget: None,
            reasoning_dialect: "auto".into(),
            context_tokens: None,
            key_count: 1,
        }
    }

    fn provider_at(url: &str) -> ProviderConfig {
        let mut p = provider();
        p.base_url = url.into();
        p
    }

    /// Each endpoint gets the spelling it understands — sending OpenAI a
    /// `reasoning` object, or OpenRouter a `reasoning_effort`, is a 400.
    #[test]
    fn reasoning_uses_the_dialect_of_the_endpoint() {
        let mut req = ChatRequest::new("");
        req.thinking = Thinking {
            effort: "high".into(),
            budget_tokens: None,
        };

        let openai = build_body(&provider_at("https://api.openai.com/v1"), &req);
        assert_eq!(openai["reasoning_effort"], "high");
        assert!(openai.get("reasoning").is_none());

        let router = build_body(&provider_at("https://openrouter.ai/api/v1"), &req);
        assert_eq!(router["reasoning"]["effort"], "high");
        assert!(router.get("reasoning_effort").is_none());

        let qwen = build_body(
            &provider_at("https://dashscope.aliyuncs.com/compatible-mode/v1"),
            &req,
        );
        assert_eq!(qwen["enable_thinking"], true);

        // A budget is only meaningful where it is supported.
        req.thinking = Thinking {
            effort: String::new(),
            budget_tokens: Some(4096),
        };
        let router = build_body(&provider_at("https://openrouter.ai/api/v1"), &req);
        assert_eq!(router["reasoning"]["max_tokens"], 4096);
        let openai = build_body(&provider_at("https://api.openai.com/v1"), &req);
        assert!(openai.get("reasoning_effort").is_none());
    }

    /// Nothing chosen must not add a single field.
    #[test]
    fn default_thinking_adds_nothing() {
        let req = ChatRequest::new("");
        let body = build_body(&provider_at("https://api.openai.com/v1"), &req);
        for key in [
            "reasoning",
            "reasoning_effort",
            "enable_thinking",
            "thinking_budget",
        ] {
            assert!(body.get(key).is_none(), "{key} must be absent");
        }
    }

    #[test]
    fn builds_messages_with_system() {
        let mut req = ChatRequest::new("system rules");
        req.messages.push(LlmMessage::user("hello"));
        let body = build_body(&provider(), &req);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "hello");
        assert_eq!(body["max_tokens"], 2048);
    }

    #[test]
    fn parses_tool_calls() {
        let raw = json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "function": { "name": "add_gift", "arguments": "{\"man_id\":\"7\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7 }
        });
        let parsed = parse_response(&raw).unwrap();
        assert_eq!(parsed.tool_calls[0].name, "add_gift");
        assert_eq!(parsed.tool_calls[0].args["man_id"], "7");
        assert_eq!(parsed.usage.total_tokens, 7);
    }
}
