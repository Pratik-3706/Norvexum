// ===========================================================================
// TUI — terminal front-end (ratatui + crossterm)
//
// Rendering is event-driven: the screen is redrawn only when something
// changed (capped at ~30 fps while streaming), each message's wrapped lines
// are cached, and only the visible rows are materialized.
//
// The main agent and every sub-agent it starts have their own `Transcript`.
// Events are routed into the right one and all of them render through the
// same code: the chat area shows the focused transcript, or the main one and
// a sub-agent side by side in split view.
// ===========================================================================

pub mod theme;

use std::cell::{Cell, RefCell};
use std::collections::{HashSet, VecDeque};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
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
/// Runs at least this long ring the terminal bell when they finish.
const LONG_RUN: Duration = Duration::from_secs(20);
/// Prompts kept in the persistent history.
const HISTORY_LIMIT: usize = 500;
/// Sub-agent rows in the agents strip before it scrolls.
const STRIP_ROWS: usize = 4;

/// Slash commands offered as completions. Most are run by the agent (see its
/// `/help`); `/agents`, `/copy code` and `/keys` are handled by the TUI.
const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/help", "show all commands"),
    ("/clear", "clear chat and memory"),
    ("/compact", "summarize older messages"),
    ("/copy", "copy last answer"),
    ("/copy chat", "copy whole chat"),
    ("/copy code", "copy the last code block"),
    ("/undo", "restore files changed by the last edit"),
    ("/model", "list or switch models"),
    ("/provider", "list or switch providers"),
    ("/agents", "list sub-agents, or show one: /agents 2"),
    ("/agents split", "main and a sub-agent side by side"),
    ("/agents stop", "stop the sub-agent being viewed"),
    ("/session resume", "continue the last session"),
    ("/session list", "list saved sessions"),
    ("/loops", "set the tool-loop limit (number or unlimited)"),
    (
        "/mcp",
        "MCP servers: list, add <name> <command|url>, remove",
    ),
    ("/ask_perms", "toggle tool approval prompts"),
    ("/msg", "note to a sub-agent: /msg tests use nextest"),
    ("/keys", "show key bindings"),
    ("/exit", "quit"),
];

/// Every key binding, shown by the F1 help overlay.
const KEY_HELP: &[(&str, &str)] = &[
    ("F1 or ?", "toggle this help"),
    ("Enter", "send (queues while a run is active)"),
    ("Alt+Enter / Ctrl+J", "new line"),
    ("Up / Down", "move between lines, then browse history"),
    ("Ctrl+R", "search prompt history (again: older match)"),
    ("Tab", "complete a / command, or switch to Activity"),
    ("Up / Down in / menu", "pick a command, Enter runs it"),
    ("PgUp / PgDn", "scroll the chat"),
    ("Shift+Up / Shift+Down", "scroll one line"),
    (
        "Ctrl+Home / Ctrl+End",
        "jump to top / latest (Home / End if empty)",
    ),
    ("Alt+Up / Alt+Down", "jump to the previous / next message"),
    ("Alt+Left / Alt+Right", "switch between main and sub-agents"),
    ("Alt+0 … Alt+9", "show main / sub-agent N"),
    ("Ctrl+S or F2", "split view: main beside a sub-agent"),
    ("Ctrl+X", "stop the sub-agent being viewed"),
    ("Mouse drag", "select text (copied on release)"),
    ("Click", "move the cursor, focus a pane or agent"),
    ("Right click", "paste"),
    ("Esc", "stop the run, clear selection or input"),
    ("Ctrl+T", "expand or collapse reasoning"),
    ("Ctrl+Y / Alt+Y", "copy the last reply / last code block"),
    ("Ctrl+A", "copy the whole chat"),
    ("c (Activity)", "copy the selected tool's output"),
    ("Ctrl+V", "paste"),
    ("Ctrl+W / Ctrl+U", "delete word / to line start"),
    ("Ctrl+L", "clear the input"),
    ("Ctrl+C", "copy selection, clear, stop, or quit"),
    ("Ctrl+D", "quit (empty input)"),
];

pub struct App {
    /// The main agent's conversation.
    pub main: Transcript,
    /// Sub-agents started this session, in start order.
    pub subagents: Vec<SubAgentView>,
    /// Transcript the keyboard works on: None for main, else an index into `subagents`.
    pub focus: Option<usize>,
    /// Main beside a sub-agent (the last one focused).
    pub split: bool,
    split_agent: usize,
    pub input: String,
    /// Cursor position in Unicode scalar values, never raw bytes.
    pub cursor_pos: usize,
    pub status: String,
    pub model_info: String,
    pub should_quit: bool,
    pub is_processing: bool,
    pub theme: Theme,
    pub input_history: Vec<String>,
    pub history_pos: Option<usize>,
    // ── Token usage tracking (main and sub-agents) ───────────────────────
    pub total_tokens_used: u64,
    pub prompt_tokens_used: u64,
    pub completion_tokens_used: u64,
    // ── Text selection state (screen coordinates) ────────────────────────
    pub selection_start: Option<(u16, u16)>,
    pub selection_end: Option<(u16, u16)>,
    pub is_selecting: bool,
    /// Tool approvals waiting for an answer, oldest first.
    pub approvals: VecDeque<PendingApproval>,
    // ── Interactive Activity panel state ─────────────────────────────────
    pub active_panel: ActivePanel,
    pub selected_tool_index: usize,
    pub show_tool_details: bool,
    pub tool_details_scroll: u16,
    pub active_skill: Option<String>,
    pub project_root: PathBuf,
    pub user_backlog: VecDeque<String>,
    /// Show full reasoning text instead of a two-line preview.
    pub show_reasoning: bool,
    /// Key binding overlay (F1).
    pub show_help: bool,
    help_scroll: u16,
    run_started: Option<Instant>,
    spinner_frame: usize,
    notice: Option<(String, Instant)>,
    cancel_requested: bool,
    /// Sub-agents the user asked to stop; the event loop cancels them.
    subagent_cancels: Vec<String>,
    /// A long run just finished: ring the terminal bell.
    bell_pending: bool,
    quit_armed_at: Option<Instant>,
    history_draft: String,
    history_file: Option<PathBuf>,
    history_search: Option<HistorySearch>,
    /// Highlighted entry of the `/` menu, and whether the user moved it.
    palette_index: usize,
    palette_moved: bool,
    /// Chat panes (the second is only used in split view).
    panes: [Pane; 2],
    /// Pane the current text selection is in.
    selection_pane: usize,
    // Geometry from the last draw, used for mouse hit-testing.
    tools_area: Cell<Rect>,
    strip_area: Cell<Rect>,
    /// First sub-agent shown in the agents strip.
    strip_first: Cell<usize>,
    /// Input box (with border) and its text area, plus the first visible row.
    input_box: Cell<Rect>,
    input_area: Cell<Rect>,
    input_scroll: Cell<usize>,
}

/// Geometry and visible text of one chat pane from the last draw, used for
/// scrolling, message jumps, mouse hit-testing and copy.
#[derive(Default)]
struct Pane {
    area: Cell<Rect>,
    max_scroll: Cell<usize>,
    total_lines: Cell<usize>,
    view_height: Cell<usize>,
    /// First line of every user/assistant message, for Alt+Up/Down.
    message_starts: RefCell<Vec<usize>>,
    visible_plain: RefCell<Vec<String>>,
    visible_meta: RefCell<Vec<RowMeta>>,
}

/// Ctrl+R state: the query, how many older matches were skipped, and the
/// input to restore on Esc.
struct HistorySearch {
    query: String,
    skip: usize,
    draft: String,
    found: bool,
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

/// An approval request and the agent waiting for it.
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub info: ApprovalInfo,
    /// (id, name) of the sub-agent asking, or None for the main agent.
    pub agent: Option<(String, String)>,
}

/// Where the answer to an approval must go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalAnswer {
    Main(bool),
    SubAgent { id: String, approved: bool },
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
    /// The task a sub-agent was given.
    Task,
    Assistant,
    Thinking,
    Error,
    Warning,
    FileWrite,
    Status,
    /// A note one agent sent another (message_agent, or the user's /msg).
    Message,
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

// ── Transcripts ───────────────────────────────────────────────────────────

/// One agent's conversation: chat lines, tool runs and scroll position.
pub struct Transcript {
    pub lines: Vec<ChatLine>,
    pub tool_log: Vec<ToolLogEntry>,
    pub is_thinking: bool,
    /// Lines scrolled up from the bottom.
    pub scroll: usize,
    /// Who signs the replies (a sub-agent's name); None for the main agent.
    agent_name: Option<String>,
    next_rev: u64,
    render_cache: RefCell<Vec<Option<CachedRender>>>,
}

impl Transcript {
    fn new(agent_name: Option<String>) -> Self {
        Self {
            lines: Vec::new(),
            tool_log: Vec::new(),
            is_thinking: false,
            scroll: 0,
            agent_name,
            next_rev: 0,
            render_cache: RefCell::new(Vec::new()),
        }
    }

    fn push_line(&mut self, content: String, style: LineStyle, tool_call_id: Option<String>) {
        self.next_rev += 1;
        self.lines.push(ChatLine {
            content,
            style,
            tool_call_id,
            rev: self.next_rev,
        });
        self.scroll = 0;
    }

    /// Mutable access to a chat line that invalidates its render cache.
    fn line_mut(&mut self, idx: usize) -> &mut ChatLine {
        self.next_rev += 1;
        let line = &mut self.lines[idx];
        line.rev = self.next_rev;
        line
    }

    fn clear(&mut self) {
        self.lines.clear();
        self.tool_log.clear();
        self.render_cache.borrow_mut().clear();
        self.is_thinking = false;
        self.scroll = 0;
    }

    /// Tools cut off by a stop never report back; mark them so they don't spin forever.
    fn stop_running_tools(&mut self) {
        for entry in &mut self.tool_log {
            if entry.status == ToolStatus::Running {
                entry.status = ToolStatus::Failed;
                entry.output = "Stopped before it finished.".into();
                entry.duration = Some(entry.started.elapsed());
            }
        }
    }

    fn last_reply(&self) -> Option<&str> {
        self.lines
            .iter()
            .rev()
            .find(|l| l.style == LineStyle::Assistant && !l.content.trim().is_empty())
            .map(|l| l.content.as_str())
    }

    fn last_code_block(&self) -> Option<String> {
        self.lines
            .iter()
            .rev()
            .filter(|l| l.style == LineStyle::Assistant)
            .find_map(|l| last_code_block(&l.content))
    }

