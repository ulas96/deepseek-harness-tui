//! Markdown rendering: pulldown-cmark via tui-markdown - a REAL parser
//! (the archived TUI notes forbid regex-based fallback rendering). Streaming
//! assistant text renders plain until the step commits, then re-renders as
//! markdown.

use ratatui::text::Text;

/// Parse committed assistant markdown into styled ratatui text.
pub fn markdown_text(text: &str) -> Text<'_> {
    tui_markdown::from_str(text)
}
