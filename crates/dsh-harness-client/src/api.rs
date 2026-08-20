//! High-level owned-run API over HarnessClient: DeepSeekHarness owns one
//! runtime subprocess across many sessions; HarnessSession.run sends a prompt
//! and settles when the whole agent next becomes idle. Mirrors the Python and
//! TypeScript SDKs' DeepSeekHarness / Session pair.

use std::path::PathBuf;

use uuid::Uuid;

use crate::client::{HarnessClient, HarnessClientOptions};
use crate::error::Error;
use crate::protocol::{HarnessNotification, InitializeParams, InitializeResult};
use crate::session::{final_response, is_inbox_receipt, ContentBlock, SessionEvent};

/// Options for the high-level DeepSeekHarness wrapper.
#[derive(Debug, Clone)]
pub struct DeepSeekHarnessOptions {
    /// Launch spec for the runtime subprocess (command, args, cwd, env, timeouts).
    pub launch: HarnessClientOptions,
    /// Workspace cwd recorded on every SDK-created session (default: the
    /// launch cwd, else the current process cwd). Resolved absolute before
    /// the handshake.
    pub cwd: Option<PathBuf>,
    /// Provider route for SDK-created agents (default 'deepseek-official').
    pub provider: Option<String>,
    /// Model for SDK-created agents (default 'deepseek-v4-flash').
    pub model: Option<String>,
    /// Maximum output tokens for each conversation-model request.
    pub max_tokens: Option<u64>,
}

impl Default for DeepSeekHarnessOptions {
    fn default() -> Self {
        Self {
            launch: HarnessClientOptions::default(),
            cwd: None,
            provider: None,
            model: None,
            max_tokens: None,
        }
    }
}

/// Input accepted by run(): plain text or content blocks sent verbatim.
#[derive(Debug, Clone)]
pub enum PromptInput {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl From<&str> for PromptInput {
    fn from(text: &str) -> Self {
        PromptInput::Text(text.to_string())
    }
}

impl From<String> for PromptInput {
    fn from(text: String) -> Self {
        PromptInput::Text(text)
    }
}

impl From<Vec<ContentBlock>> for PromptInput {
    fn from(blocks: Vec<ContentBlock>) -> Self {
        PromptInput::Blocks(blocks)
    }
}

/// Normalize run input: a string becomes one text block; blocks pass verbatim.
pub fn normalize_input(input: PromptInput) -> Vec<ContentBlock> {
    match input {
        PromptInput::Text(text) => vec![ContentBlock::Text { text }],
        PromptInput::Blocks(blocks) => blocks,
    }
}

/// Per-run options: target session and a streaming observer.
#[derive(Debug, Default, Clone)]
pub struct RunOptions {
    /// Session id to run on; omitted mints a fresh session per call.
    pub session_id: Option<String>,
}

/// One owned session activity interval, from enqueue receipt through idle.
#[derive(Debug, Clone)]
pub struct RunResult {
    /// The session the activity ran on.
    pub session_id: String,
    /// Concatenated text of the interval's last assistant message (empty
    /// when none).
    pub final_response: String,
    /// Every 'session.event' payload for the root session, in wire order.
    pub events: Vec<SessionEvent>,
    /// Every notification for the root session and discovered descendants,
    /// in wire order.
    pub notifications: Vec<HarnessNotification>,
}

/// Reusable SDK for running DeepSeek Harness agent turns in a runtime
/// subprocess. The subprocess starts lazily on first use and stays owned by
/// this instance until close; always close so the child is reaped.
pub struct DeepSeekHarness {
    options: DeepSeekHarnessOptions,
    client: HarnessClient,
    cwd: PathBuf,
    provider: String,
    model: String,
    max_tokens: Option<u64>,
    initialized: Option<Result<InitializeResult, Error>>,
    closed: bool,
}

impl DeepSeekHarness {
    /// Build over the given launch spec plus the session route. cwd is
    /// resolved absolute client-side (a relative value would double-resolve
    /// inside the child).
    pub fn new(options: DeepSeekHarnessOptions) -> Self {
        let launch_cwd = options
            .cwd
            .clone()
            .or_else(|| options.launch.cwd.clone())
            .unwrap_or_else(|| PathBuf::from("."));
        let cwd = std::path::absolute(&launch_cwd).unwrap_or(launch_cwd);
        let provider = options
            .provider
            .clone()
            .unwrap_or_else(|| "deepseek-official".to_string());
        let model = options
            .model
            .clone()
            .unwrap_or_else(|| "deepseek-v4-flash".to_string());
        let max_tokens = options.max_tokens;
        Self {
            client: HarnessClient::new(options.launch.clone()),
            options,
            cwd,
            provider,
            model,
            max_tokens,
            initialized: None,
            closed: false,
        }
    }

    /// The underlying JSON-RPC client (exposed for low-level access).
    pub fn client(&self) -> &HarnessClient {
        &self.client
    }

