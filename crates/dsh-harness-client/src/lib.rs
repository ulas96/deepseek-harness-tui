//! Rust protocol client library for the DeepSeek Harness SDK JSON-RPC
//! stdio runtime — the design twin of the TypeScript '@deepseek-ai/dsh-sdk-client'
//! and the Python 'deepseek-harness' SDK, sharing the same runtime peer,
//! protocol, and layering.
//!
//! - 'DeepSeekHarness' is the high-level owned-run API (lazy start, memoized
//!   initialize, run(input) owning the receipt-to-idle interval, close).
//! - 'HarnessClient' is the lower-level protocol client (spawn, request,
//!   subscription fan-out, teardown ladder).
//! - 'session' carries the session-log vocabulary that is part of the wire
//!   contract (SessionEvent / ContentBlock tagged unions).
//!
//! This library registers nothing on a harness context; the runtime process
//! it spawns is a complete harness whose composition its own cordis.yml
//! decides.

pub mod api;
pub mod client;
pub mod error;
pub mod fake_runtime;
pub mod launch;
pub mod protocol;
pub mod session;

pub use api::{
    normalize_input, DeepSeekHarness, DeepSeekHarnessOptions, HarnessSession, PromptInput,
    RunOptions, RunResult,
};
pub use client::{HarnessClient, HarnessClientOptions, NotificationStream};
pub use error::{Error, TransportClosedError, WireError};
pub use protocol::{
    AgentStatus, HarnessNotification, InitializeParams, InitializeResult, SdkRunStatus, ServerInfo,
    SessionEventNotification, SessionPromptParams, SessionPromptResult, SessionStatusNotification,
    SubagentFinishedNotification, SubagentStartedNotification,
};
pub use session::{
    final_response, ContentBlock, FileDiff, FinishReason, LlmFailure, Message, MessageSource,
    SessionEvent, StreamChunk, TodoItem, TokenUsage, TurnEndReason,
};
