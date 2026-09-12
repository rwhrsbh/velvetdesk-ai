//! Sync through the gateway's mailbox.
//!
//! The live relay needs both machines awake at the same second. Two people on
//! one licence rarely are — one works mornings, the other evenings — and the
//! evening shift would find nothing waiting for it. So each device leaves its
//! records in a mailbox on the gateway and collects what it has not seen.
//!
//! What the gateway holds is ciphertext under a key derived from the licence,
//! which it never stores: it can say that a record changed and when, and
//! nothing about what is in it. It keeps one row per record rather than a
//! log, because what anybody needs is the newest version of a dossier, not
//! the seventeen versions it passed through.
//!
//! The alternatives, and why not:
//!
//!   * live relay only — what we had. Zero storage, and useless to a team
//!     that does not overlap. Kept alongside this for the case where both
//!     are on at once, where it is instant.
//!   * a full operation log with vector clocks — correct in the general
//!     case, and the general case is not ours: records are whole documents
//!     written by one person at a time, and "newest wins, loser goes to
//!     backups" is what an operator can actually reason about.
//!   * some third-party file sync — another account to hold, another place
//!     the correspondence lives, and no way to bound what it keeps.

use std::collections::HashMap;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{AppError, Result};
use crate::storage::{read_json, write_json, Paths};
use crate::sync::pair::Pairing;
use crate::sync::wire::{open, seal, Msg};
use crate::sync::{apply_item, digest, note_agreed, read_base, read_item, Applied, Report};

/// How far through the mailbox this device has read.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Cursor {
    #[serde(default)]
    seq: i64,
}

fn cursor_file(paths: &Paths) -> std::path::PathBuf {
    paths.root.join("sync-cursor.json")
}

fn read_cursor(paths: &Paths) -> Cursor {
    read_json::<Cursor>(&cursor_file(paths))
        .ok()
        .flatten()
        .unwrap_or_default()
}

fn write_cursor(paths: &Paths, cursor: &Cursor) -> Result<()> {
    write_json(&cursor_file(paths), cursor)
}

/// Start again from the beginning of the mailbox.
///
/// For a device that has just joined a licence: everything the others have
/// left is news to it, however old.
pub fn rewind(paths: &Paths) -> Result<()> {
    write_cursor(paths, &Cursor::default())
}

/// The gateway's sync endpoints, from the cloud provider's base URL.
fn endpoint(relay: &str, path: &str) -> String {
    let base = relay.trim_end_matches('/').trim_end_matches("/v1");
    format!("{base}{path}")
}

/// One round: leave what is ours, collect what is not.
///
/// Pushing first is deliberate. A device that has been away comes back with
/// work of its own and with a mailbox full of somebody else's; if it pulled
/// first, every conflict would be decided before its own version had been
/// offered. Its records go up, then it reads, and both sides end up judging
/// the same pair of revisions.
pub async fn run_round(
    http: &reqwest::Client,
    paths: &Paths,
    pairing: &Pairing,
    license: &str,
    device_id: &str,
) -> Result<Report> {
    let secret = pairing.secret()?;
    let mut report = Report::default();

    // ---------------------------------------------------------- what is ours
    let mine = digest(paths)?;
    let agreed = read_base(paths);
    let mut pushed_keys: Vec<String> = vec![];
    let mut items: Vec<Value> = vec![];
    for (key, version) in &mine {
        // Already left in the mailbox at this revision: nothing to say.
        if agreed.get(key).is_some_and(|base| !version.beats(base)) {
            continue;
        }
        let body = read_item(paths, key)?;
        let sealed = seal(
            &secret,
            &Msg::Item {
                key: key.clone(),
                body,
            },
        )?;
        items.push(json!({
            "item": key,
            "rev": version.rev,
            "updated_at": version.updated_at.timestamp(),
            "sealed": B64.encode(&sealed),
        }));
        pushed_keys.push(key.clone());
    }

    for chunk in items.chunks(64) {
        let response = http
            .post(endpoint(&pairing.relay, "/sync/push"))
            .header("authorization", format!("Bearer {}", license.trim()))
            .header(crate::entitlement::DEVICE_HEADER, device_id)
            .json(&json!({
                "room": pairing.room,
                "device": pairing.device_id,
                "items": chunk,
            }))
            .send()
            .await
            .map_err(|err| AppError::Provider(format!("sync: {err}")))?;
        if !response.status().is_success() {
            return Err(refusal(response).await);
        }
        report.pushed += chunk.len();
    }
    if !pushed_keys.is_empty() {
        // Agreed only once the gateway has it: a push that failed halfway
        // must be offered again next round, not assumed.
        note_agreed(paths, &pushed_keys)?;
    }

    // ------------------------------------------------------ what is theirs
    let mut cursor = read_cursor(paths);
    loop {
        let response = http
            .get(endpoint(&pairing.relay, "/sync/pull"))
            .query(&[
                ("room", pairing.room.as_str()),
                ("since", &cursor.seq.to_string()),
                ("limit", "64"),
            ])
            .header("authorization", format!("Bearer {}", license.trim()))
            .header(crate::entitlement::DEVICE_HEADER, device_id)
            .send()
            .await
            .map_err(|err| AppError::Provider(format!("sync: {err}")))?;
        if !response.status().is_success() {
            return Err(refusal(response).await);
        }
        let page: Value = response
            .json()
            .await
            .map_err(|err| AppError::Provider(format!("sync: {err}")))?;

        let empty = vec![];
        let rows = page
            .get("items")
            .and_then(Value::as_array)
            .unwrap_or(&empty);
        for row in rows {
            let Some(sealed) = row
                .get("sealed")
                .and_then(Value::as_str)
                .and_then(|text| B64.decode(text).ok())
            else {
                report.rejected += 1;
                continue;
            };
            // A record this device left itself comes back down again; there
            // is nothing to learn from it, and applying it would be a
            // needless write.
            if row.get("device").and_then(Value::as_str) == Some(pairing.device_id.as_str()) {
                continue;
            }
            match open(&secret, &sealed) {
                Ok(Msg::Item { key, body }) => match apply_item(paths, &key, &body) {
                    Ok(Applied::Written) => report.pulled += 1,
                    Ok(Applied::Conflicted) => {
                        report.pulled += 1;
                        report.conflicts += 1;
                    }
                    Ok(Applied::Kept) => {}
                    Err(_) => report.rejected += 1,
                },
                // Sealed with a key we do not have, or not a record at all:
                // somebody else's room, or a version of the app that speaks
                // differently. Either way it is not ours to write.
                _ => report.rejected += 1,
            }
        }

        cursor.seq = page
            .get("cursor")
            .and_then(Value::as_i64)
            .unwrap_or(cursor.seq);
        write_cursor(paths, &cursor)?;
        if !page.get("more").and_then(Value::as_bool).unwrap_or(false) {
            break;
        }
    }

    report.finished_at = Some(chrono::Utc::now());
    crate::sync::write_report(paths, &report)?;
    Ok(report)
}

/// The gateway's own words when it refuses, which are the ones worth showing:
/// a seat taken, a licence expired, a room that is not yours.
async fn refusal(response: reqwest::Response) -> AppError {
    let status = response.status();
    let body: HashMap<String, Value> = response.json().await.unwrap_or_default();
    let said = body
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if said.is_empty() {
        AppError::Provider(format!("sync: the gateway answered {status}"))
    } else {
        AppError::Provider(format!("sync: {said}"))
    }
}