    /// Apply an event that only changes this transcript. Returns the status
    /// it implies, or hands the event back when the front-end must handle it.
    fn apply(&mut self, event: AgentEvent) -> Result<Option<String>, AgentEvent> {
        match event {
            AgentEvent::Thinking(text) => {
                let streaming_line =
                    self.lines.len().checked_sub(1).filter(|&i| {
                        self.is_thinking && self.lines[i].style == LineStyle::Thinking
                    });
                match streaming_line {
                    Some(idx) => self.line_mut(idx).content.push_str(&text),
                    None if text.trim().is_empty() => return Ok(Some("Thinking".into())),
                    None => {
                        self.push_line(text.trim_start().to_string(), LineStyle::Thinking, None)
                    }
                }
                self.is_thinking = true;
                self.scroll = 0;
                Ok(Some("Thinking".into()))
            }
            AgentEvent::ThinkingDone => {
                if let Some(idx) = self.lines.len().checked_sub(1) {
                    if self.is_thinking && self.lines[idx].style == LineStyle::Thinking {
                        let trimmed = self.lines[idx].content.trim_end().to_string();
                        self.line_mut(idx).content = trimmed;
                    }
                }
                self.is_thinking = false;
                Ok(None)
            }
            AgentEvent::Content(text) => {
                match self.lines.len().checked_sub(1) {
                    Some(idx) if self.lines[idx].style == LineStyle::Assistant => {
                        self.line_mut(idx).content.push_str(&text);
                        self.scroll = 0;
                    }
                    _ if !text.trim().is_empty() => {
                        self.push_line(text.trim_start().to_string(), LineStyle::Assistant, None)
                    }
                    _ => {}
                }
                Ok(None)
            }
            AgentEvent::ToolStart { name, id } => {
                let status = format!("Running {name}");
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
                Ok(Some(status))
            }
            AgentEvent::ToolArgsDelta { id, delta } => {
                let Some(entry) = self.tool_log.iter_mut().rev().find(|e| e.id == id) else {
                    return Ok(None);
                };
                entry.args.push_str(&delta);
                if !is_file_tool(&entry.name) {
                    return Ok(None);
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
                    .lines
                    .iter()
                    .rposition(|l| l.tool_call_id.as_deref() == Some(&id))
                {
                    self.line_mut(idx).content = content;
                }
                Ok(None)
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
                        .lines
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
                Ok(Some(format!(
                    "{name} {}",
                    if success { "done" } else { "failed" }
                )))
            }
            // Already reflected by the tool's chat line.
            AgentEvent::FileWrite { .. } => Ok(None),
            AgentEvent::Error(error) => {
                self.push_line(error, LineStyle::Error, None);
                Ok(Some("Error".into()))
            }
            AgentEvent::Warning(warning) => {
                self.push_line(warning, LineStyle::Warning, None);
                Ok(None)
            }
            AgentEvent::Status(status) => Ok(Some(clean_status(&status))),
            other => Err(other),
        }
    }

    /// Rendered height of one message (cached by revision and width).
    fn message_height(&self, idx: usize, width: usize, theme: &Theme, expanded: bool) -> usize {
        self.message_rows(idx, width, theme, expanded).0.len()
    }

    /// Lines plus per-row copy metadata for one message.
    fn message_rows(
        &self,
        idx: usize,
        width: usize,
        theme: &Theme,
        expanded: bool,
    ) -> (Vec<Line<'static>>, Vec<RowMeta>) {
        let line = &self.lines[idx];
        let live =
            line.style == LineStyle::Thinking && self.is_thinking && idx + 1 == self.lines.len();
        {
            let cache = self.render_cache.borrow();
            if let Some(Some(c)) = cache.get(idx) {
                if c.rev == line.rev && c.width == width && c.expanded == expanded && !live {
                    return (c.lines.clone(), c.meta.clone());
                }
            }
        }

        let (color, label) = label_for(line.style);
        let color = theme.color(color);
        let label = match (&self.agent_name, line.style) {
            _ if live => "∴ Reasoning…  (streaming)".to_string(),
            (Some(name), LineStyle::Assistant) => format!("◆ {name}"),
            _ => label.to_string(),
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
            LineStyle::User | LineStyle::Task => theme.fg,
            LineStyle::Assistant => theme.assistant_color,
            LineStyle::Status | LineStyle::Message => theme.dim_color,
            _ => color,
        };
        let mut body = render_markdown(&line.content, width, body_color, line.style, theme);
        if line.style == LineStyle::Thinking && !expanded && !live && body.lines.len() > 2 {
            let hidden = body.lines.len() - 2;
            body.truncate(2);
            body.push(
                Line::from(Span::styled(
                    format!("  … {hidden} more lines (Ctrl+T to expand)"),
                    Style::default().fg(theme.dim_color),
                )),
                RowMeta::copy_as(""),
            );
        }
        rows.append(body);
        rows.push(Line::default(), RowMeta::default());

        let mut cache = self.render_cache.borrow_mut();
        if cache.len() < self.lines.len() {
            cache.resize_with(self.lines.len(), || None);
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

/// The last fenced code block in `text` (an unclosed one at the end counts).
fn last_code_block(text: &str) -> Option<String> {
    let mut last = None;
    let mut current: Option<Vec<&str>> = None;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            match current.take() {
                Some(block) => last = Some(block.join("\n")),
                None => current = Some(Vec::new()),
            }
        } else if let Some(block) = current.as_mut() {
            block.push(line);
        }
    }
    current.map(|b| b.join("\n")).or(last)
}

// ── Sub-agents ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Running,
    Done,
    Failed,
}

/// A sub-agent started by the main agent's `spawn_agent` tool.
pub struct SubAgentView {
    pub id: String,
    pub name: String,
    pub state: AgentState,
    /// Last status it reported (or its result once finished).
    pub activity: String,
    pub tokens: u64,
    started: Instant,
    elapsed: Option<Duration>,
    pub transcript: Transcript,
}

impl SubAgentView {
    fn new(id: String, name: String, task: &str) -> Self {
        let mut transcript = Transcript::new(Some(name.clone()));
        if !task.trim().is_empty() {
            transcript.push_line(task.trim().to_string(), LineStyle::Task, None);
        }
        Self {
            id,
            name,
            state: AgentState::Running,
            activity: "Starting".into(),
            tokens: 0,
            started: Instant::now(),
            elapsed: None,
            transcript,
        }
    }

    fn elapsed(&self) -> Duration {
        self.elapsed.unwrap_or_else(|| self.started.elapsed())
    }

    fn is_running(&self) -> bool {
        self.state == AgentState::Running
    }

    /// What it is doing right now: the running tool and its target, if any.
    fn current_activity(&self, root: &Path) -> String {
        if self.is_running() {
            if let Some(t) = self
                .transcript
                .tool_log
                .iter()
                .rev()
                .find(|t| t.status == ToolStatus::Running)
            {
                let summary = clean_paths(&summarize_args(&t.name, &t.args), root);
                return if summary.is_empty() {
                    t.name.clone()
                } else {
                    format!("{} {}", t.name, summary)
                };
            }
        }
        self.activity.clone()
    }

    fn state_word(&self) -> &'static str {
        match self.state {
            AgentState::Running => "running",
            AgentState::Done => "done",
            AgentState::Failed => "failed",
        }
    }
}

impl App {
    pub fn new(model_info: &str, project_root: PathBuf) -> Self {
        let mut app = Self {
            main: Transcript::new(None),
            subagents: Vec::new(),
            focus: None,
            split: false,
            split_agent: 0,
            input: String::new(),
            cursor_pos: 0,
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
            approvals: VecDeque::new(),
            active_panel: ActivePanel::Chat,
            selected_tool_index: 0,
            show_tool_details: false,
            tool_details_scroll: 0,
            active_skill: None,
            project_root,
            user_backlog: VecDeque::new(),
            show_reasoning: false,
            show_help: false,
            help_scroll: 0,
            run_started: None,
            spinner_frame: 0,
            notice: None,
            cancel_requested: false,
            subagent_cancels: Vec::new(),
            bell_pending: false,
            quit_armed_at: None,
            history_draft: String::new(),
            history_file: None,
            history_search: None,
            palette_index: 0,
            palette_moved: false,
            panes: [Pane::default(), Pane::default()],
            selection_pane: 0,
            tools_area: Cell::new(Rect::default()),
            strip_area: Cell::new(Rect::default()),
            strip_first: Cell::new(0),
            input_box: Cell::new(Rect::default()),
            input_area: Cell::new(Rect::default()),
            input_scroll: Cell::new(0),
        };
        app.main.push_line(
            format!(
                "Working in `{}`. Ask a question, request a change, or type `/` for commands.",
                app.project_root.display()
            ),
            LineStyle::Status,
            None,
        );
        app
    }

    /// Load and keep the prompt history in `path` (one JSON string per line).
    pub fn with_history_file(mut self, path: PathBuf) -> Self {
        let (entries, raw) = load_history(&path);
        if raw > HISTORY_LIMIT * 2 {
            rewrite_history(&path, &entries);
        }
        self.input_history = entries;
        self.history_file = Some(path);
        self
    }

