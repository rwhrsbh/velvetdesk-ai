use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SCHEMA_VERSION: u32 = 1;

fn now() -> DateTime<Utc> {
    Utc::now()
}

fn schema_version() -> u32 {
    SCHEMA_VERSION
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Numeric-looking id used for models and men when the operator does not supply one.
pub fn new_numeric_id() -> String {
    let raw = uuid::Uuid::new_v4().as_u128();
    format!("{}", 1_000_000 + (raw % 9_000_000) as u64)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    #[default]
    Chat,
    Letter,
    Note,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MsgRole {
    /// Message written by the man.
    Incoming,
    /// Message sent on behalf of the model.
    Outgoing,
    /// Operator side note, never sent out.
    Note,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fact {
    #[serde(default = "new_id")]
    pub id: String,
    pub key: String,
    pub value: String,
    #[serde(default)]
    pub source: String,
    #[serde(default = "now")]
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    #[serde(default = "new_id")]
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub author: String,
    #[serde(default = "now")]
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gift {
    #[serde(default = "new_id")]
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub value: Option<f64>,
    #[serde(default)]
    pub note: String,
    #[serde(default = "now")]
    pub date: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub age: Option<u32>,
    #[serde(default)]
    pub site: String,
    #[serde(default)]
    pub avatar: String,
    #[serde(default)]
    pub bio: String,
    /// Extra system prompt appended to the base persona prompt.
    #[serde(default)]
    pub system_prompt_override: String,
    #[serde(default)]
    pub tone_rules: Vec<String>,
    /// Letters she has written, or the operator's idea of how she writes.
    /// Rules describe a voice; examples are the voice, and a model copies one
    /// far more reliably than it follows the other.
    #[serde(default)]
    pub writing_samples: Vec<String>,
    #[serde(default)]
    pub banned_phrases: Vec<String>,
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default)]
    pub facts: Vec<Fact>,
    #[serde(default = "now")]
    pub created_at: DateTime<Utc>,
    #[serde(default = "now")]
    pub updated_at: DateTime<Utc>,
    #[serde(default = "schema_version")]
    pub schema_version: u32,
}

impl Profile {
    pub fn new(id: String, name: String) -> Self {
        Profile {
            id,
            name,
            age: None,
            site: String::new(),
            avatar: String::new(),
            bio: String::new(),
            system_prompt_override: String::new(),
            tone_rules: vec![],
            writing_samples: vec![],
            banned_phrases: vec![],
            languages: vec!["en".into()],
            facts: vec![],
            created_at: now(),
            updated_at: now(),
            schema_version: SCHEMA_VERSION,
        }
    }

    /// Compact persona block injected into every prompt for this profile.
    pub fn persona_block(&self) -> String {
        let mut out = format!("MODEL PROFILE: {}", self.name);
        if let Some(age) = self.age {
            out.push_str(&format!(", {} y.o.", age));
        }
        if !self.site.is_empty() {
            out.push_str(&format!(" (site: {})", self.site));
        }
        out.push('\n');
        if !self.bio.is_empty() {
            out.push_str(&format!("bio: {}\n", self.bio));
        }
        if !self.languages.is_empty() {
            out.push_str(&format!("languages: {}\n", self.languages.join(", ")));
        }
        if !self.facts.is_empty() {
            out.push_str("profile facts:\n");
            for f in self.facts.iter().rev().take(40) {
                out.push_str(&format!("- {}: {}\n", f.key, f.value));
            }
        }
        if !self.tone_rules.is_empty() {
            out.push_str("tone rules:\n");
            for r in &self.tone_rules {
                out.push_str(&format!("- {}\n", r));
            }
        }
        if !self.writing_samples.is_empty() {
            out.push_str("how she writes — her own letters, copy this voice:\n");
            for sample in self.writing_samples.iter().take(5) {
                out.push_str(&format!("---\n{}\n", sample.trim()));
            }
            out.push_str("---\n");
        }
        if !self.banned_phrases.is_empty() {
            out.push_str(&format!(
                "never write these phrases: {}\n",
                self.banned_phrases.join(" | ")
            ));
        }
        if !self.system_prompt_override.is_empty() {
            out.push_str(&format!(
                "operator instructions: {}\n",
                self.system_prompt_override
            ));
        }
        out
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Man {
    pub id: String,
    pub model_id: String,
    pub name: String,
    #[serde(default)]
    pub age: Option<u32>,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub country: String,
    #[serde(default)]
    pub avatar: String,
    /// Free-form one-liner shown in the CRM rail.
    #[serde(default)]
    pub status: String,
    /// Relationship stage: new / warming / attached / dating / cooled.
    #[serde(default)]
    pub stage: String,
    #[serde(default)]
    pub sentiment: String,
    #[serde(default)]
    pub next_action: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub triggers: Vec<String>,
    #[serde(default)]
    pub boundaries: Vec<String>,
    #[serde(default)]
    pub gifts: Vec<Gift>,
    #[serde(default)]
    pub facts: Vec<Fact>,
    #[serde(default)]
    pub notes: Vec<Note>,
    #[serde(default)]
    pub last_contact: Option<DateTime<Utc>>,
    #[serde(default = "now")]
    pub created_at: DateTime<Utc>,
    #[serde(default = "now")]
    pub updated_at: DateTime<Utc>,
    #[serde(default = "schema_version")]
    pub schema_version: u32,
}

impl Man {
    pub fn new(model_id: String, id: String, name: String) -> Self {
        Man {
            id,
            model_id,
            name,
            age: None,
            location: String::new(),
            country: String::new(),
            avatar: String::new(),
            status: String::new(),
            stage: "new".into(),
            sentiment: String::new(),
            next_action: String::new(),
            tags: vec![],
            triggers: vec![],
            boundaries: vec![],
            gifts: vec![],
            facts: vec![],
            notes: vec![],
            last_contact: None,
            created_at: now(),
            updated_at: now(),
            schema_version: SCHEMA_VERSION,
        }
    }

    /// Compact dossier used inside LLM prompts (token-cheap).
    pub fn dossier(&self) -> String {
        let mut out = format!("TARGET: {} (id {})", self.name, self.id);
        if let Some(age) = self.age {
            out.push_str(&format!(", {} y.o.", age));
        }
        if !self.location.is_empty() {
            out.push_str(&format!(", {}", self.location));
        }
        out.push('\n');
        if !self.stage.is_empty() {
            out.push_str(&format!("stage: {}\n", self.stage));
        }
        if !self.status.is_empty() {
            out.push_str(&format!("status: {}\n", self.status));
        }
        if !self.sentiment.is_empty() {
            out.push_str(&format!("sentiment: {}\n", self.sentiment));
        }
        if !self.tags.is_empty() {
            out.push_str(&format!("tags: {}\n", self.tags.join(", ")));
        }
        if !self.triggers.is_empty() {
            out.push_str(&format!("triggers: {}\n", self.triggers.join("; ")));
        }
        if !self.boundaries.is_empty() {
            out.push_str(&format!("avoid: {}\n", self.boundaries.join("; ")));
        }
        if !self.facts.is_empty() {
            out.push_str("facts:\n");
            for f in self.facts.iter().rev().take(40) {
                out.push_str(&format!("- {}: {}\n", f.key, f.value));
            }
        }
        if !self.gifts.is_empty() {
            let list: Vec<String> = self.gifts.iter().map(|g| g.title.clone()).collect();
            out.push_str(&format!("gifts received: {}\n", list.join(", ")));
        }
        if !self.notes.is_empty() {
            out.push_str("operator notes:\n");
            for n in self.notes.iter().rev().take(15) {
                out.push_str(&format!("- {}\n", n.text));
            }
        }
        if !self.next_action.is_empty() {
            out.push_str(&format!("planned next action: {}\n", self.next_action));
        }
        out
    }

    pub fn keywords(&self) -> String {
        let mut parts = vec![
            self.name.clone(),
            self.id.clone(),
            self.location.clone(),
            self.country.clone(),
            self.status.clone(),
            self.stage.clone(),
        ];
        parts.extend(self.tags.clone());
        parts.extend(self.facts.iter().map(|f| format!("{} {}", f.key, f.value)));
        parts.extend(self.notes.iter().map(|n| n.text.clone()));
        parts.extend(self.gifts.iter().map(|g| g.title.clone()));
        parts.retain(|p| !p.is_empty());
        parts.join(" ").to_lowercase()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    #[serde(default = "new_id")]
    pub id: String,
    pub role: MsgRole,
    #[serde(default)]
    pub channel: Channel,
    pub text: String,
    #[serde(default = "now")]
    pub ts: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatThread {
    pub model_id: String,
    pub man_id: String,
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
    /// Everything before `context_from` boiled down to a few lines. Written by
    /// compaction; nothing is deleted, the older messages simply stop being
    /// sent to the model.
    #[serde(default)]
    pub context_summary: String,
    /// Index of the first message that still goes into the prompt.
    #[serde(default)]
    pub context_from: usize,
    #[serde(default = "now")]
    pub updated_at: DateTime<Utc>,
    #[serde(default = "schema_version")]
    pub schema_version: u32,
}

impl ChatThread {
    pub fn new(model_id: String, man_id: String) -> Self {
        ChatThread {
            model_id,
            man_id,
            messages: vec![],
            context_summary: String::new(),
            context_from: 0,
            updated_at: now(),
            schema_version: SCHEMA_VERSION,
        }
    }

    /// Messages the model is allowed to see: everything after the last
    /// context reset, capped at `limit`.
    pub fn live_messages(&self, limit: usize) -> &[ChatMessage] {
        let from = self.context_from.min(self.messages.len());
        let live = &self.messages[from..];
        let start = live.len().saturating_sub(limit);
        &live[start..]
    }

    pub fn transcript(&self, limit: usize) -> String {
        self.live_messages(limit)
            .iter()
            .map(|m| {
                let who = match m.role {
                    MsgRole::Incoming => "HIM",
                    MsgRole::Outgoing => "HER",
                    MsgRole::Note => "OPERATOR-NOTE",
                };
                format!("{}: {}", who, m.text)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Operator <-> copilot conversation, stored per model profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentLog {
    pub model_id: String,
    /// `None` for the profile-wide conversation.
    #[serde(default)]
    pub man_id: Option<String>,
    #[serde(default)]
    pub entries: Vec<AgentEntry>,
    #[serde(default = "schema_version")]
    pub schema_version: u32,
}

impl AgentLog {
    pub fn new(model_id: String, man_id: Option<String>) -> Self {
        AgentLog {
            model_id,
            man_id,
            entries: vec![],
            schema_version: SCHEMA_VERSION,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEntry {
    #[serde(default = "new_id")]
    pub id: String,
    /// user | assistant | system | tool
    pub sender: String,
    pub text: String,
    #[serde(default)]
    pub meta: Value,
    #[serde(default = "now")]
    pub ts: DateTime<Utc>,
}

impl AgentEntry {
    pub fn new(sender: &str, text: impl Into<String>) -> Self {
        AgentEntry {
            id: new_id(),
            sender: sender.to_string(),
            text: text.into(),
            meta: Value::Null,
            ts: now(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexMan {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub stage: String,
    #[serde(default)]
    pub keywords: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexModel {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub site: String,
    #[serde(default)]
    pub avatar: String,
    #[serde(default)]
    pub men: Vec<IndexMan>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalIndex {
    #[serde(default)]
    pub models: Vec<IndexModel>,
    #[serde(default = "now")]
    pub updated_at: DateTime<Utc>,
    #[serde(default = "schema_version")]
    pub schema_version: u32,
}

impl Default for GlobalIndex {
    fn default() -> Self {
        GlobalIndex {
            models: vec![],
            updated_at: now(),
            schema_version: SCHEMA_VERSION,
        }
    }
}

/// Search hit produced by the master agent / global search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub model_id: String,
    pub model_name: String,
    pub man_id: String,
    pub man_name: String,
    pub snippet: String,
    pub score: u32,
}
