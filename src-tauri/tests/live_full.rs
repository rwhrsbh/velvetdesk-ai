//! End-to-end checks against the real Gemini API.
//!
//! These are the questions no unit test can answer: does the model actually
//! call our tools, does a thinking level reach it intact, does the master agent
//! do what an operator asks in one sentence, does an agent really run a shell
//! command and write a file.
//!
//! Ignored by default — they cost tokens and need a key. Run with:
//!
//! ```text
//! cargo test --test live_full -- --ignored --nocapture
//! ```
//!
//! The key comes from the app's own store (`VELVETDESK_DATA_DIR`, or the
//! platform app-data directory), or from `GEMINI_API_KEY`. Nothing is printed.

use std::path::PathBuf;
use std::sync::Arc;

use velvetdesk_lib::agent::{self, master, AgentDeps, RunInput};
use velvetdesk_lib::config::{AgentMode, ProviderConfig, ProviderKind, SecurityLevel, Settings};
use velvetdesk_lib::llm::keypool::KeyPool;
use velvetdesk_lib::llm::LlmClient;
use velvetdesk_lib::models::{new_id, Profile};
use velvetdesk_lib::storage::Paths;
use velvetdesk_lib::workspace::TrustedRoot;

/// Model used for everything that does not name one: the lightest of the
/// current generation, which has the most generous free quota.
const MODEL: &str = "gemini-3.5-flash-lite";
/// Fallback when the light model is out of quota for the day.
const HEAVIER: &str = "gemini-3.5-flash";

fn app_data_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("VELVETDESK_DATA_DIR") {
        return Some(PathBuf::from(dir));
    }
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(|base| PathBuf::from(base).join("ai.velvetdesk.app"))
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join("Library/Application Support")
                .join("ai.velvetdesk.app")
        })
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(".local/share/ai.velvetdesk.app"))
    }
}

/// The operator's own key, read from where the app keeps it.
fn api_keys() -> Vec<String> {
    if let Ok(key) = std::env::var("GEMINI_API_KEY") {
        if !key.trim().is_empty() {
            return vec![key];
        }
    }
    let Some(base) = app_data_dir() else {
        return vec![];
    };
    let Ok(paths) = Paths::new(base) else {
        return vec![];
    };
    let Ok(secrets) = velvetdesk_lib::config::Secrets::load(&paths) else {
        return vec![];
    };
    secrets.keys.get("gemini").cloned().unwrap_or_default()
}

fn provider(model: &str) -> ProviderConfig {
    ProviderConfig {
        id: "gemini".into(),
        label: "Gemini".into(),
        kind: ProviderKind::Gemini,
        base_url: "https://generativelanguage.googleapis.com".into(),
        api_version: "v1beta".into(),
        model: model.into(),
        extra_headers: vec![],
        temperature: 0.6,
        max_output_tokens: None,
        transcribe_model: String::new(),
        thinking_effort: String::new(),
        thinking_budget: None,
        reasoning_dialect: "auto".into(),
        context_tokens: None,
        key_count: 1,
    }
}

struct Harness {
    paths: Paths,
    settings: Settings,
    llm: LlmClient,
    pool: Arc<KeyPool>,
}

impl Harness {
    fn new() -> Option<Harness> {
        let keys = api_keys();
        if keys.is_empty() {
            eprintln!("no Gemini key available — skipping");
            return None;
        }
        let dir = std::env::temp_dir().join(format!("velvet-live-{}", new_id()));
        let paths = Paths::new(dir).expect("temp app data");
        let settings = Settings {
            ui_language: "ru".into(),
            max_tool_turns: 8,
            ..Settings::default()
        };
        Some(Harness {
            paths,
            settings,
            llm: LlmClient::new(),
            pool: Arc::new(KeyPool::new(keys)),
        })
    }

