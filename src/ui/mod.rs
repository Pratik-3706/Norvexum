// ===========================================================================
// TUI — terminal front-end (ratatui + crossterm)
//
// Rendering is event-driven: the screen is redrawn only when something
// changed (capped at ~30 fps while streaming), each message's wrapped lines
// are cached, and only the visible rows are materialized.
// ===========================================================================

pub mod theme;

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
        Wrap,
    },
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::agent::AgentEvent;
use theme::Theme;

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Minimum time between redraws while events stream in.
const FRAME: Duration = Duration::from_millis(33);
const SPINNER_TICK: Duration = Duration::from_millis(90);
const MAX_INPUT_ROWS: u16 = 6;
const NOTICE_DURATION: Duration = Duration::from_secs(3);
/// Keys arriving closer together than this are a paste, not typing. Terminals
/// without bracketed paste (Windows) deliver pasted text as plain key events.
const PASTE_GAP: Duration = Duration::from_millis(12);

/// Slash commands offered as completions (see `/help`).
const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/help", "show all commands"),
    ("/clear", "clear chat and memory"),
    ("/compact", "summarize older messages"),
    ("/copy", "copy last answer"),
    ("/copy chat", "copy whole chat"),
    ("/undo", "restore files changed by the last edit"),
    ("/model", "list or switch models"),
    ("/provider", "list or switch providers"),
    ("/session resume", "continue the last session"),
    ("/session list", "list saved sessions"),
    ("/loops", "set the tool-loop limit"),
    ("/ask_perms", "toggle tool approval prompts"),
    ("/exit", "quit"),
];

/// Every key binding, shown by the F1 help overlay.
const KEY_HELP: &[(&str, &str)] = &[
    ("Enter", "send (queues while a run is active)"),
    ("Alt+Enter / Ctrl+J", "new line"),
    ("Up / Down", "move between lines, then browse history"),
    ("Tab", "complete a / command, or switch to Activity"),
    ("PgUp / PgDn", "scroll the chat"),
    ("Shift+Up / Shift+Down", "scroll one line"),
    ("Ctrl+Home / Ctrl+End", "jump to top / latest"),
    ("Alt+Up / Alt+Down", "jump to the previous / next message"),
    ("Mouse drag", "select text (copied on release)"),
    ("Click in the box", "move the cursor"),
    ("Right click", "paste"),
    ("Esc", "stop the run, clear selection or input"),
    ("Ctrl+T", "expand or collapse reasoning"),
    ("Ctrl+A", "copy the whole chat"),
    ("c (Activity)", "copy the selected tool's output"),
    ("Ctrl+V", "paste"),
    ("Ctrl+W / Ctrl+U", "delete word / to line start"),
    ("Ctrl+L", "clear the input"),
    ("Ctrl+C", "copy selection, clear, stop, or quit"),
    ("Ctrl+D", "quit (empty input)"),
    ("F1 or ?", "toggle this help"),
];

pub struct App {
    pub chat_lines: Vec<ChatLine>,
    pub thinking_text: String,
    pub is_thinking: bool,
    pub tool_log: Vec<ToolLogEntry>,
    pub input: String,
    /// Cursor position in Unicode scalar values, never raw bytes.
    pub cursor_pos: usize,
    /// Lines scrolled up from the bottom of the chat.
    pub chat_scroll: usize,
    pub status: String,
    pub model_info: String,
    pub should_quit: bool,
    pub is_processing: bool,
    pub theme: Theme,
    pub input_history: Vec<String>,
    pub history_pos: Option<usize>,
    // ── Token usage tracking ─────────────────────────────────────────────
    pub total_tokens_used: u64,
    pub prompt_tokens_used: u64,
    pub completion_tokens_used: u64,
    // ── Text selection state (screen coordinates) ────────────────────────
    pub selection_start: Option<(u16, u16)>,
    pub selection_end: Option<(u16, u16)>,
    pub is_selecting: bool,
    // ── Approval modal ───────────────────────────────────────────────────
    pub approval_pending: Option<ApprovalInfo>,
    // ── Interactive Activity panel state ─────────────────────────────────
    pub active_panel: ActivePanel,
    pub selected_tool_index: usize,
    pub show_tool_details: bool,
    pub tool_details_scroll: u16,
    pub active_skill: Option<String>,
    pub project_root: std::path::PathBuf,
    pub user_backlog: VecDeque<String>,
    /// Show full reasoning text instead of a two-line preview.
    pub show_reasoning: bool,
    /// Key binding overlay (F1).
    pub show_help: bool,
    run_started: Option<Instant>,
    spinner_frame: usize,
    notice: Option<(String, Instant)>,
    cancel_requested: bool,
    quit_armed_at: Option<Instant>,
    history_draft: String,
    next_rev: u64,
    render_cache: RefCell<Vec<Option<CachedRender>>>,
    // Geometry from the last draw, used for mouse hit-testing and copy.
    chat_area: Cell<Rect>,
    tools_area: Cell<Rect>,
    /// Input box (with border) and its text area, plus the first visible row.
    input_box: Cell<Rect>,
    input_area: Cell<Rect>,
    input_scroll: Cell<usize>,
    max_scroll: Cell<usize>,
    total_lines: Cell<usize>,
    view_height: Cell<usize>,
    /// First line of every user/assistant message, for Alt+Up/Down.
    message_starts: RefCell<Vec<usize>>,
    visible_plain: RefCell<Vec<String>>,
    visible_meta: RefCell<Vec<RowMeta>>,
}

struct CachedRender {
    rev: u64,
    width: usize,
    expanded: bool,
    lines: Vec<Line<'static>>,
    meta: Vec<RowMeta>,
}

/// How a rendered row joins the row above it when copied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Join {
    /// A real line break in the source.
    #[default]
    NewLine,
    /// A soft wrap that replaced a space.
    Space,
    /// A soft wrap inside a word or code line.
    Glued,
}

/// Copy metadata for one rendered row, so copied text matches the source
/// instead of the screen (no wrap breaks, gutters or indentation).
#[derive(Debug, Clone, Default)]
struct RowMeta {
    /// Display column where the text starts.
    content_col: usize,
    join: Join,
    /// Text copied in place of the rendered row (code fences, rules).
    copy_as: Option<String>,
}

impl RowMeta {
    fn text(content_col: usize, join: Join) -> Self {
        Self {
            content_col,
            join,
            copy_as: None,
        }
    }

    fn copy_as(text: impl Into<String>) -> Self {
        Self {
            copy_as: Some(text.into()),
            ..Self::default()
        }
    }
}

/// Rendered lines with their copy metadata.
#[derive(Default)]
struct Rows {
    lines: Vec<Line<'static>>,
    meta: Vec<RowMeta>,
}

impl Rows {
    fn push(&mut self, line: Line<'static>, meta: RowMeta) {
        self.lines.push(line);
        self.meta.push(meta);
    }

    fn append(&mut self, mut other: Rows) {
        self.lines.append(&mut other.lines);
        self.meta.append(&mut other.meta);
    }

    fn truncate(&mut self, len: usize) {
        self.lines.truncate(len);
        self.meta.truncate(len);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivePanel {
    Chat,
    Activity,
}

#[derive(Debug, Clone)]
pub struct ApprovalInfo {
    pub id: String,
    pub tool_name: String,
    pub args_preview: String,
}

#[derive(Debug, Clone)]
pub struct ChatLine {
    pub content: String,
    pub style: LineStyle,
    pub tool_call_id: Option<String>,
    rev: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineStyle {
    User,
    Assistant,
    Thinking,
    Error,
    Warning,
    FileWrite,
    Status,
}

#[derive(Debug, Clone)]
pub struct ToolLogEntry {
    pub id: String,
    pub name: String,
    pub status: ToolStatus,
    /// Raw (possibly still streaming) JSON arguments.
    pub args: String,
    pub output: String,
    pub started: Instant,
    pub duration: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Success,
    Failed,
}

fn is_file_tool(name: &str) -> bool {
    matches!(
        crate::tools::canonical_tool_name(name),
        "write_file" | "edit_file" | "multi_edit"
    )
}

impl App {
    pub fn new(model_info: &str, project_root: std::path::PathBuf) -> Self {
        let mut app = Self {
            chat_lines: Vec::new(),
            thinking_text: String::new(),
            is_thinking: false,
            tool_log: Vec::new(),
            input: String::new(),
            cursor_pos: 0,
            chat_scroll: 0,
            status: "Ready".into(),
            model_info: model_info.to_string(),
            should_quit: false,
            is_processing: false,
            theme: Theme::default(),
            input_history: Vec::new(),
            history_pos: None,
            total_tokens_used: 0,
            prompt_tokens_used: 0,
            completion_tokens_used: 0,
            selection_start: None,
            selection_end: None,
            is_selecting: false,
            approval_pending: None,
            active_panel: ActivePanel::Chat,
            selected_tool_index: 0,
            show_tool_details: false,
            tool_details_scroll: 0,
            active_skill: None,
            project_root,
            user_backlog: VecDeque::new(),
            show_reasoning: false,
            show_help: false,
            run_started: None,
            spinner_frame: 0,
            notice: None,
            cancel_requested: false,
            quit_armed_at: None,
            history_draft: String::new(),
            next_rev: 0,
            render_cache: RefCell::new(Vec::new()),
            chat_area: Cell::new(Rect::default()),
            tools_area: Cell::new(Rect::default()),
            input_box: Cell::new(Rect::default()),
            input_area: Cell::new(Rect::default()),
            input_scroll: Cell::new(0),
            max_scroll: Cell::new(0),
            total_lines: Cell::new(0),
            view_height: Cell::new(0),
            message_starts: RefCell::new(Vec::new()),
            visible_plain: RefCell::new(Vec::new()),
            visible_meta: RefCell::new(Vec::new()),
        };
        app.push_line(
            format!(
                "Working in `{}`. Ask a question, request a change, or type `/` for commands.",
                app.project_root.display()
            ),
            LineStyle::Status,
            None,
        );
        app
    }

    fn push_line(&mut self, content: String, style: LineStyle, tool_call_id: Option<String>) {
        self.next_rev += 1;
        self.chat_lines.push(ChatLine {
            content,
            style,
            tool_call_id,
            rev: self.next_rev,
        });
        self.auto_scroll();
    }

    /// Mutable access to a chat line that invalidates its render cache.
    fn line_mut(&mut self, idx: usize) -> &mut ChatLine {
        self.next_rev += 1;
        let line = &mut self.chat_lines[idx];
        line.rev = self.next_rev;
        line
    }

    fn notify(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), Instant::now()));
    }

    pub fn add_user_message(&mut self, message: String) {
        self.push_line(message, LineStyle::User, None);
        self.start_run();
    }

    fn start_run(&mut self) {
        self.is_processing = true;
        self.run_started = Some(Instant::now());
        self.status = "Starting".into();
    }

