//! theus run: the headless single-turn runner (M1). Drives the REAL jsonrpc
//! runtime through the owned receipt-to-idle interval and prints the turn
//! live. Exit codes reflect theus's own transport health only - the wire has
//! no prompt-level status, so model outcomes are rendered, never exit codes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use dsh_harness_client::api::{DeepSeekHarness, DeepSeekHarnessOptions, PromptInput};
use dsh_harness_client::client::HarnessClientOptions;
use dsh_harness_client::launch::resolve_launch;
use dsh_harness_client::protocol::{AgentStatus, HarnessNotification};
use dsh_harness_client::session::ContentBlock;

use crate::cli::RunArgs;
use crate::config::RuntimeConfig;

/// Read the prompt from --prompt, --file, or stdin ('-').
fn read_prompt(args: &RunArgs) -> anyhow::Result<String> {
    if let Some(prompt) = &args.prompt {
        return Ok(prompt.clone());
    }
    if let Some(path) = &args.file {
        if path == PathBuf::from("-").as_path() {
            use std::io::Read;
            let mut buffer = String::new();
            std::io::stdin().read_to_string(&mut buffer)?;
            return Ok(buffer);
        }
        return Ok(std::fs::read_to_string(path)?);
    }
    unreachable!("clap enforces prompt-or-file")
}

/// Assemble the runtime launch spec: the resolved bin plus the config as
/// positional argv[2], with the parent environment passed through (theus's
/// credential policy) and mode-specific entries layered over it.
fn build_launch(
    config: &RuntimeConfig,
    shutdown_timeout_ms: u64,
) -> anyhow::Result<HarnessClientOptions> {
    let resolved = resolve_launch(&config.checkout, config.mode).map_err(anyhow::Error::msg)?;
    let mut args = resolved.args;
    args.push(config.config.to_string_lossy().to_string());
    let mut env: HashMap<String, String> = std::env::vars().collect();
    for (key, value) in resolved.env {
        env.insert(key, value);
    }
    Ok(HarnessClientOptions {
        command: resolved.command,
        args,
        cwd: Some(config.cwd.clone()),
        env: Some(env),
        request_timeout_ms: None,
        shutdown_timeout_ms,
        dispose_eof_grace_ms: 6_000,
        dispose_grace_ms: 3_000,
    })
}

/// Live printer over the notification stream.
struct Printer {
    root: String,
    raw_events: bool,
    tool_starts: HashMap<String, u64>,
}

impl Printer {
    fn new(root: String, raw_events: bool) -> Self {
        Self {
            root,
            raw_events,
            tool_starts: HashMap::new(),
        }
    }

