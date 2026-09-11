//! Keeping two devices' work the same.
//!
//! An operator with a laptop and a desktop edits a dossier on whichever is in
//! front of them, and expects the other to know. What travels is profiles, men
//! and chats — never `settings.json`, never `secrets.json`: keys belong to the
//! machine they were typed on.
//!
//! There is no queue of pending changes. Each round asks both sides what they
//! hold — a map of record to revision — and moves only what is behind. A
//! device that was off for a week catches up in exactly the same way as one
//! that was off for a minute, and a round that dies halfway leaves nothing to
//! reconcile, because the next round will ask again.

pub mod pair;
pub mod transport;
pub mod wire;

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{AppError, Result};
use crate::models::{ChatThread, Man, Profile};
use crate::storage::{read_json, write_json, Paths};

/// Where a record stands: how many edits it has had, and when the last one
/// was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    pub rev: u64,
    pub updated_at: DateTime<Utc>,
}

impl Version {
    /// Whether this version should replace `other`.
    ///
    /// Revisions first, clocks only to break a tie: two machines disagree
    /// about the time by minutes, and the edit that came later is the one made
    /// after more edits, not the one whose clock runs fast. A tie on both is
    /// not a win — it is the same record, and moving it would be pointless
    /// traffic.
    pub fn beats(&self, other: &Version) -> bool {
        match self.rev.cmp(&other.rev) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => self.updated_at > other.updated_at,
        }
    }
}

/// What each side holds, by record key.
pub type Digest = BTreeMap<String, Version>;

/// The three kinds of record that travel, and where each one lives.
///
/// Keys read as `profile:<model>`, `man:<model>/<man>`, `chat:<model>/<man>` —
/// one flat string so a digest is a map and not a tree to walk.
pub fn profile_key(model_id: &str) -> String {
    format!("profile:{model_id}")
}

pub fn man_key(model_id: &str, man_id: &str) -> String {
    format!("man:{model_id}/{man_id}")
}

pub fn chat_key(model_id: &str, man_id: &str) -> String {
    format!("chat:{model_id}/{man_id}")
}

/// A key split back into its parts: kind, profile, and the man it belongs to.
fn parse_key(key: &str) -> Option<(&str, &str, Option<&str>)> {
    let (kind, rest) = key.split_once(':')?;
    match kind {
        "profile" => Some((kind, rest, None)),
        "man" | "chat" => {
            let (model_id, man_id) = rest.split_once('/')?;
            Some((kind, model_id, Some(man_id)))
        }
        _ => None,
    }
}

/// Everything this device holds, with the revision of each record.
pub fn digest(paths: &Paths) -> Result<Digest> {
    let mut out = Digest::new();
    for model_id in paths.list_model_ids()? {
        let Ok(scope) = paths.scope(&model_id) else {
            continue;
        };
        if let Ok(profile) = scope.read_profile() {
            out.insert(
                profile_key(&model_id),
                Version {
                    rev: profile.rev,
                    updated_at: profile.updated_at,
                },
            );
        }
        for man_id in scope.list_man_ids()? {
            if let Ok(man) = scope.read_man(&man_id) {
                out.insert(
                    man_key(&model_id, &man_id),
                    Version {
                        rev: man.rev,
                        updated_at: man.updated_at,
                    },
                );
            }
            // A chat that has never been opened has no file, and there is
            // nothing to tell the other side about.
            if scope
                .chat_file(&man_id)
                .map(|p| p.exists())
                .unwrap_or(false)
            {
                if let Ok(chat) = scope.read_chat(&man_id) {
                    out.insert(
                        chat_key(&model_id, &man_id),
                        Version {
                            rev: chat.rev,
                            updated_at: chat.updated_at,
                        },
                    );
                }
            }
        }
    }
    Ok(out)
}

/// What to ask the other side for: everything it holds that is ahead of what
/// is here, and everything it holds that is not here at all.
pub fn wanted(local: &Digest, remote: &Digest) -> Vec<String> {
    remote
        .iter()
        .filter(|(key, theirs)| match local.get(*key) {
            None => true,
            Some(ours) => theirs.beats(ours),
        })
        .map(|(key, _)| key.clone())
        .collect()
}