    pub fn handle_agent_event(&mut self, event: AgentEvent) {
        if !self.is_selecting && self.selection_start.is_some() {
            self.clear_selection();
        }
        match event {
            AgentEvent::Thinking(text) => {
                self.status = "Thinking".into();
                let streaming_line = self.chat_lines.len().checked_sub(1).filter(|&i| {
                    self.is_thinking && self.chat_lines[i].style == LineStyle::Thinking
                });
                match streaming_line {
                    Some(idx) => self.line_mut(idx).content.push_str(&text),
                    None if text.trim().is_empty() => return,
                    None => {
                        self.push_line(text.trim_start().to_string(), LineStyle::Thinking, None)
                    }
                }
                self.is_thinking = true;
                self.auto_scroll();
            }
            AgentEvent::ThinkingDone => {
                if let Some(idx) = self.chat_lines.len().checked_sub(1) {
                    if self.is_thinking && self.chat_lines[idx].style == LineStyle::Thinking {
                        let trimmed = self.chat_lines[idx].content.trim_end().to_string();
                        self.line_mut(idx).content = trimmed;
                    }
                }
                self.is_thinking = false;
            }
            AgentEvent::Content(text) => {
                if let Some(idx) = self.chat_lines.len().checked_sub(1) {
                    if self.chat_lines[idx].style == LineStyle::Assistant {
                        self.line_mut(idx).content.push_str(&text);
                        self.auto_scroll();
                        return;
                    }
                }
                if !text.trim().is_empty() {
                    self.push_line(text.trim_start().to_string(), LineStyle::Assistant, None);
                }
            }
            AgentEvent::ToolStart { name, id } => {
                self.status = format!("Running {name}");
                self.tool_log.push(ToolLogEntry {
                    id: id.clone(),
                    name: name.clone(),
                    status: ToolStatus::Running,
                    args: String::new(),
                    output: String::new(),
                    started: Instant::now(),
                    duration: None,
                });
                if is_file_tool(&name) {
                    self.push_line(
                        "Preparing file change…".into(),
                        LineStyle::FileWrite,
                        Some(id),
                    );
                }
            }
            AgentEvent::ToolArgsDelta { id, delta } => {
                let Some(entry) = self.tool_log.iter_mut().rev().find(|e| e.id == id) else {
                    return;
                };
                entry.args.push_str(&delta);
                if !is_file_tool(&entry.name) {
                    return;
                }
                let path = extract_streaming_content(&entry.args, "path");
                let key = if crate::tools::canonical_tool_name(&entry.name) == "write_file" {
                    "content"
                } else {
                    "replacement"
                };
                let body = extract_streaming_content(&entry.args, key);
                // Only the tail is shown while streaming; the full file is in the activity log.
                let tail: Vec<&str> = body.lines().collect();
                let shown = tail[tail.len().saturating_sub(12)..].join("\n");
                let content = format!(
                    "Writing `{}`…\n```\n{}\n```",
                    if path.is_empty() { "…" } else { &path },
                    shown
                );
                if let Some(idx) = self
                    .chat_lines
                    .iter()
                    .rposition(|l| l.tool_call_id.as_deref() == Some(&id))
                {
                    self.line_mut(idx).content = content;
                }
            }
            AgentEvent::ToolResult {
                id,
                name,
                result,
                success,
            } => {
                let status = if success {
                    ToolStatus::Success
                } else {
                    ToolStatus::Failed
                };
                match self.tool_log.iter_mut().rev().find(|e| e.id == id) {
                    Some(entry) => {
                        entry.status = status;
                        entry.output = result.clone();
                        entry.duration = Some(entry.started.elapsed());
                    }
                    None => self.tool_log.push(ToolLogEntry {
                        id: id.clone(),
                        name: name.clone(),
                        status,
                        args: String::new(),
                        output: result.clone(),
                        started: Instant::now(),
                        duration: Some(Duration::ZERO),
                    }),
                }
                if is_file_tool(&name) {
                    if let Some(idx) = self
                        .chat_lines
                        .iter()
                        .rposition(|l| l.tool_call_id.as_deref() == Some(&id))
                    {
                        let first = result.lines().next().unwrap_or_default().to_string();
                        self.line_mut(idx).content = if success {
                            first
                        } else {
                            format!("✗ {}", compact_detail(&result, 200))
                        };
                    }
                }
                self.status = format!("{name} {}", if success { "done" } else { "failed" });
            }
            AgentEvent::FileWrite { .. } => {
                // Already reflected by the tool's chat line.
            }
            AgentEvent::Done { usage } => {
                self.is_processing = false;
                self.is_thinking = false;
                self.thinking_text.clear();
                self.active_skill = None;
                let elapsed = self.run_started.take().map(|t| t.elapsed());
                if let Some(ref u) = usage {
                    self.total_tokens_used += u.total_tokens as u64;
                    self.prompt_tokens_used += u.prompt_tokens as u64;
                    self.completion_tokens_used += u.completion_tokens as u64;
                }
                self.status = match (usage, elapsed) {
                    (Some(u), Some(t)) if u.total_tokens > 0 => format!(
                        "Done in {} · {} tokens",
                        format_duration(t),
                        format_tokens(u.total_tokens as u64)
                    ),
                    (_, Some(t)) => format!("Done in {}", format_duration(t)),
                    _ => "Ready".into(),
                };
            }
            AgentEvent::Error(error) => {
                self.push_line(error, LineStyle::Error, None);
                self.status = "Error".into();
            }
            AgentEvent::Warning(warning) => self.push_line(warning, LineStyle::Warning, None),
            AgentEvent::Status(status) => self.status = clean_status(&status),
            AgentEvent::ModelSwitched { model, provider } => {
                self.model_info = format!("{model} / {provider}");
            }
            AgentEvent::ClearChat => {
                self.chat_lines.clear();
                self.tool_log.clear();
                self.selected_tool_index = 0;
                self.render_cache.borrow_mut().clear();
                self.chat_scroll = 0;
                self.notify("Chat cleared");
            }
            AgentEvent::Quit => self.should_quit = true,
            AgentEvent::ApprovalRequest {
                id,
                tool_name,
                args_preview,
            } => {
                self.approval_pending = Some(ApprovalInfo {
                    id,
                    tool_name,
                    args_preview,
                });
            }
            AgentEvent::TriggeredSkill(name) => self.active_skill = Some(name),
        }
    }

    fn auto_scroll(&mut self) {
        self.chat_scroll = 0;
    }

    fn scroll_chat(&mut self, delta: isize) {
        let max = self.max_scroll.get();
        self.chat_scroll = (self.chat_scroll as isize + delta).clamp(0, max as isize) as usize;
    }

    fn clear_selection(&mut self) {
        self.selection_start = None;
        self.selection_end = None;
        self.is_selecting = false;
    }

    /// The selected chat text as it appears in the source message: soft
    /// wraps are rejoined and gutters/indentation are left out.
    fn selection_text(&self) -> String {
        let (Some(start), Some(end)) = (self.selection_start, self.selection_end) else {
            return String::new();
        };
        let area = self.chat_area.get();
        let (p1, p2) = if (start.1, start.0) <= (end.1, end.0) {
            (start, end)
        } else {
            (end, start)
        };
        let visible = self.visible_plain.borrow();
        let meta = self.visible_meta.borrow();
        if visible.is_empty() {
            return String::new();
        }
        let row = |y: u16| (y.saturating_sub(area.y) as usize).min(visible.len() - 1);
        let col = |x: u16| x.saturating_sub(area.x) as usize;
        let (r1, r2) = (row(p1.1), row(p2.1));
        let mut out = String::new();
        for idx in r1..=r2 {
            let m = meta.get(idx).cloned().unwrap_or_default();
            if idx > r1 {
                match m.join {
                    Join::NewLine => out.push('\n'),
                    Join::Space => out.push(' '),
                    Join::Glued => {}
                }
            }
            match m.copy_as {
                Some(text) => out.push_str(&text),
                None => {
                    let from = if idx == r1 {
                        col(p1.0).max(m.content_col)
                    } else {
                        m.content_col
                    };
                    let to = if idx == r2 { col(p2.0) + 1 } else { usize::MAX };
                    out.push_str(slice_columns(&visible[idx], from, to).trim_end());
                }
            }
        }
        out.trim_matches('\n').trim_end().to_string()
    }

