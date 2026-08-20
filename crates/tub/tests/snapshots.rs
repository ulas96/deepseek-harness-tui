//! M2/M3: ratatui TestBackend frame snapshots against a scripted fake
//! runtime, plus the pure event->UI mapping tests. The frames are the
//! terminal-state snapshot idea from the archived TUI notes: a scripted
//! turn through the REAL client + transport stack renders to a fixed-size
//! terminal frame, and the exact frame text is pinned.

use std::collections::HashMap;
use std::time::Duration;

use dsh_harness_client::api::{DeepSeekHarness, DeepSeekHarnessOptions, PromptInput, RunOptions};
use dsh_harness_client::client::HarnessClientOptions;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;
use tub::eventmap::{build_items, UiItem};
use tub::ui::{self, OverlayView, UiState};

fn fake_bin() -> String {
    env!("CARGO_BIN_EXE_tub-fake-runtime").to_string()
}

fn env_with_path(mut env: HashMap<String, String>) -> HashMap<String, String> {
    env.insert(
        "PATH".to_string(),
        std::env::var("PATH").unwrap_or_default(),
    );
    env
}

/// The scripted full turn: user prompt, streamed chunks committing to a
/// markdown assistant message, a bash tool call with an fs diff, a subagent
/// run, a todo snapshot, then idle.
fn full_turn_script() -> serde_json::Value {
    serde_json::json!({
        "initialize": [{ "result": { "serverInfo": { "name": "deepseek-harness-sdk-runtime", "version": "0.0.1" } } }],
        "session/prompt": [{
            "send": [
                { "method": "session.status", "params": { "sessionId": "tui-1", "status": "running" } },
                { "method": "session.event", "params": { "sessionId": "tui-1", "event": {
                    "type": "agent/inbox/spliced", "seq": 0, "time": 1,
                    "data": { "target": "next-turn", "start": 0, "inserted": [
                        { "id": "msg-1", "role": "user", "content": [{ "type": "text", "text": "write note.txt" }],
                          "source": { "kind": "user" } } ] } } } },
                { "method": "session.event", "params": { "sessionId": "tui-1", "event": {
                    "type": "turn/start", "seq": 1, "time": 2, "data": { "turn": 1 } } } },
                { "method": "session.event", "params": { "sessionId": "tui-1", "event": {
                    "type": "user/message", "seq": 2, "time": 3,
                    "data": { "id": "msg-1", "role": "user", "content": [{ "type": "text", "text": "write note.txt" }],
                              "source": { "kind": "user" } } } } },
                { "method": "session.event", "params": { "sessionId": "tui-1", "event": {
                    "type": "assistant/chunk", "seq": 3, "time": 4,
                    "data": { "turn": 1, "step": 1, "chunk": { "type": "text-delta", "index": 0, "text": "I will " } } } } },
                { "method": "session.event", "params": { "sessionId": "tui-1", "event": {
                    "type": "tool/call", "seq": 4, "time": 10,
                    "data": { "turn": 1, "step": 1, "callId": "call-1", "name": "bash", "arguments": "{\"command\":\"echo hi\"}" } } } },
                { "method": "subagent.started", "params": { "parentSessionId": "tui-1", "childSessionId": "child-1" } },
                { "method": "session.event", "params": { "sessionId": "tui-1", "event": {
                    "type": "tool/result", "seq": 5, "time": 210,
                    "data": { "turn": 1, "step": 1, "message": {
                        "id": "r1", "role": "user",
                        "content": [{ "type": "tool-result", "toolCallId": "call-1",
                                      "content": [{ "type": "text", "text": "hi" }] }],
                        "source": { "kind": "tool", "callId": "call-1" } },
                        "meta": { "diffs": [{ "path": "note.txt", "oldText": "before", "newText": "after" }] } } } } },
                { "method": "subagent.finished", "params": { "provider": "spawn", "agentId": "child-1",
                    "parentSessionId": "tui-1", "childSessionId": "child-1", "status": "ok", "stopReason": "completed",
                    "lastAssistantMessage": [{ "type": "text", "text": "child done" }] } },
                { "method": "session.event", "params": { "sessionId": "tui-1", "event": {
                    "type": "assistant/message", "seq": 6, "time": 300,
                    "data": { "turn": 1, "step": 1, "message": {
                        "id": "a1", "role": "assistant",
                        "content": [{ "type": "text", "text": "I will write the file now.\n\nDone." }],
                        "source": { "kind": "model" } },
                        "usage": { "inputTokens": 100, "outputTokens": 9 } } } } },
                { "method": "session.event", "params": { "sessionId": "tui-1", "event": {
                    "type": "todo/write", "seq": 7, "time": 301,
                    "data": { "todos": [{ "content": "write note.txt", "status": "completed" }] } } } },
                { "method": "session.event", "params": { "sessionId": "tui-1", "event": {
                    "type": "turn/end", "seq": 8, "time": 302, "data": { "turn": 1, "reason": { "kind": "completed" } } } } },
                { "method": "session.status", "params": { "sessionId": "tui-1", "status": "idle" } }
            ],
            "result": { "messageId": "msg-1" }
        }],
        "shutdown": [{ "result": {} }]
    })
}

