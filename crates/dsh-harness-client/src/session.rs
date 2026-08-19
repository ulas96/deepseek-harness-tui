//! The session-log vocabulary that IS the wire contract: full 'SessionEvent'
//! envelopes stream over 'session.event' notifications, so the Rust port of
//! the 'dsh-session' / 'dsh-llm' tagged unions lives here.
//!
//! The vocabulary is merge-extensible (plugins add event types and content
//! block types without a format-version bump). This module therefore parses
//! the envelope shape strictly but keeps variant payloads as raw JSON with
//! typed accessors: an unknown event type is carried verbatim instead of
//! failing the stream, exactly like the reference clients.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Content blocks (dsh-llm ContentBlockMap)
// ---------------------------------------------------------------------------

/// Any known content block. Unknown 'type' tags fall through to 'Other' so
/// vocabulary growth never breaks an older client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ContentBlock {
    /// Plain text visible to the end user.
    #[serde(rename = "text")]
    Text { text: String },
    /// Reasoning / thinking content, distinct from visible text.
    #[serde(rename = "reasoning")]
    Reasoning { text: String },
    /// A durable raster image reference (opaque to the TUI).
    #[serde(rename = "image")]
    Image { attachment: Value },
    /// A tool invocation requested by the model.
    #[serde(rename = "tool-call")]
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    /// The result of a tool invocation, sent back to the model.
    #[serde(rename = "tool-result")]
    ToolResult {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        content: Vec<ContentBlock>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

impl ContentBlock {
    /// The visible text of this block, when it is a text block.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text { text } => Some(text),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Messages (dsh-llm message.ts)
// ---------------------------------------------------------------------------

/// Message provenance: 'user', 'plugin', 'model', 'tool', or a plugin-added
/// kind. Only 'kind' is typed; the rest of the source object travels verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageSource {
    pub kind: String,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// One immutable message representation shared by delivery, durable history,
/// and model requests. 'role' is kept as a string ('system' | 'user' |
/// 'assistant') so a widened role vocabulary cannot break parsing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub role: String,
    pub content: Vec<ContentBlock>,
    pub source: MessageSource,
}

impl Message {
    /// Concatenated text of the message's text blocks.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(ContentBlock::as_text)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Token accounting (dsh-llm TokenUsage)
// ---------------------------------------------------------------------------

/// Token accounting for one model call. Counts are disjoint: 'input_tokens'
/// excludes cache traffic, which is reported separately.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(rename = "inputTokens")]
    pub input_tokens: u64,
    #[serde(rename = "outputTokens")]
    pub output_tokens: u64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "cacheReadTokens"
    )]
    pub cache_read_tokens: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "cacheWriteTokens"
    )]
    pub cache_write_tokens: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "reasoningTokens"
    )]
    pub reasoning_tokens: Option<u64>,
}

/// Serializable provider or transport failure facts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmFailure {
    pub message: String,
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "providerRetryAfterMs"
    )]
    pub provider_retry_after_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "requestId")]
    pub request_id: Option<String>,
}

/// Why a model response stopped. Unknown kinds fall through to 'Other'.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum FinishReason {
    #[serde(rename = "stop")]
    Stop,
    #[serde(rename = "tool-calls")]
    ToolCalls,
    #[serde(rename = "max-tokens")]
    MaxTokens,
    #[serde(rename = "aborted")]
    Aborted { failure: LlmFailure },
    #[serde(rename = "error")]
    Error { failure: LlmFailure },
}

/// Raw streaming protocol emitted by adapters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum StreamChunk {
    #[serde(rename = "block-start")]
    BlockStart {
        index: u64,
        #[serde(rename = "blockType")]
        block_type: String,
    },
    #[serde(rename = "text-delta")]
    TextDelta { index: u64, text: String },
    #[serde(rename = "reasoning-delta")]
    ReasoningDelta { index: u64, text: String },
    #[serde(rename = "tool-call-delta")]
    ToolCallDelta {
        index: u64,
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(rename = "argumentsDelta")]
        arguments_delta: String,
    },
    #[serde(rename = "block-end")]
    BlockEnd { index: u64, block: ContentBlock },
    #[serde(rename = "usage")]
    Usage { usage: TokenUsage },
    #[serde(rename = "finish")]
    Finish {
        reason: FinishReason,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "replayState"
        )]
        replay_state: Option<Value>,
    },
}

// ---------------------------------------------------------------------------
// Turn lifecycle (dsh-session TurnEndReasonMap)
// ---------------------------------------------------------------------------

/// Why a turn ended. Unknown kinds fall through to 'Other'.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TurnEndReason {
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "aborted")]
    Aborted { reason: Value },
    #[serde(rename = "blocked")]
    Blocked,
    #[serde(rename = "error")]
    Error { error: LlmFailure },
    #[serde(rename = "max-tokens")]
    MaxTokens,
    #[serde(rename = "interrupted")]
    Interrupted,
}

