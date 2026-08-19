//! Low-level JSON-RPC client for a DeepSeek Harness SDK runtime subprocess.
//!
//! HarnessClient owns the child process: it spawns the runtime, speaks the
//! newline-delimited JSON-RPC stdio protocol, fans server notifications out
//! to subscriptions, and tears the child down through the shared
//! shutdown -> stdin-EOF -> SIGTERM -> SIGKILL ladder. It runs OUTSIDE any
//! harness context (its peer is a separate runtime process), mirroring the
//! TypeScript '@deepseek-ai/dsh-sdk-client' HarnessClient.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

use crate::error::{Error, TransportClosedError, WireError};
use crate::protocol::{HarnessNotification, InitializeParams, InitializeResult};
use crate::session::ContentBlock;

/// Retained stderr lines used to diagnose an unexpected runtime death.
const STDERR_TAIL_LIMIT: usize = 400;

/// Poll interval while waiting out a teardown grace.
const EXIT_POLL_MS: u64 = 50;

/// Launch and timeout options for HarnessClient.
#[derive(Debug, Clone)]
pub struct HarnessClientOptions {
    /// The runtime executable (the dsh-jsonrpc-agent bin, a packaged exe, or 'node').
    pub command: String,
    /// Arguments passed to the command (the bin path plus the config).
    pub args: Vec<String>,
    /// Working directory for the runtime process itself.
    pub cwd: Option<PathBuf>,
    /// The complete child environment. None inherits the parent env
    /// verbatim; passing a map replaces it entirely, so callers own
    /// credential policy.
    pub env: Option<HashMap<String, String>>,
    /// Per-request timeout (ms); None waits indefinitely (a turn can
    /// legitimately run long).
    pub request_timeout_ms: Option<u64>,
    /// Bound (ms) on the protocol 'shutdown' exchange inside close().
    pub shutdown_timeout_ms: u64,
    /// Grace (ms) for the runtime's stdin-EOF quiesce during close().
    pub dispose_eof_grace_ms: u64,
    /// Termination confirmation window (ms) after SIGTERM/SIGKILL.
    pub dispose_grace_ms: u64,
}

impl Default for HarnessClientOptions {
    fn default() -> Self {
        Self {
            command: "node".to_string(),
            args: Vec::new(),
            cwd: None,
            env: None,
            request_timeout_ms: None,
            shutdown_timeout_ms: 1_000,
            dispose_eof_grace_ms: 6_000,
            dispose_grace_ms: 3_000,
        }
    }
}

/// One client-side notification subscription record.
struct Subscription {
    sender: mpsc::UnboundedSender<HarnessNotification>,
    failure: Arc<Mutex<Option<TransportClosedError>>>,
}

/// Shared client state reachable from the reader/reaper tasks, the request
/// path, and the subscription streams.
struct Inner {
    stdin: Mutex<Option<ChildStdin>>,
    child: Mutex<Option<Child>>,
    /// Process id captured at spawn (kills must happen before the reaper reaps).
    pid: Mutex<Option<u32>>,
    pending: Mutex<HashMap<String, oneshot::Sender<Result<Value, Error>>>>,
    next_id: AtomicU64,
    subscriptions: Mutex<Vec<Subscription>>,
    /// Child session -> delegating parent, fed by 'subagent.started'.
    parents: Mutex<HashMap<String, String>>,
    stderr_tail: Mutex<VecDeque<String>>,
    /// Non-JSON stdout lines: a protocol violation this library records so a
    /// UI can fail loudly (stdout is reserved for JSON-RPC frames).
    stdout_violations: Mutex<Vec<String>>,
    /// Outer None = process not exited yet; Some(code) = exited (code None
    /// when killed by a signal).
    exit_code: Mutex<Option<Option<i32>>>,
    spawn_error: Mutex<Option<String>>,
    /// Reuse refused once close() has begun.
    closed: AtomicBool,
    /// Set once the stderr reader reaches EOF (its capture is complete).
    stderr_eof: AtomicBool,
    closed_error: OnceLock<TransportClosedError>,
}