    fn deps<'a>(
        &'a self,
        provider: &'a ProviderConfig,
        emit: &'a (dyn Fn(serde_json::Value) + Send + Sync),
    ) -> AgentDeps<'a> {
        AgentDeps {
            paths: &self.paths,
            settings: &self.settings,
            provider,
            pool: self.pool.clone(),
            llm: &self.llm,
            emit,
        }
    }
}

fn quiet() -> impl Fn(serde_json::Value) + Send + Sync {
    |_| {}
}

/// Print the steps a run took, so a failure says what the model did.
fn report(label: &str, steps: &[agent::RunStep], reply: &str) {
    println!("--- {label}");
    for step in steps {
        println!("    [{}] {}", step.kind, step.summary);
    }
    println!(
        "    reply: {}",
        reply
            .replace('\n', " ")
            .chars()
            .take(160)
            .collect::<String>()
    );
}

// ---------------------------------------------------------------------------
// Thinking controls
// ---------------------------------------------------------------------------

/// Every level the interface offers has to reach the API intact — a wrong
/// field name is a hard 400, which is exactly the bug this guards.
#[tokio::test]
#[ignore = "calls the real Gemini API"]
async fn every_thinking_level_is_accepted() {
    let Some(harness) = Harness::new() else {
        return;
    };
    let emit = quiet();

    for model in [MODEL, HEAVIER, "gemini-2.5-flash"] {
        for level in ["", "none", "low", "medium", "high", "xhigh"] {
            let mut config = provider(model);
            config.thinking_effort = level.to_string();

            let mut request = velvetdesk_lib::llm::ChatRequest::new(
                "Answer with one short sentence, no preamble.",
            );
            request.stream = false;
            request.thinking = velvetdesk_lib::llm::Thinking {
                effort: level.to_string(),
                budget_tokens: None,
            };
            request.messages.push(velvetdesk_lib::llm::LlmMessage::user(
                "Сколько будет 17 + 25?",
            ));

            let result = harness
                .llm
                .chat(&config, harness.pool.clone(), &request, &emit)
                .await;

            match result {
                Ok(response) => {
                    println!(
                        "    {model} · {:<7} → {}",
                        if level.is_empty() { "auto" } else { level },
                        response
                            .text
                            .replace('\n', " ")
                            .chars()
                            .take(60)
                            .collect::<String>()
                    );
                    let answer = response.text.to_lowercase();
                    assert!(
                        answer.contains("42") || answer.contains("сорок два"),
                        "{model}/{level} answered {:?}",
                        response.text
                    );
                }
                // A free-tier day runs out; that says nothing about the field
                // names, which is what this test is for.
                Err(err) if err.to_string().contains("429") => {
                    println!("    {model} · {level:<7} → quota exhausted, skipping the rest");
                    break;
                }
                Err(err) => panic!("{model} rejected thinking level {level:?}: {err}"),
            }
        }
    }
}

/// A token budget is the older spelling, and the app has to translate between
/// the two rather than send whichever the operator happened to save.
#[tokio::test]
#[ignore = "calls the real Gemini API"]
async fn a_thinking_budget_works_on_both_generations() {
    let Some(harness) = Harness::new() else {
        return;
    };
    let emit = quiet();

    for model in ["gemini-2.5-flash", MODEL] {
        let config = provider(model);
        let mut request = velvetdesk_lib::llm::ChatRequest::new("Answer with one short sentence.");
        request.stream = false;
        request.thinking = velvetdesk_lib::llm::Thinking {
            effort: String::new(),
            budget_tokens: Some(1024),
        };
        request.messages.push(velvetdesk_lib::llm::LlmMessage::user(
            "Назови столицу Германии.",
        ));

        let response = harness
            .llm
            .chat(&config, harness.pool.clone(), &request, &emit)
            .await
            .unwrap_or_else(|e| panic!("{model} rejected a 1024-token budget: {e}"));
        assert!(
            response.text.to_lowercase().contains("берлин")
                || response.text.to_lowercase().contains("berlin"),
            "{model} answered {:?}",
            response.text
        );
    }
}

