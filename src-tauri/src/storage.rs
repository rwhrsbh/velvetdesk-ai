//! Local-first storage engine with per-model sandboxing.
//!
//! Layout (inside the OS app-data directory):
//!
//! ```text
//! app_data/
//!   settings.json
//!   secrets.json          <- api keys, never leaves the machine
//!   global_index.json
//!   profiles/<model_id>/profile.json
//!   profiles/<model_id>/men/<man_id>.json
//!   profiles/<model_id>/chats/<man_id>.json
//!   profiles/<model_id>/agent_log.json      (chat with no dossier open)
//!   profiles/<model_id>/logs/<man_id>.json  (chat about one man)
//!   profiles/<model_id>/attachments/...
//! ```
//!
//! Every path handed to an agent is produced by [`Scope`], which refuses ids that
//! are not plain slugs and re-checks that the final path stays under the model
//! directory. Agents therefore cannot read a sibling model's dossiers.

use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::error::{AppError, Result};
use crate::models::*;

#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    pub fn new(root: PathBuf) -> Result<Self> {
        let root = root.join("app_data");
        fs::create_dir_all(root.join("profiles"))?;
        Ok(Paths { root })
    }

    pub fn settings_file(&self) -> PathBuf {
        self.root.join("settings.json")
    }

    pub fn secrets_file(&self) -> PathBuf {
        self.root.join("secrets.json")
    }

    /// The master agent's own conversation, above every profile.
    pub fn master_log_file(&self) -> PathBuf {
        self.root.join("master_log.json")
    }

    pub fn master_log(&self) -> Result<AgentLog> {
        Ok(read_json::<AgentLog>(&self.master_log_file())?
            .unwrap_or_else(|| AgentLog::new("master".into(), None)))
    }

    pub fn write_master_log(&self, log: &AgentLog) -> Result<()> {
        write_json(&self.master_log_file(), log)
    }

    pub fn index_file(&self) -> PathBuf {
        self.root.join("global_index.json")
    }

    pub fn profiles_dir(&self) -> PathBuf {
        self.root.join("profiles")
    }

    /// Build a sandbox restricted to a single model profile.
    pub fn scope(&self, model_id: &str) -> Result<Scope> {
        Scope::new(self.profiles_dir(), model_id)
    }

    pub fn list_model_ids(&self) -> Result<Vec<String>> {
        let dir = self.profiles_dir();
        if !dir.exists() {
            return Ok(vec![]);
        }
        let mut out = vec![];
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    if is_safe_id(name) {
                        out.push(name.to_string());
                    }
                }
            }
        }
        out.sort();
        Ok(out)
    }
}

/// Ids are used as directory / file names, so they must be plain slugs.
pub fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Isolation handle. All model-scoped file access goes through this type.
#[derive(Debug, Clone)]
pub struct Scope {
    pub model_id: String,
    pub base: PathBuf,
}

impl Scope {
    pub fn new(profiles_dir: PathBuf, model_id: &str) -> Result<Self> {
        if !is_safe_id(model_id) {
            return Err(AppError::Scope(format!("unsafe model id: {model_id}")));
        }
        let base = profiles_dir.join(model_id);
        fs::create_dir_all(base.join("men"))?;
        fs::create_dir_all(base.join("chats"))?;
        fs::create_dir_all(base.join("attachments"))?;
        Ok(Scope {
            model_id: model_id.to_string(),
            base,
        })
    }

    /// Resolve a relative path inside the sandbox, rejecting traversal.
    pub fn resolve(&self, rel: &str) -> Result<PathBuf> {
        let rel_path = Path::new(rel);
        if rel_path.is_absolute() {
            return Err(AppError::Scope(format!("absolute path rejected: {rel}")));
        }
        for comp in rel_path.components() {
            match comp {
                Component::Normal(_) => {}
                _ => return Err(AppError::Scope(format!("path traversal rejected: {rel}"))),
            }
        }
        let joined = self.base.join(rel_path);
        // Defence in depth: if the parent exists, canonicalise and re-check.
        if let Some(parent) = joined.parent() {
            if parent.exists() {
                let real_parent = fs::canonicalize(parent)?;
                let real_base = fs::canonicalize(&self.base)?;
                if !real_parent.starts_with(&real_base) {
                    return Err(AppError::Scope(format!(
                        "resolved path escapes sandbox: {rel}"
                    )));
                }
            }
        }
        Ok(joined)
    }

    pub fn profile_file(&self) -> PathBuf {
        self.base.join("profile.json")
    }