/// Grace for the runtime's stdio streams to settle after its exit edge (the
/// TS client's STREAM_SETTLE_MS): the reaper records the exit code and the
/// stderr reader drains its tail before the closed error is frozen.
const STREAM_SETTLE_MS: u64 = 150;

impl Inner {
    fn is_exited(&self) -> bool {
        self.exit_code.lock().unwrap().is_some()
    }

    fn exited_with_code(&self) -> Option<Option<i32>> {
        *self.exit_code.lock().unwrap()
    }

    fn current_closed_error(&self) -> TransportClosedError {
        self.closed_error
            .get_or_init(|| TransportClosedError {
                reason: "DeepSeek Harness runtime exited".to_string(),
                spawn_error: self.spawn_error.lock().unwrap().clone(),
                exit_code: self.exited_with_code().flatten(),
                stderr_tail: self.stderr_tail.lock().unwrap().iter().cloned().collect(),
            })
            .clone()
    }

    /// Fail every pending request and subscription: the transport is gone.
    fn fail_everything(&self) {
        let err = self.current_closed_error();
        let pending = std::mem::take(&mut *self.pending.lock().unwrap());
        for (_, tx) in pending {
            let _ = tx.send(Err(Error::TransportClosed(err.clone())));
        }
        let subs = std::mem::take(&mut *self.subscriptions.lock().unwrap());
        for sub in subs {
            *sub.failure.lock().unwrap() = Some(err.clone());
        }
    }

    /// Deliver one notification to every live subscription.
    fn dispatch(&self, notification: HarnessNotification) {
        self.record_lineage(&notification);
        let mut subs = self.subscriptions.lock().unwrap();
        subs.retain(|sub| sub.sender.send(notification.clone()).is_ok());
    }

    /// Track the parent -> child lineage for session-tree scoping.
    fn record_lineage(&self, notification: &HarnessNotification) {
        if notification.method != "subagent.started" {
            return;
        }
        let (Some(parent), Some(child)) = (
            notification.parent_session_id(),
            notification.child_session_id(),
        ) else {
            return;
        };
        if parent.is_empty() || child.is_empty() || parent == child {
            return;
        }
        self.parents
            .lock()
            .unwrap()
            .insert(child.to_string(), parent.to_string());
    }

    /// Whether 'session_id' is 'root' or a discovered descendant of it.
    fn is_descendant_of(&self, session_id: &str, root: &str) -> bool {
        let parents = self.parents.lock().unwrap();
        let mut visited: Vec<&str> = Vec::new();
        let mut current = session_id;
        loop {
            if current == root {
                return true;
            }
            if visited.contains(&current) {
                return false;
            }
            visited.push(current);
            match parents.get(current) {
                Some(parent) => current = parent,
                None => return false,
            }
        }
    }

    async fn write_frame(&self, frame: &Value) -> Result<(), Error> {
        let mut stdin = self.stdin.lock().unwrap();
        let Some(stdin) = stdin.as_mut() else {
            return Err(Error::TransportClosed(self.current_closed_error()));
        };
        let mut line = serde_json::to_vec(frame)
            .map_err(|e| Error::Protocol(format!("failed to serialize frame: {e}")))?;
        line.push(b'\n');
        stdin
            .write_all(&line)
            .await
            .map_err(|e| Error::TransportClosed(self.closed_error_with_io(e)))?;
        stdin
            .flush()
            .await
            .map_err(|e| Error::TransportClosed(self.closed_error_with_io(e)))?;
        Ok(())
    }

    fn closed_error_with_io(&self, error: std::io::Error) -> TransportClosedError {
        TransportClosedError {
            reason: format!("failed to write to the DeepSeek Harness runtime: {error}"),
            spawn_error: self.spawn_error.lock().unwrap().clone(),
            exit_code: self.exited_with_code().flatten(),
            stderr_tail: self.stderr_tail.lock().unwrap().iter().cloned().collect(),
        }
    }
}

/// Read stdout frames until EOF, dispatching responses and notifications.
async fn reader_loop(stdout: tokio::process::ChildStdout, inner: Arc<Inner>) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(frame) => handle_frame(&inner, frame).await,
            Err(error) => {
                inner
                    .stdout_violations
                    .lock()
                    .unwrap()
                    .push(format!("non-JSON stdout line ({error}): {line}"));
            }
        }
    }
    settle_then_fail(&inner).await;
}

