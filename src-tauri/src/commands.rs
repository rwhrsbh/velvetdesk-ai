use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, State};

use crate::agent::tools::{self, PendingAction};
use crate::agent::{self, AgentDeps, MasterDecision, RunInput, RunOutput};
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
    pub banned_phrases: Option<Vec<String>>,
}

#[tauri::command]
pub fn create_profile(state: State<'_, AppState>, input: NewProfile) -> Result<Profile> {
    if input.name.trim().is_empty() {
        return Err(AppError::Invalid("имя профиля обязательно".into()));
    }
    let id = match input.id.filter(|i| storage::is_safe_id(i)) {
        Some(id) => id,
        None => new_numeric_id(),
    };
    let scope = state.paths.scope(&id)?;
    if scope.profile_file().exists() {
        return Err(AppError::Invalid(format!("профиль {id} уже существует")));
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

#[tauri::command]
pub fn get_agent_log(state: State<'_, AppState>, model_id: String) -> Result<AgentLog> {
    state.paths.scope(&model_id)?.read_agent_log()
}

#[tauri::command]
pub fn clear_agent_log(state: State<'_, AppState>, model_id: String) -> Result<()> {
    let scope = state.paths.scope(&model_id)?;
    scope.write_agent_log(&AgentLog::new(model_id))
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

#[tauri::command]
pub async fn master_route(
    app: AppHandle,
    state: State<'_, AppState>,
    raw: String,
    auto_create: bool,
) -> Result<MasterDecision> {
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

    agent::master_route(&deps, &raw, auto_create).await
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
    let scope = state.paths.scope(&action.model_id)?;
    // Re-plan against current state so an approval never writes stale data.
    let plan = tools::plan_mutation(&scope, &action.tool, &action.args)?;
    tools::commit(&scope, &plan.target)?;
    storage::rebuild_index(&state.paths)?;
    Ok(action)
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
        AppError::NoKeys(format!("у провайдера {} нет рабочего ключа", provider.id))
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

/// Transcribe a dictated clip through the operator's own provider.
#[tauri::command]
pub async fn transcribe(
    state: State<'_, AppState>,
    audio_base64: String,
    mime: String,
) -> Result<String> {
    if audio_base64.trim().is_empty() {
        return Err(AppError::Invalid("пустая запись".into()));
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
        AppError::NoKeys(format!("у провайдера {} нет рабочего ключа", provider.id))
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
            Ok(text)
        }
        Err(err) => {
            pool.report_failure(lease.index, crate::llm::keypool::KeyVerdict::Transient);
            Err(AppError::Provider(err.message()))
        }
    }
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
