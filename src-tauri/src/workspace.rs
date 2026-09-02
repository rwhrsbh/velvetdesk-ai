//! Files and commands outside the profile sandbox.
//!
//! The scoped agent lives inside `app_data/profiles/<id>/` and cannot reach a
//! byte outside it. This module is the deliberate exception: it lets an agent
//! read a folder on the operator's disk, edit files there and run a shell
//! command — the way a coding assistant does — under three rules that make
//! that safe enough to offer:
//!
//! 1. **Nothing is reachable until it is granted.** The app data directory is
//!    always available; every other path has to be trusted first, and trust is
//!    per-root, remembered in settings.
//! 2. **A grant is the operator's alone.** An agent can ask for one, and the
//!    request waits in the approval queue. FULL ACCESS speeds up ordinary
//!    writes; it never hands out a new folder by itself.
//! 3. **Every destructive touch leaves a copy behind.** Overwrites and
//!    deletions are backed up first, so the doctor can put a file back.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

use crate::error::{AppError, Result};
use crate::models::new_id;
use crate::storage::Paths;

/// A folder the operator has allowed an agent into.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedRoot {
    pub path: String,
    /// False means the agent may read but not write.
    #[serde(default = "default_true")]
    pub writable: bool,
    #[serde(default = "now")]
    pub granted_at: chrono::DateTime<Utc>,
    /// What the agent said it needed the folder for.
    #[serde(default)]
    pub reason: String,
}

fn default_true() -> bool {
    true
}

fn now() -> chrono::DateTime<Utc> {
    Utc::now()
}

/// What a path is allowed to be used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

/// Resolve a path an agent asked for, refusing anything outside the trusted
/// roots. Returns the canonical path to work with.
///
/// The parent is canonicalised rather than the path itself so that creating a
/// new file works, while symlinks still cannot lead out of a trusted root.
pub fn resolve(
    paths: &Paths,
    roots: &[TrustedRoot],
    requested: &str,
    access: Access,
) -> Result<PathBuf> {
    let requested = requested.trim();
    if requested.is_empty() {
        return Err(AppError::Invalid("path is required".into()));
    }
    let candidate = PathBuf::from(requested);
    if !candidate.is_absolute() {
        return Err(AppError::Scope(format!(
            "{requested} is relative; give an absolute path"
        )));
    }
    if candidate
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(AppError::Scope(format!("{requested} contains ..")));
    }

    let real = canonical_target(&candidate)?;

    // The app's own data directory is always in reach: that is where profiles,
    // settings and backups live.
    let own = std::fs::canonicalize(&paths.root).unwrap_or_else(|_| paths.root.clone());
    if real.starts_with(&own) {
        return Ok(real);
    }

    for root in roots {
        let Ok(root_path) = std::fs::canonicalize(&root.path) else {
            continue;
        };
        if real.starts_with(&root_path) {
            if access == Access::Write && !root.writable {
                return Err(AppError::Scope(format!(
                    "{} is granted for reading only",
                    root.path
                )));
            }
            return Ok(real);
        }
    }

    Err(AppError::Scope(format!(
        "{requested} is outside every folder this agent may use — ask for access to it first"
    )))
}

/// Canonicalise what exists: the path itself, or its nearest existing parent
/// with the missing tail appended.
fn canonical_target(candidate: &Path) -> Result<PathBuf> {
    if let Ok(real) = std::fs::canonicalize(candidate) {
        return Ok(real);
    }
    let parent = candidate
        .parent()
        .ok_or_else(|| AppError::Scope("path has no parent".into()))?;
    let name = candidate
        .file_name()
        .ok_or_else(|| AppError::Scope("path has no file name".into()))?;
    let real_parent = std::fs::canonicalize(parent)
        .map_err(|_| AppError::NotFound(format!("{} does not exist", parent.display())))?;
    Ok(real_parent.join(name))
}

// ---------------------------------------------------------------------------
// Backups
// ---------------------------------------------------------------------------

/// One saved copy of a file as it was before an agent changed it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Backup {
    pub id: String,
    /// Where the file lives.
    pub original: String,
    /// Where the copy lives, inside the app data directory.
    pub copy: String,
    pub bytes: u64,
    pub reason: String,
    #[serde(default = "now")]
    pub created_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackupIndex {
    #[serde(default)]
    pub entries: Vec<Backup>,
}

pub fn backups_dir(paths: &Paths) -> PathBuf {
    paths.root.join("backups")
}

fn backup_index_file(paths: &Paths) -> PathBuf {
    backups_dir(paths).join("index.json")
}

pub fn read_backups(paths: &Paths) -> Result<BackupIndex> {
    Ok(crate::storage::read_json::<BackupIndex>(&backup_index_file(paths))?.unwrap_or_default())
}