/// Streaming is what the operator watches; it has to deliver the answer in
/// pieces and end up with the same text.
#[tokio::test]
#[ignore = "calls the real Gemini API"]
async fn a_streamed_answer_arrives_in_pieces() {
    let Some(harness) = Harness::new() else {
        return;
    };
    let deltas = std::sync::Mutex::new(Vec::<String>::new());
    let emit = |event: serde_json::Value| {
        if event["kind"] == "delta" {
            deltas
                .lock()
                .unwrap()
                .push(event["text"].as_str().unwrap_or_default().to_string());
        }
    };

    let mut config = provider(MODEL);
    config.thinking_effort = "low".into();
    let mut request = velvetdesk_lib::llm::ChatRequest::new("Answer in Russian.");
    request.thinking = velvetdesk_lib::llm::Thinking {
        effort: "low".into(),
        budget_tokens: None,
    };
    request.messages.push(velvetdesk_lib::llm::LlmMessage::user(
        "Перечисли пять городов Германии, по одному в строке.",
    ));

    let response = harness
        .llm
        .chat(&config, harness.pool.clone(), &request, &emit)
        .await
        .expect("streamed answer");

    let pieces = deltas.lock().unwrap().len();
    println!("    {pieces} pieces, {} chars", response.text.len());
    assert!(
        pieces > 1,
        "the answer arrived in one lump: {pieces} events"
    );
    assert!(!response.text.trim().is_empty());
}

/// Which raw levels each model actually accepts. Not an assertion — a probe,
/// so the mapping in the app can be based on what the API says rather than on
/// what its documentation says.
#[tokio::test]
#[ignore = "calls the real Gemini API"]
async fn probe_which_levels_each_model_takes() {
    let Some(harness) = Harness::new() else {
        return;
    };

    for model in [MODEL, HEAVIER, "gemini-2.5-flash", "gemini-2.5-flash-lite"] {
        for level in ["minimal", "low", "medium", "high"] {
            let body = serde_json::json!({
                "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
                "generationConfig": { "thinkingLevel": level },
            });
            let outcome = probe_raw(&harness, model, &body).await;
            println!("    {model} · thinkingLevel={level:<8} → {outcome}");
        }
        for budget in [0, 128, 512] {
            let body = serde_json::json!({
                "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
                "generationConfig": { "thinkingConfig": { "thinkingBudget": budget } },
            });
            let outcome = probe_raw(&harness, model, &body).await;
            println!("    {model} · thinkingBudget={budget:<6} → {outcome}");
        }
    }
}

async fn probe_raw(harness: &Harness, model: &str, body: &serde_json::Value) -> String {
    let key = api_keys().into_iter().next().unwrap_or_default();
    let url =
        format!("https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent");
    let response = harness
        .llm
        .http
        .post(url)
        .header("x-goog-api-key", key)
        .json(body)
        .send()
        .await;
    match response {
        Ok(response) => {
            let status = response.status();
            if status.is_success() {
                "ok".to_string()
            } else {
                let text = response.text().await.unwrap_or_default();
                let message = serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                    .unwrap_or(text);
                format!("{status}: {}", message.chars().take(90).collect::<String>())
            }
        }
        Err(err) => format!("transport: {err}"),
    }
}

// ---------------------------------------------------------------------------
// The scoped agent
// ---------------------------------------------------------------------------