/// Process one incoming frame: requests (never sent by this server) are
/// recorded as violations, responses complete pending entries, notifications
/// fan out to subscriptions.
async fn handle_frame(inner: &Arc<Inner>, frame: Value) {
    let id = frame.get("id").and_then(Value::as_str).map(str::to_string);
    let method = frame
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_string);
    match (id, method) {
        (Some(id), Some(method)) => {
            // Server -> client requests are dead capability: record loudly.
            inner
                .stdout_violations
                .lock()
                .unwrap()
                .push(format!("unexpected server request: {method} (id {id})"));
        }
        (Some(id), None) => {
            let pending = inner.pending.lock().unwrap().remove(&id);
            let Some(tx) = pending else { return }; // unknown id: ignore
            let outcome = if let Some(result) = frame.get("result") {
                Ok(result.clone())
            } else if let Some(error) = frame.get("error").and_then(Value::as_object) {
                Err(Error::Wire(WireError {
                    code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("runtime error")
                        .to_string(),
                    data: error.get("data").cloned(),
                }))
            } else {
                Err(Error::Protocol(format!(
                    "response for {id} carried neither result nor error: {frame}"
                )))
            };
            let _ = tx.send(outcome);
        }
        (None, Some(method)) => {
            let params = frame.get("params").cloned().unwrap_or_else(|| json!({}));
            inner.dispatch(HarnessNotification { method, params });
        }
        (None, None) => {
            inner
                .stdout_violations
                .lock()
                .unwrap()
                .push(format!("unrecognized stdout frame: {frame}"));
        }
    }
}

/// Stderr capture with a bounded tail.
async fn stderr_loop(stderr: tokio::process::ChildStderr, inner: Arc<Inner>) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let mut tail = inner.stderr_tail.lock().unwrap();
        if !line.is_empty() {
            tail.push_back(line);
            while tail.len() > STDERR_TAIL_LIMIT {
                tail.pop_front();
            }
        }
    }
    inner.stderr_eof.store(true, Ordering::SeqCst);
}

/// Reap the child and record its exit edge.
async fn reaper(mut child: Child, inner: Arc<Inner>) {
    let outcome = child.wait().await;
    *inner.exit_code.lock().unwrap() = Some(outcome.ok().and_then(|status| status.code()));
    settle_then_fail(&inner).await;
}

/// Give the exit edge and the stderr reader a bounded window to settle
/// before freezing the closed error (the error carries both). Whoever
/// reaches the end first waits; the other's repeat is idempotent.
async fn settle_then_fail(inner: &Arc<Inner>) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(STREAM_SETTLE_MS);
    while tokio::time::Instant::now() < deadline {
        if inner.is_exited() && inner.stderr_eof.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(EXIT_POLL_MS)).await;
    }
    inner.fail_everything();
}

/// JSON-RPC client for the DeepSeek Harness SDK runtime over subprocess stdio.
///
/// The subprocess starts lazily on the first request and is owned by this
/// instance until close. There is no wire-level cancel: a timed-out request
/// stays running server-side until the runtime is closed.
pub struct HarnessClient {
    options: HarnessClientOptions,
    inner: Option<Arc<Inner>>,
    /// Reuse refused once close() has begun (close is terminal).
    closed: bool,
}

impl HarnessClient {
    /// Build a client over the given launch spec. Nothing spawns until the
    /// first request.
    pub fn new(options: HarnessClientOptions) -> Self {
        Self {
            options,
            inner: None,
            closed: false,
        }
    }

    /// The launch spec this client was built with.
    pub fn options(&self) -> &HarnessClientOptions {
        &self.options
    }