    /// Copy the selected chat text to the clipboard. Returns true if something
    /// was copied. With `clear`, the highlight is removed afterwards.
    pub fn copy_selection(&mut self, clear: bool) -> bool {
        let text = self.selection_text();
        if clear {
            self.clear_selection();
        }
        if text.is_empty() {
            return false;
        }
        match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text)) {
            Ok(()) => {
                self.notify("Copied — Esc clears the highlight");
                true
            }
            Err(_) => {
                self.notify("Clipboard unavailable");
                false
            }
        }
    }

    fn copy_tool_output(&mut self) {
        let Some(entry) = self.tool_log.iter().rev().nth(self.selected_tool_index) else {
            return;
        };
        let text = clean_paths(&entry.output, &self.project_root);
        match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text)) {
            Ok(()) => self.notify(format!("Copied {} output", entry.name)),
            Err(_) => self.notify("Clipboard unavailable"),
        }
    }

    /// A key that arrived as part of a paste burst. Returns false for keys
    /// that should keep their normal meaning.
    fn paste_key(&mut self, key: KeyCode, modifiers: KeyModifiers) -> bool {
        if modifiers.contains(KeyModifiers::CONTROL) && !modifiers.contains(KeyModifiers::ALT) {
            return false;
        }
        match key {
            KeyCode::Enter => self.insert_text("\n"),
            KeyCode::Tab => self.insert_text("    "),
            KeyCode::Char(c) => self.insert_text(&c.to_string()),
            _ => return false,
        }
        self.active_panel = ActivePanel::Chat;
        self.history_pos = None;
        self.clear_selection();
        true
    }

    /// Place the input cursor under a mouse click.
    fn click_input(&mut self, column: u16, row: u16) {
        let area = self.input_area.get();
        if area.width == 0 || area.height == 0 {
            return;
        }
        let row = row.clamp(area.y, area.y + area.height - 1);
        let column = column.clamp(area.x, area.x + area.width - 1);
        let target_row = (row - area.y) as usize + self.input_scroll.get();
        let target_col = (column - area.x) as usize;
        self.cursor_pos = cursor_at(&self.input, area.width as usize, target_row, target_col);
        self.active_panel = ActivePanel::Chat;
        self.clear_selection();
    }

    /// Scroll so the previous (dir < 0) or next message starts at the top.
    fn jump_message(&mut self, dir: isize) {
        let starts = self.message_starts.borrow().clone();
        let total = self.total_lines.get();
        let view = self.view_height.get();
        let max = self.max_scroll.get();
        let top = total.saturating_sub(view + self.chat_scroll.min(max));
        let target = if dir < 0 {
            starts
                .iter()
                .rev()
                .find(|&&s| s < top)
                .copied()
                .unwrap_or(0)
        } else {
            match starts.iter().find(|&&s| s > top) {
                Some(&s) => s,
                None => {
                    self.chat_scroll = 0;
                    return;
                }
            }
        };
        self.chat_scroll = total.saturating_sub(view + target).min(max);
    }

    fn copy_transcript(&mut self) {
        let text = self
            .chat_lines
            .iter()
            .filter(|l| !matches!(l.style, LineStyle::Status | LineStyle::Thinking))
            .map(|l| format!("{}:\n{}", label_for(l.style).1, l.content))
            .collect::<Vec<_>>()
            .join("\n\n");
        match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text)) {
            Ok(()) => self.notify("Copied the whole conversation"),
            Err(_) => self.notify("Clipboard unavailable"),
        }
    }

    /// Commands matching what's typed so far (only while typing the command word).
    fn command_suggestions(&self) -> Vec<(&'static str, &'static str)> {
        if !self.input.starts_with('/') || self.input.contains('\n') {
            return Vec::new();
        }
        let typed = self.input.as_str();
        SLASH_COMMANDS
            .iter()
            .copied()
            .filter(|(cmd, _)| cmd.starts_with(typed) && *cmd != typed)
            .collect()
    }

    pub fn handle_key(&mut self, key: KeyCode, modifiers: KeyModifiers) -> Option<String> {
        let ctrl = modifiers.contains(KeyModifiers::CONTROL);
        let alt = modifiers.contains(KeyModifiers::ALT);
        let shift = modifiers.contains(KeyModifiers::SHIFT);

        if key != KeyCode::Char('c') || !ctrl {
            self.quit_armed_at = None;
        }
        // Any key other than copy/Esc drops a finished selection highlight.
        if self.selection_start.is_some()
            && !self.is_selecting
            && !(ctrl && key == KeyCode::Char('c'))
            && key != KeyCode::Esc
        {
            self.clear_selection();
        }

        if self.show_help {
            if matches!(
                key,
                KeyCode::Esc | KeyCode::Enter | KeyCode::F(1) | KeyCode::Char('q' | '?')
            ) {
                self.show_help = false;
            }
            return None;
        }
        if key == KeyCode::F(1) || (key == KeyCode::Char('?') && self.input.is_empty()) {
            self.show_help = true;
            return None;
        }

        // Tool details overlay
        if self.show_tool_details {
            match key {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                    self.show_tool_details = false
                }
                KeyCode::Char('c') => self.copy_tool_output(),
                KeyCode::Up => {
                    self.tool_details_scroll = self.tool_details_scroll.saturating_sub(1)
                }
                KeyCode::Down => {
                    self.tool_details_scroll = self.tool_details_scroll.saturating_add(1)
                }
                KeyCode::PageUp => {
                    self.tool_details_scroll = self.tool_details_scroll.saturating_sub(10)
                }
                KeyCode::PageDown => {
                    self.tool_details_scroll = self.tool_details_scroll.saturating_add(10)
                }
                _ => {}
            }
            return None;
        }

        let completing = self.input.starts_with('/') && !self.command_suggestions().is_empty();
        if key == KeyCode::Tab && !completing {
            self.active_panel = match self.active_panel {
                ActivePanel::Chat => ActivePanel::Activity,
                ActivePanel::Activity => ActivePanel::Chat,
            };
            return None;
        }

        if self.active_panel == ActivePanel::Activity {
            let last = self.tool_log.len().saturating_sub(1);
            match key {
                KeyCode::Esc => self.active_panel = ActivePanel::Chat,
                KeyCode::Up => {
                    self.selected_tool_index = self.selected_tool_index.saturating_sub(1)
                }
                KeyCode::Down => {
                    self.selected_tool_index = (self.selected_tool_index + 1).min(last)
                }
                KeyCode::PageUp => {
                    self.selected_tool_index = self.selected_tool_index.saturating_sub(5)
                }
                KeyCode::PageDown => {
                    self.selected_tool_index = (self.selected_tool_index + 5).min(last)
                }
                KeyCode::Home => self.selected_tool_index = 0,
                KeyCode::End => self.selected_tool_index = last,
                KeyCode::Char('c') => self.copy_tool_output(),
                KeyCode::Enter if !self.tool_log.is_empty() => {
                    self.show_tool_details = true;
                    self.tool_details_scroll = 0;
                }
                _ => {}
            }
            return None;
        }

        match key {
            KeyCode::Char('c') if ctrl => {
                if self.selection_start.is_some() {
                    self.copy_selection(true);
                } else if !self.input.is_empty() {
                    self.set_input(String::new());
                } else if self.is_processing {
                    self.cancel_requested = true;
                    self.notify("Stopping… (Ctrl+C again to quit)");
                    self.quit_armed_at = Some(Instant::now());
                } else if self
                    .quit_armed_at
                    .is_some_and(|t| t.elapsed() < Duration::from_secs(2))
                {
                    self.should_quit = true;
                } else {
                    self.quit_armed_at = Some(Instant::now());
                    self.notify("Press Ctrl+C again to quit");
                }
            }
            KeyCode::Char('d') if ctrl && self.input.is_empty() => self.should_quit = true,
            KeyCode::Char('a') if ctrl => self.copy_transcript(),
            KeyCode::Char('t') if ctrl => {
                self.show_reasoning = !self.show_reasoning;
                self.notify(if self.show_reasoning {
                    "Showing full reasoning"
                } else {
                    "Reasoning collapsed"
                });
            }
            KeyCode::Char('v') if ctrl => {
                if let Ok(text) = arboard::Clipboard::new().and_then(|mut cb| cb.get_text()) {
                    self.insert_text(&text.replace("\r\n", "\n"));
                }
            }
            KeyCode::Char('l') if ctrl => self.set_input(String::new()),
            KeyCode::Char('u') if ctrl => {
                let line_start = self.input[..byte_index(&self.input, self.cursor_pos)]
                    .rfind('\n')
                    .map_or(0, |i| char_count(&self.input[..=i]));
                let (a, b) = (
                    byte_index(&self.input, line_start),
                    byte_index(&self.input, self.cursor_pos),
                );
                self.input.replace_range(a..b, "");
                self.cursor_pos = line_start;
            }
            KeyCode::Char('w') if ctrl => {
                let target = self.word_left();
                let (a, b) = (
                    byte_index(&self.input, target),
                    byte_index(&self.input, self.cursor_pos),
                );
                self.input.replace_range(a..b, "");
                self.cursor_pos = target;
            }
            KeyCode::Char('j') if ctrl => self.insert_text("\n"),
            KeyCode::Enter if alt || shift => self.insert_text("\n"),
            KeyCode::Enter => {
                let message = self.input.trim().to_string();
                if message.is_empty() {
                    return None;
                }
                if self.input_history.last() != Some(&message) {
                    self.input_history.push(message.clone());
                }
                self.history_pos = None;
                self.set_input(String::new());
                self.clear_selection();

                if self.is_processing {
                    self.push_line(message.clone(), LineStyle::User, None);
                    self.user_backlog.push_back(message);
                    self.notify(format!("Queued — {} waiting", self.user_backlog.len()));
                    return None;
                }
                self.add_user_message(message.clone());
                return Some(message);
            }
            KeyCode::Tab => {
                if let Some((cmd, _)) = self.command_suggestions().first() {
                    let completed = if cmd.contains(' ') {
                        cmd.to_string()
                    } else {
                        format!("{cmd} ")
                    };
                    self.set_input(completed);
                }
            }
            KeyCode::Esc => {
                if self.selection_start.is_some() {
                    self.clear_selection();
                } else if self.is_processing {
                    self.cancel_requested = true;
                } else if !self.input.is_empty() {
                    self.set_input(String::new());
                }
            }
            KeyCode::Char(c) if !ctrl || alt => self.insert_text(&c.to_string()),
            KeyCode::Backspace if self.cursor_pos > 0 => {
                let start = byte_index(&self.input, self.cursor_pos - 1);
                let end = byte_index(&self.input, self.cursor_pos);
                self.input.replace_range(start..end, "");
                self.cursor_pos -= 1;
            }
            KeyCode::Delete if self.cursor_pos < char_count(&self.input) => {
                let start = byte_index(&self.input, self.cursor_pos);
                let end = byte_index(&self.input, self.cursor_pos + 1);
                self.input.replace_range(start..end, "");
            }
            KeyCode::Left if ctrl => self.cursor_pos = self.word_left(),
            KeyCode::Right if ctrl => self.cursor_pos = self.word_right(),
            KeyCode::Left => self.cursor_pos = self.cursor_pos.saturating_sub(1),
            KeyCode::Right => self.cursor_pos = (self.cursor_pos + 1).min(char_count(&self.input)),
            KeyCode::Home if ctrl => self.chat_scroll = self.max_scroll.get(),
            KeyCode::End if ctrl => self.chat_scroll = 0,
            KeyCode::Home => self.cursor_pos = 0,
            KeyCode::End => self.cursor_pos = char_count(&self.input),
            KeyCode::Up if alt => self.jump_message(-1),
            KeyCode::Down if alt => self.jump_message(1),
            KeyCode::Up if shift => self.scroll_chat(1),
            KeyCode::Down if shift => self.scroll_chat(-1),
            KeyCode::Up => {
                if !self.move_cursor_line(-1) {
                    self.older_history();
                }
            }
            KeyCode::Down => {
                if !self.move_cursor_line(1) {
                    self.newer_history();
                }
            }
            KeyCode::PageUp => {
                let page = (self.chat_area.get().height as isize / 2).max(3);
                self.scroll_chat(page);
            }
            KeyCode::PageDown => {
                let page = (self.chat_area.get().height as isize / 2).max(3);
                self.scroll_chat(-page);
            }
            _ => {}
        }
        None
    }

    fn set_input(&mut self, text: String) {
        self.cursor_pos = char_count(&text);
        self.input = text;
    }

    fn insert_text(&mut self, text: &str) {
        let index = byte_index(&self.input, self.cursor_pos);
        self.input.insert_str(index, text);
        self.cursor_pos += char_count(text);
    }

    fn word_left(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.cursor_pos.min(chars.len());
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    fn word_right(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.cursor_pos.min(chars.len());
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        i
    }

    /// Move the cursor to the previous/next line of a multi-line input.
    /// Returns false when there is no such line (so Up/Down browse history).
    fn move_cursor_line(&mut self, dir: isize) -> bool {
        let lines: Vec<&str> = self.input.split('\n').collect();
        if lines.len() < 2 {
            return false;
        }
        let (mut row, mut col, mut seen) = (0usize, 0usize, 0usize);
        for (i, line) in lines.iter().enumerate() {
            let len = char_count(line);
            if self.cursor_pos <= seen + len {
                row = i;
                col = self.cursor_pos - seen;
                break;
            }
            seen += len + 1;
        }
        let target = row as isize + dir;
        if target < 0 || target as usize >= lines.len() {
            return false;
        }
        let target = target as usize;
        let start: usize = lines[..target].iter().map(|l| char_count(l) + 1).sum();
        self.cursor_pos = start + col.min(char_count(lines[target]));
        true
    }

    fn older_history(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        let pos = match self.history_pos {
            None => {
                self.history_draft = self.input.clone();
                self.input_history.len() - 1
            }
            Some(pos) => pos.saturating_sub(1),
        };
        self.history_pos = Some(pos);
        self.set_input(self.input_history[pos].clone());
    }

    fn newer_history(&mut self) {
        let Some(pos) = self.history_pos else {
            return;
        };
        if pos + 1 < self.input_history.len() {
            self.history_pos = Some(pos + 1);
            self.set_input(self.input_history[pos + 1].clone());
        } else {
            self.history_pos = None;
            let draft = std::mem::take(&mut self.history_draft);
            self.set_input(draft);
        }
    }

    /// Wrapped, styled lines for one message (cached by revision and width).
    fn message_lines(&self, idx: usize, width: usize) -> Vec<Line<'static>> {
        self.message_rows(idx, width).0
    }

    /// Lines plus per-row copy metadata for one message.
    fn message_rows(&self, idx: usize, width: usize) -> (Vec<Line<'static>>, Vec<RowMeta>) {
        let line = &self.chat_lines[idx];
        let expanded = self.show_reasoning;
        {
            let cache = self.render_cache.borrow();
            if let Some(Some(c)) = cache.get(idx) {
                if c.rev == line.rev
                    && c.width == width
                    && c.expanded == expanded
                    && !(self.is_thinking && idx + 1 == self.chat_lines.len())
                {
                    return (c.lines.clone(), c.meta.clone());
                }
            }
        }

        let (color, label) = label_for(line.style);
        let color = self.theme.color(color);
        let live = line.style == LineStyle::Thinking
            && self.is_thinking
            && idx + 1 == self.chat_lines.len();
        let label = if live {
            "∴ Reasoning…  (streaming)".to_string()
        } else {
            label.to_string()
        };
        let mut rows = Rows::default();
        rows.push(
            Line::from(Span::styled(
                label,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )),
            RowMeta::default(),
        );
        let body_color = match line.style {
            LineStyle::User => self.theme.fg,
            LineStyle::Assistant => self.theme.assistant_color,
            LineStyle::Status => self.theme.dim_color,
            _ => color,
        };
        let mut body = render_markdown(&line.content, width, body_color, line.style, &self.theme);
        if line.style == LineStyle::Thinking && !expanded && !live && body.lines.len() > 2 {
            let hidden = body.lines.len() - 2;
            body.truncate(2);
            body.push(
                Line::from(Span::styled(
                    format!("  … {hidden} more lines (Ctrl+T to expand)"),
                    Style::default().fg(self.theme.dim_color),
                )),
                RowMeta::copy_as(""),
            );
        }
        rows.append(body);
        rows.push(Line::default(), RowMeta::default());

        let mut cache = self.render_cache.borrow_mut();
        if cache.len() < self.chat_lines.len() {
            cache.resize_with(self.chat_lines.len(), || None);
        }
        cache[idx] = Some(CachedRender {
            rev: line.rev,
            width,
            expanded,
            lines: rows.lines.clone(),
            meta: rows.meta.clone(),
        });
        (rows.lines, rows.meta)
    }
}

