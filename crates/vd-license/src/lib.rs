//! Licences: minted by the operator, checked by the gateway, and checked
//! again by the desktop without asking anyone.
//!
//! A licence is its own proof — payload and signature — rather than a row the
//! server looks up. That is what lets the client start offline, and it means
//! a stolen database of licence ids grants nothing.
//!
//! Format: `VD.<payload>.<signature>`, both segments base64url without
//! padding, the signature over the payload segment's bytes exactly as they
//! appear in the string.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

pub use ed25519_dalek::{SigningKey as LicenseSigningKey, VerifyingKey as LicensePublicKey};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct License {
    pub license_id: String,
    pub tier: String,
    /// Unix seconds. Zero means it does not expire.
    #[serde(default)]
    pub expires_at: i64,
    #[serde(default)]
    pub max_peers: u32,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LicenseError {
    #[error("licence is not in the VD.<payload>.<signature> shape")]
    Shape,
    #[error("licence payload is not readable")]
    Payload,
    #[error("licence signature does not match")]
    Signature,
    #[error("licence expired")]
    Expired,
    #[error("licence was revoked")]
    Revoked,
    #[error("the gateway has no public key to check licences against")]
    NoKey,
}

impl License {
    pub fn expired_at(&self, now: i64) -> bool {
        self.expires_at != 0 && self.expires_at < now
    }
}

/// Sign a licence. Used by the `mint` subcommand on the operator's own box.
pub fn mint(key: &SigningKey, license: &License) -> String {
    let payload = B64.encode(serde_json::to_vec(license).unwrap_or_default());
    let signature = key.sign(payload.as_bytes());
    format!("VD.{payload}.{}", B64.encode(signature.to_bytes()))
}

/// Read a licence and check its signature. Expiry is checked separately so a
/// caller can tell "not ours" from "ran out".
pub fn verify(token: &str, public_key: &VerifyingKey) -> Result<License, LicenseError> {
    let mut parts = token.trim().splitn(3, '.');
    let (Some("VD"), Some(payload), Some(signature)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(LicenseError::Shape);
    };
    let signature = B64
        .decode(signature)
        .ok()
        .and_then(|bytes| <[u8; 64]>::try_from(bytes).ok())
        .map(|bytes| Signature::from_bytes(&bytes))
        .ok_or(LicenseError::Signature)?;
    public_key
        .verify(payload.as_bytes(), &signature)
        .map_err(|_| LicenseError::Signature)?;
    let bytes = B64.decode(payload).map_err(|_| LicenseError::Payload)?;
    serde_json::from_slice(&bytes).map_err(|_| LicenseError::Payload)
}

/// Read a base64 public key as it is written in the config file.
pub fn public_key_from_base64(text: &str) -> Option<VerifyingKey> {
    use base64::engine::general_purpose::STANDARD;
    let bytes = STANDARD
        .decode(text.trim())
        .or_else(|_| B64.decode(text.trim()))
        .ok()?;
    <[u8; 32]>::try_from(bytes)
        .ok()
        .and_then(|raw| VerifyingKey::from_bytes(&raw).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn license() -> License {
        License {
            license_id: "VD-PRO-0001".into(),
            tier: "pro".into(),
            expires_at: 1_790_000_000,
            max_peers: 2,
        }
    }

    #[test]
    fn a_minted_licence_reads_back() {
        let token = mint(&key(), &license());
        let read = verify(&token, &key().verifying_key()).unwrap();
        assert_eq!(read.license_id, "VD-PRO-0001");
        assert_eq!(read.tier, "pro");
    }

    /// The whole point of signing: a licence edited in a text editor — a
    /// higher tier, a later date — stops verifying.
    #[test]
    fn an_edited_payload_is_refused() {
        let token = mint(&key(), &license());
        let mut parts: Vec<&str> = token.split('.').collect();
        let forged = B64.encode(
            serde_json::to_vec(&License {
                tier: "agency".into(),
                ..license()
            })
            .unwrap(),
        );
        parts[1] = &forged;
        let token = parts.join(".");
        assert_eq!(
            verify(&token, &key().verifying_key()).unwrap_err(),
            LicenseError::Signature
        );
    }

    #[test]
    fn another_signer_is_refused() {
        let token = mint(&SigningKey::from_bytes(&[9u8; 32]), &license());
        assert_eq!(
            verify(&token, &key().verifying_key()).unwrap_err(),
            LicenseError::Signature
        );
    }

    #[test]
    fn rubbish_is_refused_by_shape() {
        assert_eq!(
            verify("hello", &key().verifying_key()).unwrap_err(),
            LicenseError::Shape
        );
        assert_eq!(
            verify("VD.only-one-part", &key().verifying_key()).unwrap_err(),
            LicenseError::Shape
        );
    }

    #[test]
    fn expiry_is_read_from_the_payload() {
        let license = license();
        assert!(license.expired_at(1_790_000_001));
        assert!(!license.expired_at(1_789_999_999));
        let forever = License {
            expires_at: 0,
            ..license
        };
        assert!(!forever.expired_at(i64::MAX));
    }

    #[test]
    fn the_public_key_reads_in_either_base64_alphabet() {
        let verifying = key().verifying_key();
        let standard = STANDARD.encode(verifying.to_bytes());
        let url = B64.encode(verifying.to_bytes());
        assert_eq!(public_key_from_base64(&standard).unwrap(), verifying);
        assert_eq!(public_key_from_base64(&url).unwrap(), verifying);
        assert!(public_key_from_base64("not a key").is_none());
    }
}
