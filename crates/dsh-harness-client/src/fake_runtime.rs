//! fake-runtime: a scripted DeepSeek Harness SDK JSON-RPC stdio peer for
//! keyless tests. Exposed as a library module so test binaries in other
//! crates (theus) can run it inside their own bins.
//! keyless tests (the Rust port of the TS client's fake-runtime.ts pattern).
//!
//! It reads one compact JSON frame per line on stdin and answers from a
//! script read from $FAKE_RUNTIME_SCRIPT (JSON):
//!
//! {
//!   "initialize": [{ "result": { "serverInfo": { "name": "deepseek-harness-sdk-runtime", "version": "0.0.1" } } }],
//!   "session/prompt": [{ "send": [ { "method": "session.event", "params": ... }, ... ], "result": { "messageId": "msg-1" } }],
//!   "shutdown": [{ "result": {} }]
//! }
//!
//! Per method, handlers are consumed in order; the last one repeats. A
//! handler may emit frames before its response, answer with result or a
//! JSON-RPC error, and may specify "exit": N to exit with code N after
//! flushing the response. Unknown methods answer -32601. Malformed JSON
//! lines are ignored (mirroring the server).
//!
//! Behavior flags:
//!   FAKE_RUNTIME_STDERR_TAIL=one,two     lines printed to stderr at startup
//!   FAKE_RUNTIME_NOISY_STDOUT=1          print a non-JSON stdout line at startup
//!   FAKE_RUNTIME_IGNORE_EOF=1            do not exit when stdin hits EOF
//!   FAKE_RUNTIME_IGNORE_SIGTERM=1        install a SIGTERM handler that ignores it
//!   FAKE_RUNTIME_EXIT_AFTER_SHUTDOWN=0   exit code after answering shutdown (default 0)
//!   FAKE_RUNTIME_DELAY_MS=200            per-frame write delay (ladder tests)

use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Debug, serde::Deserialize)]
struct Handler {
    #[serde(default)]
    send: Vec<Value>,
    result: Option<Value>,
    error: Option<Value>,
    exit: Option<i32>,
    /// Emit the send frames but never answer the request (timeout tests).
    #[serde(default)]
    hang: bool,
}

/// Serve the scripted JSON-RPC stdio peer until stdin EOF or an exit edge.
pub async fn serve() {
    let script: HashMap<String, Vec<Handler>> = std::env::var("FAKE_RUNTIME_SCRIPT")
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| {
            HashMap::from([
                (
                    "initialize".to_string(),
                    vec![Handler {
                        send: vec![],
                        result: Some(json!({
                            "serverInfo": { "name": "deepseek-harness-sdk-runtime", "version": "0.0.1" }
                        })),
                        error: None,
                        exit: None,
                        hang: false,
                    }],
                ),
                (
                    "session/prompt".to_string(),
                    vec![Handler {
                        send: vec![],
                        result: Some(json!({ "messageId": "msg-1" })),
                        error: None,
                        exit: None,
                        hang: false,
                    }],
                ),
                (
                    "shutdown".to_string(),
                    vec![Handler {
                        send: vec![],
                        result: Some(json!({})),
                        error: None,
                        exit: None,
                        hang: false,
                    }],
                ),
            ])
        });
    let mut cursors: HashMap<String, usize> = HashMap::new();

    if let Some(lines) = std::env::var("FAKE_RUNTIME_STDERR_TAIL").ok() {
        for line in lines.split(',') {
            eprintln!("{line}");
        }
    }
    if std::env::var("FAKE_RUNTIME_NOISY_STDOUT").as_deref() == Ok("1") {
        println!("this line is not JSON (a deliberate stdout violation)");
    }

    #[cfg(unix)]
    if std::env::var("FAKE_RUNTIME_IGNORE_SIGTERM").as_deref() == Ok("1") {
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }
    }

    let ignore_eof = std::env::var("FAKE_RUNTIME_IGNORE_EOF").as_deref() == Ok("1");
    let delay_ms: u64 = std::env::var("FAKE_RUNTIME_DELAY_MS")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(0);
    let exit_after_shutdown: i32 = std::env::var("FAKE_RUNTIME_EXIT_AFTER_SHUTDOWN")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(0);

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(frame) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(method) = frame.get("method").and_then(Value::as_str) else {
            continue;
        };
        let id = frame.get("id").cloned();
        let handlers = script.get(method);
        let Some(handlers) = handlers else {
            if let Some(id) = id {
                respond(
                    &mut stdout,
                    id,
                    None,
                    Some(json!({ "code": -32601, "message": format!("unknown method: {method}") })),
                    delay_ms,
                )
                .await;
            }
            continue;
        };
        let cursor = cursors.entry(method.to_string()).or_insert(0);
        let index = (*cursor).min(handlers.len() - 1);
        *cursor += 1;
        let handler = &handlers[index];
        for frame in &handler.send {
            write_frame(&mut stdout, frame, delay_ms).await;
        }
        let mut exit = handler.exit;
        if method == "shutdown" {
            exit = Some(exit_after_shutdown);
        }
        if let Some(id) = id {
            if !handler.hang {
                respond(
                    &mut stdout,
                    id,
                    handler.result.clone(),
                    handler.error.clone(),
                    delay_ms,
                )
                .await;
            }
        }
        if let Some(code) = exit {
            stdout.flush().await.ok();
            std::process::exit(code);
        }
    }
    // stdin EOF: the runtime disposes immediately and exits 0 (unless told
    // to ignore EOF for ladder tests).
    stdout.flush().await.ok();
    if !ignore_eof {
        std::process::exit(0);
    }
    // Ignoring EOF: park forever so SIGTERM/SIGKILL tiers stay observable.
    std::future::pending::<()>().await;
}

async fn write_frame(out: &mut tokio::io::Stdout, frame: &Value, delay_ms: u64) {
    if delay_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
    }
    let mut line = serde_json::to_vec(frame).expect("frame serializable");
    line.push(b'\n');
    out.write_all(&line).await.ok();
    out.flush().await.ok();
}

async fn respond(
    out: &mut tokio::io::Stdout,
    id: Value,
    result: Option<Value>,
    error: Option<Value>,
    delay_ms: u64,
) {
    let mut frame = json!({ "jsonrpc": "2.0", "id": id });
    match (result, error) {
        (Some(result), _) => {
            frame["result"] = result;
        }
        (None, Some(error)) => {
            frame["error"] = error;
        }
        (None, None) => {
            frame["result"] = json!({});
        }
    }
    write_frame(out, &frame, delay_ms).await;
}