fn label_for(style: LineStyle) -> (ThemeColor, &'static str) {
    match style {
        LineStyle::User => (ThemeColor::User, "› You"),
        LineStyle::Assistant => (ThemeColor::Accent, "◆ Norvexum"),
        LineStyle::Thinking => (ThemeColor::Thinking, "∴ Reasoning"),
        LineStyle::Error => (ThemeColor::Error, "✗ Error"),
        LineStyle::Warning => (ThemeColor::Tool, "! Notice"),
        LineStyle::FileWrite => (ThemeColor::File, "✎ File"),
        LineStyle::Status => (ThemeColor::Dim, "• Norvexum"),
    }
}

#[derive(Clone, Copy)]
enum ThemeColor {
    User,
    Accent,
    Thinking,
    Error,
    Tool,
    File,
    Dim,
}

impl Theme {
    fn color(&self, c: ThemeColor) -> Color {
        match c {
            ThemeColor::User => self.user_color,
            ThemeColor::Accent => self.accent_color,
            ThemeColor::Thinking => self.thinking_color,
            ThemeColor::Error => self.error_color,
            ThemeColor::Tool => self.tool_color,
            ThemeColor::File => self.file_color,
            ThemeColor::Dim => self.dim_color,
        }
    }
}

// ── Drawing ───────────────────────────────────────────────────────────────

pub fn draw(frame: &mut Frame, app: &App) {
    let theme = &app.theme;
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.bg).fg(theme.fg)),
        area,
    );

    let input_width = area.width.saturating_sub(4).max(1) as usize;
    let input_rows = (input_line_count(&app.input, input_width) as u16).clamp(1, MAX_INPUT_ROWS);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(input_rows + 2),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(frame, rows[0], app);

    if area.width >= 100 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(72), Constraint::Percentage(28)])
            .split(rows[1]);
        draw_chat(frame, columns[0], app);
        draw_tools(frame, columns[1], app);
    } else {
        app.tools_area.set(Rect::default());
        draw_chat(frame, rows[1], app);
    }

    draw_input(frame, rows[2], app);
    draw_footer(frame, rows[3], app);
    draw_suggestions(frame, rows[2], app);

    if let Some(info) = &app.approval_pending {
        draw_approval(frame, area, app, info);
    }
    if app.show_tool_details {
        draw_tool_details(frame, area, app);
    }
    if app.show_help {
        draw_help(frame, area, app);
    }
    if let (Some(start), Some(end)) = (app.selection_start, app.selection_end) {
        highlight_selection(frame, start, end, app.chat_area.get());
    }
}

fn draw_help(frame: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let height = (KEY_HELP.len() as u16 + 4).min(area.height);
    let width = 66.min(area.width);
    let modal = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, modal);
    let lines: Vec<Line> = KEY_HELP
        .iter()
        .map(|(keys, action)| {
            Line::from(vec![
                Span::styled(format!("{keys:<24}"), Style::default().fg(theme.fg)),
                Span::styled(*action, Style::default().fg(theme.dim_color)),
            ])
        })
        .collect();
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(
                Block::default()
                    .title(Span::styled(
                        " Keys ",
                        Style::default()
                            .fg(theme.accent_color)
                            .add_modifier(Modifier::BOLD),
                    ))
                    .title_bottom(Span::styled(
                        " Esc to close ",
                        Style::default().fg(theme.dim_color),
                    ))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.border_color))
                    .padding(Padding::new(2, 2, 1, 0)),
            )
            .style(Style::default().fg(theme.fg).bg(theme.bg)),
        modal,
    );
}

/// "Enter send · Esc stop" with the keys brighter than their actions.
fn hint_line(pairs: &[(&str, &str)], theme: &Theme) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (i, (key, action)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(
                "  ·  ",
                Style::default().fg(theme.border_color),
            ));
        }
        spans.push(Span::styled(key.to_string(), Style::default().fg(theme.fg)));
        spans.push(Span::styled(
            format!(" {action}"),
            Style::default().fg(theme.dim_color),
        ));
    }
    Line::from(spans)
}

fn highlight_selection(frame: &mut Frame, start: (u16, u16), end: (u16, u16), bounds: Rect) {
    let (p1, p2) = if (start.1, start.0) <= (end.1, end.0) {
        (start, end)
    } else {
        (end, start)
    };
    let style = Style::default()
        .bg(Color::Rgb(46, 56, 92))
        .fg(Color::Rgb(240, 242, 248));
    let buffer = frame.buffer_mut();
    let right = bounds.x + bounds.width.saturating_sub(1);
    for row in p1.1.max(bounds.y)..=p2.1.min(bounds.y + bounds.height.saturating_sub(1)) {
        let from = if row == p1.1 {
            p1.0.max(bounds.x)
        } else {
            bounds.x
        };
        let to = if row == p2.1 { p2.0.min(right) } else { right };
        for col in from..=to {
            if let Some(cell) = buffer.cell_mut((col, row)) {
                cell.set_style(style);
            }
        }
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let mut title = vec![
        Span::styled(
            " NORVEXUM ",
            Style::default()
                .fg(theme.bg)
                .bg(theme.accent_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", app.model_info),
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(skill) = &app.active_skill {
        title.push(Span::styled(
            format!("  ✦ {skill}"),
            Style::default().fg(theme.accent_color),
        ));
    }
    if app.total_tokens_used > 0 {
        title.push(Span::styled(
            format!("  · {} tokens", format_tokens(app.total_tokens_used)),
            Style::default().fg(theme.dim_color),
        ));
    }

    let (indicator, color) = if app.is_processing {
        (SPINNER[app.spinner_frame % SPINNER.len()], theme.tool_color)
    } else if app.status == "Error" {
        ("✗", theme.error_color)
    } else {
        ("●", theme.success_color)
    };
    let mut status = vec![
        Span::styled(format!("{indicator} "), Style::default().fg(color)),
        Span::styled(app.status.clone(), Style::default().fg(theme.fg)),
    ];
    if let Some(started) = app.run_started.filter(|_| app.is_processing) {
        status.push(Span::styled(
            format!("  {}", format_duration(started.elapsed())),
            Style::default().fg(theme.dim_color),
        ));
    }

    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(theme.border_color));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let status = Line::from(status);
    let status_w = (status.width() as u16 + 1).min(inner.width / 2);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(10), Constraint::Length(status_w)])
        .split(inner);
    frame.render_widget(Paragraph::new(Line::from(title)), cols[0]);
    frame.render_widget(Paragraph::new(status).alignment(Alignment::Right), cols[1]);
}

fn draw_chat(frame: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(theme.border_color))
        .padding(Padding::new(2, 1, 1, 0));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.chat_area.set(inner);

    let width = inner.width.max(10) as usize;
    let view_height = inner.height as usize;

    // Heights of every message (cached), then only the visible slice is cloned.
    let mut all: Vec<Line<'static>> = Vec::new();
    let mut all_meta: Vec<RowMeta> = Vec::new();
    let mut extra: Vec<Line<'static>> = Vec::new();
    let heights: Vec<usize> = (0..app.chat_lines.len())
        .map(|i| app.message_lines(i, width).len())
        .collect();

    if app.is_processing
        && !app.is_thinking
        && app
            .chat_lines
            .last()
            .is_none_or(|l| matches!(l.style, LineStyle::User | LineStyle::Status))
    {
        extra.push(Line::from(Span::styled(
            format!("{} Working…", SPINNER[app.spinner_frame % SPINNER.len()]),
            Style::default().fg(theme.dim_color),
        )));
    }

    let total: usize = heights.iter().sum::<usize>() + extra.len();
    let max_scroll = total.saturating_sub(view_height);
    app.max_scroll.set(max_scroll);
    app.total_lines.set(total);
    app.view_height.set(view_height);
    {
        let mut starts = app.message_starts.borrow_mut();
        starts.clear();
        let mut offset = 0;
        for (line, h) in app.chat_lines.iter().zip(&heights) {
            if matches!(line.style, LineStyle::User | LineStyle::Assistant) {
                starts.push(offset);
            }
            offset += h;
        }
    }
    let scroll = app.chat_scroll.min(max_scroll);
    let start = total.saturating_sub(view_height + scroll);
    let end = (start + view_height).min(total);

    let mut offset = 0;
    for (i, h) in heights.iter().enumerate() {
        if offset + h > start && offset < end {
            let (lines, meta) = app.message_rows(i, width);
            let from = start.saturating_sub(offset);
            let to = (end - offset).min(*h);
            all.extend(lines[from..to].iter().cloned());
            all_meta.extend(meta[from..to].iter().cloned());
        }
        offset += h;
        if offset >= end {
            break;
        }
    }
    if offset < end {
        let from = start.saturating_sub(offset);
        all.extend(extra.into_iter().skip(from).take(end - offset.max(start)));
    }

    *app.visible_plain.borrow_mut() = all
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    all_meta.resize_with(all.len(), RowMeta::default);
    *app.visible_meta.borrow_mut() = all_meta;

    frame.render_widget(Paragraph::new(Text::from(all)), inner);

    if max_scroll > 0 {
        let bar = Rect::new(
            area.x + area.width.saturating_sub(1),
            inner.y,
            1,
            inner.height,
        );
        let mut state = ScrollbarState::new(max_scroll)
            .viewport_content_length(view_height)
            .position(max_scroll - scroll);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .track_style(Style::default().fg(theme.border_color))
                .thumb_symbol("┃")
                .thumb_style(Style::default().fg(theme.dim_color)),
            bar,
            &mut state,
        );
    }

    if scroll > 0 {
        let hint = format!(" ↓ {scroll} more ");
        let w = hint.width() as u16;
        let rect = Rect::new(
            inner.x + inner.width.saturating_sub(w),
            inner.y + inner.height.saturating_sub(1),
            w,
            1,
        );
        frame.render_widget(
            Paragraph::new(hint).style(Style::default().fg(theme.bg).bg(theme.dim_color)),
            rect,
        );
    }
}

