pub mod master;
pub mod prompts;
pub mod tools;
pub mod workspace_tools;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::config::{AgentMode, ProviderConfig, SecurityLevel, Settings};
use crate::error::{AppError, Result};
use crate::llm::keypool::KeyPool;
use crate::llm::{extract_json_object, ChatRequest, LlmClient, LlmMessage, Thinking, Usage};
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
    /// Overrides the provider's thinking level for this run only — the
    /// selector next to the composer.
    #[serde(default)]
    pub thinking_effort: Option<String>,
    /// A temporary chat: the exchange is never written to the agent log, while
    /// everything it does — facts, dossiers, messages — is applied as usual.
    #[serde(default)]
    pub temporary: bool,
    /// Screenshots and photos attached to this message.
    #[serde(default)]
    pub images: Vec<crate::llm::ImagePart>,
    /// The same pictures shrunk to card size, in the same order.
    ///
    /// A photo the operator sends is usually meant for the card as well as for
    /// the model to look at, and a card holds a picture, not a megabyte of it.
    /// The interface shrinks them once; `attachment:1` in a tool call means
    /// the first of these.
    #[serde(default)]
    pub avatars: Vec<String>,
    /// The caller's name for this run, echoed back on every progress event so
    /// parallel runs can be told apart.
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunStep {
    pub kind: String,
    pub tool: Option<String>,
    /// Dictionary key for `summary`, with `params` filling its placeholders.
    /// The core never writes prose: the interface holds the wording, in
    /// whichever language it runs.
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub params: Value,
    pub summary: String,
    pub detail: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunOutput {
    pub reply: String,
    /// The provider's last answer as it arrived, for the "raw answer" view.
    #[serde(default)]
    pub raw: String,
    /// Which model answered: the chosen one, or a fallback from the chain.
    #[serde(default)]
    pub model: String,
    /// Set when the reply is the app's own words rather than the model's, so
    /// the interface can say them in its language.
    #[serde(default)]
    pub reply_key: String,
    /// The model's own summary of its reasoning, when it reports one.
    #[serde(default)]
    pub thoughts: String,
    pub mode: AgentMode,
    pub security: SecurityLevel,
    pub model_id: String,
    pub man_id: Option<String>,
    pub steps: Vec<RunStep>,
    pub pending: Vec<PendingAction>,
    pub usage: Usage,
    pub key_index: usize,
    pub turns: usize,
    /// Where this turn ended up in the log: the operator's message and the
    /// answer, under the ids they were written with.
    ///
    /// The interface builds its own bubbles while a run is in flight, and
    /// without these it would go on showing them under ids the log has never
    /// heard of — which is how deleting a message left it on screen until the
    /// chat was reopened.
    #[serde(default)]
    pub user_entry_id: String,
    #[serde(default)]
    pub entry_id: String,
}

pub struct AgentDeps<'a> {
    pub paths: &'a Paths,
    pub settings: &'a Settings,
    pub provider: &'a ProviderConfig,
    pub pool: Arc<KeyPool>,
    pub llm: &'a LlmClient,
    pub emit: &'a (dyn Fn(Value) + Send + Sync),
    /// Where an action that needs a human goes the moment it is created.
    ///
    /// A run can take a minute, and an approval that only reaches the operator
    /// when the run ends is an approval they cannot give while the agent is
    /// still waiting for it. This hands it over at once — to the queue behind
    /// the panel, and to the chat that asked for it.
    pub queue: &'a (dyn Fn(&PendingAction) + Send + Sync),
    /// Raised when the operator presses stop. Checked between turns and
    /// between tool calls, and handed to the provider so a long answer stops
    /// being written the moment it is no longer wanted.
    pub cancel: Arc<AtomicBool>,
}

/// A run nobody can stop: everything the app starts on its own behalf.
pub fn never_cancelled() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
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

    // Without an open dossier the agent is given the whole roster, so a
    // question about "the new admirers" needs no tool call to answer.
    let roster = if man.is_some() {
        Vec::new()
    } else {
        scope.read_all_men().unwrap_or_default()
    };
    let system = prompts::build_system(
        &profile,
        man.as_ref(),
        &roster,
        &deps.settings.trusted_roots,
        mode,
        security,
        &deps.settings.global_style_rules,
        &deps.settings.ui_language,
    );

    // Compact before assembling the prompt when the correspondence has grown
    // past the operator's threshold, so a long thread degrades into a summary
    // instead of a provider error.
    let mut thread = thread;
    if let Some(man_id) = input.man_id.as_deref() {
        let stats = context_stats(&scope, deps.settings, deps.provider, Some(man_id))?;
        if stats.ratio >= deps.settings.auto_compact_at && stats.live_messages > 6 {
            (deps.emit)(json!({
                "kind": "compacting",
                "used": stats.used_tokens,
                "window": stats.window_tokens,
            }));
            let _ = compact_context(deps, &scope, man_id, 6).await;
            thread = scope.read_chat(man_id).ok();
        }
    }

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
    request.thinking = thinking_for(deps.provider, input.thinking_effort.as_deref());

    // What the operator and the copilot have already said to each other. Without
    // it every message starts from nothing: "make it shorter" has no letter to
    // shorten, "as we agreed" refers to nobody. The correspondence with the man
    // is separate — that is the block below — and this is the working
    // conversation about it.
    push_operator_history(&scope, input.man_id.as_deref(), &mut request);

    request.messages.push(LlmMessage::user_with_images(
        user_block,
        input.images.clone(),
    ));

    match mode {
        AgentMode::Auto => run_auto(deps, &scope, security, mode, input, request).await,
        AgentMode::Act | AgentMode::Memorize | AgentMode::Letters => {
            run_single_turn(deps, &scope, security, mode, input, request).await
        }
    }
}

/// The thinking settings for one run: the provider's own, unless the operator
/// picked a level for this message.
fn thinking_for(provider: &ProviderConfig, override_effort: Option<&str>) -> Thinking {
    match override_effort.map(str::trim).filter(|e| !e.is_empty()) {
        // An explicit level wins over a stored budget: the two would otherwise
        // contradict each other on providers that accept both.
        Some(effort) => Thinking {
            effort: effort.to_string(),
            budget_tokens: None,
        },
        None => Thinking {
            effort: provider.thinking_effort.clone(),
            budget_tokens: provider.thinking_budget,
        },
    }
}

/// Cheap token estimate, used until the provider has been asked for a real
/// count.
///
/// A flat "quarter of the characters" is only right for English: Cyrillic runs
/// closer to two characters per token, and a prompt that is mostly Russian was
/// coming out at half its true size.
pub fn estimate_tokens(text: &str) -> usize {
    let (ascii, other) = text.chars().fold((0usize, 0usize), |(ascii, other), c| {
        if c.is_ascii() {
            (ascii + 1, other)
        } else {
            (ascii, other + 1)
        }
    });
    ascii.div_ceil(4) + other.div_ceil(2)
}

/// How much of the context window one man's correspondence is using.
#[derive(Debug, Clone, Serialize)]
pub struct ContextStats {
    pub used_tokens: usize,
    pub window_tokens: u32,
    pub ratio: f32,
    /// True when the provider counted the tokens, false when they were guessed
    /// from the text — the interface says which.
    #[serde(default)]
    pub exact: bool,
    /// Messages currently sent to the model.
    pub live_messages: usize,
    pub total_messages: usize,
    pub has_summary: bool,
}

