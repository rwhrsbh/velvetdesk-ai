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
        model_chain: vec![],
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
        images: vec![],
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

/// The gauge promises "how full is the window". This checks that promise
/// against the only authority on it — the provider's own tokenizer — and keeps
/// the fallback estimate honest enough to stand in for it.
#[tokio::test]
#[ignore = "calls the real Gemini API"]
async fn the_context_gauge_matches_what_the_provider_counts() {
    let Some(mut harness) = Harness::new() else {
        return;
    };
    // The gauge describes the mode the operator has selected, so both sides of
    // this comparison have to be in the same one.
    harness.settings.agent_mode = AgentMode::Act;

    let config = provider(MODEL);
    let emit = quiet();

    let scope = harness.paths.scope("7100004").unwrap();
    let mut profile = Profile::new("7100004".into(), "Марина".into());
    profile.site = "RomanceCompass".into();
    profile.bio = "Зрелая, тёплая, ценит уважение. Пятеро детей, Оснабрюк.".into();
    scope.write_profile(&profile).unwrap();

    for i in 0..8 {
        let man = velvetdesk_lib::models::Man::new(
            "7100004".into(),
            format!("70000{i:02}"),
            format!("Кандидат {i}"),
        );
        scope.write_man(&man).unwrap();
    }

    let request =
        agent::next_request(&scope, &harness.settings, &config, None).expect("the next request");
    let key = api_keys().into_iter().next().unwrap_or_default();
    let exact =
        velvetdesk_lib::llm::gemini::count_tokens(&harness.llm.http, &config, &key, &request)
            .await
            .expect("countTokens answers");

    let stats =
        agent::context_stats(&scope, &harness.settings, &config, None).expect("the estimate");

    let input = RunInput {
        model_id: "7100004".into(),
        man_id: None,
        mode: Some(AgentMode::Act),
        security: Some(SecurityLevel::Safe),
        message: "Скажи одним словом: готова?".into(),
        channel: None,
        log_incoming: false,
        thinking_effort: Some("none".into()),
        temporary: true,
        images: vec![],
    };
    let output = agent::run(&harness.deps(&config, &emit), input)
        .await
        .expect("one turn");

    let counted = output.usage.prompt_tokens as f32;
    println!(
        "    countTokens said {exact}, the run cost {} — estimate was {}",
        output.usage.prompt_tokens, stats.used_tokens
    );

    // The counted request is the same one, minus the operator's short message,
    // so the two numbers should be within a few percent of each other.
    let exact = exact as f32;
    assert!(
        exact > counted * 0.85 && exact < counted * 1.15,
        "countTokens ({exact}) disagrees with the run ({counted})"
    );

    // The estimate only has to be close enough to drive a gauge and a
    // compaction threshold — half to double is the promise it makes.
    let estimated = stats.used_tokens as f32;
    assert!(
        estimated > counted * 0.5 && estimated < counted * 2.0,
        "the estimate is misleading: {estimated} against {counted}"
    );
}

/// The point of a chain: when a model will not answer — out of quota, or
/// simply not there — the next one takes the request rather than the operator
/// seeing an error.
#[tokio::test]
#[ignore = "calls the real Gemini API"]
async fn an_unavailable_model_hands_over_to_the_next() {
    let Some(harness) = Harness::new() else {
        return;
    };

    let mut config = provider("gemini-does-not-exist");
    config.model_chain = vec!["also-not-a-model".into(), MODEL.to_string()];

    let switches = std::sync::Mutex::new(Vec::<String>::new());
    let emit = |event: serde_json::Value| {
        if event["kind"] == "model_switch" {
            switches.lock().unwrap().push(format!(
                "{} → {}",
                event["from"].as_str().unwrap_or_default(),
                event["to"].as_str().unwrap_or_default()
            ));
        }
    };

    let mut request = velvetdesk_lib::llm::ChatRequest::new("Answer with one word.");
    request.stream = false;
    request
        .messages
        .push(velvetdesk_lib::llm::LlmMessage::user("Скажи «готово»."));

    let response = harness
        .llm
        .chat(&config, harness.pool.clone(), &request, &emit)
        .await
        .expect("the chain finds a model that answers");

    for switch in switches.lock().unwrap().iter() {
        println!("    {switch}");
    }
    assert_eq!(response.model, MODEL, "the last model should have answered");
    assert!(!response.text.trim().is_empty());
    assert_eq!(
        switches.lock().unwrap().len(),
        2,
        "both dead models should have been reported"
    );
}