/// One-line summary of a tool call's arguments for the activity panel.
fn summarize_args(name: &str, args: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(args) else {
        return extract_streaming_content(args, "path");
    };
    for key in [
        "command", "path", "query", "url", "pattern", "name", "action", "source",
    ] {
        if let Some(v) = value.get(key).and_then(|v| v.as_str()) {
            return v.to_string();
        }
    }
    if name == "batch_view_images" {
        if let Some(n) = value.get("paths").and_then(|p| p.as_array()).map(Vec::len) {
            return format!("{n} images");
        }
    }
    String::new()
}

fn draw_tools(frame: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let focused = app.active_panel == ActivePanel::Activity;
    let block = Block::default().padding(Padding::new(2, 1, 1, 0));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.tools_area.set(area);

    let mut lines = vec![Line::from(vec![
        Span::styled(
            "ACTIVITY",
            Style::default()
                .fg(if focused {
                    theme.accent_color
                } else {
                    theme.dim_color
                })
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if focused {
                "  ↑↓ select · Enter details · c copy · Esc back"
            } else {
                "  Tab to browse"
            },
            Style::default().fg(theme.dim_color),
        ),
    ])];
    lines.push(Line::default());

    if app.tool_log.is_empty() {
        lines.push(Line::from(Span::styled(
            "Tool runs will appear here.",
            Style::default().fg(theme.dim_color),
        )));
    }

    let detail_width = inner.width.saturating_sub(4).max(8) as usize;
    let mut selected_row = 0;
    for (i, entry) in app.tool_log.iter().rev().enumerate() {
        let is_selected = focused && i == app.selected_tool_index;
        if is_selected {
            selected_row = lines.len();
        }
        let (mark, color) = match entry.status {
            ToolStatus::Running => (SPINNER[app.spinner_frame % SPINNER.len()], theme.tool_color),
            ToolStatus::Success => ("✓", theme.success_color),
            ToolStatus::Failed => ("✗", theme.error_color),
        };
        let name_style = if is_selected {
            Style::default()
                .bg(theme.accent_color)
                .fg(theme.bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)
        };
        let duration = match entry.duration {
            Some(d) => format_duration(d),
            None => format_duration(entry.started.elapsed()),
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{mark} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(entry.name.clone(), name_style),
            Span::styled(
                format!("  {duration}"),
                Style::default().fg(theme.dim_color),
            ),
        ]));

        let summary = clean_paths(&summarize_args(&entry.name, &entry.args), &app.project_root);
        if !summary.is_empty() {
            lines.push(Line::from(Span::styled(
                format!(
                    "  {}",
                    crate::util::truncate_chars(&compact_detail(&summary, 200), detail_width)
                ),
                Style::default().fg(theme.dim_color),
            )));
        }
        if entry.status == ToolStatus::Failed {
            let first = entry
                .output
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or_default();
            lines.push(Line::from(Span::styled(
                format!(
                    "  {}",
                    crate::util::truncate_chars(first.trim_start_matches("ERROR: "), detail_width)
                ),
                Style::default().fg(theme.error_color),
            )));
        }
    }

    let visible = inner.height as usize;
    let scroll = if selected_row >= visible {
        (selected_row + 2).saturating_sub(visible)
    } else {
        0
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines)).scroll((scroll as u16, 0)),
        inner,
    );
}

fn draw_input(frame: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let border = if app.is_processing {
        theme.border_color
    } else {
        theme.accent_color
    };
    let title = if !app.user_backlog.is_empty() {
        format!(" Message · {} queued ", app.user_backlog.len())
    } else if app.is_processing {
        " Message (queued until the current run finishes) ".into()
    } else {
        " Message ".into()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(Span::styled(
            title,
            Style::default().fg(border).add_modifier(Modifier::BOLD),
        ))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.input_box.set(area);
    app.input_area.set(inner);
    app.input_scroll.set(0);

    if app.input.is_empty() {
        let placeholder = if app.is_processing {
            "Type to queue a follow-up · Esc to stop"
        } else {
            "What should we build?  (/ for commands, Alt+Enter for a new line)"
        };
        frame.render_widget(
            Paragraph::new(placeholder).style(Style::default().fg(theme.dim_color)),
            inner,
        );
        frame.set_cursor_position((inner.x, inner.y));
        return;
    }

    let width = inner.width.max(1) as usize;
    let (rows, cursor_row, cursor_col) = layout_input(&app.input, app.cursor_pos, width);
    let height = inner.height as usize;
    let first = (cursor_row + 1).saturating_sub(height);
    app.input_scroll.set(first);
    let lines: Vec<Line> = rows
        .iter()
        .skip(first)
        .take(height)
        .map(|r| {
            Line::from(Span::styled(
                r.clone(),
                Style::default().fg(theme.input_color),
            ))
        })
        .collect();
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
    frame.set_cursor_position((
        inner.x + cursor_col as u16,
        inner.y + (cursor_row - first) as u16,
    ));
}

fn draw_suggestions(frame: &mut Frame, input_area: Rect, app: &App) {
    let suggestions = app.command_suggestions();
    if suggestions.is_empty() || app.approval_pending.is_some() {
        return;
    }
    let shown = suggestions.len().min(7) as u16;
    let width = input_area.width.min(56);
    let rect = Rect::new(
        input_area.x + 1,
        input_area.y.saturating_sub(shown + 2),
        width,
        shown + 2,
    );
    let lines: Vec<Line> = suggestions
        .iter()
        .take(shown as usize)
        .enumerate()
        .map(|(i, (cmd, desc))| {
            let style = if i == 0 {
                Style::default().fg(app.theme.bg).bg(app.theme.accent_color)
            } else {
                Style::default().fg(app.theme.fg)
            };
            Line::from(vec![
                Span::styled(format!(" {cmd:<17}"), style.add_modifier(Modifier::BOLD)),
                Span::styled(format!(" {desc}"), Style::default().fg(app.theme.dim_color)),
            ])
        })
        .collect();
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(app.theme.border_color))
                .title(Span::styled(
                    " Tab to complete ",
                    Style::default().fg(app.theme.dim_color),
                )),
        ),
        rect,
    );
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    if let Some((notice, at)) = &app.notice {
        if at.elapsed() < NOTICE_DURATION {
            frame.render_widget(
                Paragraph::new(format!(" {notice}")).style(Style::default().fg(theme.accent_color)),
                area,
            );
            return;
        }
    }
    let narrow = area.width < 100;
    let pairs: &[(&str, &str)] = if app.selection_start.is_some() {
        &[("Ctrl+C", "copy again"), ("Esc", "clear highlight")]
    } else if app.is_processing {
        if narrow {
            &[
                ("Esc", "stop"),
                ("Enter", "queue"),
                ("PgUp/PgDn", "scroll"),
                ("F1", "keys"),
            ]
        } else {
            &[
                ("Esc", "stop"),
                ("Enter", "queue a follow-up"),
                ("PgUp/PgDn", "scroll"),
                ("Ctrl+T", "reasoning"),
                ("F1", "all keys"),
            ]
        }
    } else if narrow {
        &[
            ("Enter", "send"),
            ("↑↓", "history"),
            ("PgUp/PgDn", "scroll"),
            ("F1", "keys"),
        ]
    } else {
        &[
            ("Enter", "send"),
            ("Alt+Enter", "new line"),
            ("↑↓", "history"),
            ("/", "commands"),
            ("Tab", "activity"),
            ("F1", "all keys"),
        ]
    };
    frame.render_widget(Paragraph::new(hint_line(pairs, theme)), area);
}

fn draw_approval(frame: &mut Frame, area: Rect, app: &App, info: &ApprovalInfo) {
    let theme = &app.theme;
    let modal = centered_rect(64, 50, area);
    frame.render_widget(Clear, modal);

    let args: serde_json::Value = serde_json::from_str(&info.args_preview).unwrap_or_default();
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Tool  ", Style::default().fg(theme.dim_color)),
            Span::styled(
                info.tool_name.clone(),
                Style::default()
                    .fg(theme.tool_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::default(),
    ];
    if let Some(cmd) = args.get("command").and_then(|c| c.as_str()) {
        lines.push(Line::from(Span::styled(
            "Command",
            Style::default().fg(theme.dim_color),
        )));
        for l in cmd.lines() {
            lines.push(Line::from(Span::styled(
                format!("  $ {l}"),
                Style::default()
                    .fg(theme.input_color)
                    .add_modifier(Modifier::BOLD),
            )));
        }
        if args.get("background").and_then(|b| b.as_bool()) == Some(true) {
            lines.push(Line::from(Span::styled(
                "  (runs in the background)",
                Style::default().fg(theme.dim_color),
            )));
        }
    } else {
        lines.push(Line::from(Span::styled(
            "Arguments",
            Style::default().fg(theme.dim_color),
        )));
        for l in crate::util::truncate_chars(&info.args_preview, 1500).lines() {
            lines.push(Line::from(format!("  {l}")));
        }
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled(
            " y ",
            Style::default()
                .fg(theme.bg)
                .bg(theme.success_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" allow    "),
        Span::styled(
            " n ",
            Style::default()
                .fg(theme.bg)
                .bg(theme.error_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" deny    "),
        Span::styled(
            "(/ask_perms off disables these prompts)",
            Style::default().fg(theme.dim_color),
        ),
    ]));

    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title(Span::styled(
                        " Allow this tool call? ",
                        Style::default()
                            .fg(theme.tool_color)
                            .add_modifier(Modifier::BOLD),
                    ))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.tool_color))
                    .padding(Padding::new(2, 2, 1, 1)),
            )
            .style(Style::default().fg(theme.fg).bg(theme.bg)),
        modal,
    );
}

