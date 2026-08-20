//! M0: the wire client against a Rust fake runtime speaking the protocol —
//! handshake, prompt enqueue receipt, notification fan-out, error mapping,
//! request timeout, the teardown ladder (shutdown -> EOF -> SIGTERM ->
//! SIGKILL), stdin-EOF disposal, and reuse refusal after close.

use std::collections::HashMap;

use dsh_harness_client::api::{
    normalize_input, DeepSeekHarness, DeepSeekHarnessOptions, PromptInput, RunOptions,
};
use dsh_harness_client::client::{HarnessClient, HarnessClientOptions};
use dsh_harness_client::error::Error;
use dsh_harness_client::protocol::{AgentStatus, InitializeParams};
use dsh_harness_client::session::ContentBlock;
use serde_json::json;

fn fake_bin() -> String {
    env!("CARGO_BIN_EXE_fake-runtime").to_string()
}

fn client(env: HashMap<String, String>) -> HarnessClient {
    HarnessClient::new(HarnessClientOptions {
        command: fake_bin(),
        args: vec![],
        cwd: None,
        env: Some(env_with_path(env)),
        request_timeout_ms: Some(5_000),
        shutdown_timeout_ms: 1_000,
        dispose_eof_grace_ms: 1_000,
        dispose_grace_ms: 1_000,
    })
}

/// The fake is a Rust binary needing no PATH; pass-through is kept for
/// parity with the real runtime launch (which needs node).
fn env_with_path(mut env: HashMap<String, String>) -> HashMap<String, String> {
    env.insert(
        "PATH".to_string(),
        std::env::var("PATH").unwrap_or_default(),
    );
    env
}

fn inbox_receipt_event(message_id: &str) -> serde_json::Value {
    json!({
        "type": "agent/inbox/spliced", "seq": 0, "time": 1,
        "data": { "target": "next-turn", "start": 0, "inserted": [
            { "id": message_id, "role": "user", "content": [{ "type": "text", "text": "hi" }],
              "source": { "kind": "user" } }
        ] }
    })
}

fn full_turn_script() -> serde_json::Value {
    json!({
        "initialize": [{ "result": { "serverInfo": { "name": "deepseek-harness-sdk-runtime", "version": "0.0.1" } } }],
        "session/prompt": [{
            "send": [
                { "method": "session.status", "params": { "sessionId": "sdk-1", "status": "running" } },
                { "method": "session.event", "params": { "sessionId": "sdk-1", "event": inbox_receipt_event("msg-1") } },
                { "method": "session.event", "params": { "sessionId": "other", "event": { "type": "user/message", "seq": 1, "time": 2, "data": { "id": "x", "role": "user", "content": [], "source": { "kind": "user" } } } } },
                { "method": "session.event", "params": { "sessionId": "sdk-1", "event": {
                    "type": "assistant/message", "seq": 2, "time": 3,
                    "data": { "turn": 1, "step": 1, "message": {
                        "id": "a1", "role": "assistant",
                        "content": [{ "type": "reasoning", "text": "thinking" }, { "type": "text", "text": "final text" }],
                        "source": { "kind": "model", "provider": "deepseek-official", "model": "deepseek-v4-flash" }
                    }, "usage": { "inputTokens": 10, "outputTokens": 2 } } } } },
                { "method": "session.status", "params": { "sessionId": "sdk-1", "status": "idle" } }
            ],
            "result": { "messageId": "msg-1" }
        }],
        "shutdown": [{ "result": {} }]
    })
}

#[tokio::test]
async fn handshake_prompt_and_fan_out() {
    let mut client = client(HashMap::from([(
        "FAKE_RUNTIME_SCRIPT".to_string(),
        full_turn_script().to_string(),
    )]));
    let info = client
        .initialize(InitializeParams {
            cwd: "/tmp".to_string(),
            provider: "deepseek-official".to_string(),
            model: "deepseek-v4-flash".to_string(),
            reasoning_effort: None,
            max_tokens: None,
        })
        .await
        .unwrap();
    assert_eq!(info.server_info.name, "deepseek-harness-sdk-runtime");
    assert_eq!(info.server_info.version, "0.0.1");

    let mut sub = client.subscribe();
    let message_id = client
        .prompt(
            "sdk-1",
            vec![ContentBlock::Text {
                text: "hi".to_string(),
            }],
        )
        .await
        .unwrap();
    assert_eq!(message_id, "msg-1");

    let mut collected = Vec::new();
    for _ in 0..5 {
        collected.push(sub.next().await.unwrap());
    }
    assert_eq!(
        collected[0].session_status(),
        Some(("sdk-1", AgentStatus::Running))
    );
    assert_eq!(collected[1].method, "session.event");
    assert!(collected[1]
        .session_event()
        .unwrap()
        .inbox_splice()
        .is_some());
    assert_eq!(collected[2].session_id(), Some("other"));
    assert_eq!(
        collected[3]
            .session_event()
            .unwrap()
            .assistant_message()
            .unwrap()
            .message
            .text(),
        "final text"
    );
    assert_eq!(
        ("sdk-1", AgentStatus::Idle),
        collected[4].session_status().unwrap()
    );

    client.close().await.unwrap();
    assert_eq!(client.exit_code(), Some(Some(0)));
    assert!(client.stdout_violations().is_empty());
}