/// The operator dictates a fact; the model is expected to store it through the
/// tools rather than just talk about it.
#[tokio::test]
#[ignore = "calls the real Gemini API"]
async fn the_agent_stores_what_it_is_told() {
    let Some(harness) = Harness::new() else {
        return;
    };
    let config = provider(MODEL);
    let emit = quiet();

    let scope = harness.paths.scope("7100001").unwrap();
    let mut profile = Profile::new("7100001".into(), "Марина".into());
    profile.site = "RomanceCompass".into();
    scope.write_profile(&profile).unwrap();

    let input = RunInput {
        model_id: "7100001".into(),
        man_id: None,
        mode: Some(AgentMode::Auto),
        security: Some(SecurityLevel::Yolo),
        message: "Заведи досье на Хартвига, 65 лет, из Бюккебурга. Запиши факт: любит рыбалку."
            .into(),
        channel: None,
        log_incoming: false,
        thinking_effort: Some("low".into()),
        temporary: true,
    };

    let output = agent::run(&harness.deps(&config, &emit), input)
        .await
        .expect("the run finishes");
    report("agent stores a fact", &output.steps, &output.reply);

    let men = scope.read_all_men().unwrap();
    assert_eq!(men.len(), 1, "one dossier expected, got {}", men.len());
    let man = &men[0];
    assert!(
        man.name.to_lowercase().contains("хартвиг") || man.name.to_lowercase().contains("hartwig"),
        "unexpected name: {}",
        man.name
    );
    assert!(
        man.facts
            .iter()
            .any(|f| f.value.to_lowercase().contains("рыбал")),
        "the fact was not stored: {:?}",
        man.facts
    );
}

// ---------------------------------------------------------------------------
// The master agent
// ---------------------------------------------------------------------------

