//! File and shell tools.
//!
//! These are the tools that reach outside the app's own data. Reads and writes
//! are checked against the trusted roots in [`crate::workspace`]; a write backs
//! the previous version up first; a shell command is treated as destructive, so
//! it runs immediately only under FULL ACCESS and waits in the approval queue
//! otherwise.

use serde_json::{json, Value};
use std::time::Duration;

use super::tools::{PendingAction, Phrase, Risk, ToolOutcome};
use crate::config::SecurityLevel;
use crate::error::{AppError, Result};
use crate::llm::ToolDef;
use crate::models::new_id;
use crate::storage::Paths;
use crate::workspace::{self, Access, TrustedRoot};

/// How long a command may run before it is killed.
const SHELL_TIMEOUT: Duration = Duration::from_secs(120);
/// Cap on what a read or a command returns, so one call cannot fill the window.
const MAX_OUTPUT: usize = 24_000;

pub fn tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "fs_list".into(),
            description: "List a directory the agent has access to.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "absolute path" } },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "fs_read".into(),
            description: "Read a text file, optionally a range of lines.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "absolute path" },
                    "from_line": { "type": "integer", "description": "1-based, optional" },
                    "to_line": { "type": "integer", "description": "inclusive, optional" }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "fs_write".into(),
            description: "Write a file, replacing it entirely. The previous version is backed up."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        },
        ToolDef {
            name: "fs_edit".into(),
            description: "Replace an exact fragment of a file. Fails unless it appears once."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_text": { "type": "string", "description": "text to replace, verbatim" },
                    "new_text": { "type": "string" }
                },
                "required": ["path", "old_text", "new_text"]
            }),
        },
        ToolDef {
            name: "fs_delete".into(),
            description: "Delete a file. A copy is kept so it can be restored.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "shell".into(),
            description: "Run one shell command in a folder the agent has access to.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "the command line" },
                    "cwd": { "type": "string", "description": "absolute path to run it in" },
                    "shell": { "type": "string", "description": "powershell | bash; defaults to the platform's own" }
                },
                "required": ["command", "cwd"]
            }),
        },
        ToolDef {
            name: "request_access".into(),
            description: "Ask the operator for access to a folder. Always needs a human answer."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "absolute path to the folder" },
                    "reason": { "type": "string", "description": "why it is needed, one line" },
                    "writable": { "type": "boolean", "description": "false asks for read-only" }
                },
                "required": ["path", "reason"]
            }),
        },
    ]
}

pub fn is_workspace_tool(tool: &str) -> bool {
    matches!(
        tool,
        "fs_list" | "fs_read" | "fs_write" | "fs_edit" | "fs_delete" | "shell" | "request_access"
    )
}

pub fn risk_of(tool: &str) -> Risk {
    match tool {
        "fs_list" | "fs_read" => Risk::Read,
        "fs_write" | "fs_edit" => Risk::Write,
        // Deleting and running commands are the two that can lose work.
        _ => Risk::Destructive,
    }
}

/// Run one workspace tool.
pub fn execute(
    paths: &Paths,
    roots: &[TrustedRoot],
    security: SecurityLevel,
    tool: &str,
    args: &Value,
) -> Result<ToolOutcome> {
    match tool {
        "fs_list" => fs_list(paths, roots, args),
        "fs_read" => fs_read(paths, roots, args),
        // A folder grant is never automatic — not even under FULL ACCESS.
        "request_access" => request_access(args),
        _ => {
            if !super::tools::is_allowed(security, risk_of(tool)) {
                return Ok(queued(tool, args, risk_of(tool), summary_for(tool, args)));
            }
            match tool {
                "fs_write" => fs_write(paths, roots, args),
                "fs_edit" => fs_edit(paths, roots, args),
                "fs_delete" => fs_delete(paths, roots, args),
                "shell" => shell(paths, roots, args),
                other => Err(AppError::Invalid(format!("unknown tool: {other}"))),
            }
        }
    }
}

/// Apply a workspace action the operator approved in the queue.
pub fn commit(paths: &Paths, roots: &[TrustedRoot], tool: &str, args: &Value) -> Result<String> {
    let outcome = match tool {
        "fs_write" => fs_write(paths, roots, args)?,
        "fs_edit" => fs_edit(paths, roots, args)?,
        "fs_delete" => fs_delete(paths, roots, args)?,
        "shell" => shell(paths, roots, args)?,
        other => return Err(AppError::Invalid(format!("unknown tool: {other}"))),
    };
    Ok(outcome.summary)
}

