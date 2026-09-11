//! Turning the two wire formats into one request, and the answer back again.
//!
//! Clients arrive speaking either OpenAI's `/chat/completions` or Gemini's
//! `generateContent`, because that is what they already speak to the vendors.
//! Inside, there is one shape — the crate's `ChatRequest` — so the routing,
//! the rotation and the billing never learn which door the request came in
//! through.

use serde::Deserialize;
use serde_json::{json, Map, Value};

use vd_llm::{ChatRequest, ChatResponse, ImagePart, LlmMessage, Role, Thinking, ToolCall, ToolDef};

// ---------------------------------------------------------------- OpenAI in

#[derive(Debug, Clone, Deserialize, Default)]
pub struct OaiRequest {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub messages: Vec<Value>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub max_completion_tokens: Option<u32>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub tools: Vec<Value>,
    #[serde(default)]
    pub response_format: Option<Value>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

impl OaiRequest {
    pub fn to_chat_request(&self) -> ChatRequest {
        let mut request = ChatRequest::new("");
        let mut system = String::new();

        for message in &self.messages {
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user");
            if role == "system" || role == "developer" {
                let text = content_text(message.get("content"));
                if !text.is_empty() {
                    if !system.is_empty() {
                        system.push_str("\n\n");
                    }
                    system.push_str(&text);
                }
                continue;
            }
            request.messages.push(match role {
                "assistant" => LlmMessage::assistant(
                    content_text(message.get("content")),
                    read_tool_calls(message.get("tool_calls")),
                ),
                "tool" => LlmMessage {
                    role: Role::Tool,
                    content: content_text(message.get("content")),
                    images: vec![],
                    tool_calls: vec![],
                    tool_call_id: message
                        .get("tool_call_id")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    tool_name: message
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                },
                _ => LlmMessage::user_with_images(
                    content_text(message.get("content")),
                    content_images(message.get("content")),
                ),
            });
        }

        request.system = system;
        request.tools = self
            .tools
            .iter()
            .filter_map(|tool| {
                let function = tool.get("function").unwrap_or(tool);
                Some(ToolDef {
                    name: function.get("name")?.as_str()?.to_string(),
                    description: function
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    parameters: function
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
                })
            })
            .collect();
        if let Some(temperature) = self.temperature {
            request.temperature = temperature;
        }
        request.max_output_tokens = self.max_completion_tokens.or(self.max_tokens);
        request.force_json = self
            .response_format
            .as_ref()
            .and_then(|format| format.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|kind| kind.starts_with("json"));
        request.thinking = Thinking {
            effort: self.reasoning_effort.clone().unwrap_or_default(),
            budget_tokens: None,
        };
        request.stream = self.stream;
        request
    }
}

/// The text of a message, whether it arrived as a string or as parts.
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Pictures attached to a turn, taken from `data:` URLs. A link to a picture
/// elsewhere is left alone: fetching it would make the gateway a crawler, and
/// the desktop sends its screenshots inline anyway.
fn content_images(content: Option<&Value>) -> Vec<ImagePart> {
    let Some(Value::Array(parts)) = content else {
        return vec![];
    };
    parts
        .iter()
        .filter_map(|part| {
            let url = part.get("image_url")?.get("url")?.as_str()?;
            let rest = url.strip_prefix("data:")?;
            let (mime, data) = rest.split_once(";base64,")?;
            Some(ImagePart {
                mime: mime.to_string(),
                data: data.to_string(),
            })
        })
        .collect()
}

fn read_tool_calls(value: Option<&Value>) -> Vec<ToolCall> {
    let Some(Value::Array(calls)) = value else {
        return vec![];
    };
    calls
        .iter()
        .filter_map(|call| {
            let function = call.get("function")?;
            let args = match function.get("arguments") {
                Some(Value::String(text)) => serde_json::from_str(text).unwrap_or(json!({})),
                Some(other) => other.clone(),
                None => json!({}),
            };
            Some(ToolCall {
                id: call
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                name: function.get("name")?.as_str()?.to_string(),
                args,
                signature: String::new(),
            })
        })
        .collect()
}

// --------------------------------------------------------------- OpenAI out

fn oai_tool_calls(response: &ChatResponse) -> Value {
    Value::Array(
        response
            .tool_calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "type": "function",
                    "function": {
                        "name": call.name,
                        "arguments": call.args.to_string(),
                    }
                })
            })
            .collect(),
    )
}

