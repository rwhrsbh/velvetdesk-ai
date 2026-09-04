//! The master agent: one chat that sees every profile.
//!
//! The scoped agent works inside a single model's sandbox. This one works
//! above them: it can look across the whole index, create a profile that does
//! not exist yet, and file men under whichever profile they belong to — which
//! is what an operator pasting a fresh admirer list actually needs.
//!
//! Every write still goes through the same security policy and the same
//! approval queue as the scoped tools, so "full access" means reach, not a
//! bypass.

use serde_json::{json, Value};

use super::tools::{self, PendingAction};
use super::{AgentDeps, RunStep};
use crate::config::SecurityLevel;
use crate::error::{AppError, Result};
use crate::llm::{ChatRequest, LlmMessage, ToolDef, Usage};
use crate::models::{new_numeric_id, AgentEntry, Profile};
use crate::storage::{self, Paths};

/// System prompt for the cross-profile chat.
pub const MASTER_SYSTEM: &str = "\
You are the VelvetDesk master agent. Unlike the per-profile copilot you can see
and change every profile in this installation.

Typical work: the operator pastes a raw block from a dating site — a list of
admirers, a letter, a fragment of somebody's profile — and says whose it is.
Decide which model profile it belongs to. If that profile does not exist yet,
create it. Then file each man under it: create the dossiers, keep the site ids
exactly as pasted, and record what the block says about each man.

Rules:
- Look before you write: list_profiles and search tell you what already exists.
- Match by site id first, then by name. Never create a duplicate.
- Keep names and ids verbatim, including their capitalisation.
- Ask the operator only for what you genuinely cannot infer, and ask once, in
  one short message, after you have done everything you can.
- Answer in the operator language named below, plainly, in one or two sentences. No
  summaries of your own tool calls.";

/// Tools that reach across profiles. The scoped ones are reused verbatim, with
/// `model_id` added so the master says which sandbox it is working in.
pub fn tool_defs() -> Vec<ToolDef> {
    let mut defs = vec![
        ToolDef {
            name: "list_profiles".into(),
            description: "List every model profile with its id, site and dossier count.".into(),
            parameters: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: "search_everything".into(),
            description: "Search every profile: names, dossiers, facts, notes, correspondence."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": { "query": { "type": "string", "description": "text to look for" } },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "create_profile".into(),
            description: "Create a model profile. Use only when no existing profile matches."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "her name" },
                    "id": { "type": "string", "description": "profile id on the site, optional" },
                    "age": { "type": "integer" },
                    "site": { "type": "string" },
                    "bio": { "type": "string" },
                    "languages": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["name"]
            }),
        },
    ];

    // Everything the scoped agent can do, one profile at a time.
    for mut def in tools::tool_defs() {
        def.description = format!("{} Runs inside the given profile.", def.description);
        if let Some(properties) = def.parameters["properties"].as_object_mut() {
            properties.insert(
                "model_id".into(),
                json!({ "type": "string", "description": "id of the profile to work in" }),
            );
        }
        match def.parameters["required"].as_array_mut() {
            Some(required) => required.insert(0, json!("model_id")),
            None => def.parameters["required"] = json!(["model_id"]),
        }
        defs.push(def);
    }
    // Files and shell commands are the master's too: it is the agent an
    // operator asks to go and look at something on disk.
    defs.extend(super::workspace_tools::tool_defs());
    defs
}

