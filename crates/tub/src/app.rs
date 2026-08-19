//! The TUI application: owns the runtime subprocess, the notification
//! subscription, keyboard input, and the ratatui event loop. Ctrl+C quits -
//! while a turn is running it first asks for confirmation, because quitting
//! abandons the turn by tearing down the runtime (the wire has no mid-turn
//! cancel; this choice is documented in README.md).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use dsh_harness_client::client::{HarnessClient, HarnessClientOptions, NotificationStream};
use dsh_harness_client::error::Error as ClientError;
use dsh_harness_client::launch::resolve_launch;
use dsh_harness_client::protocol::{AgentStatus, HarnessNotification, InitializeParams};
use dsh_harness_client::session::ContentBlock;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;

use crate::cli::TuiArgs;
use crate::config::RuntimeConfig;
use crate::eventmap::{apply_event, apply_notification, UiItem};
use crate::ui::{self, UiState};

/// The workspace branch shown beside the prompt (best-effort git probe).
fn git_branch(cwd: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["-C", cwd.to_str()?, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Assemble the runtime launch spec (shared with the headless runner).
fn build_launch(config: &RuntimeConfig) -> anyhow::Result<HarnessClientOptions> {
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
        shutdown_timeout_ms: 1_000,
        dispose_eof_grace_ms: 6_000,
        dispose_grace_ms: 3_000,
    })
}

/// Multiline input buffer.
#[derive(Default)]
pub struct InputBuffer {
    lines: Vec<String>,
    cursor: (u16, u16),
}

impl InputBuffer {
    fn text(&self) -> String {
        self.lines.join("\n")
    }

    fn clear(&mut self) {
        self.lines.clear();
        self.cursor = (0, 0);
    }

    fn line(&self) -> &str {
        self.lines
            .get(self.cursor.1 as usize)
            .map(String::as_str)
            .unwrap_or("")
    }

    fn insert(&mut self, ch: char) {
        let row = self.cursor.1 as usize;
        if self.lines.len() <= row {
            self.lines.push(String::new());
        }
        let col = self.cursor.0 as usize;
        let line = &mut self.lines[row];
        let boundary = line
            .char_indices()
            .nth(col)
            .map(|(index, _)| index)
            .unwrap_or(line.len());
        line.insert(boundary, ch);
        self.cursor.0 += 1;
    }

    fn backspace(&mut self) {
        if self.cursor.0 > 0 {
            let row = self.cursor.1 as usize;
            let col = self.cursor.0 as usize;
            let line = &mut self.lines[row];
            let boundary = line
                .char_indices()
                .nth(col)
                .map(|(index, _)| index)
                .unwrap_or(line.len());
            let start = line
                .char_indices()
                .nth(col - 1)
                .map(|(index, _)| index)
                .unwrap_or(0);
            line.replace_range(start..boundary, "");
            self.cursor.0 -= 1;
        } else if self.cursor.1 > 0 {
            let row = self.cursor.1 as usize;
            let tail = self.lines.remove(row);
            let upper = &mut self.lines[row - 1];
            self.cursor.0 = upper.chars().count() as u16;
            upper.push_str(&tail);
            self.cursor.1 -= 1;
        }
    }

    fn newline(&mut self) {
        let row = self.cursor.1 as usize;
        let col = self.cursor.0 as usize;
        if self.lines.len() <= row {
            self.lines.push(String::new());
        }
        let line = &self.lines[row];
        let boundary = line
            .char_indices()
            .nth(col)
            .map(|(index, _)| index)
            .unwrap_or(line.len());
        let tail = line[boundary..].to_string();
        self.lines[row].truncate(boundary);
        self.lines.insert(row + 1, tail);
        self.cursor = (0, self.cursor.1 + 1);
    }

    fn left(&mut self) {
        self.cursor.0 = self.cursor.0.saturating_sub(1);
    }

    fn right(&mut self) {
        let line_len = self.line().chars().count() as u16;
        self.cursor.0 = self.cursor.0.saturating_add(1).min(line_len);
    }