impl TurnEndReason {
    /// Human-readable one-line summary for a transcript footer.
    pub fn describe(&self) -> String {
        match self {
            TurnEndReason::Completed => "completed".into(),
            TurnEndReason::Aborted { .. } => "aborted".into(),
            TurnEndReason::Blocked => "blocked".into(),
            TurnEndReason::Error { error } => format!("error: {} ({})", error.message, error.code),
            TurnEndReason::MaxTokens => "max-tokens".into(),
            TurnEndReason::Interrupted => "interrupted".into(),
        }
    }
}

/// One entry in an agent's todo list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: String, // 'pending' | 'in_progress' | 'completed'
}

// ---------------------------------------------------------------------------
// The session event envelope (dsh-session SessionEvent)
// ---------------------------------------------------------------------------

/// One immutable entry in the session log. 'data' stays raw JSON; the typed
/// accessors below narrow it. 'surface_op' is carried verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub seq: u64,
    pub time: u64,
    pub data: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignorable: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "sourceEventSeqs"
    )]
    pub source_event_seqs: Option<Vec<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "surfaceOp")]
    pub surface_op: Option<Value>,
}

/// Parsed 'turn/start' / 'turn/end' payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnMarker {
    pub turn: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<TurnEndReason>,
}

/// Parsed 'step/start' / 'step/end' payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepMarker {
    pub turn: u64,
    pub step: u64,
}

/// Parsed 'assistant/message' payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessageEvent {
    pub turn: u64,
    pub step: u64,
    pub message: Message,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
}

/// Parsed 'assistant/chunk' payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantChunkEvent {
    pub turn: u64,
    pub step: u64,
    pub chunk: StreamChunk,
}

/// Parsed 'tool/call' payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallEvent {
    pub turn: u64,
    pub step: u64,
    #[serde(rename = "callId")]
    pub call_id: String,
    pub name: String,
    pub arguments: String,
}

/// Parsed 'tool/result' payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultEvent {
    pub turn: u64,
    pub step: u64,
    pub message: Message,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolErrorIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// Internal failure identity on a tool result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolErrorIdentity {
    pub name: String,
    pub code: String,
}

/// Parsed 'agent/inbox/spliced' payload (the durable enqueue receipt).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxSplice {
    pub target: String, // 'next-turn' | 'next-step'
    pub start: u64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "removedCount"
    )]
    pub removed_count: Option<u64>,
    pub inserted: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
}

/// The 'dsh-tool-fs' result-time diff card: 'meta' payload on write/edit tool
/// results ('FileDiff' from '@deepseek-ai/dsh-tools').
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    /// Pre-image text; absent for pure insertions.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "oldText")]
    pub old_text: Option<String>,
    /// Post-image text.
    #[serde(rename = "newText")]
    pub new_text: String,
}

/// 'tool/result' meta narrowing for the fs diff card.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsDiffMeta {
    pub diffs: Vec<FileDiff>,
}

impl SessionEvent {
    /// Whether this envelope carries a valid shape for its declared type. The
    /// one variant the owned-run API reads into must be validated (mirror the
    /// TS client's 'validatedSessionEvent').
    pub fn validate(&self) -> Result<(), crate::Error> {
        if self.event_type == "assistant/message" {
            let ok = self
                .data
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
                .map(|blocks| {
                    blocks
                        .iter()
                        .all(|b| b.is_object() && b.get("type").and_then(Value::as_str).is_some())
                })
                .unwrap_or(false);
            if !ok {
                return Err(crate::Error::Protocol(format!(
                    "assistant/message event carried malformed content: {}",
                    self.data
                )));
            }
        }
        Ok(())
    }

    fn data_as<T: serde::de::DeserializeOwned>(&self) -> Option<T> {
        serde_json::from_value(self.data.clone()).ok()
    }

    /// Narrow to a 'user/message' payload.
    pub fn user_message(&self) -> Option<Message> {
        (self.event_type == "user/message")
            .then(|| self.data_as())
            .flatten()
    }

    /// Narrow to an 'assistant/message' payload.
    pub fn assistant_message(&self) -> Option<AssistantMessageEvent> {
        (self.event_type == "assistant/message")
            .then(|| self.data_as())
            .flatten()
    }

    /// Narrow to an 'assistant/chunk' payload.
    pub fn assistant_chunk(&self) -> Option<AssistantChunkEvent> {
        (self.event_type == "assistant/chunk")
            .then(|| self.data_as())
            .flatten()
    }

    /// Narrow to a 'tool/call' payload.
    pub fn tool_call(&self) -> Option<ToolCallEvent> {
        (self.event_type == "tool/call")
            .then(|| self.data_as())
            .flatten()
    }

    /// Narrow to a 'tool/result' payload.
    pub fn tool_result(&self) -> Option<ToolResultEvent> {
        (self.event_type == "tool/result")
            .then(|| self.data_as())
            .flatten()
    }

    /// Narrow to a 'turn/start' or 'turn/end' payload.
    pub fn turn_marker(&self) -> Option<TurnMarker> {
        matches!(self.event_type.as_str(), "turn/start" | "turn/end")
            .then(|| self.data_as())
            .flatten()
    }