/// Run one master tool call.
pub fn execute(
    paths: &Paths,
    roots: &[crate::workspace::TrustedRoot],
    security: SecurityLevel,
    tool: &str,
    args: &Value,
) -> Result<tools::ToolOutcome> {
    match tool {
        "list_profiles" => {
            let index = storage::load_index(paths)?;
            let rows: Vec<Value> = index
                .models
                .iter()
                .map(|m| {
                    json!({
                        "model_id": m.id,
                        "name": m.name,
                        "site": m.site,
                        "men": m.men.len(),
                    })
                })
                .collect();
            Ok(read_outcome(tool, json!({ "profiles": rows })))
        }
        "search_everything" => {
            let query = args
                .get("query")
                .and_then(|q| q.as_str())
                .ok_or_else(|| AppError::Invalid("query is required".into()))?;
            let hits = storage::global_search(paths, query, 40)?;
            Ok(read_outcome(tool, json!({ "hits": hits })))
        }
        "create_profile" => create_profile(paths, security, args),
        _ if super::workspace_tools::is_workspace_tool(tool) => {
            super::workspace_tools::execute(paths, roots, security, tool, args)
        }
        _ => {
            let model_id = args
                .get("model_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AppError::Invalid("model_id is required".into()))?;
            let scope = paths.scope(model_id)?;
            if !scope.profile_file().exists() {
                return Err(AppError::NotFound(format!("profile {model_id}")));
            }
            tools::execute(&scope, security, tool, args)
        }
    }
}

fn read_outcome(tool: &str, result: Value) -> tools::ToolOutcome {
    tools::ToolOutcome {
        tool: tool.to_string(),
        risk: tools::Risk::Read,
        result,
        applied: true,
        queued: None,
        changes: Value::Null,
        summary: format!("read: {tool}"),
        phrase: tools::Phrase::new(
            "step.read",
            json!({ "tool": tool }),
            format!("read: {tool}"),
        ),
    }
}

/// Creating a profile is a write like any other, so it obeys the security
/// level — under ASK it waits in the queue instead of appearing silently.
fn create_profile(
    paths: &Paths,
    security: SecurityLevel,
    args: &Value,
) -> Result<tools::ToolOutcome> {
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| AppError::Invalid("name is required".into()))?;

    let id = match args.get("id").and_then(|v| v.as_str()) {
        Some(id) if storage::is_safe_id(id) => id.to_string(),
        _ => new_numeric_id(),
    };

    let scope = paths.scope(&id)?;
    if scope.profile_file().exists() {
        return Err(AppError::message(
            "error.profileExists",
            json!({ "id": id }),
        ));
    }

    let mut profile = Profile::new(id.clone(), name.to_string());
    profile.age = args.get("age").and_then(|v| v.as_u64()).map(|a| a as u32);
    profile.site = args
        .get("site")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    profile.bio = args
        .get("bio")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if let Some(languages) = args.get("languages").and_then(|v| v.as_array()) {
        let list: Vec<String> = languages
            .iter()
            .filter_map(|l| l.as_str())
            .map(str::to_string)
            .collect();
        if !list.is_empty() {
            profile.languages = list;
        }
    }

    let phrase = tools::Phrase::new(
        "step.createProfile",
        json!({ "name": name, "id": id }),
        format!("создать анкету {name} ({id})"),
    );
    let summary = phrase.text.clone();
    if !tools::is_allowed(security, tools::Risk::Write) {
        let pending = PendingAction {
            id: crate::models::new_id(),
            model_id: id,
            tool: "create_profile".into(),
            args: args.clone(),
            risk: tools::Risk::Write,
            summary: summary.clone(),
            key: phrase.key.clone(),
            params: phrase.params.clone(),
            before: Value::Null,
            after: serde_json::to_value(&profile)?,
            created_at: chrono::Utc::now(),
        };
        return Ok(tools::ToolOutcome {
            tool: "create_profile".into(),
            risk: tools::Risk::Write,
            result: json!({ "ok": true, "applied": false, "pending_approval": true }),
            applied: false,
            queued: Some(pending),
            changes: Value::Null,
            summary,
            phrase,
        });
    }

    scope.write_profile(&profile)?;
    storage::rebuild_index(paths)?;
    Ok(tools::ToolOutcome {
        tool: "create_profile".into(),
        risk: tools::Risk::Write,
        result: json!({ "ok": true, "applied": true, "model_id": profile.id }),
        applied: true,
        queued: None,
        changes: Value::Null,
        summary,
        phrase,
    })
}

/// One master turn: the same tool loop as the scoped agent, over every profile.
/// How much of the conversation is carried into each turn.
const HISTORY_TURNS: usize = 20;