    fn delete_forward(&mut self) {
        let row = self.cursor.1 as usize;
        let col = self.cursor.0 as usize;
        let Some(char_count) = self.lines.get(row).map(|line| line.chars().count()) else {
            return;
        };
        if col < char_count {
            let line = &mut self.lines[row];
            let start = line
                .char_indices()
                .nth(col)
                .map(|(index, _)| index)
                .unwrap();
            let end = line
                .char_indices()
                .nth(col + 1)
                .map(|(index, _)| index)
                .unwrap_or(line.len());
            line.replace_range(start..end, "");
        } else if row + 1 < self.lines.len() {
            let next = self.lines.remove(row + 1);
            self.lines[row].push_str(&next);
        }
    }

    fn up(&mut self) {
        self.cursor.1 = self.cursor.1.saturating_sub(1);
        self.cursor.0 = self.cursor.0.min(self.line().chars().count() as u16);
    }

    fn down(&mut self) {
        self.cursor.1 += 1;
        self.cursor.1 = self.cursor.1.min(self.lines.len().saturating_sub(1) as u16);
        self.cursor.0 = self.cursor.0.min(self.line().chars().count() as u16);
    }

    fn delete_word(&mut self) {
        let row = self.cursor.1 as usize;
        let col = self.cursor.0 as usize;
        let line = &mut self.lines[row];
        let boundary = line
            .char_indices()
            .nth(col)
            .map(|(index, _)| index)
            .unwrap_or(line.len());
        let prefix: String = line[..boundary].chars().collect();
        let trimmed = prefix.trim_end();
        let start = trimmed
            .len()
            .saturating_sub(trimmed.rfind(' ').map(|i| i + 1).unwrap_or(0));
        line.replace_range(start..boundary, "");
        self.cursor.0 = start as u16;
    }
}

/// One UI session view (multiple sessions share one runtime connection).
struct SessionView {
    id: String,
    items: Vec<UiItem>,
    status: AgentStatus,
}

/// The single app-wide prompt waiting for its owning session to become idle.
struct PendingPrompt {
    session_id: String,
    text: String,
}

impl SessionView {
    fn new(id: String) -> Self {
        Self {
            id,
            items: Vec::new(),
            status: AgentStatus::Idle,
        }
    }
}

/// What a key press asks the async loop to do.
#[derive(Debug)]
enum KeyAction {
    None,
    Prompt(String),
    Quit,
}

/// The whole application state.
pub struct App {
    client: HarnessClient,
    views: Vec<SessionView>,
    active: usize,
    input: InputBuffer,
    scroll: usize,
    pending_prompt: Option<PendingPrompt>,
    /// Subagent session id -> root SessionView id that owns it.
    subagent_owner: HashMap<String, String>,
    confirm_quit: bool,
    error: Option<String>,
    started_at: Instant,
    route: String,
    branch: Option<String>,
}

impl App {
    pub fn new(
        client: HarnessClient,
        route: String,
        branch: Option<String>,
        session_id: Option<String>,
    ) -> Self {
        let id = session_id.unwrap_or_else(|| format!("session-{}", uuid::Uuid::new_v4().simple()));
        Self {
            client,
            views: vec![SessionView::new(id)],
            active: 0,
            input: InputBuffer::default(),
            scroll: 0,
            pending_prompt: None,
            subagent_owner: HashMap::new(),
            confirm_quit: false,
            error: None,
            started_at: Instant::now(),
            route,
            branch,
        }
    }

    fn view(&self) -> &SessionView {
        &self.views[self.active]
    }

    fn index_of(&mut self, session_id: &str) -> usize {
        if let Some(index) = self.views.iter().position(|view| view.id == session_id) {
            return index;
        }
        self.views.push(SessionView::new(session_id.to_string()));
        self.views.len() - 1
    }

    /// Resolve and record the root view that owns a subagent notification.
    fn subagent_root(&mut self, notification: &HarnessNotification) -> Option<String> {
        let parent = notification.parent_session_id()?.to_string();
        let child = notification.child_session_id()?.to_string();
        let root = self.subagent_owner.get(&parent).cloned().unwrap_or(parent);
        self.subagent_owner.insert(child, root.clone());
        Some(root)
    }

