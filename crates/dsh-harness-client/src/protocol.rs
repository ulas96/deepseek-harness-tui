//! Named wire types for the DeepSeek Harness SDK runtime protocol: lifecycle,
//! prompt, model/provider management, resume, and notification payloads
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "reasoningEffort"
    )]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "maxTokens")]
    pub max_tokens: Option<u64>,
}

/// Wire-stable server identity returned by initialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    #[serde(rename = "serverInfo")]
    pub server_info: ServerInfo,
    /// Optional methods advertised by runtimes that implement theus's extended
    /// interactive-management protocol. An absent list means a legacy runtime.
    #[serde(default)]
    pub capabilities: Vec<String>,
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

/// One selectable provider route and its currently registered model catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCatalogEntry {
    pub provider: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    /// Whether an adapter for the route is active now. Dormant catalog routes
    /// can be activated through provider onboarding.
    pub active: bool,
    #[serde(default)]
    pub declared: bool,
    #[serde(default)]
    pub models: Vec<ModelCatalogEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One model exposed by a provider adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ModelReasoningInfo>,
}

/// Selectable reasoning effort metadata for one exact model route.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelReasoningInfo {
    #[serde(default)]
    pub efforts: Vec<ReasoningEffortInfo>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "defaultEffort"
    )]
    pub default_effort: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningEffortInfo {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCatalogResult {
    pub providers: Vec<ProviderCatalogEntry>,
}

/// Session-local route applied to the next model step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSelection {
    pub provider: String,
    pub model: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "reasoningEffort"
    )]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectModelResult {
    pub selected: ModelSelection,
}

/// Lightweight persisted conversation metadata scoped to the initialized cwd.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListEntry {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: u64,
    #[serde(rename = "lastActivityAt")]
    pub last_activity_at: u64,
    pub live: bool,
    #[serde(default)]
    pub unreadable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListResult {
    pub sessions: Vec<SessionListEntry>,
}

/// Complete history and routing state returned when a conversation is adopted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionResumeResult {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub events: Vec<SessionEvent>,
    pub selection: ModelSelection,
    pub status: AgentStatus,
    pub routable: bool,
}

/// Draft provider profile used by discovery and durable add operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderDraft {
    pub provider: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "displayName"
    )]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "baseURL")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "credentialRef"
    )]
    pub credential_ref: Option<String>,
    /// A one-shot secret. Servers must never echo or log this field.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "credentialValue"
    )]
    pub credential_value: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "modelIds")]
    pub model_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "contextWindow"
    )]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "maxTokens")]
    pub max_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderDiscoverResult {
    pub models: Vec<DiscoveredModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderAddResult {
    pub provider: String,
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