#[tokio::test]
async fn wire_error_maps_code_and_data() {
    let script = json!({
        "initialize": [{ "error": { "code": -32099, "message": "no adapter registered", "data": { "provider": "nope" } } }]
    });
    let mut client = client(HashMap::from([(
        "FAKE_RUNTIME_SCRIPT".to_string(),
        script.to_string(),
    )]));
    let error = client
        .initialize(InitializeParams {
            cwd: "/tmp".to_string(),
            provider: "nope".to_string(),
            model: "x".to_string(),
            reasoning_effort: None,
            max_tokens: None,
        })
        .await
        .unwrap_err();
    match error {
        Error::Wire(wire) => {
            assert_eq!(wire.code, -32099);
            assert_eq!(wire.message, "no adapter registered");
            assert_eq!(wire.data, Some(json!({ "provider": "nope" })));
        }
        other => panic!("expected wire error, got {other:?}"),
    }
    client.close().await.unwrap();
}

#[tokio::test]
async fn unknown_method_answers_32601() {
    let mut client = client(HashMap::new());
    let error = client
        .request("nope/method", json!({}), None)
        .await
        .unwrap_err();
    match error {
        Error::Wire(wire) => assert_eq!(wire.code, -32601),
        other => panic!("expected wire error, got {other:?}"),
    }
    client.close().await.unwrap();
}

