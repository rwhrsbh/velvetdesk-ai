//! Tool surface exposed to the model, plus the mutation planner that powers
//! diff previews and the approval queue.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::SecurityLevel;
use crate::error::{AppError, Result};
use crate::llm::ToolDef;
use crate::models::*;
use crate::storage::{is_safe_id, Scope};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    /// Pure reads.
    Read,
    /// Additive or field-level updates.
    Write,
    /// Data loss or persona rewrites.
    Destructive,
}

pub fn risk_of(tool: &str) -> Risk {
    match tool {
        "list_men" | "get_man" | "get_profile" | "get_chat" | "search_scope" => Risk::Read,
        "delete_man" | "replace_profile_prompt" => Risk::Destructive,
        _ => Risk::Write,
    }
}

pub fn is_allowed(security: SecurityLevel, risk: Risk) -> bool {
    match (security, risk) {
        (_, Risk::Read) => true,
        (SecurityLevel::Yolo, _) => true,
        (SecurityLevel::Safe, Risk::Write) => true,
        (SecurityLevel::Safe, Risk::Destructive) => false,
        (SecurityLevel::Ask, _) => false,
    }
}

/// A write waiting for operator approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingAction {
    pub id: String,
    pub model_id: String,
    pub tool: String,
    pub args: Value,
    pub risk: Risk,
    pub summary: String,
    pub before: Value,
    pub after: Value,
    pub created_at: chrono::DateTime<Utc>,
}

/// What a mutating tool would write.
#[derive(Debug, Clone)]
pub enum MutTarget {
    Man(Box<Man>),
    Profile(Box<Profile>),
    Chat(Box<ChatThread>),
    DeleteMan(String),
}

