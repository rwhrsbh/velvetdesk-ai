//! System prompts. Kept in one place so operators can audit exactly what the
//! model is told before it writes anything on their behalf.

use crate::config::{AgentMode, SecurityLevel};
use crate::models::{ChatThread, Man, Profile};

pub const BASE_RULES: &str = "\
You are VelvetDesk, an operator copilot for a dating-agency workspace.
You write messages and letters on behalf of ONE woman's profile and you maintain
a private CRM about the men she talks to.

Hard rules:
- Write as the woman, in her voice, in the language of the man's last message.
- No AI tells: never open with 'I hope this message finds you', never say
  'as an AI', no bullet lists in a love letter, no corporate politeness,
  no repeated sign-offs, no em-dash-heavy rhythm, no emoji spam.
- Never invent verifiable facts (jobs, cities, family, dates, money). If a fact
  is missing from the dossier, stay vague or ask him.
- Never promise money, never ask for money, never send links or contact details.
- Reuse the dossier: names, hobbies, health issues, kids, past gifts, planned
  meetings. Continuity is the product.
- Length matches the channel: chat replies are 1-4 sentences, letters are
  3-6 short paragraphs.
- Every concrete new fact he reveals must be stored through the memory tools or
  the memory patch. Storing facts is not optional.";

pub fn security_block(level: SecurityLevel) -> &'static str {
    match level {
        SecurityLevel::Ask => {
            "\
Operator security level: ASK. Every write is queued for human approval before it
touches disk. Still call the tools normally — a queued call is reported back to
you as PENDING_APPROVAL and must not be retried."
        }
        SecurityLevel::Safe => {
            "\
Operator security level: SAFE. Additive writes (notes, facts, gifts, tags,
status updates) apply immediately. Deletions and prompt rewrites are queued for
human approval and come back as PENDING_APPROVAL."
        }
        SecurityLevel::Yolo => {
            "\
Operator security level: FULL ACCESS. All writes inside this profile's sandbox
apply immediately. The sandbox still blocks every other profile's data."
        }
    }
}

pub fn mode_block(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::Auto => {
            "\
Mode: AUTO. Decide yourself which tools to call: read the dossier and history
before answering, then persist what changed. Finish with the drafted reply as
plain text — no JSON, no preamble, no explanation of what you did."
        }
        AgentMode::Act => {
            "\
Mode: ACT (single call, minimum tokens). Do NOT call tools. Answer with ONE JSON
object and nothing else:
{
  \"reply\": \"the message to send, in his language\",
  \"memory_patch\": {
    \"status\": \"one-line CRM status, optional\",
    \"stage\": \"new|warming|attached|dating|cooled, optional\",
    \"sentiment\": \"optional\",
    \"next_action\": \"optional\",
    \"facts\": [{\"key\": \"health\", \"value\": \"epilepsy, avoids alcohol\"}],
    \"notes\": [\"operator-visible note\"],
    \"gifts\": [{\"title\": \"Virtual rose\", \"value\": 12.5}],
    \"tags\": [\"pension\"],
    \"triggers\": [\"talks warmly about his dog\"],
    \"boundaries\": [\"never mention his ex-wife\"]
  }
}
The top-level patch fields describe the man currently open. Anything about
somebody else — including a man who has no dossier yet — goes into
\"men\": [{\"name\": \"...\", \"id\": \"site id if known\", \"age\": 0,
\"location\": \"...\", \"facts\": [...], \"notes\": [...], \"tags\": [...]}].
Entries in \"men\" are matched by id, then by name; unknown ones are created.
Omit any patch field you have nothing new for. Never fabricate facts to fill it."
        }
        AgentMode::Memorize => {
            "\
Mode: MEMORIZE. The operator is dictating raw facts. Produce NO outgoing
message. Answer with ONE JSON object and nothing else:
{
  \"summary\": \"one short line describing what you stored, in Russian\",
  \"memory_patch\": { ...same shape as ACT, without \\\"reply\\\"... }
}
Split dictation into atomic facts. Keep the operator's wording for names,
numbers and dates. Do not guess anything that was not said.
If the dictation is about men other than the one currently open — a list of new
admirers, for instance — put each of them in \"men\" with his name, and never
drop a fact because no dossier exists yet: an entry in \"men\" creates one."
        }
    }
}