/// Everything the next request would carry: the system prompt with the profile
/// and either the dossier or the whole roster, plus the correspondence. Not
/// just one message — the figure is meant to answer "how full is the window".
pub fn context_stats(
    scope: &Scope,
    settings: &Settings,
    provider: &ProviderConfig,
    man_id: Option<&str>,
) -> Result<ContextStats> {
    let profile = scope.read_profile()?;
    let man = man_id.and_then(|id| scope.read_man(id).ok());
    let roster = if man.is_some() {
        Vec::new()
    } else {
        scope.read_all_men().unwrap_or_default()
    };
    let thread = man_id.and_then(|id| scope.read_chat(id).ok());

    let system = prompts::build_system(
        &profile,
        man.as_ref(),
        &roster,
        &settings.trusted_roots,
        settings.agent_mode,
        settings.security_level,
        &settings.global_style_rules,
        &settings.ui_language,
    );
    let context = prompts::context_block(thread.as_ref(), settings.history_limit);
    // The declarations are only sent in AUTO, where the model calls the tools
    // itself. ACT and MEMORIZE answer with one JSON object and carry none of
    // them — counting them there overstated the prompt by half.
    let tools = if settings.agent_mode == AgentMode::Auto {
        serde_json::to_string(&all_tool_defs()).unwrap_or_default()
    } else {
        String::new()
    };

    let window = provider.context_window();
    let used = estimate_tokens(&system) + estimate_tokens(&context) + estimate_tokens(&tools);
    Ok(ContextStats {
        used_tokens: used,
        window_tokens: window,
        ratio: used as f32 / window.max(1) as f32,
        exact: false,
        live_messages: thread
            .as_ref()
            .map(|t| t.live_messages(settings.history_limit).len())
            .unwrap_or(0),
        total_messages: thread.as_ref().map(|t| t.messages.len()).unwrap_or(0),
        has_summary: thread
            .as_ref()
            .map(|t| !t.context_summary.trim().is_empty())
            .unwrap_or(false),
    })
}

/// Forget the correspondence the model has been reading, keeping every message
/// on disk and every stored fact untouched.
pub fn clear_context(scope: &Scope, man_id: &str) -> Result<ContextStats2> {
    let mut thread = scope.read_chat(man_id)?;
    thread.context_from = thread.messages.len();
    thread.context_summary.clear();
    scope.write_chat(&thread)?;
    Ok(ContextStats2 {
        dropped: thread.messages.len(),
    })
}

/// Result of a context reset: how many messages left the prompt.
#[derive(Debug, Clone, Serialize)]
pub struct ContextStats2 {
    pub dropped: usize,
}

/// Replace the older half of the correspondence with a summary the model
/// writes itself. Nothing is deleted — `context_from` just moves forward.
pub async fn compact_context(
    deps: &AgentDeps<'_>,
    scope: &Scope,
    man_id: &str,
    keep_last: usize,
) -> Result<String> {
    let mut thread = scope.read_chat(man_id)?;
    let live = thread.messages.len().saturating_sub(thread.context_from);
    if live <= keep_last {
        return Ok(thread.context_summary.clone());
    }
    let cut = thread.messages.len() - keep_last;
    let older: Vec<String> = thread.messages[thread.context_from..cut]
        .iter()
        .map(|m| {
            let who = match m.role {
                crate::models::MsgRole::Incoming => "HIM",
                crate::models::MsgRole::Outgoing => "HER",
                crate::models::MsgRole::Note => "OPERATOR-NOTE",
            };
            format!("{who}: {}", m.text)
        })
        .collect();

    let mut request = ChatRequest::new(format!(
        "{}\n\nWrite the summary in {}.",
        prompts::COMPACTOR,
        prompts::operator_language(&deps.settings.ui_language)
    ));
    request.temperature = 0.2;
    let previous = if thread.context_summary.trim().is_empty() {
        String::new()
    } else {
        format!(
            "Summary so far (fold the new material into it):\n{}\n\n",
            thread.context_summary.trim()
        )
    };
    request.messages.push(LlmMessage::user(format!(
        "{previous}Messages to compress:\n{}",
        older.join("\n")
    )));

    let response = deps
        .llm
        .chat(deps.provider, deps.pool.clone(), &request, deps.emit)
        .await?;

    thread.context_summary = response.text.trim().to_string();
    thread.context_from = cut;
    scope.write_chat(&thread)?;
    Ok(thread.context_summary.clone())
}

/// Write the digest of a correspondence without touching anything.
///
/// Summarising and deleting are two different acts, and the operator gets to
/// read the summary before the letters it replaces are gone. Nothing here is
/// written to disk: the digest comes back as text, and `apply_digest` is what
/// makes it the record.
pub async fn digest_preview(
    deps: &AgentDeps<'_>,
    model_id: &str,
    man_id: &str,
    keep_last: usize,
) -> Result<DigestPreview> {
    let scope = deps.paths.scope(model_id)?;
    let thread = scope.read_chat(man_id)?;
    let keep_last = keep_last.min(thread.messages.len());
    if thread.messages.len().saturating_sub(keep_last) < 2 {
        return Err(AppError::message("error.nothingToCompact", json!({})));
    }

    let cut = thread.messages.len() - keep_last;
    let older: Vec<String> = thread.messages[..cut]
        .iter()
        .map(|m| {
            let who = match m.role {
                crate::models::MsgRole::Incoming => "HIM",
                crate::models::MsgRole::Outgoing => "HER",
                crate::models::MsgRole::Note => "OPERATOR-NOTE",
            };
            format!("{who} ({}): {}", m.ts.format("%Y-%m-%d"), m.text)
        })
        .collect();

    let mut request = ChatRequest::new(format!(
        "{}\n\nWrite it in {}.",
        prompts::THREAD_DIGEST,
        prompts::operator_language(&deps.settings.ui_language)
    ));
    request.temperature = 0.2;
    let previous = if thread.context_summary.trim().is_empty() {
        String::new()
    } else {
        format!(
            "The digest written last time, which these messages continue:\n{}\n\n",
            thread.context_summary.trim()
        )
    };
    request.messages.push(LlmMessage::user(format!(
        "{previous}Correspondence to fold away ({} messages):\n{}",
        older.len(),
        older.join("\n")
    )));

    let response = deps
        .llm
        .chat(deps.provider, deps.pool.clone(), &request, deps.emit)
        .await?;
    let digest = response.text.trim().to_string();
    if digest.is_empty() {
        return Err(AppError::message("error.emptySummary", json!({})));
    }

    Ok(DigestPreview {
        digest,
        folding: older.len(),
        keeping: keep_last,
        usage: response.usage,
    })
}

/// A digest the operator has not accepted yet.
#[derive(Debug, Clone, Serialize)]
pub struct DigestPreview {
    pub digest: String,
    /// How many letters this would replace, and how many would stay.
    pub folding: usize,
    pub keeping: usize,
    pub usage: Usage,
}

/// Make an accepted digest the record: the letters it replaces are deleted.
///
/// A copy of the correspondence is written first, and if that copy cannot be
/// written nothing else happens — the digest is a judgement, and the operator
/// may want the letters back tomorrow.
pub fn apply_digest(
    paths: &Paths,
    model_id: &str,
    man_id: &str,
    keep_last: usize,
    digest: &str,
) -> Result<ChatThread> {
    let scope = paths.scope(model_id)?;
    let mut thread = scope.read_chat(man_id)?;
    let keep_last = keep_last.min(thread.messages.len());
    let cut = thread.messages.len() - keep_last;
    if digest.trim().is_empty() {
        return Err(AppError::message("error.emptySummary", json!({})));
    }

    scope.back_up_chat(man_id)?;
    thread.context_summary = digest.trim().to_string();
    thread.messages = thread.messages.split_off(cut);
    thread.context_from = 0;
    thread.updated_at = chrono::Utc::now();
    scope.write_chat(&thread)?;
    Ok(thread)
}

