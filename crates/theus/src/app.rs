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
use dsh_harness_client::protocol::{
    AgentStatus, DiscoveredModel, HarnessNotification, InitializeParams, ModelCatalogEntry,
    ModelCatalogResult, ModelSelection, ProviderCatalogEntry, ProviderDraft, SessionListEntry,
};
use dsh_harness_client::session::ContentBlock;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;

use crate::cli::TuiArgs;
use crate::commands::{self, ProviderCommand, SlashCommand};
use crate::config::RuntimeConfig;
use crate::eventmap::{apply_event, apply_notification, build_items, UiItem};
use crate::ui::{self, OverlayView, UiState};

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
    title: Option<String>,
    items: Vec<UiItem>,
    status: AgentStatus,
    selection: ModelSelection,
}

/// The single app-wide prompt waiting for its owning session to become idle.
struct PendingPrompt {
    session_id: String,
    text: String,
}

impl SessionView {
    fn new(id: String, selection: ModelSelection) -> Self {
        Self {
            id,
            title: None,
            items: Vec::new(),
            status: AgentStatus::Idle,
            selection,
        }
    }

    fn route(&self) -> String {
        match &self.selection.reasoning_effort {
            Some(effort) => format!(
                "{}/{} [{}]",
                self.selection.provider, self.selection.model, effort
            ),
            None => format!(
                "{}/{} [default]",
                self.selection.provider, self.selection.model
            ),
        }
    }
}

#[derive(Debug, Clone)]
enum PickerAction {
    SelectModel(ModelSelection),
    SetEffort(Option<String>),
    ChooseProvider(ProviderCatalogEntry),
    StartProviderForm(Option<String>),
    Resume(String),
}

#[derive(Debug, Clone)]
struct PickerItem {
    label: String,
    action: PickerAction,
}

#[derive(Debug, Clone)]
struct Picker {
    title: String,
    items: Vec<PickerItem>,
    selected: usize,
}

#[derive(Debug, Clone)]
struct ProviderField {
    label: &'static str,
    value: String,
    secret: bool,
}

#[derive(Debug, Clone)]
struct ProviderForm {
    fields: Vec<ProviderField>,
    active: usize,
}

impl ProviderForm {
    fn new(provider: Option<String>) -> Self {
        Self {
            fields: vec![
                ProviderField {
                    label: "Route",
                    value: provider.unwrap_or_default(),
                    secret: false,
                },
                ProviderField {
                    label: "Display name",
                    value: String::new(),
                    secret: false,
                },
                ProviderField {
                    label: "Base URL (blank = catalog)",
                    value: String::new(),
                    secret: false,
                },
                ProviderField {
                    label: "API protocol",
                    value: "openai-completions".into(),
                    secret: false,
                },
                ProviderField {
                    label: "Credential ref",
                    value: String::new(),
                    secret: false,
                },
                ProviderField {
                    label: "API key (optional)",
                    value: String::new(),
                    secret: true,
                },
            ],
            active: 0,
        }
    }