    /// Route one wire notification into the app state; returns a queued
    /// prompt and its owning session when it becomes sendable.
    fn handle_notification(
        &mut self,
        notification: HarnessNotification,
    ) -> Option<(String, String)> {
        match notification.method.as_str() {
            "session.event" => {
                let session_id = notification.session_id().map(str::to_string)?;
                let event = notification.session_event()?;
                if let Err(error) = event.validate() {
                    self.error = Some(error.to_string());
                    return None;
                }
                let index = self.index_of(&session_id);
                apply_event(&mut self.views[index].items, &event);
                None
            }
            "session.status" => {
                let (session_id, status) = notification.session_status()?;
                let index = self.index_of(session_id);
                let was_running = self.views[index].status == AgentStatus::Running;
                self.views[index].status = status;
                if was_running
                    && status == AgentStatus::Idle
                    && self
                        .pending_prompt
                        .as_ref()
                        .is_some_and(|pending| pending.session_id == session_id)
                {
                    let pending = self.pending_prompt.take().unwrap();
                    return Some((pending.session_id, pending.text));
                }
                None
            }
            "subagent.started" | "subagent.finished" => {
                let root = self.subagent_root(&notification)?;
                let index = self.index_of(&root);
                apply_notification(&mut self.views[index].items, &notification);
                None
            }
            _ => None,
        }
    }

    /// Enqueue one prompt on an explicit session (the fast RPC; the turn itself
    /// streams back through the subscription).
    async fn enqueue(&mut self, session_id: &str, text: String) {
        let blocks = vec![ContentBlock::Text { text }];
        match self.client.prompt(session_id, blocks).await {
            Ok(_) => {
                let index = self.index_of(session_id);
                self.views[index].status = AgentStatus::Running;
            }
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    /// Handle one key event.
    fn handle_key(&mut self, key: KeyEvent) -> KeyAction {
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.view().status == AgentStatus::Running && !self.confirm_quit {
                    self.confirm_quit = true;
                    return KeyAction::None;
                }
                KeyAction::Quit
            }
            KeyCode::Char('q') if key.modifiers.is_empty() => KeyAction::Quit,
            KeyCode::Esc => KeyAction::Quit,
            KeyCode::Enter => {
                let text = self.input.text();
                if text.trim().is_empty() {
                    return KeyAction::None;
                }
                if self.view().status == AgentStatus::Running {
                    let session_id = self.view().id.clone();
                    self.input.clear();
                    self.pending_prompt = Some(PendingPrompt { session_id, text });
                    return KeyAction::None;
                }
                self.input.clear();
                self.confirm_quit = false;
                KeyAction::Prompt(text)
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let id = format!("session-{}", uuid::Uuid::new_v4().simple());
                self.views.push(SessionView::new(id));
                self.active = self.views.len() - 1;
                self.scroll = 0;
                KeyAction::None
            }
            KeyCode::Char(ch) => {
                if key.modifiers.contains(KeyModifiers::ALT) && ch == '\n' {
                    self.input.newline();
                } else if key.modifiers.contains(KeyModifiers::CONTROL) && ch == 'w' {
                    self.input.delete_word();
                } else if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                {
                    self.input.insert(ch);
                }
                KeyAction::None
            }
            KeyCode::Backspace => {
                self.input.backspace();
                KeyAction::None
            }
            KeyCode::Delete => {
                self.input.delete_forward();
                KeyAction::None
            }
            KeyCode::Left => {
                self.input.left();
                KeyAction::None
            }
            KeyCode::Right => {
                self.input.right();
                KeyAction::None
            }
            KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.scroll += 1;
                KeyAction::None
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.scroll = self.scroll.saturating_sub(1);
                KeyAction::None
            }
            KeyCode::Up => {
                self.input.up();
                KeyAction::None
            }
            KeyCode::Down => {
                self.input.down();
                KeyAction::None
            }
            KeyCode::PageUp => {
                self.scroll += 5;
                KeyAction::None
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(5);
                KeyAction::None
            }
            KeyCode::Tab => {
                self.active = (self.active + 1) % self.views.len();
                self.scroll = 0;
                KeyAction::None
            }
            _ => KeyAction::None,
        }
    }

    fn ui_state<'a>(&'a self, cwd: &'a str) -> UiState<'a> {
        let view = self.view();
        let status = match (view.status, self.confirm_quit) {
            (AgentStatus::Running, _) => "running",
            (AgentStatus::Idle, true) => "idle (confirm quit)",
            (AgentStatus::Idle, false) => "idle",
        };
        UiState {
            items: &view.items,
            status,
            session_id: &view.id,
            route: &self.route,
            cwd,
            branch: self.branch.as_deref(),
            input_lines: &self.input.lines,
            cursor: self.input.cursor,
            scroll: self.scroll,
            queued: self.pending_prompt.is_some(),
            confirm_quit: self.confirm_quit,
            error: self.error.as_deref(),
            violations: self.client.stdout_violations().len(),
            elapsed: self.started_at.elapsed(),
        }
    }
}