fn finish_reason(response: &ChatResponse) -> &'static str {
    if !response.tool_calls.is_empty() {
        return "tool_calls";
    }
    match response.finish_reason.to_uppercase().as_str() {
        "MAX_TOKENS" | "LENGTH" => "length",
        _ => "stop",
    }
}

pub fn oai_usage(response: &ChatResponse) -> Value {
    json!({
        "prompt_tokens": response.usage.prompt_tokens,
        "completion_tokens": response.usage.completion_tokens,
        "total_tokens": response.usage.total_tokens,
        "prompt_tokens_details": { "cached_tokens": response.usage.cached_tokens },
    })
}

pub fn oai_completion(id: &str, created: i64, model: &str, response: &ChatResponse) -> Value {
    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    message.insert("content".into(), json!(response.text));
    if !response.tool_calls.is_empty() {
        message.insert("tool_calls".into(), oai_tool_calls(response));
    }
    json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": finish_reason(response),
        }],
        "usage": oai_usage(response),
    })
}

pub fn oai_chunk(id: &str, created: i64, model: &str, delta: &str) -> Value {
    json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{ "index": 0, "delta": { "content": delta }, "finish_reason": Value::Null }],
    })
}

/// The last chunk of a stream: what ended it, the tool calls if there were
/// any, and the usage the bill was written from.
pub fn oai_final_chunk(id: &str, created: i64, model: &str, response: &ChatResponse) -> Value {
    let mut delta = Map::new();
    if !response.tool_calls.is_empty() {
        delta.insert("tool_calls".into(), oai_tool_calls(response));
    }
    json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": Value::Object(delta),
            "finish_reason": finish_reason(response),
        }],
        "usage": oai_usage(response),
    })
}

// ---------------------------------------------------------------- Gemini in

/// Gemini's own request shape, as the vendor's clients send it.
pub fn gemini_to_chat_request(body: &Value, stream: bool) -> ChatRequest {
    let mut request = ChatRequest::new(content_parts_text(body.get("systemInstruction")));

    if let Some(Value::Array(contents)) = body.get("contents") {
        for content in contents {
            let role = content
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user");
            let text = content_parts_text(Some(content));
            let images = content_parts_images(content);
            request.messages.push(if role == "model" {
                LlmMessage::assistant(text, gemini_tool_calls(content))
            } else {
                LlmMessage::user_with_images(text, images)
            });
        }
    }

    if let Some(Value::Array(tools)) = body.get("tools") {
        for tool in tools {
            let Some(Value::Array(declarations)) = tool.get("functionDeclarations") else {
                continue;
            };
            for declaration in declarations {
                let Some(name) = declaration.get("name").and_then(Value::as_str) else {
                    continue;
                };
                request.tools.push(ToolDef {
                    name: name.to_string(),
                    description: declaration
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    parameters: declaration
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
                });
            }
        }
    }

    if let Some(config) = body.get("generationConfig") {
        if let Some(temperature) = config.get("temperature").and_then(Value::as_f64) {
            request.temperature = temperature as f32;
        }
        request.max_output_tokens = config
            .get("maxOutputTokens")
            .and_then(Value::as_u64)
            .map(|n| n as u32);
        request.force_json = config
            .get("responseMimeType")
            .and_then(Value::as_str)
            .is_some_and(|mime| mime.contains("json"));
        if let Some(budget) = config
            .get("thinkingConfig")
            .and_then(|t| t.get("thinkingBudget"))
            .and_then(Value::as_i64)
        {
            request.thinking.budget_tokens = Some(budget as i32);
        }
    }

    request.stream = stream;
    request
}