/// Copy a file aside before it is overwritten or deleted.
///
/// Missing files are not an error: creating a new file has nothing to back up.
pub fn back_up(paths: &Paths, target: &Path, reason: &str) -> Result<Option<Backup>> {
    if !target.is_file() {
        return Ok(None);
    }
    let id = new_id();
    let dir = backups_dir(paths).join(&id);
    std::fs::create_dir_all(&dir)?;

    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let copy = dir.join(&name);
    std::fs::copy(target, &copy)?;

    let backup = Backup {
        id,
        original: target.to_string_lossy().to_string(),
        copy: copy.to_string_lossy().to_string(),
        bytes: std::fs::metadata(&copy).map(|m| m.len()).unwrap_or(0),
        reason: reason.to_string(),
        created_at: Utc::now(),
    };

    let mut index = read_backups(paths)?;
    index.entries.push(backup.clone());
    // Keep the history bounded; the oldest copies are removed with their files.
    while index.entries.len() > 300 {
        let oldest = index.entries.remove(0);
        let _ = std::fs::remove_dir_all(backups_dir(paths).join(&oldest.id));
    }
    crate::storage::write_json(&backup_index_file(paths), &index)?;
    Ok(Some(backup))
}

/// Put a backed-up file back where it came from.
pub fn restore(paths: &Paths, backup_id: &str) -> Result<PathBuf> {
    let index = read_backups(paths)?;
    let backup = index
        .entries
        .iter()
        .find(|b| b.id == backup_id)
        .ok_or_else(|| AppError::NotFound(format!("backup {backup_id}")))?;

    let original = PathBuf::from(&backup.original);
    if let Some(parent) = original.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // The file being replaced is itself worth keeping.
    let _ = back_up(paths, &original, "restore");
    std::fs::copy(&backup.copy, &original)?;
    Ok(original)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Paths {
        let dir = std::env::temp_dir().join(format!("velvet-ws-{}", new_id()));
        Paths::new(dir).unwrap()
    }

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("velvet-root-{name}-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn nothing_outside_a_granted_root_is_reachable() {
        let paths = paths();
        let granted = temp_root("granted");
        let other = temp_root("other");
        std::fs::write(other.join("secret.txt"), b"nope").unwrap();

        let roots = vec![TrustedRoot {
            path: granted.to_string_lossy().to_string(),
            writable: true,
            granted_at: Utc::now(),
            reason: String::new(),
        }];

        let inside = granted.join("notes.md");
        assert!(resolve(&paths, &roots, &inside.to_string_lossy(), Access::Write).is_ok());

        let outside = other.join("secret.txt");
        assert!(resolve(&paths, &roots, &outside.to_string_lossy(), Access::Read).is_err());

        // The app's own data is always available, granted or not.
        let own = paths.root.join("settings.json");
        std::fs::write(&own, b"{}").unwrap();
        assert!(resolve(&paths, &[], &own.to_string_lossy(), Access::Read).is_ok());
    }

    #[test]
    fn read_only_grants_refuse_writes() {
        let paths = paths();
        let root = temp_root("readonly");
        std::fs::write(root.join("a.txt"), b"hi").unwrap();
        let roots = vec![TrustedRoot {
            path: root.to_string_lossy().to_string(),
            writable: false,
            granted_at: Utc::now(),
            reason: String::new(),
        }];
        let file = root.join("a.txt");
        assert!(resolve(&paths, &roots, &file.to_string_lossy(), Access::Read).is_ok());
        assert!(resolve(&paths, &roots, &file.to_string_lossy(), Access::Write).is_err());
    }

    #[test]
    fn traversal_and_relative_paths_are_refused() {
        let paths = paths();
        let root = temp_root("traversal");
        let roots = vec![TrustedRoot {
            path: root.to_string_lossy().to_string(),
            writable: true,
            granted_at: Utc::now(),
            reason: String::new(),
        }];

        let escape = format!("{}/../secret.txt", root.to_string_lossy());
        assert!(resolve(&paths, &roots, &escape, Access::Read).is_err());
        assert!(resolve(&paths, &roots, "relative/path.txt", Access::Read).is_err());
    }

    #[test]
    fn a_backup_survives_the_file_being_replaced() {
        let paths = paths();
        let root = temp_root("backup");
        let file = root.join("letter.txt");
        std::fs::write(&file, b"original text").unwrap();

        let backup = back_up(&paths, &file, "test overwrite").unwrap().unwrap();
        std::fs::write(&file, b"clobbered").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "clobbered");

        let restored = restore(&paths, &backup.id).unwrap();
        assert_eq!(restored, file);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "original text");

        // Restoring kept a copy of the clobbered version too.
        let index = read_backups(&paths).unwrap();
        assert_eq!(index.entries.len(), 2);
    }

    #[test]
    fn backing_up_a_missing_file_is_not_an_error() {
        let paths = paths();
        let root = temp_root("missing");
        assert!(back_up(&paths, &root.join("nope.txt"), "test")
            .unwrap()
            .is_none());
    }
}
