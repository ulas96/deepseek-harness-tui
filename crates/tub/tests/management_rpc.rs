//! Regression coverage for HANDOFF.md finding 1: management RPCs
//! (`model/catalog`, `session/select-model`, `session/list`,
//! `session/resume`, `provider/discover`, `provider/add`) must not block the
//! TUI event loop while in flight. This drives the real client + transport
//! stack against a scripted fake runtime (mirrors `tests/snapshots.rs`), not
//! `App`'s own event loop (which needs a real terminal), so it asserts the
//! narrower, directly-testable claim: dispatching a slash command that
//! issues a management RPC returns promptly even when that RPC hangs,
//! because the RPC now runs on a spawned task instead of being awaited
//! inline.

use std::collections::HashMap;
use std::time::Duration;

use dsh_harness_client::client::{HarnessClient, HarnessClientOptions};
use dsh_harness_client::protocol::{InitializeParams, ModelSelection};
use tokio::sync::mpsc;
use tub::app::App;
use tub::commands::SlashCommand;

fn fake_bin() -> String {
    env!("CARGO_BIN_EXE_tub-fake-runtime").to_string()
}

#[tokio::test]
async fn management_rpc_does_not_block_the_caller_while_hung() {
    let script = serde_json::json!({
        "initialize": [{ "result": { "serverInfo": { "name": "deepseek-harness-sdk-runtime", "version": "0.0.1" } } }],
        "model/catalog": [{ "hang": true }]
    });
    let mut env = HashMap::new();
    env.insert("FAKE_RUNTIME_SCRIPT".to_string(), script.to_string());
    env.insert(
        "PATH".to_string(),
        std::env::var("PATH").unwrap_or_default(),
    );
    let mut client = HarnessClient::new(HarnessClientOptions {
        command: fake_bin(),
        args: vec![],
        cwd: None,
        env: Some(env),
        request_timeout_ms: None,
        shutdown_timeout_ms: 1_000,
        dispose_eof_grace_ms: 1_000,
        dispose_grace_ms: 1_000,
    });
    client
        .initialize(InitializeParams {
            cwd: "/tmp".to_string(),
            provider: "deepseek-official".to_string(),
            model: "x".to_string(),
            reasoning_effort: None,
            max_tokens: None,
        })
        .await
        .expect("handshake");
    // HarnessClient::clone shares the same runtime, so this handle can tear
    // the child down after the test even though the original moves into App.
    let mut teardown = client.clone();

    let (mgmt_tx, mut mgmt_rx) = mpsc::unbounded_channel();
    let mut app = App::new(
        client,
        mgmt_tx,
        ModelSelection {
            provider: "p".into(),
            model: "m".into(),
            reasoning_effort: None,
        },
        None,
        Some("s1".to_string()),
        std::path::PathBuf::from("/work"),
    );

    let dispatched = tokio::time::timeout(
        Duration::from_millis(500),
        app.dispatch_slash_for_test(SlashCommand::Model(None)),
    )
    .await;
    assert!(
        dispatched.is_ok(),
        "dispatching /model should return promptly instead of blocking on the hung RPC"
    );
    // The hung runtime never replies, so no management reply has landed yet.
    assert!(mgmt_rx.try_recv().is_err());

    teardown.close().await.ok();
}