/// Run the interactive TUI.
pub async fn run(args: TuiArgs) -> anyhow::Result<()> {
    let config = RuntimeConfig::resolve(&args.shared).map_err(anyhow::Error::msg)?;
    let branch = git_branch(&config.cwd);
    let route = format!("{}/{}", config.provider, config.model);
    let cwd = config.cwd.to_string_lossy().to_string();
    let launch = build_launch(&config)?;
    let mut client = HarnessClient::new(launch);
    let mut terminal = ratatui::init();
    let result = async {
        terminal.draw(|frame| {
            let area = frame.area();
            let paragraph =
                ratatui::widgets::Paragraph::new("starting DeepSeek Harness runtime...")
                    .block(ratatui::widgets::Block::bordered().title(" tub "));
            frame.render_widget(paragraph, area);
        })?;
        client
            .initialize(InitializeParams {
                cwd: config.cwd.to_string_lossy().to_string(),
                provider: config.provider.clone(),
                model: config.model.clone(),
                max_tokens: config.max_tokens,
            })
            .await
            .map_err(anyhow::Error::new)?;
        let subscription = client.subscribe();
        let mut app = App::new(client, route, branch, args.session);
        run_loop(&mut terminal, &mut app, subscription, &cwd).await
    }
    .await;
    // Always restore the terminal, then tear the runtime down.
    ratatui::restore();
    result
}

