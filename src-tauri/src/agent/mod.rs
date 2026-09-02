pub mod prompts;
pub mod tools;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::config::{AgentMode, ProviderConfig, SecurityLevel, Settings};
use crate::error::{AppError, Result};
use crate::llm::keypool::KeyPool;
use crate::llm::{extract_json_object, ChatRequest, LlmClient, LlmMessage, Usage};
use crate::models::*;
use crate::storage::{Paths, Scope};
use tools::{PendingAction, ToolOutcome};

#[derive(Debug, Clone, Deserialize)]
pub struct RunInput {
    pub model_id: String,
    #[serde(default)]
    pub man_id: Option<String>,
    #[serde(default)]
    pub mode: Option<AgentMode>,
    #[serde(default)]
    pub security: Option<SecurityLevel>,
    pub message: String,
    /// chat | letter — only used as a hint for the draft length.
    #[serde(default)]
    pub channel: Option<String>,
    /// When true the operator's text is stored as an incoming message first.
    #[serde(default)]
    pub log_incoming: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunStep {
    pub kind: String,
    pub tool: Option<String>,
    pub summary: String,
    pub detail: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunOutput {
    pub reply: String,
    pub mode: AgentMode,
    pub security: SecurityLevel,
    pub model_id: String,
    pub man_id: Option<String>,
    pub steps: Vec<RunStep>,
    pub pending: Vec<PendingAction>,
    pub usage: Usage,
    pub key_index: usize,
    pub turns: usize,
}

pub struct AgentDeps<'a> {
    pub paths: &'a Paths,
    pub settings: &'a Settings,
    pub provider: &'a ProviderConfig,
    pub pool: Arc<KeyPool>,
    pub llm: &'a LlmClient,
    pub emit: &'a (dyn Fn(Value) + Send + Sync),
}

pub async fn run(deps: &AgentDeps<'_>, input: RunInput) -> Result<RunOutput> {
    let mode = input.mode.unwrap_or(deps.settings.agent_mode);
    let security = input.security.unwrap_or(deps.settings.security_level);
    let scope = deps.paths.scope(&input.model_id)?;
    let profile = scope.read_profile()?;

    let man = match &input.man_id {
        Some(id) => Some(scope.read_man(id)?),
        None => None,
    };
    let thread = match &input.man_id {
        Some(id) => Some(scope.read_chat(id)?),
        None => None,
    };

    if input.log_incoming {
        if let Some(man_id) = &input.man_id {
            let _ = tools::execute(
                &scope,
                SecurityLevel::Yolo,
                "append_chat",
                &json!({
                    "man_id": man_id,
                    "role": "incoming",
                    "channel": input.channel.clone().unwrap_or_else(|| "chat".into()),
                    "text": input.message,
                }),
            );
        }
    }

    let system = prompts::build_system(
        &profile,
        man.as_ref(),
        mode,
        security,
        &deps.settings.global_style_rules,
    );

    let mut user_block = String::new();
    let ctx = prompts::context_block(thread.as_ref(), deps.settings.history_limit);
    if !ctx.is_empty() {
        user_block.push_str(&ctx);
        user_block.push('\n');
    }
    if let Some(channel) = &input.channel {
        user_block.push_str(&format!("Channel: {channel}\n"));
    }
    user_block.push_str("Operator input:\n");
    user_block.push_str(&input.message);

    let mut request = ChatRequest::new(system);
    request.temperature = deps.provider.temperature;
    request.max_output_tokens = deps.provider.max_output_tokens;
    request.messages.push(LlmMessage::user(user_block));

    match mode {
        AgentMode::Auto => run_auto(deps, &scope, security, mode, input, request).await,
        AgentMode::Act | AgentMode::Memorize => {
            run_single_turn(deps, &scope, security, mode, input, request).await
        }
    }
}

async fn run_auto(
    deps: &AgentDeps<'_>,
    scope: &Scope,
    security: SecurityLevel,
    mode: AgentMode,
    input: RunInput,
    mut request: ChatRequest,
) -> Result<RunOutput> {
    request.tools = tools::tool_defs();

    let mut steps: Vec<RunStep> = vec![];
    let mut pending: Vec<PendingAction> = vec![];
    let mut usage = Usage::default();
    let mut key_index = 0usize;
    let mut reply = String::new();
    let mut turns = 0usize;

    for turn in 0..deps.settings.max_tool_turns.max(1) {
        turns = turn + 1;
        let response = deps
            .llm
            .chat(deps.provider, deps.pool.clone(), &request, deps.emit)
            .await?;

        usage.prompt_tokens += response.usage.prompt_tokens;
        usage.completion_tokens += response.usage.completion_tokens;
        usage.total_tokens += response.usage.total_tokens;
        key_index = response.key_index;

        if response.tool_calls.is_empty() {
            reply = response.text;
            break;
        }

        request.messages.push(LlmMessage::assistant(
            response.text.clone(),
            response.tool_calls.clone(),
        ));
        if !response.text.trim().is_empty() {
            reply = response.text.clone();
        }

        for call in &response.tool_calls {
            let outcome = tools::execute(scope, security, &call.name, &call.args);
            let (result_json, step) = match outcome {
                Ok(ToolOutcome {
                    result,
                    summary,
                    queued,
                    applied,
                    risk,
                    tool,
                }) => {
                    if let Some(action) = queued {
                        pending.push(action);
                    }
                    let step = RunStep {
                        kind: if applied {
                            "tool".into()
                        } else {
                            "tool_pending".into()
                        },
                        tool: Some(tool.clone()),
                        summary: summary.clone(),
                        detail: json!({ "args": call.args, "risk": risk, "applied": applied }),
                    };
                    (result, step)
                }
                Err(err) => {
                    let message = err.to_string();
                    (
                        json!({ "ok": false, "error": message }),
                        RunStep {
                            kind: "tool_error".into(),
                            tool: Some(call.name.clone()),
                            summary: message,
                            detail: json!({ "args": call.args }),
                        },
                    )
                }
            };
            (deps.emit)(json!({ "kind": "step", "step": step }));
            steps.push(step);
            request
                .messages
                .push(LlmMessage::tool_result(call, result_json.to_string()));
        }
    }

    if reply.trim().is_empty() {
        reply =
            "Инструменты отработали, но модель не вернула текст ответа. Проверь шаги ниже.".into();
    }

    finish(
        scope, mode, security, input, reply, steps, pending, usage, key_index, turns,
    )
}

async fn run_single_turn(
    deps: &AgentDeps<'_>,
    scope: &Scope,
    security: SecurityLevel,
    mode: AgentMode,
    input: RunInput,
    mut request: ChatRequest,
) -> Result<RunOutput> {
    request.force_json = true;
    request.tools = vec![];

    let response = deps
        .llm
        .chat(deps.provider, deps.pool.clone(), &request, deps.emit)
        .await?;

    let parsed = extract_json_object(&response.text).ok_or_else(|| {
        AppError::Provider(format!(
            "model did not return JSON in {:?} mode: {}",
            mode,
            response.text.chars().take(300).collect::<String>()
        ))
    })?;

    let reply = match mode {
        AgentMode::Memorize => parsed
            .get("summary")
            .and_then(|s| s.as_str())
            .unwrap_or("Факты записаны.")
            .to_string(),
        _ => parsed
            .get("reply")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
    };

    let patch = parsed
        .get("memory_patch")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));

    let (steps, pending) =
        apply_patch(scope, security, input.man_id.as_deref(), &patch, deps.emit)?;

    finish(
        scope,
        mode,
        security,
        input,
        reply,
        steps,
        pending,
        response.usage,
        response.key_index,
        1,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish(
    scope: &Scope,
    mode: AgentMode,
    security: SecurityLevel,
    input: RunInput,
    reply: String,
    steps: Vec<RunStep>,
    pending: Vec<PendingAction>,
    usage: Usage,
    key_index: usize,
    turns: usize,
) -> Result<RunOutput> {
    let _ = scope.append_agent_entry(AgentEntry::new("user", input.message.clone()));
    let mut entry = AgentEntry::new("assistant", reply.clone());
    entry.meta = json!({
        "mode": mode,
        "security": security,
        "man_id": input.man_id,
        "steps": steps,
        "pending": pending.len(),
        "usage": usage,
    });
    let _ = scope.append_agent_entry(entry);

    Ok(RunOutput {
        reply,
        mode,
        security,
        model_id: input.model_id,
        man_id: input.man_id,
        steps,
        pending,
        usage,
        key_index,
        turns,
    })
}

/// Translate an ACT / MEMORIZE memory patch into ordinary tool calls so that
/// the security policy and the approval queue behave identically everywhere.
///
/// A patch may describe the selected man (its top-level fields) and other men
/// by name in `men` — dictation often mentions people who have no dossier yet,
/// and those must land somewhere instead of being dropped.
pub fn apply_patch(
    scope: &Scope,
    security: SecurityLevel,
    man_id: Option<&str>,
    patch: &Value,
    emit: &(dyn Fn(Value) + Send + Sync),
) -> Result<(Vec<RunStep>, Vec<PendingAction>)> {
    let mut steps = vec![];
    let mut pending = vec![];

    if patch.is_null() || patch.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        return Ok((steps, pending));
    }

    let known = scope.read_all_men().unwrap_or_default();
    let mut calls: Vec<(String, Value)> = vec![];

    // Men the patch names explicitly: update the ones that exist, create the
    // rest. A dossier is created together with its facts and notes in a single
    // action, so nothing here depends on a write still awaiting approval.
    if let Some(men) = patch.get("men").and_then(|m| m.as_array()) {
        for entry in men {
            calls.extend(calls_for_entry(&known, entry));
        }
    }

    // The top-level fields belong to the man the operator has open. Without one
    // they can still be attributed if the patch says whose they are.
    let about_a_man = MAN_PATCH_FIELDS.iter().any(|f| patch.get(*f).is_some());
    if about_a_man {
        match man_id {
            Some(id) => calls.extend(man_calls(id, patch)),
            None if patch.get("name").is_some() => calls.extend(calls_for_entry(&known, patch)),
            None => steps.push(RunStep {
                kind: "warn".into(),
                tool: None,
                summary: "Патч памяти пропущен: не выбран мужчина и в патче нет имени".into(),
                detail: patch.clone(),
            }),
        }
    }

    for (tool, args) in calls {
        match tools::execute(scope, security, &tool, &args) {
            Ok(outcome) => {
                if let Some(action) = outcome.queued {
                    pending.push(action);
                }
                let step = RunStep {
                    kind: if outcome.applied {
                        "patch".into()
                    } else {
                        "patch_pending".into()
                    },
                    tool: Some(tool),
                    summary: outcome.summary,
                    detail: args,
                };
                emit(json!({ "kind": "step", "step": step }));
                steps.push(step);
            }
            Err(err) => steps.push(RunStep {
                kind: "patch_error".into(),
                tool: Some(tool),
                summary: err.to_string(),
                detail: args,
            }),
        }
    }

    Ok((steps, pending))
}