/// Learn how this woman writes, from what she has already written.
///
/// A profile carries tone rules and writing samples, and both are usually left
/// empty — nobody sits down to describe their own voice. Her outgoing letters
/// are right there, so the description is taken from them: the newest ones are
/// kept verbatim as samples, because a model copies an example far more
/// reliably than it follows a rule.
pub async fn learn_voice(
    deps: &AgentDeps<'_>,
    model_id: &str,
    sample_limit: usize,
) -> Result<Profile> {
    let scope = deps.paths.scope(model_id)?;
    let mut profile = scope.read_profile()?;

    let mut letters: Vec<(chrono::DateTime<chrono::Utc>, String)> = vec![];
    for man in scope.read_all_men()? {
        let Ok(thread) = scope.read_chat(&man.id) else {
            continue;
        };
        for message in thread.messages {
            if message.role == crate::models::MsgRole::Outgoing && !message.text.trim().is_empty() {
                letters.push((message.ts, message.text));
            }
        }
    }
    if letters.len() < 2 {
        return Err(AppError::message("error.notEnoughLetters", json!({})));
    }
    // Newest first: the way she writes now is what should be copied.
    letters.sort_by_key(|(ts, _)| std::cmp::Reverse(*ts));
    letters.truncate(sample_limit.clamp(2, 10));

    let mut request = ChatRequest::new(format!(
        "{}\n\nWrite it in {}.",
        prompts::VOICE_ANALYST,
        prompts::operator_language(&deps.settings.ui_language)
    ));
    request.temperature = 0.3;
    request.messages.push(LlmMessage::user(format!(
        "Letters written by {} ({} of them, newest first):\n\n{}",
        profile.name,
        letters.len(),
        letters
            .iter()
            .map(|(_, text)| format!("---\n{text}"))
            .collect::<Vec<_>>()
            .join("\n")
    )));

    let response = deps
        .llm
        .chat(deps.provider, deps.pool.clone(), &request, deps.emit)
        .await?;

    let rules: Vec<String> = response
        .text
        .lines()
        .map(|line| {
            line.trim_start_matches(['-', '*', '•', ' '])
                .trim()
                .to_string()
        })
        .filter(|line| !line.is_empty())
        .collect();
    if rules.is_empty() {
        return Err(AppError::message("error.emptySummary", json!({})));
    }

    profile.tone_rules = rules;
    profile.writing_samples = letters.into_iter().map(|(_, text)| text).collect();
    profile.updated_at = chrono::Utc::now();
    scope.write_profile(&profile)?;
    Ok(profile)
}

/// Fold a whole conversation into one summary.
///
/// The chat is what grows: every turn carries the ones before it. Compaction
/// hands the model everything said so far, asks for the short version, and
/// replaces the log with it — the messages themselves are gone from the prompt
/// from then on, which is the point.
pub async fn compact_chat(
    deps: &AgentDeps<'_>,
    model_id: &str,
    man_id: Option<&str>,
) -> Result<AgentLog> {
    let scope = deps.paths.scope(model_id)?;
    let log = scope.read_agent_log(man_id)?;

    // Nothing to gain from summarising two lines.
    if log.entries.len() < 4 {
        return Err(AppError::message(
            "error.nothingToCompact",
            json!({ "n": log.entries.len() }),
        ));
    }

    let transcript: String = log
        .entries
        .iter()
        .filter(|entry| !entry.text.trim().is_empty())
        .map(|entry| format!("{}: {}", entry.sender.to_uppercase(), entry.text))
        .collect::<Vec<_>>()
        .join("\n");

    let mut request = ChatRequest::new(format!(
        "{}\n\nWrite the summary in {}.",
        prompts::CHAT_COMPACTOR,
        prompts::operator_language(&deps.settings.ui_language)
    ));
    request.temperature = 0.2;
    request.stream = false;
    request.messages.push(LlmMessage::user(transcript));

    let response = deps
        .llm
        .chat(deps.provider, deps.pool.clone(), &request, deps.emit)
        .await?;

    let summary = response.text.trim().to_string();
    if summary.is_empty() {
        return Err(AppError::message("error.emptySummary", json!({})));
    }

    let mut entry = AgentEntry::new("system", summary);
    entry.meta = json!({
        "summary": true,
        "replaced": log.entries.len(),
        "usage": response.usage,
        "model": response.model,
    });

    let mut compacted = AgentLog::new(model_id.to_string(), man_id.map(str::to_string));
    compacted.entries.push(entry);
    scope.write_agent_log(&compacted)?;
    Ok(compacted)
}