    /// Spawn the runtime subprocess and start reading frames. Idempotent
    /// while the process is live; errors after close (reuse is refused).
    pub fn start(&mut self) -> Result<(), Error> {
        if self.closed {
            return Err(Error::TransportClosed(self.closed_client_error()));
        }
        if self.inner.is_some() {
            return Ok(());
        }
        let mut command = Command::new(&self.options.command);
        command.args(&self.options.args);
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        if let Some(cwd) = &self.options.cwd {
            command.current_dir(cwd);
        }
        if let Some(env) = &self.options.env {
            command.env_clear();
            command.envs(env);
        }
        let mut child = command.spawn().map_err(|error| {
            Error::TransportClosed(TransportClosedError {
                reason: "DeepSeek Harness runtime failed to start".to_string(),
                spawn_error: Some(error.to_string()),
                exit_code: None,
                stderr_tail: Vec::new(),
            })
        })?;
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let stdin = child.stdin.take().expect("stdin piped");
        let pid = child.id();
        let inner = Arc::new(Inner {
            stdin: Mutex::new(Some(stdin)),
            child: Mutex::new(Some(child)),
            pid: Mutex::new(pid),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            subscriptions: Mutex::new(Vec::new()),
            parents: Mutex::new(HashMap::new()),
            stderr_tail: Mutex::new(VecDeque::new()),
            stdout_violations: Mutex::new(Vec::new()),
            exit_code: Mutex::new(None),
            spawn_error: Mutex::new(None),
            closed: AtomicBool::new(false),
            stderr_eof: AtomicBool::new(false),
            closed_error: OnceLock::new(),
        });
        tokio::spawn(reader_loop(stdout, inner.clone()));
        tokio::spawn(stderr_loop(stderr, inner.clone()));
        // The child moves into the reaper.
        let child = inner.child.lock().unwrap().take().expect("child present");
        tokio::spawn(reaper(child, inner.clone()));
        self.inner = Some(inner);
        Ok(())
    }

    /// Perform the process-wide handshake.
    pub async fn initialize(
        &mut self,
        params: InitializeParams,
    ) -> Result<InitializeResult, Error> {
        let result = self
            .request(
                "initialize",
                serde_json::to_value(params).expect("serializable"),
                None,
            )
            .await?;
        let info = result.get("serverInfo").ok_or_else(|| {
            Error::Protocol(format!("initialize returned no server identity: {result}"))
        })?;
        let name = info.get("name").and_then(Value::as_str).ok_or_else(|| {
            Error::Protocol(format!("initialize returned no server name: {result}"))
        })?;
        let version = info.get("version").and_then(Value::as_str).ok_or_else(|| {
            Error::Protocol(format!("initialize returned no server version: {result}"))
        })?;
        Ok(InitializeResult {
            server_info: crate::protocol::ServerInfo {
                name: name.to_string(),
                version: version.to_string(),
            },
        })
    }

