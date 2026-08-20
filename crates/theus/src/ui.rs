//! ratatui rendering: transcript pane (streaming markdown, tool-call cards
//! with elapsed timing, diff cards, subagent cards, todo snapshots), prompt
//! status line, and the input pane. Rendering is a pure function of UiState,
//! so TestBackend frame snapshots cover it without a terminal.

use std::time::Duration;

use ratatui::prelude::*;
use ratatui::widgets::*;
use unicode_width::UnicodeWidthStr;

use crate::eventmap::{SubagentState, ToolState, UiItem};
use crate::markdown::markdown_text;

/// Owned presentation of an interactive command picker/form.
pub struct OverlayView {
    pub title: String,
    pub lines: Vec<String>,
    pub selected: Option<usize>,
    pub hint: String,
    /// Text-entry caret as (index into `lines`, column within that line).
    /// `None` for overlays with no free-text field (pickers, confirmations).
    pub cursor: Option<(usize, usize)>,
}

/// Everything the renderer needs, borrowed from the app state.
pub struct UiState<'a> {
    pub items: &'a [UiItem],
    pub status: &'a str,
    pub session_id: &'a str,
    pub route: String,
    pub cwd: &'a str,
    pub branch: Option<&'a str>,
    pub input_lines: &'a [String],
    pub cursor: (u16, u16),
    /// Scroll offset from the bottom (0 = pinned to the newest line).
    pub scroll: usize,
    pub queued: bool,
    pub confirm_quit: bool,
    pub error: Option<&'a str>,
    pub violations: usize,
    pub elapsed: Duration,
    pub overlay: Option<OverlayView>,
}

/// Render the whole frame.
pub fn render(frame: &mut Frame, state: &UiState<'_>) {
    let area = frame.area();
    let [transcript_area, status_area, input_area] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(3),
    ])
    .areas(area);

    // Transcript ----------------------------------------------------------
    let width = transcript_area.width.saturating_sub(2) as usize;
    let mut visual: Vec<Line<'static>> = Vec::new();
    for item in state.items {
        item_lines(item, &mut visual, width);
    }
    if let Some(error) = state.error {
        visual.push(Line::from(Span::styled(
            format!("error: {error}"),
            Style::default().fg(Color::Red).bold(),
        )));
    }
    for _ in 0..state.violations {
        visual.push(Line::from(Span::styled(
            "stdout violation: a runtime stdout line was not JSON (stdout is reserved for JSON-RPC)",
            Style::default().fg(Color::Red),
        )));
    }
    let total = visual.len();
    let height = transcript_area.height.saturating_sub(2) as usize;
    let max_scroll = total.saturating_sub(height);
    let scroll = state.scroll.min(max_scroll);
    let start = total.saturating_sub(height + scroll);
    let end = total.saturating_sub(scroll);
    let window: Vec<Line<'static>> = visual[start..end.min(total)].to_vec();
    let transcript = Paragraph::new(window)
        .block(
            Block::bordered()
                .border_style(Style::default().fg(Color::DarkGray))
                .title(" theus "),
        )
        .style(Style::default().fg(Color::Reset));
    frame.render_widget(transcript, transcript_area);

    // Status bar ----------------------------------------------------------
    let branch = state.branch.map(|b| format!("@{b}")).unwrap_or_default();
    let queued = if state.queued { " [+1 queued]" } else { "" };
    let confirm = if state.confirm_quit {
        " | press ^C again to kill the runtime and quit"
    } else {
        ""
    };
    let status_text = format!(
        " {} | {} | {} | {}{} | {:.1}s{} | Tab: sessions | Enter: prompt | ^C: quit{}",
        state.session_id,
        state.status,
        state.route,
        state.cwd,
        branch,
        state.elapsed.as_secs_f64(),
        queued,
        confirm,
    );
    let status =
        Paragraph::new(status_text).style(Style::default().fg(Color::Black).bg(Color::Cyan));
    frame.render_widget(status, status_area);

    // Input pane ----------------------------------------------------------
    let mut input_lines: Vec<Line<'static>> = state
        .input_lines
        .iter()
        .map(|line| Line::from(Span::raw(line.clone())))
        .collect();
    if input_lines.is_empty() {
        input_lines.push(Line::from(Span::raw("")));
    }
    let input = Paragraph::new(input_lines).block(
        Block::bordered()
            .border_style(Style::default().fg(Color::DarkGray))
            .title(" prompt "),
    );
    frame.render_widget(input, input_area);
    if let Some(overlay) = &state.overlay {
        if let Some(cursor) = render_overlay(frame, area, overlay) {
            frame.set_cursor_position(cursor);
        }
    } else {
        frame.set_cursor_position(Position {
            x: input_area.x + 1 + state.cursor.0,
            y: input_area.y + 1 + state.cursor.1,
        });
    }
}

