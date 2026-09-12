//! Pairing two devices, and the key that pairing leaves behind.
//!
//! One device makes an invite, the other types it in. The invite carries a
//! random 32-byte key; from then on everything between them is sealed with it,
//! and the relay in the middle sees ciphertext and a room name it cannot work
//! backwards from.
//!
//! The pairing file sits beside the keys and never travels: a device is paired
//! by being told the secret, not by being in a list somewhere.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::error::{AppError, Result};
use crate::storage::{read_json, write_json, Paths};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Pairing {
    /// Who this device is, so a peer can tell two of them apart.
    pub device_id: String,
    /// The shared secret, base64url. Never leaves this file or the invite.
    pub key: String,
    /// Where the relay should put us. Derived from the key, so knowing the
    /// room tells nobody anything about the secret.
    pub room: String,
    /// The relay to meet at, as a base URL — the gateway's, normally.
    #[serde(default)]
    pub relay: String,
    /// Sync automatically when the app is running.
    #[serde(default)]
    pub auto: bool,
}

impl Pairing {
    pub fn file(paths: &Paths) -> std::path::PathBuf {
        paths.root.join("sync.json")
    }

    pub fn load(paths: &Paths) -> Result<Option<Pairing>> {
        read_json(&Pairing::file(paths))
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        write_json(&Pairing::file(paths), self)?;
        restrict(&Pairing::file(paths));
        Ok(())
    }

    pub fn forget(paths: &Paths) -> Result<()> {
        let path = Pairing::file(paths);
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    /// The secret as bytes, for sealing.
    pub fn secret(&self) -> Result<[u8; 32]> {
        let raw = B64
            .decode(self.key.trim())
            .map_err(|_| AppError::Invalid("sync: the pairing key is not readable".into()))?;
        <[u8; 32]>::try_from(raw)
            .map_err(|_| AppError::Invalid("sync: the pairing key is the wrong size".into()))
    }

    /// The invite another device types in.
    pub fn invite(&self) -> String {
        format!(
            "VDSYNC.{}.{}",
            self.key.trim(),
            B64.encode(self.relay.as_bytes())
        )
    }
}

/// The room name a key meets in.
///
/// A hash of the secret with a label: the relay needs something to group two
/// devices by, and this gives it one that cannot be turned back into the key.
pub fn room_for(secret: &[u8; 32]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"velvetdesk-sync-room/v1");
    hash.update(secret);
    B64.encode(&hash.finalize()[..16])
}

/// The pairing a licence implies.
///
/// Two machines running the same subscription should find each other without
/// anybody reading a code down the phone, so the secret is derived from the
/// licence itself: same licence, same key, same room, on every device the
/// operator installs.
///
/// What this costs, stated plainly: the gateway is handed the licence token
/// to check its signature, so a gateway that chose to misbehave could derive
/// this key and read what passes through it. It never stores the token — only
/// the id inside it — and the payloads stay sealed from everyone else. An
/// operator who would rather not extend even that much trust can pair by
/// invite instead, with a key the server has never seen.
pub fn secret_from_license(token: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"velvetdesk-sync-secret/v1");
    hash.update(token.trim().as_bytes());
    hash.finalize().into()
}

/// Pair this device off the licence, keeping the identity it already has.
///
/// Re-running this is harmless: the key and room are a function of the
/// licence, and the device id is only regenerated when there was none.
pub fn from_license(paths: &Paths, token: &str, relay: &str) -> Result<Pairing> {
    if token.trim().is_empty() {
        return Err(AppError::message("sync.noLicense", serde_json::json!({})));
    }
    let secret = secret_from_license(token);
    let existing = Pairing::load(paths)?;
    let pairing = Pairing {
        device_id: existing
            .as_ref()
            .map(|p| p.device_id.clone())
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string()),
        key: B64.encode(secret),
        room: room_for(&secret),
        relay: relay.trim_end_matches('/').to_string(),
        auto: existing.map(|p| p.auto).unwrap_or(true),
    };
    pairing.save(paths)?;
    Ok(pairing)
}