/// Full system prompt for a scoped model-agent run.
pub fn build_system(
    profile: &Profile,
    man: Option<&Man>,
    roster: &[Man],
    mode: AgentMode,
    security: SecurityLevel,
    global_rules: &str,
) -> String {
    let mut out = String::with_capacity(2048);
    out.push_str(BASE_RULES);
    out.push_str("\n\n");
    out.push_str(&profile.persona_block());
    out.push('\n');
    if let Some(man) = man {
        out.push_str(&man.dossier());
        out.push('\n');
    } else {
        // The profile-wide chat: no single target, so the agent gets the CRM
        // itself and can answer about anyone without a round of tool calls.
        out.push_str(&roster_block(roster));
    }
    out.push_str(&format!(
        "Storage sandbox: profiles/{}/ — you cannot read or write any other profile.\n\n",
        profile.id
    ));
    out.push_str(mode_block(mode));
    out.push_str("\n\n");
    out.push_str(security_block(security));
    if !global_rules.trim().is_empty() {
        out.push_str("\n\nHouse rules from the operator:\n");
        out.push_str(global_rules.trim());
    }
    out
}

/// Everyone in this profile's CRM, one line each. Enough to reason about the
/// roster; `get_man` still fetches the full dossier when it matters.
fn roster_block(roster: &[Man]) -> String {
    if roster.is_empty() {
        return "This profile has no dossiers yet. Create one with create_man when the operator \
                describes a man.\n\n"
            .to_string();
    }
    let mut out =
        String::from("No single man is selected — this is the profile-wide chat. The CRM holds:\n");
    for man in roster.iter().take(120) {
        let mut line = format!("- {} (id {})", man.name, man.id);
        if let Some(age) = man.age {
            line.push_str(&format!(", {age}"));
        }
        if !man.location.is_empty() {
            line.push_str(&format!(", {}", man.location));
        }
        if !man.stage.is_empty() {
            line.push_str(&format!(" · stage {}", man.stage));
        }
        if !man.status.is_empty() {
            line.push_str(&format!(" · {}", man.status));
        }
        if !man.tags.is_empty() {
            line.push_str(&format!(" · {}", man.tags.join(", ")));
        }
        out.push_str(&line);
        out.push('\n');
    }
    if roster.len() > 120 {
        out.push_str(&format!(
            "…and {} more, use list_men.\n",
            roster.len() - 120
        ));
    }
    out.push_str("Read a full dossier with get_man before writing to him.\n\n");
    out
}

/// Conversation context appended to the operator's request.
pub fn context_block(thread: Option<&ChatThread>, limit: usize) -> String {
    let Some(thread) = thread else {
        return String::new();
    };
    let mut block = String::new();
    if !thread.context_summary.trim().is_empty() {
        block.push_str(&format!(
            "Earlier correspondence, summarised:\n{}\n\n",
            thread.context_summary.trim()
        ));
    }
    let transcript = thread.transcript(limit);
    if !transcript.is_empty() {
        block.push_str(&format!(
            "Recent correspondence (oldest first):\n{transcript}\n"
        ));
    }
    block
}

/// Asks for the summary that replaces the messages dropped by compaction.
pub const COMPACTOR: &str = "\
You compress a dating-agency correspondence so the copilot can keep working
with a fraction of the tokens. Preserve, in Russian, as terse bullet lines:
who he is and what he wants, agreed facts and dates, gifts and money, promises
made by either side, open questions, and the current tone of the relationship.
Drop small talk, greetings and anything already obvious from the dossier.
Never invent anything. Answer with the summary only — no preface, no JSON.";

/// Master agent: routes a raw pasted blob to the right profile / man.
pub const MASTER_ROUTER: &str = "\
You are the VelvetDesk master router. You receive a raw blob pasted by the
operator (a letter, a profile snippet, a note) plus a list of candidate models
and men from the global index.

Answer with ONE JSON object:
{
  \"model_id\": \"best matching model id or null\",
  \"man_id\": \"best matching man id or null\",
  \"confidence\": 0.0-1.0,
  \"create_man\": { \"name\": \"...\", \"age\": 0, \"location\": \"...\" },
  \"reason\": \"one short line in Russian\",
  \"facts\": [{\"key\": \"...\", \"value\": \"...\"}]
}
Use create_man only when no existing dossier matches and the blob clearly
describes a new man. Never guess an id that is not in the candidate list.";
