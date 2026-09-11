//! A round of sync between two devices, without the network in the way.
//!
//! The transport is a relay that moves sealed bytes; what matters here is
//! everything on either side of it — what each device says it holds, what it
//! asks for, and what it does with what arrives.

use std::path::PathBuf;

use velvetdesk_lib::models::{Fact, Man, Profile};
use velvetdesk_lib::storage::Paths;
use velvetdesk_lib::sync::{apply_item, digest, read_item, wanted, Applied};

/// Two devices, each with their own data directory.
struct Device {
    paths: Paths,
    _dir: TempDir,
}

/// A directory that cleans up after itself, so a failing test does not leave
/// half a workspace behind.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn device(name: &str) -> Device {
    let dir = std::env::temp_dir().join(format!(
        "velvetdesk-sync-{name}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let paths = Paths::new(dir.clone()).unwrap();
    Device {
        paths,
        _dir: TempDir(dir),
    }
}

/// A profile with one man in it, as a device would hold after a day's work.
fn seed(device: &Device, model_id: &str, man_id: &str) {
    std::fs::create_dir_all(device.paths.profiles_dir().join(model_id)).unwrap();
    let scope = device.paths.scope(model_id).unwrap();
    scope
        .write_profile(&Profile::new(model_id.to_string(), "Alina".into()))
        .unwrap();
    scope
        .write_man(&Man::new(
            model_id.to_string(),
            man_id.to_string(),
            "Eric".into(),
        ))
        .unwrap();
}

/// Move everything one device is behind on. This is what `transport` does
/// over the wire, with the sealing left out.
fn round(from: &Device, to: &Device) -> Vec<Applied> {
    let theirs = digest(&from.paths).unwrap();
    let mine = digest(&to.paths).unwrap();
    wanted(&mine, &theirs)
        .into_iter()
        .map(|key| {
            let body = read_item(&from.paths, &key).unwrap();
            apply_item(&to.paths, &key, &body).unwrap()
        })
        .collect()
}

/// The plain case: the laptop learns a fact the desktop wrote.
#[test]
fn a_fact_written_on_one_device_arrives_on_the_other() {
    let desktop = device("desktop");
    let laptop = device("laptop");
    seed(&desktop, "alina", "eric");

    // Nothing on the laptop yet, so the whole profile travels.
    let applied = round(&desktop, &laptop);
    assert!(!applied.is_empty());
    assert!(applied.iter().all(|a| *a == Applied::Written));

    // The desktop learns something about him.
    let scope = desktop.paths.scope("alina").unwrap();
    let mut man = scope.read_man("eric").unwrap();
    man.facts.push(Fact {
        id: "f1".into(),
        key: "travel".into(),
        value: "flies to Warsaw in March".into(),
        source: "operator".into(),
        created_at: chrono::Utc::now(),
    });
    scope.write_man(&man).unwrap();

    let applied = round(&desktop, &laptop);
    assert_eq!(applied, [Applied::Written], "only the man moved");

    let arrived = laptop
        .paths
        .scope("alina")
        .unwrap()
        .read_man("eric")
        .unwrap();
    assert_eq!(arrived.facts.len(), 1);
    assert_eq!(arrived.facts[0].value, "flies to Warsaw in March");
    // The write bumped the revision on the way to disk; the copy that
    // travelled carries that number, not the one held before the edit.
    assert_eq!(
        arrived.rev,
        scope.read_man("eric").unwrap().rev,
        "the copy keeps the revision it was given"
    );

    // Running the round again moves nothing: both sides are level.
    assert!(round(&desktop, &laptop).is_empty());
}

/// A record taken from another device is not an edit of ours, so sending it
/// back is not an update — otherwise two devices would trade the same record
/// forever, each bumping it on arrival.
#[test]
fn a_received_record_does_not_bounce_back() {
    let desktop = device("desktop");
    let laptop = device("laptop");
    seed(&desktop, "alina", "eric");
    round(&desktop, &laptop);

    assert!(
        round(&laptop, &desktop).is_empty(),
        "the laptop has nothing the desktop is behind on"
    );
}

/// Both devices edited the same man while apart. The one with more edits
/// wins, and the loser is kept rather than quietly dropped.
#[test]
fn a_conflict_keeps_the_losing_copy() {
    let desktop = device("desktop");
    let laptop = device("laptop");
    seed(&desktop, "alina", "eric");
    round(&desktop, &laptop);

    // The laptop makes one edit; the desktop makes two.
    let laptop_scope = laptop.paths.scope("alina").unwrap();
    let mut theirs = laptop_scope.read_man("eric").unwrap();
    theirs.status = "wrote on the train".into();
    laptop_scope.write_man(&theirs).unwrap();

    let desktop_scope = desktop.paths.scope("alina").unwrap();
    for note in ["called", "called again"] {
        let mut mine = desktop_scope.read_man("eric").unwrap();
        mine.status = note.into();
        desktop_scope.write_man(&mine).unwrap();
    }

    let applied = round(&desktop, &laptop);
    assert_eq!(applied, [Applied::Conflicted]);

    let now = laptop_scope.read_man("eric").unwrap();
    assert_eq!(now.status, "called again", "more edits wins");

    // The copy that lost is on disk where it can be read tomorrow.
    let backups = laptop.paths.root.join("backups");
    let conflicts: Vec<_> = std::fs::read_dir(&backups)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("conflict_"))
        .collect();
    assert_eq!(conflicts.len(), 1);
    let saved = std::fs::read_to_string(conflicts[0].path().join("man_alina_eric.json")).unwrap();
    assert!(
        saved.contains("wrote on the train"),
        "the losing copy is the one that was kept: {saved}"
    );
}

/// Keys and settings are the machine's own. A digest that offered them would
/// be a digest that eventually moved them.
#[test]
fn keys_and_settings_never_appear_in_a_digest() {
    let desktop = device("desktop");
    seed(&desktop, "alina", "eric");
    std::fs::write(desktop.paths.secrets_file(), "{\"keys\":{}}").unwrap();
    std::fs::write(desktop.paths.settings_file(), "{}").unwrap();

    let keys: Vec<String> = digest(&desktop.paths).unwrap().into_keys().collect();
    assert!(keys.iter().all(|key| {
        key.starts_with("profile:") || key.starts_with("man:") || key.starts_with("chat:")
    }));
    assert!(!keys.iter().any(|key| key.contains("secret")));
}