    fn notify(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), Instant::now()));
    }

    pub fn add_user_message(&mut self, message: String) {
        self.main.push_line(message, LineStyle::User, None);
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
        let event = match self.main.apply(event) {
            Ok(status) => {
                if let Some(status) = status {
                    self.status = status;
                }
                return;
            }
            Err(event) => event,
        };
        match event {
            AgentEvent::SubAgentStarted { id, name, task } => {
                self.subagent_started(id, name, &task)
            }
            AgentEvent::SubAgent { id, event } => self.subagent_event(&id, *event),
            AgentEvent::SubAgentFinished {
                id,
                success,
                summary,
            } => self.subagent_finished(&id, success, &summary),
            AgentEvent::Done { usage } => {
                self.is_processing = false;
                self.main.is_thinking = false;
                self.main.stop_running_tools();
                self.active_skill = None;
                // Nothing is waiting for an answer once the run is over.
                self.approvals.retain(|a| a.agent.is_some());
                let elapsed = self.run_started.take().map(|t| t.elapsed());
                if let Some(ref u) = usage {
                    self.add_usage(u);
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
                if let Some(t) = elapsed.filter(|t| *t >= LONG_RUN) {
                    self.bell_pending = true;
                    self.notify(format!("Finished in {}", format_duration(t)));
                }
            }
            AgentEvent::ModelSwitched { model, provider } => {
                self.model_info = format!("{model} / {provider}");
            }
            AgentEvent::ClearChat => {
                self.main.clear();
                self.subagents.retain(SubAgentView::is_running);
                self.focus = None;
                self.split_agent = 0;
                self.selected_tool_index = 0;
                self.notify("Chat cleared");
            }
            AgentEvent::Quit => self.should_quit = true,
            AgentEvent::ApprovalRequest {
                id,
                tool_name,
                args_preview,
            } => self.approvals.push_back(PendingApproval {
                info: ApprovalInfo {
                    id,
                    tool_name,
                    args_preview,
                },
                agent: None,
            }),
            AgentEvent::TriggeredSkill(name) => self.active_skill = Some(name),
            AgentEvent::AgentMessage { from, to, text } => {
                self.agent_message(None, &from, &to, &text)
            }
            // Transcript events were handled by `Transcript::apply`.
            _ => {}
        }
    }

    fn add_usage(&mut self, u: &crate::ai::types::UsageStats) {
        self.total_tokens_used += u.total_tokens as u64;
        self.prompt_tokens_used += u.prompt_tokens as u64;
        self.completion_tokens_used += u.completion_tokens as u64;
    }

    fn subagent_index(&self, id: &str) -> Option<usize> {
        self.subagents.iter().position(|s| s.id == id)
    }

    fn subagent_started(&mut self, id: String, name: String, task: &str) {
        match self.subagent_index(&id) {
            // Its first events overtook the start notice.
            Some(i) => {
                let sub = &mut self.subagents[i];
                sub.transcript.agent_name = Some(name.clone());
                sub.transcript.render_cache.borrow_mut().clear();
                sub.name = name.clone();
            }
            None => self
                .subagents
                .push(SubAgentView::new(id, name.clone(), task)),
        }
        let n = self.subagents.len();
        self.notify(if n <= 9 {
            format!("Sub-agent {name} started · Alt+{n} to watch")
        } else {
            format!("Sub-agent {name} started · /agents {n} to watch")
        });
    }

    /// Route an event a sub-agent produced into its own transcript.
    fn subagent_event(&mut self, id: &str, event: AgentEvent) {
        let idx = match self.subagent_index(id) {
            Some(i) => i,
            None => {
                self.subagents
                    .push(SubAgentView::new(id.to_string(), id.to_string(), ""));
                self.subagents.len() - 1
            }
        };
        let sub = &mut self.subagents[idx];
        match sub.transcript.apply(event) {
            Ok(Some(status)) => sub.activity = status,
            Ok(None) => {}
            Err(AgentEvent::ApprovalRequest {
                id: call_id,
                tool_name,
                args_preview,
            }) => {
                sub.activity = format!("waiting for approval: {tool_name}");
                let agent = Some((sub.id.clone(), sub.name.clone()));
                self.approvals.push_back(PendingApproval {
                    info: ApprovalInfo {
                        id: call_id,
                        tool_name,
                        args_preview,
                    },
                    agent,
                });
            }
            Err(AgentEvent::Done { usage }) => {
                sub.transcript.is_thinking = false;
                if let Some(u) = usage {
                    sub.tokens += u.total_tokens as u64;
                    self.add_usage(&u);
                }
            }
            Err(AgentEvent::AgentMessage { from, to, text }) => {
                self.agent_message(Some(idx), &from, &to, &text)
            }
            Err(_) => {}
        }
    }

    /// Show a message between agents in the sender's transcript and, when it
    /// is someone else's, the recipient's. `sender` is None for main.
    fn agent_message(&mut self, sender: Option<usize>, from: &str, to: &str, text: &str) {
        let line = format!("**{from} → {to}:** {text}");
        let recipient = if to.trim().eq_ignore_ascii_case("main") {
            Some(None)
        } else {
            self.subagents
                .iter()
                .position(|s| s.name.eq_ignore_ascii_case(to.trim()) || s.id == to.trim())
                .map(Some)
        };
        self.transcript_mut(sender)
            .push_line(line.clone(), LineStyle::Message, None);
        if let Some(view) = recipient.filter(|&r| r != sender) {
            self.transcript_mut(view)
                .push_line(line, LineStyle::Message, None);
        }
    }

    /// `/msg <agent> <text>`: drop a note into a running sub-agent.
    fn send_note(&mut self, to: &str, text: &str) {
        match crate::agent::subagent::deliver("user", to, text) {
            Ok(_) => {
                let view = match self.resolve_agent(to) {
                    Some(i) => Some(i),
                    None if to.eq_ignore_ascii_case("main") => None,
                    None => {
                        self.notify(format!("Sent to {to}"));
                        return;
                    }
                };
                self.transcript_mut(view).push_line(
                    format!("**you → {to}:** {text}"),
                    LineStyle::Message,
                    None,
                );
                self.notify(format!("Sent to {to}"));
            }
            Err(e) => self.notify(e),
        }
    }

    fn subagent_finished(&mut self, id: &str, success: bool, summary: &str) {
        let Some(i) = self.subagent_index(id) else {
            return;
        };
        let sub = &mut self.subagents[i];
        sub.state = if success {
            AgentState::Done
        } else {
            AgentState::Failed
        };
        sub.elapsed = Some(sub.started.elapsed());
        sub.transcript.is_thinking = false;
        sub.transcript.stop_running_tools();
        let took = format_duration(sub.elapsed());
        let tools = sub.transcript.tool_log.len();
        if success {
            sub.activity = format!("finished · {tools} tool calls");
            sub.transcript
                .push_line(format!("Finished in {took}."), LineStyle::Status, None);
        } else {
            sub.activity = compact_detail(summary, 160);
            sub.transcript
                .push_line(summary.to_string(), LineStyle::Error, None);
        }
        let name = sub.name.clone();
        self.approvals
            .retain(|a| a.agent.as_ref().is_none_or(|(aid, _)| aid != id));
        self.notify(if success {
            format!("Sub-agent {name} finished in {took}")
        } else {
            format!("Sub-agent {name} stopped: {}", compact_detail(summary, 80))
        });
    }

    /// Answer the oldest approval request; says where the answer must go.
    pub fn answer_approval(&mut self, approved: bool) -> Option<ApprovalAnswer> {
        let pending = self.approvals.pop_front()?;
        let verdict = if approved { "Approved" } else { "Denied" };
        Some(match pending.agent {
            Some((id, name)) => {
                if let Some(i) = self.subagent_index(&id) {
                    self.subagents[i].activity = format!("{verdict} {}", pending.info.tool_name);
                }
                self.notify(format!("{verdict} for {name}"));
                ApprovalAnswer::SubAgent { id, approved }
            }
            None => {
                self.notify(verdict);
                ApprovalAnswer::Main(approved)
            }
        })
    }

    // ── Views ─────────────────────────────────────────────────────────────

    fn transcript(&self, view: Option<usize>) -> &Transcript {
        match view.and_then(|i| self.subagents.get(i)) {
            Some(sub) => &sub.transcript,
            None => &self.main,
        }
    }

    fn transcript_mut(&mut self, view: Option<usize>) -> &mut Transcript {
        match view {
            Some(i) if i < self.subagents.len() => &mut self.subagents[i].transcript,
            _ => &mut self.main,
        }
    }

    /// The transcript the keyboard works on.
    pub fn focused(&self) -> &Transcript {
        self.transcript(self.focus)
    }

    fn split_active(&self) -> bool {
        self.split && !self.subagents.is_empty()
    }

    /// Which transcript a chat pane shows.
    fn pane_view(&self, pane: usize) -> Option<usize> {
        match (pane, self.split_active()) {
            (0, true) => None,
            (0, false) => self.focus,
            _ => Some(self.split_agent.min(self.subagents.len().saturating_sub(1))),
        }
    }

    /// The pane showing the focused transcript.
    fn focused_pane(&self) -> usize {
        usize::from(self.split_active() && self.focus.is_some())
    }

    fn view_busy(&self, view: Option<usize>) -> bool {
        match view.and_then(|i| self.subagents.get(i)) {
            Some(sub) => sub.is_running(),
            None => self.is_processing,
        }
    }

    pub fn set_focus(&mut self, focus: Option<usize>) {
        let focus = focus.filter(|&i| i < self.subagents.len());
        if let Some(i) = focus {
            self.split_agent = i;
        }
        if focus != self.focus {
            self.focus = focus;
            self.selected_tool_index = 0;
            self.show_tool_details = false;
            self.clear_selection();
        }
    }

    /// Step through main, sub-agent 1, sub-agent 2, … and around.
    fn cycle_focus(&mut self, dir: isize) {
        if self.subagents.is_empty() {
            self.notify("No sub-agents yet");
            return;
        }
        let slots = self.subagents.len() as isize + 1;
        let current = self.focus.map_or(0, |i| i as isize + 1);
        let next = (current + dir).rem_euclid(slots);
        self.set_focus(if next == 0 {
            None
        } else {
            Some(next as usize - 1)
        });
    }

    /// Alt+0 shows main, Alt+N sub-agent N.
    fn focus_number(&mut self, n: usize) {
        if n == 0 {
            self.set_focus(None);
        } else if n <= self.subagents.len() {
            self.set_focus(Some(n - 1));
        } else {
            self.notify(format!("No sub-agent {n}"));
        }
    }

    fn toggle_split(&mut self) {
        if self.subagents.is_empty() {
            self.notify("Split view shows a sub-agent beside main — none started yet");
            return;
        }
        self.split = !self.split;
        if self.split && self.focus.is_none() {
            self.split_agent = self.subagents.len() - 1;
        }
        self.clear_selection();
        self.notify(if self.split {
            "Split view on (Ctrl+S to turn off)"
        } else {
            "Split view off"
        });
    }

    /// Stop sub-agent `idx` (the event loop does the actual cancel).
    fn cancel_subagent(&mut self, idx: Option<usize>) {
        let Some(sub) = idx.and_then(|i| self.subagents.get_mut(i)) else {
            self.notify("Show a sub-agent first (Alt+→ or /agents N)");
            return;
        };
        if !sub.is_running() {
            let name = sub.name.clone();
            self.notify(format!("{name} is not running"));
            return;
        }
        sub.activity = "Stopping…".into();
        self.subagent_cancels.push(sub.id.clone());
        let name = sub.name.clone();
        self.notify(format!("Stopping {name}…"));
    }

    /// A sub-agent by 1-based number or (prefix of its) name.
    fn resolve_agent(&self, which: &str) -> Option<usize> {
        if let Ok(n) = which.parse::<usize>() {
            return n.checked_sub(1).filter(|&i| i < self.subagents.len());
        }
        let which = which.to_lowercase();
        self.subagents
            .iter()
            .position(|s| s.name.to_lowercase() == which)
            .or_else(|| {
                self.subagents
                    .iter()
                    .position(|s| s.name.to_lowercase().starts_with(&which))
            })
    }

    fn list_agents(&mut self) {
        let text = if self.subagents.is_empty() {
            "No sub-agents yet. The agent starts them (spawn_agent) for independent parts of a \
             bigger job; ask for it, e.g. \"use sub-agents for the tests and the docs\"."
                .to_string()
        } else {
            let mut text = String::from("Sub-agents:\n");
            for (i, s) in self.subagents.iter().enumerate() {
                text.push_str(&format!(
                    "- {} **{}** — {}, {}\n",
                    i + 1,
                    s.name,
                    s.state_word(),
                    format_duration(s.elapsed())
                ));
            }
            text.push_str(
                "\n`Alt+1…9` or `/agents N` to watch one, `Ctrl+S` to split, `Ctrl+X` to stop one.",
            );
            text
        };
        self.set_focus(None);
        self.main.push_line(text, LineStyle::Status, None);
    }

    /// Commands the TUI handles itself. Returns true when `input` was one.
    fn local_command(&mut self, input: &str) -> bool {
        let parts: Vec<&str> = input.split_whitespace().collect();
        match parts.as_slice() {
            ["/msg"] | ["/msg", _] => self.notify("Usage: /msg <agent> <text>"),
            ["/msg", to, ..] => {
                let text = input
                    .trim_start()
                    .trim_start_matches("/msg")
                    .trim_start()
                    .trim_start_matches(to)
                    .trim();
                self.send_note(to, text);
            }
            ["/keys"] => {
                self.show_help = true;
                self.help_scroll = 0;
            }
            ["/copy", "code"] => self.copy_last_code_block(),
            ["/agents"] => self.list_agents(),
            ["/agents", "split"] => self.toggle_split(),
            ["/agents", "stop"] => self.cancel_subagent(self.focus),
            ["/agents", "stop", which] => self.cancel_subagent(self.resolve_agent(which)),
            ["/agents", "main" | "0"] => self.set_focus(None),
            ["/agents", which] => match self.resolve_agent(which) {
                Some(i) => self.set_focus(Some(i)),
                None => self.notify(format!("No sub-agent \"{which}\"")),
            },
            _ => return false,
        }
        true
    }

    // ── Scrolling ─────────────────────────────────────────────────────────

    fn scroll_pane(&mut self, pane: usize, delta: isize) {
        let max = self.panes[pane].max_scroll.get();
        let view = self.pane_view(pane);
        let t = self.transcript_mut(view);
        t.scroll = (t.scroll as isize + delta).clamp(0, max as isize) as usize;
    }

    fn scroll_chat(&mut self, delta: isize) {
        self.scroll_pane(self.focused_pane(), delta);
    }

    fn scroll_to(&mut self, top: bool) {
        let max = self.panes[self.focused_pane()].max_scroll.get();
        let focus = self.focus;
        self.transcript_mut(focus).scroll = if top { max } else { 0 };
    }

    fn page(&self) -> isize {
        (self.panes[self.focused_pane()].area.get().height as isize / 2).max(3)
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
        let pane = &self.panes[self.selection_pane];
        let area = pane.area.get();
        let (p1, p2) = if (start.1, start.0) <= (end.1, end.0) {
            (start, end)
        } else {
            (end, start)
        };
        let visible = pane.visible_plain.borrow();
        let meta = pane.visible_meta.borrow();
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

    fn copy_text(&mut self, text: String, done: impl Into<String>) -> bool {
        match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text)) {
            Ok(()) => {
                self.notify(done);
                true
            }
            Err(_) => {
                self.notify("Clipboard unavailable");
                false
            }
        }
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
        self.copy_text(text, "Copied — Esc clears the highlight")
    }

    fn copy_tool_output(&mut self) {
        let Some(entry) = self
            .focused()
            .tool_log
            .iter()
            .rev()
            .nth(self.selected_tool_index)
        else {
            return;
        };
        let text = clean_paths(&entry.output, &self.project_root);
        let done = format!("Copied {} output", entry.name);
        self.copy_text(text, done);
    }

    fn copy_last_reply(&mut self) {
        match self.focused().last_reply().map(str::to_string) {
            Some(text) => {
                self.copy_text(text, "Copied the last reply");
            }
            None => self.notify("No reply to copy yet"),
        }
    }

    fn copy_last_code_block(&mut self) {
        match self.focused().last_code_block() {
            Some(code) => {
                let lines = code.lines().count();
                self.copy_text(code, format!("Copied the last code block ({lines} lines)"));
            }
            None => self.notify("No code block to copy"),
        }
    }

    /// Text from the clipboard or a bracketed paste.
    fn paste_text(&mut self, text: &str) {
        self.history_search = None;
        self.insert_text(&text.replace("\r\n", "\n"));
    }

    /// A key that arrived as part of a paste burst. Returns false for keys
    /// that should keep their normal meaning.
    fn paste_key(&mut self, key: KeyCode, modifiers: KeyModifiers) -> bool {
        if modifiers.contains(KeyModifiers::CONTROL) && !modifiers.contains(KeyModifiers::ALT) {
            return false;
        }
        match key {
            KeyCode::Enter => self.paste_text("\n"),
            KeyCode::Tab => self.paste_text("    "),
            KeyCode::Char(c) => self.paste_text(&c.to_string()),
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

    pub fn handle_mouse(&mut self, kind: MouseEventKind, column: u16, row: u16) {
        let inside = |r: Rect| {
            r.width > 0
                && column >= r.x
                && column < r.x + r.width
                && row >= r.y
                && row < r.y + r.height
        };
        let over_tools = inside(self.tools_area.get());
        // Include the chat's left padding so a drag can start at the margin.
        let pane_at = (0..2).find(|&p| {
            let a = self.panes[p].area.get();
            a.width > 0 && inside(Rect::new(a.x.saturating_sub(2), a.y, a.width + 2, a.height))
        });
        match kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let strip = self.strip_area.get();
                if over_tools {
                    self.active_panel = ActivePanel::Activity;
                    self.clear_selection();
                } else if inside(self.input_box.get()) {
                    self.click_input(column, row);
                } else if inside(strip) {
                    let r = (row - strip.y) as usize;
                    if r == 0 {
                        self.set_focus(None);
                    } else {
                        self.set_focus(Some(self.strip_first.get() + r - 1));
                    }
                } else if let Some(p) = pane_at {
                    let view = self.pane_view(p);
                    self.set_focus(view);
                    self.active_panel = ActivePanel::Chat;
                    self.selection_pane = p;
                    self.selection_start = Some((column, row));
                    self.selection_end = Some((column, row));
                    self.is_selecting = true;
                } else {
                    self.clear_selection();
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                if let Ok(text) = arboard::Clipboard::new().and_then(|mut cb| cb.get_text()) {
                    self.paste_text(&text);
                }
            }
            MouseEventKind::Drag(_) if self.is_selecting => {
                self.selection_end = Some((column, row));
            }
            MouseEventKind::Up(_) if self.is_selecting => {
                self.selection_end = Some((column, row));
                self.is_selecting = false;
                if self.selection_start != self.selection_end {
                    // Copy right away but keep the highlight visible.
                    self.copy_selection(false);
                } else {
                    self.clear_selection();
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = kind == MouseEventKind::ScrollUp;
                if over_tools {
                    let last = self.focused().tool_log.len().saturating_sub(1);
                    self.selected_tool_index = if up {
                        self.selected_tool_index.saturating_sub(1)
                    } else {
                        (self.selected_tool_index + 1).min(last)
                    };
                } else {
                    let pane = pane_at.unwrap_or_else(|| self.focused_pane());
                    self.scroll_pane(pane, if up { 3 } else { -3 });
                }
            }
            _ => {}
        }
    }

    /// Scroll so the previous (dir < 0) or next message starts at the top.
    fn jump_message(&mut self, dir: isize) {
        let pane = &self.panes[self.focused_pane()];
        let starts = pane.message_starts.borrow().clone();
        let total = pane.total_lines.get();
        let view = pane.view_height.get();
        let max = pane.max_scroll.get();
        let focus = self.focus;
        let t = self.transcript_mut(focus);
        let top = total.saturating_sub(view + t.scroll.min(max));
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
                    t.scroll = 0;
                    return;
                }
            }
        };
        t.scroll = total.saturating_sub(view + target).min(max);
    }

    fn copy_transcript(&mut self) {
        let t = self.focused();
        let text = t
            .lines
            .iter()
            .filter(|l| !matches!(l.style, LineStyle::Status | LineStyle::Thinking))
            .map(|l| {
                let label = match (&t.agent_name, l.style) {
                    (Some(name), LineStyle::Assistant) => format!("◆ {name}"),
                    _ => label_for(l.style).1.to_string(),
                };
                format!("{label}:\n{}", l.content)
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        self.copy_text(text, "Copied the whole conversation");
    }

    /// Commands matching what's typed so far (only while typing the command
    /// word): prefix matches first, then fuzzy (in-order letters) matches.
    fn command_suggestions(&self) -> Vec<(&'static str, &'static str)> {
        if !self.input.starts_with('/')
            || self.input.contains('\n')
            || self.history_search.is_some()
        {
            return Vec::new();
        }
        let typed = self.input.to_lowercase();
        let mut ranked: Vec<(usize, usize, (&'static str, &'static str))> = SLASH_COMMANDS
            .iter()
            .copied()
            .filter(|(cmd, _)| *cmd != typed)
            .filter_map(|entry| {
                if entry.0.starts_with(&typed) {
                    Some((0, 0, entry))
                } else if !typed.contains(char::is_whitespace) {
                    fuzzy_span(entry.0, &typed).map(|span| (1, span, entry))
                } else {
                    None
                }
            })
            .collect();
        ranked.sort_by_key(|&(tier, span, _)| (tier, span));
        ranked.into_iter().map(|(_, _, entry)| entry).collect()
    }

    /// The command Enter should run instead of the typed text: the one picked
    /// in the `/` menu, or the only match.
    fn palette_choice(&self) -> Option<&'static str> {
        let suggestions = self.command_suggestions();
        if suggestions.is_empty() {
            return None;
        }
        let typed = self.input.trim();
        let exact = SLASH_COMMANDS.iter().any(|(cmd, _)| *cmd == typed);
        if self.palette_moved || (suggestions.len() == 1 && !exact) {
            Some(suggestions[self.palette_index.min(suggestions.len() - 1)].0)
        } else {
            None
        }
    }

    fn remember(&mut self, message: &str) {
        if self.input_history.last().map(String::as_str) == Some(message) {
            return;
        }
        self.input_history.retain(|m| m != message);
        self.input_history.push(message.to_string());
        if self.input_history.len() > HISTORY_LIMIT {
            let extra = self.input_history.len() - HISTORY_LIMIT;
            self.input_history.drain(..extra);
        }
        if let Some(path) = &self.history_file {
            append_history(path, message);
        }
    }

    // ── History search (Ctrl+R) ───────────────────────────────────────────

    fn start_history_search(&mut self) {
        if self.input_history.is_empty() {
            self.notify("No prompt history yet");
            return;
        }
        self.history_search = Some(HistorySearch {
            query: String::new(),
            skip: 0,
            draft: self.input.clone(),
            found: true,
        });
        self.update_history_search();
    }

    /// Show the newest prompt containing the query (after `skip` older ones).
    fn update_history_search(&mut self) {
        let Some(search) = &self.history_search else {
            return;
        };
        let query = search.query.to_lowercase();
        let matches: Vec<usize> = (0..self.input_history.len())
            .rev()
            .filter(|&i| self.input_history[i].to_lowercase().contains(&query))
            .collect();
        let skip = search.skip.min(matches.len().saturating_sub(1));
        let text = matches.get(skip).map(|&i| self.input_history[i].clone());
        if let Some(search) = self.history_search.as_mut() {
            search.skip = skip;
            search.found = text.is_some();
        }
        self.set_input(text.unwrap_or_default());
    }

    /// Leave the search and put back what was typed before it.
    fn cancel_history_search(&mut self) {
        if let Some(search) = self.history_search.take() {
            self.set_input(search.draft);
        }
    }

    fn history_search_key(&mut self, key: KeyCode, modifiers: KeyModifiers) -> Option<String> {
        let ctrl = modifiers.contains(KeyModifiers::CONTROL);
        let alt = modifiers.contains(KeyModifiers::ALT);
        let search = self.history_search.as_mut()?;
        match key {
            KeyCode::Char('r') if ctrl => {
                search.skip += 1;
                self.update_history_search();
            }
            KeyCode::Esc => self.cancel_history_search(),
            KeyCode::Char('g' | 'c') if ctrl => self.cancel_history_search(),
            KeyCode::Char(c) if !ctrl || alt => {
                search.query.push(c);
                search.skip = 0;
                self.update_history_search();
            }
            KeyCode::Backspace => {
                search.query.pop();
                search.skip = 0;
                self.update_history_search();
            }
            KeyCode::Enter => {
                self.history_search = None;
                return self.handle_key_inner(KeyCode::Enter, KeyModifiers::NONE);
            }
            // Anything else keeps the match for editing.
            _ => self.history_search = None,
        }
        None
    }

    pub fn handle_key(&mut self, key: KeyCode, modifiers: KeyModifiers) -> Option<String> {
        let before = self.input.clone();
        let out = self.handle_key_inner(key, modifiers);
        if self.input != before {
            self.palette_index = 0;
            self.palette_moved = false;
        }
        out
    }

    fn handle_key_inner(&mut self, key: KeyCode, modifiers: KeyModifiers) -> Option<String> {
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
            match key {
                KeyCode::Esc | KeyCode::Enter | KeyCode::F(1) | KeyCode::Char('q' | '?') => {
                    self.show_help = false
                }
                KeyCode::Up => self.help_scroll = self.help_scroll.saturating_sub(1),
                KeyCode::Down => self.help_scroll = self.help_scroll.saturating_add(1),
                KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(10),
                KeyCode::PageDown => self.help_scroll = self.help_scroll.saturating_add(10),
                _ => {}
            }
            return None;
        }
        if key == KeyCode::F(1) || (key == KeyCode::Char('?') && self.input.is_empty()) {
            self.show_help = true;
            self.help_scroll = 0;
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

        if self.history_search.is_some() {
            return self.history_search_key(key, modifiers);
        }

        // Agent views work from both panels.
        if self.agent_view_key(key, ctrl, alt) {
            return None;
        }

        let suggestions = self.command_suggestions().len();
        if key == KeyCode::Tab && suggestions == 0 {
            self.active_panel = match self.active_panel {
                ActivePanel::Chat => ActivePanel::Activity,
                ActivePanel::Activity => ActivePanel::Chat,
            };
            return None;
        }

        if self.active_panel == ActivePanel::Activity {
            let last = self.focused().tool_log.len().saturating_sub(1);
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
                KeyCode::Enter if !self.focused().tool_log.is_empty() => {
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
            KeyCode::Char('y') if ctrl => self.copy_last_reply(),
            KeyCode::Char('y') if alt => self.copy_last_code_block(),
            KeyCode::Char('r') if ctrl => self.start_history_search(),
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
                    self.paste_text(&text);
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
                let message = match self.palette_choice() {
                    Some(cmd) => cmd.to_string(),
                    None => self.input.trim().to_string(),
                };
                if message.is_empty() {
                    return None;
                }
                self.remember(&message);
                self.history_pos = None;
                self.set_input(String::new());
                self.clear_selection();

                if message.starts_with('/') && self.local_command(&message) {
                    return None;
                }
                // The reply comes from the main agent: show it.
                if !self.split_active() {
                    self.set_focus(None);
                }
                if self.is_processing {
                    self.main.push_line(message.clone(), LineStyle::User, None);
                    self.user_backlog.push_back(message);
                    self.notify(format!("Queued — {} waiting", self.user_backlog.len()));
                    return None;
                }
                self.add_user_message(message.clone());
                return Some(message);
            }
            KeyCode::Tab => {
                let suggestions = self.command_suggestions();
                if let Some((cmd, _)) =
                    suggestions.get(self.palette_index.min(suggestions.len() - 1))
                {
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
                } else if self.focus.is_some() && !self.split_active() {
                    self.set_focus(None);
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
            KeyCode::Home if ctrl || self.input.is_empty() => self.scroll_to(true),
            KeyCode::End if ctrl || self.input.is_empty() => self.scroll_to(false),
            KeyCode::Home => self.cursor_pos = 0,
            KeyCode::End => self.cursor_pos = char_count(&self.input),
            KeyCode::Up if alt => self.jump_message(-1),
            KeyCode::Down if alt => self.jump_message(1),
            KeyCode::Up if shift => self.scroll_chat(1),
            KeyCode::Down if shift => self.scroll_chat(-1),
            KeyCode::Up | KeyCode::Down if suggestions > 0 => {
                let last = suggestions - 1;
                let current = self.palette_index.min(last);
                self.palette_index = if key == KeyCode::Up {
                    current.checked_sub(1).unwrap_or(last)
                } else if current == last {
                    0
                } else {
                    current + 1
                };
                self.palette_moved = true;
            }
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
            KeyCode::PageUp => self.scroll_chat(self.page()),
            KeyCode::PageDown => self.scroll_chat(-self.page()),
            _ => {}
        }
        None
    }

    /// Keys that switch, split or stop agent views. Returns true if handled.
    fn agent_view_key(&mut self, key: KeyCode, ctrl: bool, alt: bool) -> bool {
        match key {
            KeyCode::Left if alt && !ctrl => self.cycle_focus(-1),
            KeyCode::Right if alt && !ctrl => self.cycle_focus(1),
            KeyCode::Char(c @ '0'..='9') if alt && !ctrl => {
                self.focus_number(c as usize - '0' as usize)
            }
            KeyCode::Char('s') if ctrl => self.toggle_split(),
            KeyCode::F(2) => self.toggle_split(),
            KeyCode::Char('x') if ctrl => self.cancel_subagent(self.focus),
            _ => return false,
        }
        true
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
}

/// Width of the shortest stretch of `candidate` holding the letters of
/// `typed` in order, or None when they don't all appear.
fn fuzzy_span(candidate: &str, typed: &str) -> Option<usize> {
    let cand: Vec<char> = candidate.to_lowercase().chars().collect();
    let pattern: Vec<char> = typed.trim_start_matches('/').chars().collect();
    if pattern.is_empty() {
        return None;
    }
    let mut best: Option<usize> = None;
    for start in 0..cand.len() {
        if cand[start] != pattern[0] {
            continue;
        }
        let mut p = 1;
        let mut end = start;
        for (i, &c) in cand.iter().enumerate().skip(start + 1) {
            if p == pattern.len() {
                break;
            }
            if c == pattern[p] {
                p += 1;
                end = i;
            }
        }
        if p == pattern.len() {
            let span = end - start + 1;
            best = Some(best.map_or(span, |b| b.min(span)));
        }
    }
    best
}

// ── Prompt history file ───────────────────────────────────────────────────

/// Entries (oldest first, newest copy of each kept) and the raw line count.
fn load_history(path: &Path) -> (Vec<String>, usize) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (Vec::new(), 0);
    };
    let raw: Vec<String> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<String>(l).ok())
        .collect();
    let mut seen = HashSet::new();
    let mut entries: Vec<String> = raw
        .iter()
        .rev()
        .filter(|m| seen.insert(m.as_str()))
        .take(HISTORY_LIMIT)
        .cloned()
        .collect();
    entries.reverse();
    (entries, raw.len())
}

