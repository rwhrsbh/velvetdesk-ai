//! Downloads a real Whisper model from Hugging Face. Ignored by default
//! because it pulls ~41 MB:
//!
//! ```bash
//! cargo test --test local_models -- --ignored --nocapture
//! ```

use velvetdesk_lib::storage::Paths;
use velvetdesk_lib::whisper;

fn temp_paths(tag: &str) -> Paths {
    let dir = std::env::temp_dir().join(format!("velvet-model-test-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    Paths::new(dir).unwrap()
}

#[tokio::test]
#[ignore = "downloads ~41 MB from Hugging Face"]
async fn downloads_the_tiny_model_and_serves_it() {
    let paths = temp_paths("tiny");
    let model = whisper::find("whisper-tiny").unwrap();
    assert!(!whisper::is_installed(&paths, &model));

    let http = reqwest::Client::new();
    let seen = std::sync::Mutex::new(Vec::<String>::new());
    let report = |p: whisper::DownloadProgress| {
        let mut seen = seen.lock().unwrap();
        if !seen.contains(&p.file) {
            println!("· {} ({}/{})", p.file, p.file_index + 1, p.file_count);
            seen.push(p.file);
        }
    };

    let done = whisper::download(&http, &paths, &model, &report)
        .await
        .expect("download");

    assert!(done.installed, "every required file must be present");
    println!(
        "downloaded {:.1} MB (catalogue estimate {:.1} MB)",
        done.bytes_on_disk as f64 / 1_048_576.0,
        model.size_bytes as f64 / 1_048_576.0
    );

    // The estimate shown in the UI must be close to reality.
    let ratio = done.bytes_on_disk as f64 / model.size_bytes as f64;
    assert!(
        (0.8..1.2).contains(&ratio),
        "catalogue size is off: {ratio:.2}x"
    );

    // The webview reads the weights through this resolver.
    let served = whisper::resolve_asset(
        &paths,
        "/onnx-community/whisper-tiny/onnx/encoder_model_quantized.onnx",
    )
    .expect("weights must be reachable over the model scheme");
    assert!(std::fs::metadata(&served).unwrap().len() > 1_000_000);

    // A second run is a no-op: nothing is re-downloaded.
    let again = whisper::download(&http, &paths, &model, &|_| {})
        .await
        .expect("second run");
    assert_eq!(again.bytes_on_disk, done.bytes_on_disk);

    whisper::remove(&paths, &model).unwrap();
    assert!(!whisper::is_installed(&paths, &model));
}