fn draw_tool_details(frame: &mut Frame, area: Rect, app: &App) {
    let Some(entry) = app.tool_log.iter().rev().nth(app.selected_tool_index) else {
        return;
    };
    let theme = &app.theme;
    let modal = centered_rect(80, 80, area);
    frame.render_widget(Clear, modal);

    let color = match entry.status {
        ToolStatus::Running => theme.tool_color,
        ToolStatus::Success => theme.success_color,
        ToolStatus::Failed => theme.error_color,
    };
    let status = match entry.status {
        ToolStatus::Running => "running",
        ToolStatus::Success => "succeeded",
        ToolStatus::Failed => "failed",
    };
    let heading = |t: &str| {
        Line::from(Span::styled(
            t.to_string(),
            Style::default()
                .fg(theme.accent_color)
                .add_modifier(Modifier::BOLD),
        ))
    };

    let mut text = vec![
        Line::from(vec![
            Span::styled(
                status,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "  ·  {}",
                    format_duration(entry.duration.unwrap_or_else(|| entry.started.elapsed()))
                ),
                Style::default().fg(theme.dim_color),
            ),
        ]),
        Line::default(),
        heading("ARGUMENTS"),
    ];
    let args = serde_json::from_str::<serde_json::Value>(&entry.args)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| entry.args.clone());
    for l in crate::util::truncate_chars(&args, 4000).lines() {
        text.push(Line::from(l.to_string()));
    }
    text.push(Line::default());
    text.push(heading("OUTPUT"));
    let output = clean_paths(&entry.output, &app.project_root);
    for l in crate::util::truncate_chars(&output, 8000).lines() {
        text.push(Line::from(l.to_string()));
    }

    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .title(Span::styled(
                        format!(" {} ", entry.name),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ))
                    .title_bottom(Span::styled(
                        " ↑↓ PgUp/PgDn scroll · c copy output · Esc close ",
                        Style::default().fg(theme.dim_color),
                    ))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(color))
                    .padding(Padding::new(2, 2, 1, 1)),
            )
            .style(Style::default().fg(theme.fg).bg(theme.bg))
            .scroll((app.tool_details_scroll, 0))
            .wrap(Wrap { trim: false }),
        modal,
    );
}

fn clean_paths(text: &str, project_root: &std::path::Path) -> String {
    let root = project_root.to_string_lossy();
    if root.is_empty() {
        return text.to_string();
    }
    let forward = format!("{}/", root.replace('\\', "/").trim_end_matches('/'));
    let backward = format!("{}\\", root.replace('/', "\\").trim_end_matches('\\'));
    text.replace(&forward, "").replace(&backward, "")
}

// ── Event loop ────────────────────────────────────────────────────────────

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    );
}

pub async fn run_tui(mut app: App, handle: crate::runtime::AgentHandle) -> io::Result<()> {
    let crate::runtime::AgentHandle {
        events: mut agent_rx,
        user_tx,
        cancel_tx,
        approval_tx,
        ..
    } = handle;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    // A panic must not leave the user's terminal in raw mode.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<(Event, Instant)>();
    std::thread::spawn(move || {
        while let Ok(event) = event::read() {
            // Timestamped at read time so paste bursts can be told from typing.
            if event_tx.send((event, Instant::now())).is_err() {
                break;
            }
        }
    });

    let mut dirty = true;
    let mut last_draw = Instant::now() - FRAME;
    let mut last_key_at: Option<Instant> = None;

    let result: io::Result<()> = async {
        loop {
            if dirty && last_draw.elapsed() >= FRAME {
                terminal.draw(|frame| draw(frame, &app))?;
                last_draw = Instant::now();
                dirty = false;
            }
            let wait = if dirty {
                FRAME.saturating_sub(last_draw.elapsed())
            } else if app.is_processing {
                SPINNER_TICK
            } else {
                Duration::from_millis(500)
            };

            tokio::select! {
                Some(event) = agent_rx.recv() => {
                    app.handle_agent_event(event);
                    // Coalesce bursts of streamed tokens into one redraw.
                    while let Ok(event) = agent_rx.try_recv() {
                        app.handle_agent_event(event);
                    }
                    dirty = true;
                }
                Some((event, at)) = event_rx.recv() => {
                    dirty = true;
                    match event {
                        Event::Key(key) if key.kind != KeyEventKind::Release => {
                            let burst = last_key_at
                                .is_some_and(|t| at.saturating_duration_since(t) < PASTE_GAP);
                            last_key_at = Some(at);
                            if app.approval_pending.is_some() {
                                let answer = match key.code {
                                    KeyCode::Char('y' | 'Y') | KeyCode::Enter => Some(true),
                                    KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(false),
                                    _ => None,
                                };
                                if let Some(approved) = answer {
                                    let _ = approval_tx.send(approved);
                                    app.approval_pending = None;
                                    app.notify(if approved { "Approved" } else { "Denied" });
                                }
                                continue;
                            }
                            // Newlines inside a pasted block stay in the input.
                            if burst && app.paste_key(key.code, key.modifiers) {
                                continue;
                            }
                            if let Some(message) = app.handle_key(key.code, key.modifiers) {
                                let _ = user_tx.send(message);
                            }
                        }
                        Event::Paste(text) => app.insert_text(&text.replace("\r\n", "\n")),
                        Event::Mouse(mouse) => {
                            let inside = |r: Rect| {
                                r.width > 0
                                    && mouse.column >= r.x
                                    && mouse.column < r.x + r.width
                                    && mouse.row >= r.y
                                    && mouse.row < r.y + r.height
                            };
                            let over_tools = inside(app.tools_area.get());
                            let chat = app.chat_area.get();
                            // Include the chat's left padding so a drag can start at the margin.
                            let chat_hit = Rect::new(
                                chat.x.saturating_sub(2),
                                chat.y,
                                chat.width + 2,
                                chat.height,
                            );
                            match mouse.kind {
                                MouseEventKind::Down(MouseButton::Left) => {
                                    if over_tools {
                                        app.active_panel = ActivePanel::Activity;
                                        app.clear_selection();
                                    } else if inside(app.input_box.get()) {
                                        app.click_input(mouse.column, mouse.row);
                                    } else if inside(chat_hit) {
                                        app.active_panel = ActivePanel::Chat;
                                        app.selection_start = Some((mouse.column, mouse.row));
                                        app.selection_end = Some((mouse.column, mouse.row));
                                        app.is_selecting = true;
                                    } else {
                                        app.clear_selection();
                                    }
                                }
                                MouseEventKind::Down(MouseButton::Right) => {
                                    if let Ok(text) = arboard::Clipboard::new().and_then(|mut cb| cb.get_text()) {
                                        app.insert_text(&text.replace("\r\n", "\n"));
                                    }
                                }
                                MouseEventKind::Drag(_) if app.is_selecting => {
                                    app.selection_end = Some((mouse.column, mouse.row));
                                }
                                MouseEventKind::Up(_) if app.is_selecting => {
                                    app.selection_end = Some((mouse.column, mouse.row));
                                    app.is_selecting = false;
                                    if app.selection_start != app.selection_end {
                                        // Copy right away but keep the highlight visible.
                                        app.copy_selection(false);
                                    } else {
                                        app.clear_selection();
                                    }
                                }
                                MouseEventKind::ScrollUp => {
                                    if over_tools {
                                        app.selected_tool_index = app.selected_tool_index.saturating_sub(1);
                                    } else {
                                        app.scroll_chat(3);
                                    }
                                }
                                MouseEventKind::ScrollDown => {
                                    if over_tools {
                                        app.selected_tool_index = (app.selected_tool_index + 1)
                                            .min(app.tool_log.len().saturating_sub(1));
                                    } else {
                                        app.scroll_chat(-3);
                                    }
                                }
                                _ => {}
                            }
                        }
                        Event::Resize(..) => {}
                        _ => dirty = false,
                    }
                }
                _ = tokio::time::sleep(wait) => {
                    if app.is_processing || app.tool_log.iter().any(|t| t.status == ToolStatus::Running) {
                        app.spinner_frame = app.spinner_frame.wrapping_add(1);
                        dirty = true;
                    }
                    if app.notice.as_ref().is_some_and(|(_, at)| at.elapsed() >= NOTICE_DURATION) {
                        app.notice = None;
                        dirty = true;
                    }
                }
            }

            if std::mem::take(&mut app.cancel_requested) {
                app.user_backlog.clear();
                let _ = cancel_tx.send(());
            }
            if !app.is_processing {
                if let Some(next) = app.user_backlog.pop_front() {
                    app.start_run();
                    app.status = "Starting next message".into();
                    let _ = user_tx.send(next);
                }
            }
            if app.should_quit {
                break;
            }
        }
        Ok(())
    }
    .await;

    restore_terminal();
    terminal.show_cursor()?;
    result
}

// ── Markdown-aware message rendering ──────────────────────────────────────

