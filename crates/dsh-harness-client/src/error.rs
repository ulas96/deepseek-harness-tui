//! Typed error surface for the SDK client, mirroring the TypeScript
//! '@deepseek-ai/dsh-sdk-client' error taxonomy: wire error responses preserve
//! their 'code' and optional 'data'; transport loss carries the runtime's exit
//! code and a bounded stderr tail; protocol violations and request timeouts are
//! distinct variants so callers can branch without string matching.

use std::fmt;

use serde_json::Value;

/// A JSON-RPC error response from the runtime. The wire 'code' and optional
/// 'data' are preserved verbatim (mirror JsonRpcResponseError).
#[derive(Debug, Clone)]
pub struct WireError {
    /// Wire error code (e.g. -32601 method not found, -32603 handler failure).
    pub code: i64,
    /// Human-readable error message from the runtime.
    pub message: String,
    /// Optional structured error data from the runtime.
    pub data: Option<Value>,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "runtime error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for WireError {}

/// The runtime subprocess is gone or unusable: it exited, its stdio closed, or
/// it was never launchable. Carries the exit code and a bounded stderr tail
/// when available (mirror TransportClosedError).
#[derive(Debug, Clone)]
pub struct TransportClosedError {
    /// Human-readable reason the transport is closed.
    pub reason: String,
    /// Process spawn failure message, when the child never started.
    pub spawn_error: Option<String>,
    /// Process exit code, when the child exited.
    pub exit_code: Option<i32>,
    /// Bounded tail of the child's stderr, for diagnostics.
    pub stderr_tail: Vec<String>,
}

impl fmt::Display for TransportClosedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.reason)?;
        if let Some(spawn) = &self.spawn_error {
            write!(f, "\nspawn error: {spawn}")?;
        }
        if let Some(code) = self.exit_code {
            write!(f, "\nexit code: {code}")?;
        }
        if !self.stderr_tail.is_empty() {
            write!(f, "\nstderr tail:\n{}", self.stderr_tail.join("\n"))?;
        }
        Ok(())
    }
}

impl std::error::Error for TransportClosedError {}

/// Every failure the client library surfaces to callers.
#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    /// The runtime answered with a JSON-RPC error response.
    #[error(transparent)]
    Wire(#[from] WireError),

    /// A request exceeded its configured bound. The transport drops the
    /// pending entry (an abandonment): server-side work still runs to close.
    #[error("request `{method}` timed out after {timeout_ms}ms waiting for the DeepSeek Harness runtime")]
    RequestTimeout { method: String, timeout_ms: u64 },

    /// The runtime answered outside its documented protocol.
    #[error("protocol violation: {0}")]
    Protocol(String),

    /// The runtime subprocess is gone or unusable.
    #[error(transparent)]
    TransportClosed(#[from] TransportClosedError),
}