    /// The copilot conversation. Each dossier has its own; `None` is the
    /// profile-wide chat shown when no dossier is open.
    pub fn agent_log_file(&self, man_id: Option<&str>) -> Result<PathBuf> {
        match man_id {
            None => Ok(self.base.join("agent_log.json")),
            Some(id) => {
                if !is_safe_id(id) {
                    return Err(AppError::Scope(format!("unsafe man id: {id}")));
                }
                Ok(self.logs_dir().join(format!("{id}.json")))
            }
        }
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.base.join("logs")
    }

    pub fn men_dir(&self) -> PathBuf {
        self.base.join("men")
    }

    pub fn chats_dir(&self) -> PathBuf {
        self.base.join("chats")
    }

    pub fn attachments_dir(&self) -> PathBuf {
        self.base.join("attachments")
    }

    pub fn man_file(&self, man_id: &str) -> Result<PathBuf> {
        if !is_safe_id(man_id) {
            return Err(AppError::Scope(format!("unsafe man id: {man_id}")));
        }
        Ok(self.men_dir().join(format!("{man_id}.json")))
    }

    pub fn chat_file(&self, man_id: &str) -> Result<PathBuf> {
        if !is_safe_id(man_id) {
            return Err(AppError::Scope(format!("unsafe man id: {man_id}")));
        }
        Ok(self.chats_dir().join(format!("{man_id}.json")))
    }

    // ----- typed IO -------------------------------------------------------

    pub fn read_profile(&self) -> Result<Profile> {
        read_json(&self.profile_file())?
            .ok_or_else(|| AppError::NotFound(format!("profile {}", self.model_id)))
    }

    pub fn write_profile(&self, profile: &Profile) -> Result<()> {
        write_json(&self.profile_file(), profile)
    }

    pub fn list_man_ids(&self) -> Result<Vec<String>> {
        let dir = self.men_dir();
        if !dir.exists() {
            return Ok(vec![]);
        }
        let mut out = vec![];
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(stem) = name.strip_suffix(".json") {
                if is_safe_id(stem) {
                    out.push(stem.to_string());
                }
            }
        }
        out.sort();
        Ok(out)
    }

    pub fn read_man(&self, man_id: &str) -> Result<Man> {
        read_json(&self.man_file(man_id)?)?
            .ok_or_else(|| AppError::NotFound(format!("man {man_id}")))
    }

    pub fn write_man(&self, man: &Man) -> Result<()> {
        let path = self.man_file(&man.id)?;
        write_json(&path, man)
    }

    pub fn delete_man(&self, man_id: &str) -> Result<()> {
        let path = self.man_file(man_id)?;
        if path.exists() {
            fs::remove_file(path)?;
        }
        let chat = self.chat_file(man_id)?;
        if chat.exists() {
            fs::remove_file(chat)?;
        }
        Ok(())
    }

    pub fn read_all_men(&self) -> Result<Vec<Man>> {
        let mut out = vec![];
        for id in self.list_man_ids()? {
            if let Ok(man) = self.read_man(&id) {
                out.push(man);
            }
        }
        out.sort_by_key(|m| std::cmp::Reverse(m.updated_at));
        Ok(out)
    }

    pub fn read_chat(&self, man_id: &str) -> Result<ChatThread> {
        let path = self.chat_file(man_id)?;
        match read_json::<ChatThread>(&path)? {
            Some(thread) => Ok(thread),
            None => Ok(ChatThread::new(self.model_id.clone(), man_id.to_string())),
        }
    }

    pub fn write_chat(&self, thread: &ChatThread) -> Result<()> {
        let path = self.chat_file(&thread.man_id)?;
        write_json(&path, thread)
    }

    pub fn read_agent_log(&self, man_id: Option<&str>) -> Result<AgentLog> {
        match read_json::<AgentLog>(&self.agent_log_file(man_id)?)? {
            Some(mut log) => {
                log.man_id = man_id.map(str::to_string);
                Ok(log)
            }
            None => Ok(AgentLog::new(
                self.model_id.clone(),
                man_id.map(str::to_string),
            )),
        }
    }

    pub fn write_agent_log(&self, log: &AgentLog) -> Result<()> {
        let path = self.agent_log_file(log.man_id.as_deref())?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_json(&path, log)
    }

    pub fn append_agent_entry(&self, man_id: Option<&str>, entry: AgentEntry) -> Result<()> {
        let mut log = self.read_agent_log(man_id)?;
        log.entries.push(entry);
        if log.entries.len() > 600 {
            let cut = log.entries.len() - 600;
            log.entries.drain(0..cut);
        }
        self.write_agent_log(&log)
    }
}

