//! M1: keyless integration acceptance. Drives the REAL dsh-jsonrpc-agent
//! runtime from the checkout through the llm-replay overlay - the same
//! mechanics as examples/jsonrpc-agent/tests/sdk.snapshot.ts - and asserts a
//! clean transcript, the persisted JSONL session, and a clean exit-0
//! shutdown. This is the removal note's 'assembled lifecycle and transcript
//! acceptance' for the theus frontend.
//!
//! Skips itself without DSH_CHECKOUT (repo convention: keyless tests never
//! need a key or network; they need a checkout + pnpm install).
//! CI runs it with --include-ignored.

use std::collections::HashMap;
use std::path::PathBuf;

use dsh_harness_client::api::{DeepSeekHarness, DeepSeekHarnessOptions, PromptInput, RunOptions};
use dsh_harness_client::client::HarnessClient;
use dsh_harness_client::client::HarnessClientOptions;
use dsh_harness_client::launch::{resolve_launch, RuntimeMode};
use dsh_harness_client::protocol::{AgentStatus, InitializeParams, ModelSelection, ProviderDraft};

const SCENARIO_PROMPT: &str = "Reply with exactly: SDK snapshot OK";
const SCENARIO_SESSION: &str = "sdk-snapshot-text";
const SCENARIO_FINAL: &str = "SDK snapshot OK";

fn checkout() -> Option<PathBuf> {
    std::env::var_os("DSH_CHECKOUT").map(PathBuf::from)
}

fn runtime_mode() -> RuntimeMode {
    match std::env::var("DSH_EXAMPLE_MODE").ok() {
        Some(raw) => RuntimeMode::parse(&raw).unwrap_or_default(),
        None => RuntimeMode::default(),
    }
}

#[tokio::test]
#[ignore = "requires a DeepSeek Harness checkout (DSH_CHECKOUT) with pnpm install"]
async fn replays_text_turn_through_real_runtime() {
    let Some(checkout) = checkout() else {
        eprintln!("skipping: set DSH_CHECKOUT to a DeepSeek Harness checkout to run the keyless integration");
        return;
    };
    let temp = tempfile::tempdir().expect("tempdir");
    let cwd = temp.path().to_path_buf();
    let sessions_root = cwd.join(".sessions");
    std::fs::create_dir_all(&sessions_root).expect("sessions root");

    // Hydrate the replay fixture into the isolated cwd ({{cwd}} token).
    let fixture_source =
        checkout.join("examples/jsonrpc-agent/tests/snapshots/text-turn/session.jsonl");
    let fixture = cwd.join("session.jsonl");
    let hydrated = std::fs::read_to_string(&fixture_source)
        .expect("fixture readable")
        .replace("{{cwd}}", &cwd.to_string_lossy());
    std::fs::write(&fixture, hydrated).expect("fixture hydrated");

    let overlay = checkout.join("examples/jsonrpc-agent/cordis.snapshot.yml");
    assert!(
        overlay.is_file(),
        "replay overlay missing: {}",
        overlay.display()
    );

    let launch = resolve_launch(&checkout, runtime_mode()).expect("launch resolves");
    let mut env: HashMap<String, String> = std::env::vars().collect();
    for (key, value) in launch.env {
        env.insert(key, value);
    }
    env.insert(
        "DSH_CORDIS_CONFIG".to_string(),
        overlay.to_string_lossy().to_string(),
    );
    env.insert(
        "DSH_SNAPSHOT_FILE".to_string(),
        fixture.to_string_lossy().to_string(),
    );
    env.insert(
        "DSH_SESSION_ROOT".to_string(),
        sessions_root.to_string_lossy().to_string(),
    );
    env.insert("DSH_CWD".to_string(), cwd.to_string_lossy().to_string());
    env.insert("DSH_SNAPSHOT".to_string(), "replay".to_string());

    let mut harness = DeepSeekHarness::new(DeepSeekHarnessOptions {
        launch: HarnessClientOptions {
            command: launch.command,
            args: launch.args,
            cwd: Some(cwd.clone()),
            env: Some(env),
            request_timeout_ms: Some(110_000),
            shutdown_timeout_ms: 10_000,
            dispose_eof_grace_ms: 10_000,
            dispose_grace_ms: 5_000,
        },
        cwd: Some(cwd.clone()),
        provider: Some("deepseek-official".to_string()),
        model: Some("deepseek-v4-flash".to_string()),
        max_tokens: None,
    });

    let result = harness
        .run(
            PromptInput::Text(SCENARIO_PROMPT.to_string()),
            &RunOptions {
                session_id: Some(SCENARIO_SESSION.to_string()),
            },
        )
        .await
        .expect("turn completes");

    // Clean transcript: the replay's recorded final answer.
    assert_eq!(result.final_response, SCENARIO_FINAL);
    // The owned interval ends on the root session's next idle.
    assert_eq!(
        result.notifications.last().unwrap().session_status(),
        Some((SCENARIO_SESSION, AgentStatus::Idle))
    );
    // The durable enqueue receipt and the committed assistant message are in
    // the event stream.
    assert!(result.events.iter().any(|e| e.inbox_splice().is_some()));
    assert!(result.events.iter().any(|e| e
        .assistant_message()
        .map(|m| m.message.text() == SCENARIO_FINAL)
        .unwrap_or(false)));
    // No stdout violations: stdout carried only JSON-RPC frames.
    assert!(harness.client().stdout_violations().is_empty());

    // Clean shutdown first: the protocol shutdown drains persistence, so the
    // stored log is complete before it is read back (mirroring the TS test's
    // ordering: run -> close -> read persisted logs).
    harness.close().await.expect("clean teardown");
    assert_eq!(harness.client().exit_code(), Some(Some(0)));

    // Persisted JSONL session under the sessions root.
    let jsonl = walk_jsonl(&sessions_root);
    assert_eq!(
        jsonl.len(),
        1,
        "expected exactly one persisted session log, got {jsonl:?}"
    );
    let content = std::fs::read_to_string(&jsonl[0]).expect("log readable");
    let header: serde_json::Value =
        serde_json::from_str(content.lines().next().expect("log has a header line"))
            .expect("header is JSON");
    assert_eq!(
        header["type"], "session",
        "log starts with the session header"
    );
    let types: Vec<String> = content
        .lines()
        .filter_map(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| v["type"].as_str().map(str::to_string))
        })
        .collect();
    assert!(
        types.iter().any(|t| t == "assistant/message"),
        "log carries the assistant message; types: {types:?}"
    );
}

