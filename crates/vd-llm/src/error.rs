//! Failures a provider call can end in.
//!
//! The desktop has its own error type with a dozen more variants — files,
//! scopes, storage — and this is the subset that talking to a model can
//! produce. The client converts one into the other on the way out, so a
//! refusal stays a refusal and an exhausted pool stays an exhausted pool.

use serde::{Serialize, Serializer};

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("http error: {0}")]
    Http(String),

    #[error("invalid input: {0}")]
    Invalid(String),

    #[error("provider error: {0}")]
    Provider(String),

    #[error("no usable api key: {0}")]
    NoKeys(String),

    /// The model refused the request, or answered with nothing at all.
    #[error("model declined: {reason}")]
    Blocked { reason: String },

    #[error("{0}")]
    Other(String),

    /// An error the operator is meant to read, named rather than written out:
    /// the interface holds the wording, in whichever language it is running.
    #[error("{key}")]
    Message {
        key: String,
        params: serde_json::Value,
    },
}

impl LlmError {
    /// A message the interface translates. `params` fills its placeholders.
    pub fn message(key: &str, params: serde_json::Value) -> Self {
        LlmError::Message {
            key: key.to_string(),
            params,
        }
    }

    /// Stable name of the failure, so a caller can phrase the rest.
    pub fn kind(&self) -> &'static str {
        match self {
            LlmError::Json(_) => "json",
            LlmError::Http(_) => "http",
            LlmError::Invalid(_) => "invalid",
            LlmError::Provider(_) => "provider",
            LlmError::NoKeys(_) => "no_keys",
            LlmError::Blocked { .. } => "blocked",
            LlmError::Other(_) => "other",
            LlmError::Message { .. } => "message",
        }
    }
}

impl From<reqwest::Error> for LlmError {
    fn from(value: reqwest::Error) -> Self {
        LlmError::Http(value.to_string())
    }
}

impl Serialize for LlmError {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(Some(3))?;
        map.serialize_entry("kind", self.kind())?;
        match self {
            LlmError::Message { key, params } => {
                map.serialize_entry("key", key)?;
                map.serialize_entry("params", params)?;
            }
            other => {
                map.serialize_entry("message", &other.to_string())?;
            }
        }
        map.end()
    }
}

pub type Result<T> = std::result::Result<T, LlmError>;
