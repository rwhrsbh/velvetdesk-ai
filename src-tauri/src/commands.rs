use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, State};

use crate::agent::tools::{self, PendingAction};
use crate::agent::{self, AgentDeps, RunInput, RunOutput};
use crate::config::Settings;
use crate::doctor::{self, DoctorReport};
use crate::entitlement;
use crate::error::{AppError, Result};
use crate::llm::keypool::KeyStatus;
use crate::models::*;
use crate::state::AppState;
use crate::storage;

pub const AGENT_EVENT: &str = "velvetdesk://agent";

/// Progress events, stamped with the run they came from.
///
/// Several runs can be in flight at once — one per chat — and the interface has
/// to know whose delta, step or retry it is looking at. The id is the caller's:
/// it labels its own run and recognises the events coming back.
/// Hand an action that needs a human straight to the queue and to the chat.
///
/// The panel and the bubble both learn about it while the run is still going,
/// which is the only time approving it does the run any good.
fn queueing<'a>(
    app: &'a AppHandle,
    state: &'a AppState,
    run: Option<String>,
) -> impl Fn(&tools::PendingAction) + Send + Sync + 'a {
    move |action: &tools::PendingAction| {
        state.pending.write().push(action.clone());
        let mut payload = json!({ "kind": "pending", "action": action });
        if let (Some(run), Some(fields)) = (run.as_deref(), payload.as_object_mut()) {
            fields.insert("run".into(), json!(run));
        }
        let _ = app.emit(AGENT_EVENT, payload);
    }
}