/// Render a picker/form popup, windowed around the current selection so it
/// never scrolls off-screen with no feedback. Returns the terminal cursor
/// position for the overlay's text-entry caret, if it has one and it's
/// currently within the visible window.
fn render_overlay(frame: &mut Frame, area: Rect, overlay: &OverlayView) -> Option<Position> {
    let width = area.width.saturating_mul(4) / 5;
    let content_height = overlay.lines.len().saturating_add(3) as u16;
    let height = content_height.min(area.height.saturating_sub(2)).max(5);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);

    // Inner rows available for `overlay.lines`: the block's inner height
    // (popup height minus its top/bottom border) minus one row for the hint.
    let inner_height = height.saturating_sub(2) as usize;
    let visible = inner_height.saturating_sub(1).max(1);
    let total = overlay.lines.len();
    let start = if total <= visible {
        0
    } else {
        let selected = overlay.selected.unwrap_or(0);
        selected
            .saturating_sub(visible.saturating_sub(1))
            .min(total - visible)
    };
    let end = (start + visible).min(total);

    let mut lines = Vec::with_capacity(end - start + 1);
    for (offset, line) in overlay.lines[start..end].iter().enumerate() {
        let index = start + offset;
        let style = if overlay.selected == Some(index) {
            Style::default().fg(Color::Black).bg(Color::Cyan).bold()
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(line.clone(), style)));
    }
    let hint = if total > visible {
        format!("{} · {}-{} of {total}", overlay.hint, start + 1, end)
    } else {
        overlay.hint.clone()
    };
    lines.push(Line::from(Span::styled(
        hint,
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(format!(" {} ", overlay.title)))
            .wrap(Wrap { trim: false }),
        popup,
    );

    overlay.cursor.and_then(|(row, col)| {
        (start..end).contains(&row).then(|| Position {
            x: popup.x + 1 + col as u16,
            y: popup.y + 1 + (row - start) as u16,
        })
    })
}

/// Append the wrapped, styled lines of one transcript item.
fn item_lines(item: &UiItem, out: &mut Vec<Line<'static>>, width: usize) {
    match item {
        UiItem::UserPrompt { text } => {
            out.push(Line::from(Span::styled(
                "you",
                Style::default().fg(Color::Cyan).bold(),
            )));
            push_wrapped(out, width, text, Style::default().fg(Color::Cyan));
        }
        UiItem::Context { summary } => {
            push_wrapped(
                out,
                width,
                summary,
                Style::default().fg(Color::DarkGray).italic(),
            );
        }
        UiItem::TurnStart { turn } => {
            let bar = format!("--- turn {turn} ");
            let fill = width.saturating_sub(bar.width()).max(0);
            let line = format!("{}{}", bar, "-".repeat(fill));
            out.push(Line::from(Span::styled(
                line,
                Style::default().fg(Color::Blue).bold(),
            )));
        }
        UiItem::TurnEnd { turn, reason } => {
            push_wrapped(
                out,
                width,
                &format!("(turn {turn} ended: {reason})"),
                Style::default().fg(Color::DarkGray),
            );
        }
        UiItem::Assistant {
            text,
            streaming,
            usage,
            ..
        } => {
            if *streaming {
                push_wrapped(out, width, text, Style::default().fg(Color::Yellow));
                if let Some(last) = out.last_mut() {
                    last.push_span(Span::styled("\u{258c}", Style::default().fg(Color::Yellow)));
                }
            } else {
                let markdown = markdown_text(text);
                for line in markdown.lines {
                    let mut owned = Line::from(Span::styled("  ", Style::default()));
                    for span in line.spans {
                        owned
                            .push_span(Span::styled(span.content.clone().into_owned(), span.style));
                    }
                    push_wrapped_line(out, width, owned);
                }
                if let Some(usage) = usage {
                    push_wrapped(
                        out,
                        width,
                        &format!(
                            "(in={} out={} tokens)",
                            usage.input_tokens, usage.output_tokens
                        ),
                        Style::default().fg(Color::DarkGray),
                    );
                }
            }
        }
        UiItem::ToolCall {
            name,
            arguments,
            state,
            ..
        } => {
            let (style, label) = match state {
                ToolState::Running => (
                    Style::default().fg(Color::Yellow),
                    format!("[tool] {name} - running"),
                ),
                ToolState::Completed {
                    elapsed_ms,
                    is_error,
                } => {
                    if *is_error {
                        (
                            Style::default().fg(Color::Red),
                            format!("[tool] {name} - failed ({elapsed_ms}ms)"),
                        )
                    } else {
                        (
                            Style::default().fg(Color::Green),
                            format!("[tool] {name} - done ({elapsed_ms}ms)"),
                        )
                    }
                }
            };
            out.push(Line::from(Span::styled(label, style.bold())));
            if !arguments.is_empty() {
                let compact: String = arguments.chars().take(160).collect();
                push_wrapped(
                    out,
                    width,
                    &format!("  args: {compact}"),
                    Style::default().fg(Color::DarkGray),
                );
            }
        }
        UiItem::Diff { path, old, new } => {
            out.push(Line::from(Span::styled(
                format!("[diff] {path}"),
                Style::default().fg(Color::Magenta).bold(),
            )));
            if let Some(old) = old {
                for line in old.lines() {
                    push_wrapped(
                        out,
                        width,
                        &format!("- {line}"),
                        Style::default().fg(Color::Red),
                    );
                }
            }
            for line in new.lines() {
                push_wrapped(
                    out,
                    width,
                    &format!("+ {line}"),
                    Style::default().fg(Color::Green),
                );
            }
        }
        UiItem::Subagent { child, state } => {
            let (style, label) = match state {
                SubagentState::Running => (
                    Style::default().fg(Color::Yellow),
                    format!("[subagent] {child} - running"),
                ),
                SubagentState::Finished {
                    status,
                    stop_reason,
                    ..
                } => {
                    let style = if status == "ok" {
                        Color::Green
                    } else {
                        Color::Red
                    };
                    (
                        Style::default().fg(style),
                        format!("[subagent] {child} - {status}/{stop_reason}"),
                    )
                }
            };
            out.push(Line::from(Span::styled(label, style.bold())));
            if let SubagentState::Finished { answer, .. } = state {
                if !answer.is_empty() {
                    push_wrapped(
                        out,
                        width,
                        &format!("  {answer}"),
                        Style::default().fg(Color::DarkGray),
                    );
                }
            }
        }
        UiItem::Todo { items } => {
            for item in items {
                let (mark, style) = match item.status.as_str() {
                    "completed" => ("[x]", Color::Green),
                    "in_progress" => ("[>]", Color::Yellow),
                    _ => ("[ ]", Color::DarkGray),
                };
                push_wrapped(
                    out,
                    width,
                    &format!("{mark} {}", item.content),
                    Style::default().fg(style),
                );
            }
        }
    }
}