    fn draft(&self) -> ProviderDraft {
        let optional = |value: &str| (!value.trim().is_empty()).then(|| value.trim().to_string());
        let provider = self.fields[0].value.trim().to_string();
        let credential_value = optional(&self.fields[5].value);
        let credential_ref = optional(&self.fields[4].value).or_else(|| {
            credential_value.as_ref().map(|_| {
                let normalized: String = provider
                    .chars()
                    .map(|ch| {
                        if ch.is_ascii_alphanumeric() {
                            ch.to_ascii_uppercase()
                        } else {
                            '_'
                        }
                    })
                    .collect();
                format!("{normalized}_API_KEY")
            })
        });
        ProviderDraft {
            provider,
            display_name: optional(&self.fields[1].value),
            base_url: optional(&self.fields[2].value),
            api: optional(&self.fields[3].value),
            credential_ref,
            credential_value,
            model_ids: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
struct DiscoveredProvider {
    draft: ProviderDraft,
    models: Vec<DiscoveredModel>,
    selected: Vec<bool>,
    cursor: usize,
}

#[derive(Debug, Clone)]
enum Overlay {
    Picker(Picker),
    ProviderForm(ProviderForm),
    DiscoveredProvider(DiscoveredProvider),
    ConfirmInit,
    Message { title: String, body: String },
}

/// What a key press asks the async loop to do.
#[derive(Debug)]
enum KeyAction {
    None,
    Prompt(String),
    Slash(SlashCommand),
    Picker(PickerAction),
    DiscoverProvider(ProviderDraft),
    AddProvider(ProviderDraft),
    Init,
    Quit,
}

/// One completed management RPC's effect on `App`, applied on the main loop
/// once the background task that ran the request finishes. Keeps
/// `model/catalog`, `session/select-model`, `session/list`, `session/resume`,
/// `provider/discover`, and `provider/add` off the event loop's critical
/// path so notifications and redraws never freeze while one is in flight.
type ManagementReply = Box<dyn FnOnce(&mut App) + Send>;

/// The whole application state.
pub struct App {
    client: HarnessClient,
    mgmt_tx: mpsc::UnboundedSender<ManagementReply>,
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
    default_selection: ModelSelection,
    catalog: Option<ModelCatalogResult>,
    overlay: Option<Overlay>,
    branch: Option<String>,
    cwd: std::path::PathBuf,
}

impl App {
    pub fn new(
        client: HarnessClient,
        mgmt_tx: mpsc::UnboundedSender<ManagementReply>,
        selection: ModelSelection,
        branch: Option<String>,
        session_id: Option<String>,
        cwd: std::path::PathBuf,
    ) -> Self {
        let id = session_id.unwrap_or_else(|| format!("session-{}", uuid::Uuid::new_v4().simple()));
        Self {
            client,
            mgmt_tx,
            views: vec![SessionView::new(id, selection.clone())],
            active: 0,
            input: InputBuffer::default(),
            scroll: 0,
            pending_prompt: None,
            subagent_owner: HashMap::new(),
            confirm_quit: false,
            error: None,
            started_at: Instant::now(),
            default_selection: selection,
            catalog: None,
            overlay: None,
            branch,
            cwd,
        }
    }

    /// Test-only entry point mirroring the key-driven slash-command dispatch
    /// path (`execute_slash` is private, and integration tests under
    /// `tests/` can only reach `pub` items). Used to prove management RPCs
    /// no longer block the caller — see `tests/management_rpc.rs`.
    #[doc(hidden)]
    pub async fn dispatch_slash_for_test(&mut self, command: SlashCommand) {
        self.execute_slash(command).await;
    }

    fn view(&self) -> &SessionView {
        &self.views[self.active]
    }

    fn index_of(&mut self, session_id: &str) -> usize {
        if let Some(index) = self.views.iter().position(|view| view.id == session_id) {
            return index;
        }
        self.views.push(SessionView::new(
            session_id.to_string(),
            self.default_selection.clone(),
        ));
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
        if self.overlay.is_some() {
            return self.handle_overlay_key(key);
        }
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.handle_ctrl_c()
            }
            KeyCode::Char('q') if key.modifiers.is_empty() => KeyAction::Quit,
            KeyCode::Esc => KeyAction::Quit,
            KeyCode::Enter => {
                let mut text = self.input.text();
                if text.trim().is_empty() {
                    return KeyAction::None;
                }
                match commands::parse(&text) {
                    Ok(Some(command)) => {
                        self.input.clear();
                        self.confirm_quit = false;
                        return KeyAction::Slash(command);
                    }
                    Ok(None) => {
                        if text.trim_start().starts_with("//") {
                            let offset = text.find("//").unwrap_or(0);
                            text.remove(offset);
                        }
                    }
                    Err(error) => {
                        self.input.clear();
                        self.error = Some(error);
                        return KeyAction::None;
                    }
                }
                if self.view().status == AgentStatus::Running {
                    if self.pending_prompt.is_some() {
                        self.error = Some("a prompt is already queued; it was not replaced".into());
                        return KeyAction::None;
                    }
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
                self.views
                    .push(SessionView::new(id, self.default_selection.clone()));
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

    fn handle_ctrl_c(&mut self) -> KeyAction {
        if self.view().status == AgentStatus::Running && !self.confirm_quit {
            self.confirm_quit = true;
            return KeyAction::None;
        }
        KeyAction::Quit
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) -> KeyAction {
        let Some(mut overlay) = self.overlay.take() else {
            return KeyAction::None;
        };
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return self.handle_ctrl_c();
        }
        match &mut overlay {
            Overlay::Picker(picker) => match key.code {
                KeyCode::Esc => KeyAction::None,
                KeyCode::Up => {
                    picker.selected = picker.selected.saturating_sub(1);
                    self.overlay = Some(overlay);
                    KeyAction::None
                }
                KeyCode::Down => {
                    picker.selected =
                        (picker.selected + 1).min(picker.items.len().saturating_sub(1));
                    self.overlay = Some(overlay);
                    KeyAction::None
                }
                KeyCode::Enter => picker
                    .items
                    .get(picker.selected)
                    .map(|item| KeyAction::Picker(item.action.clone()))
                    .unwrap_or(KeyAction::None),
                _ => {
                    self.overlay = Some(overlay);
                    KeyAction::None
                }
            },
            Overlay::ProviderForm(form) => match key.code {
                KeyCode::Esc => KeyAction::None,
                KeyCode::Up => {
                    form.active = form.active.saturating_sub(1);
                    self.overlay = Some(overlay);
                    KeyAction::None
                }
                KeyCode::Down | KeyCode::Tab => {
                    form.active = (form.active + 1).min(form.fields.len() - 1);
                    self.overlay = Some(overlay);
                    KeyAction::None
                }
                KeyCode::Backspace => {
                    form.fields[form.active].value.pop();
                    self.overlay = Some(overlay);
                    KeyAction::None
                }
                KeyCode::Char(ch)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    form.fields[form.active].value.push(ch);
                    self.overlay = Some(overlay);
                    KeyAction::None
                }
                KeyCode::Enter if form.active + 1 < form.fields.len() => {
                    form.active += 1;
                    self.overlay = Some(overlay);
                    KeyAction::None
                }
                KeyCode::Enter => {
                    let draft = form.draft();
                    if draft.provider.is_empty() {
                        self.error = Some("route must not be blank".into());
                        self.overlay = Some(overlay);
                        KeyAction::None
                    } else {
                        KeyAction::DiscoverProvider(draft)
                    }
                }
                _ => {
                    self.overlay = Some(overlay);
                    KeyAction::None
                }
            },
            Overlay::DiscoveredProvider(discovered) => match key.code {
                KeyCode::Esc => KeyAction::None,
                KeyCode::Up => {
                    discovered.cursor = discovered.cursor.saturating_sub(1);
                    self.overlay = Some(overlay);
                    KeyAction::None
                }
                KeyCode::Down => {
                    discovered.cursor =
                        (discovered.cursor + 1).min(discovered.models.len().saturating_sub(1));
                    self.overlay = Some(overlay);
                    KeyAction::None
                }
                KeyCode::Char(' ') => {
                    if let Some(selected) = discovered.selected.get_mut(discovered.cursor) {
                        *selected = !*selected;
                    }
                    self.overlay = Some(overlay);
                    KeyAction::None
                }
                KeyCode::Enter => {
                    let mut draft = discovered.draft.clone();
                    draft.model_ids = discovered
                        .models
                        .iter()
                        .zip(&discovered.selected)
                        .filter(|(_, selected)| **selected)
                        .map(|(model, _)| model.id.clone())
                        .collect();
                    if draft.model_ids.is_empty() {
                        self.error = Some("select at least one model".into());
                        self.overlay = Some(overlay);
                        KeyAction::None
                    } else {
                        KeyAction::AddProvider(draft)
                    }
                }
                _ => {
                    self.overlay = Some(overlay);
                    KeyAction::None
                }
            },
            Overlay::ConfirmInit => match key.code {
                KeyCode::Enter | KeyCode::Char('y') => KeyAction::Init,
                KeyCode::Esc | KeyCode::Char('n') => KeyAction::None,
                _ => {
                    self.overlay = Some(overlay);
                    KeyAction::None
                }
            },
            Overlay::Message { .. } => match key.code {
                KeyCode::Enter | KeyCode::Esc => KeyAction::None,
                _ => {
                    self.overlay = Some(overlay);
                    KeyAction::None
                }
            },
        }
    }

    fn overlay_view(&self) -> Option<OverlayView> {
        match self.overlay.as_ref()? {
            Overlay::Picker(picker) => Some(OverlayView {
                title: picker.title.clone(),
                lines: picker.items.iter().map(|item| item.label.clone()).collect(),
                selected: Some(picker.selected),
                hint: "↑/↓ move · Enter select · Esc close".into(),
                cursor: None,
            }),
            Overlay::ProviderForm(form) => {
                let lines: Vec<String> = form
                    .fields
                    .iter()
                    .map(|field| {
                        let value = if field.secret && !field.value.is_empty() {
                            "•".repeat(field.value.chars().count())
                        } else {
                            field.value.clone()
                        };
                        format!("{}: {}", field.label, value)
                    })
                    .collect();
                // The form only ever appends/removes at the end of the active
                // field's value, so the caret always sits at the line's end.
                let cursor_col = lines[form.active].chars().count();
                Some(OverlayView {
                    title: "Add provider · discover models".into(),
                    lines,
                    selected: Some(form.active),
                    hint: "type to edit · ↑/↓ fields · Enter next/discover · Esc close".into(),
                    cursor: Some((form.active, cursor_col)),
                })
            }
            Overlay::DiscoveredProvider(discovered) => Some(OverlayView {
                title: format!("Models discovered for {}", discovered.draft.provider),
                lines: discovered
                    .models
                    .iter()
                    .zip(&discovered.selected)
                    .map(|(model, selected)| {
                        format!(
                            "[{}] {}",
                            if *selected { 'x' } else { ' ' },
                            model.name.as_deref().unwrap_or(&model.id)
                        )
                    })
                    .collect(),
                selected: Some(discovered.cursor),
                hint: "Space toggle · Enter save provider · Esc close".into(),
                cursor: None,
            }),
            Overlay::ConfirmInit => Some(OverlayView {
                title: "Update AGENTS.md?".into(),
                lines: vec![
                    "AGENTS.md already exists. Ask the agent to inspect the repo and update it?"
                        .into(),
                ],
                selected: None,
                hint: "Enter/y confirm · n/Esc cancel".into(),
                cursor: None,
            }),
            Overlay::Message { title, body } => Some(OverlayView {
                title: title.clone(),
                lines: body.lines().map(str::to_string).collect(),
                selected: None,
                hint: "Enter/Esc close".into(),
                cursor: None,
            }),
        }
    }

    async fn execute_action(&mut self, action: KeyAction) {
        match action {
            KeyAction::None | KeyAction::Quit => {}
            KeyAction::Prompt(prompt) => {
                let session_id = self.view().id.clone();
                self.enqueue(&session_id, prompt).await;
            }
            KeyAction::Slash(command) => self.execute_slash(command).await,
            KeyAction::Picker(action) => self.execute_picker(action),
            KeyAction::DiscoverProvider(draft) => {
                let mut client = self.client.clone();
                let tx = self.mgmt_tx.clone();
                tokio::spawn(async move {
                    let result = client.discover_provider(&draft).await;
                    let _ = tx.send(Box::new(move |app: &mut App| match result {
                        Ok(result) => {
                            let selected = if draft.base_url.is_none() {
                                vec![true; result.models.len()]
                            } else {
                                vec![false; result.models.len()]
                            };
                            app.overlay = Some(Overlay::DiscoveredProvider(DiscoveredProvider {
                                draft,
                                models: result.models,
                                selected,
                                cursor: 0,
                            }));
                        }
                        Err(error) => app.error = Some(error.to_string()),
                    }) as ManagementReply);
                });
            }
            KeyAction::AddProvider(draft) => {
                let mut client = self.client.clone();
                let tx = self.mgmt_tx.clone();
                tokio::spawn(async move {
                    let reply: ManagementReply = match client.add_provider(&draft).await {
                        Ok(result) => match client.model_catalog().await {
                            Ok(catalog) => Box::new(move |app: &mut App| {
                                app.catalog = Some(catalog);
                                app.open_model_picker(Some(&result.provider));
                            }),
                            Err(error) => {
                                Box::new(move |app: &mut App| app.error = Some(error.to_string()))
                            }
                        },
                        Err(error) => {
                            Box::new(move |app: &mut App| app.error = Some(error.to_string()))
                        }
                    };
                    let _ = tx.send(reply);
                });
            }
            KeyAction::Init => self.submit_init().await,
        }
    }

    async fn execute_slash(&mut self, command: SlashCommand) {
        self.error = None;
        match command {
            SlashCommand::Help => {
                self.overlay = Some(Overlay::Message {
                    title: "Slash commands".into(),
                    body: "/model [provider/model]\n/effort [default|id]\n/provider [route]\n/provider add\n/resume [session-id]\n/init\n//text sends /text".into(),
                });
            }
            SlashCommand::Init => {
                if self.cwd.join("AGENTS.md").exists() {
                    self.overlay = Some(Overlay::ConfirmInit);
                } else {
                    self.submit_init().await;
                }
            }
            SlashCommand::Resume(session_id) => {
                if self.view().status == AgentStatus::Running {
                    self.error = Some("wait for the active turn to finish before resuming".into());
                    return;
                }
                if let Some(session_id) = session_id {
                    self.resume(&session_id);
                } else {
                    self.list_sessions_then_open_picker();
                }
            }
            SlashCommand::Model(route) => {
                if self.view().status == AgentStatus::Running {
                    self.error = Some(
                        "wait for the active turn to finish before opening model management".into(),
                    );
                    return;
                }
                self.refresh_catalog_then(move |app| match route {
                    Some(route) => match app.resolve_model_argument(&route) {
                        Ok(selection) => app.select_model(selection),
                        Err(error) => app.error = Some(error),
                    },
                    None => app.open_model_picker(None),
                });
            }
            SlashCommand::Effort(effort) => {
                if self.view().status == AgentStatus::Running {
                    self.error = Some(
                        "wait for the active turn to finish before opening model management".into(),
                    );
                    return;
                }
                self.refresh_catalog_then(move |app| {
                    if let Some(effort) = effort {
                        let effort = (effort != "default").then_some(effort);
                        if let Err(error) = app.validate_effort(effort.as_deref()) {
                            app.error = Some(error);
                            return;
                        }
                        app.apply_effort(effort);
                    } else {
                        app.open_effort_picker();
                    }
                });
            }
            SlashCommand::Provider(command) => {
                if matches!(command, ProviderCommand::Add) {
                    self.overlay = Some(Overlay::ProviderForm(ProviderForm::new(None)));
                    return;
                }
                if self.view().status == AgentStatus::Running {
                    self.error = Some(
                        "wait for the active turn to finish before opening model management".into(),
                    );
                    return;
                }
                self.refresh_catalog_then(move |app| match command {
                    ProviderCommand::Add => unreachable!(),
                    ProviderCommand::Choose(Some(provider)) => {
                        let entry = app
                            .catalog
                            .as_ref()
                            .and_then(|catalog| {
                                catalog
                                    .providers
                                    .iter()
                                    .find(|entry| entry.provider == provider)
                            })
                            .cloned();
                        match entry {
                            Some(entry) => app.choose_provider(entry),
                            None => {
                                app.error = Some(format!("unknown provider route \"{provider}\""))
                            }
                        }
                    }
                    ProviderCommand::Choose(None) => app.open_provider_picker(),
                });
            }
        }
    }

    fn execute_picker(&mut self, action: PickerAction) {
        match action {
            PickerAction::SelectModel(selection) => self.select_model(selection),
            PickerAction::SetEffort(effort) => self.apply_effort(effort),
            PickerAction::ChooseProvider(provider) => self.choose_provider(provider),
            PickerAction::StartProviderForm(provider) => {
                self.overlay = Some(Overlay::ProviderForm(ProviderForm::new(provider)));
            }
            PickerAction::Resume(session_id) => self.resume(&session_id),
        }
    }

    /// Fetch the model catalog on a spawned task and, once it lands, run
    /// `then` against `App` on the main loop — keeps `model/catalog` (and
    /// any RPC `then` chains onto, e.g. `select_model`) off the event loop's
    /// critical path. See `ManagementReply`.
    fn refresh_catalog_then(&mut self, then: impl FnOnce(&mut App) + Send + 'static) {
        let mut client = self.client.clone();
        let tx = self.mgmt_tx.clone();
        tokio::spawn(async move {
            let result = client.model_catalog().await;
            let _ = tx.send(Box::new(move |app: &mut App| match result {
                Ok(catalog) => {
                    app.catalog = Some(catalog);
                    then(app);
                }
                Err(error) => {
                    app.error = Some(format!(
                        "this runtime does not support model management: {error}"
                    ));
                }
            }) as ManagementReply);
        });
    }

    fn list_sessions_then_open_picker(&mut self) {
        let mut client = self.client.clone();
        let tx = self.mgmt_tx.clone();
        tokio::spawn(async move {
            let result = client.list_sessions().await;
            let _ = tx.send(Box::new(move |app: &mut App| match result {
                Ok(result) => {
                    let items = result
                        .sessions
                        .into_iter()
                        .filter(|session| !session.unreadable)
                        .map(|session| PickerItem {
                            label: session_label(&session),
                            action: PickerAction::Resume(session.session_id),
                        })
                        .collect();
                    app.open_picker("Resume conversation", items);
                }
                Err(error) => app.error = Some(error.to_string()),
            }) as ManagementReply);
        });
    }

    fn open_picker(&mut self, title: impl Into<String>, items: Vec<PickerItem>) {
        let title = title.into();
        if items.is_empty() {
            self.error = Some(format!("{title} has no available entries"));
            return;
        }
        self.overlay = Some(Overlay::Picker(Picker {
            title,
            items,
            selected: 0,
        }));
    }

    fn open_model_picker(&mut self, provider: Option<&str>) {
        let Some(catalog) = &self.catalog else { return };
        let mut items = Vec::new();
        for route in catalog
            .providers
            .iter()
            .filter(|route| route.active && provider.is_none_or(|id| route.provider == id))
        {
            for model in &route.models {
                items.push(PickerItem {
                    label: format!("{}/{} · {}", route.provider, model.id, model.name),
                    action: PickerAction::SelectModel(selection_for(route, model)),
                });
            }
        }
        self.open_picker("Choose model", items);
    }

    fn open_effort_picker(&mut self) {
        let selection = &self.view().selection;
        let model = self.catalog.as_ref().and_then(|catalog| {
            catalog
                .providers
                .iter()
                .find(|route| route.provider == selection.provider)
                .and_then(|route| {
                    route
                        .models
                        .iter()
                        .find(|model| model.id == selection.model)
                })
        });
        let mut items = vec![PickerItem {
            label: "Default · provider/model default".into(),
            action: PickerAction::SetEffort(None),
        }];
        if let Some(reasoning) = model.and_then(|model| model.reasoning.as_ref()) {
            items.extend(reasoning.efforts.iter().map(|effort| PickerItem {
                label: format!("{} · {}", effort.id, effort.name),
                action: PickerAction::SetEffort(Some(effort.id.clone())),
            }));
        }
        self.open_picker("Choose reasoning effort", items);
    }

    fn open_provider_picker(&mut self) {
        let Some(catalog) = &self.catalog else { return };
        let mut items = vec![PickerItem {
            label: "+ Add provider…".into(),
            action: PickerAction::StartProviderForm(None),
        }];
        items.extend(
            catalog
                .providers
                .iter()
                .cloned()
                .map(|provider| PickerItem {
                    label: format!(
                        "{} · {}{}",
                        provider.provider,
                        provider.display_name,
                        if provider.active { "" } else { " (configure)" }
                    ),
                    action: PickerAction::ChooseProvider(provider),
                }),
        );
        self.open_picker("Choose provider", items);
    }

    fn choose_provider(&mut self, provider: ProviderCatalogEntry) {
        if provider.active {
            self.open_model_picker(Some(&provider.provider));
        } else {
            self.overlay = Some(Overlay::ProviderForm(ProviderForm::new(Some(
                provider.provider,
            ))));
        }
    }

    fn resolve_model_argument(&self, argument: &str) -> Result<ModelSelection, String> {
        let catalog = self.catalog.as_ref().ok_or("model catalog is not loaded")?;
        if let Some((provider, model)) = argument.split_once('/') {
            let route = catalog
                .providers
                .iter()
                .find(|route| route.provider == provider && route.active)
                .ok_or_else(|| format!("unknown active provider \"{provider}\""))?;
            let model = route
                .models
                .iter()
                .find(|entry| entry.id == model)
                .ok_or_else(|| format!("provider \"{provider}\" has no model \"{model}\""))?;
            return Ok(selection_for(route, model));
        }
        let current = &self.view().selection.provider;
        let mut matches = catalog
            .providers
            .iter()
            .filter(|route| route.active)
            .flat_map(|route| {
                route
                    .models
                    .iter()
                    .filter(move |model| model.id == argument)
                    .map(move |model| (route, model))
            });
        let first = matches
            .next()
            .ok_or_else(|| format!("unknown model \"{argument}\""))?;
        let picked = if first.0.provider == *current {
            first
        } else {
            matches
                .find(|(route, _)| route.provider == *current)
                .unwrap_or(first)
        };
        Ok(selection_for(picked.0, picked.1))
    }

    fn validate_effort(&self, effort: Option<&str>) -> Result<(), String> {
        let Some(effort) = effort else { return Ok(()) };
        let selection = &self.view().selection;
        let supported = self
            .catalog
            .as_ref()
            .and_then(|catalog| {
                catalog
                    .providers
                    .iter()
                    .find(|route| route.provider == selection.provider)
            })
            .and_then(|route| {
                route
                    .models
                    .iter()
                    .find(|model| model.id == selection.model)
            })
            .and_then(|model| model.reasoning.as_ref())
            .is_some_and(|reasoning| reasoning.efforts.iter().any(|entry| entry.id == effort));
        supported.then_some(()).ok_or_else(|| {
            format!(
                "reasoning effort \"{effort}\" is not supported by {}/{}",
                selection.provider, selection.model
            )
        })
    }

    fn apply_effort(&mut self, effort: Option<String>) {
        let mut selection = self.view().selection.clone();
        selection.reasoning_effort = effort;
        self.select_model(selection);
    }

    fn select_model(&mut self, selection: ModelSelection) {
        let session_id = self.view().id.clone();
        let mut client = self.client.clone();
        let tx = self.mgmt_tx.clone();
        tokio::spawn(async move {
            let result = client.select_model(&session_id, &selection).await;
            let _ = tx.send(Box::new(move |app: &mut App| match result {
                Ok(result) => {
                    if let Some(view) = app.views.iter_mut().find(|view| view.id == session_id) {
                        view.selection = result.selected;
                    }
                }
                Err(error) => app.error = Some(error.to_string()),
            }) as ManagementReply);
        });
    }

    fn resume(&mut self, session_id: &str) {
        let mut client = self.client.clone();
        let tx = self.mgmt_tx.clone();
        let session_id = session_id.to_string();
        tokio::spawn(async move {
            let result = client.resume_session(&session_id).await;
            let _ = tx.send(Box::new(move |app: &mut App| match result {
                Ok(resumed) => {
                    let already_open = app.views.iter().any(|view| view.id == resumed.session_id);
                    let index = app.index_of(&resumed.session_id);
                    app.views[index].title = resumed.title;
                    if !already_open {
                        app.views[index].items = build_items(&resumed.events, &[]);
                    }
                    app.views[index].status = resumed.status;
                    app.views[index].selection = resumed.selection;
                    app.active = index;
                    app.scroll = 0;
                    if !resumed.routable {
                        app.error = Some(
                            "the resumed provider is unavailable; choose /model before prompting"
                                .into(),
                        );
                    }
                }
                Err(error) => app.error = Some(error.to_string()),
            }) as ManagementReply);
        });
    }

    async fn submit_init(&mut self) {
        let prompt = "Inspect this repository and create or update AGENTS.md at the repository root. Summarize the repository's purpose, architecture, important commands, conventions, and verification workflow as concise instructions for future coding agents. Preserve accurate useful guidance already in AGENTS.md. Modify only AGENTS.md, and verify every claim against the repository.".to_string();
        let session_id = self.view().id.clone();
        if self.view().status == AgentStatus::Running {
            if self.pending_prompt.is_some() {
                self.error = Some("a prompt is already queued; /init did not replace it".into());
            } else {
                self.pending_prompt = Some(PendingPrompt {
                    session_id,
                    text: prompt,
                });
            }
        } else {
            self.enqueue(&session_id, prompt).await;
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
            session_id: view.title.as_deref().unwrap_or(&view.id),
            route: view.route(),
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
            overlay: self.overlay_view(),
        }
    }
}

fn selection_for(provider: &ProviderCatalogEntry, model: &ModelCatalogEntry) -> ModelSelection {
    ModelSelection {
        provider: provider.provider.clone(),
        model: model.id.clone(),
        reasoning_effort: model
            .reasoning
            .as_ref()
            .and_then(|reasoning| reasoning.default_effort.clone()),
    }
}

fn session_label(session: &SessionListEntry) -> String {
    let title = session.title.as_deref().unwrap_or("Untitled conversation");
    let live = if session.live { " · live" } else { "" };
    format!("{title} · {}{live}", session.session_id)
}

/// Run the interactive TUI.
pub async fn run(args: TuiArgs) -> anyhow::Result<()> {
    let config = RuntimeConfig::resolve(&args.shared).map_err(anyhow::Error::msg)?;
    let branch = git_branch(&config.cwd);
    let selection = ModelSelection {
        provider: config.provider.clone(),
        model: config.model.clone(),
        reasoning_effort: None,
    };
    let cwd = config.cwd.to_string_lossy().to_string();
    let launch = build_launch(&config)?;
    let mut client = HarnessClient::new(launch);
    let mut terminal = ratatui::init();
    let result = async {
        terminal.draw(|frame| {
            let area = frame.area();
            let paragraph =
                ratatui::widgets::Paragraph::new("starting DeepSeek Harness runtime...")
                    .block(ratatui::widgets::Block::bordered().title(" theus "));
            frame.render_widget(paragraph, area);
        })?;
        let initialized = match client
            .initialize(InitializeParams {
                cwd: config.cwd.to_string_lossy().to_string(),
                provider: config.provider.clone(),
                model: config.model.clone(),
                reasoning_effort: None,
                max_tokens: config.max_tokens,
            })
            .await
        {
            Ok(result) => result,
            Err(error) => {
                close_runtime(&mut client).await;
                return Err(anyhow::Error::new(error));
            }
        };
        let subscription = client.subscribe();
        let (mgmt_tx, mgmt_rx) = mpsc::unbounded_channel();
        let mut app = App::new(
            client,
            mgmt_tx,
            selection,
            branch,
            args.session,
            config.cwd.clone(),
        );
        if initialized.capabilities.is_empty() {
            app.error = Some(
                "runtime is legacy: slash-command management requires the updated Harness checkout"
                    .into(),
            );
        }
        run_loop(&mut terminal, &mut app, subscription, mgmt_rx, &cwd).await
    }
    .await;
    // Runtime teardown happens before this returns; always restore the terminal.
    ratatui::restore();
    result
}

/// The main event loop: notifications, keys, and a redraw tick.
async fn run_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    mut subscription: NotificationStream,
    mut mgmt_rx: mpsc::UnboundedReceiver<ManagementReply>,
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
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let result = async {
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
                    let action = app.handle_key(key);
                    if matches!(action, KeyAction::Quit) { break }
                    app.execute_action(action).await;
                }
                job = mgmt_rx.recv() => {
                    if let Some(job) = job { job(app); }
                }
                _ = &mut shutdown => break,
                _ = tick.tick() => {}
            }
        }
        Ok(())
    }
    .await;

    // This is deliberately outside the loop future so draw, input, signal,
    // and transport errors all shut down and reap the runtime before return.
    close_runtime(&mut app.client).await;
    result
}