/// One record, as it travels.
pub fn read_item(paths: &Paths, key: &str) -> Result<Value> {
    let (kind, model_id, man_id) = parse_key(key)
        .ok_or_else(|| AppError::Invalid(format!("sync: cannot read the key {key}")))?;
    let scope = paths.scope(model_id)?;
    match (kind, man_id) {
        ("profile", _) => Ok(serde_json::to_value(scope.read_profile()?)?),
        ("man", Some(man_id)) => Ok(serde_json::to_value(scope.read_man(man_id)?)?),
        ("chat", Some(man_id)) => Ok(serde_json::to_value(scope.read_chat(man_id)?)?),
        _ => Err(AppError::Invalid(format!(
            "sync: cannot read the key {key}"
        ))),
    }
}

/// What happened to one record when it arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// Written, with nothing of ours lost.
    Written,
    /// Written over an edit of ours, which went to `backups/conflict_*` first.
    Conflicted,
    /// Ours was the same or further along, so nothing changed.
    Kept,
}

/// Take a record from the other side.
///
/// The incoming copy is parsed into the real type before anything is written:
/// a peer that sends nonsense gets it refused here rather than leaving a file
/// the app cannot read afterwards. What it replaces is copied aside first, so
/// a conflict costs a look in a folder rather than a day's work.
pub fn apply_item(paths: &Paths, key: &str, body: &Value) -> Result<Applied> {
    let (kind, model_id, man_id) = parse_key(key)
        .ok_or_else(|| AppError::Invalid(format!("sync: cannot read the key {key}")))?;
    let scope = paths.scope(model_id)?;

    match (kind, man_id) {
        ("profile", _) => {
            let incoming: Profile = serde_json::from_value(body.clone())?;
            let theirs = Version {
                rev: incoming.rev,
                updated_at: incoming.updated_at,
            };
            let local = scope.read_profile().ok();
            let verdict = verdict(
                local.as_ref().map(|p| Version {
                    rev: p.rev,
                    updated_at: p.updated_at,
                }),
                theirs,
            );
            if verdict == Applied::Kept {
                return Ok(Applied::Kept);
            }
            if verdict == Applied::Conflicted {
                if let Some(local) = &local {
                    stash(paths, key, &serde_json::to_value(local)?)?;
                }
            }
            scope.write_profile_verbatim(&incoming)?;
            Ok(verdict)
        }
        ("man", Some(man_id)) => {
            let incoming: Man = serde_json::from_value(body.clone())?;
            if incoming.id != man_id || incoming.model_id != model_id {
                return Err(AppError::Invalid(
                    "sync: the record does not belong where it was sent".into(),
                ));
            }
            let theirs = Version {
                rev: incoming.rev,
                updated_at: incoming.updated_at,
            };
            let local = scope.read_man(man_id).ok();
            let verdict = verdict(
                local.as_ref().map(|m| Version {
                    rev: m.rev,
                    updated_at: m.updated_at,
                }),
                theirs,
            );
            if verdict == Applied::Kept {
                return Ok(Applied::Kept);
            }
            if verdict == Applied::Conflicted {
                if let Some(local) = &local {
                    stash(paths, key, &serde_json::to_value(local)?)?;
                }
            }
            scope.write_man_verbatim(&incoming)?;
            Ok(verdict)
        }
        ("chat", Some(man_id)) => {
            let incoming: ChatThread = serde_json::from_value(body.clone())?;
            if incoming.man_id != man_id {
                return Err(AppError::Invalid(
                    "sync: the chat does not belong where it was sent".into(),
                ));
            }
            let theirs = Version {
                rev: incoming.rev,
                updated_at: incoming.updated_at,
            };
            let local = scope.read_chat(man_id).ok().filter(|_| {
                scope
                    .chat_file(man_id)
                    .map(|path| path.exists())
                    .unwrap_or(false)
            });
            let verdict = verdict(
                local.as_ref().map(|c| Version {
                    rev: c.rev,
                    updated_at: c.updated_at,
                }),
                theirs,
            );
            if verdict == Applied::Kept {
                return Ok(Applied::Kept);
            }
            if verdict == Applied::Conflicted {
                if let Some(local) = &local {
                    stash(paths, key, &serde_json::to_value(local)?)?;
                }
            }
            scope.write_chat_verbatim(&incoming)?;
            Ok(verdict)
        }
        _ => Err(AppError::Invalid(format!(
            "sync: cannot read the key {key}"
        ))),
    }
}

/// Whether the incoming record wins, and whether taking it costs anything.
///
/// A record that is simply new here is written without ceremony. One that
/// replaces an edit made here — a revision of our own, not just the copy we
/// were given — is a conflict, and the loser is kept.
fn verdict(local: Option<Version>, incoming: Version) -> Applied {
    match local {
        None => Applied::Written,
        Some(ours) if incoming.beats(&ours) => {
            if ours.rev > 0 {
                Applied::Conflicted
            } else {
                Applied::Written
            }
        }
        Some(_) => Applied::Kept,
    }
}