fn render_markdown(text: &str, width: usize, color: Color, kind: LineStyle, theme: &Theme) -> Rows {
    let mut rows = Rows::default();
    let mut in_code = false;
    let code_bg = Color::Rgb(24, 25, 29);
    let rule = theme.border_color;
    let width = width.max(8);
    let base = if kind == LineStyle::Thinking {
        Style::default().fg(color).add_modifier(Modifier::ITALIC)
    } else {
        Style::default().fg(color)
    };

    for raw in text.lines() {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            let lang = trimmed.trim_start_matches('`').trim();
            let label = if in_code && !lang.is_empty() {
                format!("  ╭─ {lang}")
            } else if in_code {
                "  ╭─".to_string()
            } else {
                "  ╰─".to_string()
            };
            let fence = if in_code {
                format!("```{lang}")
            } else {
                "```".to_string()
            };
            rows.push(
                Line::from(Span::styled(label, Style::default().fg(rule))),
                RowMeta::copy_as(fence),
            );
            continue;
        }

        if in_code {
            let code_style = Style::default().fg(Color::Rgb(214, 216, 220)).bg(code_bg);
            let expanded = raw.replace('\t', "    ");
            for (i, chunk) in hard_wrap(&expanded, width.saturating_sub(4))
                .into_iter()
                .enumerate()
            {
                rows.push(
                    Line::from(vec![
                        Span::styled("  │ ", Style::default().fg(rule)),
                        Span::styled(chunk, code_style),
                    ]),
                    RowMeta::text(4, if i == 0 { Join::NewLine } else { Join::Glued }),
                );
            }
            continue;
        }

        if trimmed.is_empty() {
            rows.push(Line::default(), RowMeta::default());
            continue;
        }

        // Horizontal rules
        if trimmed.len() >= 3
            && (trimmed.chars().all(|c| c == '-')
                || trimmed.chars().all(|c| c == '*')
                || trimmed.chars().all(|c| c == '_'))
        {
            rows.push(
                Line::from(Span::styled(
                    format!("  {}", "─".repeat(width.saturating_sub(2).min(60))),
                    Style::default().fg(rule),
                )),
                RowMeta::copy_as("---"),
            );
            continue;
        }

        // Tables are shown verbatim (wrapping would break the columns).
        if trimmed.starts_with('|') {
            for (i, chunk) in hard_wrap(trimmed, width.saturating_sub(2))
                .into_iter()
                .enumerate()
            {
                rows.push(
                    Line::from(Span::styled(
                        format!("  {chunk}"),
                        Style::default().fg(color),
                    )),
                    RowMeta::text(2, if i == 0 { Join::NewLine } else { Join::Glued }),
                );
            }
            continue;
        }

        let indent = raw.len() - trimmed.len();
        let pad = " ".repeat(2 + indent.min(12));
        // `lead`: where copied text starts on the first row (bullets and
        // numbers are kept, indentation and quote bars are not).
        let (prefix, content, style, lead) = if let Some(h) = trimmed.strip_prefix("### ") {
            ("  ".to_string(), h, base.add_modifier(Modifier::BOLD), 2)
        } else if let Some(h) = trimmed.strip_prefix("## ") {
            (
                "  ".to_string(),
                h,
                base.fg(theme.accent_color).add_modifier(Modifier::BOLD),
                2,
            )
        } else if let Some(h) = trimmed.strip_prefix("# ") {
            (
                "  ".to_string(),
                h,
                base.fg(theme.accent_color)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                2,
            )
        } else if let Some(q) = trimmed.strip_prefix("> ") {
            let prefix = format!("{pad}▏ ");
            let lead = prefix.width();
            (
                prefix,
                q,
                base.fg(theme.dim_color).add_modifier(Modifier::ITALIC),
                lead,
            )
        } else if let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            let bullet = if indent >= 2 { "◦" } else { "•" };
            (format!("{pad}{bullet} "), item, base, pad.width())
        } else if let Some((num, rest)) = split_numbered(trimmed) {
            (format!("{pad}{num}. "), rest, base, pad.width())
        } else {
            (pad.clone(), trimmed, base, pad.width())
        };

        let cont = " ".repeat(prefix.width());
        let cont_w = cont.width();
        let spans = parse_inline_markdown(content, style, kind);
        for (i, (line, join)) in wrap_spans(spans, width, prefix, cont)
            .into_iter()
            .enumerate()
        {
            rows.push(
                line,
                RowMeta::text(if i == 0 { lead } else { cont_w }, join),
            );
        }
    }
    rows
}

/// "12. item" → ("12", "item")
fn split_numbered(line: &str) -> Option<(&str, &str)> {
    let digits = line.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 || digits > 3 {
        return None;
    }
    let rest = line[digits..].strip_prefix(". ")?;
    Some((&line[..digits], rest))
}

/// Parse inline markdown: **bold**, *italic*, `code`, ~~strikethrough~~
fn parse_inline_markdown(text: &str, base_style: Style, _kind: LineStyle) -> Vec<Span<'static>> {
    if !text.contains(['*', '`', '~', '_']) {
        return vec![Span::styled(text.to_string(), base_style)];
    }

    let mut spans = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;
    let code_style = Style::default()
        .fg(Color::Rgb(214, 186, 150))
        .bg(Color::Rgb(30, 31, 36));

    let flush = |current: &mut String, spans: &mut Vec<Span<'static>>| {
        if !current.is_empty() {
            spans.push(Span::styled(std::mem::take(current), base_style));
        }
    };
    // Find the closing delimiter; unclosed markers are kept as literal text.
    let find = |from: usize, delim: &[char]| -> Option<usize> {
        (from..=len.saturating_sub(delim.len())).find(|&j| chars[j..j + delim.len()] == *delim)
    };

    while i < len {
        let two = |a: char| i + 1 < len && chars[i] == a && chars[i + 1] == a;
        if two('*') || two('_') {
            let d = [chars[i], chars[i]];
            if let Some(end) = find(i + 2, &d).filter(|&e| e > i + 2) {
                flush(&mut current, &mut spans);
                spans.push(Span::styled(
                    chars[i + 2..end].iter().collect::<String>(),
                    base_style.add_modifier(Modifier::BOLD),
                ));
                i = end + 2;
                continue;
            }
        } else if two('~') {
            if let Some(end) = find(i + 2, &['~', '~']).filter(|&e| e > i + 2) {
                flush(&mut current, &mut spans);
                spans.push(Span::styled(
                    chars[i + 2..end].iter().collect::<String>(),
                    base_style.add_modifier(Modifier::CROSSED_OUT),
                ));
                i = end + 2;
                continue;
            }
        } else if chars[i] == '`' {
            if let Some(end) = find(i + 1, &['`']).filter(|&e| e > i + 1) {
                flush(&mut current, &mut spans);
                spans.push(Span::styled(
                    chars[i + 1..end].iter().collect::<String>(),
                    code_style,
                ));
                i = end + 1;
                continue;
            }
        } else if chars[i] == '*' && i + 1 < len && !chars[i + 1].is_whitespace() {
            if let Some(end) = find(i + 1, &['*']).filter(|&e| e > i + 1) {
                flush(&mut current, &mut spans);
                spans.push(Span::styled(
                    chars[i + 1..end].iter().collect::<String>(),
                    base_style.add_modifier(Modifier::ITALIC),
                ));
                i = end + 1;
                continue;
            }
        }
        current.push(chars[i]);
        i += 1;
    }
    flush(&mut current, &mut spans);
    spans
}

/// Greedy word wrap that keeps span styles and measures display width
/// (CJK and emoji take two columns). Each row says how it joins the row
/// above, so copying can undo the wrapping.
fn wrap_spans(
    spans: Vec<Span<'static>>,
    width: usize,
    first_prefix: String,
    cont_prefix: String,
) -> Vec<(Line<'static>, Join)> {
    let width = width.max(first_prefix.width() + 4);
    let mut lines: Vec<(Line<'static>, Join)> = Vec::new();
    let mut current: Vec<Span<'static>> = vec![Span::raw(first_prefix.clone())];
    let mut current_join = Join::NewLine;
    let mut cur_w = first_prefix.width();
    let mut line_has_text = false;
    let mut pending_space: Option<Style> = None;

    let new_line = |lines: &mut Vec<(Line<'static>, Join)>,
                    current: &mut Vec<Span<'static>>,
                    join: &mut Join,
                    next: Join| {
        lines.push((Line::from(std::mem::take(current)), *join));
        current.push(Span::raw(cont_prefix.clone()));
        *join = next;
    };

    for span in spans {
        let style = span.style;
        for (is_space, piece) in split_words(&span.content) {
            if is_space {
                if line_has_text {
                    pending_space = Some(style);
                }
                continue;
            }
            let w = piece.width();
            let space = usize::from(pending_space.is_some());
            if cur_w + space + w <= width {
                if let Some(s) = pending_space.take() {
                    current.push(Span::styled(" ", s));
                    cur_w += 1;
                }
                current.push(Span::styled(piece.to_string(), style));
                cur_w += w;
            } else if w <= width.saturating_sub(cont_prefix.width()) {
                // The break replaced a space, unless the word continued a styled run.
                let next = if pending_space.is_some() {
                    Join::Space
                } else {
                    Join::Glued
                };
                new_line(&mut lines, &mut current, &mut current_join, next);
                cur_w = cont_prefix.width();
                current.push(Span::styled(piece.to_string(), style));
                cur_w += w;
                pending_space = None;
            } else {
                // A single word longer than the line: break it by columns.
                if let Some(s) = pending_space.take() {
                    current.push(Span::styled(" ", s));
                    cur_w += 1;
                }
                let mut buf = String::new();
                for ch in piece.chars() {
                    let cw = ch.width().unwrap_or(0);
                    if cur_w + cw > width {
                        current.push(Span::styled(std::mem::take(&mut buf), style));
                        new_line(&mut lines, &mut current, &mut current_join, Join::Glued);
                        cur_w = cont_prefix.width();
                    }
                    buf.push(ch);
                    cur_w += cw;
                }
                current.push(Span::styled(buf, style));
            }
            line_has_text = true;
        }
    }
    if line_has_text || lines.is_empty() {
        lines.push((Line::from(current), current_join));
    }
    lines
}

/// Cursor index for a click at (`row`, `col`) of the wrapped input,
/// mirroring `layout_input`.
fn cursor_at(input: &str, width: usize, row: usize, col: usize) -> usize {
    let width = width.max(1);
    let (mut r, mut c) = (0usize, 0usize);
    for (i, ch) in input.chars().enumerate() {
        if ch == '\n' {
            if r == row {
                return i;
            }
            r += 1;
            c = 0;
            continue;
        }
        let cw = ch.width().unwrap_or(0);
        if c + cw > width {
            r += 1;
            c = 0;
        }
        if r > row || (r == row && c + cw > col) {
            return i;
        }
        c += cw;
    }
    input.chars().count()
}

/// Split into alternating runs of whitespace and non-whitespace.
fn split_words(text: &str) -> Vec<(bool, &str)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_space: Option<bool> = None;
    for (i, c) in text.char_indices() {
        let space = c.is_whitespace();
        if in_space.is_some_and(|s| s != space) {
            out.push((in_space.unwrap_or(false), &text[start..i]));
            start = i;
        }
        in_space = Some(space);
    }
    if start < text.len() {
        out.push((in_space.unwrap_or(false), &text[start..]));
    }
    out
}

/// Break a line into chunks of at most `width` display columns.
fn hard_wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut w = 0;
    for ch in text.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > width && !buf.is_empty() {
            out.push(std::mem::take(&mut buf));
            w = 0;
        }
        buf.push(ch);
        w += cw;
    }
    if !buf.is_empty() || out.is_empty() {
        out.push(buf);
    }
    out
}

/// Wrap the input box text. Returns (rows, cursor_row, cursor_col).
fn layout_input(input: &str, cursor: usize, width: usize) -> (Vec<String>, usize, usize) {
    let width = width.max(1);
    let mut rows = vec![String::new()];
    let mut row_w = 0;
    let (mut cursor_row, mut cursor_col) = (0, 0);
    for (i, ch) in input.chars().enumerate() {
        if i == cursor {
            cursor_row = rows.len() - 1;
            cursor_col = row_w;
        }
        if ch == '\n' {
            rows.push(String::new());
            row_w = 0;
            continue;
        }
        let cw = ch.width().unwrap_or(0);
        if row_w + cw > width {
            rows.push(String::new());
            row_w = 0;
            if i == cursor {
                cursor_row = rows.len() - 1;
                cursor_col = 0;
            }
        }
        if let Some(last) = rows.last_mut() {
            last.push(ch);
        }
        row_w += cw;
    }
    if cursor >= input.chars().count() {
        if row_w >= width {
            rows.push(String::new());
            row_w = 0;
        }
        cursor_row = rows.len() - 1;
        cursor_col = row_w;
    }
    (rows, cursor_row, cursor_col)
}