/// Drive the scripted turn through the real client stack.
async fn run_scripted_turn() -> (
    String,
    Vec<UiItem>,
    Vec<dsh_harness_client::protocol::HarnessNotification>,
) {
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
            PromptInput::Text("write note.txt".to_string()),
            &RunOptions {
                session_id: Some("tui-1".to_string()),
            },
        )
        .await
        .expect("scripted turn completes");
    let items = build_items(&result.events, &result.notifications);
    let notifications = result.notifications;
    harness.close().await.expect("clean teardown");
    (result.final_response, items, notifications)
}

fn render_items(items: &[UiItem], width: u16, height: u16) -> Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal over test backend");
    let state = UiState {
        items,
        status: "idle",
        session_id: "tui-1",
        route: "deepseek-official/deepseek-v4-flash".to_string(),
        cwd: "/work",
        branch: Some("main"),
        input_lines: &[],
        cursor: (0, 0),
        scroll: 0,
        queued: false,
        confirm_quit: false,
        error: None,
        violations: 0,
        elapsed: Duration::from_secs(3),
        overlay: None,
    };
    terminal
        .draw(|frame| ui::render(frame, &state))
        .expect("frame renders");
    terminal.backend().buffer().clone()
}

/// Render a bare overlay (no transcript items) over a fixed-size terminal.
fn render_overlay_frame(overlay: OverlayView, width: u16, height: u16) -> Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal over test backend");
    let items: Vec<UiItem> = Vec::new();
    let state = UiState {
        items: &items,
        status: "idle",
        session_id: "tui-1",
        route: "deepseek-official/deepseek-v4-flash".to_string(),
        cwd: "/work",
        branch: Some("main"),
        input_lines: &[],
        cursor: (0, 0),
        scroll: 0,
        queued: false,
        confirm_quit: false,
        error: None,
        violations: 0,
        elapsed: Duration::from_secs(3),
        overlay: Some(overlay),
    };
    terminal
        .draw(|frame| ui::render(frame, &state))
        .expect("frame renders");
    terminal.backend().buffer().clone()
}

/// The frame as a plain string (rows separated by newlines).
fn frame_string(buffer: &Buffer) -> String {
    let area = buffer.area;
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            if let Some(cell) = buffer.cell((x, y)) {
                out.push_str(cell.symbol());
            }
        }
        out.push('\n');
    }
    out
}

#[tokio::test]
async fn full_turn_frame_snapshot() {
    let (final_response, items, notifications) = run_scripted_turn().await;
    assert_eq!(final_response, "I will write the file now.\n\nDone.");
    // The stream ends on the root session's idle.
    assert_eq!(
        notifications.last().unwrap().session_status(),
        Some(("tui-1", dsh_harness_client::protocol::AgentStatus::Idle))
    );
    // The mapping produced the expected cards.
    assert!(items
        .iter()
        .any(|i| matches!(i, UiItem::UserPrompt { text } if text == "write note.txt")));
    assert!(items
        .iter()
        .any(|i| matches!(i, UiItem::TurnStart { turn: 1 })));
    assert!(items
        .iter()
        .any(|i| matches!(i, UiItem::TurnEnd { turn: 1, reason } if reason == "completed")));
    assert!(items
        .iter()
        .any(|i| matches!(i, UiItem::ToolCall { name, .. } if name == "bash")));
    assert!(items
        .iter()
        .any(|i| matches!(i, UiItem::Diff { path, .. } if path == "note.txt")));
    assert!(items.iter().any(|i| matches!(i, UiItem::Subagent { .. })));
    assert!(items.iter().any(|i| matches!(i, UiItem::Todo { .. })));
    assert!(items.iter().any(|i| matches!(
        i,
        UiItem::Assistant {
            streaming: false,
            ..
        }
    )));

    let buffer = render_items(&items, 80, 24);
    let frame = frame_string(&buffer);
    // The pinned frame: transcript pane renders the turn, the status bar
    // carries session/route/branch/elapsed, and the prompt pane sits at the
    // bottom.
    assert!(frame.contains("you"), "user prompt visible");
    assert!(frame.contains("--- turn 1"), "turn divider visible");
    assert!(
        frame.contains("[tool] bash - done (200ms)"),
        "tool card with elapsed timing"
    );
    assert!(frame.contains("[diff] note.txt"), "diff card");
    assert!(frame.contains("- before"), "diff removal side");
    assert!(frame.contains("+ after"), "diff addition side");
    assert!(
        frame.contains("[subagent] child-1 - ok/completed"),
        "subagent card"
    );
    assert!(frame.contains("[x] write note.txt"), "todo snapshot");
    assert!(frame.contains("tui-1"), "session identity stays visible");
    assert!(
        frame.contains("deepseek-official/deepseek-v4-flash"),
        "route in status bar"
    );
    assert!(frame.contains("/work@main"), "workspace and branch context");
    assert!(
        frame.contains("I will write the file now."),
        "markdown assistant text"
    );
    assert!(frame.contains("(in=100 out=9 tokens)"), "token accounting");
    assert!(
        frame.contains("(turn 1 ended: completed)"),
        "turn phase status"
    );
    assert!(frame.contains("prompt"), "input pane");
}