    /// The absolute workspace cwd recorded on every SDK-created session.
    pub fn cwd(&self) -> &PathBuf {
        &self.cwd
    }

    /// The resolved provider route for SDK-created agents.
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// The resolved model for SDK-created agents.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Start the subprocess and perform the initialize handshake once
    /// (memoized). On failure the runtime is reaped and a fresh client
    /// replaces it, so a later call retries with a new subprocess — unless
    /// close already ended this harness.
    pub async fn start(&mut self) -> Result<InitializeResult, Error> {
        if self.closed {
            return Err(Error::Protocol(
                "DeepSeek Harness instance is closed".to_string(),
            ));
        }
        if let Some(initialized) = &self.initialized {
            return initialized.clone();
        }
        let result = async {
            let params = InitializeParams {
                cwd: self.cwd.to_string_lossy().to_string(),
                provider: self.provider.clone(),
                model: self.model.clone(),
                reasoning_effort: None,
                max_tokens: self.max_tokens,
            };
            self.client.initialize(params).await
        }
        .await;
        match result {
            Ok(info) => {
                self.initialized = Some(Ok(info.clone()));
                Ok(info)
            }
            Err(error) => {
                self.initialized = None;
                let _ = self.client.close().await;
                if !self.closed {
                    self.client = HarnessClient::new(self.options.launch.clone());
                }
                Err(error)
            }
        }
    }

    /// Open a session handle (no wire traffic; the runtime creates the
    /// session on its first prompt).
    pub fn session(&self, session_id: Option<&str>) -> HarnessSession {
        let id = session_id
            .map(str::to_string)
            .unwrap_or_else(|| format!("session-{}", Uuid::new_v4().simple()));
        HarnessSession::new(self, id)
    }

    /// Run one prompt on a fresh (or named) session. Mirrors the TS client's
    /// DeepSeekHarness.run(input, options).
    pub async fn run(
        &mut self,
        input: PromptInput,
        options: &RunOptions,
    ) -> Result<RunResult, Error> {
        let session_id = self.session(options.session_id.as_deref()).id;
        self.run_on(&session_id, input).await
    }

    /// Run one prompt on an explicit session id (validated client-side: an
    /// unknown id would silently create an agent server-side).
    pub async fn run_on(
        &mut self,
        session_id: &str,
        input: PromptInput,
    ) -> Result<RunResult, Error> {
        self.run_observed(session_id, input, |_| {}).await
    }

    /// Run one prompt on an explicit session id, invoking an observer with
    /// every notification for this session tree in wire order — the streaming
    /// surface a live UI renders from.
    pub async fn run_observed<F>(
        &mut self,
        session_id: &str,
        input: PromptInput,
        mut observer: F,
    ) -> Result<RunResult, Error>
    where
        F: FnMut(&HarnessNotification),
    {
        self.start().await?;
        let content_blocks = normalize_input(input);
        let mut subscription = self.client.subscribe_session_tree(session_id);
        let mut events: Vec<SessionEvent> = Vec::new();
        let mut notifications: Vec<HarnessNotification> = Vec::new();

        let run = async {
            let message_id = self.client.prompt(session_id, content_blocks).await?;
            let mut received = false;
            loop {
                let notification = subscription.next().await?;
                if !received {
                    let receipt = notification.method == "session.event"
                        && notification.session_id() == Some(session_id)
                        && notification
                            .session_event()
                            .map(|event| is_inbox_receipt(&event, &message_id))
                            .unwrap_or(false);
                    if !receipt {
                        continue;
                    }
                    received = true;
                }
                let is_idle = notification
                    .session_status()
                    .map(|(id, status)| {
                        id == session_id && status == crate::protocol::AgentStatus::Idle
                    })
                    .unwrap_or(false);
                observer(&notification);
                if notification.method == "session.event"
                    && notification.session_id() == Some(session_id)
                {
                    let event = notification.session_event().ok_or_else(|| {
                        Error::Protocol("session.event carried no event envelope".to_string())
                    })?;
                    event.validate()?;
                    events.push(event);
                }
                notifications.push(notification);
                if is_idle {
                    break;
                }
            }
            Ok::<(), Error>(())
        }
        .await;
        let outcome = match run {
            Ok(()) => Ok(RunResult {
                session_id: session_id.to_string(),
                final_response: final_response(&events),
                events,
                notifications,
            }),
            Err(error) => Err(error),
        };
        subscription.close();
        outcome
    }

    /// Shut down and reap the runtime subprocess. Idempotent and terminal —
    /// a closed harness no longer retries a failed handshake.
    pub async fn close(&mut self) -> Result<(), Error> {
        self.closed = true;
        self.client.close().await
    }
}

/// One SDK session: a stable id plus owned activity intervals.
#[derive(Debug, Clone)]
pub struct HarnessSession {
    /// The wire session id this handle runs on.
    pub id: String,
}

impl HarnessSession {
    fn new(_harness: &DeepSeekHarness, id: String) -> Self {
        Self { id }
    }
}