    /// Queue one prompt and return its durable inbox identity.
    pub async fn prompt(
        &mut self,
        session_id: &str,
        content_blocks: Vec<ContentBlock>,
    ) -> Result<String, Error> {
        let params = json!({
            "sessionId": session_id,
            "contentBlocks": content_blocks,
        });
        let result = self.request("session/prompt", params, None).await?;
        let message_id = result
            .get("messageId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                Error::Protocol(format!("session/prompt returned no message id: {result}"))
            })?;
        Ok(message_id.to_string())
    }

    /// Send one JSON-RPC request and await its result.
    ///
    /// Rejects with Error::Wire on an error response, Error::RequestTimeout
    /// on timeout (an abandonment: the pending entry is dropped), and
    /// Error::TransportClosed when the runtime is gone.
    pub async fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout_ms: Option<u64>,
    ) -> Result<Value, Error> {
        self.start()?;
        let inner = self.inner.as_ref().expect("started above");
        if inner.closed.load(Ordering::SeqCst) || inner.is_exited() {
            return Err(Error::TransportClosed(inner.current_closed_error()));
        }
        let id = format!("req_{}", inner.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        inner.pending.lock().unwrap().insert(id.clone(), tx);
        let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(error) = inner.write_frame(&frame).await {
            inner.pending.lock().unwrap().remove(&id);
            return Err(error);
        }
        let timeout = timeout_ms.or(self.options.request_timeout_ms);
        match timeout {
            Some(ms) => {
                match tokio::time::timeout(Duration::from_millis(ms), rx).await {
                    Err(_) => {
                        // Abandonment: the server-side work still runs to close.
                        inner.pending.lock().unwrap().remove(&id);
                        Err(Error::RequestTimeout {
                            method: method.to_string(),
                            timeout_ms: ms,
                        })
                    }
                    Ok(Err(_)) => Err(Error::TransportClosed(inner.current_closed_error())),
                    Ok(Ok(outcome)) => outcome,
                }
            }
            None => match rx.await {
                Err(_) => Err(Error::TransportClosed(inner.current_closed_error())),
                Ok(outcome) => outcome,
            },
        }
    }

    /// Subscribe to every server notification. The returned stream rejects
    /// after the runtime dies or the client closes, draining what was already
    /// delivered first (mirroring the TS subscription contract).
    pub fn subscribe(&self) -> NotificationStream {
        let Some(inner) = &self.inner else {
            return NotificationStream::born_failed(Error::TransportClosed(TransportClosedError {
                reason: "DeepSeek Harness runtime is not running".to_string(),
                spawn_error: None,
                exit_code: None,
                stderr_tail: Vec::new(),
            }));
        };
        let (sender, rx) = mpsc::unbounded_channel();
        let failure = Arc::new(Mutex::new(None::<TransportClosedError>));
        if inner.closed.load(Ordering::SeqCst) {
            *failure.lock().unwrap() = Some(inner.current_closed_error());
        } else {
            inner.subscriptions.lock().unwrap().push(Subscription {
                sender,
                failure: failure.clone(),
            });
        }
        NotificationStream {
            rx,
            failure,
            filter: None,
            inner: Some(inner.clone()),
            root: None,
        }
    }

    /// Subscribe to one session and the descendants discovered from
    /// 'subagent.started' lineage edges (the runtime notifies for every
    /// session in its context; scoping is client-side, mirroring the Python
    /// and TypeScript SDKs).
    pub fn subscribe_session_tree(&self, root_session_id: &str) -> NotificationStream {
        let mut stream = self.subscribe();
        stream.root = Some(root_session_id.to_string());
        stream
    }

    /// Shut the runtime down and reap it: a best-effort protocol 'shutdown'
    /// bounded by shutdown_timeout_ms, then the shared stdin-EOF -> SIGTERM
    /// -> SIGKILL ladder until the process actually exits. Idempotent; a
    /// closed client refuses reuse.
    pub async fn close(&mut self) -> Result<(), Error> {
        let Some(inner) = self.inner.clone() else {
            self.closed = true;
            return Ok(());
        };
        self.closed = true;
        inner.closed.store(true, Ordering::SeqCst);
        // 1. Protocol shutdown (bounded). Diagnostic only: the ladder below is
        // the authoritative teardown for a runtime that cannot answer anymore.
        if !inner.is_exited() {
            let _ = self
                .request_bare(
                    &inner,
                    "shutdown",
                    json!({}),
                    Some(self.options.shutdown_timeout_ms),
                )
                .await;
        }
        // 2. Close stdin (EOF) and allow cooperative quiesce.
        inner.stdin.lock().unwrap().take();
        if wait_for_exit(&inner, self.options.dispose_eof_grace_ms).await {
            finish_close(&inner);
            return Ok(());
        }
        // 3. SIGTERM on POSIX; Windows skips straight to forced termination.
        terminate(&inner, TermSignal::Graceful);
        if wait_for_exit(&inner, self.options.dispose_grace_ms).await {
            finish_close(&inner);
            return Ok(());
        }
        // 4. SIGKILL and await a bounded exit edge.
        terminate(&inner, TermSignal::Forced);
        if !wait_for_exit(&inner, self.options.dispose_grace_ms).await {
            let _ = inner.child.lock().unwrap().take();
            return Err(Error::TransportClosed(TransportClosedError {
                reason: format!(
                    "runtime process did not exit within {}ms after SIGKILL",
                    self.options.dispose_grace_ms
                ),
                spawn_error: None,
                exit_code: None,
                stderr_tail: inner.stderr_tail.lock().unwrap().iter().cloned().collect(),
            }));
        }
        finish_close(&inner);
        Ok(())
    }

    /// A request variant used by the teardown path itself: it bypasses the
    /// reuse guard so the protocol 'shutdown' exchange can still fire.
    async fn request_bare(
        &self,
        inner: &Arc<Inner>,
        method: &str,
        params: Value,
        timeout_ms: Option<u64>,
    ) -> Result<Value, Error> {
        if inner.is_exited() {
            return Err(Error::TransportClosed(inner.current_closed_error()));
        }
        let id = format!("req_{}", inner.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        inner.pending.lock().unwrap().insert(id.clone(), tx);
        let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        inner.write_frame(&frame).await?;
        let timeout = timeout_ms.or(self.options.request_timeout_ms);
        match timeout {
            Some(ms) => match tokio::time::timeout(Duration::from_millis(ms), rx).await {
                Err(_) => {
                    inner.pending.lock().unwrap().remove(&id);
                    Err(Error::RequestTimeout {
                        method: method.to_string(),
                        timeout_ms: ms,
                    })
                }
                Ok(Err(_)) => Err(Error::TransportClosed(inner.current_closed_error())),
                Ok(Ok(outcome)) => outcome,
            },
            None => match rx.await {
                Err(_) => Err(Error::TransportClosed(inner.current_closed_error())),
                Ok(outcome) => outcome,
            },
        }
    }

    /// The runtime's exit code after close() (None when killed by signal or
    /// never started), for test assertions and diagnostics.
    pub fn exit_code(&self) -> Option<Option<i32>> {
        self.inner
            .as_ref()
            .and_then(|inner| inner.exited_with_code())
    }

    /// The reuse-refusal error for a client whose close() has begun.
    fn closed_client_error(&self) -> TransportClosedError {
        match &self.inner {
            Some(inner) => inner.current_closed_error(),
            None => TransportClosedError {
                reason: "DeepSeek Harness runtime client is closed".to_string(),
                spawn_error: None,
                exit_code: None,
                stderr_tail: Vec::new(),
            },
        }
    }

    /// Non-JSON stdout lines observed on the wire (a protocol violation —
    /// stdout is reserved for JSON-RPC frames).
    pub fn stdout_violations(&self) -> Vec<String> {
        self.inner
            .as_ref()
            .map(|inner| inner.stdout_violations.lock().unwrap().clone())
            .unwrap_or_default()
    }
}

fn finish_close(inner: &Arc<Inner>) {
    inner.fail_everything();
    *inner.subscriptions.lock().unwrap() = Vec::new();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TermSignal {
    Graceful,
    Forced,
}

/// Deliver a termination signal to the child. POSIX distinguishes SIGTERM
/// (graceful) from SIGKILL (forced); Windows maps both to TerminateProcess.
fn terminate(inner: &Arc<Inner>, signal: TermSignal) {
    let Some(pid) = *inner.pid.lock().unwrap() else {
        return;
    };
    #[cfg(unix)]
    {
        let sig = match signal {
            TermSignal::Graceful => libc::SIGTERM,
            TermSignal::Forced => libc::SIGKILL,
        };
        // SAFETY: pid is the still-live child's process id; kill(2) is
        // async-signal-safe and cannot touch Rust memory.
        unsafe { libc::kill(pid as i32, sig) };
    }
    #[cfg(windows)]
    {
        // The child is owned by the reaper task, so signal it by pid via
        // taskkill: graceful closes the window, forced terminates the tree
        // (the same mapping Node applies to the two signal names).
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string()])
            .args(match signal {
                TermSignal::Graceful => &[""][..0],
                TermSignal::Forced => &["/F", "/T"][..],
            })
            .output();
    }
}