/// One letter, ready for the operator to read and send.
#[derive(Debug, Clone, Serialize)]
pub struct Letter {
    pub man_id: String,
    pub name: String,
    pub text: String,
    pub usage: Usage,
    /// Set instead of `text` when this one could not be written.
    #[serde(default)]
    pub error: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LettersInput {
    pub model_id: String,
    /// Whom to write to. Empty means everyone in the profile.
    #[serde(default)]
    pub man_ids: Vec<String>,
    /// A temporary chat keeps nothing, letters included.
    #[serde(default)]
    pub temporary: bool,
    /// What the letters are about; empty lets her write what comes next.
    #[serde(default)]
    pub brief: String,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub thinking_effort: Option<String>,
    /// The caller's name for this run; see `RunInput::run_id`.
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LettersOutput {
    pub letters: Vec<Letter>,
    pub usage: Usage,
    pub key_index: usize,
}

/// Write to several men in one go, each letter in her voice and about him.
///
/// One call per man rather than one call for all of them: a batch prompt makes
/// the model average the letters together, which is exactly what an operator
/// sending a round of messages must not have.
pub async fn write_letters(deps: &AgentDeps<'_>, input: LettersInput) -> Result<LettersOutput> {
    let scope = deps.paths.scope(&input.model_id)?;
    let profile = scope.read_profile()?;

    let men: Vec<Man> = if input.man_ids.is_empty() {
        scope.read_all_men()?
    } else {
        input
            .man_ids
            .iter()
            .filter_map(|id| scope.read_man(id).ok())
            .collect()
    };
    if men.is_empty() {
        return Err(AppError::message("error.noRecipients", json!({})));
    }

    let mut letters = vec![];
    let mut usage = Usage::default();
    let mut key_index = 0usize;

    for (index, man) in men.iter().enumerate() {
        (deps.emit)(json!({
            "kind": "letter_progress",
            "done": index,
            "total": men.len(),
            "name": man.name,
        }));

        let thread = scope.read_chat(&man.id).ok();
        let system = prompts::build_system(
            &profile,
            Some(man),
            &[],
            &deps.settings.trusted_roots,
            AgentMode::Letters,
            deps.settings.security_level,
            &deps.settings.global_style_rules,
            &deps.settings.ui_language,
        );

        let mut request = ChatRequest::new(system);
        request.temperature = deps.provider.temperature;
        request.max_output_tokens = deps.provider.max_output_tokens;
        request.thinking = thinking_for(deps.provider, input.thinking_effort.as_deref());

        let mut ask = prompts::context_block(thread.as_ref(), deps.settings.history_limit);
        if let Some(channel) = &input.channel {
            ask.push_str(&format!("Channel: {channel}\n"));
        }
        ask.push_str(if input.brief.trim().is_empty() {
            "Write the next letter from her to him."
        } else {
            "What this letter is about:\n"
        });
        if !input.brief.trim().is_empty() {
            ask.push_str(input.brief.trim());
        }
        request.messages.push(LlmMessage::user(ask));

        match deps
            .llm
            .chat(deps.provider, deps.pool.clone(), &request, deps.emit)
            .await
        {
            Ok(response) => {
                usage.prompt_tokens += response.usage.prompt_tokens;
                usage.completion_tokens += response.usage.completion_tokens;
                usage.total_tokens += response.usage.total_tokens;
                key_index = response.key_index;
                letters.push(Letter {
                    man_id: man.id.clone(),
                    name: man.name.clone(),
                    text: response.text.trim().to_string(),
                    usage: response.usage,
                    error: String::new(),
                });
            }
            // One failure does not cost the operator the whole round.
            Err(err) => letters.push(Letter {
                man_id: man.id.clone(),
                name: man.name.clone(),
                text: String::new(),
                usage: Usage::default(),
                error: err.to_string(),
            }),
        }
    }

    // The letters belong to the conversation they were asked for in: writing to
    // one man files them in his chat, a round files them in the profile's.
    // Without this they lived only on screen and were gone on the next switch.
    if !input.temporary {
        let target = if input.man_ids.len() == 1 {
            input.man_ids.first().map(String::as_str)
        } else {
            None
        };

        if !input.brief.trim().is_empty() {
            let _ = scope.append_agent_entry(target, AgentEntry::new("user", input.brief.clone()));
        }
        for letter in &letters {
            let mut entry = AgentEntry::new(
                "assistant",
                if letter.error.is_empty() {
                    letter.text.clone()
                } else {
                    letter.error.clone()
                },
            );
            entry.meta = json!({
                "letter": true,
                "man_id": letter.man_id,
                "recipient": letter.name,
                "failed": !letter.error.is_empty(),
                "usage": letter.usage,
            });
            let _ = scope.append_agent_entry(target, entry);
        }
    }

    Ok(LettersOutput {
        letters,
        usage,
        key_index,
    })
}

/// The request the next scoped run would send, minus the operator's message.
///
/// Built here so the gauge measures the same thing the run sends: anything
/// that drifts between the two makes the figure a lie.
pub fn next_request(
    scope: &Scope,
    settings: &Settings,
    provider: &ProviderConfig,
    man_id: Option<&str>,
) -> Result<ChatRequest> {
    let profile = scope.read_profile()?;
    let man = man_id.and_then(|id| scope.read_man(id).ok());
    let roster = if man.is_some() {
        Vec::new()
    } else {
        scope.read_all_men().unwrap_or_default()
    };
    let thread = man_id.and_then(|id| scope.read_chat(id).ok());

    let system = prompts::build_system(
        &profile,
        man.as_ref(),
        &roster,
        &settings.trusted_roots,
        settings.agent_mode,
        settings.security_level,
        &settings.global_style_rules,
        &settings.ui_language,
    );

    let mut request = ChatRequest::new(system);
    request.temperature = provider.temperature;
    if settings.agent_mode == AgentMode::Auto {
        request.tools = all_tool_defs();
    }
    push_operator_history(scope, man_id, &mut request);
    let context = prompts::context_block(thread.as_ref(), settings.history_limit);
    if !context.is_empty() {
        request.messages.push(LlmMessage::user(context));
    }
    Ok(request)
}

/// Whether a finished turn actually finished.
///
/// Gemini says STOP when it is done and MAX_TOKENS when it hit the ceiling; a
/// stream that is cut off says nothing at all, because the event carrying the
/// reason never arrived.
fn is_cut_short(finish_reason: &str) -> bool {
    let reason = finish_reason.trim();
    reason.is_empty() || reason.eq_ignore_ascii_case("MAX_TOKENS") || reason == "length"
}

/// Put the operator's own pictures where the model asked for them.
///
/// A picture attached to a message has no address the model could name, so it
/// names its place instead — `attachment:1` — and the app swaps in the picture
/// itself. Anything else, a URL above all, is left exactly as it was written.
pub fn resolve_attachments(args: &mut Value, avatars: &[String]) {
    match args {
        Value::String(text) => {
            let Some(rest) = text.strip_prefix("attachment:") else {
                if text.trim() == "attachment" {
                    if let Some(first) = avatars.first() {
                        *text = first.clone();
                    }
                }
                return;
            };
            let index: usize = rest.trim().parse().unwrap_or(0);
            if let Some(picture) = avatars.get(index.saturating_sub(1)) {
                *text = picture.clone();
            }
        }
        Value::Array(items) => {
            for item in items {
                resolve_attachments(item, avatars);
            }
        }
        Value::Object(fields) => {
            for (_, value) in fields.iter_mut() {
                resolve_attachments(value, avatars);
            }
        }
        _ => {}
    }
}

/// What the model writes to say it has finished.
const END_MARKER: &str = "/END/";

/// What the model wraps a message to him in.
pub const DRAFT_OPEN: &str = "/DRAFT/";
pub const DRAFT_CLOSE: &str = "/END DRAFT/";

/// The part of an answer that is meant for him, when it is marked.
///
/// An answer usually says two things at once: a line to the operator about
/// what was checked, and the message itself. Filing the whole of it sent the
/// commentary to the man and kept it as an example of how she writes, which is
/// how "Досье Нейла проверила" ended up in her voice. Only what the model put
/// between the markers is the message.
pub fn draft_of(text: &str) -> Option<String> {
    let start = text.find(DRAFT_OPEN)? + DRAFT_OPEN.len();
    let rest = &text[start..];
    let end = rest.find(DRAFT_CLOSE).unwrap_or(rest.len());
    let draft = rest[..end].trim();
    (!draft.is_empty()).then(|| draft.to_string())
}

/// The same text with the markers taken out, for anything that wants it whole.
pub fn strip_draft_markers(text: &str) -> String {
    if !text.contains(DRAFT_OPEN) && !text.contains(DRAFT_CLOSE) {
        return text.to_string();
    }
    text.replace(DRAFT_OPEN, "")
        .replace(DRAFT_CLOSE, "")
        .trim()
        .to_string()
}

/// Take the end marker off an answer, saying whether it was there.
///
/// Its presence is the one reliable sign that nothing was lost on the way: a
/// stream cut in the middle ends wherever it ended, and no model writes the
/// marker before it has finished. It never reaches the operator, and never
/// reaches the man.
fn take_end_marker(text: &mut String) -> bool {
    let trimmed = text.trim_end();
    let Some(stripped) = trimmed.strip_suffix(END_MARKER) else {
        // A model that forgot the marker mid-sentence still leaves it nowhere
        // else, so a stray one anywhere is cleaned up but proves nothing.
        if text.contains(END_MARKER) {
            *text = text.replace(END_MARKER, "").trim_end().to_string();
        }
        return false;
    };
    *text = stripped.trim_end().to_string();
    true
}

/// Whether this turn needs carrying on.
///
/// A provider that ran out of output tokens says so, and that is enough. A
/// silent stop is ambiguous — some endpoints simply never send a reason — so
/// the text has to look unfinished as well, or every complete answer would
/// cost a second call to confirm it was complete.
fn was_interrupted(finish_reason: &str, text: &str) -> bool {
    let reason = finish_reason.trim();
    if reason.eq_ignore_ascii_case("MAX_TOKENS") || reason == "length" {
        return true;
    }
    reason.is_empty() && looks_unfinished(text)
}

/// A sentence that was never closed: no final punctuation, no closing quote.
fn looks_unfinished(text: &str) -> bool {
    /// What the end of a finished message looks like.
    const CLOSERS: &str = ".!?\u{2026}\"'\u{00bb})]}:\u{2014}";

    let Some(last) = text.trim_end().chars().next_back() else {
        return false;
    };
    if CLOSERS.contains(last) || last.is_numeric() {
        return false;
    }
    // An emoji ends a message as firmly as a full stop does.
    !matches!(last as u32, 0x1F300..=0x1FAFF | 0x2600..=0x27BF)
}

/// What a continuation brought back.
struct Continued {
    text: String,
    raw: String,
    turns: usize,
    still_cut: bool,
}

/// Carry on an answer the provider stopped writing.
///
/// Up to two more calls: the model is handed what it has written so far and
/// asked to continue from exactly there. More than that and a model stuck in a
/// loop would spend the operator's quota on it.
async fn continue_reply(
    deps: &AgentDeps<'_>,
    request: &mut ChatRequest,
    written: &str,
    usage: &mut Usage,
) -> Continued {
    let mut carried = Continued {
        text: String::new(),
        raw: String::new(),
        turns: 0,
        still_cut: true,
    };
    request.tools.clear();

    for _ in 0..2 {
        let so_far = format!("{written}{}", carried.text);
        request
            .messages
            .push(LlmMessage::assistant(so_far.clone(), vec![]));
        request.messages.push(LlmMessage::user(
            "Your answer was cut off mid-sentence. Continue it from exactly where it              stops, in the same language and voice. Do not repeat a single word of what              is already written, do not start again, do not explain — just carry on.",
        ));

        let Ok(response) = deps
            .llm
            .chat(deps.provider, deps.pool.clone(), request, deps.emit)
            .await
        else {
            return carried;
        };

        usage.prompt_tokens += response.usage.prompt_tokens;
        usage.completion_tokens += response.usage.completion_tokens;
        usage.total_tokens += response.usage.total_tokens;
        carried.turns += 1;
        if !carried.raw.is_empty() {
            carried.raw.push('\n');
        }
        carried.raw.push_str(&response.raw);

        let mut piece = response.text;
        let finished = take_end_marker(&mut piece);
        carried.text.push_str(&piece);

        if finished || !is_cut_short(&response.finish_reason) {
            carried.still_cut = false;
            return carried;
        }
    }
    carried
}

/// Collect one turn's payload into the run's record of what the provider said.
///
/// Held whole up to a ceiling, because a chain of four streamed turns is what
/// the operator wants to read when they open "the raw answer"; the slice that
/// goes into the chat log is cut from this at the end.
fn push_raw(raw: &mut String, turn: usize, payload: &str) {
    if payload.trim().is_empty() {
        return;
    }
    if !raw.is_empty() {
        raw.push_str(
            "

",
        );
    }
    raw.push_str(&format!(
        "--- turn {turn} ---
{payload}"
    ));
    if raw.len() > crate::llm::RAW_KEEP {
        *raw = crate::llm::keep_raw(raw);
    }
}

/// How many turns of the operator's own conversation are carried into a run.
const OPERATOR_HISTORY: usize = 20;

/// Replay the recent operator/copilot turns into a request.
///
/// A compacted chat is one summary entry followed by whatever came after it, so
/// replaying the tail of the log is also what makes `/compact` mean anything.
fn push_operator_history(scope: &Scope, man_id: Option<&str>, request: &mut ChatRequest) {
    let Ok(log) = scope.read_agent_log(man_id) else {
        return;
    };
    for entry in log.entries.iter().rev().take(OPERATOR_HISTORY).rev() {
        match entry.sender.as_str() {
            "user" => request.messages.push(LlmMessage::user(entry.text.clone())),
            "assistant" => request
                .messages
                .push(LlmMessage::assistant(entry.text.clone(), vec![])),
            // System notes are the interface talking to itself.
            _ => {}
        }
    }
}

/// Everything an AUTO run declares: the profile tools plus files and commands.
pub fn all_tool_defs() -> Vec<crate::llm::ToolDef> {
    let mut defs = tools::tool_defs();
    defs.extend(workspace_tools::tool_defs());
    defs
}

async fn run_auto(
    deps: &AgentDeps<'_>,
    scope: &Scope,
    security: SecurityLevel,
    mode: AgentMode,
    input: RunInput,
    mut request: ChatRequest,
) -> Result<RunOutput> {
    request.tools = all_tool_defs();
    request.cancel = Some(deps.cancel.clone());

    let mut steps: Vec<RunStep> = vec![];
    let mut pending: Vec<PendingAction> = vec![];
    let mut usage = Usage::default();
    let mut key_index = 0usize;
    let mut reply = String::new();
    // True once a turn that called no tools produced text: that is an answer.
    // Anything a model types *before* calling a tool is a preamble — often a
    // half-written sentence — and one of those standing in for the answer is
    // what left the operator with "We had some rough" and nothing after it.
    let mut answered = false;
    // Set when the provider stopped mid-answer and even the continuation did
    // not finish it: the operator is told rather than left guessing.
    let mut cut_short = false;
    let mut thoughts = String::new();
    let mut reply_key = String::new();
    let mut model = String::new();
    let mut raw = String::new();
    let mut turns = 0usize;
    // What has already been fetched this run, so a model that asks for the
    // same dossier three times gets the answer it already has instead of
    // spending another turn — and another few thousand tokens — on it.
    let mut fetched: std::collections::HashMap<String, Value> = std::collections::HashMap::new();

    // Set when the operator stopped the run: whatever was written by then is
    // kept — a half-written letter is still worth reading — and nothing more
    // is spent on it.
    let mut stopped = false;
    let max_turns = deps.settings.max_tool_turns.max(1);
    for turn in 0..max_turns {
        if deps.cancel.load(Ordering::Relaxed) {
            stopped = true;
            break;
        }
        turns = turn + 1;
        // On the last turn the tools are taken away: the model has had its
        // chance to look things up and now has to answer. Without this a run
        // could end on a tool call and leave the operator with no reply at all.
        if turn + 1 == max_turns {
            request.tools.clear();
        }
        let response = match deps
            .llm
            .chat(deps.provider, deps.pool.clone(), &request, deps.emit)
            .await
        {
            Ok(response) => response,
            Err(err) => {
                // Stop is not a failure: the operator asked for it.
                if deps.cancel.load(Ordering::Relaxed) {
                    stopped = true;
                    break;
                }
                return Err(err.into());
            }
        };

        usage.prompt_tokens += response.usage.prompt_tokens;
        usage.completion_tokens += response.usage.completion_tokens;
        usage.total_tokens += response.usage.total_tokens;
        key_index = response.key_index;
        model = response.model.clone();
        // Every turn of the chain, not only the last one: a run that called
        // three tools answered four times, and "the provider's answer" means
        // all four when the operator opens it.
        push_raw(&mut raw, turns, &response.raw);
        if !response.thoughts.is_empty() {
            if !thoughts.is_empty() {
                thoughts.push_str("\n\n");
            }
            thoughts.push_str(&response.thoughts);
            // A model that answers in one go reports its thinking only here,
            // so the UI is told about it even when nothing was streamed.
            (deps.emit)(json!({ "kind": "thought", "text": response.thoughts }));
        }

        if response.tool_calls.is_empty() {
            reply = response.text;
            let finished = take_end_marker(&mut reply);
            answered = !reply.trim().is_empty();

            // The stream can end in the middle of a sentence: the connection
            // closes, or the model runs into its output ceiling, and what
            // arrives is "В досье по" with no finish reason to explain it.
            // Asking it to carry on from exactly there costs one more call and
            // saves the letter. A model that signed off with the marker is
            // taken at its word and nothing more is spent on it.
            if answered && !finished && was_interrupted(&response.finish_reason, &reply) {
                let carried = continue_reply(deps, &mut request, &reply, &mut usage).await;
                turns += carried.turns;
                push_raw(&mut raw, turns, &carried.raw);
                if !carried.text.trim().is_empty() {
                    reply.push_str(&carried.text);
                }
                cut_short = carried.still_cut;
            }
            // A provider that declines an answer returns a finished turn with
            // nothing in it. Saying so beats an empty bubble the operator has
            // to guess at.
            if reply.trim().is_empty() && steps.is_empty() {
                reply_key = "chat.providerDeclined".to_string();
                reply = format!(
                    "Провайдер не вернул ответ (finish_reason: {}).",
                    if response.finish_reason.is_empty() {
                        "unknown".into()
                    } else {
                        response.finish_reason.clone()
                    }
                );
            }
            break;
        }

        request.messages.push(LlmMessage::assistant(
            response.text.clone(),
            response.tool_calls.clone(),
        ));
        // Kept only as a fallback: if every later turn fails, half a sentence
        // still beats an empty bubble — but it never ends the run on its own.
        if !response.text.trim().is_empty() {
            reply = response.text.clone();
        }

        for call in &response.tool_calls {
            if deps.cancel.load(Ordering::Relaxed) {
                stopped = true;
                break;
            }
            // A repeated read is answered from the cache. Writes are never
            // cached: asking twice to store something is a real second write.
            let signature = format!("{}:{}", call.name, call.args);
            if tools::risk_of(&call.name) == tools::Risk::Read
                && !workspace_tools::is_workspace_tool(&call.name)
            {
                if let Some(cached) = fetched.get(&signature) {
                    let step = RunStep {
                        kind: "tool_cached".into(),
                        tool: Some(call.name.clone()),
                        summary: format!("read: {} (already fetched)", call.name),
                        key: "step.readCached".into(),
                        params: json!({ "tool": call.name }),
                        detail: json!({
                            "args": call.args,
                            "result": crate::llm::cap_raw(&cached.to_string()),
                        }),
                    };
                    (deps.emit)(json!({ "kind": "step", "step": step }));
                    steps.push(step);
                    request.messages.push(LlmMessage::tool_result(
                        call,
                        json!({
                            "note": "already fetched in this run; use the earlier result",
                            "result": cached,
                        })
                        .to_string(),
                    ));
                    continue;
                }
            }

            // A picture the operator attached is named by its place in the
            // message; here it becomes the picture itself.
            let mut args = call.args.clone();
            resolve_attachments(&mut args, &input.avatars);

            // Files and shell commands live outside the profile sandbox and
            // are checked against the folders the operator has trusted.
            let outcome = if workspace_tools::is_workspace_tool(&call.name) {
                workspace_tools::execute(
                    deps.paths,
                    &deps.settings.trusted_roots,
                    security,
                    &call.name,
                    &args,
                )
            } else {
                tools::execute(scope, security, &call.name, &args)
            };
            let (result_json, step) = match outcome {
                Ok(ToolOutcome {
                    result,
                    summary,
                    phrase,
                    changes,
                    queued,
                    applied,
                    risk,
                    tool,
                }) => {
                    let waiting = queued.as_ref().map(|action| action.id.clone());
                    if let Some(action) = queued {
                        (deps.queue)(&action);
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
                        key: phrase.key,
                        params: phrase.params,
                        // Everything the operator might want to open: what was
                        // asked, what came back, and what it changed.
                        detail: json!({
                            "args": call.args,
                            "risk": risk,
                            "applied": applied,
                            // The chat shows its own approve/reject buttons for
                            // this one, so it carries the action's name.
                            "pending": waiting,
                            "result": crate::llm::cap_raw(&result.to_string()),
                            "changes": changes,
                        }),
                    };
                    (result, step)
                }
                Err(err) => {
                    let message = err.to_string();
                    // A named error carries its wording as a key, and printing
                    // it raw showed the operator the key instead of the words.
                    let (key, params) = err.phrasing();
                    (
                        json!({ "ok": false, "error": message }),
                        RunStep {
                            kind: "tool_error".into(),
                            tool: Some(call.name.clone()),
                            summary: message,
                            key,
                            params,
                            detail: json!({ "args": call.args }),
                        },
                    )
                }
            };
            (deps.emit)(json!({ "kind": "step", "step": step }));
            steps.push(step);
            if tools::risk_of(&call.name) == tools::Risk::Read
                && !workspace_tools::is_workspace_tool(&call.name)
            {
                fetched.insert(signature, result_json.clone());
            }
            request
                .messages
                .push(LlmMessage::tool_result(call, result_json.to_string()));
        }
        if stopped {
            break;
        }
    }

    // The turns ran out while the model was still calling tools. One more
    // call, with nothing left to reach for, turns what it gathered into the
    // answer the operator asked for — the run used to end in silence here.
    if !answered && !steps.is_empty() && !stopped {
        request.tools.clear();
        // A model that started writing and then reached for a tool has half a
        // letter in the conversation already. Told to answer from scratch it
        // writes a different one; told what it started, it finishes that one.
        let ask = if reply.trim().is_empty() {
            "Answer the operator now, in full, from what you have already gathered. \
             Do not call tools and do not narrate what you did."
                .to_string()
        } else {
            format!(
                "You began your answer with:\n\n{}\n\nWrite it out in full now, from the \
                 beginning, using what you have gathered. Do not call tools and do not \
                 narrate what you did.",
                reply.trim()
            )
        };
        request.messages.push(LlmMessage::user(ask));
        if let Ok(response) = deps
            .llm
            .chat(deps.provider, deps.pool.clone(), &request, deps.emit)
            .await
        {
            usage.prompt_tokens += response.usage.prompt_tokens;
            usage.completion_tokens += response.usage.completion_tokens;
            usage.total_tokens += response.usage.total_tokens;
            turns += 1;
            push_raw(&mut raw, turns, &response.raw);
            let mut text = response.text;
            take_end_marker(&mut text);
            if !text.trim().is_empty() {
                reply = text;
                answered = true;
            }
        }
    }

    if stopped {
        // Whatever was written stays; a run with nothing to show says so.
        if reply.trim().is_empty() {
            reply_key = "chat.stopped".to_string();
            reply = "Остановлено оператором.".into();
        } else {
            steps.push(RunStep {
                kind: "warn".into(),
                tool: None,
                summary: "Остановлено оператором".into(),
                key: "step.stopped".into(),
                params: json!({}),
                detail: Value::Null,
            });
        }
    } else if reply.trim().is_empty() {
        reply_key = "chat.noReplyText".to_string();
        reply = "Инструменты отработали, но модель не вернула текст ответа.".into();
    } else if cut_short {
        steps.push(RunStep {
            kind: "warn".into(),
            tool: None,
            summary: "Ответ оборван: провайдер прекратил передачу".into(),
            key: "step.replyTruncated".into(),
            params: json!({}),
            detail: Value::Null,
        });
    } else if !answered {
        // All that survived is what the model typed before it reached for a
        // tool — usually half a sentence. It is still shown, because half a
        // draft beats none, but it is labelled rather than passed off as the
        // finished answer.
        steps.push(RunStep {
            kind: "warn".into(),
            tool: None,
            summary: "Ответ оборван: модель ушла в инструменты и не дописала".into(),
            key: "step.replyCutShort".into(),
            params: json!({}),
            detail: Value::Null,
        });
    }

    finish(
        deps.paths, scope, mode, security, input, reply, reply_key, model, raw, thoughts, steps,
        pending, usage, key_index, turns,
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
    request.force_json = mode != AgentMode::Letters;
    request.tools = vec![];

    let response = deps
        .llm
        .chat(deps.provider, deps.pool.clone(), &request, deps.emit)
        .await?;

    // A letter is prose: there is no JSON to parse and nothing to write to
    // disk. The operator decides what to do with it.
    if mode == AgentMode::Letters {
        return finish(
            deps.paths,
            scope,
            mode,
            security,
            input,
            response.text,
            String::new(),
            response.model.clone(),
            response.raw.clone(),
            response.thoughts,
            vec![],
            vec![],
            response.usage,
            response.key_index,
            1,
        );
    }

    let parsed = extract_json_object(&response.text).ok_or_else(|| {
        AppError::Provider(format!(
            "model did not return JSON in {:?} mode: {}",
            mode,
            response.text.chars().take(300).collect::<String>()
        ))
    })?;

    let mut reply_key = String::new();
    let reply = match mode {
        AgentMode::Memorize => parsed
            .get("summary")
            .and_then(|s| s.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| {
                reply_key = "chat.factsStored".to_string();
                "Факты записаны.".to_string()
            }),
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

    let (steps, pending) = apply_patch(
        scope,
        security,
        input.man_id.as_deref(),
        &patch,
        deps.emit,
        deps.queue,
    )?;

    finish(
        deps.paths,
        scope,
        mode,
        security,
        input,
        reply,
        reply_key,
        response.model.clone(),
        response.raw.clone(),
        response.thoughts,
        steps,
        pending,
        response.usage,
        response.key_index,
        1,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish(
    paths: &Paths,
    scope: &Scope,
    mode: AgentMode,
    security: SecurityLevel,
    input: RunInput,
    reply: String,
    reply_key: String,
    model: String,
    raw: String,
    thoughts: String,
    steps: Vec<RunStep>,
    pending: Vec<PendingAction>,
    usage: Usage,
    key_index: usize,
    turns: usize,
) -> Result<RunOutput> {
    let mut user_entry_id = String::new();
    let mut entry_id = String::new();
    if !input.temporary {
        let man = input.man_id.as_deref();
        let asked = AgentEntry::new("user", input.message.clone());
        user_entry_id = asked.id.clone();
        let _ = scope.append_agent_entry(man, asked);
        let mut entry = AgentEntry::new("assistant", reply.clone());
        entry_id = entry.id.clone();
        // The log keeps a slice of the payload — enough to glance at — and the
        // whole of it goes beside the conversation, so "the raw answer" can
        // show the raw answer rather than its first eighty thousand characters.
        if raw.len() > crate::llm::RAW_LIMIT {
            let _ = paths.write_raw(&entry_id, &raw);
        }
        entry.meta = json!({
            "mode": mode,
            "security": security,
            "man_id": input.man_id,
            "steps": steps,
            "pending": pending.len(),
            "usage": usage,
            "thoughts": thoughts,
            "model": model,
            "raw": crate::llm::cap_raw(&raw),
            // Set when the whole payload is on disk under this message's id.
            "raw_kept": raw.len() > crate::llm::RAW_LIMIT,
            // Set when the words are the app's own rather than the model's:
            // stored in the language the core was written in, said by the
            // interface in whichever language it is running.
            "reply_key": reply_key,
        });
        let _ = scope.append_agent_entry(man, entry);
    }

    Ok(RunOutput {
        reply,
        reply_key,
        model,
        raw: crate::llm::cap_raw(&raw),
        thoughts,
        mode,
        security,
        model_id: input.model_id,
        man_id: input.man_id,
        steps,
        pending,
        usage,
        key_index,
        turns,
        user_entry_id,
        entry_id,
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
    queue: &(dyn Fn(&PendingAction) + Send + Sync),
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
            // Nothing to attribute them to. If the patch also listed men, they
            // have already been handled and the leftovers are noise; otherwise
            // the operator is told what was dropped, with the patch to look at.
            None if !calls.is_empty() => {}
            None => steps.push(RunStep {
                kind: "warn".into(),
                tool: None,
                summary: "Facts not stored: no man is selected and the patch names none".into(),
                key: "step.patchUnattributed".into(),
                params: json!({}),
                detail: patch.clone(),
            }),
        }
    }

    for (tool, args) in calls {
        match tools::execute(scope, security, &tool, &args) {
            Ok(outcome) => {
                let waiting = outcome.queued.as_ref().map(|action| action.id.clone());
                if let Some(action) = outcome.queued {
                    queue(&action);
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
                    key: outcome.phrase.key,
                    params: outcome.phrase.params,
                    detail: json!({ "args": args, "changes": outcome.changes, "pending": waiting }),
                };
                emit(json!({ "kind": "step", "step": step }));
                steps.push(step);
            }
            Err(err) => steps.push(RunStep {
                kind: "patch_error".into(),
                tool: Some(tool),
                summary: err.to_string(),
                key: String::new(),
                params: Value::Null,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// An answer says two things at once: a line to the operator, and the
    /// message itself. Only the second is filed and only the second is kept as
    /// an example of her voice.
    #[test]
    fn only_what_is_marked_is_meant_for_him() {
        let answer = "Досье проверила, пишу вдогонку.
                      /DRAFT/
                      Neil, I am still curious about your coffee answer!
                      /END DRAFT/
                      Сохранить это как пример её голоса?";
        assert_eq!(
            draft_of(answer).unwrap(),
            "Neil, I am still curious about your coffee answer!"
        );
        assert!(draft_of("no markers at all").is_none());
        assert_eq!(strip_draft_markers("/DRAFT/ hi /END DRAFT/"), "hi");
    }

    /// A photo has no address of its own, so the model names its place in the
    /// message and the app puts the picture there. Anything else is left alone.
    #[test]
    fn a_named_attachment_becomes_the_picture() {
        let pictures = vec!["data:image/jpeg;base64,AAA".to_string()];
        let mut args = json!({
            "name": "Neil",
            "avatar": "attachment:1",
            "notes": ["attachment:2", "https://example.com/p.jpg"]
        });
        resolve_attachments(&mut args, &pictures);
        assert_eq!(args["avatar"], "data:image/jpeg;base64,AAA");
        // No second picture was attached: the name stays as written rather
        // than turning into somebody else's photo.
        assert_eq!(args["notes"][0], "attachment:2");
        assert_eq!(args["notes"][1], "https://example.com/p.jpg");
        assert_eq!(args["name"], "Neil");
    }
    use crate::storage::Paths;
    use chrono::Utc;

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
            apply_patch(&scope, SecurityLevel::Yolo, None, &patch, &|_| {}, &|_| {}).unwrap();

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
        apply_patch(&scope, SecurityLevel::Yolo, None, &patch, &|_| {}, &|_| {}).unwrap();

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
            apply_patch(&scope, SecurityLevel::Ask, None, &patch, &|_| {}, &|_| {}).unwrap();

        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].tool, "create_man");
        assert_eq!(pending[0].after["facts"].as_array().unwrap().len(), 1);
        assert_eq!(pending[0].after["notes"].as_array().unwrap().len(), 1);
        assert_eq!(scope.read_all_men().unwrap().len(), 1);
    }

    /// The marker proves the answer arrived whole, and never reaches the letter.
    #[test]
    fn the_end_marker_is_taken_off_the_answer() {
        let mut whole = "Привет, Neil!

/END/"
            .to_string();
        assert!(take_end_marker(&mut whole));
        assert_eq!(whole, "Привет, Neil!");

        let mut cut = "Привет, Neil".to_string();
        assert!(!take_end_marker(&mut cut));
        assert_eq!(cut, "Привет, Neil");

        // A marker that wandered into the middle proves nothing but is still
        // not something to send to a man.
        let mut stray = "Первая часть /END/ вторая".to_string();
        assert!(!take_end_marker(&mut stray));
        assert!(!stray.contains("/END/"));
    }

    /// A silent stop only counts as an interruption when the text broke off;
    /// otherwise every finished answer would cost a call to confirm it.
    #[test]
    fn only_an_unfinished_sentence_is_carried_on() {
        assert!(was_interrupted("", "В досье по"));
        assert!(was_interrupted("MAX_TOKENS", "Готово."));
        assert!(!was_interrupted("", "Готово."));
        assert!(!was_interrupted("", "Написала ему, жду ответа!"));
        assert!(!was_interrupted("STOP", "В досье по"));
    }

    /// A turn that says nothing about why it stopped did not stop on purpose.
    #[test]
    fn a_missing_finish_reason_means_the_answer_was_cut() {
        assert!(is_cut_short(""));
        assert!(is_cut_short("   "));
        assert!(is_cut_short("MAX_TOKENS"));
        assert!(is_cut_short("max_tokens"));
        assert!(is_cut_short("length"));
        assert!(!is_cut_short("STOP"));
        assert!(!is_cut_short("stop"));
    }

    /// The provider's answer means every turn of the chain, trimmed as it grows.
    #[test]
    fn the_raw_record_keeps_every_turn() {
        let mut raw = String::new();
        push_raw(&mut raw, 1, "{\"first\":true}");
        push_raw(&mut raw, 2, "   ");
        push_raw(&mut raw, 3, "{\"third\":true}");

        assert!(raw.contains("--- turn 1 ---"));
        assert!(raw.contains("--- turn 3 ---"));
        assert!(
            !raw.contains("--- turn 2 ---"),
            "an empty payload adds nothing"
        );
        assert!(raw.contains("{\"first\":true}") && raw.contains("{\"third\":true}"));

        push_raw(&mut raw, 4, &"x".repeat(crate::llm::RAW_LIMIT + 100));
        assert!(
            raw.len() < crate::llm::RAW_LIMIT + 200,
            "it is trimmed as it grows"
        );
    }

    /// The copilot is given the conversation the operator is having with it,
    /// not only the correspondence it is about.
    #[test]
    fn the_prompt_carries_the_operators_own_chat() {
        let scope = scope();
        scope
            .append_agent_entry(None, AgentEntry::new("user", "напиши ему про рыбалку"))
            .unwrap();
        scope
            .append_agent_entry(None, AgentEntry::new("assistant", "Готово, вот письмо"))
            .unwrap();
        scope
            .append_agent_entry(None, AgentEntry::new("system", "внутренняя пометка"))
            .unwrap();

        let settings = Settings::default();
        let provider = settings.providers[0].clone();
        let request = next_request(&scope, &settings, &provider, None).unwrap();

        let said: Vec<&str> = request
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert!(said.contains(&"напиши ему про рыбалку"));
        assert!(said.contains(&"Готово, вот письмо"));
        assert!(
            !said.contains(&"внутренняя пометка"),
            "notes the interface writes to itself are not the operator's words"
        );
    }

    /// What the model is given about a dossier: the digest of the letters that
    /// were folded away, and every message still on file after it.
    #[test]
    fn the_prompt_carries_the_correspondence() {
        let mut thread = ChatThread::new("2428653".into(), "1219749".into());
        thread.context_summary = "Он писал про рыбалку и обещал фото.".into();
        for n in 0..4 {
            thread.messages.push(crate::models::ChatMessage {
                id: format!("m{n}"),
                role: if n % 2 == 0 {
                    crate::models::MsgRole::Incoming
                } else {
                    crate::models::MsgRole::Outgoing
                },
                channel: crate::models::Channel::Chat,
                text: format!("message {n}"),
                ts: Utc::now(),
            });
        }

        let block = prompts::context_block(Some(&thread), 40);
        assert!(block.contains("Он писал про рыбалку"));
        for n in 0..4 {
            assert!(
                block.contains(&format!("message {n}")),
                "message {n} is missing"
            );
        }
        assert!(block.contains("HIM: message 0"));
        assert!(block.contains("HER: message 1"));
    }

    /// A pasted roster of admirers becomes dossiers, and the stray top-level
    /// fields the model tacked on are not reported as lost work.
    #[test]
    fn a_roster_of_admirers_becomes_dossiers() {
        let scope = scope();
        let patch = json!({
            "status": "смотрю список интересов",
            "men": [
                { "name": "LANGKA", "id": "804329GDN", "age": 37, "tags": ["admirer"],
                  "notes": ["01.09.2026: I want to get to know you better!"] },
                { "name": "ERIC COX", "id": "628101GDN", "age": 36, "tags": ["admirer"] }
            ]
        });
        let (steps, _) =
            apply_patch(&scope, SecurityLevel::Yolo, None, &patch, &|_| {}, &|_| {}).unwrap();

        assert!(steps.iter().all(|s| s.kind != "warn"));
        let men = scope.read_all_men().unwrap();
        assert_eq!(men.len(), 3);
        let langka = scope.read_man("804329GDN").unwrap();
        assert_eq!(langka.name, "LANGKA");
        assert_eq!(langka.age, Some(37));
        assert_eq!(langka.notes.len(), 1);
    }

    /// Without a selected man, a patch that names whom it is about is applied
    /// rather than skipped.
    #[test]
    fn top_level_patch_with_a_name_is_attributed() {
        let scope = scope();
        let patch = json!({ "name": "Hartwig", "status": "перезвонит вечером" });
        let (steps, _) =
            apply_patch(&scope, SecurityLevel::Yolo, None, &patch, &|_| {}, &|_| {}).unwrap();

        assert!(steps.iter().all(|s| s.kind != "warn"));
        assert_eq!(
            scope.read_man("1219749").unwrap().status,
            "перезвонит вечером"
        );
    }

    fn thread_with(scope: &Scope, messages: usize) -> ChatThread {
        let mut thread = scope
            .read_chat("1219749")
            .unwrap_or_else(|_| ChatThread::new("2428653".into(), "1219749".into()));
        for i in 0..messages {
            thread.messages.push(ChatMessage {
                id: new_id(),
                role: if i % 2 == 0 {
                    MsgRole::Incoming
                } else {
                    MsgRole::Outgoing
                },
                text: format!("message number {i} about the weather in Osnabrück"),
                channel: Channel::default(),
                ts: Utc::now(),
            });
        }
        scope.write_chat(&thread).unwrap();
        thread
    }

    /// Clearing the context hides the correspondence from the model without
    /// losing a single message — that is the whole point of the command.
    #[test]
    fn clearing_context_keeps_every_message_on_disk() {
        let scope = scope();
        thread_with(&scope, 12);

        clear_context(&scope, "1219749").unwrap();

        let thread = scope.read_chat("1219749").unwrap();
        assert_eq!(thread.messages.len(), 12, "nothing may be deleted");
        assert_eq!(thread.live_messages(40).len(), 0, "nothing may be sent");
        assert!(prompts::context_block(Some(&thread), 40).is_empty());

        // A later message is visible again.
        let mut thread = thread;
        thread.messages.push(ChatMessage {
            id: new_id(),
            role: MsgRole::Incoming,
            text: "и ещё одно".into(),
            channel: Channel::default(),
            ts: Utc::now(),
        });
        assert_eq!(thread.live_messages(40).len(), 1);
    }

    /// The summary written by compaction replaces the older messages in the
    /// prompt and the tail stays verbatim.
    #[test]
    fn a_summary_stands_in_for_the_older_messages() {
        let scope = scope();
        let mut thread = thread_with(&scope, 20);
        thread.context_summary = "Он на пенсии, обещал приехать в субботу.".into();
        thread.context_from = 14;
        scope.write_chat(&thread).unwrap();

        let thread = scope.read_chat("1219749").unwrap();
        assert_eq!(thread.live_messages(40).len(), 6);

        let block = prompts::context_block(Some(&thread), 40);
        assert!(block.contains("Он на пенсии"));
        assert!(block.contains("message number 19"));
        assert!(!block.contains("message number 3"), "compacted away");
    }

    /// The history limit still applies on top of the reset point.
    #[test]
    fn history_limit_caps_the_live_window() {
        let scope = scope();
        thread_with(&scope, 30);
        let thread = scope.read_chat("1219749").unwrap();
        assert_eq!(thread.live_messages(5).len(), 5);
        assert!(thread.transcript(5).contains("message number 29"));
    }

    /// The gauge answers "how full is the window", so it has to count the
    /// system prompt and the tool declarations, not just the correspondence.
    #[test]
    fn context_stats_measure_the_whole_prompt() {
        let scope = scope();
        let settings = Settings::default();
        let provider = ProviderConfig {
            model: "gemini-2.5-flash".into(),
            ..settings.providers[0].clone()
        };

        let empty = context_stats(&scope, &settings, &provider, None).unwrap();
        assert!(
            empty.used_tokens > 500,
            "the system prompt and tools alone are bigger than that: {}",
            empty.used_tokens
        );

        thread_with(&scope, 40);
        let with_thread = context_stats(&scope, &settings, &provider, Some("1219749")).unwrap();
        assert!(
            with_thread.used_tokens > empty.used_tokens,
            "correspondence must add to the count"
        );
        assert_eq!(with_thread.window_tokens, 1_048_576);
        assert!(with_thread.ratio > 0.0 && with_thread.ratio < 1.0);
    }

    #[test]
    fn token_estimate_tracks_length() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
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
            &|_| {},
        )
        .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(scope.read_man("1219749").unwrap().notes.len(), 0);
    }
}