fn append_history(path: &Path, message: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(line) = serde_json::to_string(message) else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

fn rewrite_history(path: &Path, entries: &[String]) {
    let text: String = entries
        .iter()
        .filter_map(|m| serde_json::to_string(m).ok())
        .map(|l| l + "\n")
        .collect();
    let _ = std::fs::write(path, text);
}

fn label_for(style: LineStyle) -> (ThemeColor, &'static str) {
    match style {
        LineStyle::User => (ThemeColor::User, "› You"),
        LineStyle::Task => (ThemeColor::User, "› Task"),
        LineStyle::Assistant => (ThemeColor::Accent, "◆ Norvexum"),
        LineStyle::Thinking => (ThemeColor::Thinking, "∴ Reasoning"),
        LineStyle::Error => (ThemeColor::Error, "✗ Error"),
        LineStyle::Warning => (ThemeColor::Tool, "! Notice"),
        LineStyle::FileWrite => (ThemeColor::File, "✎ File"),
        LineStyle::Status => (ThemeColor::Dim, "• Norvexum"),
        LineStyle::Message => (ThemeColor::Thinking, "⇄ Message"),
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
    let strip_rows = agents_strip_height(app, area.height, input_rows);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(strip_rows),
            Constraint::Min(4),
            Constraint::Length(input_rows + 2),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(frame, rows[0], app);
    if strip_rows > 0 {
        draw_agents(frame, rows[1], app);
    } else {
        app.strip_area.set(Rect::default());
    }

    app.panes[1].area.set(Rect::default());
    app.tools_area.set(Rect::default());
    let body = rows[2];
    if app.split_active() {
        let widths: &[u16] = if area.width >= 140 {
            &[36, 36, 28]
        } else {
            &[50, 50]
        };
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(widths.iter().map(|&w| Constraint::Percentage(w)))
            .split(body);
        draw_chat(frame, columns[0], app, 0);
        draw_chat(frame, columns[1], app, 1);
        if let Some(&tools) = columns.get(2) {
            draw_tools(frame, tools, app);
        }
    } else if area.width >= 100 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(72), Constraint::Percentage(28)])
            .split(body);
        draw_chat(frame, columns[0], app, 0);
        draw_tools(frame, columns[1], app);
    } else {
        draw_chat(frame, body, app, 0);
    }

    draw_input(frame, rows[3], app);
    draw_footer(frame, rows[4], app);
    draw_suggestions(frame, rows[3], app);

    if let Some(pending) = app.approvals.front() {
        draw_approval(frame, area, app, pending);
    }
    if app.show_tool_details {
        draw_tool_details(frame, area, app);
    }
    if app.show_help {
        draw_help(frame, area, app);
    }
    if let (Some(start), Some(end)) = (app.selection_start, app.selection_end) {
        highlight_selection(frame, start, end, app.panes[app.selection_pane].area.get());
    }
}