/// The main event loop: notifications, keys, and a redraw tick.
async fn run_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    mut subscription: NotificationStream,
    cwd: &str,
) -> anyhow::Result<()> {
    let (key_tx, mut key_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || loop {
        let Ok(available) = event::poll(Duration::from_millis(100)) else {
            break;
        };
        if !available {
            continue;
        }
        match event::read() {
            Ok(Event::Key(key)) => {
                if key_tx.send(key).is_err() {
                    break;
                }
            }
            Ok(Event::Resize(_, _)) => {}
            Ok(_) => {}
            Err(_) => break,
        }
    });
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        terminal.draw(|frame| ui::render(frame, &app.ui_state(cwd)))?;
        tokio::select! {
            notification = subscription.next() => {
                match notification {
                    Ok(notification) => {
                        if let Some((session_id, prompt)) = app.handle_notification(notification) {
                            app.enqueue(&session_id, prompt).await;
                        }
                    }
                    Err(ClientError::TransportClosed(error)) => {
                        app.error = Some(format!("runtime gone: {error}"));
                        break;
                    }
                    Err(error) => {
                        app.error = Some(error.to_string());
                        break;
                    }
                }
            }
            key = key_rx.recv() => {
                let Some(key) = key else { break };
                match app.handle_key(key) {
                    KeyAction::None => {}
                    KeyAction::Prompt(prompt) => {
                        let session_id = app.view().id.clone();
                        app.enqueue(&session_id, prompt).await;
                    }
                    KeyAction::Quit => break,
                }
            }
            _ = tick.tick() => {}
        }
    }
    // Orderly teardown: protocol shutdown drains persistence, then the
    // ladder reaps the child.
    match app.client.close().await {
        Ok(()) => {}
        Err(error) => {
            eprintln!("tub: runtime teardown: {error}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dummy_client() -> HarnessClient {
        HarnessClient::new(HarnessClientOptions {
            command: "true".to_string(),
            ..HarnessClientOptions::default()
        })
    }

    fn app() -> App {
        App::new(
            dummy_client(),
            "p/m".to_string(),
            None,
            Some("s1".to_string()),
        )
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn status_notification(session: &str, status: &str) -> HarnessNotification {
        serde_json::from_value(json!({
            "method": "session.status",
            "params": { "sessionId": session, "status": status }
        }))
        .unwrap()
    }

    #[test]
    fn enter_submits_prompt_when_idle() {
        let mut app = app();
        for ch in "hi".chars() {
            app.input.insert(ch);
        }
        match app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)) {
            KeyAction::Prompt(text) => assert_eq!(text, "hi"),
            other => panic!("expected prompt action, got {other:?}"),
        }
        assert!(app.input.text().is_empty());
    }

    #[test]
    fn enter_queues_prompt_when_running_and_fires_on_idle() {
        let mut app = app();
        app.views[0].status = AgentStatus::Running;
        for ch in "later".chars() {
            app.input.insert(ch);
        }
        assert!(matches!(
            app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)),
            KeyAction::None
        ));
        assert!(app.input.text().is_empty());
        let pending = app.pending_prompt.as_ref().unwrap();
        assert_eq!(pending.session_id, "s1");
        assert_eq!(pending.text, "later");
        let fired = app.handle_notification(status_notification("s1", "idle"));
        assert_eq!(fired, Some(("s1".to_string(), "later".to_string())));
        assert!(app.pending_prompt.is_none());
    }

    #[test]
    fn right_clamps_to_line_end() {
        let mut input = InputBuffer::default();
        for ch in "hi".chars() {
            input.insert(ch);
        }
        input.right();
        input.right();
        assert_eq!(input.cursor.0, 2);
    }

    #[test]
    fn backspace_after_right_at_line_end_removes_only_one_character() {
        let mut input = InputBuffer::default();
        for ch in "hi".chars() {
            input.insert(ch);
        }
        input.right();
        input.right();
        input.backspace();
        assert_eq!(input.text(), "h");
    }

    #[test]
    fn delete_key_removes_utf8_character_under_cursor() {
        let mut app = app();
        for ch in "aéz".chars() {
            app.input.insert(ch);
        }
        app.input.left();
        app.input.left();
        app.handle_key(key(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(app.input.text(), "az");
        assert_eq!(app.input.cursor, (1, 0));
    }

    #[test]
    fn delete_key_at_line_end_joins_the_next_line() {
        let mut app = app();
        for ch in "hi".chars() {
            app.input.insert(ch);
        }
        app.input.newline();
        for ch in "there".chars() {
            app.input.insert(ch);
        }
        app.input.up();
        app.handle_key(key(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(app.input.text(), "hithere");
        assert_eq!(app.input.cursor, (2, 0));
    }

    #[test]
    fn unrelated_session_idle_and_input_do_not_consume_queued_prompt() {
        let mut app = app();
        app.views[0].status = AgentStatus::Running;
        for ch in "later".chars() {
            app.input.insert(ch);
        }
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));

        app.handle_key(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
        let other_id = app.views[1].id.clone();
        for ch in "for b".chars() {
            app.input.insert(ch);
        }
        match app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)) {
            KeyAction::Prompt(text) => assert_eq!(text, "for b"),
            other => panic!("expected prompt action, got {other:?}"),
        }
        let pending = app.pending_prompt.as_ref().unwrap();
        assert_eq!(pending.session_id, "s1");
        assert_eq!(pending.text, "later");

        app.handle_notification(status_notification(&other_id, "running"));
        assert_eq!(
            app.handle_notification(status_notification(&other_id, "idle")),
            None
        );
        assert!(app.pending_prompt.is_some());

        assert_eq!(
            app.handle_notification(status_notification("s1", "idle")),
            Some(("s1".to_string(), "later".to_string()))
        );
        assert!(app.pending_prompt.is_none());
    }

    #[test]
    fn ctrl_c_confirms_then_quits_while_running() {
        let mut app = app();
        app.views[0].status = AgentStatus::Running;
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            KeyAction::None
        ));
        assert!(app.confirm_quit);
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            KeyAction::Quit
        ));
    }

    #[test]
    fn ctrl_c_quits_immediately_when_idle() {
        let mut app = app();
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            KeyAction::Quit
        ));
    }

    #[test]
    fn tab_cycles_sessions_and_ctrl_n_mints_one() {
        let mut app = app();
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('n'), KeyModifiers::CONTROL)),
            KeyAction::None
        ));
        assert_eq!(app.views.len(), 2);
        assert_eq!(app.active, 1);
        app.handle_key(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.active, 0);
    }

    #[test]
    fn notifications_route_to_sessions_and_fork_new_views() {
        let mut app = app();
        let event: HarnessNotification = serde_json::from_value(json!({
            "method": "session.event",
            "params": { "sessionId": "s1", "event": {
                "type": "turn/start", "seq": 1, "time": 1, "data": { "turn": 1 } } }
        }))
        .unwrap();
        assert_eq!(app.handle_notification(event.clone()), None);
        assert_eq!(app.views[0].items.len(), 1);
        // A foreign session's event creates its own view, not items on s1.
        let foreign: HarnessNotification = serde_json::from_value(json!({
            "method": "session.event",
            "params": { "sessionId": "s9", "event": {
                "type": "turn/start", "seq": 1, "time": 1, "data": { "turn": 1 } } }
        }))
        .unwrap();
        app.handle_notification(foreign);
        assert_eq!(app.views.len(), 2);
        assert_eq!(app.views[0].items.len(), 1);
        assert_eq!(app.views[1].items.len(), 1);
    }

    #[test]
    fn subagent_notifications_route_to_root_session_not_active_tab() {
        let mut app = app();
        app.handle_key(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(app.active, 1);

        let started: HarnessNotification = serde_json::from_value(json!({
            "method": "subagent.started",
            "params": { "parentSessionId": "s1", "childSessionId": "sub-1" }
        }))
        .unwrap();
        app.handle_notification(started);

        let nested_started: HarnessNotification = serde_json::from_value(json!({
            "method": "subagent.started",
            "params": { "parentSessionId": "sub-1", "childSessionId": "sub-2" }
        }))
        .unwrap();
        app.handle_notification(nested_started);

        let nested_finished: HarnessNotification = serde_json::from_value(json!({
            "method": "subagent.finished",
            "params": {
                "provider": "spawn",
                "agentId": "a2",
                "parentSessionId": "sub-1",
                "childSessionId": "sub-2",
                "status": "ok",
                "stopReason": "completed"
            }
        }))
        .unwrap();
        app.handle_notification(nested_finished);

        let finished: HarnessNotification = serde_json::from_value(json!({
            "method": "subagent.finished",
            "params": {
                "provider": "spawn",
                "agentId": "a1",
                "parentSessionId": "s1",
                "childSessionId": "sub-1",
                "status": "ok",
                "stopReason": "completed"
            }
        }))
        .unwrap();
        app.handle_notification(finished);

        assert_eq!(app.views[0].items.len(), 2);
        assert!(app.views[0].items.iter().all(|item| matches!(
            item,
            UiItem::Subagent {
                state: crate::eventmap::SubagentState::Finished { .. },
                ..
            }
        )));
        assert!(app.views[1].items.is_empty());
        assert_eq!(
            app.subagent_owner.get("sub-1").map(String::as_str),
            Some("s1")
        );
        assert_eq!(
            app.subagent_owner.get("sub-2").map(String::as_str),
            Some("s1")
        );
    }

    #[test]
    fn running_transitions_on_status_notifications() {
        let mut app = app();
        app.handle_notification(status_notification("s1", "running"));
        assert_eq!(app.views[0].status, AgentStatus::Running);
        app.handle_notification(status_notification("s1", "idle"));
        assert_eq!(app.views[0].status, AgentStatus::Idle);
        let state = app.ui_state("/work");
        assert_eq!(state.status, "idle");
        assert_eq!(state.session_id, "s1");
        assert_eq!(state.route, "p/m");
    }
}