/// The system prompt for a master turn: who it is, what it may touch, and
/// every profile in the installation.
fn system_prompt(deps: &AgentDeps<'_>, security: SecurityLevel) -> Result<String> {
    let index = storage::load_index(deps.paths)?;
    let roster: Vec<Value> = index
        .models
        .iter()
        .map(|m| json!({ "model_id": m.id, "name": m.name, "site": m.site, "men": m.men.len() }))
        .collect();

    let folders = if deps.settings.trusted_roots.is_empty() {
        String::new()
    } else {
        let list: Vec<String> = deps
            .settings
            .trusted_roots
            .iter()
            .map(|root| {
                format!(
                    "- {} ({})",
                    root.path,
                    if root.writable {
                        "read and write"
                    } else {
                        "read only"
                    }
                )
            })
            .collect();
        format!(
            "\n\nFolders you may use, with absolute paths:\n{}\nAnything else needs \
             request_access and a human answer.",
            list.join("\n")
        )
    };

    Ok(format!(
        "{MASTER_SYSTEM}{folders}\n\nOperator language: {}.\n\nProfiles in this installation:\n{}\n\n{}",
        super::prompts::operator_language(&deps.settings.ui_language),
        serde_json::to_string_pretty(&roster)?,
        super::prompts::security_block(security)
    ))
}

/// The request the next master turn would send, minus the operator's message.
pub fn next_request(deps: &AgentDeps<'_>) -> Result<ChatRequest> {
    let security = deps.settings.security_level;
    let mut request = ChatRequest::new(system_prompt(deps, security)?);
    request.temperature = deps.provider.temperature;
    request.tools = tool_defs();

    let log = deps.paths.master_log()?;
    for entry in log.entries.iter().rev().take(HISTORY_TURNS).rev() {
        match entry.sender.as_str() {
            "user" => request.messages.push(LlmMessage::user(entry.text.clone())),
            "assistant" => request
                .messages
                .push(LlmMessage::assistant(entry.text.clone(), vec![])),
            _ => {}
        }
    }
    Ok(request)
}

