//! Which machine this is.
//!
//! A licence for ten devices has to mean ten machines, so a seat is claimed
//! by the hardware and not by the install: reinstalling the app, clearing
//! the data folder or copying the settings across must not hand out a
//! eleventh seat, and moving to a new laptop must not silently keep the old
//! one's.
//!
//! Every desktop already has an id for this — Windows keeps a MachineGuid,
//! Linux a machine-id, macOS an IOPlatformUUID — and none of them is a
//! secret worth leaking, so what leaves the machine is a hash with a label
//! rather than the id itself. If the platform has nothing to offer, a random
//! id is written beside the data and used instead: worse, but it still
//! counts one machine as one seat for as long as that file lives.

use std::sync::OnceLock;

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use sha2::{Digest as _, Sha256};

use crate::storage::Paths;

static CACHED: OnceLock<String> = OnceLock::new();

/// The id this machine sends to the gateway. Stable across reinstalls.
pub fn device_id(paths: &Paths) -> String {
    CACHED
        .get_or_init(|| {
            let raw = platform_id().unwrap_or_else(|| fallback_id(paths));
            let mut hash = Sha256::new();
            // Labelled, so the same machine id used by something else of
            // ours would not produce the same string.
            hash.update(b"velvetdesk-device/v1");
            hash.update(raw.trim().as_bytes());
            B64.encode(&hash.finalize()[..16])
        })
        .clone()
}

#[cfg(target_os = "windows")]
fn platform_id() -> Option<String> {
    // The registry is read through reg.exe rather than a registry crate: one
    // process at startup, cached for the life of the app, against a whole
    // dependency for one string.
    let output = std::process::Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\Microsoft\Cryptography",
            "/v",
            "MachineGuid",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .find_map(|line| {
            line.split_whitespace()
                .last()
                .filter(|_| line.contains("MachineGuid"))
        })
        .map(|guid| guid.to_string())
}

#[cfg(target_os = "linux")]
fn platform_id() -> Option<String> {
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(text) = std::fs::read_to_string(path) {
            if !text.trim().is_empty() {
                return Some(text.trim().to_string());
            }
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn platform_id() -> Option<String> {
    let output = std::process::Command::new("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .find(|line| line.contains("IOPlatformUUID"))
        .and_then(|line| line.split('"').nth(3))
        .map(|uuid| uuid.to_string())
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
fn platform_id() -> Option<String> {
    // Phones: the data directory is per-install and cannot be copied to
    // another handset without root, so the stored id is the honest answer.
    None
}

/// A random id kept beside the data, for platforms that offer nothing.
fn fallback_id(paths: &Paths) -> String {
    let path = paths.root.join("device.txt");
    if let Ok(text) = std::fs::read_to_string(&path) {
        if !text.trim().is_empty() {
            return text.trim().to_string();
        }
    }
    let fresh = uuid::Uuid::new_v4().simple().to_string();
    let _ = std::fs::create_dir_all(&paths.root);
    let _ = std::fs::write(&path, &fresh);
    fresh
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> Paths {
        let dir = std::env::temp_dir().join(format!("velvet-hwid-{}", crate::models::new_id()));
        Paths::new(dir).unwrap()
    }

    /// Whatever the machine offers, the id is short, stable within a run,
    /// and does not carry the raw platform id out with it.
    #[test]
    fn the_id_is_a_hash_and_not_the_machine_id() {
        let paths = scratch();
        let first = device_id(&paths);
        assert_eq!(first, device_id(&paths));
        assert!(!first.is_empty() && first.len() <= 32);
        if let Some(raw) = platform_id() {
            assert!(!first.contains(raw.trim()));
        }
    }

    /// With nothing from the platform, the same folder keeps the same id.
    #[test]
    fn the_fallback_id_survives_a_restart() {
        let paths = scratch();
        let first = fallback_id(&paths);
        assert_eq!(first, fallback_id(&paths));
        assert_ne!(first, fallback_id(&scratch()));
    }
}