/// Put the losing copy somewhere it can be read tomorrow.
fn stash(paths: &Paths, key: &str, body: &Value) -> Result<()> {
    let stamp = Utc::now().format("%Y%m%d-%H%M%S");
    let dir = paths.root.join("backups").join(format!("conflict_{stamp}"));
    std::fs::create_dir_all(&dir)?;
    write_json(
        &dir.join(format!("{}.json", key.replace([':', '/'], "_"))),
        body,
    )
}

/// What one round did, for the operator and for the log.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Report {
    pub pulled: usize,
    pub pushed: usize,
    pub conflicts: usize,
    /// Records the other side sent that we refused to write.
    pub rejected: usize,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
}

/// The last round, kept so the interface can say when it happened.
pub fn read_report(paths: &Paths) -> Result<Option<Report>> {
    read_json(&paths.root.join("sync-report.json"))
}

pub fn write_report(paths: &Paths, report: &Report) -> Result<()> {
    write_json(&paths.root.join("sync-report.json"), report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).unwrap()
    }

    /// More edits wins, whatever the clocks say — a laptop running ten
    /// minutes fast must not win an argument it did not have.
    #[test]
    fn revisions_beat_clocks() {
        let ahead = Version {
            rev: 5,
            updated_at: at(1_000),
        };
        let fast_clock = Version {
            rev: 4,
            updated_at: at(999_999),
        };
        assert!(ahead.beats(&fast_clock));
        assert!(!fast_clock.beats(&ahead));
    }

    /// Same number of edits: the clock is all there is left to go on.
    #[test]
    fn the_clock_breaks_a_tie() {
        let older = Version {
            rev: 3,
            updated_at: at(1_000),
        };
        let newer = Version {
            rev: 3,
            updated_at: at(2_000),
        };
        assert!(newer.beats(&older));
        assert!(!older.beats(&newer));
        assert!(!older.beats(&older), "the same record is not news");
    }

    #[test]
    fn wanted_is_what_they_have_and_we_do_not() {
        let local = Digest::from([
            (
                "man:m1/a".to_string(),
                Version {
                    rev: 2,
                    updated_at: at(10),
                },
            ),
            (
                "man:m1/b".to_string(),
                Version {
                    rev: 9,
                    updated_at: at(10),
                },
            ),
        ]);
        let remote = Digest::from([
            (
                "man:m1/a".to_string(),
                Version {
                    rev: 3,
                    updated_at: at(5),
                },
            ),
            (
                "man:m1/b".to_string(),
                Version {
                    rev: 1,
                    updated_at: at(99),
                },
            ),
            (
                "man:m1/c".to_string(),
                Version {
                    rev: 1,
                    updated_at: at(1),
                },
            ),
        ]);
        let mut keys = wanted(&local, &remote);
        keys.sort();
        assert_eq!(keys, ["man:m1/a", "man:m1/c"]);
    }

    /// A record we have never seen arrives quietly. One that overwrites our
    /// own edit is a conflict, and the interface says so.
    #[test]
    fn taking_a_record_over_our_own_edit_is_a_conflict() {
        let incoming = Version {
            rev: 4,
            updated_at: at(100),
        };
        assert_eq!(verdict(None, incoming), Applied::Written);
        assert_eq!(
            verdict(
                Some(Version {
                    rev: 2,
                    updated_at: at(50)
                }),
                incoming
            ),
            Applied::Conflicted
        );
        assert_eq!(
            verdict(
                Some(Version {
                    rev: 0,
                    updated_at: at(50)
                }),
                incoming
            ),
            Applied::Written,
            "a copy we were given and never touched is not an edit of ours"
        );
        assert_eq!(
            verdict(
                Some(Version {
                    rev: 9,
                    updated_at: at(50)
                }),
                incoming
            ),
            Applied::Kept
        );
    }

    #[test]
    fn keys_read_back_into_their_parts() {
        assert_eq!(parse_key("profile:m1"), Some(("profile", "m1", None)));
        assert_eq!(parse_key("man:m1/x"), Some(("man", "m1", Some("x"))));
        assert_eq!(parse_key("chat:m1/x"), Some(("chat", "m1", Some("x"))));
        assert_eq!(parse_key("settings:m1"), None, "settings never travel");
        assert_eq!(parse_key("man:m1"), None);
    }
}