async fn close_runtime(client: &mut HarnessClient) {
    if let Err(error) = client.close().await {
        eprintln!("theus: runtime teardown: {error}");
    }
}

/// Resolve when the TUI process is asked to terminate outside its key map.
/// Ctrl+C typed in raw mode still arrives through crossterm; this also covers
/// process-manager termination and a terminal hangup on Unix.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let Ok(mut terminate) = signal(SignalKind::terminate()) else {
            let _ = tokio::signal::ctrl_c().await;
            return;
        };
        let Ok(mut hangup) = signal(SignalKind::hangup()) else {
            let _ = tokio::signal::ctrl_c().await;
            return;
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
            _ = hangup.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
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
        let (mgmt_tx, _mgmt_rx) = mpsc::unbounded_channel();
        App::new(
            dummy_client(),
            mgmt_tx,
            ModelSelection {
                provider: "p".into(),
                model: "m".into(),
                reasoning_effort: None,
            },
            None,
            Some("s1".to_string()),
            std::path::PathBuf::from("/work"),
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
    fn slash_commands_are_intercepted_and_double_slash_is_literal() {
        let mut app = app();
        for ch in "/model".chars() {
            app.input.insert(ch);
        }
        assert!(matches!(
            app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)),
            KeyAction::Slash(SlashCommand::Model(None))
        ));

        for ch in "//model".chars() {
            app.input.insert(ch);
        }
        match app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)) {
            KeyAction::Prompt(text) => assert_eq!(text, "/model"),
            other => panic!("expected literal prompt, got {other:?}"),
        }
    }

    #[test]
    fn provider_key_is_masked_in_overlay() {
        let mut app = app();
        let mut form = ProviderForm::new(None);
        form.fields[5].value = "top-secret".into();
        app.overlay = Some(Overlay::ProviderForm(form));
        let overlay = app.overlay_view().unwrap();
        assert!(!overlay.lines.join("\n").contains("top-secret"));
        assert!(overlay.lines[5].contains('•'));
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
    fn ui_state_prefers_resumed_title_over_raw_session_id() {
        let mut app = app();
        assert_eq!(app.ui_state("/work").session_id, "s1");
        app.views[0].title = Some("My Title".into());
        assert_eq!(app.ui_state("/work").session_id, "My Title");
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
    fn ctrl_c_quits_while_overlay_open() {
        let mut app = app();
        app.overlay = Some(Overlay::Message {
            title: "t".into(),
            body: "b".into(),
        });
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            KeyAction::Quit
        ));
        assert!(app.overlay.is_none());
    }

    #[test]
    fn ctrl_c_confirms_then_quits_while_overlay_open_and_running() {
        let mut app = app();
        app.views[0].status = AgentStatus::Running;
        app.overlay = Some(Overlay::Message {
            title: "t".into(),
            body: "b".into(),
        });
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            KeyAction::None
        ));
        assert!(app.confirm_quit);
        assert!(app.overlay.is_none());
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
        assert_eq!(state.route, "p/m [default]");
    }
}