/// Patch fields that only make sense for one particular man.
const MAN_PATCH_FIELDS: &[&str] = &[
    "status",
    "stage",
    "sentiment",
    "next_action",
    "location",
    "country",
    "age",
    "facts",
    "notes",
    "gifts",
    "tags",
    "triggers",
    "boundaries",
];

/// Fields copied verbatim when a dossier is created from a patch entry.
const CREATE_FIELDS: &[&str] = &[
    "name",
    "id",
    "age",
    "location",
    "country",
    "status",
    "stage",
    "sentiment",
    "next_action",
    "tags",
    "triggers",
    "boundaries",
    "facts",
    "notes",
];

/// Match a patch entry against the dossiers that already exist: by site id
/// first, then by name, so repeating a dictation does not fork the CRM.
fn resolve_man(known: &[Man], entry: &Value) -> Option<String> {
    let id = entry
        .get("man_id")
        .or_else(|| entry.get("id"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(id) = id {
        if let Some(man) = known.iter().find(|m| m.id == id) {
            return Some(man.id.clone());
        }
    }
    let name = entry.get("name").and_then(|v| v.as_str())?.trim();
    if name.is_empty() {
        return None;
    }
    known
        .iter()
        .find(|m| m.name.trim().eq_ignore_ascii_case(name))
        .map(|m| m.id.clone())
}

/// Update an existing dossier, or create it when the patch describes someone
/// new.
fn calls_for_entry(known: &[Man], entry: &Value) -> Vec<(String, Value)> {
    if let Some(id) = resolve_man(known, entry) {
        return man_calls(&id, entry);
    }
    let name = entry
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or_default();
    if name.is_empty() {
        return vec![];
    }
    let mut args = serde_json::Map::new();
    for field in CREATE_FIELDS {
        if let Some(value) = entry.get(*field) {
            if !value.is_null() {
                args.insert((*field).to_string(), value.clone());
            }
        }
    }
    vec![("create_man".to_string(), Value::Object(args))]
}

fn gift_calls(patch: &Value) -> Vec<Value> {
    let Some(gifts) = patch.get("gifts").and_then(|g| g.as_array()) else {
        return vec![];
    };
    let mut out = vec![];
    for gift in gifts {
        let title = gift.as_str().map(|s| s.to_string()).or_else(|| {
            gift.get("title")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
        });
        let Some(title) = title else { continue };
        let mut args = json!({ "title": title });
        if let Some(value) = gift.get("value") {
            args["value"] = value.clone();
        }
        if let Some(kind) = gift.get("kind") {
            args["kind"] = kind.clone();
        }
        out.push(args);
    }
    out
}

/// Tool calls that write one man's patch fields onto an existing dossier.
fn man_calls(man_id: &str, patch: &Value) -> Vec<(String, Value)> {
    let mut calls: Vec<(String, Value)> = vec![];

    let mut update = serde_json::Map::new();
    update.insert("man_id".into(), json!(man_id));
    for field in [
        "status",
        "stage",
        "sentiment",
        "next_action",
        "location",
        "country",
    ] {
        if let Some(value) = patch.get(field).and_then(|v| v.as_str()) {
            if !value.trim().is_empty() {
                update.insert(field.into(), json!(value));
            }
        }
    }
    if let Some(age) = patch.get("age").and_then(|v| v.as_u64()) {
        update.insert("age".into(), json!(age));
    }
    if update.len() > 1 {
        update.insert("touch_last_contact".into(), json!(true));
        calls.push(("update_man".into(), Value::Object(update)));
    }

    if let Some(facts) = patch.get("facts").and_then(|f| f.as_array()) {
        for fact in facts {
            let key = fact.get("key").and_then(|k| k.as_str());
            let value = fact.get("value").and_then(|v| v.as_str());
            match (key, value) {
                (Some(key), Some(value)) => calls.push((
                    "add_man_fact".into(),
                    json!({ "man_id": man_id, "key": key, "value": value }),
                )),
                _ => {
                    if let Some(text) = fact.as_str() {
                        calls.push((
                            "add_man_fact".into(),
                            json!({ "man_id": man_id, "key": "fact", "value": text }),
                        ));
                    }
                }
            }
        }
    }

    if let Some(notes) = patch.get("notes").and_then(|n| n.as_array()) {
        for note in notes {
            if let Some(text) = note
                .as_str()
                .or_else(|| note.get("text").and_then(|t| t.as_str()))
            {
                calls.push((
                    "add_man_note".into(),
                    json!({ "man_id": man_id, "text": text }),
                ));
            }
        }
    }

    for mut args in gift_calls(patch) {
        args["man_id"] = json!(man_id);
        calls.push(("add_gift".into(), args));
    }

    let mut tag_args = json!({ "man_id": man_id });
    let mut has_tags = false;
    for field in ["tags", "triggers", "boundaries"] {
        if let Some(items) = patch.get(field).and_then(|t| t.as_array()) {
            let list: Vec<&str> = items.iter().filter_map(|i| i.as_str()).collect();
            if !list.is_empty() {
                tag_args[field] = json!(list);
                has_tags = true;
            }
        }
    }
    if has_tags {
        calls.push(("add_tags".into(), tag_args));
    }

    calls
}

// ---------------------------------------------------------------------------
// Master agent
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct MasterDecision {
    pub model_id: Option<String>,
    pub man_id: Option<String>,
    pub confidence: f64,
    pub reason: String,
    pub created: Option<String>,
    pub hits: Vec<SearchHit>,
    pub steps: Vec<RunStep>,
    pub usage: Usage,
}

/// Route a raw pasted blob to a profile / dossier, creating one when needed.
pub async fn master_route(
    deps: &AgentDeps<'_>,
    raw: &str,
    auto_create: bool,
) -> Result<MasterDecision> {
    let hits = crate::storage::global_search(deps.paths, raw, 12)?;
    let index = crate::storage::load_index(deps.paths)?;

    let candidates: Vec<Value> = index
        .models
        .iter()
        .map(|m| {
            json!({
                "model_id": m.id,
                "model_name": m.name,
                "site": m.site,
                "men": m.men.iter().take(60).map(|man| json!({
                    "man_id": man.id,
                    "name": man.name,
                    "stage": man.stage,
                    "tags": man.tags,
                })).collect::<Vec<_>>()
            })
        })
        .collect();

    let mut request = ChatRequest::new(prompts::MASTER_ROUTER);
    request.force_json = true;
    request.temperature = 0.2;
    request.messages.push(LlmMessage::user(format!(
        "Candidates:\n{}\n\nKeyword pre-matches:\n{}\n\nRaw blob:\n{}",
        serde_json::to_string(&candidates)?,
        serde_json::to_string(&hits)?,
        raw
    )));

    let response = deps
        .llm
        .chat(deps.provider, deps.pool.clone(), &request, deps.emit)
        .await?;

    let parsed = extract_json_object(&response.text)
        .ok_or_else(|| AppError::Provider("master router returned no JSON".into()))?;

    let model_id = parsed
        .get("model_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty() && *s != "null")
        .map(|s| s.to_string());
    let mut man_id = parsed
        .get("man_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty() && *s != "null")
        .map(|s| s.to_string());
    let reason = parsed
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let confidence = parsed
        .get("confidence")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    let mut steps = vec![];
    let mut created = None;

    if let Some(model_id) = &model_id {
        let scope = deps.paths.scope(model_id)?;

        if man_id.is_none() && auto_create {
            if let Some(new_man) = parsed.get("create_man") {
                if let Some(name) = new_man.get("name").and_then(|n| n.as_str()) {
                    let mut args = json!({ "name": name });
                    for field in ["age", "location", "country"] {
                        if let Some(v) = new_man.get(field) {
                            args[field] = v.clone();
                        }
                    }
                    let outcome = tools::execute(&scope, SecurityLevel::Yolo, "create_man", &args)?;
                    steps.push(RunStep {
                        kind: "master_create".into(),
                        tool: Some("create_man".into()),
                        summary: outcome.summary.clone(),
                        detail: args,
                    });
                    // Re-read to learn the generated id.
                    if let Some(man) = scope
                        .read_all_men()?
                        .into_iter()
                        .find(|m| m.name.eq_ignore_ascii_case(name))
                    {
                        created = Some(man.id.clone());
                        man_id = Some(man.id);
                    }
                }
            }
        }

        if let (Some(man_id), Some(facts)) =
            (&man_id, parsed.get("facts").and_then(|f| f.as_array()))
        {
            for fact in facts {
                let (Some(key), Some(value)) = (
                    fact.get("key").and_then(|k| k.as_str()),
                    fact.get("value").and_then(|v| v.as_str()),
                ) else {
                    continue;
                };
                let args = json!({ "man_id": man_id, "key": key, "value": value });
                if let Ok(outcome) =
                    tools::execute(&scope, deps.settings.security_level, "add_man_fact", &args)
                {
                    steps.push(RunStep {
                        kind: "master_fact".into(),
                        tool: Some("add_man_fact".into()),
                        summary: outcome.summary,
                        detail: args,
                    });
                }
            }
        }

        crate::storage::rebuild_index(deps.paths)?;
    }

    Ok(MasterDecision {
        model_id,
        man_id,
        confidence,
        reason,
        created,
        hits,
        steps,
        usage: response.usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Paths;

    fn scope() -> Scope {
        let dir = std::env::temp_dir().join(format!("velvet-agent-{}", new_id()));
        let paths = Paths::new(dir).unwrap();
        let scope = paths.scope("2428653").unwrap();
        scope
            .write_profile(&Profile::new("2428653".into(), "Marina".into()))
            .unwrap();
        scope
            .write_man(&Man::new(
                "2428653".into(),
                "1219749".into(),
                "Hartwig".into(),
            ))
            .unwrap();
        scope
    }

    #[test]
    fn patch_applies_every_section() {
        let scope = scope();
        let patch = json!({
            "status": "успокоен, ждёт встречи",
            "stage": "warming",
            "facts": [{ "key": "health", "value": "epilepsy" }],
            "notes": ["предложил встречу у Шлosstor"],
            "gifts": [{ "title": "Virtual rose", "value": 9.0 }],
            "tags": ["pension"],
            "boundaries": ["не упоминать алкоголь"]
        });
        let (steps, pending) = apply_patch(
            &scope,
            SecurityLevel::Yolo,
            Some("1219749"),
            &patch,
            &|_| {},
        )
        .unwrap();
        assert!(pending.is_empty());
        assert_eq!(steps.len(), 5);

        let man = scope.read_man("1219749").unwrap();
        assert_eq!(man.stage, "warming");
        assert_eq!(man.facts.len(), 1);
        assert_eq!(man.notes.len(), 1);
        assert_eq!(man.gifts.len(), 1);
        assert_eq!(man.tags, vec!["pension".to_string()]);
        assert_eq!(man.boundaries.len(), 1);
        assert!(man.last_contact.is_some());
    }

    #[test]
    fn patch_without_target_is_skipped() {
        let scope = scope();
        let (steps, pending) = apply_patch(
            &scope,
            SecurityLevel::Safe,
            None,
            &json!({ "status": "x" }),
            &|_| {},
        )
        .unwrap();
        assert!(pending.is_empty());
        assert_eq!(steps[0].kind, "warn");
    }

    /// Dictation about men who have no dossier yet used to be dropped with a
    /// warning. Each entry now creates one, with its facts attached.
    #[test]
    fn patch_creates_dossiers_for_unknown_men() {
        let scope = scope();
        let patch = json!({
            "men": [
                {
                    "name": "Влад",
                    "id": "3786141",
                    "age": 44,
                    "location": "Гамбург",
                    "facts": [{ "key": "работа", "value": "механик" }],
                    "notes": ["написал из списка интересов"],
                    "tags": ["новый"]
                },
                { "name": "Sven" }
            ]
        });
        let (steps, pending) =
            apply_patch(&scope, SecurityLevel::Yolo, None, &patch, &|_| {}).unwrap();

        assert!(pending.is_empty());
        assert!(
            steps.iter().all(|s| s.kind == "patch"),
            "unexpected steps: {steps:?}"
        );

        let vlad = scope.read_man("3786141").unwrap();
        assert_eq!(vlad.name, "Влад");
        assert_eq!(vlad.age, Some(44));
        assert_eq!(vlad.location, "Гамбург");
        assert_eq!(vlad.facts.len(), 1);
        assert_eq!(vlad.facts[0].value, "механик");
        assert_eq!(vlad.notes.len(), 1);
        assert_eq!(vlad.tags, vec!["новый".to_string()]);

        let men = scope.read_all_men().unwrap();
        assert!(men.iter().any(|m| m.name == "Sven"));
        assert_eq!(men.len(), 3, "Hartwig plus the two new ones");
    }

    /// A second dictation about the same man must land on his dossier instead
    /// of forking a duplicate — matched by id, and by name when no id is given.
    #[test]
    fn patch_matches_existing_men_instead_of_duplicating() {
        let scope = scope();
        let patch = json!({
            "men": [
                { "name": "hartwig", "facts": [{ "key": "health", "value": "epilepsy" }] },
                { "name": "Anything", "id": "1219749", "status": "ждёт письма" }
            ]
        });
        apply_patch(&scope, SecurityLevel::Yolo, None, &patch, &|_| {}).unwrap();

        assert_eq!(scope.read_all_men().unwrap().len(), 1);
        let man = scope.read_man("1219749").unwrap();
        assert_eq!(man.name, "Hartwig", "a match must not rename him");
        assert_eq!(man.facts.len(), 1);
        assert_eq!(man.status, "ждёт письма");
    }

    /// Under ASK the whole dossier is one queued action, so approving it later
    /// cannot leave facts pointing at a man who was never written.
    #[test]
    fn creating_a_man_under_ask_is_a_single_action() {
        let scope = scope();
        let patch = json!({
            "men": [{
                "name": "Влад",
                "facts": [{ "key": "работа", "value": "механик" }],
                "notes": ["из списка интересов"]
            }]
        });
        let (_steps, pending) =
            apply_patch(&scope, SecurityLevel::Ask, None, &patch, &|_| {}).unwrap();

        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].tool, "create_man");
        assert_eq!(pending[0].after["facts"].as_array().unwrap().len(), 1);
        assert_eq!(pending[0].after["notes"].as_array().unwrap().len(), 1);
        assert_eq!(scope.read_all_men().unwrap().len(), 1);
    }

    /// Without a selected man, a patch that names whom it is about is applied
    /// rather than skipped.
    #[test]
    fn top_level_patch_with_a_name_is_attributed() {
        let scope = scope();
        let patch = json!({ "name": "Hartwig", "status": "перезвонит вечером" });
        let (steps, _) = apply_patch(&scope, SecurityLevel::Yolo, None, &patch, &|_| {}).unwrap();

        assert!(steps.iter().all(|s| s.kind != "warn"));
        assert_eq!(
            scope.read_man("1219749").unwrap().status,
            "перезвонит вечером"
        );
    }

    #[test]
    fn ask_mode_patch_queues_actions() {
        let scope = scope();
        let (_steps, pending) = apply_patch(
            &scope,
            SecurityLevel::Ask,
            Some("1219749"),
            &json!({ "notes": ["новая заметка"] }),
            &|_| {},
        )
        .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(scope.read_man("1219749").unwrap().notes.len(), 0);
    }
}