/// The scenario an operator actually types: a new profile and three men, in one
/// sentence, with nothing on disk yet.
#[tokio::test]
#[ignore = "calls the real Gemini API"]
async fn the_master_creates_a_profile_and_its_men() {
    let Some(harness) = Harness::new() else {
        return;
    };
    let config = provider(MODEL);
    let emit = quiet();

    let output = master::chat(
        &harness.deps(&config, &emit),
        master::MasterInput {
            message: "Мне нужно, чтобы появилось три новых мужика на анкете Карина, 43 года: \
                      Денис, Кирилл и Владислав. Анкеты Карина ещё нет — создай её."
                .into(),
            security: Some(SecurityLevel::Yolo),
            thinking_effort: Some("medium".into()),
            temporary: true,
        },
    )
    .await
    .expect("the master answers");
    report("master creates a profile", &output.steps, &output.reply);

    let index = velvetdesk_lib::storage::rebuild_index(&harness.paths).unwrap();
    let karina = index
        .models
        .iter()
        .find(|m| m.name.to_lowercase().contains("карин"))
        .expect("the profile was created");
    let profile = harness
        .paths
        .scope(&karina.id)
        .unwrap()
        .read_profile()
        .unwrap();
    assert_eq!(profile.age, Some(43), "age was not carried over");

    let names: Vec<String> = karina.men.iter().map(|m| m.name.to_lowercase()).collect();
    for expected in ["денис", "кирилл", "владислав"] {
        assert!(
            names.iter().any(|n| n.contains(expected)),
            "{expected} is missing, got {names:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Files and commands
// ---------------------------------------------------------------------------

/// The agent is asked to look at a folder, run a command in it and write a
/// file — the whole workspace surface, driven by the model rather than by a
/// direct call.
#[tokio::test]
#[ignore = "calls the real Gemini API"]
async fn the_agent_runs_a_command_and_writes_a_file() {
    let Some(mut harness) = Harness::new() else {
        return;
    };

    let workspace = std::env::temp_dir().join(format!("velvet-live-ws-{}", new_id()));
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("notes.txt"), b"first line\n").unwrap();
    let workspace = std::fs::canonicalize(&workspace).unwrap();

    harness.settings.trusted_roots = vec![TrustedRoot {
        path: workspace.to_string_lossy().to_string(),
        writable: true,
        granted_at: chrono::Utc::now(),
        reason: "live test".into(),
    }];

    let config = provider(MODEL);
    let emit = quiet();
    let scope = harness.paths.scope("7100002").unwrap();
    scope
        .write_profile(&Profile::new("7100002".into(), "Марина".into()))
        .unwrap();

    let input = RunInput {
        model_id: "7100002".into(),
        man_id: None,
        mode: Some(AgentMode::Auto),
        security: Some(SecurityLevel::Yolo),
        message: format!(
            "В папке {} посмотри файлы, выполни в ней команду, которая печатает \
             текущую папку, и создай файл report.txt со строкой READY внутри.",
            velvetdesk_lib::workspace::display_path(&workspace)
        ),
        channel: None,
        log_incoming: false,
        thinking_effort: Some("low".into()),
        temporary: true,
    };

    let output = agent::run(&harness.deps(&config, &emit), input)
        .await
        .expect("the run finishes");
    report("agent uses the workspace", &output.steps, &output.reply);

    let used: Vec<&str> = output
        .steps
        .iter()
        .filter_map(|s| s.tool.as_deref())
        .collect();
    assert!(
        used.contains(&"shell"),
        "no command was run, tools used: {used:?}"
    );

    let report_file = workspace.join("report.txt");
    assert!(
        report_file.is_file(),
        "report.txt was not written, tools used: {used:?}"
    );
    assert!(std::fs::read_to_string(&report_file)
        .unwrap()
        .to_uppercase()
        .contains("READY"));
}

/// A folder that was never granted stays out of reach, whatever the model is
/// asked to do with it.
#[tokio::test]
#[ignore = "calls the real Gemini API"]
async fn an_ungranted_folder_stays_out_of_reach() {
    let Some(harness) = Harness::new() else {
        return;
    };

    let secret = std::env::temp_dir().join(format!("velvet-live-secret-{}", new_id()));
    std::fs::create_dir_all(&secret).unwrap();
    std::fs::write(secret.join("private.txt"), b"do not read me").unwrap();

    let config = provider(MODEL);
    let emit = quiet();
    let scope = harness.paths.scope("7100003").unwrap();
    scope
        .write_profile(&Profile::new("7100003".into(), "Марина".into()))
        .unwrap();

    let input = RunInput {
        model_id: "7100003".into(),
        man_id: None,
        mode: Some(AgentMode::Auto),
        security: Some(SecurityLevel::Yolo),
        message: format!(
            "Прочитай файл {} и скажи, что в нём написано.",
            secret.join("private.txt").display()
        ),
        channel: None,
        log_incoming: false,
        thinking_effort: Some("low".into()),
        temporary: true,
    };

    let output = agent::run(&harness.deps(&config, &emit), input)
        .await
        .expect("the run finishes");
    report("agent is refused a folder", &output.steps, &output.reply);

    assert!(
        !output.reply.contains("do not read me"),
        "the file's contents leaked into the answer"
    );
    let errors = output
        .steps
        .iter()
        .filter(|s| s.kind == "tool_error")
        .count();
    let asked = output
        .steps
        .iter()
        .any(|s| s.tool.as_deref() == Some("request_access"));
    assert!(
        errors > 0 || asked,
        "the agent neither failed nor asked for access: {:?}",
        output.steps
    );
}

/// The tool declarations the models receive have to stay valid: a single bad
/// schema is a 400 for the whole request, whatever it was about.
#[tokio::test]
#[ignore = "calls the real Gemini API"]
async fn every_declared_tool_is_accepted_by_the_api() {
    let Some(harness) = Harness::new() else {
        return;
    };
    let emit = quiet();

    let mut request = velvetdesk_lib::llm::ChatRequest::new("Answer with one word.");
    request.stream = false;
    // The master set already contains the scoped tools and the workspace ones,
    // so this is every declaration the app can send.
    request.tools = master::tool_defs();
    request
        .messages
        .push(velvetdesk_lib::llm::LlmMessage::user("Скажи «готово»."));

    println!("    {} tool declarations", request.tools.len());
    let response = harness
        .llm
        .chat(&provider(MODEL), harness.pool.clone(), &request, &emit)
        .await
        .expect("the API accepts every declaration");
    assert!(!response.text.trim().is_empty());
}
