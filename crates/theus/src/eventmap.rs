//! Pure event -> UI mapping: every transcript item is a function of the
//! session event stream (plus the subagent notifications). No UI state is
//! hidden in the mapping - the same fold builds initial transcripts and
//! incremental updates, so tests can snapshot it deterministically.

use dsh_harness_client::protocol::HarnessNotification;
use dsh_harness_client::session::{ContentBlock, SessionEvent, TodoItem, TokenUsage};

/// One renderable transcript entry.
#[derive(Debug, Clone, PartialEq)]
pub enum UiItem {
    /// A direct human prompt.
    UserPrompt { text: String },
    /// Producer-supplied context (plugin injections, tool echoes): a
    /// collapsed one-line notice.
    Context { summary: String },
    /// 'turn N' divider.
    TurnStart { turn: u64 },
    /// The turn's terminal reason.
    TurnEnd { turn: u64, reason: String },
    /// Assistant output: streamed deltas (streaming) or the committed
    /// message (markdown-renderable).
    Assistant {
        turn: u64,
        step: u64,
        text: String,
        streaming: bool,
        usage: Option<TokenUsage>,
    },
    /// One model-requested tool invocation.
    ToolCall {
        call_id: String,
        name: String,
        arguments: String,
        started_at: u64,
        state: ToolState,
    },
    /// An fs diff card attached to a write/edit tool result.
    Diff {
        path: String,
        old: Option<String>,
        new: String,
    },
    /// An in-runtime child session (from subagent.started/finished).
    Subagent { child: String, state: SubagentState },
    /// The latest whole-list todo snapshot.
    Todo { items: Vec<TodoItem> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolState {
    Running,
    Completed { elapsed_ms: u64, is_error: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub enum SubagentState {
    Running,
    Finished {
        status: String,
        stop_reason: String,
        answer: String,
    },
}

/// One-line summary bound for context notices (the harness's own bound).
const CONTEXT_SUMMARY_MAX_CHARS: usize = 120;

fn bound_summary(text: &str) -> String {
    let first_line = text.lines().next().unwrap_or("");
    if first_line.chars().count() <= CONTEXT_SUMMARY_MAX_CHARS {
        first_line.to_string()
    } else {
        let cut: String = first_line
            .chars()
            .take(CONTEXT_SUMMARY_MAX_CHARS - 1)
            .collect();
        format!("{cut}...")
    }
}

/// Concatenated text of content blocks.
pub fn blocks_text(blocks: &[ContentBlock]) -> String {
    blocks.iter().filter_map(ContentBlock::as_text).collect()
}

/// Apply one session event to the transcript (the incremental step of the
/// pure fold).
pub fn apply_event(items: &mut Vec<UiItem>, event: &SessionEvent) {
    match event.event_type.as_str() {
        "user/message" => {
            if let Some(message) = event.user_message() {
                let text = blocks_text(&message.content);
                if text.is_empty() {
                    return;
                }
                match message.source.kind.as_str() {
                    "user" => items.push(UiItem::UserPrompt { text }),
                    _ => items.push(UiItem::Context {
                        summary: bound_summary(&text),
                    }),
                }
            }
        }
        "turn/start" => {
            if let Some(marker) = event.turn_marker() {
                items.push(UiItem::TurnStart { turn: marker.turn });
            }
        }
        "turn/end" => {
            if let Some(marker) = event.turn_marker() {
                let reason = marker.reason.map(|r| r.describe()).unwrap_or_default();
                items.push(UiItem::TurnEnd {
                    turn: marker.turn,
                    reason,
                });
            }
        }
        "assistant/chunk" => {
            let Some(chunk_event) = event.assistant_chunk() else {
                return;
            };
            let delta = match &chunk_event.chunk {
                dsh_harness_client::session::StreamChunk::TextDelta { text, .. } => text.clone(),
                _ => String::new(),
            };
            if delta.is_empty() {
                return;
            }
            // A committed message for this step supersedes trailing chunks.
            if items.iter().any(|item| matches!(item, UiItem::Assistant { turn, step, streaming: false, .. } if *turn == chunk_event.turn && *step == chunk_event.step)) {
                return;
            }
            match items.iter_mut().rev().find(|item| {
                matches!(item, UiItem::Assistant { turn, step, streaming: true, .. } if *turn == chunk_event.turn && *step == chunk_event.step)
            }) {
                Some(UiItem::Assistant { text, .. }) => text.push_str(&delta),
                Some(_) => unreachable!(),
                None => items.push(UiItem::Assistant {
                    turn: chunk_event.turn,
                    step: chunk_event.step,
                    text: delta,
                    streaming: true,
                    usage: None,
                }),
            }
        }
        "assistant/message" => {
            let Some(assistant) = event.assistant_message() else {
                return;
            };
            let text = blocks_text(&assistant.message.content);
            let committed = UiItem::Assistant {
                turn: assistant.turn,
                step: assistant.step,
                text,
                streaming: false,
                usage: assistant.usage,
            };
            match items.iter_mut().rev().find(|item| {
                matches!(item, UiItem::Assistant { turn, step, streaming: true, .. } if *turn == assistant.turn && *step == assistant.step)
            }) {
                Some(slot) => *slot = committed,
                None => items.push(committed),
            }
        }
        "tool/call" => {
            if let Some(call) = event.tool_call() {
                items.push(UiItem::ToolCall {
                    call_id: call.call_id,
                    name: call.name,
                    arguments: call.arguments,
                    started_at: event.time,
                    state: ToolState::Running,
                });
            }
        }
        "tool/result" => {
            let Some(result) = event.tool_result() else {
                return;
            };
            let call_id = result
                .message
                .content
                .first()
                .and_then(|block| match block {
                    ContentBlock::ToolResult { tool_call_id, .. } => Some(tool_call_id.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            if let Some(slot) = items.iter_mut().rev().find(|item| {
                matches!(item, UiItem::ToolCall { call_id: id, state: ToolState::Running, .. } if *id == call_id)
            }) {
                if let UiItem::ToolCall { state, started_at, .. } = slot {
                    let elapsed_ms = event.time.saturating_sub(*started_at);
                    *state = ToolState::Completed {
                        elapsed_ms,
                        is_error: result.error.is_some(),
                    };
                }
            }
            if let Some(diffs) = event.fs_diffs() {
                for diff in diffs {
                    items.push(UiItem::Diff {
                        path: diff.path,
                        old: diff.old_text,
                        new: diff.new_text,
                    });
                }
            }
        }
        "todo/write" => {
            if let Some(todos) = event.todo_items() {
                match items
                    .iter_mut()
                    .rev()
                    .find(|item| matches!(item, UiItem::Todo { .. }))
                {
                    Some(UiItem::Todo { items: existing }) => *existing = todos,
                    Some(_) => unreachable!(),
                    None => items.push(UiItem::Todo { items: todos }),
                }
            }
        }
        _ => {}
    }
}

/// Apply one non-event notification (subagent lifecycle) to the transcript.
pub fn apply_notification(items: &mut Vec<UiItem>, notification: &HarnessNotification) {
    match notification.method.as_str() {
        "subagent.started" => {
            let Some((_parent, child)) = notification.subagent_started() else {
                return;
            };
            items.push(UiItem::Subagent {
                child,
                state: SubagentState::Running,
            });
        }
        "subagent.finished" => {
            let Some(finished) = notification.subagent_finished() else {
                return;
            };
            let answer = finished
                .last_assistant_message
                .as_ref()
                .map(|blocks| blocks_text(blocks))
                .unwrap_or_default();
            let status = match finished.status {
                dsh_harness_client::protocol::SdkRunStatus::Ok => "ok",
                dsh_harness_client::protocol::SdkRunStatus::Error => "error",
            };
            if let Some(slot) = items.iter_mut().rev().find(|item| {
                matches!(item, UiItem::Subagent { child, state: SubagentState::Running } if *child == finished.child_session_id)
            }) {
                if let UiItem::Subagent { state, .. } = slot {
                    *state = SubagentState::Finished {
                        status: status.to_string(),
                        stop_reason: finished.stop_reason,
                        answer,
                    };
                }
            }
        }
        _ => {}
    }
}

/// The pure fold over a complete interval: events in wire order, then the
/// subagent notifications (their interleaving with events is not
/// reconstructible from the payloads alone; subagents render after the turn
/// items they arrived during).
pub fn build_items(events: &[SessionEvent], notifications: &[HarnessNotification]) -> Vec<UiItem> {
    let mut items = Vec::new();
    for event in events {
        apply_event(&mut items, event);
    }
    for notification in notifications {
        apply_notification(&mut items, notification);
    }
    items
}