#[tokio::test]
#[ignore = "requires a DeepSeek Harness checkout (DSH_CHECKOUT) with pnpm install"]
async fn management_protocol_works_with_bundled_composition() {
    let Some(checkout) = checkout() else { return };
    let temp = tempfile::tempdir().expect("tempdir");
    let cwd = temp.path().join("workspace");
    let dsh_home = temp.path().join("dsh-home");
    let sessions = temp.path().join("sessions");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&dsh_home).unwrap();
    std::fs::create_dir_all(&sessions).unwrap();

    let launch = resolve_launch(&checkout, runtime_mode()).expect("launch resolves");
    let config = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../cordis.yml");
    let mut env: HashMap<String, String> = std::env::vars().collect();
    for (key, value) in launch.env {
        env.insert(key, value);
    }
    env.insert("DSH_CORDIS_CONFIG".into(), config.to_string_lossy().into());
    env.insert("DSH_HOME".into(), dsh_home.to_string_lossy().into());
    env.insert("DSH_SESSION_ROOT".into(), sessions.to_string_lossy().into());
    env.insert("DSH_CWD".into(), cwd.to_string_lossy().into());

    let mut client = HarnessClient::new(HarnessClientOptions {
        command: launch.command,
        args: launch.args,
        cwd: Some(cwd.clone()),
        env: Some(env),
        request_timeout_ms: Some(30_000),
        shutdown_timeout_ms: 10_000,
        dispose_eof_grace_ms: 10_000,
        dispose_grace_ms: 5_000,
    });
    let initialized = client
        .initialize(InitializeParams {
            cwd: cwd.to_string_lossy().into(),
            provider: "deepseek-official".into(),
            model: "deepseek-v4-flash".into(),
            reasoning_effort: None,
            max_tokens: None,
        })
        .await
        .expect("initialize");
    assert!(initialized
        .capabilities
        .iter()
        .any(|method| method == "session/resume"));

    let catalog = client.model_catalog().await.expect("model catalog");
    assert!(catalog.providers.iter().any(|provider| {
        provider.provider == "deepseek-official" && provider.active && !provider.models.is_empty()
    }));
    client
        .select_model(
            "pending-session",
            &ModelSelection {
                provider: "deepseek-official".into(),
                model: "deepseek-v4-flash".into(),
                reasoning_effort: Some("max".into()),
            },
        )
        .await
        .expect("pending session model selection");

    let draft = ProviderDraft {
        provider: "openai".into(),
        display_name: None,
        base_url: None,
        api: None,
        credential_ref: None,
        credential_value: None,
        model_ids: Vec::new(),
    };
    assert!(!client
        .discover_provider(&draft)
        .await
        .expect("catalog discovery")
        .models
        .is_empty());
    client
        .add_provider(&draft)
        .await
        .expect("persist catalog provider");
    let catalog = client.model_catalog().await.expect("updated model catalog");
    assert!(catalog
        .providers
        .iter()
        .any(|provider| provider.provider == "openai" && provider.active));
    client.close().await.expect("clean close");
}

fn walk_jsonl(root: &std::path::Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}