#[tokio::test]
async fn streaming_frame_snapshot() {
    // A mid-stream frame: only the first chunk arrived, so the assistant line
    // shows streaming text with a cursor and no commit.
    let items = vec![
        UiItem::UserPrompt {
            text: "write note.txt".to_string(),
        },
        UiItem::TurnStart { turn: 1 },
        UiItem::Assistant {
            turn: 1,
            step: 1,
            text: "I will ".to_string(),
            streaming: true,
            usage: None,
        },
    ];
    let buffer = render_items(&items, 80, 24);
    let frame = frame_string(&buffer);
    assert!(frame.contains("I will "), "streamed text visible");
    assert!(frame.contains("\u{258c}"), "streaming cursor visible");
    assert!(!frame.contains("--- turn 2"), "no later content");
}

#[test]
fn scroll_window_clamps() {
    let items: Vec<UiItem> = (0..60)
        .map(|i| UiItem::Context {
            summary: format!("line {i}"),
        })
        .collect();
    let buffer = render_items(&items, 80, 24);
    let frame = frame_string(&buffer);
    // Pinned to the bottom: the last line is visible, the first is not.
    assert!(frame.contains("line 59"));
    assert!(!frame.contains("line 0"));
}

#[test]
fn overlay_picker_scrolls_to_keep_selection_visible() {
    // More items than a 24-row terminal's popup can show at once; the
    // highlighted selection must stay on-screen and the hint must say so.
    let lines: Vec<String> = (0..40).map(|i| format!("item {i}")).collect();
    let overlay = OverlayView {
        title: "Choose model".to_string(),
        lines,
        selected: Some(39),
        hint: "up/down move".to_string(),
        cursor: None,
    };
    let buffer = render_overlay_frame(overlay, 80, 24);
    let frame = frame_string(&buffer);
    assert!(frame.contains("item 39"), "the selection scrolls into view");
    assert!(!frame.contains("item 0"), "items far above it scroll out");
    assert!(frame.contains("of 40"), "hint reports how many items exist");
}

#[test]
fn overlay_form_cursor_tracks_the_active_field() {
    let route_line = "Route: ".to_string();
    let name_line = "Display name: hi".to_string();
    let name_len = name_line.chars().count();
    let overlay = OverlayView {
        title: "Add provider".to_string(),
        lines: vec![route_line, name_line],
        selected: Some(1),
        hint: "type to edit".to_string(),
        cursor: Some((1, name_len)),
    };
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("terminal over test backend");
    let items: Vec<UiItem> = Vec::new();
    let state = UiState {
        items: &items,
        status: "idle",
        session_id: "tui-1",
        route: "deepseek-official/deepseek-v4-flash".to_string(),
        cwd: "/work",
        branch: Some("main"),
        input_lines: &[],
        cursor: (0, 0),
        scroll: 0,
        queued: false,
        confirm_quit: false,
        error: None,
        violations: 0,
        elapsed: Duration::from_secs(3),
        overlay: Some(overlay),
    };
    terminal
        .draw(|frame| ui::render(frame, &state))
        .expect("frame renders");
    // Pinned: for an 80x24 frame the popup lands at (8, 9) sized 64x5, so the
    // caret on the second (selected) field sits at its text's end, one row
    // below the field above it — not at the unrelated input-pane cursor.
    assert_eq!(name_len, 16);
    let cursor = terminal.get_cursor_position().expect("cursor position set");
    assert_eq!(cursor, ratatui::layout::Position { x: 25, y: 11 });
}