#[tokio::test]
async fn request_timeout_is_an_abandonment() {
    let script = json!({
        "initialize": [{ "result": { "serverInfo": { "name": "deepseek-harness-sdk-runtime", "version": "0.0.1" } } }],
        "session/prompt": [{ "hang": true }]
    });
    let mut client = HarnessClient::new(HarnessClientOptions {
        command: fake_bin(),
        args: vec![],
        cwd: None,
        env: Some(env_with_path(HashMap::from([(
            "FAKE_RUNTIME_SCRIPT".to_string(),
            script.to_string(),
        )]))),
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
        .unwrap();
    let start = std::time::Instant::now();
    let error = client
        .request(
            "session/prompt",
            json!({ "sessionId": "s", "contentBlocks": [] }),
            Some(200),
        )
        .await
        .unwrap_err();
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
    match error {
        Error::RequestTimeout { method, timeout_ms } => {
            assert_eq!(method, "session/prompt");
            assert_eq!(timeout_ms, 200);
        }
        other => panic!("expected timeout, got {other:?}"),
    }
    // The runtime stayed alive through the abandonment: close must reap it.
    client.close().await.unwrap();
}

#[tokio::test]
async fn owned_run_collects_receipt_to_idle() {
    let env = HashMap::from([(
        "FAKE_RUNTIME_SCRIPT".to_string(),
        full_turn_script().to_string(),
    )]);
    let mut harness = DeepSeekHarness::new(DeepSeekHarnessOptions {
        launch: HarnessClientOptions {
            command: fake_bin(),
            args: vec![],
            cwd: None,
            env: Some(env_with_path(env)),
            request_timeout_ms: Some(5_000),
            shutdown_timeout_ms: 1_000,
            dispose_eof_grace_ms: 1_000,
            dispose_grace_ms: 1_000,
        },
        cwd: None,
        provider: None,
        model: None,
        max_tokens: None,
    });
    let result = harness
        .run(
            PromptInput::Text("hi".to_string()),
            &RunOptions {
                session_id: Some("sdk-1".to_string()),
            },
        )
        .await
        .unwrap();
    assert_eq!(result.session_id, "sdk-1");
    assert_eq!(result.final_response, "final text");
    // The interval runs from the enqueue receipt through idle: the foreign
    // session event never enters the root event list.
    assert_eq!(result.events.len(), 2);
    assert!(result.events[0].inbox_splice().is_some());
    assert!(result.events[1].assistant_message().is_some());
    assert_eq!(
        result.notifications.last().unwrap().session_status(),
        Some(("sdk-1", AgentStatus::Idle))
    );
    harness.close().await.unwrap();
    assert_eq!(harness.client().exit_code(), Some(Some(0)));
}

#[tokio::test]
async fn teardown_ladder_shutdown_then_eof() {
    // Clean path: protocol shutdown answers, the child exits 0.
    let mut client = client(HashMap::new());
    client.start().unwrap();
    client.close().await.unwrap();
    assert_eq!(client.exit_code(), Some(Some(0)));
}

#[tokio::test]
async fn teardown_ladder_eof_tier() {
    // The fake ignores stdin EOF, so the ladder must escalate to SIGTERM
    // (the fake's default disposition: killed by signal, exit code None).
    let env = HashMap::from([
        ("FAKE_RUNTIME_IGNORE_EOF".to_string(), "1".to_string()),
        // No shutdown handler: the protocol shutdown exchange gets -32601,
        // which is diagnostic-only, then the ladder proceeds.
        ("FAKE_RUNTIME_SCRIPT".to_string(), json!({ "initialize": [{ "result": { "serverInfo": { "name": "x", "version": "0" } } }] }).to_string()),
    ]);
    let mut client = client(env);
    client.start().unwrap();
    client.close().await.unwrap();
    assert_eq!(client.exit_code(), Some(None)); // exited by signal
}

#[tokio::test]
async fn teardown_ladder_sigkill_tier() {
    // The fake ignores both EOF and SIGTERM: only SIGKILL can end it.
    let env = HashMap::from([
        ("FAKE_RUNTIME_IGNORE_EOF".to_string(), "1".to_string()),
        ("FAKE_RUNTIME_IGNORE_SIGTERM".to_string(), "1".to_string()),
        ("FAKE_RUNTIME_SCRIPT".to_string(), json!({ "initialize": [{ "result": { "serverInfo": { "name": "x", "version": "0" } } }] }).to_string()),
    ]);
    let mut client = client(env);
    client.start().unwrap();
    client.close().await.unwrap();
    assert_eq!(client.exit_code(), Some(None)); // SIGKILL
}

#[tokio::test]
async fn transport_death_fails_pending_with_stderr_tail() {
    // The fake dies right after answering initialize: a later prompt races
    // the exit edge and must reject with TransportClosed carrying the tail.
    let env = HashMap::from([
        ("FAKE_RUNTIME_STDERR_TAIL".to_string(), "diagnostic line one,diagnostic line two".to_string()),
        ("FAKE_RUNTIME_SCRIPT".to_string(), json!({
            "initialize": [{ "result": { "serverInfo": { "name": "x", "version": "0" } }, "exit": 3 }]
        }).to_string()),
    ]);
    let mut client = client(env);
    client
        .initialize(InitializeParams {
            cwd: "/tmp".to_string(),
            provider: "deepseek-official".to_string(),
            model: "x".to_string(),
            reasoning_effort: None,
            max_tokens: None,
        })
        .await
        .unwrap();
    let error = client.prompt("s", vec![]).await.unwrap_err();
    match error {
        Error::TransportClosed(transport) => {
            assert_eq!(transport.exit_code, Some(3));
            assert_eq!(
                transport.stderr_tail,
                vec!["diagnostic line one", "diagnostic line two"]
            );
        }
        other => panic!("expected transport closed, got {other:?}"),
    }
}

#[tokio::test]
async fn close_refuses_reuse() {
    let mut client = client(HashMap::new());
    client.start().unwrap();
    client.close().await.unwrap();
    let error = client
        .request("initialize", json!({}), None)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::TransportClosed(_)));
}

#[tokio::test]
async fn stdout_violations_are_recorded() {
    let env = HashMap::from([
        ("FAKE_RUNTIME_NOISY_STDOUT".to_string(), "1".to_string()),
        ("FAKE_RUNTIME_SCRIPT".to_string(), json!({ "initialize": [{ "result": { "serverInfo": { "name": "x", "version": "0" } } }] }).to_string()),
    ]);
    let mut client = client(env);
    client
        .initialize(InitializeParams {
            cwd: "/tmp".to_string(),
            provider: "deepseek-official".to_string(),
            model: "x".to_string(),
            reasoning_effort: None,
            max_tokens: None,
        })
        .await
        .unwrap();
    client.close().await.unwrap();
    let violations = client.stdout_violations();
    assert!(!violations.is_empty());
    assert!(violations
        .iter()
        .any(|line| line.contains("deliberate stdout violation")));
}

