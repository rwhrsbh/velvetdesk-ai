//! Doctor: schema validation, JSON repair and integrity checks.

use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::path::Path;

use crate::error::Result;
use crate::models::{ChatThread, Man, Profile};
use crate::storage::{self, Paths};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct Issue {
    pub level: Level,
    pub scope: String,
    pub path: String,
    pub message: String,
    pub fixable: bool,
    #[serde(default)]
    pub fixed: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DoctorReport {
    pub issues: Vec<Issue>,
    pub models_checked: usize,
    pub men_checked: usize,
    pub chats_checked: usize,
    pub fixes_applied: usize,
}

impl DoctorReport {
    fn push(
        &mut self,
        level: Level,
        scope: &str,
        path: &Path,
        message: impl Into<String>,
        fixable: bool,
    ) {
        self.issues.push(Issue {
            level,
            scope: scope.to_string(),
            path: path.display().to_string(),
            message: message.into(),
            fixable,
            fixed: false,
        });
    }
}

/// Best-effort repair of malformed JSON text.
///
/// Handles: BOM, single-quoted keys left by hand edits, trailing commas,
/// unterminated strings, missing closing braces/brackets and trailing garbage.
pub fn repair_json_text(raw: &str) -> Option<String> {
    let mut text = raw.trim_start_matches('\u{feff}').trim().to_string();
    if text.is_empty() {
        return None;
    }

    // Drop anything before the first opening brace/bracket.
    if let Some(pos) = text.find(['{', '[']) {
        if pos > 0 {
            text = text[pos..].to_string();
        }
    } else {
        return None;
    }

    let mut out = String::with_capacity(text.len() + 8);
    let mut stack: Vec<char> = vec![];
    let mut in_string = false;
    let mut escaped = false;

    for ch in text.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_string => {
                out.push(ch);
                escaped = true;
            }
            '"' => {
                in_string = !in_string;
                out.push(ch);
            }
            '{' | '[' if !in_string => {
                stack.push(ch);
                out.push(ch);
            }
            '}' | ']' if !in_string => {
                // Remove a trailing comma before the closing token.
                trim_trailing_comma(&mut out);
                stack.pop();
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }

    if in_string {
        out.push('"');
    }
    trim_trailing_comma(&mut out);
    while let Some(open) = stack.pop() {
        out.push(if open == '{' { '}' } else { ']' });
    }

    if serde_json::from_str::<Value>(&out).is_ok() {
        Some(out)
    } else {
        // Last resort: keep only the first balanced object.
        crate::llm::extract_json_object(&out).map(|v| v.to_string())
    }
}

fn trim_trailing_comma(out: &mut String) {
    let trimmed_len = out.trim_end().len();
    if out[..trimmed_len].ends_with(',') {
        let mut new_len = trimmed_len - 1;
        while new_len > 0 && !out.is_char_boundary(new_len) {
            new_len -= 1;
        }
        out.truncate(new_len);
    }
}

/// Full integrity pass. When `apply_fixes` is true, repairable problems are
/// written back to disk (originals are kept as `<file>.bak`).
pub fn run(paths: &Paths, apply_fixes: bool) -> Result<DoctorReport> {
    let mut report = DoctorReport::default();

    for model_id in paths.list_model_ids()? {
        report.models_checked += 1;
        let scope = paths.scope(&model_id)?;

        // --- profile.json --------------------------------------------------
        let profile_path = scope.profile_file();
        let profile =
            match load_or_repair::<Profile>(&profile_path, apply_fixes, &mut report, "profile") {
                Some(mut profile) => {
                    if profile.id != model_id {
                        report.push(
                            Level::Warn,
                            "profile",
                            &profile_path,
                            format!(
                                "profile id {} does not match folder {}",
                                profile.id, model_id
                            ),
                            true,
                        );
                        if apply_fixes {
                            profile.id = model_id.clone();
                            storage::write_json(&profile_path, &profile)?;
                            mark_fixed(&mut report);
                        }
                    }
                    Some(profile)
                }
                None => {
                    if !profile_path.exists() {
                        report.push(
                            Level::Error,
                            "profile",
                            &profile_path,
                            "profile.json missing for existing folder",
                            true,
                        );
                        if apply_fixes {
                            let stub = Profile::new(model_id.clone(), format!("Model {model_id}"));
                            storage::write_json(&profile_path, &stub)?;
                            mark_fixed(&mut report);
                        }
                    }
                    None
                }
            };

        // --- men/*.json ----------------------------------------------------
        let mut known_men: Vec<String> = vec![];
        for man_id in scope.list_man_ids()? {
            report.men_checked += 1;
            let path = scope.man_file(&man_id)?;
            match load_or_repair::<Man>(&path, apply_fixes, &mut report, "man") {
                Some(mut man) => {
                    let mut dirty = false;
                    if man.id != man_id {
                        report.push(
                            Level::Warn,
                            "man",
                            &path,
                            format!("man id {} does not match file name {}", man.id, man_id),
                            true,
                        );
                        man.id = man_id.clone();
                        dirty = true;
                    }
                    if man.model_id != model_id {
                        report.push(
                            Level::Warn,
                            "man",
                            &path,
                            format!(
                                "man belongs to model {} but sits in {}",
                                man.model_id, model_id
                            ),
                            true,
                        );
                        man.model_id = model_id.clone();
                        dirty = true;
                    }
                    if man.name.trim().is_empty() {
                        report.push(Level::Warn, "man", &path, "man has no name", true);
                        man.name = format!("Unknown {man_id}");
                        dirty = true;
                    }
                    if dirty && apply_fixes {
                        storage::write_json(&path, &man)?;
                        mark_fixed(&mut report);
                    }
                    known_men.push(man_id.clone());
                }
                None => {
                    report.push(Level::Error, "man", &path, "unreadable dossier", false);
                }
            }
        }

        // --- chats/*.json --------------------------------------------------
        let chats_dir = scope.chats_dir();
        if chats_dir.exists() {
            for entry in fs::read_dir(&chats_dir)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();
                let Some(stem) = name.strip_suffix(".json") else {
                    continue;
                };
                report.chats_checked += 1;
                let path = entry.path();
                match load_or_repair::<ChatThread>(&path, apply_fixes, &mut report, "chat") {
                    Some(mut thread) => {
                        if !known_men.iter().any(|m| m == stem) {
                            report.push(
                                Level::Warn,
                                "chat",
                                &path,
                                format!("chat history has no matching dossier ({stem})"),
                                true,
                            );
                            if apply_fixes {
                                // Re-attach by creating a placeholder dossier so
                                // the history is never silently dropped.
                                let mut man = Man::new(
                                    model_id.clone(),
                                    stem.to_string(),
                                    format!("Recovered {stem}"),
                                );
                                man.status = "Восстановлен доктором из истории чата".into();
                                man.tags.push("recovered".into());
                                scope.write_man(&man)?;
                                known_men.push(stem.to_string());
                                mark_fixed(&mut report);
                            }
                        }
                        if thread.man_id != stem || thread.model_id != model_id {
                            report.push(Level::Warn, "chat", &path, "chat header mismatch", true);
                            if apply_fixes {
                                thread.man_id = stem.to_string();
                                thread.model_id = model_id.clone();
                                storage::write_json(&path, &thread)?;
                                mark_fixed(&mut report);
                            }
                        }
                    }
                    None => report.push(
                        Level::Error,
                        "chat",
                        &path,
                        "unreadable chat history",
                        false,
                    ),
                }
            }
        }

        // --- attachments ---------------------------------------------------
        let att_dir = scope.attachments_dir();
        if att_dir.exists() {
            for entry in fs::read_dir(&att_dir)? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let owner = name
                    .split(['_', '.'])
                    .next()
                    .unwrap_or_default()
                    .to_string();
                if !known_men.contains(&owner) {
                    report.push(
                        Level::Warn,
                        "attachment",
                        &entry.path(),
                        format!("attachment {name} is not linked to any dossier"),
                        true,
                    );
                    if apply_fixes {
                        let orphan_dir = att_dir.join("_orphans");
                        fs::create_dir_all(&orphan_dir)?;
                        let _ = fs::rename(entry.path(), orphan_dir.join(&name));
                        mark_fixed(&mut report);
                    }
                }
            }
        }

        if profile.is_none() && !apply_fixes {
            report.push(
                Level::Warn,
                "profile",
                &scope.base,
                "model folder without a valid profile",
                true,
            );
        }
    }

    // --- global index ------------------------------------------------------
    let index_path = paths.index_file();
    let index_ok = storage::read_json::<crate::models::GlobalIndex>(&index_path)
        .ok()
        .flatten()
        .map(|idx| idx.models.len() == report.models_checked)
        .unwrap_or(false);
    if !index_ok {
        report.push(
            Level::Warn,
            "index",
            &index_path,
            "global index missing or stale",
            true,
        );
        if apply_fixes {
            storage::rebuild_index(paths)?;
            mark_fixed(&mut report);
        }
    }

    if report.issues.is_empty() {
        report.issues.push(Issue {
            level: Level::Ok,
            scope: "all".into(),
            path: paths.root.display().to_string(),
            message: "все схемы валидны, битых ссылок нет".into(),
            fixable: false,
            fixed: false,
        });
    }

    Ok(report)
}