fn input_line_count(input: &str, width: usize) -> usize {
    layout_input(input, input.chars().count(), width).0.len()
}

/// Columns `[from, to)` of a rendered line (by display width).
fn slice_columns(line: &str, from: usize, to: usize) -> String {
    let mut out = String::new();
    let mut col = 0;
    for ch in line.chars() {
        let w = ch.width().unwrap_or(0);
        if col >= from && col < to {
            out.push(ch);
        }
        col += w;
        if col >= to {
            break;
        }
    }
    out
}

fn byte_index(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(index, _)| index)
}

fn char_count(text: &str) -> usize {
    text.chars().count()
}

fn compact_detail(text: &str, max: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    crate::util::truncate_chars(&one_line, max)
}

fn clean_status(status: &str) -> String {
    let trimmed = status.trim();
    // Drop a leading emoji/symbol: the header already shows a state indicator.
    match trimmed.split_once(' ') {
        Some((first, rest)) if !first.chars().any(|c| c.is_alphanumeric()) => rest.to_string(),
        _ => trimmed.to_string(),
    }
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}K", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else if secs >= 10 {
        format!("{secs}s")
    } else {
        format!("{:.1}s", d.as_secs_f32())
    }
}

/// Best-effort extraction of a string value from (possibly incomplete) JSON.
fn extract_streaming_content(json_accumulated: &str, key: &str) -> String {
    let pattern = format!("\"{}\":", key);
    let Some(pos) = json_accumulated.find(&pattern) else {
        return String::new();
    };
    let rest = &json_accumulated[pos + pattern.len()..];
    let Some(quote_start) = rest.find('"') else {
        return String::new();
    };
    let mut result = String::new();
    let mut chars = rest[quote_start + 1..].chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('n') => result.push('\n'),
                Some('t') => result.push('\t'),
                Some('r') => {}
                Some('u') => {
                    let hex: String = chars.by_ref().take(4).collect();
                    if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        result.push(ch);
                    }
                }
                Some(other) => result.push(other),
                None => break,
            },
            '"' => break,
            other => result.push(other),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn unicode_input_editing_uses_character_boundaries() {
        let mut app = App::new("test", std::path::PathBuf::new());
        app.handle_key(KeyCode::Char('日'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('本'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Left, KeyModifiers::NONE);
        app.handle_key(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(app.input, "本");
        assert_eq!(app.cursor_pos, 0);
    }

    fn render(app: &App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn layout_is_responsive() {
        let app = App::new("test-model / test-provider", std::path::PathBuf::new());
        let wide = render(&app, 120, 30);
        assert!(wide.contains("NORVEXUM"));
        assert!(wide.contains("ACTIVITY"));

        let narrow = render(&app, 70, 20);
        assert!(narrow.contains("NORVEXUM"));
        assert!(!narrow.contains("ACTIVITY"));
    }

    #[test]
    fn inline_markdown_parses_bold() {
        let spans =
            parse_inline_markdown("hello **world**", Style::default(), LineStyle::Assistant);
        assert_eq!(spans.len(), 2);
        assert!(spans[1].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn inline_markdown_parses_code_and_keeps_unclosed_markers() {
        let spans = parse_inline_markdown("use `println!`", Style::default(), LineStyle::Assistant);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].content, "println!");
        let plain = parse_inline_markdown("2 * 3 = 6", Style::default(), LineStyle::Assistant);
        let text: String = plain.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "2 * 3 = 6");
    }

    #[test]
    fn styled_wrapping_respects_display_width() {
        let spans = parse_inline_markdown(
            "日本語 **bold words** here",
            Style::default(),
            LineStyle::Assistant,
        );
        let lines: Vec<Line> = wrap_spans(spans, 12, "  ".into(), "  ".into())
            .into_iter()
            .map(|(line, _)| line)
            .collect();
        for line in &lines {
            assert!(line.width() <= 12, "line too wide: {line:?}");
        }
        assert!(lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD))
        }));
    }

    #[test]
    fn multiline_input_and_history() {
        let mut app = App::new("m", std::path::PathBuf::new());
        for c in "first".chars() {
            app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert_eq!(
            app.handle_key(KeyCode::Enter, KeyModifiers::NONE)
                .as_deref(),
            Some("first")
        );
        app.is_processing = false;

        app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Enter, KeyModifiers::ALT);
        app.handle_key(KeyCode::Char('b'), KeyModifiers::NONE);
        assert_eq!(app.input, "a\nb");
        // Up moves within the input first, then into history.
        app.handle_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.cursor_pos, 1);
        app.handle_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.input, "first");
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.input, "a\nb");
    }

    #[test]
    fn esc_does_not_quit_and_messages_queue_while_busy() {
        let mut app = App::new("m", std::path::PathBuf::new());
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.should_quit);

        app.is_processing = true;
        app.handle_key(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(app.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);
        assert_eq!(app.user_backlog.len(), 1);
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.cancel_requested);
    }

    #[test]
    fn copied_selection_matches_the_source_text() {
        let mut app = App::new("m", std::path::PathBuf::new());
        app.add_user_message("hi".into());
        app.handle_agent_event(AgentEvent::Content(
            "This first paragraph is long enough that it has to wrap onto more than one row.\n\n```rust\nfn main() {\n    println!(\"x\");\n}\n```\n- a bullet".into(),
        ));
        app.handle_agent_event(AgentEvent::Done { usage: None });
        render(&app, 60, 40);
        app.selection_start = Some((0, 0));
        app.selection_end = Some((59, 39));
        let text = app.selection_text();
        assert!(
            text.contains(
                "This first paragraph is long enough that it has to wrap onto more than one row."
            ),
            "wrapped paragraph was not rejoined:\n{text}"
        );
        assert!(
            text.contains("```rust\nfn main() {\n    println!(\"x\");\n}\n```"),
            "code block lost its shape:\n{text}"
        );
        assert!(text.contains("\n• a bullet"), "bullet missing:\n{text}");
        assert!(!text.contains("│"), "gutter leaked into the copy:\n{text}");
    }

    #[test]
    fn pasted_newlines_stay_in_the_input() {
        let mut app = App::new("m", std::path::PathBuf::new());
        app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(app.paste_key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.paste_key(KeyCode::Char('b'), KeyModifiers::NONE));
        assert!(app.paste_key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input, "a\nb    ");
        assert!(!app.paste_key(KeyCode::Char('v'), KeyModifiers::CONTROL));
        assert!(!app.is_processing);
    }

    #[test]
    fn clicking_the_input_moves_the_cursor() {
        assert_eq!(cursor_at("hello world", 20, 0, 3), 3);
        assert_eq!(cursor_at("hello world", 20, 0, 50), 11);
        assert_eq!(cursor_at("ab\ncd", 20, 1, 1), 4);
        assert_eq!(cursor_at("ab\ncd", 20, 0, 9), 2);
        // Wrapped at width 4: "abcd" / "ef"
        assert_eq!(cursor_at("abcdef", 4, 1, 1), 5);
    }

    #[test]
    fn alt_arrows_jump_between_messages() {
        let mut app = App::new("m", std::path::PathBuf::new());
        for i in 0..6 {
            app.add_user_message(format!("question {i}"));
            app.handle_agent_event(AgentEvent::Content(format!("answer {i}\n").repeat(6)));
            app.handle_agent_event(AgentEvent::Done { usage: None });
        }
        render(&app, 70, 20);
        assert_eq!(app.chat_scroll, 0);
        app.handle_key(KeyCode::Up, KeyModifiers::ALT);
        let after_one = app.chat_scroll;
        assert!(after_one > 0);
        render(&app, 70, 20);
        app.handle_key(KeyCode::Up, KeyModifiers::ALT);
        assert!(app.chat_scroll > after_one);
        render(&app, 70, 20);
        app.handle_key(KeyCode::Down, KeyModifiers::ALT);
        assert_eq!(app.chat_scroll, after_one);
    }

    #[test]
    fn help_overlay_toggles_and_swallows_keys() {
        let mut app = App::new("m", std::path::PathBuf::new());
        app.handle_key(KeyCode::F(1), KeyModifiers::NONE);
        assert!(app.show_help);
        app.handle_key(KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(app.input.is_empty());
        assert!(render(&app, 120, 30).contains("toggle this help"));
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.show_help);
        // `?` opens help only when it isn't part of a message.
        app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('?'), KeyModifiers::NONE);
        assert_eq!(app.input, "a?");
        assert!(!app.show_help);
    }

    #[test]
    fn slash_suggestions_complete_with_tab() {
        let mut app = App::new("m", std::path::PathBuf::new());
        for c in "/com".chars() {
            app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        app.handle_key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.input, "/compact ");
        assert_eq!(app.active_panel, ActivePanel::Chat);
    }

    #[test]
    fn chat_renders_streamed_reply_and_tool_activity() {
        let mut app = App::new("m", std::path::PathBuf::new());
        app.add_user_message("hi".into());
        app.handle_agent_event(AgentEvent::Content("Hello **there**".into()));
        app.handle_agent_event(AgentEvent::ToolStart {
            name: "read_file".into(),
            id: "1".into(),
        });
        app.handle_agent_event(AgentEvent::ToolArgsDelta {
            id: "1".into(),
            delta: r#"{"path":"src/main.rs"}"#.into(),
        });
        app.handle_agent_event(AgentEvent::ToolResult {
            id: "1".into(),
            name: "read_file".into(),
            result: "ok".into(),
            success: true,
        });
        app.handle_agent_event(AgentEvent::Done { usage: None });
        let screen = render(&app, 120, 30);
        assert!(screen.contains("Hello there"));
        assert!(screen.contains("read_file"));
        assert!(screen.contains("src/main.rs"));
        assert!(!app.is_processing);
    }

    #[test]
    fn reasoning_is_visible_while_streaming() {
        let mut app = App::new("m", std::path::PathBuf::new());
        app.add_user_message("why is the sky blue".into());
        app.handle_agent_event(AgentEvent::Status("Thinking (step 1/49)...".into()));
        app.handle_agent_event(AgentEvent::Thinking("Rayleigh scattering ".into()));
        app.handle_agent_event(AgentEvent::Thinking("affects short wavelengths".into()));
        let screen = render(&app, 120, 30);
        assert!(
            screen.contains("Rayleigh scattering"),
            "live reasoning not drawn"
        );
        assert!(screen.contains("short wavelengths"));
        assert!(screen.contains("streaming"));
        // Still fully visible once reasoning is done and the answer streams.
        app.handle_agent_event(AgentEvent::ThinkingDone);
        app.handle_agent_event(AgentEvent::Content(
            "Because of Rayleigh scattering.".into(),
        ));
        let screen = render(&app, 120, 30);
        assert!(screen.contains("short wavelengths"));
        assert!(screen.contains("Because of"));
    }
}