/// Wrap one styled string at the given width, appending to out.
fn push_wrapped(out: &mut Vec<Line<'static>>, width: usize, text: &str, style: Style) {
    let wrap = textwrap_wrap(text, width);
    for chunk in wrap {
        out.push(Line::from(Span::styled(chunk, style)));
    }
}

/// Wrap an already-styled line (markdown output) at the given width.
fn push_wrapped_line(out: &mut Vec<Line<'static>>, width: usize, line: Line<'static>) {
    if line.width() <= width {
        out.push(line);
        return;
    }
    let mut current: Line<'static> = Line::default();
    let mut current_width = 0usize;
    for span in line.spans {
        let mut text = span.content.clone().into_owned();
        while !text.is_empty() {
            let remaining = width.saturating_sub(current_width).max(1);
            let (chunk, rest) = split_at_width(&text, remaining);
            let chunk_width = chunk.width();
            let mut copy = span.clone();
            copy.content = chunk.into();
            current.push_span(copy);
            current_width += chunk_width;
            text = rest;
            if current_width >= width {
                out.push(std::mem::take(&mut current));
                current_width = 0;
            }
        }
    }
    if !current.spans.is_empty() {
        out.push(current);
    }
}

/// A conservative width-aware wrapper: hard-wraps at spaces when possible.
fn textwrap_wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    for raw_line in text.split('\n') {
        if raw_line.width() <= width {
            out.push(raw_line.to_string());
            continue;
        }
        let mut current = String::new();
        for word in raw_line.split(' ') {
            let candidate = if current.is_empty() {
                word.to_string()
            } else {
                format!("{current} {word}")
            };
            if candidate.width() <= width {
                current = candidate;
            } else if current.is_empty() {
                let (chunk, rest) = split_at_width(word, width);
                out.push(chunk);
                current = rest;
            } else {
                out.push(std::mem::take(&mut current));
                current = word.to_string();
            }
        }
        if !current.is_empty() {
            out.push(current);
        }
    }
    out
}

/// Split at a display-width boundary (character-safe).
fn split_at_width(text: &str, width: usize) -> (String, String) {
    let mut taken = String::new();
    let mut used = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.peek() {
        let w = ch.to_string().width();
        if used + w > width {
            break;
        }
        taken.push(*ch);
        used += w;
        chars.next();
    }
    let rest: String = chars.collect();
    (taken, rest)
}