// ---------------------------------------------------------------------------
// Letters
// ---------------------------------------------------------------------------

/// Two women, two voices, the same brief. The letters have to come out
/// recognisably different — that is the whole point of giving a profile
/// samples of how she writes.
#[tokio::test]
#[ignore = "calls the real Gemini API"]
async fn each_woman_writes_in_her_own_voice() {
    let Some(harness) = Harness::new() else {
        return;
    };
    let config = provider(MODEL);
    let emit = quiet();

    // One writes in short, plain lines. The other is warm and wordy.
    let terse = harness.paths.scope("7200001").unwrap();
    let mut profile = Profile::new("7200001".into(), "Ирина".into());
    profile.bio = "Врач, 38, Киев. Мало свободного времени.".into();
    profile.tone_rules = vec!["короткие предложения".into(), "без восклицаний".into()];
    profile.writing_samples = vec![
        "Привет. День был длинный, две операции подряд. Как ты?".into(),
        "Спасибо за фото. Красиво. У нас дождь вторую неделю.".into(),
    ];
    terse.write_profile(&profile).unwrap();
    let mut man =
        velvetdesk_lib::models::Man::new("7200001".into(), "7200101".into(), "Hartwig".into());
    man.age = Some(65);
    man.location = "Bückeburg".into();
    terse.write_man(&man).unwrap();

    let warm = harness.paths.scope("7200002").unwrap();
    let mut profile = Profile::new("7200002".into(), "Алёна".into());
    profile.bio = "Воспитатель в детском саду, 29, Одесса. Любит море.".into();
    profile.tone_rules = vec!["тёплый тон".into(), "много бытовых деталей".into()];
    profile.writing_samples = vec![
        "Мой дорогой, сегодня утром я шла на работу и увидела, как рыбаки \
         вытаскивают сети, и подумала о тебе — ты бы точно остановился \
         посмотреть, правда? Дети сегодня рисовали море, и я поставила их \
         рисунки на подоконник, чтобы солнце их грело."
            .into(),
        "Знаешь, я всё думаю о твоих словах. Вечером заварила чай с мятой, \
         села у окна и долго смотрела на огни в порту."
            .into(),
    ];
    warm.write_profile(&profile).unwrap();
    let mut man =
        velvetdesk_lib::models::Man::new("7200002".into(), "7200201".into(), "Hartwig".into());
    man.age = Some(65);
    man.location = "Bückeburg".into();
    warm.write_man(&man).unwrap();

    let brief = "Поблагодари за фотографию и спроси про его выходные.";
    let mut written = vec![];
    for model_id in ["7200001", "7200002"] {
        let output = agent::write_letters(
            &harness.deps(&config, &emit),
            agent::LettersInput {
                model_id: model_id.into(),
                man_ids: vec![],
                temporary: true,
                brief: brief.into(),
                channel: Some("letter".into()),
                thinking_effort: Some("low".into()),
            },
        )
        .await
        .expect("letters are written");

        assert_eq!(output.letters.len(), 1);
        let letter = &output.letters[0];
        assert!(letter.error.is_empty(), "{}", letter.error);
        assert!(!letter.text.is_empty(), "an empty letter");
        println!("--- {model_id}: {}\n{}\n", letter.name, letter.text);
        written.push(letter.text.clone());
    }

    let (terse_letter, warm_letter) = (&written[0], &written[1]);

    // A letter is prose to be sent, not a reply to the operator.
    for letter in &written {
        let lower = letter.to_lowercase();
        for giveaway in ["subject:", "тема:", "вот письмо", "here is the letter"] {
            assert!(
                !lower.contains(giveaway),
                "letter reads as a draft: {letter}"
            );
        }
        // Naming him in the opening line is a style choice, not a requirement;
        // what matters is that the brief was actually written about.
        let about_the_brief = ["фото", "photo", "foto"]
            .iter()
            .any(|word| lower.contains(word))
            && ["выходн", "weekend", "wochenende"]
                .iter()
                .any(|word| lower.contains(word));
        assert!(about_the_brief, "the brief was ignored: {letter}");
    }

    // The wordy one should be plainly longer, and its sentences longer too.
    let sentence_length = |text: &str| {
        let sentences = text
            .split(['.', '!', '?'])
            .filter(|s| s.trim().len() > 3)
            .count();
        text.chars().count() as f32 / sentences.max(1) as f32
    };
    println!(
        "    terse: {} chars, {:.0} per sentence · warm: {} chars, {:.0} per sentence",
        terse_letter.chars().count(),
        sentence_length(terse_letter),
        warm_letter.chars().count(),
        sentence_length(warm_letter)
    );
    assert!(
        sentence_length(warm_letter) > sentence_length(terse_letter),
        "the two voices came out the same"
    );
}