fn content_parts_text(content: Option<&Value>) -> String {
    let Some(Value::Array(parts)) = content.and_then(|c| c.get("parts")) else {
        return String::new();
    };
    parts
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

fn content_parts_images(content: &Value) -> Vec<ImagePart> {
    let Some(Value::Array(parts)) = content.get("parts") else {
        return vec![];
    };
    parts
        .iter()
        .filter_map(|part| {
            let inline = part.get("inlineData").or_else(|| part.get("inline_data"))?;
            Some(ImagePart {
                mime: inline
                    .get("mimeType")
                    .or_else(|| inline.get("mime_type"))?
                    .as_str()?
                    .to_string(),
                data: inline.get("data")?.as_str()?.to_string(),
            })
        })
        .collect()
}

fn gemini_tool_calls(content: &Value) -> Vec<ToolCall> {
    let Some(Value::Array(parts)) = content.get("parts") else {
        return vec![];
    };
    parts
        .iter()
        .filter_map(|part| {
            let call = part.get("functionCall")?;
            Some(ToolCall {
                id: String::new(),
                name: call.get("name")?.as_str()?.to_string(),
                args: call.get("args").cloned().unwrap_or_else(|| json!({})),
                signature: part
                    .get("thoughtSignature")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect()
}

// --------------------------------------------------------------- Gemini out

fn gemini_parts(response: &ChatResponse) -> Vec<Value> {
    let mut parts = vec![];
    if !response.text.is_empty() {
        parts.push(json!({ "text": response.text }));
    }
    for call in &response.tool_calls {
        let mut part = json!({ "functionCall": { "name": call.name, "args": call.args } });
        if !call.signature.is_empty() {
            part["thoughtSignature"] = json!(call.signature);
        }
        parts.push(part);
    }
    parts
}

pub fn gemini_usage(response: &ChatResponse) -> Value {
    json!({
        "promptTokenCount": response.usage.prompt_tokens,
        "candidatesTokenCount": response.usage.completion_tokens,
        "totalTokenCount": response.usage.total_tokens,
        "cachedContentTokenCount": response.usage.cached_tokens,
    })
}

pub fn gemini_response(model: &str, response: &ChatResponse) -> Value {
    json!({
        "candidates": [{
            "content": { "role": "model", "parts": gemini_parts(response) },
            "finishReason": if response.finish_reason.is_empty() {
                "STOP".to_string()
            } else {
                response.finish_reason.to_uppercase()
            },
            "index": 0,
        }],
        "usageMetadata": gemini_usage(response),
        "modelVersion": model,
    })
}

pub fn gemini_chunk(delta: &str) -> Value {
    json!({
        "candidates": [{
            "content": { "role": "model", "parts": [{ "text": delta }] },
            "index": 0,
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_turns_are_lifted_out_of_the_messages() {
        let body: OaiRequest = serde_json::from_value(json!({
            "model": "deepseek-chat",
            "messages": [
                { "role": "system", "content": "be brief" },
                { "role": "system", "content": "and kind" },
                { "role": "user", "content": "hello" }
            ]
        }))
        .unwrap();
        let request = body.to_chat_request();
        assert_eq!(request.system, "be brief\n\nand kind");
        assert_eq!(request.messages.len(), 1);
        assert_eq!(request.messages[0].content, "hello");
    }

    /// Content arrives as a bare string or as parts, and a picture arrives as
    /// a data URL — the desktop sends screenshots that way.
    #[test]
    fn parts_and_pictures_survive_the_crossing() {
        let body: OaiRequest = serde_json::from_value(json!({
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": "what is this" },
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAB" } },
                    { "type": "image_url", "image_url": { "url": "https://example.invalid/x.png" } }
                ]
            }]
        }))
        .unwrap();
        let request = body.to_chat_request();
        assert_eq!(request.messages[0].content, "what is this");
        assert_eq!(
            request.messages[0].images.len(),
            1,
            "the linked one is left alone"
        );
        assert_eq!(request.messages[0].images[0].mime, "image/png");
    }

    #[test]
    fn tool_calls_and_results_round_trip() {
        let body: OaiRequest = serde_json::from_value(json!({
            "messages": [
                { "role": "assistant", "content": "", "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": { "name": "add_fact", "arguments": "{\"man_id\":\"m1\"}" }
                }]},
                { "role": "tool", "tool_call_id": "call_1", "name": "add_fact", "content": "ok" }
            ],
            "tools": [{ "type": "function", "function": {
                "name": "add_fact",
                "description": "remember something",
                "parameters": { "type": "object", "properties": {} }
            }}]
        }))
        .unwrap();
        let request = body.to_chat_request();
        assert_eq!(request.messages[0].tool_calls[0].name, "add_fact");
        assert_eq!(request.messages[0].tool_calls[0].args["man_id"], "m1");
        assert_eq!(request.messages[1].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(request.tools[0].name, "add_fact");
    }

    #[test]
    fn json_mode_and_limits_carry_over() {
        let body: OaiRequest = serde_json::from_value(json!({
            "messages": [],
            "temperature": 0.2,
            "max_tokens": 512,
            "response_format": { "type": "json_object" },
            "reasoning_effort": "high"
        }))
        .unwrap();
        let request = body.to_chat_request();
        assert!(request.force_json);
        assert_eq!(request.max_output_tokens, Some(512));
        assert_eq!(request.temperature, 0.2);
        assert_eq!(request.thinking.effort, "high");
    }

    fn answer() -> ChatResponse {
        ChatResponse {
            text: "hi".into(),
            raw: String::new(),
            model: "deepseek-chat".into(),
            thoughts: String::new(),
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                name: "add_fact".into(),
                args: json!({ "man_id": "m1" }),
                signature: String::new(),
            }],
            usage: vd_llm::Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cached_tokens: 6,
            },
            finish_reason: String::new(),
            key_index: 0,
            attempts: 1,
        }
    }

    /// An answer with tool calls ends as `tool_calls`, and the cached tokens
    /// are reported where an OpenAI client looks for them.
    #[test]
    fn the_openai_answer_carries_calls_and_cache() {
        let body = oai_completion("id", 1, "deepseek-chat", &answer());
        assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
        let arguments = body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        assert!(
            arguments.contains("m1"),
            "arguments go as a string: {arguments}"
        );
        assert_eq!(body["usage"]["prompt_tokens_details"]["cached_tokens"], 6);
    }

    #[test]
    fn the_gemini_answer_carries_calls_and_cache() {
        let body = gemini_response("gemini-2.5-flash", &answer());
        let parts = body["candidates"][0]["content"]["parts"]
            .as_array()
            .unwrap();
        assert_eq!(parts[0]["text"], "hi");
        assert_eq!(parts[1]["functionCall"]["name"], "add_fact");
        assert_eq!(body["usageMetadata"]["cachedContentTokenCount"], 6);
    }

    #[test]
    fn a_gemini_request_reads_as_one_of_ours() {
        let request = gemini_to_chat_request(
            &json!({
                "systemInstruction": { "parts": [{ "text": "be brief" }] },
                "contents": [
                    { "role": "user", "parts": [
                        { "text": "look" },
                        { "inlineData": { "mimeType": "image/png", "data": "AAAB" } }
                    ]},
                    { "role": "model", "parts": [{ "functionCall": { "name": "add_fact", "args": { "a": 1 } } }] }
                ],
                "generationConfig": { "temperature": 0.3, "maxOutputTokens": 128, "responseMimeType": "application/json" },
                "tools": [{ "functionDeclarations": [{ "name": "add_fact", "parameters": { "type": "object" } }] }]
            }),
            true,
        );
        assert_eq!(request.system, "be brief");
        assert_eq!(request.messages[0].images.len(), 1);
        assert_eq!(request.messages[1].tool_calls[0].name, "add_fact");
        assert_eq!(request.max_output_tokens, Some(128));
        assert!(request.force_json);
        assert!(request.stream);
        assert_eq!(request.tools.len(), 1);
    }

    /// Length is the one finish reason a client acts on differently, so it
    /// has to survive both spellings.
    #[test]
    fn a_cut_off_answer_says_so_in_openai_terms() {
        let mut response = answer();
        response.tool_calls.clear();
        response.finish_reason = "MAX_TOKENS".into();
        let body = oai_completion("id", 1, "m", &response);
        assert_eq!(body["choices"][0]["finish_reason"], "length");
    }
}