/// Start a pairing on this device and hand back the invite.
pub fn create(paths: &Paths, relay: &str) -> Result<Pairing> {
    use rand::RngCore;
    let mut secret = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut secret);
    let pairing = Pairing {
        device_id: uuid::Uuid::new_v4().simple().to_string(),
        key: B64.encode(secret),
        room: room_for(&secret),
        relay: relay.trim_end_matches('/').to_string(),
        auto: true,
    };
    pairing.save(paths)?;
    Ok(pairing)
}

/// Join the pairing an invite describes.
pub fn join(paths: &Paths, invite: &str, relay_fallback: &str) -> Result<Pairing> {
    let mut parts = invite.trim().splitn(3, '.');
    let (Some("VDSYNC"), Some(key), relay) = (parts.next(), parts.next(), parts.next()) else {
        return Err(AppError::message("sync.badInvite", serde_json::json!({})));
    };
    let secret = <[u8; 32]>::try_from(
        B64.decode(key)
            .map_err(|_| AppError::message("sync.badInvite", serde_json::json!({})))?,
    )
    .map_err(|_| AppError::message("sync.badInvite", serde_json::json!({})))?;

    // The invite carries the relay it was made on; an invite written by hand
    // may not, and then the app's own gateway is the sensible place to meet.
    let relay = relay
        .and_then(|encoded| B64.decode(encoded).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| relay_fallback.to_string());

    let pairing = Pairing {
        device_id: uuid::Uuid::new_v4().simple().to_string(),
        key: B64.encode(secret),
        room: room_for(&secret),
        relay: relay.trim_end_matches('/').to_string(),
        auto: true,
    };
    pairing.save(paths)?;
    Ok(pairing)
}

#[cfg(unix)]
fn restrict(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict(_path: &std::path::Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairing() -> Pairing {
        let secret = [3u8; 32];
        Pairing {
            device_id: "dev-1".into(),
            key: B64.encode(secret),
            room: room_for(&secret),
            relay: "https://cloud.velvetdesk.ai".into(),
            auto: true,
        }
    }

    /// Both devices have to land in the same room, and the room must not give
    /// the secret away to the relay that sees it.
    #[test]
    fn the_room_follows_the_key_and_not_the_other_way() {
        let same = room_for(&[3u8; 32]);
        assert_eq!(pairing().room, same);
        assert_ne!(same, room_for(&[4u8; 32]));
        assert!(!same.contains(&B64.encode([3u8; 32])));
    }

    /// An invite read back gives the same secret and the same relay.
    #[test]
    fn an_invite_round_trips() {
        let invite = pairing().invite();
        let mut parts = invite.splitn(3, '.');
        assert_eq!(parts.next(), Some("VDSYNC"));
        let key = parts.next().unwrap();
        assert_eq!(B64.decode(key).unwrap(), [3u8; 32]);
        let relay = String::from_utf8(B64.decode(parts.next().unwrap()).unwrap()).unwrap();
        assert_eq!(relay, "https://cloud.velvetdesk.ai");
    }

    /// The same licence has to land two machines in the same room, and a
    /// different licence must not come anywhere near it.
    #[test]
    fn a_licence_pairs_the_devices_that_share_it() {
        let mine = secret_from_license("VD.payload.signature");
        assert_eq!(mine, secret_from_license("  VD.payload.signature  "));
        assert_ne!(mine, secret_from_license("VD.someone.else"));
        assert_eq!(
            room_for(&mine),
            room_for(&secret_from_license("VD.payload.signature"))
        );
    }

    #[test]
    fn the_secret_reads_back_as_bytes() {
        assert_eq!(pairing().secret().unwrap(), [3u8; 32]);
        let short = Pairing {
            key: B64.encode([1u8; 8]),
            ..pairing()
        };
        assert!(short.secret().is_err());
    }
}