fn emitter(app: &AppHandle, run: Option<String>) -> impl Fn(Value) + Send + Sync + '_ {
    move |payload: Value| {
        let mut payload = payload;
        if let (Some(run), Some(fields)) = (run.as_deref(), payload.as_object_mut()) {
            fields.insert("run".into(), json!(run));
        }
        let _ = app.emit(AGENT_EVENT, payload);
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AppInfo {
    pub version: String,
    pub data_dir: String,
    pub platform: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Bootstrap {
    pub info: AppInfo,
    pub settings: Settings,
    pub profiles: Vec<Profile>,
    pub index: GlobalIndex,
    pub pending: Vec<PendingAction>,
}

#[tauri::command]
pub fn bootstrap(state: State<'_, AppState>) -> Result<Bootstrap> {
    let profiles = read_profiles(&state)?;
    Ok(Bootstrap {
        info: AppInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            data_dir: state.paths.root.display().to_string(),
            platform: std::env::consts::OS.to_string(),
        },
        settings: state.settings_view(),
        index: storage::load_index(&state.paths)?,
        profiles,
        pending: state.pending.read().clone(),
    })
}

fn read_profiles(state: &State<'_, AppState>) -> Result<Vec<Profile>> {
    let mut out = vec![];
    for id in state.paths.list_model_ids()? {
        if let Ok(profile) = state.paths.scope(&id)?.read_profile() {
            out.push(profile);
        }
    }
    // Whatever order the operator dragged the rail into, then by name for
    // everything they have not moved.
    out.sort_by(|a, b| {
        a.sort_order
            .cmp(&b.sort_order)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(out)
}

/// The provider's answer to one message, whole.
///
/// The chat log keeps a slice of it; this is the rest, kept beside the
/// conversation and swept away once a couple of hundred newer ones exist.
#[tauri::command]
pub fn read_raw(state: State<'_, AppState>, entry_id: String) -> Result<Option<String>> {
    state.paths.read_raw(&entry_id)
}

/// Store the order the operator dragged the profiles into.
///
/// The list is the new order, front to back; anything missing from it keeps
/// its place at the end. Nothing else about the card is touched — this is a
/// rearrangement, not an edit, so `updated_at` stays where it was.
#[tauri::command]
pub fn reorder_profiles(state: State<'_, AppState>, ids: Vec<String>) -> Result<Vec<Profile>> {
    for (index, id) in ids.iter().enumerate() {
        let scope = state.paths.scope(id)?;
        let Ok(mut profile) = scope.read_profile() else {
            continue;
        };
        profile.sort_order = index as i32;
        scope.write_profile(&profile)?;
    }
    storage::rebuild_index(&state.paths)?;
    read_profiles(&state)
}

/// The same for the dossiers of one profile.
#[tauri::command]
pub fn reorder_men(
    state: State<'_, AppState>,
    model_id: String,
    ids: Vec<String>,
) -> Result<Vec<Man>> {
    let scope = state.paths.scope(&model_id)?;
    for (index, id) in ids.iter().enumerate() {
        let Ok(mut man) = scope.read_man(id) else {
            continue;
        };
        man.sort_order = index as i32;
        scope.write_man(&man)?;
    }
    storage::rebuild_index(&state.paths)?;
    scope.read_all_men()
}

// ---------------------------------------------------------------------------
// Profiles
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn list_profiles(state: State<'_, AppState>) -> Result<Vec<Profile>> {
    read_profiles(&state)
}

#[derive(Debug, Deserialize)]
pub struct NewProfile {
    pub name: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub age: Option<u32>,
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub bio: Option<String>,
    #[serde(default)]
    pub system_prompt_override: Option<String>,
    #[serde(default)]
    pub languages: Option<Vec<String>>,
    #[serde(default)]
    pub tone_rules: Option<Vec<String>>,
    #[serde(default)]
    pub writing_samples: Option<Vec<String>>,
    #[serde(default)]
    pub banned_phrases: Option<Vec<String>>,
}

#[tauri::command]
pub fn create_profile(state: State<'_, AppState>, input: NewProfile) -> Result<Profile> {
    if input.name.trim().is_empty() {
        return Err(AppError::message("error.profileNameRequired", json!({})));
    }
    let id = match input.id.filter(|i| storage::is_safe_id(i)) {
        Some(id) => id,
        None => new_numeric_id(),
    };
    entitlement::check_profiles(storage::load_index(&state.paths)?.models.len())?;
    let scope = state.paths.scope(&id)?;
    if scope.profile_file().exists() {
        return Err(AppError::message(
            "error.profileExists",
            json!({ "id": id }),
        ));
    }
    let mut profile = Profile::new(id, input.name.trim().to_string());
    profile.age = input.age;
    profile.site = input.site.unwrap_or_default();
    profile.avatar = input.avatar.unwrap_or_default();
    profile.bio = input.bio.unwrap_or_default();
    profile.system_prompt_override = input.system_prompt_override.unwrap_or_default();
    if let Some(languages) = input.languages {
        if !languages.is_empty() {
            profile.languages = languages;
        }
    }
    profile.tone_rules = input.tone_rules.unwrap_or_default();
    profile.writing_samples = input.writing_samples.unwrap_or_default();
    profile.banned_phrases = input.banned_phrases.unwrap_or_default();
    scope.write_profile(&profile)?;
    storage::rebuild_index(&state.paths)?;
    Ok(profile)
}

#[tauri::command]
pub fn get_profile(state: State<'_, AppState>, model_id: String) -> Result<Profile> {
    state.paths.scope(&model_id)?.read_profile()
}

#[tauri::command]
pub fn save_profile(state: State<'_, AppState>, profile: Profile) -> Result<Profile> {
    let scope = state.paths.scope(&profile.id)?;
    let mut next = profile;
    next.updated_at = chrono::Utc::now();
    scope.write_profile(&next)?;
    storage::rebuild_index(&state.paths)?;
    Ok(next)
}

#[tauri::command]
pub fn delete_profile(state: State<'_, AppState>, model_id: String) -> Result<()> {
    if !storage::is_safe_id(&model_id) {
        return Err(AppError::Scope(format!("unsafe model id: {model_id}")));
    }
    let dir = state.paths.profiles_dir().join(&model_id);
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    storage::rebuild_index(&state.paths)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Men CRM
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn list_men(state: State<'_, AppState>, model_id: String) -> Result<Vec<Man>> {
    state.paths.scope(&model_id)?.read_all_men()
}

#[tauri::command]
pub fn get_man(state: State<'_, AppState>, model_id: String, man_id: String) -> Result<Man> {
    state.paths.scope(&model_id)?.read_man(&man_id)
}

#[tauri::command]
pub fn save_man(state: State<'_, AppState>, man: Man) -> Result<Man> {
    let scope = state.paths.scope(&man.model_id)?;
    let mut next = man;
    next.updated_at = chrono::Utc::now();
    scope.write_man(&next)?;
    storage::rebuild_index(&state.paths)?;
    Ok(next)
}

#[tauri::command]
pub fn create_man(state: State<'_, AppState>, model_id: String, args: Value) -> Result<Man> {
    let scope = state.paths.scope(&model_id)?;
    let plan = tools::plan_mutation(&scope, "create_man", &args)?;
    tools::commit(&scope, &plan.target)?;
    storage::rebuild_index(&state.paths)?;
    serde_json::from_value::<Man>(plan.after).map_err(AppError::Json)
}

#[tauri::command]
pub fn delete_man(state: State<'_, AppState>, model_id: String, man_id: String) -> Result<()> {
    state.paths.scope(&model_id)?.delete_man(&man_id)?;
    storage::rebuild_index(&state.paths)?;
    Ok(())
}

#[tauri::command]
pub fn get_chat(
    state: State<'_, AppState>,
    model_id: String,
    man_id: String,
) -> Result<ChatThread> {
    state.paths.scope(&model_id)?.read_chat(&man_id)
}

#[derive(Debug, Deserialize)]
pub struct NewMessage {
    pub model_id: String,
    pub man_id: String,
    pub role: String,
    #[serde(default)]
    pub channel: Option<String>,
    pub text: String,
}

#[tauri::command]
pub fn append_message(state: State<'_, AppState>, input: NewMessage) -> Result<ChatThread> {
    let scope = state.paths.scope(&input.model_id)?;
    let args = json!({
        "man_id": input.man_id,
        "role": input.role,
        "channel": input.channel.unwrap_or_else(|| "chat".into()),
        "text": input.text,
    });
    let plan = tools::plan_mutation(&scope, "append_chat", &args)?;
    tools::commit(&scope, &plan.target)?;
    scope.read_chat(&input.man_id)
}

/// The conversation for one dossier, or the profile-wide one when `man_id` is
/// absent — each dossier is its own chat.
#[tauri::command]
pub fn get_agent_log(
    state: State<'_, AppState>,
    model_id: String,
    man_id: Option<String>,
) -> Result<AgentLog> {
    state
        .paths
        .scope(&model_id)?
        .read_agent_log(man_id.as_deref())
}

#[tauri::command]
pub fn clear_agent_log(
    state: State<'_, AppState>,
    model_id: String,
    man_id: Option<String>,
) -> Result<()> {
    let scope = state.paths.scope(&model_id)?;
    scope.write_agent_log(&AgentLog::new(model_id, man_id))
}

/// Where the releases live. The check is read-only and needs no credentials.
const RELEASES_URL: &str = "https://api.github.com/repos/rwhrsbh/velvetdesk-ai/releases/latest";

#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    /// The version of the newest release, without the leading `v`.
    pub version: String,
    /// The version running right now.
    pub current: String,
    /// True when the release is newer than what is running.
    pub newer: bool,
    /// What the release says about itself.
    pub notes: String,
    /// The release page, and the file for this platform when there is one.
    pub page: String,
    pub download: Option<String>,
}

/// Compare two dotted versions the way people read them: 0.2.10 beats 0.2.9.
fn newer_than(candidate: &str, current: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v')
            .split(['.', '-'])
            .map(|piece| piece.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (a, b) = (parts(candidate), parts(current));
    for index in 0..a.len().max(b.len()) {
        let left = a.get(index).copied().unwrap_or(0);
        let right = b.get(index).copied().unwrap_or(0);
        if left != right {
            return left > right;
        }
    }
    false
}

/// The installer for the machine this is running on, out of a release's files.
fn asset_for_platform(assets: &[Value]) -> Option<String> {
    let wanted: &[&str] = match std::env::consts::OS {
        "windows" => &[".msi", ".exe"],
        "macos" => &[".dmg", ".app.tar.gz"],
        // The release carries one APK per architecture plus a universal one;
        // the universal build runs everywhere, so it is what a phone is offered
        // unless its own architecture is named in the file.
        "android" => &["universal-release.apk", ".apk"],
        _ => &[".AppImage", ".deb", ".rpm"],
    };
    for suffix in wanted {
        for asset in assets {
            let name = asset["name"].as_str().unwrap_or("");
            if name.ends_with(suffix) {
                return asset["browser_download_url"].as_str().map(str::to_string);
            }
        }
    }
    None
}

/// Ask the release page whether there is something newer.
///
/// Nothing is downloaded here: the answer is shown to the operator, and only
/// their click opens the installer's download in their browser.
#[tauri::command]
pub async fn check_update(state: State<'_, AppState>) -> Result<UpdateInfo> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let response = state
        .llm
        .http
        .get(RELEASES_URL)
        .header("accept", "application/vnd.github+json")
        .header("user-agent", format!("velvetdesk/{current}"))
        .send()
        .await
        .map_err(|e| AppError::Provider(format!("update check failed: {e}")))?;
    if !response.status().is_success() {
        return Err(AppError::Provider(format!(
            "update check failed: HTTP {}",
            response.status()
        )));
    }
    let release: Value = response
        .json()
        .await
        .map_err(|e| AppError::Provider(format!("update check failed: {e}")))?;

    let tag = release["tag_name"].as_str().unwrap_or_default();
    let version = tag.trim_start_matches('v').to_string();
    let assets = release["assets"].as_array().cloned().unwrap_or_default();

    Ok(UpdateInfo {
        newer: !version.is_empty() && newer_than(&version, &current),
        version,
        current,
        notes: release["body"].as_str().unwrap_or_default().to_string(),
        page: release["html_url"].as_str().unwrap_or_default().to_string(),
        download: asset_for_platform(&assets),
    })
}

/// Replace a man's correspondence with what the operator edited.
///
/// The record is theirs to correct: a message pasted with the wrong role, a
/// typo his letter never had, a line that belongs to another man. Messages keep
/// the ids they came with, new ones are given their own, and the window that
/// still reaches the model is clamped to what is left.
#[tauri::command]
pub fn save_chat(
    state: State<'_, AppState>,
    model_id: String,
    man_id: String,
    messages: Vec<ChatMessage>,
    summary: Option<String>,
) -> Result<ChatThread> {
    let scope = state.paths.scope(&model_id)?;
    let mut thread = scope.read_chat(&man_id)?;
    thread.messages = messages;
    // The digest of what was folded away is the operator's text too.
    if let Some(summary) = summary {
        thread.context_summary = summary;
    }
    thread.context_from = thread.context_from.min(thread.messages.len());
    thread.updated_at = chrono::Utc::now();
    scope.write_chat(&thread)?;
    Ok(thread)
}

/// Write the digest of a correspondence, changing nothing.
#[tauri::command]
pub async fn digest_chat(
    app: AppHandle,
    state: State<'_, AppState>,
    model_id: String,
    man_id: String,
    keep_last: Option<usize>,
) -> Result<agent::DigestPreview> {
    entitlement::ensure_room(&state.paths)?;
    let settings = state.settings_view();
    let provider = state.active_provider()?;
    let pool = state.pool(&provider.id);
    let emit = emitter(&app, None);
    let queue = queueing(&app, &state, None);
    let deps = AgentDeps {
        paths: &state.paths,
        settings: &settings,
        provider: &provider,
        pool,
        llm: &state.llm,
        emit: &emit,
        queue: &queue,
        cancel: agent::never_cancelled(),
    };
    let preview = agent::digest_preview(&deps, &model_id, &man_id, keep_last.unwrap_or(6)).await?;
    entitlement::charge(&state.paths)?;
    Ok(preview)
}

/// Accept a digest: the letters it replaces are copied aside and deleted.
#[tauri::command]
pub fn apply_digest(
    state: State<'_, AppState>,
    model_id: String,
    man_id: String,
    keep_last: Option<usize>,
    digest: String,
) -> Result<ChatThread> {
    agent::apply_digest(
        &state.paths,
        &model_id,
        &man_id,
        keep_last.unwrap_or(6),
        &digest,
    )
}

/// Describe how this woman writes, from the letters she has already sent.
#[tauri::command]
pub async fn learn_voice(
    app: AppHandle,
    state: State<'_, AppState>,
    model_id: String,
    samples: Option<usize>,
) -> Result<Profile> {
    let settings = state.settings_view();
    let provider = state.active_provider()?;
    let pool = state.pool(&provider.id);
    let emit = emitter(&app, None);
    let queue = queueing(&app, &state, None);
    let deps = AgentDeps {
        paths: &state.paths,
        settings: &settings,
        provider: &provider,
        pool,
        llm: &state.llm,
        emit: &emit,
        queue: &queue,
        cancel: agent::never_cancelled(),
    };
    agent::learn_voice(&deps, &model_id, samples.unwrap_or(10)).await
}

/// Drop the picked entries from a conversation.
///
/// The operator's own chat with the agent: what is removed here stops being
/// shown and stops being counted, and for the master chat — the one that is
/// replayed to the model turn after turn — it also stops being sent.
#[tauri::command]
pub fn delete_agent_entries(
    state: State<'_, AppState>,
    model_id: String,
    man_id: Option<String>,
    ids: Vec<String>,
) -> Result<AgentLog> {
    let scope = state.paths.scope(&model_id)?;
    let mut log = scope.read_agent_log(man_id.as_deref())?;
    log.entries.retain(|entry| !ids.contains(&entry.id));
    scope.write_agent_log(&log)?;
    Ok(log)
}

#[tauri::command]
pub fn delete_master_entries(state: State<'_, AppState>, ids: Vec<String>) -> Result<AgentLog> {
    let mut log = state.paths.master_log()?;
    log.entries.retain(|entry| !ids.contains(&entry.id));
    state.paths.write_master_log(&log)?;
    Ok(log)
}

/// Remove messages from a man's correspondence.
///
/// This is the record every prompt is built from, so a message deleted here is
/// gone from the next request as well — which is usually the point: something
/// was filed by mistake, or the operator does not want it steering the model.
/// The window of messages that still go to the model is pulled back so it keeps
/// pointing at the same place in a shorter thread.
#[tauri::command]
pub fn delete_chat_messages(
    state: State<'_, AppState>,
    model_id: String,
    man_id: String,
    ids: Vec<String>,
) -> Result<ChatThread> {
    let scope = state.paths.scope(&model_id)?;
    let mut thread = scope.read_chat(&man_id)?;
    thread.remove(&ids);
    scope.write_chat(&thread)?;
    Ok(thread)
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn run_agent(
    app: AppHandle,
    state: State<'_, AppState>,
    input: RunInput,
) -> Result<RunOutput> {
    entitlement::ensure_room(&state.paths)?;
    let settings = state.settings_view();
    let provider = state.active_provider()?;
    let pool = state.pool(&provider.id);
    let emit = emitter(&app, input.run_id.clone());
    let queue = queueing(&app, &state, input.run_id.clone());
    let run_id = input.run_id.clone().unwrap_or_default();
    let cancel = state.cancel_flag(&run_id);

    let deps = AgentDeps {
        paths: &state.paths,
        settings: &settings,
        provider: &provider,
        pool,
        llm: &state.llm,
        emit: &emit,
        queue: &queue,
        cancel,
    };

    let output = agent::run(&deps, input).await;
    state.drop_cancel(&run_id);
    let output = output?;
    entitlement::charge(&state.paths)?;
    // The actions reached the queue as they were made; nothing to add here.
    storage::rebuild_index(&state.paths)?;
    Ok(output)
}

/// Stop a run in flight.
///
/// The switch is raised, not the thread killed: the run notices between turns
/// and inside the stream it is reading, and ends with whatever it had written
/// by then — a half-finished letter is still worth having, and the tools that
/// already ran are still recorded.
#[tauri::command]
pub fn cancel_run(state: State<'_, AppState>, run_id: String) -> Result<()> {
    state.cancel_run(&run_id);
    Ok(())
}

/// What the correspondence currently costs, so the UI can show a gauge and
/// the operator knows when compaction is due.
#[tauri::command]
pub async fn context_stats(
    state: State<'_, AppState>,
    model_id: String,
    man_id: Option<String>,
) -> Result<agent::ContextStats> {
    let settings = state.settings_view();
    let provider = state.active_provider()?;
    let scope = state.paths.scope(&model_id)?;
    let mut stats = agent::context_stats(&scope, &settings, &provider, man_id.as_deref())?;

    if let Ok(request) = agent::next_request(&scope, &settings, &provider, man_id.as_deref()) {
        count_exactly(&state, &provider, &request, &mut stats).await;
    }
    Ok(stats)
}

/// Replace the estimate with the provider's own count, when it offers one.
///
/// Gemini's `countTokens` uses the tokenizer that does the real work and costs
/// no generation quota. A failure here is not worth surfacing: the gauge simply
/// stays on the estimate, which is what it says it is.
async fn count_exactly(
    state: &State<'_, AppState>,
    provider: &crate::config::ProviderConfig,
    request: &crate::llm::ChatRequest,
    stats: &mut agent::ContextStats,
) {
    if provider.kind != crate::config::ProviderKind::Gemini {
        return;
    }
    let Some(lease) = state.pool(&provider.id).acquire() else {
        return;
    };
    match crate::llm::gemini::count_tokens(&state.llm.http, provider, &lease.key, request).await {
        Ok(tokens) => {
            stats.used_tokens = tokens as usize;
            stats.ratio = tokens as f32 / stats.window_tokens.max(1) as f32;
            stats.exact = true;
        }
        Err(err) => eprintln!("countTokens unavailable: {}", err.message()),
    }
}

/// Drop the correspondence from the prompt without deleting a single message
/// or a single remembered fact.
#[tauri::command]
pub fn clear_context(
    state: State<'_, AppState>,
    model_id: String,
    man_id: String,
) -> Result<agent::ContextStats> {
    let scope = state.paths.scope(&model_id)?;
    agent::clear_context(&scope, &man_id)?;
    let settings = state.settings_view();
    let provider = state.active_provider()?;
    agent::context_stats(&scope, &settings, &provider, Some(&man_id))
}

/// Fold the open chat into a summary that stands in for it.
#[tauri::command]
pub async fn compact_chat(
    app: AppHandle,
    state: State<'_, AppState>,
    model_id: String,
    man_id: Option<String>,
) -> Result<AgentLog> {
    entitlement::ensure_room(&state.paths)?;
    let settings = state.settings_view();
    let provider = state.active_provider()?;
    let pool = state.pool(&provider.id);
    let emit = emitter(&app, None);
    let queue = queueing(&app, &state, None);

    let deps = AgentDeps {
        paths: &state.paths,
        settings: &settings,
        provider: &provider,
        pool,
        llm: &state.llm,
        emit: &emit,
        queue: &queue,
        cancel: agent::never_cancelled(),
    };
    let log = agent::compact_chat(&deps, &model_id, man_id.as_deref()).await?;
    entitlement::charge(&state.paths)?;
    Ok(log)
}

/// Summarise the older messages and keep only the tail verbatim.
#[tauri::command]
pub async fn compact_context(
    app: AppHandle,
    state: State<'_, AppState>,
    model_id: String,
    man_id: String,
    keep_last: Option<usize>,
) -> Result<agent::ContextStats> {
    let settings = state.settings_view();
    let provider = state.active_provider()?;
    let pool = state.pool(&provider.id);
    let emit = emitter(&app, None);
    let queue = queueing(&app, &state, None);
    let scope = state.paths.scope(&model_id)?;

    let deps = AgentDeps {
        paths: &state.paths,
        settings: &settings,
        provider: &provider,
        pool,
        llm: &state.llm,
        emit: &emit,
        queue: &queue,
        cancel: agent::never_cancelled(),
    };

    agent::compact_context(&deps, &scope, &man_id, keep_last.unwrap_or(6)).await?;
    agent::context_stats(&scope, &settings, &provider, Some(&man_id))
}

/// The master chat: one conversation with access to every profile.
/// Write to one man or to a whole list, each letter in her voice.
#[tauri::command]
pub async fn write_letters(
    app: AppHandle,
    state: State<'_, AppState>,
    input: agent::LettersInput,
) -> Result<agent::LettersOutput> {
    let settings = state.settings_view();
    let provider = state.active_provider()?;
    let pool = state.pool(&provider.id);
    let emit = emitter(&app, None);
    let queue = queueing(&app, &state, None);

    let deps = AgentDeps {
        paths: &state.paths,
        settings: &settings,
        provider: &provider,
        pool,
        llm: &state.llm,
        emit: &emit,
        queue: &queue,
        cancel: agent::never_cancelled(),
    };
    agent::write_letters(&deps, input).await
}

#[tauri::command]
pub async fn master_chat(
    app: AppHandle,
    state: State<'_, AppState>,
    input: agent::master::MasterInput,
) -> Result<agent::master::MasterOutput> {
    entitlement::ensure_room(&state.paths)?;
    let settings = state.settings_view();
    let provider = state.active_provider()?;
    let pool = state.pool(&provider.id);
    let emit = emitter(&app, input.run_id.clone());
    let queue = queueing(&app, &state, input.run_id.clone());
    let run_id = input.run_id.clone().unwrap_or_default();
    let cancel = state.cancel_flag(&run_id);

    let deps = AgentDeps {
        paths: &state.paths,
        settings: &settings,
        provider: &provider,
        pool,
        llm: &state.llm,
        emit: &emit,
        queue: &queue,
        cancel,
    };

    let output = agent::master::chat(&deps, input).await;
    state.drop_cancel(&run_id);
    let output = output?;
    entitlement::charge(&state.paths)?;
    // The actions reached the queue as they were made; nothing to add here.
    Ok(output)
}

/// What the master chat's next turn would cost.
#[tauri::command]
pub async fn master_context_stats(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<agent::ContextStats> {
    let settings = state.settings_view();
    let provider = state.active_provider()?;
    let pool = state.pool(&provider.id);
    let emit = emitter(&app, None);
    let queue = queueing(&app, &state, None);

    let deps = AgentDeps {
        paths: &state.paths,
        settings: &settings,
        provider: &provider,
        pool,
        llm: &state.llm,
        emit: &emit,
        queue: &queue,
        cancel: agent::never_cancelled(),
    };
    let mut stats = agent::master::context_stats(&deps)?;
    if let Ok(request) = agent::master::next_request(&deps) {
        count_exactly(&state, &provider, &request, &mut stats).await;
    }
    Ok(stats)
}

#[tauri::command]
pub fn get_master_log(state: State<'_, AppState>) -> Result<AgentLog> {
    state.paths.master_log()
}

#[tauri::command]
pub fn clear_master_log(state: State<'_, AppState>) -> Result<()> {
    state
        .paths
        .write_master_log(&AgentLog::new("master".into(), None))
}

#[tauri::command]
pub fn global_search(state: State<'_, AppState>, query: String) -> Result<Vec<SearchHit>> {
    storage::global_search(&state.paths, &query, 50)
}

#[tauri::command]
pub fn rebuild_index(state: State<'_, AppState>) -> Result<GlobalIndex> {
    storage::rebuild_index(&state.paths)
}

// ---------------------------------------------------------------------------
// Approval queue
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn pending_list(state: State<'_, AppState>) -> Result<Vec<PendingAction>> {
    Ok(state.pending.read().clone())
}

#[tauri::command]
pub fn pending_approve(state: State<'_, AppState>, id: String) -> Result<PendingAction> {
    let action = {
        let mut queue = state.pending.write();
        let idx = queue
            .iter()
            .position(|a| a.id == id)
            .ok_or_else(|| AppError::NotFound(format!("pending action {id}")))?;
        queue.remove(idx)
    };
    // Granting a folder is the one approval that changes what agents may
    // reach, so it is written into settings rather than executed.
    if action.tool == "request_access" {
        let path = action.args["path"].as_str().unwrap_or_default().to_string();
        let writable = action.args["writable"].as_bool().unwrap_or(true);
        let reason = action.args["reason"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let mut settings = state.settings.write();
        settings.trusted_roots.retain(|r| r.path != path);
        settings.trusted_roots.push(crate::workspace::TrustedRoot {
            path,
            writable,
            granted_at: chrono::Utc::now(),
            reason,
        });
        settings.save(&state.paths)?;
        return Ok(action);
    }

    if crate::agent::workspace_tools::is_workspace_tool(&action.tool) {
        let roots = state.settings.read().trusted_roots.clone();
        crate::agent::workspace_tools::commit(&state.paths, &roots, &action.tool, &action.args)?;
        return Ok(action);
    }

    if action.tool == "create_profile" {
        crate::agent::master::execute(
            &state.paths,
            &[],
            crate::config::SecurityLevel::Yolo,
            "create_profile",
            &action.args,
        )?;
        storage::rebuild_index(&state.paths)?;
        return Ok(action);
    }

    let scope = state.paths.scope(&action.model_id)?;
    // Re-plan against current state so an approval never writes stale data.
    let plan = tools::plan_mutation(&scope, &action.tool, &action.args)?;
    tools::commit(&scope, &plan.target)?;
    storage::rebuild_index(&state.paths)?;
    Ok(action)
}

/// Folders agents may use, and the ability to take one back.
#[tauri::command]
pub fn list_trusted_roots(
    state: State<'_, AppState>,
) -> Result<Vec<crate::workspace::TrustedRoot>> {
    Ok(state.settings.read().trusted_roots.clone())
}

#[tauri::command]
pub fn trust_folder(
    state: State<'_, AppState>,
    path: String,
    writable: Option<bool>,
) -> Result<Vec<crate::workspace::TrustedRoot>> {
    if !std::path::Path::new(&path).is_dir() {
        return Err(AppError::message(
            "error.notAFolder",
            json!({ "path": path }),
        ));
    }
    // Stored as picked, not as canonicalised: the extended-length form Windows
    // returns reads as a different folder to everyone who sees it.
    let path = crate::workspace::display_path(std::path::Path::new(&path));
    let mut settings = state.settings.write();
    settings.trusted_roots.retain(|r| r.path != path);
    settings.trusted_roots.push(crate::workspace::TrustedRoot {
        path,
        writable: writable.unwrap_or(true),
        granted_at: chrono::Utc::now(),
        reason: "granted by the operator".into(),
    });
    settings.save(&state.paths)?;
    Ok(settings.trusted_roots.clone())
}

#[tauri::command]
pub fn revoke_folder(
    state: State<'_, AppState>,
    path: String,
) -> Result<Vec<crate::workspace::TrustedRoot>> {
    let mut settings = state.settings.write();
    settings.trusted_roots.retain(|r| r.path != path);
    settings.save(&state.paths)?;
    Ok(settings.trusted_roots.clone())
}

/// Copies kept before an agent overwrote or deleted a file.
#[tauri::command]
pub fn list_backups(state: State<'_, AppState>) -> Result<Vec<crate::workspace::Backup>> {
    let mut entries = crate::workspace::read_backups(&state.paths)?.entries;
    entries.reverse();
    Ok(entries)
}

#[tauri::command]
pub fn restore_backup(state: State<'_, AppState>, backup_id: String) -> Result<String> {
    let restored = crate::workspace::restore(&state.paths, &backup_id)?;
    Ok(restored.to_string_lossy().to_string())
}

#[tauri::command]
pub fn pending_reject(state: State<'_, AppState>, id: String) -> Result<()> {
    state.pending.write().retain(|a| a.id != id);
    Ok(())
}

#[tauri::command]
pub fn pending_clear(state: State<'_, AppState>) -> Result<()> {
    state.pending.write().clear();
    Ok(())
}

// ---------------------------------------------------------------------------
// Doctor
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn doctor_scan(state: State<'_, AppState>) -> Result<DoctorReport> {
    doctor::run(&state.paths, false)
}

#[tauri::command]
pub fn doctor_fix(state: State<'_, AppState>) -> Result<DoctorReport> {
    doctor::run(&state.paths, true)
}

// ---------------------------------------------------------------------------
// Settings & keys
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> Result<Settings> {
    Ok(state.settings_view())
}

#[tauri::command]
pub fn save_settings(state: State<'_, AppState>, settings: Settings) -> Result<Settings> {
    state.save_settings(settings)?;
    Ok(state.settings_view())
}

/// Where the VelvetDesk Cloud subscription stands.
///
/// The licence is checked here, against the public key in settings, before
/// anyone is asked anything: a signature is a fact the laptop can establish on
/// its own, and it is what lets the app say "expired" while the network is
/// down. The credits come from the gateway, because only the gateway knows
/// what has been spent.
#[derive(Debug, Clone, Serialize)]
pub struct CloudStatus {
    /// True when the licence is signed by the key we carry and still in date.
    pub valid: bool,
    pub license_id: String,
    pub tier: String,
    pub expires_at: i64,
    pub max_peers: u32,
    /// Empty when the licence itself is fine.
    pub problem: String,
    /// Filled in only when the gateway answered.
    pub credits_left_5h: Option<f64>,
    pub credits_left_week: Option<f64>,
    /// What the plan allows in each window, so what is left can be read as a
    /// share of it rather than as a bare number.
    pub credits_5h: Option<f64>,
    pub credits_week: Option<f64>,
    pub reset_at: Option<i64>,
}

const CLOUD_PROVIDER: &str = "velvetdesk-cloud";

/// Where sync stands on this device.
#[derive(Debug, Clone, Serialize)]
pub struct SyncState {
    pub paired: bool,
    pub device_id: String,
    /// The invite to read out to the other device. Only this device's own.
    pub invite: String,
    pub relay: String,
    pub auto: bool,
    pub last: Option<crate::sync::Report>,
    /// True when the plan includes sync at all.
    pub allowed: bool,
    /// How many machines this licence covers.
    pub devices: u32,
    /// True when the pairing came from the licence rather than an invite:
    /// nothing was typed in, and nothing has to be.
    pub from_license: bool,
}

/// The relay to meet at: the cloud provider's address, minus its API path.
fn relay_base(state: &AppState) -> String {
    state
        .settings
        .read()
        .provider(CLOUD_PROVIDER)
        .map(|p| {
            p.base_url
                .trim_end_matches('/')
                .trim_end_matches("/v1")
                .to_string()
        })
        .unwrap_or_default()
}

fn license_key(state: &AppState) -> String {
    state
        .secrets
        .read()
        .for_provider(CLOUD_PROVIDER)
        .first()
        .cloned()
        .unwrap_or_default()
}

/// Where sync stands, pairing this device off the licence if it has not been
/// paired yet.
///
/// The operator with a subscription never sees a pairing step: installing the
/// app on the second machine and pasting the same licence is the whole
/// procedure. Devices beyond what the licence covers are turned away by the
/// gateway, which is the only party that can count them.
#[tauri::command]
pub fn sync_state(state: State<'_, AppState>) -> Result<SyncState> {
    let limits = crate::entitlement::limits();
    let license = license_key(&state);
    let mut pairing = crate::sync::pair::Pairing::load(&state.paths)?;
    if pairing.is_none() && limits.sync && !license.trim().is_empty() {
        let relay = relay_base(&state);
        if !relay.is_empty() {
            pairing = crate::sync::pair::from_license(&state.paths, &license, &relay).ok();
        }
    }
    let from_license = pairing.as_ref().is_some_and(|p| {
        !license.trim().is_empty()
            && p.key
                == base64::Engine::encode(
                    &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                    crate::sync::pair::secret_from_license(&license),
                )
    });
    let last = crate::sync::read_report(&state.paths)?;
    Ok(match pairing {
        Some(pairing) => SyncState {
            paired: true,
            device_id: pairing.device_id.clone(),
            invite: pairing.invite(),
            relay: pairing.relay.clone(),
            auto: pairing.auto,
            last,
            allowed: limits.sync,
            devices: limits.devices,
            from_license,
        },
        None => SyncState {
            paired: false,
            device_id: String::new(),
            invite: String::new(),
            relay: relay_base(&state),
            auto: false,
            last,
            allowed: limits.sync,
            devices: limits.devices,
            from_license: false,
        },
    })
}

/// Start a pairing here and hand back the invite for the other device.
#[tauri::command]
pub fn sync_create_invite(state: State<'_, AppState>) -> Result<SyncState> {
    let relay = relay_base(&state);
    if relay.is_empty() {
        return Err(AppError::message("sync.noRelay", json!({})));
    }
    crate::sync::pair::create(&state.paths, &relay)?;
    sync_state(state)
}

#[tauri::command]
pub fn sync_join(state: State<'_, AppState>, invite: String) -> Result<SyncState> {
    let relay = relay_base(&state);
    crate::sync::pair::join(&state.paths, &invite, &relay)?;
    sync_state(state)
}

/// Unpair this device. The other one keeps its own copy of everything; what
/// stops is the traffic between them.
#[tauri::command]
pub fn sync_forget(state: State<'_, AppState>) -> Result<SyncState> {
    crate::sync::pair::Pairing::forget(&state.paths)?;
    sync_state(state)
}

#[tauri::command]
pub fn sync_set_auto(state: State<'_, AppState>, auto: bool) -> Result<SyncState> {
    if let Some(mut pairing) = crate::sync::pair::Pairing::load(&state.paths)? {
        pairing.auto = auto;
        pairing.save(&state.paths)?;
    }
    sync_state(state)
}

/// One round, now, because the operator pressed the button.
#[tauri::command]
pub async fn sync_now(state: State<'_, AppState>) -> Result<crate::sync::Report> {
    if !crate::entitlement::limits().sync {
        return Err(AppError::message("limit.sync", json!({})));
    }
    let pairing = crate::sync::pair::Pairing::load(&state.paths)?
        .ok_or_else(|| AppError::message("sync.notPaired", json!({})))?;
    let license = license_key(&state);
    crate::sync::mailbox::run_round(
        &state.llm.http,
        &state.paths,
        &pairing,
        &license,
        &crate::hwid::device_id(&state.paths),
    )
    .await
}

/// The plan, the day's meter and what is left of both.
///
/// Cheap and synchronous on purpose: the UI asks for it on every screen that
/// has a limit to show, and nothing here touches the network.
#[tauri::command]
pub fn plan_state(state: State<'_, AppState>) -> Result<entitlement::PlanState> {
    let token = license_key(&state);
    let profiles = storage::load_index(&state.paths)?.models.len();
    Ok(entitlement::plan_state(&state.paths, &token, profiles))
}

#[tauri::command]
pub async fn cloud_status(state: State<'_, AppState>) -> Result<CloudStatus> {
    let settings = state.settings.read().clone();
    let token = state
        .secrets
        .read()
        .for_provider(CLOUD_PROVIDER)
        .first()
        .cloned()
        .unwrap_or_default();
    // Same rule as activation: the build decides where the subscription is,
    // and a stale address in the settings file does not get a vote.
    let mut base_url = entitlement::cloud_base_url();
    if base_url.is_empty() {
        base_url = settings
            .provider(CLOUD_PROVIDER)
            .map(|p| p.base_url.trim_end_matches('/').to_string())
            .unwrap_or_default();
    }

    let mut status = CloudStatus {
        valid: false,
        license_id: String::new(),
        tier: String::new(),
        expires_at: 0,
        max_peers: 0,
        problem: String::new(),
        credits_left_5h: None,
        credits_left_week: None,
        credits_5h: None,
        credits_week: None,
        reset_at: None,
    };

    if token.trim().is_empty() {
        status.problem = "license.missing".into();
        return Ok(status);
    }

    // The signature is checked against the key baked into this binary, and
    // when there is none — a development build — the gateway's own answer
    // stands in for it. Reporting "the signature does not match" to somebody
    // holding a licence the gateway accepts was the worst of both: true
    // about this build, useless about their subscription.
    let entitlement = entitlement::read_here(&state.paths, &token);
    status.license_id = entitlement.license_id.clone();
    status.tier = entitlement.tier.clone();
    status.expires_at = entitlement.expires_at;
    status.max_peers = entitlement.limits.devices;
    status.valid = entitlement.valid;
    status.problem = entitlement.problem.clone();
    if !entitlement.valid && entitlement.problem.starts_with("license.invalid") {
        return Ok(status);
    }

    if base_url.is_empty() {
        return Ok(status);
    }

    // What is left is the gateway's to say. A gateway that cannot be reached
    // leaves the numbers empty rather than guessing at them.
    // The same header every other call carries: asking what the licence has
    // left is also how a freshly entered key claims its seat, so a licence
    // whose devices are all taken says so here rather than at the first
    // reply the operator tries to write.
    let response = state
        .llm
        .http
        .get(format!("{base_url}/usage"))
        .header("authorization", format!("Bearer {}", token.trim()))
        .header(
            entitlement::DEVICE_HEADER,
            crate::hwid::device_id(&state.paths),
        )
        .send()
        .await;
    let Ok(response) = response else {
        return Ok(status);
    };
    if !response.status().is_success() {
        if response.status().as_u16() == 403 || response.status().as_u16() == 401 {
            status.valid = false;
            // The gateway's own words, when it has any: "this licence covers
            // 10 devices and 10 are already registered" is the one refusal
            // the operator can actually act on, and a generic "refused"
            // would send them to support to find that out.
            let said = response
                .json::<Value>()
                .await
                .ok()
                .and_then(|body| {
                    body.pointer("/error/message")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_default();
            status.problem = if said.is_empty() {
                "license.refused".into()
            } else {
                format!("license.refused:{said}")
            };
        }
        return Ok(status);
    }
    let Ok(body) = response.json::<Value>().await else {
        return Ok(status);
    };
    status.credits_left_5h = body.get("credits_left_5h").and_then(Value::as_f64);
    status.credits_left_week = body.get("credits_left_week").and_then(Value::as_f64);
    status.credits_5h = body.get("credits_5h").and_then(Value::as_f64);
    status.credits_week = body.get("credits_week").and_then(Value::as_f64);
    status.reset_at = body.get("reset_at").and_then(Value::as_i64);
    if status.license_id.is_empty() {
        status.license_id = body
            .get("license_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        status.tier = body
            .get("tier")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
    }
    Ok(status)
}

#[tauri::command]
pub fn list_keys(state: State<'_, AppState>, provider_id: String) -> Result<Vec<KeyStatus>> {
    Ok(state.pool(&provider_id).status())
}

/// Take a licence key, and say plainly what it is.
///
/// Saving whatever was pasted and reporting success was the wrong shape:
/// somebody who types the licence *id* instead of the key, or pastes half of
/// one, was told the key had been accepted and then found that nothing had
/// changed. The signature is checked here, before anything is stored, and
/// the answer names the actual problem.
#[tauri::command]
pub async fn activate_license(
    state: State<'_, AppState>,
    token: String,
) -> Result<entitlement::PlanState> {
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err(AppError::message("license.empty", json!({})));
    }
    // A licence key is the whole signed token. The id of a licence — the
    // name it was issued under — looks like a key to someone who has only
    // ever been sent one, and saying so is kinder than failing silently.
    if !token.starts_with("VD.") || token.matches('.').count() != 2 {
        return Err(AppError::message("license.notAKey", json!({})));
    }

    let local = entitlement::read(&token);
    match local.problem.split(':').next().unwrap_or("") {
        // Signed by our key and in date: nothing else to ask anyone.
        "" if local.valid => {}
        "license.expired" => {
            return Err(AppError::message(
                "license.expiredOn",
                json!({ "date": local.expires_at }),
            ))
        }
        // This build carries no key to check against — a development run, or
        // one compiled without the secret. The gateway can still say, and it
        // is the gateway that decides in the end.
        "license.noPublicKey" => confirm_with_gateway(&state, &token).await?,
        _ => return Err(AppError::message("license.notOurs", json!({}))),
    }

    let mut secrets = state.secrets.read().clone();
    secrets
        .keys
        .insert(entitlement::CLOUD_PROVIDER.to_string(), vec![token]);
    state.save_secrets(secrets)?;
    plan_state(state)
}

/// Ask the gateway whether this licence is good, and remember what it said.
async fn confirm_with_gateway(state: &AppState, token: &str) -> Result<()> {
    // The settings file may carry an address written by an older build —
    // including an empty one — so the build's own address is what counts,
    // and what is on disk is only a fallback for it.
    let mut base_url = entitlement::cloud_base_url();
    if base_url.is_empty() {
        base_url = state
            .settings
            .read()
            .provider(entitlement::CLOUD_PROVIDER)
            .map(|p| p.base_url.trim_end_matches('/').to_string())
            .unwrap_or_default();
    }
    if base_url.is_empty() {
        return Err(AppError::message("license.buildHasNoGateway", json!({})));
    }

    let response = state
        .llm
        .http
        .get(format!("{base_url}/usage"))
        .header("authorization", format!("Bearer {token}"))
        .header(
            entitlement::DEVICE_HEADER,
            crate::hwid::device_id(&state.paths),
        )
        .send()
        .await
        .map_err(|err| {
            AppError::message("license.noGateway", json!({ "error": err.to_string() }))
        })?;

    let status = response.status();
    let body: Value = response.json().await.unwrap_or_else(|_| json!({}));
    if !status.is_success() {
        let said = body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        return Err(AppError::message(
            "license.refusedBy",
            json!({ "message": if said.is_empty() { status.to_string() } else { said } }),
        ));
    }

    let verdict = entitlement::Verdict {
        token_hash: entitlement::fingerprint(token),
        tier: body
            .get("tier")
            .and_then(Value::as_str)
            .unwrap_or("pro")
            .to_string(),
        license_id: body
            .get("license_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        expires_at: body.get("expires_at").and_then(Value::as_i64).unwrap_or(0),
        max_peers: body
            .get("max_peers")
            .and_then(Value::as_u64)
            .unwrap_or(2)
            .min(u32::MAX as u64) as u32,
        checked_at: chrono::Utc::now().timestamp(),
    };
    entitlement::write_verdict(&state.paths, &verdict)?;
    Ok(())
}

#[tauri::command]
pub fn set_keys(
    state: State<'_, AppState>,
    provider_id: String,
    keys: Vec<String>,
) -> Result<Vec<KeyStatus>> {
    let mut secrets = state.secrets.read().clone();
    let cleaned: Vec<String> = keys
        .into_iter()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .collect();
    secrets.keys.insert(provider_id.clone(), cleaned);
    state.save_secrets(secrets)?;
    Ok(state.pool(&provider_id).status())
}

#[tauri::command]
pub fn add_key(
    state: State<'_, AppState>,
    provider_id: String,
    key: String,
) -> Result<Vec<KeyStatus>> {
    let mut secrets = state.secrets.read().clone();
    let entry = secrets.keys.entry(provider_id.clone()).or_default();
    let key = key.trim().to_string();
    if key.is_empty() {
        return Err(AppError::Invalid("пустой ключ".into()));
    }
    if !entry.contains(&key) {
        entry.push(key);
    }
    state.save_secrets(secrets)?;
    Ok(state.pool(&provider_id).status())
}

#[tauri::command]
pub fn remove_key(
    state: State<'_, AppState>,
    provider_id: String,
    index: usize,
) -> Result<Vec<KeyStatus>> {
    let mut secrets = state.secrets.read().clone();
    if let Some(list) = secrets.keys.get_mut(&provider_id) {
        if index < list.len() {
            list.remove(index);
        }
    }
    state.save_secrets(secrets)?;
    Ok(state.pool(&provider_id).status())
}

/// Ask the provider which models the stored keys may use. The detected API
/// version is written back to settings so the operator never types it.
#[tauri::command]
pub async fn list_provider_models(
    state: State<'_, AppState>,
    provider_id: String,
) -> Result<crate::llm::catalog::ModelCatalog> {
    let provider = {
        let settings = state.settings.read();
        settings
            .provider(&provider_id)
            .cloned()
            .ok_or_else(|| AppError::NotFound(format!("provider {provider_id}")))?
    };
    let pool = state.pool(&provider.id);
    let lease = pool.acquire().ok_or_else(|| {
        AppError::message("error.noWorkingKey", json!({ "provider": provider.id }))
    })?;

    let catalog =
        match crate::llm::catalog::list_models(&state.llm.http, &provider, &lease.key).await {
            Ok(catalog) => {
                pool.report_success(lease.index);
                catalog
            }
            Err(err) => {
                pool.report_failure(lease.index, crate::llm::keypool::KeyVerdict::Transient);
                return Err(AppError::Provider(err.message()));
            }
        };

    // Remember the version that actually answered.
    let mut settings = state.settings.read().clone();
    if let Some(target) = settings.providers.iter_mut().find(|p| p.id == provider.id) {
        target.api_version = catalog.api_version.clone();
    }
    state.save_settings(settings)?;

    Ok(catalog)
}

/// The largest picture worth pulling in from the web, in bytes.
const MAX_FETCHED_IMAGE: usize = 8 * 1024 * 1024;

/// Fetch a picture the operator dragged in from a browser.
///
/// Dragging an image out of a web page hands the app a link, not the file, and
/// the page it came from usually refuses a request made from the webview. The
/// download happens here instead, and only for pictures: the content type is
/// checked, the size is capped, and nothing else is followed.
#[tauri::command]
pub async fn fetch_image(state: State<'_, AppState>, url: String) -> Result<Value> {
    let trimmed = url.trim();
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return Err(AppError::message("error.notAnImageLink", json!({})));
    }

    let response = state
        .llm
        .http
        .get(trimmed)
        .send()
        .await
        .map_err(|e| AppError::Provider(format!("image download failed: {e}")))?;
    if !response.status().is_success() {
        return Err(AppError::Provider(format!(
            "image download failed: HTTP {}",
            response.status()
        )));
    }

    let mime = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if !mime.starts_with("image/") {
        return Err(AppError::message("error.notAnImageLink", json!({})));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| AppError::Provider(format!("image download failed: {e}")))?;
    if bytes.len() > MAX_FETCHED_IMAGE {
        return Err(AppError::message(
            "error.imageTooBig",
            json!({ "mb": MAX_FETCHED_IMAGE / (1024 * 1024) }),
        ));
    }

    let name = trimmed
        .rsplit('/')
        .next()
        .and_then(|tail| tail.split('?').next())
        .filter(|tail| !tail.is_empty())
        .unwrap_or("image")
        .to_string();

    use base64::Engine;
    Ok(json!({
        "name": name,
        "mime": mime,
        "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
    }))
}

/// Transcribe a dictated clip through the operator's own provider.
#[tauri::command]
pub async fn transcribe(
    state: State<'_, AppState>,
    audio_base64: String,
    mime: String,
) -> Result<String> {
    if audio_base64.trim().is_empty() {
        return Err(AppError::message("error.emptyRecording", json!({})));
    }
    // Dictation is a model call like any other, and costs the free plan one
    // — once it has come back with words.
    entitlement::ensure_room(&state.paths)?;
    let provider = {
        let settings = state.settings.read();
        settings
            .speech()
            .cloned()
            .ok_or_else(|| AppError::Invalid("no speech provider configured".into()))?
    };
    let pool = state.pool(&provider.id);
    let lease = pool.acquire().ok_or_else(|| {
        AppError::message("error.noWorkingKey", json!({ "provider": provider.id }))
    })?;

    match crate::llm::catalog::transcribe(
        &state.llm.http,
        &provider,
        &lease.key,
        &audio_base64,
        &mime,
    )
    .await
    {
        Ok(text) => {
            pool.report_success(lease.index);
            entitlement::charge(&state.paths)?;
            Ok(text)
        }
        Err(err) => {
            pool.report_failure(lease.index, crate::llm::keypool::KeyVerdict::Transient);
            Err(AppError::Provider(err.message()))
        }
    }
}

// ---------------------------------------------------------------------------
// On-device Whisper
// ---------------------------------------------------------------------------

pub const MODEL_EVENT: &str = "velvetdesk://model";

#[tauri::command]
pub fn list_local_models(state: State<'_, AppState>) -> Result<Vec<crate::whisper::LocalModel>> {
    Ok(crate::whisper::list(&state.paths))
}

/// Base URL the webview uses to read downloaded weights. Custom schemes are
/// served over http on Windows and Android, and as a real scheme elsewhere.
#[tauri::command]
pub fn local_models_base_url() -> Result<String> {
    if cfg!(any(windows, target_os = "android")) {
        Ok(format!("http://{}.localhost", crate::MODEL_SCHEME))
    } else {
        Ok(format!("{}://localhost", crate::MODEL_SCHEME))
    }
}

#[tauri::command]
pub async fn download_local_model(
    app: AppHandle,
    state: State<'_, AppState>,
    model_id: String,
) -> Result<crate::whisper::LocalModel> {
    let model = crate::whisper::find(&model_id)?;
    let app_for_events = app.clone();
    let report = move |progress: crate::whisper::DownloadProgress| {
        let _ = app_for_events.emit(MODEL_EVENT, progress);
    };
    crate::whisper::download(&state.llm.http, &state.paths, &model, &report).await
}

#[tauri::command]
pub fn delete_local_model(
    state: State<'_, AppState>,
    model_id: String,
) -> Result<Vec<crate::whisper::LocalModel>> {
    let model = crate::whisper::find(&model_id)?;
    crate::whisper::remove(&state.paths, &model)?;
    Ok(crate::whisper::list(&state.paths))
}

/// Cheap connectivity probe: one-token request through the pool.
#[tauri::command]
pub async fn test_provider(app: AppHandle, state: State<'_, AppState>) -> Result<Value> {
    let provider = state.active_provider()?;
    let pool = state.pool(&provider.id);
    let emit = emitter(&app, None);
    let mut request = crate::llm::ChatRequest::new(
        "You are a connectivity probe. Answer with the single word: ok",
    );
    request.temperature = 0.0;
    request.max_output_tokens = Some(16);
    request.messages.push(crate::llm::LlmMessage::user("ping"));

    let response = state.llm.chat(&provider, pool, &request, &emit).await?;
    Ok(json!({
        "provider": provider.id,
        "model": provider.model,
        "text": response.text,
        "key_index": response.key_index,
        "attempts": response.attempts,
        "usage": response.usage,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Versions are compared piece by piece, not as strings: 0.2.10 is newer
    /// than 0.2.9, and the leading `v` of a tag is not part of the number.
    #[test]
    fn a_newer_release_is_recognised() {
        assert!(newer_than("v0.2.10", "0.2.9"));
        assert!(newer_than("0.3.0", "0.2.99"));
        assert!(!newer_than("0.2.9", "0.2.9"));
        assert!(!newer_than("0.2.8", "0.2.9"));
        assert!(newer_than("1.0.0", "0.9.9"));
    }

    /// The installer offered is the one this machine can actually run.
    #[test]
    fn the_platforms_installer_is_picked() {
        let assets = vec![
            json!({ "name": "VelvetDesk-0.2.1.AppImage", "browser_download_url": "u/appimage" }),
            json!({ "name": "VelvetDesk-0.2.1.msi", "browser_download_url": "u/msi" }),
            json!({ "name": "VelvetDesk-0.2.1.dmg", "browser_download_url": "u/dmg" }),
            json!({ "name": "VelvetDesk-0.2.1-app-universal-release.apk", "browser_download_url": "u/apk" }),
        ];
        let picked = asset_for_platform(&assets).unwrap();
        let expected = match std::env::consts::OS {
            "windows" => "u/msi",
            "macos" => "u/dmg",
            "android" => "u/apk",
            _ => "u/appimage",
        };
        assert_eq!(picked, expected);
    }
}
