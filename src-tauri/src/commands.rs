use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, State};

use crate::agent::tools::{self, PendingAction};
use crate::agent::{self, AgentDeps, RunInput, RunOutput};
use crate::config::Settings;
use crate::doctor::{self, DoctorReport};
use crate::error::{AppError, Result};
use crate::llm::keypool::KeyStatus;
use crate::models::*;
use crate::state::AppState;
use crate::storage;

pub const AGENT_EVENT: &str = "velvetdesk://agent";

fn emitter(app: &AppHandle) -> impl Fn(Value) + Send + Sync + '_ {
    move |payload: Value| {
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
    out.sort_by_key(|p| p.name.to_lowercase());
    Ok(out)
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
) -> Result<ChatThread> {
    let scope = state.paths.scope(&model_id)?;
    let mut thread = scope.read_chat(&man_id)?;
    thread.messages = messages;
    thread.context_from = thread.context_from.min(thread.messages.len());
    thread.updated_at = chrono::Utc::now();
    scope.write_chat(&thread)?;
    Ok(thread)
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
    let settings = state.settings_view();
    let provider = state.active_provider()?;
    let pool = state.pool(&provider.id);
    let emit = emitter(&app);

    let deps = AgentDeps {
        paths: &state.paths,
        settings: &settings,
        provider: &provider,
        pool,
        llm: &state.llm,
        emit: &emit,
    };

    let output = agent::run(&deps, input).await?;
    if !output.pending.is_empty() {
        state.pending.write().extend(output.pending.clone());
    }
    storage::rebuild_index(&state.paths)?;
    Ok(output)
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
    let settings = state.settings_view();
    let provider = state.active_provider()?;
    let pool = state.pool(&provider.id);
    let emit = emitter(&app);

    let deps = AgentDeps {
        paths: &state.paths,
        settings: &settings,
        provider: &provider,
        pool,
        llm: &state.llm,
        emit: &emit,
    };
    agent::compact_chat(&deps, &model_id, man_id.as_deref()).await
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
    let emit = emitter(&app);
    let scope = state.paths.scope(&model_id)?;

    let deps = AgentDeps {
        paths: &state.paths,
        settings: &settings,
        provider: &provider,
        pool,
        llm: &state.llm,
        emit: &emit,
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
    let emit = emitter(&app);

    let deps = AgentDeps {
        paths: &state.paths,
        settings: &settings,
        provider: &provider,
        pool,
        llm: &state.llm,
        emit: &emit,
    };
    agent::write_letters(&deps, input).await
}

#[tauri::command]
pub async fn master_chat(
    app: AppHandle,
    state: State<'_, AppState>,
    input: agent::master::MasterInput,
) -> Result<agent::master::MasterOutput> {
    let settings = state.settings_view();
    let provider = state.active_provider()?;
    let pool = state.pool(&provider.id);
    let emit = emitter(&app);

    let deps = AgentDeps {
        paths: &state.paths,
        settings: &settings,
        provider: &provider,
        pool,
        llm: &state.llm,
        emit: &emit,
    };

    let output = agent::master::chat(&deps, input).await?;
    if !output.pending.is_empty() {
        state.pending.write().extend(output.pending.clone());
    }
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
    let emit = emitter(&app);

    let deps = AgentDeps {
        paths: &state.paths,
        settings: &settings,
        provider: &provider,
        pool,
        llm: &state.llm,
        emit: &emit,
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

#[tauri::command]
pub fn list_keys(state: State<'_, AppState>, provider_id: String) -> Result<Vec<KeyStatus>> {
    Ok(state.pool(&provider_id).status())
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
    language: Option<String>,
) -> Result<String> {
    if audio_base64.trim().is_empty() {
        return Err(AppError::message("error.emptyRecording", json!({})));
    }
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

    let language = language.unwrap_or_else(|| {
        let settings = state.settings.read();
        settings.speech_language.clone()
    });

    match crate::llm::catalog::transcribe(
        &state.llm.http,
        &provider,
        &lease.key,
        &audio_base64,
        &mime,
        &language,
    )
    .await
    {
        Ok(text) => {
            pool.report_success(lease.index);
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
    let emit = emitter(&app);
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

/// Populate an empty workspace with one demo profile so the UI is never blank.
#[tauri::command]
pub fn seed_demo(state: State<'_, AppState>) -> Result<Vec<Profile>> {
    if !state.paths.list_model_ids()?.is_empty() {
        return read_profiles(&state);
    }
    let scope = state.paths.scope("2428653")?;
    let mut profile = Profile::new("2428653".into(), "Marina Kazachok".into());
    profile.age = Some(42);
    profile.site = "RomanceCompass".into();
    profile.bio = "Зрелая, тёплая, ценит уважение и ухаживания. Пятеро детей, Оснабрюк.".into();
    profile.tone_rules = vec![
        "тёплый, спокойный тон без восторженных восклицаний".into(),
        "короткие абзацы, живые детали быта".into(),
    ];
    profile.banned_phrases = vec!["I hope this message finds you well".into()];
    profile.languages = vec!["de".into(), "en".into(), "ru".into()];
    scope.write_profile(&profile)?;

    let mut man = Man::new("2428653".into(), "1219749".into(), "Hartwig Buesing".into());
    man.age = Some(65);
    man.location = "Bückeburg, Germany".into();
    man.stage = "warming".into();
    man.status = "Осторожен, предложил встречу у Schlosstor".into();
    man.tags = vec!["Pension".into(), "No Games".into(), "Hikes".into()];
    man.facts = vec![
        Fact {
            id: new_id(),
            key: "health".into(),
            value: "эпилепсия, избегает алкоголя".into(),
            source: "seed".into(),
            created_at: chrono::Utc::now(),
        },
        Fact {
            id: new_id(),
            key: "hobby".into(),
            value: "походы в Швеции".into(),
            source: "seed".into(),
            created_at: chrono::Utc::now(),
        },
    ];
    scope.write_man(&man)?;
    storage::rebuild_index(&state.paths)?;
    read_profiles(&state)
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
        ];
        let picked = asset_for_platform(&assets).unwrap();
        let expected = match std::env::consts::OS {
            "windows" => "u/msi",
            "macos" => "u/dmg",
            _ => "u/appimage",
        };
        assert_eq!(picked, expected);
    }
}