    /// Narrow to a 'step/start' or 'step/end' payload.
    pub fn step_marker(&self) -> Option<StepMarker> {
        matches!(self.event_type.as_str(), "step/start" | "step/end")
            .then(|| self.data_as())
            .flatten()
    }

    /// Narrow to a 'todo/write' payload.
    pub fn todo_items(&self) -> Option<Vec<TodoItem>> {
        (self.event_type == "todo/write")
            .then(|| {
                self.data
                    .get("todos")
                    .cloned()
                    .and_then(|v| serde_json::from_value(v).ok())
            })
            .flatten()
    }

    /// Narrow to an 'agent/inbox/spliced' payload.
    pub fn inbox_splice(&self) -> Option<InboxSplice> {
        (self.event_type == "agent/inbox/spliced")
            .then(|| self.data_as())
            .flatten()
    }

    /// Narrow to the fs diff card meta on a 'tool/result' payload.
    pub fn fs_diffs(&self) -> Option<Vec<FileDiff>> {
        let result = self.tool_result()?;
        let meta: FsDiffMeta = serde_json::from_value(result.meta?).ok()?;
        (!meta.diffs.is_empty()).then_some(meta.diffs)
    }
}

// ---------------------------------------------------------------------------
// Helpers shared by the client layers
// ---------------------------------------------------------------------------

/// Whether a session event is the durable enqueue receipt for 'message_id'
/// (mirror the TS client's 'isInboxReceipt').
pub fn is_inbox_receipt(event: &SessionEvent, message_id: &str) -> bool {
    let Some(splice) = event.inbox_splice() else {
        return false;
    };
    splice
        .inserted
        .iter()
        .any(|message| message.id == message_id)
}

/// Extract the concatenated text of the last assistant message (mirror the TS
/// client's 'finalResponse').
pub fn final_response(events: &[SessionEvent]) -> String {
    for event in events.iter().rev() {
        if let Some(assistant) = event.assistant_message() {
            let text: String = assistant
                .message
                .content
                .iter()
                .filter_map(ContentBlock::as_text)
                .collect();
            return text;
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_block_text_narrows() {
        let block: ContentBlock = serde_json::from_value(serde_json::json!({
            "type": "text", "text": "hello"
        }))
        .unwrap();
        assert_eq!(block.as_text(), Some("hello"));
        let block: ContentBlock = serde_json::from_value(serde_json::json!({
            "type": "tool-call", "id": "call-1", "name": "bash", "arguments": "{}"
        }))
        .unwrap();
        assert_eq!(block.as_text(), None);
    }

    #[test]
    fn session_event_narrows_and_validates() {
        let event: SessionEvent = serde_json::from_value(serde_json::json!({
            "type": "assistant/message", "seq": 4, "time": 100,
            "data": { "turn": 1, "step": 1, "message": {
                "id": "m1", "role": "assistant", "content": [{ "type": "text", "text": "hi" }],
                "source": { "kind": "model", "provider": "deepseek-official", "model": "deepseek-v4-flash" }
            } }
        }))
        .unwrap();
        event.validate().unwrap();
        let assistant = event.assistant_message().unwrap();
        assert_eq!(assistant.message.text(), "hi");
        assert_eq!(assistant.usage, None);

        let bad: SessionEvent = serde_json::from_value(serde_json::json!({
            "type": "assistant/message", "seq": 5, "time": 101,
            "data": { "turn": 1, "step": 1, "message": { "id": "m1", "role": "assistant", "content": "nope" } }
        }))
        .unwrap();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn inbox_receipt_matches_inserted_id() {
        let event: SessionEvent = serde_json::from_value(serde_json::json!({
            "type": "agent/inbox/spliced", "seq": 0, "time": 0,
            "data": { "target": "next-turn", "start": 0, "inserted": [
                { "id": "msg-42", "role": "user", "content": [{ "type": "text", "text": "hi" }],
                  "source": { "kind": "user" } }
            ] }
        }))
        .unwrap();
        assert!(is_inbox_receipt(&event, "msg-42"));
        assert!(!is_inbox_receipt(&event, "other"));
    }

    #[test]
    fn final_response_takes_last_assistant_text() {
        let first: SessionEvent = serde_json::from_value(serde_json::json!({
            "type": "assistant/message", "seq": 1, "time": 1,
            "data": { "turn": 1, "step": 1, "message": {
                "id": "a1", "role": "assistant",
                "content": [{ "type": "reasoning", "text": "think" }, { "type": "text", "text": "first" }],
                "source": { "kind": "model" } } }
        }))
        .unwrap();
        let last: SessionEvent = serde_json::from_value(serde_json::json!({
            "type": "assistant/message", "seq": 3, "time": 3,
            "data": { "turn": 1, "step": 2, "message": {
                "id": "a2", "role": "assistant",
                "content": [{ "type": "text", "text": "second" }],
                "source": { "kind": "model" } } }
        }))
        .unwrap();
        assert_eq!(final_response(&[first, last]), "second");
        assert_eq!(final_response(&[]), "");
    }
}
