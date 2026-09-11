//! What the two devices say to each other, and the envelope it travels in.
//!
//! Everything is sealed with the pairing key before it leaves: the relay
//! forwards bytes between two members of a room and can read none of them.
//! That is the whole reason the relay is allowed to exist on someone else's
//! machine.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Digest, Report};
use crate::error::{AppError, Result};

/// One turn of the conversation between two paired devices.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Msg {
    /// Who just arrived, and what they hold.
    Hello { device_id: String, digest: Digest },
    /// Records the sender is behind on and would like sent.
    Want { keys: Vec<String> },
    /// One record, whole.
    Item { key: String, body: Value },
    /// Nothing further from this side.
    Done { report: Report },
}

/// Seal one message with the pairing key.
///
/// The nonce is random and travels in front of the ciphertext: 24 bytes of it,
/// so a device that syncs every minute for years still has no chance of using
/// one twice.
pub fn seal(secret: &[u8; 32], msg: &Msg) -> Result<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new(secret.into());
    let mut nonce = [0u8; 24];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let plain = serde_json::to_vec(msg)?;
    let sealed = cipher
        .encrypt(XNonce::from_slice(&nonce), plain.as_ref())
        .map_err(|_| AppError::Other("sync: could not seal a message".into()))?;
    let mut out = Vec::with_capacity(24 + sealed.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// Open a message. A frame that does not open is not ours: a stranger in the
/// room, or a relay that changed something on the way.
pub fn open(secret: &[u8; 32], frame: &[u8]) -> Result<Msg> {
    if frame.len() <= 24 {
        return Err(AppError::Invalid("sync: a frame arrived truncated".into()));
    }
    let (nonce, sealed) = frame.split_at(24);
    let cipher = XChaCha20Poly1305::new(secret.into());
    let plain = cipher
        .decrypt(XNonce::from_slice(nonce), sealed)
        .map_err(|_| AppError::Invalid("sync: a frame did not open with our key".into()))?;
    Ok(serde_json::from_slice(&plain)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn msg() -> Msg {
        let mut digest = Digest::new();
        digest.insert(
            "man:m1/a".into(),
            super::super::Version {
                rev: 4,
                updated_at: DateTime::<Utc>::from_timestamp(1_000, 0).unwrap(),
            },
        );
        Msg::Hello {
            device_id: "dev-1".into(),
            digest,
        }
    }

    #[test]
    fn a_sealed_message_opens_again() {
        let secret = [9u8; 32];
        let frame = seal(&secret, &msg()).unwrap();
        match open(&secret, &frame).unwrap() {
            Msg::Hello { device_id, digest } => {
                assert_eq!(device_id, "dev-1");
                assert_eq!(digest["man:m1/a"].rev, 4);
            }
            other => panic!("expected a hello, got {other:?}"),
        }
    }

    /// The relay is not trusted, and neither is anyone else in the room: a
    /// frame sealed with another key does not open, and neither does one that
    /// was altered on the way.
    #[test]
    fn a_frame_from_a_stranger_does_not_open() {
        let frame = seal(&[9u8; 32], &msg()).unwrap();
        assert!(open(&[8u8; 32], &frame).is_err());

        let mut tampered = frame.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert!(open(&[9u8; 32], &tampered).is_err());
    }

    #[test]
    fn a_truncated_frame_is_refused_rather_than_panicking() {
        assert!(open(&[9u8; 32], &[0u8; 10]).is_err());
        assert!(open(&[9u8; 32], &[]).is_err());
    }

    /// Two seals of the same message must not look alike, or the relay learns
    /// when nothing changed.
    #[test]
    fn the_same_message_seals_differently_every_time() {
        let secret = [9u8; 32];
        assert_ne!(
            seal(&secret, &msg()).unwrap(),
            seal(&secret, &msg()).unwrap()
        );
    }
}