/// Poll the child's recorded exit edge until it appears or the grace expires.
async fn wait_for_exit(inner: &Arc<Inner>, grace_ms: u64) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(grace_ms);
    loop {
        if inner.is_exited() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(EXIT_POLL_MS)).await;
    }
}

// ---------------------------------------------------------------------------
// Notification streams
// ---------------------------------------------------------------------------

/// Awaitable notification subscription. After the runtime dies the stream
/// drains already-delivered notifications and then rejects; after client
/// close it rejects immediately.
pub struct NotificationStream {
    rx: mpsc::UnboundedReceiver<HarnessNotification>,
    failure: Arc<Mutex<Option<TransportClosedError>>>,
    filter: Option<Box<dyn Fn(&HarnessNotification) -> bool + Send>>,
    inner: Option<Arc<Inner>>,
    root: Option<String>,
}

impl NotificationStream {
    fn born_failed(error: Error) -> Self {
        let (_, rx) = mpsc::unbounded_channel();
        let transport = match error {
            Error::TransportClosed(t) => t,
            other => TransportClosedError {
                reason: other.to_string(),
                spawn_error: None,
                exit_code: None,
                stderr_tail: Vec::new(),
            },
        };
        Self {
            rx,
            failure: Arc::new(Mutex::new(Some(transport))),
            filter: None,
            inner: None,
            root: None,
        }
    }