/// What the next master turn would cost, so the gauge describes this chat
/// rather than whichever profile happens to be selected behind it.
pub fn context_stats(deps: &AgentDeps<'_>) -> Result<super::ContextStats> {
    let security = deps.settings.security_level;
    let system = system_prompt(deps, security)?;
    let log = deps.paths.master_log()?;
    let history: String = log
        .entries
        .iter()
        .rev()
        .take(HISTORY_TURNS)
        .map(|entry| entry.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    let tools = serde_json::to_string(&tool_defs()).unwrap_or_default();

    let window = deps.provider.context_window();
    let used = super::estimate_tokens(&system)
        + super::estimate_tokens(&history)
        + super::estimate_tokens(&tools);
    Ok(super::ContextStats {
        used_tokens: used,
        window_tokens: window,
        ratio: used as f32 / window.max(1) as f32,
        exact: false,
        live_messages: log.entries.len().min(HISTORY_TURNS),
        total_messages: log.entries.len(),
        has_summary: false,
    })
}

pub async fn chat(deps: &AgentDeps<'_>, input: MasterInput) -> Result<MasterOutput> {
    let security = input.security.unwrap_or(deps.settings.security_level);
    let mut request = ChatRequest::new(system_prompt(deps, security)?);
    request.temperature = deps.provider.temperature;
    request.max_output_tokens = deps.provider.max_output_tokens;
    request.thinking = super::thinking_for(deps.provider, input.thinking_effort.as_deref());
    request.tools = tool_defs();

    let log = deps.paths.master_log()?;
    for entry in log.entries.iter().rev().take(HISTORY_TURNS).rev() {
        match entry.sender.as_str() {
            "user" => request.messages.push(LlmMessage::user(entry.text.clone())),
            "assistant" => request
                .messages
                .push(LlmMessage::assistant(entry.text.clone(), vec![])),
            _ => {}
        }
    }
    request.messages.push(LlmMessage::user_with_images(
        input.message.clone(),
        input.images.clone(),
    ));

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
            let (result, step) = match execute(
                deps.paths,
                &deps.settings.trusted_roots,
                security,
                &call.name,
                &call.args,
            ) {
                Ok(outcome) => {
                    if let Some(action) = outcome.queued.clone() {
                        pending.push(action);
                    }
                    let step = RunStep {
                        kind: if outcome.applied {
                            "tool".into()
                        } else {
                            "tool_pending".into()
                        },
                        tool: Some(call.name.clone()),
                        summary: outcome.summary.clone(),
                        key: outcome.phrase.key.clone(),
                        params: outcome.phrase.params.clone(),
                        detail: call.args.clone(),
                    };
                    (outcome.result, step)
                }
                Err(err) => (
                    json!({ "error": err.to_string() }),
                    RunStep {
                        kind: "tool_error".into(),
                        tool: Some(call.name.clone()),
                        summary: err.to_string(),
                        key: String::new(),
                        params: Value::Null,
                        detail: call.args.clone(),
                    },
                ),
            };
            (deps.emit)(json!({ "kind": "step", "step": step }));
            steps.push(step);
            request
                .messages
                .push(LlmMessage::tool_result(call, result.to_string()));
        }
    }

    if !input.temporary {
        let mut log = deps.paths.master_log()?;
        log.entries.push(AgentEntry::new("user", input.message));
        let mut entry = AgentEntry::new("assistant", reply.clone());
        entry.meta = json!({ "steps": steps, "usage": usage, "pending": pending.len() });
        log.entries.push(entry);
        if log.entries.len() > 400 {
            let cut = log.entries.len() - 400;
            log.entries.drain(0..cut);
        }
        deps.paths.write_master_log(&log)?;
    }

    storage::rebuild_index(deps.paths)?;

    Ok(MasterOutput {
        reply,
        steps,
        pending,
        usage,
        key_index,
        turns,
    })
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct MasterInput {
    pub message: String,
    /// Screenshots and photos attached to this message.
    #[serde(default)]
    pub images: Vec<crate::llm::ImagePart>,
    #[serde(default)]
    pub security: Option<SecurityLevel>,
    #[serde(default)]
    pub thinking_effort: Option<String>,
    #[serde(default)]
    pub temporary: bool,
    /// The caller's name for this run; see `RunInput::run_id`.
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MasterOutput {
    pub reply: String,
    pub steps: Vec<RunStep>,
    pub pending: Vec<PendingAction>,
    pub usage: Usage,
    pub key_index: usize,
    pub turns: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::new_id;

    fn paths() -> Paths {
        let dir = std::env::temp_dir().join(format!("velvet-master-{}", new_id()));
        Paths::new(dir).unwrap()
    }

    /// Every scoped tool is offered to the master too, and each one gained the
    /// profile argument — without it the master could not say where to write.
    #[test]
    fn scoped_tools_are_available_with_a_profile_argument() {
        let defs = tool_defs();
        assert!(defs.len() > tools::tool_defs().len());

        for scoped in tools::tool_defs() {
            let master = defs
                .iter()
                .find(|d| d.name == scoped.name)
                .unwrap_or_else(|| panic!("{} is missing from the master", scoped.name));
            assert!(
                master.parameters["properties"]["model_id"].is_object(),
                "{} takes no model_id",
                scoped.name
            );
            let required = master.parameters["required"].as_array().unwrap();
            assert_eq!(required[0], "model_id", "{}", scoped.name);
        }
    }

    #[test]
    fn creates_a_profile_and_then_refuses_to_duplicate_it() {
        let paths = paths();
        let args = json!({ "name": "Злата", "id": "2428999", "site": "RomanceCompass" });

        let outcome = execute(&paths, &[], SecurityLevel::Yolo, "create_profile", &args).unwrap();
        assert!(outcome.applied);
        let profile = paths.scope("2428999").unwrap().read_profile().unwrap();
        assert_eq!(profile.name, "Злата");
        assert_eq!(profile.site, "RomanceCompass");

        let again = execute(&paths, &[], SecurityLevel::Yolo, "create_profile", &args);
        assert!(again.is_err(), "a second create must not overwrite");
    }

    /// Under ASK the profile is queued rather than written, like every other
    /// mutation.
    #[test]
    fn creating_a_profile_under_ask_waits_for_approval() {
        let paths = paths();
        let outcome = execute(
            &paths,
            &[],
            SecurityLevel::Ask,
            "create_profile",
            &json!({ "name": "Злата" }),
        )
        .unwrap();

        assert!(!outcome.applied);
        let queued = outcome.queued.expect("queued action");
        assert_eq!(queued.tool, "create_profile");
        assert!(!paths
            .scope(&queued.model_id)
            .unwrap()
            .profile_file()
            .exists());
    }

    /// A scoped tool refuses to run against a profile that does not exist,
    /// rather than creating an empty sandbox for it.
    #[test]
    fn scoped_tools_need_an_existing_profile() {
        let paths = paths();
        let result = execute(
            &paths,
            &[],
            SecurityLevel::Yolo,
            "create_man",
            &json!({ "model_id": "nope", "name": "Vincent" }),
        );
        assert!(result.is_err());
    }
}