#[derive(Debug, Clone)]
pub struct MutationPlan {
    pub summary: String,
    pub before: Value,
    pub after: Value,
    pub target: MutTarget,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolOutcome {
    pub tool: String,
    pub risk: Risk,
    /// JSON string handed back to the model.
    pub result: Value,
    pub applied: bool,
    pub queued: Option<PendingAction>,
    pub summary: String,
}

// ---------------------------------------------------------------------------
// Declarations
// ---------------------------------------------------------------------------

fn def(name: &str, description: &str, parameters: Value) -> ToolDef {
    ToolDef {
        name: name.to_string(),
        description: description.to_string(),
        parameters,
    }
}

fn str_prop(desc: &str) -> Value {
    json!({ "type": "string", "description": desc })
}

pub fn tool_defs() -> Vec<ToolDef> {
    vec![
        def(
            "list_men",
            "List the men in this profile's CRM with their stage and tags.",
            json!({
                "type": "object",
                "properties": { "query": str_prop("optional substring filter") },
            }),
        ),
        def(
            "get_man",
            "Read the full dossier of one man.",
            json!({
                "type": "object",
                "properties": { "man_id": str_prop("dossier id") },
                "required": ["man_id"]
            }),
        ),
        def(
            "get_profile",
            "Read the current model profile: persona, tone rules, facts.",
            json!({ "type": "object", "properties": {} }),
        ),
        def(
            "get_chat",
            "Read recent correspondence with one man.",
            json!({
                "type": "object",
                "properties": {
                    "man_id": str_prop("dossier id"),
                    "limit": { "type": "integer", "description": "how many last messages (default 30)" }
                },
                "required": ["man_id"]
            }),
        ),
        def(
            "search_scope",
            "Search dossiers, notes and facts inside this profile only.",
            json!({
                "type": "object",
                "properties": { "query": str_prop("search text") },
                "required": ["query"]
            }),
        ),
        def(
            "create_man",
            "Create a new dossier for a man.",
            json!({
                "type": "object",
                "properties": {
                    "name": str_prop("his name"),
                    "id": str_prop("site id, optional"),
                    "age": { "type": "integer" },
                    "location": str_prop("city / country"),
                    "tags": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["name"]
            }),
        ),
        def(
            "update_man",
            "Update CRM fields of a dossier. Only send fields that changed.",
            json!({
                "type": "object",
                "properties": {
                    "man_id": str_prop("dossier id"),
                    "status": str_prop("one-line status"),
                    "stage": str_prop("new|warming|attached|dating|cooled"),
                    "sentiment": str_prop("his current mood"),
                    "next_action": str_prop("what the operator should do next"),
                    "location": str_prop("city / country"),
                    "country": str_prop("country"),
                    "age": { "type": "integer" },
                    "touch_last_contact": { "type": "boolean", "description": "set last contact to now" }
                },
                "required": ["man_id"]
            }),
        ),
        def(
            "add_man_fact",
            "Store one atomic fact about him (health, family, work, plans).",
            json!({
                "type": "object",
                "properties": {
                    "man_id": str_prop("dossier id"),
                    "key": str_prop("short label, e.g. health"),
                    "value": str_prop("the fact itself")
                },
                "required": ["man_id", "key", "value"]
            }),
        ),
        def(
            "add_man_note",
            "Add an operator-visible note to a dossier.",
            json!({
                "type": "object",
                "properties": {
                    "man_id": str_prop("dossier id"),
                    "text": str_prop("note text")
                },
                "required": ["man_id", "text"]
            }),
        ),
        def(
            "add_gift",
            "Record a gift he sent.",
            json!({
                "type": "object",
                "properties": {
                    "man_id": str_prop("dossier id"),
                    "title": str_prop("what he sent"),
                    "kind": str_prop("virtual|real|money"),
                    "value": { "type": "number" },
                    "note": str_prop("context")
                },
                "required": ["man_id", "title"]
            }),
        ),
        def(
            "add_tags",
            "Add tags, triggers or boundaries to a dossier.",
            json!({
                "type": "object",
                "properties": {
                    "man_id": str_prop("dossier id"),
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "triggers": { "type": "array", "items": { "type": "string" } },
                    "boundaries": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["man_id"]
            }),
        ),
        def(
            "append_chat",
            "Append a message to the correspondence log.",
            json!({
                "type": "object",
                "properties": {
                    "man_id": str_prop("dossier id"),
                    "role": str_prop("incoming|outgoing|note"),
                    "channel": str_prop("chat|letter|note"),
                    "text": str_prop("message body")
                },
                "required": ["man_id", "role", "text"]
            }),
        ),
        def(
            "update_profile",
            "Update the model's own profile: bio, facts, tone rules, banned phrases.",
            json!({
                "type": "object",
                "properties": {
                    "bio": str_prop("short bio"),
                    "site": str_prop("dating site"),
                    "add_facts": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": { "key": { "type": "string" }, "value": { "type": "string" } }
                        }
                    },
                    "add_tone_rules": { "type": "array", "items": { "type": "string" } },
                    "add_banned_phrases": { "type": "array", "items": { "type": "string" } }
                }
            }),
        ),
        def(
            "replace_profile_prompt",
            "Overwrite the persona instruction block (destructive).",
            json!({
                "type": "object",
                "properties": { "text": str_prop("new persona instructions") },
                "required": ["text"]
            }),
        ),
        def(
            "delete_man",
            "Delete a dossier and its correspondence (destructive).",
            json!({
                "type": "object",
                "properties": { "man_id": str_prop("dossier id") },
                "required": ["man_id"]
            }),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| match v {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

fn arg_u32(args: &Value, key: &str) -> Option<u32> {
    args.get(key).and_then(|v| match v {
        Value::Number(n) => n.as_u64().map(|x| x as u32),
        Value::String(s) => s.trim().parse::<u32>().ok(),
        _ => None,
    })
}

fn arg_f64(args: &Value, key: &str) -> Option<f64> {
    args.get(key).and_then(|v| match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    })
}

fn arg_vec(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| i.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect(),
        Some(Value::String(s)) if !s.trim().is_empty() => vec![s.trim().to_string()],
        _ => vec![],
    }
}

fn require_man_id(args: &Value) -> Result<String> {
    let id =
        arg_str(args, "man_id").ok_or_else(|| AppError::Invalid("man_id is required".into()))?;
    if !is_safe_id(&id) {
        return Err(AppError::Invalid(format!("unsafe man_id: {id}")));
    }
    Ok(id)
}

fn push_unique(list: &mut Vec<String>, items: Vec<String>) -> usize {
    let mut added = 0;
    for item in items {
        if !list.iter().any(|x| x.eq_ignore_ascii_case(&item)) {
            list.push(item);
            added += 1;
        }
    }
    added
}

/// Read-only tools run immediately; mutating tools are planned first.
pub fn execute(
    scope: &Scope,
    security: SecurityLevel,
    tool: &str,
    args: &Value,
) -> Result<ToolOutcome> {
    let risk = risk_of(tool);

    if risk == Risk::Read {
        let result = execute_read(scope, tool, args)?;
        return Ok(ToolOutcome {
            tool: tool.to_string(),
            risk,
            result,
            applied: true,
            queued: None,
            summary: format!("read: {tool}"),
        });
    }

    let plan = plan_mutation(scope, tool, args)?;

    if is_allowed(security, risk) {
        commit(scope, &plan.target)?;
        Ok(ToolOutcome {
            tool: tool.to_string(),
            risk,
            result: json!({ "ok": true, "applied": true, "summary": plan.summary }),
            applied: true,
            queued: None,
            summary: plan.summary,
        })
    } else {
        let pending = PendingAction {
            id: new_id(),
            model_id: scope.model_id.clone(),
            tool: tool.to_string(),
            args: args.clone(),
            risk,
            summary: plan.summary.clone(),
            before: plan.before,
            after: plan.after,
            created_at: Utc::now(),
        };
        Ok(ToolOutcome {
            tool: tool.to_string(),
            risk,
            result: json!({
                "ok": true,
                "applied": false,
                "status": "PENDING_APPROVAL",
                "summary": plan.summary,
                "note": "queued for the operator; do not retry this call"
            }),
            applied: false,
            queued: Some(pending),
            summary: plan.summary,
        })
    }
}

fn execute_read(scope: &Scope, tool: &str, args: &Value) -> Result<Value> {
    match tool {
        "list_men" => {
            let query = arg_str(args, "query").unwrap_or_default().to_lowercase();
            let men = scope.read_all_men()?;
            let rows: Vec<Value> = men
                .iter()
                .filter(|m| query.is_empty() || m.keywords().contains(&query))
                .map(|m| {
                    json!({
                        "id": m.id,
                        "name": m.name,
                        "age": m.age,
                        "location": m.location,
                        "stage": m.stage,
                        "status": m.status,
                        "tags": m.tags,
                        "gifts": m.gifts.len(),
                        "last_contact": m.last_contact,
                    })
                })
                .collect();
            Ok(json!({ "count": rows.len(), "men": rows }))
        }
        "get_man" => {
            let id = require_man_id(args)?;
            let man = scope.read_man(&id)?;
            Ok(json!({ "dossier": man.dossier(), "raw": man }))
        }
        "get_profile" => {
            let profile = scope.read_profile()?;
            Ok(json!({ "persona": profile.persona_block(), "raw": profile }))
        }
        "get_chat" => {
            let id = require_man_id(args)?;
            let limit = arg_u32(args, "limit").unwrap_or(30) as usize;
            let thread = scope.read_chat(&id)?;
            Ok(json!({
                "man_id": id,
                "messages": thread.messages.len(),
                "transcript": thread.transcript(limit),
            }))
        }
        "search_scope" => {
            let query = arg_str(args, "query")
                .ok_or_else(|| AppError::Invalid("query is required".into()))?
                .to_lowercase();
            let men = scope.read_all_men()?;
            let hits: Vec<Value> = men
                .iter()
                .filter(|m| m.keywords().contains(&query))
                .map(|m| json!({ "id": m.id, "name": m.name, "status": m.status, "tags": m.tags }))
                .collect();
            Ok(json!({ "count": hits.len(), "hits": hits }))
        }
        other => Err(AppError::Invalid(format!("unknown read tool: {other}"))),
    }
}

/// Compute what a mutating tool would change, without writing anything.
pub fn plan_mutation(scope: &Scope, tool: &str, args: &Value) -> Result<MutationPlan> {
    match tool {
        "create_man" => {
            let name = arg_str(args, "name")
                .ok_or_else(|| AppError::Invalid("name is required".into()))?;
            let id = match arg_str(args, "id") {
                Some(id) if is_safe_id(&id) => id,
                _ => new_numeric_id(),
            };
            if scope.man_file(&id)?.exists() {
                return Err(AppError::Invalid(format!("dossier {id} already exists")));
            }
            let mut man = Man::new(scope.model_id.clone(), id.clone(), name.clone());
            man.age = arg_u32(args, "age");
            man.location = arg_str(args, "location").unwrap_or_default();
            man.country = arg_str(args, "country").unwrap_or_default();
            man.tags = arg_vec(args, "tags");
            man.avatar = arg_str(args, "avatar").unwrap_or_default();
            man.stage = arg_str(args, "stage").unwrap_or_else(|| "new".into());
            man.next_action = arg_str(args, "next_action").unwrap_or_default();
            man.triggers = arg_vec(args, "triggers");
            man.boundaries = arg_vec(args, "boundaries");
            man.status = arg_str(args, "status").unwrap_or_else(|| "Новый контакт".into());
            Ok(MutationPlan {
                summary: format!("создать досье {name} ({id})"),
                before: Value::Null,
                after: serde_json::to_value(&man)?,
                target: MutTarget::Man(Box::new(man)),
            })
        }
        "update_man" => {
            let id = require_man_id(args)?;
            let before = scope.read_man(&id)?;
            let mut man = before.clone();
            let mut changed: Vec<String> = vec![];
            for (key, field) in [
                ("status", 0),
                ("stage", 1),
                ("sentiment", 2),
                ("next_action", 3),
                ("location", 4),
                ("country", 5),
            ] {
                if let Some(value) = arg_str(args, key) {
                    match field {
                        0 => man.status = value.clone(),
                        1 => man.stage = value.clone(),
                        2 => man.sentiment = value.clone(),
                        3 => man.next_action = value.clone(),
                        4 => man.location = value.clone(),
                        _ => man.country = value.clone(),
                    }
                    changed.push(format!("{key}={value}"));
                }
            }
            if let Some(age) = arg_u32(args, "age") {
                man.age = Some(age);
                changed.push(format!("age={age}"));
            }
            if args
                .get("touch_last_contact")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                man.last_contact = Some(Utc::now());
                changed.push("last_contact=now".into());
            }
            if changed.is_empty() {
                return Err(AppError::Invalid(
                    "update_man called without changes".into(),
                ));
            }
            man.updated_at = Utc::now();
            Ok(MutationPlan {
                summary: format!("обновить {} — {}", man.name, changed.join(", ")),
                before: serde_json::to_value(&before)?,
                after: serde_json::to_value(&man)?,
                target: MutTarget::Man(Box::new(man)),
            })
        }
        "add_man_fact" => {
            let id = require_man_id(args)?;
            let key =
                arg_str(args, "key").ok_or_else(|| AppError::Invalid("key is required".into()))?;
            let value = arg_str(args, "value")
                .ok_or_else(|| AppError::Invalid("value is required".into()))?;
            let before = scope.read_man(&id)?;
            let mut man = before.clone();
            if let Some(existing) = man
                .facts
                .iter_mut()
                .find(|f| f.key.eq_ignore_ascii_case(&key))
            {
                existing.value = value.clone();
            } else {
                man.facts.push(Fact {
                    id: new_id(),
                    key: key.clone(),
                    value: value.clone(),
                    source: "agent".into(),
                    created_at: Utc::now(),
                });
            }
            man.updated_at = Utc::now();
            Ok(MutationPlan {
                summary: format!("факт {} → {}: {}", man.name, key, value),
                before: serde_json::to_value(&before)?,
                after: serde_json::to_value(&man)?,
                target: MutTarget::Man(Box::new(man)),
            })
        }
        "add_man_note" => {
            let id = require_man_id(args)?;
            let text = arg_str(args, "text")
                .ok_or_else(|| AppError::Invalid("text is required".into()))?;
            let before = scope.read_man(&id)?;
            let mut man = before.clone();
            man.notes.push(Note {
                id: new_id(),
                text: text.clone(),
                author: "agent".into(),
                created_at: Utc::now(),
            });
            man.updated_at = Utc::now();
            Ok(MutationPlan {
                summary: format!("заметка {}: {}", man.name, truncate(&text, 60)),
                before: serde_json::to_value(&before)?,
                after: serde_json::to_value(&man)?,
                target: MutTarget::Man(Box::new(man)),
            })
        }
        "add_gift" => {
            let id = require_man_id(args)?;
            let title = arg_str(args, "title")
                .ok_or_else(|| AppError::Invalid("title is required".into()))?;
            let before = scope.read_man(&id)?;
            let mut man = before.clone();
            man.gifts.push(Gift {
                id: new_id(),
                title: title.clone(),
                kind: arg_str(args, "kind").unwrap_or_else(|| "virtual".into()),
                value: arg_f64(args, "value"),
                note: arg_str(args, "note").unwrap_or_default(),
                date: Utc::now(),
            });
            man.updated_at = Utc::now();
            Ok(MutationPlan {
                summary: format!("подарок {} → {}", man.name, title),
                before: serde_json::to_value(&before)?,
                after: serde_json::to_value(&man)?,
                target: MutTarget::Man(Box::new(man)),
            })
        }
        "add_tags" => {
            let id = require_man_id(args)?;
            let before = scope.read_man(&id)?;
            let mut man = before.clone();
            let mut added = 0;
            added += push_unique(&mut man.tags, arg_vec(args, "tags"));
            added += push_unique(&mut man.triggers, arg_vec(args, "triggers"));
            added += push_unique(&mut man.boundaries, arg_vec(args, "boundaries"));
            if added == 0 {
                return Err(AppError::Invalid("add_tags added nothing new".into()));
            }
            man.updated_at = Utc::now();
            Ok(MutationPlan {
                summary: format!("метки {} (+{added})", man.name),
                before: serde_json::to_value(&before)?,
                after: serde_json::to_value(&man)?,
                target: MutTarget::Man(Box::new(man)),
            })
        }
        "append_chat" => {
            let id = require_man_id(args)?;
            let text = arg_str(args, "text")
                .ok_or_else(|| AppError::Invalid("text is required".into()))?;
            let role = match arg_str(args, "role").unwrap_or_default().as_str() {
                "incoming" | "him" | "his" => MsgRole::Incoming,
                "note" => MsgRole::Note,
                _ => MsgRole::Outgoing,
            };
            let channel = match arg_str(args, "channel").unwrap_or_default().as_str() {
                "letter" | "mail" => Channel::Letter,
                "note" => Channel::Note,
                _ => Channel::Chat,
            };
            let before = scope.read_chat(&id)?;
            let mut thread = before.clone();
            thread.messages.push(ChatMessage {
                id: new_id(),
                role,
                channel,
                text: text.clone(),
                ts: Utc::now(),
            });
            thread.updated_at = Utc::now();
            Ok(MutationPlan {
                summary: format!("в переписку {id}: {}", truncate(&text, 60)),
                before: json!({ "messages": before.messages.len() }),
                after: json!({ "messages": thread.messages.len(), "added": text }),
                target: MutTarget::Chat(Box::new(thread)),
            })
        }
        "update_profile" => {
            let before = scope.read_profile()?;
            let mut profile = before.clone();
            let mut changed = vec![];
            if let Some(bio) = arg_str(args, "bio") {
                profile.bio = bio;
                changed.push("bio");
            }
            if let Some(site) = arg_str(args, "site") {
                profile.site = site;
                changed.push("site");
            }
            if let Some(Value::Array(facts)) = args.get("add_facts") {
                for f in facts {
                    let (Some(key), Some(value)) = (
                        f.get("key").and_then(|k| k.as_str()),
                        f.get("value").and_then(|v| v.as_str()),
                    ) else {
                        continue;
                    };
                    profile.facts.push(Fact {
                        id: new_id(),
                        key: key.to_string(),
                        value: value.to_string(),
                        source: "agent".into(),
                        created_at: Utc::now(),
                    });
                    changed.push("facts");
                }
            }
            if push_unique(&mut profile.tone_rules, arg_vec(args, "add_tone_rules")) > 0 {
                changed.push("tone_rules");
            }
            if push_unique(
                &mut profile.banned_phrases,
                arg_vec(args, "add_banned_phrases"),
            ) > 0
            {
                changed.push("banned_phrases");
            }
            if changed.is_empty() {
                return Err(AppError::Invalid(
                    "update_profile called without changes".into(),
                ));
            }
            profile.updated_at = Utc::now();
            Ok(MutationPlan {
                summary: format!("профиль {} — {}", profile.name, changed.join(", ")),
                before: serde_json::to_value(&before)?,
                after: serde_json::to_value(&profile)?,
                target: MutTarget::Profile(Box::new(profile)),
            })
        }
        "replace_profile_prompt" => {
            let text = arg_str(args, "text")
                .ok_or_else(|| AppError::Invalid("text is required".into()))?;
            let before = scope.read_profile()?;
            let mut profile = before.clone();
            profile.system_prompt_override = text.clone();
            profile.updated_at = Utc::now();
            Ok(MutationPlan {
                summary: format!("переписать персону {}", profile.name),
                before: json!({ "system_prompt_override": before.system_prompt_override }),
                after: json!({ "system_prompt_override": text }),
                target: MutTarget::Profile(Box::new(profile)),
            })
        }
        "delete_man" => {
            let id = require_man_id(args)?;
            let before = scope.read_man(&id)?;
            Ok(MutationPlan {
                summary: format!("УДАЛИТЬ досье {} ({})", before.name, id),
                before: serde_json::to_value(&before)?,
                after: Value::Null,
                target: MutTarget::DeleteMan(id),
            })
        }
        other => Err(AppError::Invalid(format!("unknown tool: {other}"))),
    }
}

pub fn commit(scope: &Scope, target: &MutTarget) -> Result<()> {
    match target {
        MutTarget::Man(man) => scope.write_man(man),
        MutTarget::Profile(profile) => scope.write_profile(profile),
        MutTarget::Chat(thread) => scope.write_chat(thread),
        MutTarget::DeleteMan(id) => scope.delete_man(id),
    }
}

fn truncate(text: &str, max: usize) -> String {
    let clean = text.replace('\n', " ");
    if clean.chars().count() <= max {
        return clean;
    }
    let head: String = clean.chars().take(max).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Paths;

    fn scope() -> Scope {
        let dir = std::env::temp_dir().join(format!("velvet-tools-{}", new_id()));
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
    fn safe_mode_applies_writes_but_queues_deletes() {
        let scope = scope();
        let out = execute(
            &scope,
            SecurityLevel::Safe,
            "add_man_note",
            &json!({ "man_id": "1219749", "text": "любит походы" }),
        )
        .unwrap();
        assert!(out.applied);
        assert_eq!(scope.read_man("1219749").unwrap().notes.len(), 1);

        let del = execute(
            &scope,
            SecurityLevel::Safe,
            "delete_man",
            &json!({ "man_id": "1219749" }),
        )
        .unwrap();
        assert!(!del.applied);
        assert!(del.queued.is_some());
        assert!(scope.read_man("1219749").is_ok(), "delete must not run");
    }

    #[test]
    fn ask_mode_queues_everything() {
        let scope = scope();
        let out = execute(
            &scope,
            SecurityLevel::Ask,
            "add_gift",
            &json!({ "man_id": "1219749", "title": "Rose" }),
        )
        .unwrap();
        assert!(!out.applied);
        assert_eq!(scope.read_man("1219749").unwrap().gifts.len(), 0);
    }

    #[test]
    fn yolo_deletes() {
        let scope = scope();
        let out = execute(
            &scope,
            SecurityLevel::Yolo,
            "delete_man",
            &json!({ "man_id": "1219749" }),
        )
        .unwrap();
        assert!(out.applied);
        assert!(scope.read_man("1219749").is_err());
    }

    #[test]
    fn facts_are_upserted_by_key() {
        let scope = scope();
        for value in ["epilepsy", "epilepsy, no alcohol"] {
            execute(
                &scope,
                SecurityLevel::Yolo,
                "add_man_fact",
                &json!({ "man_id": "1219749", "key": "health", "value": value }),
            )
            .unwrap();
        }
        let man = scope.read_man("1219749").unwrap();
        assert_eq!(man.facts.len(), 1);
        assert_eq!(man.facts[0].value, "epilepsy, no alcohol");
    }

    #[test]
    fn read_tools_never_need_approval() {
        let scope = scope();
        let out = execute(&scope, SecurityLevel::Ask, "list_men", &json!({})).unwrap();
        assert!(out.applied);
        assert_eq!(out.result["count"], 1);
    }
}