    /// Await the next matching notification. Rejects once the runtime is gone
    /// and the queue has drained.
    pub async fn next(&mut self) -> Result<HarnessNotification, Error> {
        loop {
            match self.rx.recv().await {
                Some(notification) => {
                    if self.accepts(&notification) {
                        return Ok(notification);
                    }
                }
                None => {
                    let failure = self.failure.lock().unwrap().clone();
                    let error = failure.map(Error::TransportClosed).unwrap_or_else(|| {
                        Error::TransportClosed(TransportClosedError {
                            reason: "notification subscription closed".to_string(),
                            spawn_error: None,
                            exit_code: None,
                            stderr_tail: Vec::new(),
                        })
                    });
                    return Err(error);
                }
            }
        }
    }

    /// Drain one already-delivered matching notification without waiting.
    pub fn try_next(&mut self) -> Option<HarnessNotification> {
        loop {
            match self.rx.try_recv() {
                Ok(notification) => {
                    if self.accepts(&notification) {
                        return Some(notification);
                    }
                }
                Err(_) => return None,
            }
        }
    }

    /// Detach from the client: queued items drop and pending waits reject.
    pub fn close(&mut self) {
        self.rx.close();
    }

    fn accepts(&self, notification: &HarnessNotification) -> bool {
        if let Some(filter) = &self.filter {
            if !filter(notification) {
                return false;
            }
        }
        match (&self.inner, &self.root) {
            (Some(inner), Some(root)) => {
                if notification.method == "subagent.started"
                    || notification.method == "subagent.finished"
                {
                    let parent = notification.parent_session_id().unwrap_or("");
                    let child = notification.child_session_id().unwrap_or("");
                    return inner.is_descendant_of(parent, root) || child == root;
                }
                match notification.session_id() {
                    Some(session_id) => inner.is_descendant_of(session_id, root),
                    None => false,
                }
            }
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lineage_scoping_walks_parent_chain() {
        let inner = Inner {
            stdin: Mutex::new(None),
            child: Mutex::new(None),
            pid: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            subscriptions: Mutex::new(Vec::new()),
            parents: Mutex::new(HashMap::from([
                ("child".to_string(), "parent".to_string()),
                ("grandchild".to_string(), "child".to_string()),
            ])),
            stderr_tail: Mutex::new(VecDeque::new()),
            stdout_violations: Mutex::new(Vec::new()),
            exit_code: Mutex::new(None),
            spawn_error: Mutex::new(None),
            closed: AtomicBool::new(false),
            stderr_eof: AtomicBool::new(false),
            closed_error: OnceLock::new(),
        };
        assert!(inner.is_descendant_of("grandchild", "parent"));
        assert!(inner.is_descendant_of("child", "parent"));
        assert!(!inner.is_descendant_of("parent", "child"));
        assert!(!inner.is_descendant_of("unrelated", "parent"));
        assert!(!inner.is_descendant_of("child", "grandchild"));
    }

    #[test]
    fn status_narrowing() {
        let n = HarnessNotification {
            method: "session.status".to_string(),
            params: json!({ "sessionId": "s", "status": "idle" }),
        };
        assert_eq!(
            n.session_status(),
            Some(("s", crate::protocol::AgentStatus::Idle))
        );
    }
}