#[tokio::test]
async fn normalize_input_string_becomes_text_block() {
    let blocks = normalize_input(PromptInput::Text("hello".to_string()));
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].as_text(), Some("hello"));
}

#[tokio::test]
async fn management_methods_are_typed_and_validate_results() {
    use dsh_harness_client::protocol::{ModelSelection, ProviderDraft};

    let script = json!({
        "initialize": [{ "result": {
            "serverInfo": { "name": "deepseek-harness-sdk-runtime", "version": "0.0.1" },
            "capabilities": ["model/catalog", "session/resume"]
        }}],
        "model/catalog": [{ "result": { "providers": [{
            "provider": "route", "displayName": "Route", "active": true,
            "declared": false, "models": [{ "id": "model", "name": "Model" }]
        }] }}],
        "session/select-model": [{ "result": { "selected": {
            "provider": "route", "model": "model", "reasoningEffort": "high"
        } }}],
        "session/list": [{ "result": { "sessions": [{
            "sessionId": "s1", "title": "Prior", "createdAt": 1,
            "lastActivityAt": 2, "live": false, "unreadable": false
        }] }}],
        "session/resume": [{ "result": {
            "sessionId": "s1", "title": "Prior", "events": [],
            "selection": { "provider": "route", "model": "model" },
            "status": "idle", "routable": true
        }}],
        "provider/discover": [{ "result": { "models": [{ "id": "found" }] }}],
        "provider/add": [{ "result": { "provider": "route-2" } }],
        "shutdown": [{ "result": {} }]
    });
    let mut client = client(HashMap::from([(
        "FAKE_RUNTIME_SCRIPT".to_string(),
        script.to_string(),
    )]));
    let initialized = client
        .initialize(InitializeParams {
            cwd: "/tmp".into(),
            provider: "route".into(),
            model: "model".into(),
            reasoning_effort: None,
            max_tokens: None,
        })
        .await
        .unwrap();
    assert_eq!(
        initialized.capabilities,
        ["model/catalog", "session/resume"]
    );
    assert_eq!(
        client.model_catalog().await.unwrap().providers[0].models[0].id,
        "model"
    );
    let selection = ModelSelection {
        provider: "route".into(),
        model: "model".into(),
        reasoning_effort: Some("high".into()),
    };
    assert_eq!(
        client
            .select_model("s1", &selection)
            .await
            .unwrap()
            .selected,
        selection
    );
    assert_eq!(
        client.list_sessions().await.unwrap().sessions[0]
            .title
            .as_deref(),
        Some("Prior")
    );
    assert!(client.resume_session("s1").await.unwrap().routable);
    let draft = ProviderDraft {
        provider: "route-2".into(),
        display_name: None,
        base_url: None,
        api: None,
        credential_ref: None,
        credential_value: None,
        model_ids: Vec::new(),
    };
    assert_eq!(
        client.discover_provider(&draft).await.unwrap().models[0].id,
        "found"
    );
    assert_eq!(
        client.add_provider(&draft).await.unwrap().provider,
        "route-2"
    );
    client.close().await.unwrap();
}

#[tokio::test]
async fn management_methods_are_bounded_by_a_default_timeout() {
    // A hung runtime response to a management RPC (e.g. provider/discover
    // probing an unreachable custom endpoint) must not block the caller
    // forever the way a hung session/prompt legitimately could.
    let script = json!({
        "initialize": [{ "result": { "serverInfo": { "name": "deepseek-harness-sdk-runtime", "version": "0.0.1" } } }],
        "model/catalog": [{ "hang": true }]
    });
    let mut client = HarnessClient::new(HarnessClientOptions {
        command: fake_bin(),
        args: vec![],
        cwd: None,
        env: Some(env_with_path(HashMap::from([(
            "FAKE_RUNTIME_SCRIPT".to_string(),
            script.to_string(),
        )]))),
        request_timeout_ms: Some(3_000),
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
        .unwrap();
    let start = std::time::Instant::now();
    let error = client.model_catalog().await.unwrap_err();
    assert!(start.elapsed() < std::time::Duration::from_secs(6));
    match error {
        Error::RequestTimeout { method, .. } => assert_eq!(method, "model/catalog"),
        other => panic!("expected a request timeout, got {other:?}"),
    }
}