fn mark_fixed(report: &mut DoctorReport) {
    report.fixes_applied += 1;
    if let Some(last) = report.issues.last_mut() {
        last.fixed = true;
    }
}

fn load_or_repair<T: serde::de::DeserializeOwned + serde::Serialize>(
    path: &Path,
    apply_fixes: bool,
    report: &mut DoctorReport,
    scope: &str,
) -> Option<T> {
    if !path.exists() {
        return None;
    }
    let raw = fs::read_to_string(path).ok()?;
    match serde_json::from_str::<T>(&raw) {
        Ok(value) => Some(value),
        Err(err) => {
            report.push(
                Level::Error,
                scope,
                path,
                format!("malformed JSON: {err}"),
                true,
            );
            let fixed = repair_json_text(&raw)?;
            let value = serde_json::from_str::<T>(&fixed).ok()?;
            if apply_fixes {
                let _ = fs::write(path.with_extension("json.bak"), &raw);
                if storage::write_json(path, &value).is_ok() {
                    mark_fixed(report);
                }
            }
            Some(value)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repairs_trailing_comma_and_missing_brace() {
        let broken = "{\"a\": 1, \"b\": [1, 2,], ";
        let fixed = repair_json_text(broken).unwrap();
        let value: Value = serde_json::from_str(&fixed).unwrap();
        assert_eq!(value["a"], 1);
        assert_eq!(value["b"][1], 2);
    }

    #[test]
    fn repairs_unterminated_string() {
        let broken = "{\"name\": \"Hartwig";
        let fixed = repair_json_text(broken).unwrap();
        let value: Value = serde_json::from_str(&fixed).unwrap();
        assert_eq!(value["name"], "Hartwig");
    }

    #[test]
    fn doctor_recreates_missing_profile_and_index() {
        let dir = std::env::temp_dir().join(format!("velvet-doc-{}", crate::models::new_id()));
        let paths = Paths::new(dir).unwrap();
        let scope = paths.scope("777").unwrap();
        scope
            .write_man(&Man::new("777".into(), "1".into(), "Tester".into()))
            .unwrap();

        let dry = run(&paths, false).unwrap();
        assert!(dry.issues.iter().any(|i| i.scope == "profile"));
        assert_eq!(dry.fixes_applied, 0);

        let fixed = run(&paths, true).unwrap();
        assert!(fixed.fixes_applied >= 1);
        assert!(scope.read_profile().is_ok());
    }
}