    fn print(&mut self, notification: &HarnessNotification) {
        if self.raw_events {
            println!(
                "{}",
                serde_json::to_string(notification).unwrap_or_default()
            );
            return;
        }
        match notification.method.as_str() {
            "session.status" => {
                if notification.session_id() != Some(&self.root) {
                    return;
                }
                match notification.session_status() {
                    Some((_, AgentStatus::Running)) => println!(">> running"),
                    Some((_, AgentStatus::Idle)) => println!(">> idle"),
                    _ => {}
                }
            }
            "session.event" => {
                if notification.session_id() != Some(&self.root) {
                    return;
                }
                let Some(event) = notification.session_event() else {
                    return;
                };
                match event.event_type.as_str() {
                    "turn/start" => {
                        if let Some(marker) = event.turn_marker() {
                            println!("-- turn {} --", marker.turn);
                        }
                    }
                    "turn/end" => {
                        if let Some(marker) = event.turn_marker() {
                            let reason = marker.reason.map(|r| r.describe()).unwrap_or_default();
                            println!("-- turn {}: {reason}", marker.turn);
                        }
                    }
                    "assistant/message" => {
                        if let Some(assistant) = event.assistant_message() {
                            let text = assistant.message.text();
                            if !text.is_empty() {
                                println!(
                                    "assistant (turn {}, step {}):",
                                    assistant.turn, assistant.step
                                );
                                for line in text.lines() {
                                    println!("  {line}");
                                }
                            }
                            if let Some(usage) = &assistant.usage {
                                println!(
                                    "  [usage in={} out={}]",
                                    usage.input_tokens, usage.output_tokens
                                );
                            }
                        }
                    }
                    "tool/call" => {
                        if let Some(call) = event.tool_call() {
                            self.tool_starts.insert(call.call_id.clone(), event.time);
                            println!("  [tool] {} - running", call.name);
                        }
                    }
                    "tool/result" => {
                        if let Some(result) = event.tool_result() {
                            let call_id = result
                                .message
                                .content
                                .first()
                                .and_then(|block| match block {
                                    ContentBlock::ToolResult { tool_call_id, .. } => {
                                        Some(tool_call_id.clone())
                                    }
                                    _ => None,
                                })
                                .unwrap_or_default();
                            let start = self.tool_starts.remove(&call_id);
                            let elapsed = start.map(|t| event.time.saturating_sub(t));
                            let name = result
                                .error
                                .as_ref()
                                .map(|e| format!("{} (error {})", e.name, e.code))
                                .unwrap_or_else(|| "tool".to_string());
                            let timing = elapsed.map(|ms| format!(", {ms}ms")).unwrap_or_default();
                            println!("  [tool] {name} - completed{timing}");
                            if let Some(diffs) = event.fs_diffs() {
                                for diff in diffs {
                                    let old_lines = diff
                                        .old_text
                                        .as_ref()
                                        .map(|t| t.lines().count())
                                        .unwrap_or(0);
                                    let new_lines = diff.new_text.lines().count();
                                    println!(
                                        "    [diff] {} (+{new_lines} -{old_lines})",
                                        diff.path
                                    );
                                }
                            }
                        }
                    }
                    "todo/write" => {
                        if let Some(items) = event.todo_items() {
                            for item in items {
                                let mark = match item.status.as_str() {
                                    "completed" => "[x]",
                                    "in_progress" => "[>]",
                                    _ => "[ ]",
                                };
                                println!("    {mark} {}", item.content);
                            }
                        }
                    }
                    "user/message" => {
                        if let Some(message) = event.user_message() {
                            if message.source.kind == "user" {
                                let text = message.text();
                                if !text.is_empty() {
                                    println!("you: {text}");
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            "subagent.started" => {
                let Some((_parent, child)) = notification.subagent_started() else {
                    return;
                };
                println!("  [subagent] started: {child}");
            }
            "subagent.finished" => {
                let Some(finished) = notification.subagent_finished() else {
                    return;
                };
                let answer = finished
                    .last_assistant_message
                    .as_ref()
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter_map(|b| b.as_text())
                            .collect::<String>()
                    })
                    .unwrap_or_default();
                let status = match finished.status {
                    dsh_harness_client::protocol::SdkRunStatus::Ok => "ok",
                    dsh_harness_client::protocol::SdkRunStatus::Error => "error",
                };
                let answer_part = if answer.is_empty() {
                    String::new()
                } else {
                    format!(" - {answer}")
                };
                println!(
                    "  [subagent] finished: {status}/{}{answer_part}",
                    finished.stop_reason
                );
            }
            _ => {}
        }
    }
}

/// Run one prompt headlessly.
pub async fn run(args: RunArgs) -> anyhow::Result<()> {
    let config = RuntimeConfig::resolve(&args.shared).map_err(anyhow::Error::msg)?;
    let prompt = read_prompt(&args)?;
    let launch = build_launch(&config, args.shutdown_timeout_ms)?;
    let mut harness = DeepSeekHarness::new(DeepSeekHarnessOptions {
        launch,
        cwd: Some(config.cwd.clone()),
        provider: Some(config.provider.clone()),
        model: Some(config.model.clone()),
        max_tokens: config.max_tokens,
    });
    let session_id = harness.session(args.session.as_deref()).id;
    println!(
        "-- theus - {} - {}/{} - {} --",
        session_id,
        config.provider,
        config.model,
        config.cwd.display()
    );
    let started = Instant::now();
    let mut printer = Printer::new(session_id.clone(), args.events);
    let result = harness
        .run_observed(&session_id, PromptInput::Text(prompt), |notification| {
            printer.print(notification)
        })
        .await;
    let outcome: anyhow::Result<()> = match result {
        Ok(result) => {
            println!("----");
            println!("final response: {}", result.final_response);
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json_of_result(&result))?
                );
            }
            Ok(())
        }
        Err(error) => Err(anyhow::Error::new(error)),
    };
    let close_result = harness.close().await;
    for violation in harness.client().stdout_violations() {
        eprintln!("theus: stdout violation (stdout is reserved for JSON-RPC): {violation}");
    }
    if let Err(error) = close_result {
        eprintln!("theus: runtime teardown: {error}");
    }
    eprintln!("theus: turn took {:.1}s", started.elapsed().as_secs_f64());
    outcome
}

fn json_of_result(result: &dsh_harness_client::api::RunResult) -> serde_json::Value {
    serde_json::json!({
        "sessionId": result.session_id,
        "finalResponse": result.final_response,
        "events": result.events,
        "notifications": result.notifications,
    })
}
