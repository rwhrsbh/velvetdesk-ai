use serde::{Serialize, Serializer};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("http error: {0}")]
    Http(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid input: {0}")]
    Invalid(String),

    #[error("scope violation: {0}")]
    Scope(String),

    #[error("provider error: {0}")]
    Provider(String),

    #[error("no usable api key: {0}")]
    NoKeys(String),

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

impl AppError {
    /// A message the interface translates. `params` fills its placeholders.
    pub fn message(key: &str, params: serde_json::Value) -> Self {
        AppError::Message {
            key: key.to_string(),
            params,
        }
    }

    /// Stable name of the failure, so the interface can phrase the rest.
    pub fn kind(&self) -> &'static str {
        match self {
            AppError::Io(_) => "io",
            AppError::Json(_) => "json",
            AppError::Http(_) => "http",
            AppError::NotFound(_) => "not_found",
            AppError::Invalid(_) => "invalid",
            AppError::Scope(_) => "scope",
            AppError::Provider(_) => "provider",
            AppError::NoKeys(_) => "no_keys",
            AppError::Other(_) => "other",
            AppError::Message { .. } => "message",
        }
    }
}

impl From<reqwest::Error> for AppError {
    fn from(value: reqwest::Error) -> Self {
        AppError::Http(value.to_string())
    }
}

impl From<tauri::Error> for AppError {
    fn from(value: tauri::Error) -> Self {
        AppError::Other(value.to_string())
    }
}

impl Serialize for AppError {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(Some(3))?;
        map.serialize_entry("kind", self.kind())?;
        match self {
            AppError::Message { key, params } => {
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

pub type Result<T> = std::result::Result<T, AppError>;
