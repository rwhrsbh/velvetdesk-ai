//! Live provider checks. Ignored by default — they need a real API key and
//! network access:
//!
//! ```bash
//! GEMINI_API_KEY=… cargo test --test live_speech -- --ignored --nocapture
//! GROQ_API_KEY=…   cargo test --test live_speech -- --ignored --nocapture
//! ```
//!
//! They verify what unit tests cannot: that the endpoints, auth headers and
//! response shapes this app assumes are the ones the providers actually serve.

use base64::Engine;
use velvetdesk_lib::config::{ProviderConfig, ProviderKind};
use velvetdesk_lib::llm::catalog;

fn gemini(model: &str, speech: &str) -> ProviderConfig {
    ProviderConfig {
        id: "gemini".into(),
        label: "Gemini".into(),
        kind: ProviderKind::Gemini,
        base_url: "https://generativelanguage.googleapis.com".into(),
        api_version: "v1beta".into(),
        model: model.into(),
        extra_headers: vec![],
        temperature: 0.0,
        max_output_tokens: None,
        transcribe_model: speech.into(),
        key_count: 1,
    }
}

fn groq(speech: &str) -> ProviderConfig {
    ProviderConfig {
        id: "groq".into(),
        label: "Groq".into(),
        kind: ProviderKind::OpenaiCompatible,
        base_url: "https://api.groq.com/openai/v1".into(),
        api_version: "v1".into(),
        model: "llama-3.3-70b-versatile".into(),
        extra_headers: vec![],
        temperature: 0.0,
        max_output_tokens: None,
        transcribe_model: speech.into(),
        key_count: 1,
    }
}

/// A one-second 16 kHz mono WAV holding a 440 Hz tone.
///
/// Real speech is not needed: these tests assert that the request shape is
/// accepted and parsed, not that a specific sentence comes back.
fn tone_wav() -> Vec<u8> {
    let sample_rate = 16_000u32;
    let samples: Vec<i16> = (0..sample_rate)
        .map(|i| {
            let t = i as f32 / sample_rate as f32;
            ((t * 440.0 * std::f32::consts::TAU).sin() * 6000.0) as i16
        })
        .collect();

    let data_len = (samples.len() * 2) as u32;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // PCM header size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

fn clip_base64() -> String {
    base64::engine::general_purpose::STANDARD.encode(tone_wav())
}

fn key(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|k| !k.trim().is_empty())
}

#[tokio::test]
#[ignore = "needs GEMINI_API_KEY"]
async fn gemini_lists_models_and_flags_speech_capable_ones() {
    let Some(api_key) = key("GEMINI_API_KEY") else {
        eprintln!("skipped: no GEMINI_API_KEY");
        return;
    };
    let http = reqwest::Client::new();
    let catalog = catalog::list_models(&http, &gemini("gemini-2.5-flash", ""), &api_key)
        .await
        .expect("model list");

    println!(
        "api {} · {} models, {} accept audio",
        catalog.api_version,
        catalog.models.len(),
        catalog.models.iter().filter(|m| m.audio).count()
    );
    assert!(!catalog.models.is_empty());

    // Nothing that only makes images, speech or vectors may reach the
    // dictation picker.
    for model in catalog.models.iter().filter(|m| m.audio) {
        let id = model.id.to_lowercase();
        assert!(
            !id.contains("image")
                && !id.contains("tts")
                && !id.contains("embedding")
                && !id.contains("banana")
                && !id.contains("live"),
            "{id} must not be offered for dictation"
        );
    }
}

#[tokio::test]
#[ignore = "needs GEMINI_API_KEY"]
async fn gemini_transcribes_through_generate_content() {
    let Some(api_key) = key("GEMINI_API_KEY") else {
        eprintln!("skipped: no GEMINI_API_KEY");
        return;
    };
    let http = reqwest::Client::new();
    let provider = gemini("gemini-2.5-flash", "gemini-2.5-flash");
    let result = catalog::transcribe(&http, &provider, &api_key, &clip_base64(), "audio/wav").await;
    match result {
        Ok(text) => println!("inline transcript: {text:?}"),
        Err(err) => panic!("inline transcription failed: {}", err.message()),
    }
}

#[tokio::test]
#[ignore = "needs GEMINI_API_KEY"]
async fn gemini_transcribes_through_interactions_api() {
    let Some(api_key) = key("GEMINI_API_KEY") else {
        eprintln!("skipped: no GEMINI_API_KEY");
        return;
    };
    let http = reqwest::Client::new();
    // Dedicated speech model: uploads via Files API, then Interactions.
    let provider = gemini("gemini-2.5-flash", "gemini-3.5-transcribe");
    match catalog::transcribe(&http, &provider, &api_key, &clip_base64(), "audio/wav").await {
        Ok(text) => println!("transcribe-model transcript: {text:?}"),
        Err(err) => panic!("interactions transcription failed: {}", err.message()),
    }
}

#[tokio::test]
#[ignore = "needs GROQ_API_KEY"]
async fn groq_transcribes_through_openai_endpoint() {
    let Some(api_key) = key("GROQ_API_KEY") else {
        eprintln!("skipped: no GROQ_API_KEY");
        return;
    };
    let http = reqwest::Client::new();
    let provider = groq("whisper-large-v3-turbo");
    match catalog::transcribe(&http, &provider, &api_key, &clip_base64(), "audio/wav").await {
        Ok(text) => println!("groq transcript: {text:?}"),
        Err(err) => panic!("groq transcription failed: {}", err.message()),
    }
}

/// Points at any OpenAI-compatible local server (whisper.cpp `whisper-server`,
/// faster-whisper-server, LM Studio). Set LOCAL_STT_URL to its /v1 base.
#[tokio::test]
#[ignore = "needs LOCAL_STT_URL"]
async fn local_server_transcribes() {
    let Some(base) = key("LOCAL_STT_URL") else {
        eprintln!("skipped: no LOCAL_STT_URL");
        return;
    };
    let http = reqwest::Client::new();
    let mut provider = groq(&std::env::var("LOCAL_STT_MODEL").unwrap_or("whisper-1".into()));
    provider.base_url = base;
    match catalog::transcribe(&http, &provider, "local", &clip_base64(), "audio/wav").await {
        Ok(text) => println!("local transcript: {text:?}"),
        Err(err) => panic!("local transcription failed: {}", err.message()),
    }
}