// ----- json helpers -------------------------------------------------------

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path)?;
    if raw.trim().is_empty() {
        return Ok(None);
    }
    match serde_json::from_str::<T>(&raw) {
        Ok(v) => Ok(Some(v)),
        Err(first) => {
            // Doctor-style tolerance: retry once with a repaired copy of the text.
            match crate::doctor::repair_json_text(&raw) {
                Some(fixed) => match serde_json::from_str::<T>(&fixed) {
                    Ok(v) => Ok(Some(v)),
                    Err(_) => Err(AppError::Json(first)),
                },
                None => Err(AppError::Json(first)),
            }
        }
    }
}

/// Atomic write: serialise to `<file>.tmp`, then rename over the target.
pub fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_string_pretty(value)?;
    fs::write(&tmp, data)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

// ----- global index -------------------------------------------------------

pub fn rebuild_index(paths: &Paths) -> Result<GlobalIndex> {
    let mut index = GlobalIndex::default();
    for model_id in paths.list_model_ids()? {
        let scope = paths.scope(&model_id)?;
        let profile = match scope.read_profile() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let men = scope.read_all_men()?;
        index.models.push(IndexModel {
            id: profile.id.clone(),
            name: profile.name.clone(),
            site: profile.site.clone(),
            avatar: profile.avatar.clone(),
            men: men
                .iter()
                .map(|m| IndexMan {
                    id: m.id.clone(),
                    name: m.name.clone(),
                    tags: m.tags.clone(),
                    stage: m.stage.clone(),
                    keywords: m.keywords(),
                })
                .collect(),
        });
    }
    index.updated_at = chrono::Utc::now();
    write_json(&paths.index_file(), &index)?;
    Ok(index)
}

pub fn load_index(paths: &Paths) -> Result<GlobalIndex> {
    match read_json::<GlobalIndex>(&paths.index_file())? {
        Some(idx) => Ok(idx),
        None => rebuild_index(paths),
    }
}

/// Global cross-profile search used by the master agent.
pub fn global_search(paths: &Paths, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Ok(vec![]);
    }
    let terms: Vec<&str> = q.split_whitespace().collect();
    let index = load_index(paths)?;
    let mut hits = vec![];
    for model in &index.models {
        for man in &model.men {
            let hay = format!("{} {} {}", man.name.to_lowercase(), man.id, man.keywords);
            let mut score = 0u32;
            for term in &terms {
                if hay.contains(term) {
                    score += 1;
                }
                if man.name.to_lowercase() == *term || man.id == *term {
                    score += 5;
                }
            }
            if score > 0 {
                let snippet = snippet_around(&hay, terms[0]);
                hits.push(SearchHit {
                    model_id: model.id.clone(),
                    model_name: model.name.clone(),
                    man_id: man.id.clone(),
                    man_name: man.name.clone(),
                    snippet,
                    score,
                });
            }
        }
    }
    hits.sort_by_key(|h| std::cmp::Reverse(h.score));
    hits.truncate(limit);
    Ok(hits)
}

fn snippet_around(hay: &str, term: &str) -> String {
    match hay.find(term) {
        Some(pos) => {
            let start = pos.saturating_sub(60);
            let end = (pos + term.len() + 90).min(hay.len());
            let start = floor_char_boundary(hay, start);
            let end = floor_char_boundary(hay, end);
            hay[start..end].to_string()
        }
        None => hay.chars().take(120).collect(),
    }
}

fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths(tag: &str) -> Paths {
        let dir = std::env::temp_dir().join(format!("velvetdesk-test-{tag}-{}", new_id()));
        Paths::new(dir).unwrap()
    }

    #[test]
    fn rejects_traversal() {
        let paths = temp_paths("scope");
        let scope = paths.scope("2428653").unwrap();
        assert!(scope.resolve("../1984221/profile.json").is_err());
        assert!(scope.resolve("/etc/passwd").is_err());
        assert!(scope.man_file("../evil").is_err());
        assert!(scope.resolve("men/1219749.json").is_ok());
    }

    #[test]
    fn round_trips_man_and_index() {
        let paths = temp_paths("io");
        let scope = paths.scope("2428653").unwrap();
        scope
            .write_profile(&Profile::new("2428653".into(), "Marina".into()))
            .unwrap();
        let man = Man::new("2428653".into(), "1219749".into(), "Hartwig".into());
        scope.write_man(&man).unwrap();
        assert_eq!(scope.read_man("1219749").unwrap().name, "Hartwig");
        let index = rebuild_index(&paths).unwrap();
        assert_eq!(index.models.len(), 1);
        assert_eq!(index.models[0].men.len(), 1);
        let hits = global_search(&paths, "hartwig", 10).unwrap();
        assert_eq!(hits.len(), 1);
    }
}
