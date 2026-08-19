//! Named wire types for the DeepSeek Harness SDK runtime protocol: the three
//! request/result pairs and the four server-to-client notification payloads
//! exchanged over the newline-delimited JSON-RPC stdio transport (the Rust
//! port of '@deepseek-ai/dsh-sdk-protocol/types').

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session::{ContentBlock, SessionEvent};

// ---------------------------------------------------------------------------
// Client -> server requests
// ---------------------------------------------------------------------------

/// Parameters for the process-wide SDK handshake. 'cwd' must be ABSOLUTE —
/// resolve it client-side before it crosses the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeParams {
    pub cwd: String,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "maxTokens")]
    pub max_tokens: Option<u64>,
}

/// Wire-stable server identity returned by initialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    #[serde(rename = "serverInfo")]
    pub server_info: ServerInfo,
}

/// 'serverInfo.name' is the wire-stable 'deepseek-harness-sdk-runtime'.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

/// One user turn on one SDK session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPromptParams {
    /// The SDK-side session id; an unknown id lazily creates the agent+session.
    #[serde(rename = "sessionId")]
    pub session_id: String,
    /// The prompt content blocks, sent verbatim as the user message.
    #[serde(rename = "contentBlocks")]
    pub content_blocks: Vec<ContentBlock>,
}

/// Durable enqueue receipt for one prompt: the identity of the queued user
/// message only — not an assistant message, turn end, or prompt result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPromptResult {
    #[serde(rename = "messageId")]
    pub message_id: String,
}

// ---------------------------------------------------------------------------
// Server -> client notifications
// ---------------------------------------------------------------------------

/// 'session.event' payload: one session-log event, streamed as recorded, for
/// EVERY session in the runtime (unfiltered — scope client-side).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEventNotification {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    /// The full session-log event envelope.
    pub event: SessionEvent,
}

/// Whole-agent lifecycle state for one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Idle,
    Running,
}

impl AgentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentStatus::Idle => "idle",
            AgentStatus::Running => "running",
        }
    }
}

/// 'session.status' payload: the whole-agent state after a transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStatusNotification {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub status: AgentStatus,
}

/// 'subagent.started' payload: an in-runtime child session was created.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentStartedNotification {
    #[serde(rename = "parentSessionId")]
    pub parent_session_id: String,
    #[serde(rename = "childSessionId")]
    pub child_session_id: String,
}

/// Deployment-mapped SDK outcome: 'ok' for an accepted result, 'error'
/// otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SdkRunStatus {
    Ok,
    Error,
}

/// 'subagent.finished' payload: an in-process subagent run ended (remote runs
/// are not reported). 'stop_reason' stays a string — the vocabulary is
/// merge-extensible ('completed' | 'aborted' | 'error' | 'max-tokens' |
/// 'refusal' today).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentFinishedNotification {
    pub provider: String,
    #[serde(rename = "agentId")]
    pub agent_id: String,
    #[serde(rename = "parentSessionId")]
    pub parent_session_id: String,
    #[serde(rename = "childSessionId")]
    pub child_session_id: String,
    pub status: SdkRunStatus,
    #[serde(rename = "stopReason")]
    pub stop_reason: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "lastAssistantMessage"
    )]
    pub last_assistant_message: Option<Vec<ContentBlock>>,
}

// ---------------------------------------------------------------------------
// Raw notification envelope
// ---------------------------------------------------------------------------

/// One server-to-client notification as received off the wire: the method name
/// plus raw params. Typed accessors narrow per method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessNotification {
    pub method: String,
    pub params: Value,
}

impl HarnessNotification {
    /// The session an event or status notification belongs to, when applicable.
    pub fn session_id(&self) -> Option<&str> {
        self.params.get("sessionId").and_then(Value::as_str)
    }

    /// The delegating session of a subagent notification, when applicable.
    pub fn parent_session_id(&self) -> Option<&str> {
        self.params.get("parentSessionId").and_then(Value::as_str)
    }

    /// The child session of a subagent notification, when applicable.
    pub fn child_session_id(&self) -> Option<&str> {
        self.params.get("childSessionId").and_then(Value::as_str)
    }

    /// Narrow to a 'session.event' payload.
    pub fn session_event(&self) -> Option<SessionEvent> {
        (self.method == "session.event")
            .then(|| self.params.get("event"))
            .flatten()
            .and_then(|event| serde_json::from_value(event.clone()).ok())
    }

    /// Narrow to a 'session.status' payload.
    pub fn session_status(&self) -> Option<(&str, AgentStatus)> {
        if self.method != "session.status" {
            return None;
        }
        let status = match self.params.get("status")?.as_str()? {
            "idle" => AgentStatus::Idle,
            "running" => AgentStatus::Running,
            _ => return None,
        };
        Some((self.session_id()?, status))
    }

    /// Narrow to a 'subagent.started' payload.
    pub fn subagent_started(&self) -> Option<(String, String)> {
        if self.method != "subagent.started" {
            return None;
        }
        Some((
            self.parent_session_id()?.to_string(),
            self.child_session_id()?.to_string(),
        ))
    }

    /// Narrow to a 'subagent.finished' payload.
    pub fn subagent_finished(&self) -> Option<SubagentFinishedNotification> {
        (self.method == "subagent.finished")
            .then(|| serde_json::from_value(self.params.clone()).ok())
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_narrowing() {
        let n: HarnessNotification = serde_json::from_value(serde_json::json!({
            "method": "session.status",
            "params": { "sessionId": "s1", "status": "running" }
        }))
        .unwrap();
        assert_eq!(n.session_status(), Some(("s1", AgentStatus::Running)));
        assert!(n.session_event().is_none());

        let n: HarnessNotification = serde_json::from_value(serde_json::json!({
            "method": "subagent.started",
            "params": { "parentSessionId": "s1", "childSessionId": "s2" }
        }))
        .unwrap();
        assert_eq!(
            n.subagent_started(),
            Some(("s1".to_string(), "s2".to_string()))
        );
    }
}