/// A brief goes to everyone in the profile, and each letter is written for its
/// own man rather than copied.
#[tokio::test]
#[ignore = "calls the real Gemini API"]
async fn a_round_of_letters_is_written_one_by_one() {
    let Some(harness) = Harness::new() else {
        return;
    };
    let config = provider(MODEL);
    let emit = quiet();

    let scope = harness.paths.scope("7200003").unwrap();
    let mut profile = Profile::new("7200003".into(), "Марина".into());
    profile.bio = "Флорист, 42, Оснабрюк.".into();
    profile.writing_samples =
        vec!["Доброе утро. Сегодня собирала букет из белых роз — вспомнила наш разговор.".into()];
    scope.write_profile(&profile).unwrap();

    for (id, name, place) in [
        ("7200301", "Hartwig", "Bückeburg"),
        ("7200302", "Sven", "Malmö"),
        ("7200303", "Josef", "Wien"),
    ] {
        let mut man = velvetdesk_lib::models::Man::new("7200003".into(), id.into(), name.into());
        man.location = place.into();
        scope.write_man(&man).unwrap();
    }

    let output = agent::write_letters(
        &harness.deps(&config, &emit),
        agent::LettersInput {
            model_id: "7200003".into(),
            man_ids: vec![],
            temporary: false,
            brief: "Короткое письмо: как прошли выходные.".into(),
            channel: Some("letter".into()),
            thinking_effort: Some("none".into()),
        },
    )
    .await
    .expect("a round of letters");

    assert_eq!(output.letters.len(), 3);
    for letter in &output.letters {
        assert!(letter.error.is_empty(), "{}: {}", letter.name, letter.error);
        // No letter may name a different man — the round is personal, not a
        // template with the wrong name pasted in.
        let lower = letter.text.to_lowercase();
        for other in ["hartwig", "sven", "josef"] {
            if other != letter.name.to_lowercase() {
                assert!(
                    !lower.contains(other),
                    "{} was sent a letter mentioning {other}: {}",
                    letter.name,
                    letter.text
                );
            }
        }
        println!("--- {}\n{}\n", letter.name, letter.text);
    }

    // Written one by one, so no two are the same text.
    let first = &output.letters[0].text;
    assert!(
        output.letters.iter().skip(1).any(|l| &l.text != first),
        "every man got the same letter"
    );
    assert!(output.usage.total_tokens > 0, "usage was not reported");

    // And the round is part of the conversation it was asked for in: it was
    // living only on screen, and vanished on the next switch of profile.
    let log = scope.read_agent_log(None).unwrap();
    assert_eq!(
        log.entries
            .iter()
            .filter(|e| e.sender == "assistant")
            .count(),
        3,
        "the letters were not kept in the chat"
    );
    assert!(
        log.entries.iter().any(|e| e.sender == "user"),
        "the brief was not kept"
    );
    assert_eq!(log.entries[1].meta["recipient"], output.letters[0].name);

    // Writing to one man files the letter in his own chat instead.
    let single = agent::write_letters(
        &harness.deps(&config, &emit),
        agent::LettersInput {
            model_id: "7200003".into(),
            man_ids: vec!["7200301".into()],
            temporary: false,
            brief: "Короткое письмо про погоду.".into(),
            channel: Some("chat".into()),
            thinking_effort: Some("none".into()),
        },
    )
    .await
    .expect("one letter");
    assert_eq!(single.letters.len(), 1);
    let his_chat = scope.read_agent_log(Some("7200301")).unwrap();
    assert_eq!(
        his_chat
            .entries
            .iter()
            .filter(|e| e.sender == "assistant")
            .count(),
        1,
        "the letter did not land in his chat"
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
            images: vec![],
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
        images: vec![],
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
        images: vec![],
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