fn arg_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| AppError::Invalid(format!("{key} is required")))
}

fn summary_for(tool: &str, args: &Value) -> Phrase {
    let path = args
        .get("path")
        .or_else(|| args.get("cwd"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let command = args.get("command").and_then(|c| c.as_str()).unwrap_or("");
    match tool {
        "fs_write" => Phrase::new(
            "step.fsWrite",
            json!({ "path": path }),
            format!("записать файл {path}"),
        ),
        "fs_edit" => Phrase::new(
            "step.fsEdit",
            json!({ "path": path }),
            format!("правка файла {path}"),
        ),
        "fs_delete" => Phrase::new(
            "step.fsDelete",
            json!({ "path": path }),
            format!("удалить файл {path}"),
        ),
        "shell" => Phrase::new(
            "step.shellRun",
            json!({ "command": command }),
            format!("выполнить: {command}"),
        ),
        other => Phrase::new("", json!({}), other.to_string()),
    }
}

fn queued(tool: &str, args: &Value, risk: Risk, phrase: Phrase) -> ToolOutcome {
    let pending = PendingAction {
        id: new_id(),
        model_id: String::new(),
        tool: tool.to_string(),
        args: args.clone(),
        risk,
        summary: phrase.text.clone(),
        key: phrase.key.clone(),
        params: phrase.params.clone(),
        before: Value::Null,
        after: args.clone(),
        created_at: chrono::Utc::now(),
    };
    ToolOutcome {
        tool: tool.to_string(),
        risk,
        result: json!({ "ok": true, "applied": false, "pending_approval": true }),
        applied: false,
        queued: Some(pending),
        changes: Value::Null,
        summary: phrase.text.clone(),
        phrase,
    }
}

fn done(tool: &str, risk: Risk, result: Value, phrase: Phrase) -> ToolOutcome {
    ToolOutcome {
        tool: tool.to_string(),
        risk,
        result,
        applied: true,
        queued: None,
        changes: Value::Null,
        summary: phrase.text.clone(),
        phrase,
    }
}

fn truncate(text: String) -> (String, bool) {
    if text.len() <= MAX_OUTPUT {
        return (text, false);
    }
    let mut cut = MAX_OUTPUT;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    (text[..cut].to_string(), true)
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

fn fs_list(paths: &Paths, roots: &[TrustedRoot], args: &Value) -> Result<ToolOutcome> {
    let requested = arg_str(args, "path")?;
    let dir = workspace::resolve(paths, roots, &requested, Access::Read)?;
    let mut entries = vec![];
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        entries.push(json!({
            "name": entry.file_name().to_string_lossy(),
            "kind": if meta.is_dir() { "dir" } else { "file" },
            "bytes": if meta.is_file() { meta.len() } else { 0 },
        }));
        if entries.len() >= 500 {
            break;
        }
    }
    Ok(done(
        "fs_list",
        Risk::Read,
        json!({ "path": dir.to_string_lossy(), "entries": entries }),
        Phrase::new(
            "step.fsList",
            json!({ "path": workspace::display_path(&dir) }),
            format!("список {}", workspace::display_path(&dir)),
        ),
    ))
}

fn fs_read(paths: &Paths, roots: &[TrustedRoot], args: &Value) -> Result<ToolOutcome> {
    let requested = arg_str(args, "path")?;
    let file = workspace::resolve(paths, roots, &requested, Access::Read)?;
    let text = std::fs::read_to_string(&file)
        .map_err(|e| AppError::Invalid(format!("{}: {e}", file.display())))?;

    let from = args.get("from_line").and_then(|v| v.as_u64()).unwrap_or(1);
    let to = args
        .get("to_line")
        .and_then(|v| v.as_u64())
        .unwrap_or(u64::MAX);
    let selected: String = text
        .lines()
        .enumerate()
        .filter(|(i, _)| {
            let line = *i as u64 + 1;
            line >= from && line <= to
        })
        .map(|(i, line)| format!("{}: {line}\n", i + 1))
        .collect();

    let (content, truncated) = truncate(selected);
    Ok(done(
        "fs_read",
        Risk::Read,
        json!({ "path": file.to_string_lossy(), "content": content, "truncated": truncated }),
        Phrase::new(
            "step.fsRead",
            json!({ "path": workspace::display_path(&file) }),
            format!("чтение {}", workspace::display_path(&file)),
        ),
    ))
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

fn fs_write(paths: &Paths, roots: &[TrustedRoot], args: &Value) -> Result<ToolOutcome> {
    let requested = arg_str(args, "path")?;
    let content = arg_str(args, "content")?;
    let file = workspace::resolve(paths, roots, &requested, Access::Write)?;

    let backup = workspace::back_up(paths, &file, "fs_write")?;
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&file, content.as_bytes())?;

    Ok(done(
        "fs_write",
        Risk::Write,
        json!({
            "path": file.to_string_lossy(),
            "bytes": content.len(),
            "backup": backup.as_ref().map(|b| b.id.clone()),
        }),
        Phrase::new(
            "step.fsWritten",
            json!({ "path": workspace::display_path(&file) }),
            format!("записан {}", workspace::display_path(&file)),
        ),
    ))
}

fn fs_edit(paths: &Paths, roots: &[TrustedRoot], args: &Value) -> Result<ToolOutcome> {
    let requested = arg_str(args, "path")?;
    let old_text = arg_str(args, "old_text")?;
    let new_text = arg_str(args, "new_text")?;
    let file = workspace::resolve(paths, roots, &requested, Access::Write)?;

    let text = std::fs::read_to_string(&file)
        .map_err(|e| AppError::Invalid(format!("{}: {e}", file.display())))?;
    let hits = text.matches(&old_text).count();
    if hits == 0 {
        return Err(AppError::Invalid(
            "old_text does not appear in the file".into(),
        ));
    }
    if hits > 1 {
        return Err(AppError::Invalid(format!(
            "old_text appears {hits} times; include more context so it is unique"
        )));
    }

    let backup = workspace::back_up(paths, &file, "fs_edit")?;
    std::fs::write(&file, text.replacen(&old_text, &new_text, 1).as_bytes())?;

    Ok(done(
        "fs_edit",
        Risk::Write,
        json!({
            "path": file.to_string_lossy(),
            "backup": backup.as_ref().map(|b| b.id.clone()),
        }),
        Phrase::new(
            "step.fsEdited",
            json!({ "path": workspace::display_path(&file) }),
            format!("правка {}", workspace::display_path(&file)),
        ),
    ))
}

fn fs_delete(paths: &Paths, roots: &[TrustedRoot], args: &Value) -> Result<ToolOutcome> {
    let requested = arg_str(args, "path")?;
    let file = workspace::resolve(paths, roots, &requested, Access::Write)?;
    if !file.is_file() {
        return Err(AppError::NotFound(format!("{}", file.display())));
    }
    let backup = workspace::back_up(paths, &file, "fs_delete")?;
    std::fs::remove_file(&file)?;

    Ok(done(
        "fs_delete",
        Risk::Destructive,
        json!({
            "path": file.to_string_lossy(),
            "backup": backup.as_ref().map(|b| b.id.clone()),
            "restorable": backup.is_some(),
        }),
        Phrase::new(
            "step.fsDeleted",
            json!({ "path": workspace::display_path(&file) }),
            format!("удалён {}", workspace::display_path(&file)),
        ),
    ))
}

// ---------------------------------------------------------------------------
// Shell
// ---------------------------------------------------------------------------

fn shell(paths: &Paths, roots: &[TrustedRoot], args: &Value) -> Result<ToolOutcome> {
    let command = arg_str(args, "command")?;
    let cwd = arg_str(args, "cwd")?;
    let dir = workspace::resolve(paths, roots, &cwd, Access::Write)?;
    if !dir.is_dir() {
        return Err(AppError::Invalid(format!(
            "{} is not a folder",
            dir.display()
        )));
    }

    let requested_shell = args
        .get("shell")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_lowercase();

    let mut process = if cfg!(windows) && requested_shell != "bash" {
        let mut c = std::process::Command::new("powershell.exe");
        c.args(["-NoProfile", "-NonInteractive", "-Command", &command]);
        c
    } else {
        let mut c = std::process::Command::new("bash");
        c.args(["-lc", &command]);
        c
    };
    process.current_dir(&dir);

    let output = run_with_timeout(process)?;
    let (stdout, out_cut) = truncate(String::from_utf8_lossy(&output.stdout).to_string());
    let (stderr, err_cut) = truncate(String::from_utf8_lossy(&output.stderr).to_string());

    Ok(done(
        "shell",
        Risk::Destructive,
        json!({
            "exit_code": output.status.code(),
            "stdout": stdout,
            "stderr": stderr,
            "truncated": out_cut || err_cut,
        }),
        Phrase::new(
            "step.shellDone",
            json!({ "command": command, "code": output.status.code() }),
            format!("выполнено: {command}"),
        ),
    ))
}

/// Wait for a command, killing it if it overruns.
fn run_with_timeout(mut command: std::process::Command) -> Result<std::process::Output> {
    use std::io::Read;

    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|e| AppError::Invalid(format!("cannot start the shell: {e}")))?;

    let started = std::time::Instant::now();
    loop {
        match child.try_wait()? {
            Some(status) => {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(mut out) = child.stdout.take() {
                    let _ = out.read_to_end(&mut stdout);
                }
                if let Some(mut err) = child.stderr.take() {
                    let _ = err.read_to_end(&mut stderr);
                }
                return Ok(std::process::Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            None if started.elapsed() > SHELL_TIMEOUT => {
                let _ = child.kill();
                return Err(AppError::Invalid(format!(
                    "the command ran longer than {} s and was stopped",
                    SHELL_TIMEOUT.as_secs()
                )));
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

// ---------------------------------------------------------------------------
// Access requests
// ---------------------------------------------------------------------------

/// Ask for a folder. This never applies by itself: the operator answers it in
/// the approval queue, and only then does the folder become reachable.
fn request_access(args: &Value) -> Result<ToolOutcome> {
    let path = arg_str(args, "path")?;
    let reason = arg_str(args, "reason")?;
    let writable = args
        .get("writable")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let phrase = Phrase::new(
        "step.requestAccess",
        json!({ "path": path, "reason": reason, "writable": writable }),
        format!(
            "доступ к папке {path} ({}) — {reason}",
            if writable {
                "чтение и запись"
            } else {
                "чтение"
            }
        ),
    );
    let summary = phrase.text.clone();
    let pending = PendingAction {
        id: new_id(),
        model_id: String::new(),
        tool: "request_access".into(),
        args: json!({ "path": path, "reason": reason, "writable": writable }),
        risk: Risk::Destructive,
        summary: summary.clone(),
        key: phrase.key.clone(),
        params: phrase.params.clone(),
        before: Value::Null,
        after: json!({ "path": path, "writable": writable, "reason": reason }),
        created_at: chrono::Utc::now(),
    };

    Ok(ToolOutcome {
        tool: "request_access".into(),
        risk: Risk::Destructive,
        result: json!({
            "ok": true,
            "applied": false,
            "pending_approval": true,
            "note": "the operator has to answer this before the folder can be used",
        }),
        applied: false,
        queued: Some(pending),
        changes: Value::Null,
        summary,
        phrase,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Paths {
        let dir = std::env::temp_dir().join(format!("velvet-wst-{}", new_id()));
        Paths::new(dir).unwrap()
    }

    fn root() -> (std::path::PathBuf, Vec<TrustedRoot>) {
        let dir = std::env::temp_dir().join(format!("velvet-wsroot-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let roots = vec![TrustedRoot {
            path: dir.to_string_lossy().to_string(),
            writable: true,
            granted_at: chrono::Utc::now(),
            reason: String::new(),
        }];
        (dir, roots)
    }

    #[test]
    fn writing_backs_up_what_was_there() {
        let paths = paths();
        let (dir, roots) = root();
        let file = dir.join("letter.txt");
        std::fs::write(&file, b"first draft").unwrap();

        let outcome = execute(
            &paths,
            &roots,
            SecurityLevel::Yolo,
            "fs_write",
            &json!({ "path": file.to_string_lossy(), "content": "second draft" }),
        )
        .unwrap();
        assert!(outcome.applied);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "second draft");

        let backup_id = outcome.result["backup"].as_str().unwrap().to_string();
        workspace::restore(&paths, &backup_id).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "first draft");
    }

    #[test]
    fn deleting_leaves_something_to_restore() {
        let paths = paths();
        let (dir, roots) = root();
        let file = dir.join("notes.md");
        std::fs::write(&file, b"keep me").unwrap();

        let outcome = execute(
            &paths,
            &roots,
            SecurityLevel::Yolo,
            "fs_delete",
            &json!({ "path": workspace::display_path(&file) }),
        )
        .unwrap();
        assert!(!file.exists());

        let backup_id = outcome.result["backup"].as_str().unwrap().to_string();
        workspace::restore(&paths, &backup_id).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep me");
    }

    #[test]
    fn an_edit_must_match_exactly_once() {
        let paths = paths();
        let (dir, roots) = root();
        let file = dir.join("dup.txt");
        std::fs::write(&file, b"alpha\nalpha\n").unwrap();

        let twice = execute(
            &paths,
            &roots,
            SecurityLevel::Yolo,
            "fs_edit",
            &json!({ "path": file.to_string_lossy(), "old_text": "alpha", "new_text": "beta" }),
        );
        assert!(twice.is_err(), "an ambiguous edit must be refused");

        std::fs::write(&file, b"alpha\nbeta\n").unwrap();
        execute(
            &paths,
            &roots,
            SecurityLevel::Yolo,
            "fs_edit",
            &json!({ "path": file.to_string_lossy(), "old_text": "beta", "new_text": "gamma" }),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\ngamma\n");
    }

    /// Writing and running commands wait for approval below FULL ACCESS.
    #[test]
    fn writes_and_commands_are_queued_unless_full_access() {
        let paths = paths();
        let (dir, roots) = root();
        let file = dir.join("queued.txt");

        for security in [SecurityLevel::Ask, SecurityLevel::Safe] {
            let outcome = execute(
                &paths,
                &roots,
                security,
                "shell",
                &json!({ "command": "echo hi", "cwd": dir.to_string_lossy() }),
            )
            .unwrap();
            assert!(!outcome.applied, "{security:?} must queue a command");
            assert!(outcome.queued.is_some());
        }

        let outcome = execute(
            &paths,
            &roots,
            SecurityLevel::Ask,
            "fs_write",
            &json!({ "path": file.to_string_lossy(), "content": "x" }),
        )
        .unwrap();
        assert!(!outcome.applied);
        assert!(!file.exists(), "nothing may be written before approval");

        // SAFE applies ordinary writes.
        let outcome = execute(
            &paths,
            &roots,
            SecurityLevel::Safe,
            "fs_write",
            &json!({ "path": file.to_string_lossy(), "content": "x" }),
        )
        .unwrap();
        assert!(outcome.applied);
    }

    /// A folder grant is the operator's decision — FULL ACCESS does not make it
    /// automatic.
    #[test]
    fn access_requests_always_wait_for_a_human() {
        let paths = paths();
        let (dir, _) = root();

        for security in [SecurityLevel::Ask, SecurityLevel::Safe, SecurityLevel::Yolo] {
            let outcome = execute(
                &paths,
                &[],
                security,
                "request_access",
                &json!({ "path": dir.to_string_lossy(), "reason": "нужны письма" }),
            )
            .unwrap();
            assert!(
                !outcome.applied,
                "{security:?} must not grant access itself"
            );
            assert_eq!(outcome.queued.unwrap().tool, "request_access");
        }
    }

    #[test]
    fn untrusted_folders_are_out_of_reach() {
        let paths = paths();
        let (dir, _) = root();
        std::fs::write(dir.join("secret.txt"), b"private").unwrap();

        let result = execute(
            &paths,
            &[],
            SecurityLevel::Yolo,
            "fs_read",
            &json!({ "path": dir.join("secret.txt").to_string_lossy() }),
        );
        assert!(result.is_err());
    }

    #[test]
    fn a_command_runs_in_the_folder_it_was_given() {
        let paths = paths();
        let (dir, roots) = root();
        std::fs::write(dir.join("marker.txt"), b"hi").unwrap();

        let outcome = execute(
            &paths,
            &roots,
            SecurityLevel::Yolo,
            "shell",
            &json!({
                "command": if cfg!(windows) { "Get-ChildItem -Name" } else { "ls" },
                "cwd": dir.to_string_lossy(),
            }),
        )
        .unwrap();

        assert_eq!(outcome.result["exit_code"], 0);
        assert!(
            outcome.result["stdout"]
                .as_str()
                .unwrap()
                .contains("marker.txt"),
            "stdout was {:?}",
            outcome.result["stdout"]
        );
    }
}
