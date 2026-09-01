//! On-device speech recognition: model catalogue, downloader and file server.
//!
//! The app ships without any weights. An operator picks a Whisper size, the
//! files are pulled from Hugging Face into the app data directory once, and
//! from then on dictation runs offline — no API key, no provider, no network.
//!
//! Inference itself happens in the webview (transformers.js + ONNX Runtime).
//! Rust owns the download and serves the files over a private URI scheme, so
//! the webview never talks to the network directly.

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

use crate::error::{AppError, Result};
use crate::storage::Paths;

/// Files transformers.js needs for a Whisper repo, besides the weights.
const CONFIG_FILES: &[&str] = &[
    "config.json",
    "generation_config.json",
    "preprocessor_config.json",
    "tokenizer.json",
    "tokenizer_config.json",
];

/// Quantised weights: encoder plus the merged decoder (dtype `q8`).
const WEIGHT_FILES: &[&str] = &[
    "onnx/encoder_model_quantized.onnx",
    "onnx/decoder_model_merged_quantized.onnx",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalModel {
    /// Short id used by the UI and stored in settings.
    pub id: String,
    /// Hugging Face repository.
    pub repo: String,
    pub label: String,
    /// Rough download size in bytes (quantised encoder + decoder + configs).
    pub size_bytes: u64,
    pub note: String,
    #[serde(default)]
    pub installed: bool,
    #[serde(default)]
    pub bytes_on_disk: u64,
}

/// The sizes below are the real quantised file sizes reported by the Hub.
pub fn catalogue() -> Vec<LocalModel> {
    vec![
        LocalModel {
            id: "whisper-tiny".into(),
            repo: "onnx-community/whisper-tiny".into(),
            label: "Whisper tiny".into(),
            size_bytes: 41_000_000,
            note: "самая быстрая, ru/uk/en, для коротких заметок".into(),
            installed: false,
            bytes_on_disk: 0,
        },
        LocalModel {
            id: "whisper-base".into(),
            repo: "onnx-community/whisper-base".into(),
            label: "Whisper base".into(),
            size_bytes: 76_000_000,
            note: "баланс скорости и точности".into(),
            installed: false,
            bytes_on_disk: 0,
        },
        LocalModel {
            id: "whisper-small".into(),
            repo: "onnx-community/whisper-small".into(),
            label: "Whisper small".into(),
            size_bytes: 240_000_000,
            note: "лучшее качество для русского и украинского".into(),
            installed: false,
            bytes_on_disk: 0,
        },
    ]
}

pub fn find(id: &str) -> Result<LocalModel> {
    catalogue()
        .into_iter()
        .find(|m| m.id == id)
        .ok_or_else(|| AppError::NotFound(format!("local model {id}")))
}

pub fn models_dir(paths: &Paths) -> PathBuf {
    paths.root.join("models")
}

pub fn model_dir(paths: &Paths, model: &LocalModel) -> PathBuf {
    models_dir(paths).join(&model.repo)
}

fn required_files() -> Vec<String> {
    CONFIG_FILES
        .iter()
        .chain(WEIGHT_FILES.iter())
        .map(|f| f.to_string())
        .collect()
}

/// A model counts as installed only when every required file is on disk.
pub fn is_installed(paths: &Paths, model: &LocalModel) -> bool {
    let dir = model_dir(paths, model);
    required_files().iter().all(|f| dir.join(f).is_file())
}

pub fn bytes_on_disk(paths: &Paths, model: &LocalModel) -> u64 {
    let dir = model_dir(paths, model);
    required_files()
        .iter()
        .filter_map(|f| std::fs::metadata(dir.join(f)).ok())
        .map(|m| m.len())
        .sum()
}

pub fn list(paths: &Paths) -> Vec<LocalModel> {
    catalogue()
        .into_iter()
        .map(|mut model| {
            model.installed = is_installed(paths, &model);
            model.bytes_on_disk = bytes_on_disk(paths, &model);
            model
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct DownloadProgress {
    pub model_id: String,
    pub file: String,
    pub file_index: usize,
    pub file_count: usize,
    pub received: u64,
    pub total: u64,
}

/// Download every missing file of a model, reporting progress as it goes.
pub async fn download(
    http: &reqwest::Client,
    paths: &Paths,
    model: &LocalModel,
    on_progress: &(dyn Fn(DownloadProgress) + Send + Sync),
) -> Result<LocalModel> {
    let dir = model_dir(paths, model);
    std::fs::create_dir_all(dir.join("onnx"))?;

    let files = required_files();
    let file_count = files.len();

    for (file_index, file) in files.iter().enumerate() {
        let target = dir.join(file);
        if target.is_file() {
            continue;
        }
        let url = format!(
            "https://huggingface.co/{}/resolve/main/{}?download=true",
            model.repo, file
        );
        let response = http
            .get(&url)
            .send()
            .await
            .map_err(|e| AppError::Http(e.to_string()))?;
        if !response.status().is_success() {
            return Err(AppError::Http(format!(
                "{} → HTTP {}",
                file,
                response.status()
            )));
        }
        let total = response.content_length().unwrap_or(0);

        // Write to a temporary name so a cancelled download never looks
        // installed.
        let tmp = target.with_extension("part");
        let mut sink = std::fs::File::create(&tmp)?;
        let mut received = 0u64;
        let mut stream = response.bytes_stream();
        let mut last_report = 0u64;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| AppError::Http(e.to_string()))?;
            std::io::Write::write_all(&mut sink, &chunk)?;
            received += chunk.len() as u64;
            // Report at most every 512 KB to keep the IPC channel quiet.
            if received - last_report > 512 * 1024 {
                last_report = received;
                on_progress(DownloadProgress {
                    model_id: model.id.clone(),
                    file: file.clone(),
                    file_index,
                    file_count,
                    received,
                    total,
                });
            }
        }
        drop(sink);
        std::fs::rename(&tmp, &target)?;

        on_progress(DownloadProgress {
            model_id: model.id.clone(),
            file: file.clone(),
            file_index,
            file_count,
            received,
            total: received,
        });
    }

    let mut done = model.clone();
    done.installed = is_installed(paths, model);
    done.bytes_on_disk = bytes_on_disk(paths, model);
    Ok(done)
}

pub fn remove(paths: &Paths, model: &LocalModel) -> Result<()> {
    let dir = model_dir(paths, model);
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    Ok(())
}

/// Map a URI path from the private scheme onto a file inside the models
/// directory, refusing anything that tries to leave it.
pub fn resolve_asset(paths: &Paths, uri_path: &str) -> Option<PathBuf> {
    let decoded = percent_decode(uri_path);
    let relative = decoded.trim_start_matches('/');
    if relative.is_empty() {
        return None;
    }
    let candidate = Path::new(relative);
    for component in candidate.components() {
        if !matches!(component, Component::Normal(_)) {
            return None;
        }
    }
    let full = models_dir(paths).join(candidate);
    // Only serve files that really live under the models directory.
    let base = std::fs::canonicalize(models_dir(paths)).ok()?;
    let real = std::fs::canonicalize(&full).ok()?;
    if !real.starts_with(&base) || !real.is_file() {
        return None;
    }
    Some(real)
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

pub fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("json") => "application/json",
        Some("onnx") => "application/octet-stream",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::new_id;

    fn temp_paths() -> Paths {
        let dir = std::env::temp_dir().join(format!("velvet-whisper-{}", new_id()));
        Paths::new(dir).unwrap()
    }

    #[test]
    fn catalogue_is_multilingual_and_ordered_by_size() {
        let models = catalogue();
        assert_eq!(models.len(), 3);
        for model in &models {
            // The `.en` repos are English-only and would break ru/uk dictation.
            assert!(
                !model.repo.ends_with(".en"),
                "{} is English-only",
                model.repo
            );
            assert!(model.repo.starts_with("onnx-community/"));
        }
        assert!(models[0].size_bytes < models[1].size_bytes);
        assert!(models[1].size_bytes < models[2].size_bytes);
    }

    #[test]
    fn model_counts_as_installed_only_with_every_file() {
        let paths = temp_paths();
        let model = find("whisper-tiny").unwrap();
        assert!(!is_installed(&paths, &model));

        let dir = model_dir(&paths, &model);
        std::fs::create_dir_all(dir.join("onnx")).unwrap();
        for file in CONFIG_FILES {
            std::fs::write(dir.join(file), b"{}").unwrap();
        }
        // Config files alone are not enough.
        assert!(!is_installed(&paths, &model));

        for file in WEIGHT_FILES {
            std::fs::write(dir.join(file), b"weights").unwrap();
        }
        assert!(is_installed(&paths, &model));
        assert_eq!(list(&paths).iter().filter(|m| m.installed).count(), 1);

        remove(&paths, &model).unwrap();
        assert!(!is_installed(&paths, &model));
    }

    #[test]
    fn asset_paths_cannot_escape_the_models_directory() {
        let paths = temp_paths();
        let model = find("whisper-tiny").unwrap();
        let dir = model_dir(&paths, &model);
        std::fs::create_dir_all(dir.join("onnx")).unwrap();
        std::fs::write(dir.join("config.json"), b"{}").unwrap();
        // A secret sitting next to the models directory.
        std::fs::write(paths.root.join("secrets.json"), b"key").unwrap();

        let good = resolve_asset(&paths, "/onnx-community/whisper-tiny/config.json");
        assert!(good.is_some());

        for evil in [
            "/../secrets.json",
            "/onnx-community/../../secrets.json",
            "/onnx-community/whisper-tiny/../../../secrets.json",
        ] {
            assert!(
                resolve_asset(&paths, evil).is_none(),
                "{evil} must be refused"
            );
        }
    }

    #[test]
    fn percent_encoded_paths_are_decoded() {
        assert_eq!(percent_decode("/a%20b/c.json"), "/a b/c.json");
        assert_eq!(percent_decode("/plain.json"), "/plain.json");
    }
}