fn draw_help(frame: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let mut lines: Vec<Line> = KEY_HELP
        .iter()
        .map(|(keys, action)| {
            Line::from(vec![
                Span::styled(format!("{keys:<24}"), Style::default().fg(theme.fg)),
                Span::styled(*action, Style::default().fg(theme.dim_color)),
            ])
        })
        .collect();
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "COMMANDS",
        Style::default()
            .fg(theme.accent_color)
            .add_modifier(Modifier::BOLD),
    )));
    for (cmd, action) in SLASH_COMMANDS {
        lines.push(Line::from(vec![
            Span::styled(format!("{cmd:<24}"), Style::default().fg(theme.fg)),
            Span::styled(*action, Style::default().fg(theme.dim_color)),
        ]));
    }

    let height = (lines.len() as u16 + 4).min(area.height);
    let width = 72.min(area.width);
    let modal = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let visible = height.saturating_sub(4);
    let scroll = app
        .help_scroll
        .min((lines.len() as u16).saturating_sub(visible));
    let hint = if (lines.len() as u16) > visible {
        " ↑↓ scroll · Esc to close "
    } else {
        " Esc to close "
    };
    frame.render_widget(Clear, modal);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .scroll((scroll, 0))
            .block(
                Block::default()
                    .title(Span::styled(
                        " Keys ",
                        Style::default()
                            .fg(theme.accent_color)
                            .add_modifier(Modifier::BOLD),
                    ))
                    .title_bottom(Span::styled(hint, Style::default().fg(theme.dim_color)))
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
        let mut tokens = format!("  · {} tokens", format_tokens(app.total_tokens_used));
        if area.width >= 120 && app.prompt_tokens_used + app.completion_tokens_used > 0 {
            tokens.push_str(&format!(
                " ({} in · {} out)",
                format_tokens(app.prompt_tokens_used),
                format_tokens(app.completion_tokens_used)
            ));
        }
        title.push(Span::styled(tokens, Style::default().fg(theme.dim_color)));
    }
    let running = app.subagents.iter().filter(|s| s.is_running()).count();
    if running > 0 {
        title.push(Span::styled(
            format!(
                "  · {running} sub-agent{} running",
                if running == 1 { "" } else { "s" }
            ),
            Style::default().fg(theme.tool_color),
        ));
    }
    if crate::tools::browser::is_open() {
        let busy = crate::tools::browser::agent_busy();
        title.push(Span::styled(
            if busy {
                "  · browser in use"
            } else {
                "  · browser open"
            },
            Style::default().fg(if busy {
                theme.tool_color
            } else {
                theme.dim_color
            }),
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

/// Rows for the agents strip (0 when there are no sub-agents or no room).
fn agents_strip_height(app: &App, height: u16, input_rows: u16) -> u16 {
    if app.subagents.is_empty() {
        return 0;
    }
    // Main row, one row per sub-agent and the bottom border.
    let wanted = (app.subagents.len().min(STRIP_ROWS) + 2) as u16;
    // Header, input box, footer and at least six chat rows come first.
    let room = height.saturating_sub(2 + input_rows + 2 + 1 + 6);
    if room < 3 { 0 } else { wanted.min(room) }
}

/// One row of the agents strip.
struct AgentRow<'a> {
    focused: bool,
    number: String,
    name: &'a str,
    icon: &'a str,
    icon_color: Color,
    activity: String,
    right: String,
}

fn agent_row(row: AgentRow, name_w: usize, width: usize, theme: &Theme) -> Line<'static> {
    let name = if row.name.width() > name_w {
        crate::util::truncate_chars(row.name, name_w.saturating_sub(1))
    } else {
        row.name.to_string()
    };
    let name = format!("{name}{}", " ".repeat(name_w.saturating_sub(name.width())));
    let left_w = 2 + row.number.width() + 1 + name_w + 2 + row.icon.width() + 1;
    let room = width.saturating_sub(left_w + row.right.width() + 2);
    let mut activity = compact_detail(&row.activity, 300);
    if activity.width() > room {
        activity = crate::util::truncate_chars(&activity, room.saturating_sub(1));
    }
    let gap = width.saturating_sub(left_w + activity.width() + row.right.width());
    let (marker, name_style) = if row.focused {
        (
            "▸ ",
            Style::default()
                .fg(theme.accent_color)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        ("  ", Style::default().fg(theme.fg))
    };
    Line::from(vec![
        Span::styled(marker, Style::default().fg(theme.accent_color)),
        Span::styled(
            format!("{} ", row.number),
            Style::default().fg(theme.dim_color),
        ),
        Span::styled(name, name_style),
        Span::raw("  "),
        Span::styled(row.icon.to_string(), Style::default().fg(row.icon_color)),
        Span::raw(" "),
        Span::styled(
            activity,
            Style::default().fg(if row.focused {
                theme.fg
            } else {
                theme.dim_color
            }),
        ),
        Span::raw(" ".repeat(gap)),
        Span::styled(row.right, Style::default().fg(theme.dim_color)),
    ])
}

/// Main plus each sub-agent: state, what it is doing, tokens and time.
fn draw_agents(frame: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let len = app.subagents.len();
    let agent_rows = (area.height as usize).saturating_sub(2);
    let mut first = app.strip_first.get().min(len.saturating_sub(agent_rows));
    if let Some(f) = app.focus {
        if f < first {
            first = f;
        } else if f >= first + agent_rows {
            first = f + 1 - agent_rows;
        }
    }
    app.strip_first.set(first);
    let shown: Vec<usize> = (first..len).take(agent_rows).collect();
    let hidden = len - shown.len();

    let mut block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(theme.border_color))
        .padding(Padding::new(1, 1, 0, 0))
        .title_bottom(
            Line::from(Span::styled(
                " Alt+←→ switch · Ctrl+S split · Ctrl+X stop ",
                Style::default().fg(theme.dim_color),
            ))
            .right_aligned(),
        );
    if hidden > 0 {
        block = block.title_bottom(Span::styled(
            format!(" +{hidden} more "),
            Style::default().fg(theme.dim_color),
        ));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.strip_area.set(inner);

    let spinner = SPINNER[app.spinner_frame % SPINNER.len()];
    let name_w = shown
        .iter()
        .map(|&i| app.subagents[i].name.width())
        .chain([4])
        .max()
        .unwrap_or(4)
        .min(18);
    let width = inner.width as usize;

    let (icon, icon_color) = if app.is_processing {
        (spinner, theme.tool_color)
    } else if app.status == "Error" {
        ("✗", theme.error_color)
    } else {
        ("●", theme.success_color)
    };
    let mut lines = vec![agent_row(
        AgentRow {
            focused: app.focus.is_none(),
            number: "0".into(),
            name: "main",
            icon,
            icon_color,
            activity: app.status.clone(),
            right: app
                .run_started
                .filter(|_| app.is_processing)
                .map(|t| format_duration(t.elapsed()))
                .unwrap_or_default(),
        },
        name_w,
        width,
        theme,
    )];
    for &i in &shown {
        let sub = &app.subagents[i];
        let (icon, icon_color) = match sub.state {
            AgentState::Running => (spinner, theme.tool_color),
            AgentState::Done => ("✓", theme.success_color),
            AgentState::Failed => ("✗", theme.error_color),
        };
        let mut right = format_duration(sub.elapsed());
        if sub.tokens > 0 {
            right = format!("{} tok  {right}", format_tokens(sub.tokens));
        }
        lines.push(agent_row(
            AgentRow {
                focused: app.focus == Some(i),
                number: (i + 1).to_string(),
                name: &sub.name,
                icon,
                icon_color,
                activity: sub.current_activity(&app.project_root),
                right,
            },
            name_w,
            width,
            theme,
        ));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// Title row of a chat pane that isn't the plain main view.
fn pane_title(app: &App, view: Option<usize>, focused: bool) -> Line<'static> {
    let theme = &app.theme;
    let strong = Style::default()
        .fg(if focused {
            theme.accent_color
        } else {
            theme.dim_color
        })
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(theme.dim_color);
    match view.and_then(|i| app.subagents.get(i).map(|s| (i, s))) {
        None => Line::from(vec![
            Span::styled("MAIN", strong),
            Span::styled(format!("  {}", app.status), dim),
        ]),
        Some((i, sub)) => {
            let mut spans = vec![
                Span::styled(format!("SUB-AGENT {} · {}", i + 1, sub.name), strong),
                Span::styled(
                    format!(
                        "  {} · {}",
                        sub.state_word(),
                        format_duration(sub.elapsed())
                    ),
                    dim,
                ),
            ];
            if !app.split_active() {
                spans.push(Span::styled("  ·  Alt+0 back to main", dim));
            }
            Line::from(spans)
        }
    }
}

fn draw_chat(frame: &mut Frame, area: Rect, app: &App, pane_idx: usize) {
    let theme = &app.theme;
    let pane = &app.panes[pane_idx];
    let view = app.pane_view(pane_idx);
    let transcript = app.transcript(view);
    let expanded = app.show_reasoning;
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(theme.border_color))
        .padding(Padding::new(2, 1, 1, 0));
    let mut inner = block.inner(area);
    frame.render_widget(block, area);
    if (app.split_active() || view.is_some()) && inner.height > 2 {
        let focused = app.focused_pane() == pane_idx;
        frame.render_widget(
            Paragraph::new(pane_title(app, view, focused)),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        inner = Rect::new(inner.x, inner.y + 2, inner.width, inner.height - 2);
    }
    pane.area.set(inner);

    let width = inner.width.max(10) as usize;
    let view_height = inner.height as usize;

    // Heights of every message (cached), then only the visible slice is cloned.
    let mut all: Vec<Line<'static>> = Vec::new();
    let mut all_meta: Vec<RowMeta> = Vec::new();
    let mut extra: Vec<Line<'static>> = Vec::new();
    let heights: Vec<usize> = (0..transcript.lines.len())
        .map(|i| transcript.message_height(i, width, theme, expanded))
        .collect();

    if app.view_busy(view)
        && !transcript.is_thinking
        && transcript.lines.last().is_none_or(|l| {
            matches!(
                l.style,
                LineStyle::User | LineStyle::Task | LineStyle::Status
            )
        })
    {
        extra.push(Line::from(Span::styled(
            format!("{} Working…", SPINNER[app.spinner_frame % SPINNER.len()]),
            Style::default().fg(theme.dim_color),
        )));
    }

    let total: usize = heights.iter().sum::<usize>() + extra.len();
    let max_scroll = total.saturating_sub(view_height);
    pane.max_scroll.set(max_scroll);
    pane.total_lines.set(total);
    pane.view_height.set(view_height);
    {
        let mut starts = pane.message_starts.borrow_mut();
        starts.clear();
        let mut offset = 0;
        for (line, h) in transcript.lines.iter().zip(&heights) {
            if matches!(
                line.style,
                LineStyle::User | LineStyle::Task | LineStyle::Assistant
            ) {
                starts.push(offset);
            }
            offset += h;
        }
    }
    let scroll = transcript.scroll.min(max_scroll);
    let start = total.saturating_sub(view_height + scroll);
    let end = (start + view_height).min(total);

    let mut offset = 0;
    for (i, h) in heights.iter().enumerate() {
        if offset + h > start && offset < end {
            let (lines, meta) = transcript.message_rows(i, width, theme, expanded);
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

    *pane.visible_plain.borrow_mut() = all
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    all_meta.resize_with(all.len(), RowMeta::default);
    *pane.visible_meta.borrow_mut() = all_meta;

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

/// One-line summary of a tool call's arguments (activity panel, agents
/// strip and headless output).
pub(crate) fn summarize_args(name: &str, args: &str) -> String {
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
    let tool_log = &app.focused().tool_log;
    let block = Block::default().padding(Padding::new(2, 1, 1, 0));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.tools_area.set(area);

    let mut heading = vec![Span::styled(
        "ACTIVITY",
        Style::default()
            .fg(if focused {
                theme.accent_color
            } else {
                theme.dim_color
            })
            .add_modifier(Modifier::BOLD),
    )];
    if let Some(sub) = app.focus.and_then(|i| app.subagents.get(i)) {
        heading.push(Span::styled(
            format!(" · {}", sub.name),
            Style::default().fg(theme.dim_color),
        ));
    }
    heading.push(Span::styled(
        if focused {
            "  ↑↓ select · Enter details · c copy · Esc back"
        } else {
            "  Tab to browse"
        },
        Style::default().fg(theme.dim_color),
    ));
    let mut lines = vec![Line::from(heading), Line::default()];

    if tool_log.is_empty() {
        lines.push(Line::from(Span::styled(
            "Tool runs will appear here.",
            Style::default().fg(theme.dim_color),
        )));
    }

    let detail_width = inner.width.saturating_sub(4).max(8) as usize;
    let mut selected_row = 0;
    for (i, entry) in tool_log.iter().rev().enumerate() {
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
    let title = if let Some(search) = &app.history_search {
        format!(
            " History search: {}{} ",
            search.query,
            if search.found { "" } else { "  (no match)" }
        )
    } else if !app.user_backlog.is_empty() {
        format!(" Message · {} queued ", app.user_backlog.len())
    } else if app.is_processing {
        " Message (queued until the current run finishes) ".into()
    } else if app.focus.is_some() {
        " Message to the main agent ".into()
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
        let placeholder = if app.history_search.is_some() {
            "Type to search earlier prompts · Ctrl+R older · Esc cancel"
        } else if app.is_processing {
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
    if suggestions.is_empty() || !app.approvals.is_empty() {
        return;
    }
    let shown = suggestions.len().min(7);
    let selected = app.palette_index.min(suggestions.len() - 1);
    let first = (selected + 1).saturating_sub(shown);
    let width = input_area.width.min(60);
    let rect = Rect::new(
        input_area.x + 1,
        input_area.y.saturating_sub(shown as u16 + 2),
        width,
        shown as u16 + 2,
    );
    let lines: Vec<Line> = suggestions
        .iter()
        .enumerate()
        .skip(first)
        .take(shown)
        .map(|(i, (cmd, desc))| {
            let style = if i == selected {
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
    let hint = if suggestions.len() > shown {
        format!(" Tab complete · ↑↓ {} more ", suggestions.len() - shown)
    } else {
        " Tab complete · ↑↓ pick · Enter run ".to_string()
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(app.theme.border_color))
                .title(Span::styled(hint, Style::default().fg(app.theme.dim_color))),
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
    } else if app.history_search.is_some() {
        &[
            ("Ctrl+R", "older"),
            ("Enter", "send"),
            ("→", "edit"),
            ("Esc", "cancel"),
        ]
    } else if app.focus.is_some() {
        &[
            ("Alt+0", "main"),
            ("Alt+←→", "switch"),
            ("Ctrl+X", "stop agent"),
            ("Ctrl+S", "split"),
            ("F1", "keys"),
        ]
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
            ("Ctrl+R", "search history"),
            ("/", "commands"),
            ("Tab", "activity"),
            ("F1", "all keys"),
        ]
    };
    frame.render_widget(Paragraph::new(hint_line(pairs, theme)), area);
}

fn draw_approval(frame: &mut Frame, area: Rect, app: &App, pending: &PendingApproval) {
    let theme = &app.theme;
    let info = &pending.info;
    let modal = centered_rect(64, 50, area);
    frame.render_widget(Clear, modal);

    let args: serde_json::Value = serde_json::from_str(&info.args_preview).unwrap_or_default();
    let mut lines = Vec::new();
    if let Some((_, name)) = &pending.agent {
        lines.push(Line::from(vec![
            Span::styled("Agent ", Style::default().fg(theme.dim_color)),
            Span::styled(
                format!("sub-agent {name}"),
                Style::default()
                    .fg(theme.accent_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
    }
    lines.push(Line::from(vec![
        Span::styled("Tool  ", Style::default().fg(theme.dim_color)),
        Span::styled(
            info.tool_name.clone(),
            Style::default()
                .fg(theme.tool_color)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::default());
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

    let title = match &pending.agent {
        Some((_, name)) => format!(" Allow this tool call by {name}? "),
        None => " Allow this tool call? ".to_string(),
    };
    let mut block = Block::default()
        .title(Span::styled(
            title,
            Style::default()
                .fg(theme.tool_color)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.tool_color))
        .padding(Padding::new(2, 2, 1, 1));
    if app.approvals.len() > 1 {
        block = block.title_bottom(Span::styled(
            format!(" {} more waiting ", app.approvals.len() - 1),
            Style::default().fg(theme.dim_color),
        ));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .block(block)
            .style(Style::default().fg(theme.fg).bg(theme.bg)),
        modal,
    );
}

fn draw_tool_details(frame: &mut Frame, area: Rect, app: &App) {
    let Some(entry) = app
        .focused()
        .tool_log
        .iter()
        .rev()
        .nth(app.selected_tool_index)
    else {
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

fn clean_paths(text: &str, project_root: &Path) -> String {
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
    use crate::agent::subagent;

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
            let busy = app.is_processing || app.subagents.iter().any(SubAgentView::is_running);
            let wait = if dirty {
                FRAME.saturating_sub(last_draw.elapsed())
            } else if busy {
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
                            if !app.approvals.is_empty() {
                                let answer = match key.code {
                                    KeyCode::Char('y' | 'Y') | KeyCode::Enter => Some(true),
                                    KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(false),
                                    _ => None,
                                };
                                match answer.and_then(|a| app.answer_approval(a)) {
                                    Some(ApprovalAnswer::Main(approved)) => {
                                        let _ = approval_tx.send(approved);
                                    }
                                    Some(ApprovalAnswer::SubAgent { id, approved }) => {
                                        subagent::respond_approval(&id, approved);
                                    }
                                    None => {}
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
                        Event::Paste(text) => app.paste_text(&text),
                        Event::Mouse(mouse) => app.handle_mouse(mouse.kind, mouse.column, mouse.row),
                        Event::Resize(..) => {}
                        _ => dirty = false,
                    }
                }
                _ = tokio::time::sleep(wait) => {
                    if busy || app.main.tool_log.iter().any(|t| t.status == ToolStatus::Running) {
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
            for id in app.subagent_cancels.drain(..) {
                subagent::cancel(&id);
            }
            if std::mem::take(&mut app.bell_pending) {
                let backend = terminal.backend_mut();
                let _ = backend.write_all(b"\x07");
                let _ = backend.flush();
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

pub(crate) fn format_duration(d: Duration) -> String {
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

    const MULTIBYTE: [&str; 3] = ["Ayoohi!👋", "héllo wörld 🎉🎉", "日本語テキスト"];

    #[test]
    fn multibyte_replies_render_and_copy_at_any_width() {
        // "Ayoohi!👋" streamed as two deltas crashed an older build.
        let mut app = App::new("m", std::path::PathBuf::new());
        app.add_user_message("héllo 👋".into());
        app.handle_agent_event(AgentEvent::Content("Ayoohi!".into()));
        app.handle_agent_event(AgentEvent::Content("👋".into()));
        app.handle_agent_event(AgentEvent::Content(
            "\n\n## 🎉 héllo\n**wörld 🎉🎉** and `日本語` and *テキ*\n\
             - 👋 bullet\n1. 日本語テキスト\n> ünïcödé 🎉\n| 👋 | 日本 |\n\
             ```\nlet s = \"👋日本\";\n```\n"
                .into(),
        ));
        app.handle_agent_event(AgentEvent::Content("👋".repeat(40)));
        app.handle_agent_event(AgentEvent::ToolStart {
            name: "read_file".into(),
            id: "1".into(),
        });
        app.handle_agent_event(AgentEvent::ToolArgsDelta {
            id: "1".into(),
            delta: r#"{"path":"日本/👋.rs"}"#.into(),
        });
        app.handle_agent_event(AgentEvent::Done { usage: None });

        // Odd widths make two-column glyphs straddle every wrap point.
        for (w, h) in [(120, 40), (61, 40), (47, 30), (33, 30)] {
            let screen = render(&app, w, h);
            assert!(screen.contains("Ayoohi!"), "reply missing at width {w}");
            // Select from every column on the first row to every column on
            // the last, so slices start and end inside wide glyphs.
            for x1 in 0..w {
                app.selection_start = Some((x1, 0));
                app.selection_end = Some((w - 1 - x1, h - 1));
                let _ = app.selection_text();
            }
        }
        app.selection_start = Some((0, 0));
        app.selection_end = Some((119, 39));
        render(&app, 120, 40);
        let text = app.selection_text();
        assert!(
            text.contains("Ayoohi!👋"),
            "emoji lost from the copy:\n{text}"
        );
        assert!(text.contains("1. 日本語テキスト"), "CJK lost:\n{text}");
    }

    #[test]
    fn wrapping_helpers_split_on_character_boundaries() {
        for text in MULTIBYTE {
            for width in 1..=12 {
                let rows = hard_wrap(text, width);
                assert_eq!(rows.concat(), text);
                for row in &rows {
                    // A lone wide glyph may exceed a one-column width.
                    assert!(row.width() <= width.max(2), "{row:?} wider than {width}");
                }

                let spans = parse_inline_markdown(text, Style::default(), LineStyle::Assistant);
                for (line, _) in wrap_spans(spans, width, "  ".into(), "  ".into()) {
                    let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                    assert!(text.contains(joined.trim()), "{joined:?} not from {text:?}");
                }

                let len = text.chars().count();
                for cursor in 0..=len {
                    let (rows, row, col) = layout_input(text, cursor, width);
                    assert_eq!(rows.concat(), text);
                    assert_eq!(cursor_at(text, width, row, col), cursor.min(len));
                }
            }

            let words: String = split_words(text).into_iter().map(|(_, w)| w).collect();
            assert_eq!(words, text);
            let total = text.width();
            for from in 0..=total {
                for to in from..=total + 1 {
                    let part = slice_columns(text, from, to);
                    assert!(text.contains(&part), "{part:?} not from {text:?}");
                }
            }
            for max in 0..=text.chars().count() {
                let _ = compact_detail(text, max);
            }
            assert_eq!(split_numbered(text), None);
            assert!(fuzzy_span(text, "👋日").is_none_or(|n| n > 0));
        }
        assert_eq!(split_numbered("12. 日本"), Some(("12", "日本")));
        assert_eq!(
            extract_streaming_content(r#"{"path":"日本/👋.rs"#, "path"),
            "日本/👋.rs"
        );
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
        assert_eq!(app.main.scroll, 0);
        app.handle_key(KeyCode::Up, KeyModifiers::ALT);
        let after_one = app.main.scroll;
        assert!(after_one > 0);
        render(&app, 70, 20);
        app.handle_key(KeyCode::Up, KeyModifiers::ALT);
        assert!(app.main.scroll > after_one);
        render(&app, 70, 20);
        app.handle_key(KeyCode::Down, KeyModifiers::ALT);
        assert_eq!(app.main.scroll, after_one);
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

    fn sub(id: &str, event: AgentEvent) -> AgentEvent {
        AgentEvent::SubAgent {
            id: id.into(),
            event: Box::new(event),
        }
    }

    fn start_subagent(app: &mut App, id: &str, name: &str) {
        app.handle_agent_event(AgentEvent::SubAgentStarted {
            id: id.into(),
            name: name.into(),
            task: format!("task for {name}"),
        });
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
    }

    #[test]
    fn subagent_events_go_to_their_own_transcript() {
        let mut app = App::new("m", std::path::PathBuf::new());
        app.add_user_message("split the work".into());
        start_subagent(&mut app, "agent-1", "tests");
        start_subagent(&mut app, "agent-2", "docs");
        app.handle_agent_event(sub("agent-1", AgentEvent::Content("tests report".into())));
        app.handle_agent_event(sub(
            "agent-2",
            AgentEvent::ToolStart {
                name: "read_file".into(),
                id: "t1".into(),
            },
        ));
        app.handle_agent_event(sub(
            "agent-2",
            AgentEvent::ToolArgsDelta {
                id: "t1".into(),
                delta: r#"{"path":"README.md"}"#.into(),
            },
        ));

        assert!(app.main.lines.iter().all(|l| l.content != "tests report"));
        assert!(app.main.tool_log.is_empty());
        let tests = &app.subagents[0].transcript;
        assert_eq!(tests.lines[0].style, LineStyle::Task);
        assert_eq!(tests.last_reply(), Some("tests report"));
        assert_eq!(app.subagents[1].transcript.lines.len(), 1);
        assert_eq!(
            app.subagents[1].current_activity(Path::new("")),
            "read_file README.md"
        );

        // An event for an unknown id still gets a transcript.
        app.handle_agent_event(sub("agent-9", AgentEvent::Content("hi".into())));
        assert_eq!(app.subagents.len(), 3);

        app.handle_agent_event(sub(
            "agent-1",
            AgentEvent::Done {
                usage: Some(crate::ai::types::UsageStats {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                    reasoning_tokens: None,
                }),
            },
        ));
        assert_eq!(app.subagents[0].tokens, 15);
        assert_eq!(app.total_tokens_used, 15);
        // A sub-agent's Done doesn't end the main run.
        assert!(app.is_processing);

        app.handle_agent_event(AgentEvent::SubAgentFinished {
            id: "agent-1".into(),
            success: true,
            summary: "tests report".into(),
        });
        app.handle_agent_event(AgentEvent::SubAgentFinished {
            id: "agent-2".into(),
            success: false,
            summary: "Cancelled".into(),
        });
        assert_eq!(app.subagents[0].state, AgentState::Done);
        assert_eq!(app.subagents[1].state, AgentState::Failed);
        // The cut-off tool no longer spins.
        assert_eq!(
            app.subagents[1].transcript.tool_log[0].status,
            ToolStatus::Failed
        );
    }

    #[test]
    fn switching_views_shows_the_sub_agent_transcript() {
        let mut app = App::new("m", std::path::PathBuf::new());
        app.add_user_message("go".into());
        app.handle_agent_event(AgentEvent::Content("main says hi".into()));
        start_subagent(&mut app, "agent-1", "tests");
        app.handle_agent_event(sub("agent-1", AgentEvent::Content("sub says hi".into())));

        let screen = render(&app, 120, 30);
        assert!(screen.contains("main says hi"));
        assert!(!screen.contains("sub says hi"));
        assert!(screen.contains("tests"), "agents strip missing");

        app.handle_key(KeyCode::Right, KeyModifiers::ALT);
        assert_eq!(app.focus, Some(0));
        let screen = render(&app, 120, 30);
        assert!(screen.contains("sub says hi"));
        assert!(!screen.contains("main says hi"));
        assert!(screen.contains("SUB-AGENT 1"));

        // Wraps around: sub-agent 1 → main.
        app.handle_key(KeyCode::Right, KeyModifiers::ALT);
        assert_eq!(app.focus, None);
        app.handle_key(KeyCode::Char('1'), KeyModifiers::ALT);
        assert_eq!(app.focus, Some(0));
        app.handle_key(KeyCode::Char('0'), KeyModifiers::ALT);
        assert_eq!(app.focus, None);
        app.handle_key(KeyCode::Char('7'), KeyModifiers::ALT);
        assert_eq!(app.focus, None);

        // Split view: both transcripts at once.
        app.handle_key(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(app.split);
        let screen = render(&app, 120, 30);
        assert!(screen.contains("main says hi"));
        assert!(screen.contains("sub says hi"));
        assert!(screen.contains("MAIN"));
        app.handle_key(KeyCode::F(2), KeyModifiers::NONE);
        assert!(!app.split);

        // `/agents N` from the input box.
        type_text(&mut app, "/agents 1");
        assert_eq!(app.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);
        assert_eq!(app.focus, Some(0));
        type_text(&mut app, "/agents main");
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.focus, None);
    }

    #[test]
    fn sending_a_message_returns_to_the_main_view() {
        let mut app = App::new("m", std::path::PathBuf::new());
        start_subagent(&mut app, "agent-1", "tests");
        app.set_focus(Some(0));
        type_text(&mut app, "next");
        assert_eq!(
            app.handle_key(KeyCode::Enter, KeyModifiers::NONE)
                .as_deref(),
            Some("next")
        );
        assert_eq!(app.focus, None);
    }

    #[test]
    fn sub_agent_approvals_are_routed_to_the_sub_agent() {
        let mut app = App::new("m", std::path::PathBuf::new());
        app.add_user_message("go".into());
        start_subagent(&mut app, "agent-1", "tests");
        app.handle_agent_event(sub(
            "agent-1",
            AgentEvent::ApprovalRequest {
                id: "c1".into(),
                tool_name: "run_command".into(),
                args_preview: r#"{"command":"cargo test"}"#.into(),
            },
        ));
        app.handle_agent_event(AgentEvent::ApprovalRequest {
            id: "c2".into(),
            tool_name: "write_file".into(),
            args_preview: "{}".into(),
        });
        assert_eq!(app.approvals.len(), 2);
        let screen = render(&app, 120, 30);
        assert!(screen.contains("by tests"), "asking agent not shown");
        assert!(screen.contains("1 more waiting"));

        assert_eq!(
            app.answer_approval(true),
            Some(ApprovalAnswer::SubAgent {
                id: "agent-1".into(),
                approved: true
            })
        );
        assert_eq!(
            app.answer_approval(false),
            Some(ApprovalAnswer::Main(false))
        );
        assert_eq!(app.answer_approval(true), None);

        // A sub-agent that ends drops its unanswered requests.
        app.handle_agent_event(sub(
            "agent-1",
            AgentEvent::ApprovalRequest {
                id: "c3".into(),
                tool_name: "run_command".into(),
                args_preview: "{}".into(),
            },
        ));
        app.handle_agent_event(AgentEvent::SubAgentFinished {
            id: "agent-1".into(),
            success: false,
            summary: "Cancelled".into(),
        });
        assert!(app.approvals.is_empty());
    }

    #[test]
    fn ctrl_x_stops_only_the_focused_sub_agent() {
        let mut app = App::new("m", std::path::PathBuf::new());
        app.add_user_message("go".into());
        start_subagent(&mut app, "agent-1", "tests");
        start_subagent(&mut app, "agent-2", "docs");
        app.handle_key(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert!(
            app.subagent_cancels.is_empty(),
            "main view has no sub-agent to stop"
        );
        app.handle_key(KeyCode::Char('2'), KeyModifiers::ALT);
        app.handle_key(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(app.subagent_cancels, vec!["agent-2".to_string()]);
        assert!(!app.cancel_requested);
        // Esc still stops the whole run.
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.cancel_requested);
    }

    #[test]
    fn clear_chat_keeps_only_running_sub_agents() {
        let mut app = App::new("m", std::path::PathBuf::new());
        start_subagent(&mut app, "agent-1", "tests");
        start_subagent(&mut app, "agent-2", "docs");
        app.handle_agent_event(AgentEvent::SubAgentFinished {
            id: "agent-1".into(),
            success: true,
            summary: "ok".into(),
        });
        app.set_focus(Some(1));
        app.handle_agent_event(AgentEvent::ClearChat);
        assert_eq!(app.subagents.len(), 1);
        assert_eq!(app.subagents[0].id, "agent-2");
        assert_eq!(app.focus, None);
    }

    #[test]
    fn ctrl_r_searches_prompt_history() {
        let mut app = App::new("m", std::path::PathBuf::new());
        app.input_history = vec![
            "fix the parser".into(),
            "write docs".into(),
            "fix the lexer".into(),
        ];
        type_text(&mut app, "draft");
        app.handle_key(KeyCode::Char('r'), KeyModifiers::CONTROL);
        type_text(&mut app, "fix");
        assert_eq!(app.input, "fix the lexer");
        app.handle_key(KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert_eq!(app.input, "fix the parser");
        // No older match: stays on the oldest one.
        app.handle_key(KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert_eq!(app.input, "fix the parser");
        assert!(render(&app, 120, 30).contains("History search: fix"));
        // Esc restores what was typed before.
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.input, "draft");
        assert!(app.history_search.is_none());

        app.set_input(String::new());
        app.handle_key(KeyCode::Char('r'), KeyModifiers::CONTROL);
        type_text(&mut app, "docs");
        assert_eq!(
            app.handle_key(KeyCode::Enter, KeyModifiers::NONE)
                .as_deref(),
            Some("write docs")
        );
        // Re-sent prompts move to the end of the history instead of repeating.
        assert_eq!(
            app.input_history.last().map(String::as_str),
            Some("write docs")
        );
        assert_eq!(app.input_history.len(), 3);
    }

    #[test]
    fn prompt_history_persists_across_sessions() {
        let dir = std::env::temp_dir().join(format!("norvexum-history-{}", std::process::id()));
        let path = dir.join("prompts.jsonl");
        let _ = std::fs::remove_dir_all(&dir);

        let mut app = App::new("m", std::path::PathBuf::new()).with_history_file(path.clone());
        type_text(&mut app, "first");
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        app.is_processing = false;
        type_text(&mut app, "two");
        app.handle_key(KeyCode::Enter, KeyModifiers::ALT);
        type_text(&mut app, "lines");
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        app.is_processing = false;
        type_text(&mut app, "first");
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        let reloaded = App::new("m", std::path::PathBuf::new()).with_history_file(path);
        assert_eq!(reloaded.input_history, vec!["two\nlines", "first"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slash_menu_is_fuzzy_and_navigable() {
        let mut app = App::new("m", std::path::PathBuf::new());
        type_text(&mut app, "/cmp");
        let names: Vec<&str> = app.command_suggestions().iter().map(|s| s.0).collect();
        assert_eq!(names.first(), Some(&"/compact"));

        // Prefix matches rank before fuzzy ones.
        app.set_input(String::new());
        type_text(&mut app, "/co");
        let names: Vec<&str> = app.command_suggestions().iter().map(|s| s.0).collect();
        assert_eq!(
            &names[..4],
            &["/compact", "/copy", "/copy chat", "/copy code"]
        );

        // Arguments never fuzzy-match.
        app.set_input(String::new());
        type_text(&mut app, "/model gpt");
        assert!(app.command_suggestions().is_empty());

        // Down picks the next entry, Tab completes it.
        app.set_input(String::new());
        type_text(&mut app, "/co");
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        app.handle_key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.input, "/copy ");
        assert!(app.input_history.is_empty());
    }

    #[test]
    fn enter_runs_the_picked_or_only_command() {
        let mut app = App::new("m", std::path::PathBuf::new());
        type_text(&mut app, "/comp");
        assert_eq!(
            app.handle_key(KeyCode::Enter, KeyModifiers::NONE)
                .as_deref(),
            Some("/compact")
        );
        app.is_processing = false;
        // Ambiguous and not picked: sent as typed.
        type_text(&mut app, "/c");
        assert_eq!(
            app.handle_key(KeyCode::Enter, KeyModifiers::NONE)
                .as_deref(),
            Some("/c")
        );
        app.is_processing = false;
        // `/keys` is handled by the TUI itself.
        type_text(&mut app, "/keys");
        assert_eq!(app.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);
        assert!(app.show_help);
    }

    #[test]
    fn last_reply_and_code_block_are_found() {
        let mut app = App::new("m", std::path::PathBuf::new());
        app.add_user_message("q".into());
        app.handle_agent_event(AgentEvent::Content(
            "Try:\n```rust\nfn a() {}\n```\nor\n```sh\ncargo run\n```\n".into(),
        ));
        app.handle_agent_event(AgentEvent::Done { usage: None });
        app.add_user_message("q2".into());
        app.handle_agent_event(AgentEvent::Content("No code here.".into()));
        app.handle_agent_event(AgentEvent::Done { usage: None });
        assert_eq!(app.focused().last_reply(), Some("No code here."));
        assert_eq!(
            app.focused().last_code_block().as_deref(),
            Some("cargo run")
        );
        assert_eq!(
            last_code_block("```\nunclosed\nblock").as_deref(),
            Some("unclosed\nblock")
        );
    }

    #[test]
    fn long_runs_ring_the_bell() {
        let mut app = App::new("m", std::path::PathBuf::new());
        app.add_user_message("quick".into());
        app.handle_agent_event(AgentEvent::Done { usage: None });
        assert!(!app.bell_pending);
        app.add_user_message("slow".into());
        app.run_started = Some(Instant::now() - LONG_RUN);
        app.handle_agent_event(AgentEvent::Done { usage: None });
        assert!(app.bell_pending);
    }

    #[test]
    fn home_and_end_scroll_when_the_input_is_empty() {
        let mut app = App::new("m", std::path::PathBuf::new());
        for i in 0..10 {
            app.add_user_message(format!("question {i}"));
            app.handle_agent_event(AgentEvent::Content(format!("answer {i}\n").repeat(4)));
            app.handle_agent_event(AgentEvent::Done { usage: None });
        }
        render(&app, 70, 20);
        app.handle_key(KeyCode::Home, KeyModifiers::NONE);
        assert!(app.main.scroll > 0);
        app.handle_key(KeyCode::End, KeyModifiers::NONE);
        assert_eq!(app.main.scroll, 0);
        // With text in the box they move the cursor instead.
        type_text(&mut app, "abc");
        app.handle_key(KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(app.cursor_pos, 0);
        assert_eq!(app.main.scroll, 0);
    }

    #[test]
    fn clicking_the_agents_strip_focuses_an_agent() {
        let mut app = App::new("m", std::path::PathBuf::new());
        start_subagent(&mut app, "agent-1", "tests");
        start_subagent(&mut app, "agent-2", "docs");
        render(&app, 120, 30);
        let strip = app.strip_area.get();
        assert!(strip.height >= 3);
        app.handle_mouse(
            MouseEventKind::Down(MouseButton::Left),
            strip.x + 3,
            strip.y + 2,
        );
        assert_eq!(app.focus, Some(1));
        app.handle_mouse(
            MouseEventKind::Down(MouseButton::Left),
            strip.x + 3,
            strip.y,
        );
        assert_eq!(app.focus, None);
    }

    #[test]
    fn agent_messages_show_for_sender_and_recipient() {
        let mut app = App::new("m", std::path::PathBuf::new());
        start_subagent(&mut app, "agent-1", "tests");
        start_subagent(&mut app, "agent-2", "docs");
        let has = |t: &Transcript, text: &str| {
            t.lines
                .iter()
                .any(|l| l.style == LineStyle::Message && l.content.contains(text))
        };
        // Sub-agent to main.
        app.handle_agent_event(sub(
            "agent-1",
            AgentEvent::AgentMessage {
                from: "tests".into(),
                to: "main".into(),
                text: "all green".into(),
            },
        ));
        assert!(has(&app.subagents[0].transcript, "tests → main:"));
        assert!(has(&app.main, "all green"));
        assert!(!has(&app.subagents[1].transcript, "all green"));
        // Main to a sub-agent, by name.
        app.handle_agent_event(AgentEvent::AgentMessage {
            from: "main".into(),
            to: "docs".into(),
            text: "mention the flag".into(),
        });
        assert!(has(&app.main, "main → docs:"));
        assert!(has(&app.subagents[1].transcript, "mention the flag"));

        // `/msg` needs a recipient and text; unknown agents are reported, not sent.
        type_text(&mut app, "/msg tests");
        assert_eq!(app.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);
        type_text(&mut app, "/msg nobody hello");
        assert_eq!(app.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);
        assert!(!app.is_processing);
    }
}
