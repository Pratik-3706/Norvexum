pub mod theme;

use std::io;

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseButton,
        MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Padding, Paragraph, Wrap},
};
use tokio::sync::mpsc;

use crate::agent::AgentEvent;
use theme::Theme;

pub struct App {
    pub chat_lines: Vec<ChatLine>,
    pub thinking_text: String,
    pub is_thinking: bool,
    pub tool_log: Vec<ToolLogEntry>,
    pub input: String,
    /// Cursor position in Unicode scalar values, never raw bytes.
    pub cursor_pos: usize,
    pub chat_scroll: u16,
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
    // ── Text selection state ─────────────────────────────────────────────
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
    pub user_backlog: Vec<String>,
    pub terminal_width: u16,
    pub terminal_height: u16,
    pub rendered_chat_lines: std::cell::RefCell<Vec<String>>,
    pub chat_visible_y: std::cell::Cell<u16>,
    pub chat_visible_height: std::cell::Cell<u16>,
    pub chat_scroll_offset: std::cell::Cell<u16>,
    pub chat_visible_x: std::cell::Cell<u16>,
    pub chat_visible_width: std::cell::Cell<u16>,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineStyle {
    User,
    Assistant,
    Thinking,
    Error,
    FileWrite,
    Status,
}

#[derive(Debug, Clone)]
pub struct ToolLogEntry {
    pub id: String,
    pub name: String,
    pub status: ToolStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Copy)]
pub enum ToolStatus {
    Running,
    Success,
    Failed,
}

impl App {
    pub fn new(model_info: &str, project_root: std::path::PathBuf) -> Self {
        Self {
            chat_lines: vec![ChatLine {
                content: "Ask a question, request a change, or type /help.".into(),
                style: LineStyle::Status,
                tool_call_id: None,
            }],
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
            user_backlog: Vec::new(),
            terminal_width: 0,
            terminal_height: 0,
            rendered_chat_lines: std::cell::RefCell::new(Vec::new()),
            chat_visible_y: std::cell::Cell::new(0),
            chat_visible_height: std::cell::Cell::new(0),
            chat_scroll_offset: std::cell::Cell::new(0),
            chat_visible_x: std::cell::Cell::new(0),
            chat_visible_width: std::cell::Cell::new(0),
        }
    }

    pub fn add_user_message(&mut self, message: String) {
        self.chat_lines.push(ChatLine {
            content: message,
            style: LineStyle::User,
            tool_call_id: None,
        });
        self.is_processing = true;
        self.status = "Starting".into();
        self.auto_scroll();
    }

    pub fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Thinking(text) => {
                self.is_thinking = true;
                self.thinking_text.push_str(&text);
                self.status = "Thinking".into();
                self.auto_scroll();
            }
            AgentEvent::ThinkingDone => {
                if !self.thinking_text.is_empty() {
                    self.chat_lines.push(ChatLine {
                        content: std::mem::take(&mut self.thinking_text),
                        style: LineStyle::Thinking,
                        tool_call_id: None,
                    });
                }
                self.is_thinking = false;
                self.auto_scroll();
            }
            AgentEvent::Content(text) => {
                if let Some(last) = self.chat_lines.last_mut() {
                    if last.style == LineStyle::Assistant {
                        last.content.push_str(&text);
                        self.auto_scroll();
                        return;
                    }
                }
                self.chat_lines.push(ChatLine {
                    content: text,
                    style: LineStyle::Assistant,
                    tool_call_id: None,
                });
                self.auto_scroll();
            }
            AgentEvent::ToolStart { name, id } => {
                self.status = format!("Running {name}");
                self.tool_log.push(ToolLogEntry {
                    id: id.clone(),
                    name: name.clone(),
                    status: ToolStatus::Running,
                    detail: String::new(),
                });
                if name == "write_file" || name == "edit_file" {
                    self.chat_lines.push(ChatLine {
                        content: format!("📝 Preparing to update file..."),
                        style: LineStyle::FileWrite,
                        tool_call_id: Some(id),
                    });
                }
                self.auto_scroll();
            }
            AgentEvent::ToolArgsDelta { id, delta } => {
                if let Some(entry) = self.tool_log.iter_mut().rev().find(|entry| entry.id == id) {
                    entry.detail.push_str(&delta);

                    let is_write = entry.name == "write_file";
                    let is_edit = entry.name == "edit_file";
                    if is_write || is_edit {
                        let path = extract_streaming_content(&entry.detail, "path");
                        let path_display = if path.is_empty() { "..." } else { &path };
                        let key = if is_write { "content" } else { "replacement" };
                        let content = extract_streaming_content(&entry.detail, key);

                        if let Some(line) = self
                            .chat_lines
                            .iter_mut()
                            .rev()
                            .find(|l| l.tool_call_id == Some(id.clone()))
                        {
                            line.content = format!(
                                "📝 Writing file `{}`:\n```\n{}\n```",
                                path_display, content
                            );
                        }
                    }
                }
            }
            AgentEvent::ToolResult {
                id,
                name,
                result,
                success,
            } => {
                if let Some(entry) = self.tool_log.iter_mut().rev().find(|entry| entry.id == id) {
                    entry.status = if success {
                        ToolStatus::Success
                    } else {
                        ToolStatus::Failed
                    };
                    entry.detail = compact_detail(&result, 180);
                } else {
                    self.tool_log.push(ToolLogEntry {
                        id: id.clone(),
                        name: name.clone(),
                        status: if success {
                            ToolStatus::Success
                        } else {
                            ToolStatus::Failed
                        },
                        detail: compact_detail(&result, 180),
                    });
                }
                // Update final code block layout if write/edit tool completed
                if name == "write_file" || name == "edit_file" {
                    if let Some(line) = self
                        .chat_lines
                        .iter_mut()
                        .rev()
                        .find(|l| l.tool_call_id == Some(id.clone()))
                    {
                        let prefix = if success {
                            "✅ Successfully wrote"
                        } else {
                            "❌ Failed to write"
                        };
                        let detail = compact_detail(&result, 120);
                        line.content = format!("{} to file:\n{}", prefix, detail);
                    }
                }
                self.status = if success {
                    "Tool completed".into()
                } else {
                    "Tool failed".into()
                };
                self.auto_scroll();
            }
            AgentEvent::FileWrite {
                path,
                content_preview,
            } => {
                self.chat_lines.push(ChatLine {
                    content: format!(
                        "📝 Updated {path}\n{}",
                        compact_detail(&content_preview, 160)
                    ),
                    style: LineStyle::FileWrite,
                    tool_call_id: None,
                });
                self.auto_scroll();
            }
            AgentEvent::Done { usage } => {
                self.is_processing = false;
                self.is_thinking = false;
                self.active_skill = None;
                if let Some(ref u) = usage {
                    self.total_tokens_used += u.total_tokens as u64;
                    self.prompt_tokens_used += u.prompt_tokens as u64;
                    self.completion_tokens_used += u.completion_tokens as u64;
                }
                self.status = usage.map_or_else(
                    || format!("Ready | {} tokens total", self.total_tokens_used),
                    |u| {
                        format!(
                            "Done — {} tokens (total: {})",
                            u.total_tokens, self.total_tokens_used
                        )
                    },
                );
                self.auto_scroll();
            }
            AgentEvent::Error(error) => {
                self.chat_lines.push(ChatLine {
                    content: error,
                    style: LineStyle::Error,
                    tool_call_id: None,
                });
                self.is_processing = false;
                self.is_thinking = false;
                self.active_skill = None;
                self.status = "Error".into();
                self.auto_scroll();
            }
            AgentEvent::Status(status) => self.status = clean_status(&status),
            AgentEvent::ModelSwitched { model, provider } => {
                self.model_info = format!("{model} / {provider}");
            }
            AgentEvent::ClearChat => {
                self.chat_lines.clear();
                self.chat_scroll = 0;
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
            AgentEvent::TriggeredSkill(name) => {
                self.active_skill = Some(name);
            }
        }
    }

    fn auto_scroll(&mut self) {
        self.chat_scroll = 0;
    }

    /// Copy selected text to clipboard. Returns true if something was copied.
    pub fn copy_selection(&mut self, _buffer: Option<&ratatui::buffer::Buffer>) -> bool {
        if self.active_panel != ActivePanel::Chat {
            self.selection_start = None;
            self.selection_end = None;
            self.is_selecting = false;
            return false;
        }

        if let (Some(start), Some(end)) = (self.selection_start, self.selection_end) {
            let rendered = self.rendered_chat_lines.borrow();
            if rendered.is_empty() {
                self.selection_start = None;
                self.selection_end = None;
                self.is_selecting = false;
                return false;
            }

            let chat_y = self.chat_visible_y.get();
            let chat_h = self.chat_visible_height.get();
            let chat_scroll = self.chat_scroll_offset.get();
            let chat_x = self.chat_visible_x.get();
            let chat_w = self.chat_visible_width.get();

            let (p1, p2) = if start.1 < end.1 || (start.1 == end.1 && start.0 <= end.0) {
                (start, end)
            } else {
                (end, start)
            };

            let r1 = p1.1.clamp(chat_y, chat_y + chat_h.saturating_sub(1));
            let r2 = p2.1.clamp(chat_y, chat_y + chat_h.saturating_sub(1));

            let s = (r1 as usize)
                .saturating_sub(chat_y as usize)
                .saturating_add(chat_scroll as usize)
                .min(rendered.len().saturating_sub(1));
            let e = (r2 as usize)
                .saturating_sub(chat_y as usize)
                .saturating_add(chat_scroll as usize)
                .min(rendered.len().saturating_sub(1));

            let slice_chars = |st: &str, start_idx: usize, end_idx: usize| -> String {
                st.chars().skip(start_idx).take(end_idx.saturating_sub(start_idx) + 1).collect()
            };
            let slice_chars_from = |st: &str, start_idx: usize| -> String {
                st.chars().skip(start_idx).collect()
            };

            let mut selected_lines = Vec::new();
            for idx in s..=e {
                let line = &rendered[idx];

                let sliced = if s == e {
                    let col_start = (p1.0 as usize).saturating_sub(chat_x as usize);
                    let col_end = (p2.0 as usize).saturating_sub(chat_x as usize);
                    slice_chars(line, col_start, col_end)
                } else if idx == s {
                    let col_start = (p1.0 as usize).saturating_sub(chat_x as usize);
                    slice_chars_from(line, col_start)
                } else if idx == e {
                    let col_end = (p2.0 as usize).saturating_sub(chat_x as usize);
                    slice_chars(line, 0, col_end)
                } else {
                    line.to_string()
                };

                selected_lines.push(sliced);
            }

            let selected = selected_lines.join("\n");
            let selected_trimmed = selected.trim().to_string();

            if !selected_trimmed.is_empty() {
                if let Ok(mut clipboard) = arboard::Clipboard::new() {
                    let _ = clipboard.set_text(&selected_trimmed);
                    self.selection_start = None;
                    self.selection_end = None;
                    self.is_selecting = false;
                    return true;
                }
            }
        }
        false
    }

    pub fn handle_key(&mut self, key: KeyCode, modifiers: KeyModifiers) -> Option<String> {
        // Tab key toggles focus between Chat input and Activity panel
        if key == KeyCode::Tab {
            self.active_panel = match self.active_panel {
                ActivePanel::Chat => ActivePanel::Activity,
                ActivePanel::Activity => ActivePanel::Chat,
            };
            return None;
        }

        // Handle tool details overlay modal keys
        if self.show_tool_details {
            match key {
                KeyCode::Esc | KeyCode::Enter => {
                    self.show_tool_details = false;
                }
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

        // Intercept navigation keys when Activity panel is focused
        if self.active_panel == ActivePanel::Activity {
            match key {
                KeyCode::Esc => {
                    self.active_panel = ActivePanel::Chat;
                }
                KeyCode::Up => {
                    self.selected_tool_index = self.selected_tool_index.saturating_sub(1);
                }
                KeyCode::Down => {
                    if !self.tool_log.is_empty() {
                        self.selected_tool_index = (self.selected_tool_index + 1)
                            .min(self.tool_log.len().saturating_sub(1));
                    }
                }
                KeyCode::PageUp => {
                    self.selected_tool_index = self.selected_tool_index.saturating_sub(5);
                }
                KeyCode::PageDown => {
                    if !self.tool_log.is_empty() {
                        self.selected_tool_index = (self.selected_tool_index + 5)
                            .min(self.tool_log.len().saturating_sub(1));
                    }
                }
                KeyCode::Enter => {
                    if !self.tool_log.is_empty() {
                        self.show_tool_details = true;
                        self.tool_details_scroll = 0;
                    }
                }
                _ => {}
            }
            return None;
        }

        match key {
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                // If there's a selection, copy it instead of quitting
                if self.selection_start.is_some() && self.selection_end.is_some() {
                    self.copy_selection(None);
                    return None;
                }
                self.should_quit = true;
                None
            }
            KeyCode::Char('v') if modifiers.contains(KeyModifiers::CONTROL) => {
                if let Ok(mut clipboard) = arboard::Clipboard::new() {
                    if let Ok(text) = clipboard.get_text() {
                        let text = text.replace(['\r', '\n'], " ");
                        self.insert_text(&text);
                    }
                }
                None
            }
            KeyCode::Char('a') if modifiers.contains(KeyModifiers::CONTROL) => {
                // Select all — set selection to cover entire chat
                self.selection_start = Some((0, 0));
                let total_lines = self.chat_lines.len() as u16;
                self.selection_end = Some((0, total_lines));
                self.is_selecting = false;
                None
            }
            KeyCode::Char('l') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.clear();
                self.cursor_pos = 0;
                None
            }
            KeyCode::Enter => {
                let message = self.input.trim().to_string();
                if message.is_empty() {
                    return None;
                }
                self.input_history.push(message.clone());
                self.history_pos = None;
                self.input.clear();
                self.cursor_pos = 0;

                if self.is_processing {
                    // Queue the message to backlog
                    self.chat_lines.push(ChatLine {
                        content: message.clone(),
                        style: LineStyle::User,
                        tool_call_id: None,
                    });
                    self.user_backlog.push(message);
                    self.auto_scroll();
                    None
                } else {
                    self.add_user_message(message.clone());
                    Some(message)
                }
            }
            KeyCode::Char(c) => {
                self.insert_text(&c.to_string());
                None
            }
            KeyCode::Backspace if self.cursor_pos > 0 => {
                let start = byte_index(&self.input, self.cursor_pos - 1);
                let end = byte_index(&self.input, self.cursor_pos);
                self.input.replace_range(start..end, "");
                self.cursor_pos -= 1;
                None
            }
            KeyCode::Delete if self.cursor_pos < char_count(&self.input) => {
                let start = byte_index(&self.input, self.cursor_pos);
                let end = byte_index(&self.input, self.cursor_pos + 1);
                self.input.replace_range(start..end, "");
                None
            }
            KeyCode::Left => {
                self.cursor_pos = self.cursor_pos.saturating_sub(1);
                None
            }
            KeyCode::Right => {
                self.cursor_pos = (self.cursor_pos + 1).min(char_count(&self.input));
                None
            }
            KeyCode::Home => {
                self.cursor_pos = 0;
                None
            }
            KeyCode::End => {
                self.cursor_pos = char_count(&self.input);
                None
            }
            KeyCode::Up if modifiers.contains(KeyModifiers::CONTROL) => {
                self.older_history();
                None
            }
            KeyCode::Down if modifiers.contains(KeyModifiers::CONTROL) => {
                self.newer_history();
                None
            }
            KeyCode::Up => {
                self.chat_scroll = self.chat_scroll.saturating_add(2);
                None
            }
            KeyCode::Down => {
                self.chat_scroll = self.chat_scroll.saturating_sub(2);
                None
            }
            KeyCode::PageUp => {
                self.chat_scroll = self.chat_scroll.saturating_add(8);
                None
            }
            KeyCode::PageDown => {
                self.chat_scroll = self.chat_scroll.saturating_sub(8);
                None
            }
            _ => None,
        }
    }

    fn insert_text(&mut self, text: &str) {
        let index = byte_index(&self.input, self.cursor_pos);
        self.input.insert_str(index, text);
        self.cursor_pos += char_count(text);
    }

    fn older_history(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        let pos = self
            .history_pos
            .map_or(self.input_history.len() - 1, |pos| pos.saturating_sub(1));
        self.history_pos = Some(pos);
        self.input = self.input_history[pos].clone();
        self.cursor_pos = char_count(&self.input);
    }

    fn newer_history(&mut self) {
        let Some(pos) = self.history_pos else {
            return;
        };
        if pos + 1 < self.input_history.len() {
            self.history_pos = Some(pos + 1);
            self.input = self.input_history[pos + 1].clone();
            self.cursor_pos = char_count(&self.input);
        } else {
            self.history_pos = None;
            self.input.clear();
            self.cursor_pos = 0;
        }
    }
}

fn highlight_selection(frame: &mut Frame, start: (u16, u16), end: (u16, u16), _theme: &Theme) {
    let (s_col, s_row) = start;
    let (e_col, e_row) = end;

    // Normalize start/end so start is earlier in the document than end
    let (p1_col, p1_row, p2_col, p2_row) = if s_row < e_row || (s_row == e_row && s_col <= e_col) {
        (s_col, s_row, e_col, e_row)
    } else {
        (e_col, e_row, s_col, s_row)
    };

    let area = frame.area();
    let buffer = frame.buffer_mut();

    // Highlighting style: reversed or deep navy/blue block selection
    let selection_style = Style::default()
        .bg(ratatui::style::Color::Rgb(40, 60, 110))
        .fg(ratatui::style::Color::Rgb(240, 244, 255));

    for row in p1_row..=p2_row {
        if row >= area.height {
            continue;
        }

        let start_col = if row == p1_row { p1_col } else { 0 };
        let end_col = if row == p2_row { p2_col } else { area.width.saturating_sub(1) };

        for col in start_col..=end_col {
            if col >= area.width {
                continue;
            }
            let cell = &mut buffer[(col, row)];
            cell.set_style(selection_style);
        }
    }
}

pub fn draw(frame: &mut Frame, app: &App) {
    let theme = &app.theme;
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.bg).fg(theme.fg)),
        area,
    );

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(3),
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
        draw_chat(frame, rows[1], app);
    }

    draw_input(frame, rows[2], app);
    draw_footer(frame, rows[3], app);

    // Render approval modal if pending
    if let Some(info) = &app.approval_pending {
        let modal_area = centered_rect(60, 45, area);
        frame.render_widget(ratatui::widgets::Clear, modal_area);

        let border_color = app.theme.tool_color;
        let block = Block::default()
            .title(Span::styled(
                " ⚠️  TOOL RUN APPROVAL ",
                Style::default()
                    .fg(border_color)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .padding(Padding::new(1, 1, 1, 1));

        let prompt_text = format!(
            "The model wants to execute the following tool:\n\n\
             Tool: {}\n\n\
             Arguments:\n  {}\n\n\
             Allow execution? [y]es / [n]o",
            info.tool_name, info.args_preview
        );

        let paragraph = Paragraph::new(prompt_text)
            .block(block)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(app.theme.fg));

        frame.render_widget(paragraph, modal_area);
    }

    // Render tool details modal if show_tool_details is true
    if app.show_tool_details {
        if let Some(entry) = app.tool_log.iter().rev().nth(app.selected_tool_index) {
            let modal_area = centered_rect(75, 75, area);
            frame.render_widget(ratatui::widgets::Clear, modal_area);

            let border_color = match entry.status {
                ToolStatus::Running => app.theme.tool_color,
                ToolStatus::Success => app.theme.success_color,
                ToolStatus::Failed => app.theme.error_color,
            };

            let block = Block::default()
                .title(Span::styled(
                    format!(" 🔧  TOOL DETAILS: {} ", entry.name),
                    Style::default()
                        .fg(border_color)
                        .add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .padding(Padding::new(2, 2, 1, 1));

            // Format full details
            let status_str = match entry.status {
                ToolStatus::Running => "Running...",
                ToolStatus::Success => "Success",
                ToolStatus::Failed => "Failed",
            };

            let mut text = vec![
                Line::from(vec![
                    Span::styled("Tool Name: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(&entry.name),
                ]),
                Line::from(vec![
                    Span::styled("Status:    ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::styled(
                        status_str,
                        Style::default()
                            .fg(border_color)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::default(),
                Line::from(Span::styled(
                    "DETAILS / OUTPUT:",
                    Style::default()
                        .add_modifier(Modifier::BOLD)
                        .fg(app.theme.accent_color),
                )),
                Line::default(),
            ];

            for line in entry.detail.lines() {
                text.push(Line::from(line));
            }

            text.push(Line::default());
            text.push(Line::from(Span::styled(
                "─── Use Up/Down/PageUp/PageDown to scroll | Esc/Enter to close ───",
                Style::default().fg(app.theme.dim_color),
            )));

            let paragraph = Paragraph::new(text)
                .block(block)
                .scroll((app.tool_details_scroll, 0))
                .wrap(Wrap { trim: false });

            frame.render_widget(paragraph, modal_area);
        }
    }

    if let (Some(start), Some(end)) = (app.selection_start, app.selection_end) {
        highlight_selection(frame, start, end, &app.theme);
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
    let token_info = if app.total_tokens_used > 0 {
        format!("  │ Tokens: {}", format_tokens(app.total_tokens_used))
    } else {
        String::new()
    };

    let mut title_spans = vec![
        Span::styled(
            " NORVEXUM ",
            Style::default()
                .fg(app.theme.bg)
                .bg(app.theme.accent_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", app.model_info),
            Style::default()
                .fg(app.theme.fg)
                .add_modifier(Modifier::BOLD),
        ),
    ];

    if let Some(ref skill) = app.active_skill {
        title_spans.push(Span::styled(
            format!("  │ Skill: {}", skill),
            Style::default()
                .fg(app.theme.accent_color)
                .add_modifier(Modifier::BOLD),
        ));
    }

    title_spans.push(Span::styled(
        token_info,
        Style::default().fg(app.theme.dim_color),
    ));

    let title = Line::from(title_spans);
    let state_color = if app.status == "Error" {
        app.theme.error_color
    } else if app.is_processing {
        app.theme.tool_color
    } else {
        app.theme.success_color
    };
    let header = Paragraph::new(Text::from(vec![
        title,
        Line::from(vec![
            Span::styled(" STATUS  ", Style::default().fg(app.theme.dim_color)),
            Span::styled(&app.status, Style::default().fg(state_color)),
        ]),
    ]))
    .block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(app.theme.border_color)),
    );
    frame.render_widget(header, area);
}

fn draw_chat(frame: &mut Frame, area: Rect, app: &App) {
    let width = area.width.saturating_sub(6) as usize;
    let mut lines = Vec::new();

    for message in &app.chat_lines {
        let (label, color) = match message.style {
            LineStyle::User => ("YOU", app.theme.user_color),
            LineStyle::Assistant => ("NORVEXUM", app.theme.assistant_color),
            LineStyle::Thinking => ("REASONING", app.theme.thinking_color),
            LineStyle::Error => ("ERROR", app.theme.error_color),
            LineStyle::FileWrite => ("FILE", app.theme.file_color),
            LineStyle::Status => ("TIP", app.theme.dim_color),
        };
        lines.push(Line::from(Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
        lines.extend(render_message(
            &message.content,
            width,
            color,
            message.style,
        ));
        lines.push(Line::default());
    }

    if app.is_thinking && !app.thinking_text.is_empty() {
        lines.push(Line::from(Span::styled(
            "REASONING  ● live",
            Style::default()
                .fg(app.theme.thinking_color)
                .add_modifier(Modifier::BOLD),
        )));
        let recent = wrap_text(&app.thinking_text, width);
        for line in recent.iter().skip(recent.len().saturating_sub(4)) {
            lines.push(Line::from(Span::styled(
                format!("│ {line}"),
                Style::default()
                    .fg(app.theme.thinking_color)
                    .add_modifier(Modifier::DIM),
            )));
        }
    }

    let visible = area.height.saturating_sub(2) as usize;
    let scroll = lines
        .len()
        .saturating_sub(visible)
        .saturating_sub(app.chat_scroll as usize) as u16;

    // Store the rendered plain text lines for clean copy selection
    let plain_lines: Vec<String> = lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        })
        .collect();
    *app.rendered_chat_lines.borrow_mut() = plain_lines;
    app.chat_visible_y.set(area.y + 1); // 1 line for top padding
    app.chat_visible_height.set(area.height.saturating_sub(2));
    app.chat_scroll_offset.set(scroll);
    app.chat_visible_x.set(area.x + 2); // 2 columns of left padding
    app.chat_visible_width.set(width as u16);

    let chat = Paragraph::new(Text::from(lines))
        .block(
            Block::default()
                .borders(Borders::RIGHT)
                .border_style(Style::default().fg(app.theme.border_color))
                .padding(Padding::new(2, 1, 1, 0)),
        )
        .scroll((scroll, 0));

    frame.render_widget(chat, area);
}

fn clean_paths(text: &str, project_root: &std::path::Path) -> String {
    let mut cleaned = text.to_string();
    if let Some(parent) = project_root.parent() {
        let parent_str_forward = parent.to_string_lossy().replace('\\', "/");
        let parent_str_backward = parent.to_string_lossy().replace('/', "\\");

        // Ensure trailing slash is included to strip prefix cleanly
        let p_forward = if parent_str_forward.ends_with('/') {
            parent_str_forward
        } else {
            parent_str_forward + "/"
        };
        let p_backward = if parent_str_backward.ends_with('\\') {
            parent_str_backward
        } else {
            parent_str_backward + "\\"
        };

        cleaned = cleaned.replace(&p_forward, "");
        cleaned = cleaned.replace(&p_backward, "");
    } else {
        let root_str_forward = project_root.to_string_lossy().replace('\\', "/");
        let root_str_backward = project_root.to_string_lossy().replace('/', "\\");
        cleaned = cleaned.replace(&root_str_forward, ".");
        cleaned = cleaned.replace(&root_str_backward, ".");
    }
    cleaned
}

fn draw_tools(frame: &mut Frame, area: Rect, app: &App) {
    let title = if app.active_panel == ActivePanel::Activity {
        "ACTIVITY (Focused)"
    } else {
        "ACTIVITY [Tab]"
    };

    let mut lines = vec![Line::from(Span::styled(
        title,
        Style::default()
            .fg(if app.active_panel == ActivePanel::Activity {
                app.theme.accent_color
            } else {
                app.theme.dim_color
            })
            .add_modifier(Modifier::BOLD),
    ))];
    lines.push(Line::default());

    if app.tool_log.is_empty() {
        lines.push(Line::from(Span::styled(
            "Tool runs will appear here.",
            Style::default().fg(app.theme.dim_color),
        )));
    }

    // Render all tools in reverse order (newest first)
    for (i, entry) in app.tool_log.iter().rev().enumerate() {
        let is_selected = app.active_panel == ActivePanel::Activity && i == app.selected_tool_index;

        let (mark, color) = match entry.status {
            ToolStatus::Running => ("~", app.theme.tool_color),
            ToolStatus::Success => ("+", app.theme.success_color),
            ToolStatus::Failed => ("!", app.theme.error_color),
        };

        let cursor = if is_selected { "> " } else { "  " };
        let style = if is_selected {
            Style::default()
                .bg(app.theme.accent_color)
                .fg(app.theme.bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(app.theme.fg)
                .add_modifier(Modifier::BOLD)
        };

        lines.push(Line::from(vec![
            Span::styled(
                cursor,
                Style::default()
                    .fg(app.theme.accent_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{mark} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(&entry.name, style),
        ]));

        let cleaned_detail = clean_paths(&entry.detail, &app.project_root);
        for detail in wrap_text(&cleaned_detail, area.width.saturating_sub(6) as usize)
            .into_iter()
            .take(2)
        {
            let detail_style = if is_selected {
                Style::default().bg(app.theme.accent_color).fg(app.theme.bg)
            } else {
                Style::default().fg(app.theme.dim_color)
            };
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(detail, detail_style),
            ]));
        }
        lines.push(Line::default());
    }

    // Standard scroll calculation for tool log
    let visible = area.height.saturating_sub(3) as usize;
    let scroll_y = {
        let mut line_offset = 0;
        let mut selected_y = 0;
        for (i, entry) in app.tool_log.iter().rev().enumerate() {
            let cleaned_detail = clean_paths(&entry.detail, &app.project_root);
            let detail_lines = wrap_text(&cleaned_detail, area.width.saturating_sub(6) as usize)
                .into_iter()
                .take(2)
                .count();
            let item_height = 1 + detail_lines + 1;
            if i == app.selected_tool_index {
                selected_y = line_offset;
            }
            line_offset += item_height;
        }
        if selected_y >= visible {
            (selected_y - visible / 2) as u16
        } else {
            0
        }
    };

    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::default().padding(Padding::new(2, 1, 1, 0)))
            .scroll((scroll_y, 0)),
        area,
    );
}

fn draw_input(frame: &mut Frame, area: Rect, app: &App) {
    let border = if app.is_processing {
        app.theme.border_color
    } else {
        app.theme.accent_color
    };
    let available = area.width.saturating_sub(5) as usize;
    let cursor = app.cursor_pos;
    let offset = cursor.saturating_sub(available.saturating_sub(1));
    let visible: String = app.input.chars().skip(offset).take(available).collect();
    let display = if visible.is_empty() {
        if app.is_processing {
            "Working... (type message to queue)".to_string()
        } else {
            "What should we build?".to_string()
        }
    } else {
        visible
    };
    let color = if app.input.is_empty() {
        app.theme.dim_color
    } else {
        app.theme.input_color
    };
    frame.render_widget(
        Paragraph::new(display)
            .style(Style::default().fg(color))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border))
                    .title(Span::styled(
                        " COMMAND ",
                        Style::default().fg(border).add_modifier(Modifier::BOLD),
                    ))
                    .padding(Padding::horizontal(1)),
            ),
        area,
    );
    if !app.is_processing {
        frame.set_cursor_position((
            area.x + 2 + cursor.saturating_sub(offset) as u16,
            area.y + 1,
        ));
    }
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let help = if app.is_processing {
        " Esc stop  |  Ctrl+C quit "
    } else if app.selection_start.is_some() {
        " Ctrl+C copy selection  |  Esc clear selection  |  Ctrl+A select all "
    } else {
        " Enter send  |  Ctrl+Up history  |  PgUp scroll  |  Drag to select  |  Ctrl+C quit "
    };
    frame.render_widget(
        Paragraph::new(help).style(Style::default().fg(app.theme.dim_color)),
        area,
    );
}

pub async fn run_tui(
    mut app: App,
    mut agent_rx: mpsc::UnboundedReceiver<AgentEvent>,
    user_tx: mpsc::UnboundedSender<String>,
    cancel_tx: mpsc::UnboundedSender<()>,
    approval_tx: mpsc::UnboundedSender<bool>,
) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(event) = event::read() {
            if event_tx.send(event).is_err() {
                break;
            }
        }
    });

    // Tick interval for smooth streaming redraws (60fps)
    let mut tick_interval = tokio::time::interval(std::time::Duration::from_millis(16));

    let result = async {
        loop {
            terminal.draw(|frame| draw(frame, &app))?;
            tokio::select! {
                Some(event) = agent_rx.recv() => {
                    app.handle_agent_event(event);
                    if !app.is_processing && !app.user_backlog.is_empty() {
                        let next_msg = app.user_backlog.remove(0);
                        app.is_processing = true;
                        app.status = "Starting next task".into();
                        let _ = user_tx.send(next_msg);
                    }
                }
                Some(event) = event_rx.recv() => match event {
                    Event::Key(key) if key.kind == event::KeyEventKind::Press => {
                        if app.approval_pending.is_some() {
                            match key.code {
                                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                                    let _ = approval_tx.send(true);
                                    app.approval_pending = None;
                                }
                                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                    let _ = approval_tx.send(false);
                                    app.approval_pending = None;
                                    app.is_processing = false;
                                }
                                _ => {}
                            }
                            continue;
                        }

                        if key.code == KeyCode::Esc {
                            if app.is_selecting || app.selection_start.is_some() {
                                // Clear selection
                                app.selection_start = None;
                                app.selection_end = None;
                                app.is_selecting = false;
                            } else if app.is_processing {
                                app.user_backlog.clear();
                                let _ = cancel_tx.send(());
                            } else {
                                app.should_quit = true;
                            }
                        } else if let Some(message) = app.handle_key(key.code, key.modifiers) {
                            let _ = user_tx.send(message);
                        }
                    }
                    Event::Mouse(mouse) => match mouse.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            let size = terminal.size().unwrap_or_default();
                            let is_on_tools = size.width >= 100 && mouse.column >= (size.width * 72 / 100);
                            if is_on_tools {
                                app.active_panel = ActivePanel::Activity;
                            } else {
                                app.active_panel = ActivePanel::Chat;
                            }

                            app.selection_start = Some((mouse.column, mouse.row));
                            app.selection_end = Some((mouse.column, mouse.row));
                            app.is_selecting = true;
                        }
                        MouseEventKind::Down(MouseButton::Right) => {
                            if let Ok(mut clipboard) = arboard::Clipboard::new() {
                                if let Ok(text) = clipboard.get_text() {
                                    let text = text.replace(['\r', '\n'], " ");
                                    app.insert_text(&text);
                                }
                            }
                        }
                        MouseEventKind::Drag(_) => {
                            if app.is_selecting {
                                app.selection_end = Some((mouse.column, mouse.row));
                            }
                        }
                        MouseEventKind::Up(_) => {
                            if app.is_selecting {
                                app.selection_end = Some((mouse.column, mouse.row));
                                app.is_selecting = false;
                                if let (Some(start), Some(end)) = (app.selection_start, app.selection_end) {
                                    if start != end {
                                        app.copy_selection(None);
                                    } else {
                                        app.selection_start = None;
                                        app.selection_end = None;
                                    }
                                }
                            }
                        }
                        MouseEventKind::ScrollUp => {
                            let size = terminal.size().unwrap_or_default();
                            let is_on_tools = size.width >= 100 && mouse.column >= (size.width * 70 / 100);
                            if is_on_tools || app.active_panel == ActivePanel::Activity {
                                app.selected_tool_index = app.selected_tool_index.saturating_sub(1);
                            } else {
                                app.chat_scroll = app.chat_scroll.saturating_add(3);
                            }
                        }
                        MouseEventKind::ScrollDown => {
                            let size = terminal.size().unwrap_or_default();
                            let is_on_tools = size.width >= 100 && mouse.column >= (size.width * 70 / 100);
                            if is_on_tools || app.active_panel == ActivePanel::Activity {
                                if !app.tool_log.is_empty() {
                                    app.selected_tool_index = (app.selected_tool_index + 1)
                                        .min(app.tool_log.len().saturating_sub(1));
                                }
                            } else {
                                app.chat_scroll = app.chat_scroll.saturating_sub(3);
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                },
                _ = tick_interval.tick() => {
                    // Tick for smooth streaming — just triggers a redraw
                }
            }
            if app.should_quit {
                break;
            }
        }
        io::Result::Ok(())
    }
    .await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    result
}

// ── Markdown-aware message rendering ──────────────────────────────────────

fn render_message(
    text: &str,
    width: usize,
    color: ratatui::style::Color,
    kind: LineStyle,
) -> Vec<Line<'static>> {
    let mut result = Vec::new();
    let mut in_code = false;
    let code_bg = ratatui::style::Color::Rgb(25, 28, 40);

    for raw in text.lines() {
        if raw.trim_start().starts_with("```") {
            in_code = !in_code;
            if in_code {
                // Code block start — show language label if present
                let lang = raw.trim_start().strip_prefix("```").unwrap_or("");
                if !lang.is_empty() {
                    result.push(Line::from(Span::styled(
                        format!("  ┌─ {} ", lang),
                        Style::default()
                            .fg(ratatui::style::Color::Rgb(100, 110, 140))
                            .bg(code_bg),
                    )));
                } else {
                    result.push(Line::from(Span::styled(
                        "  ┌──────",
                        Style::default()
                            .fg(ratatui::style::Color::Rgb(60, 65, 80))
                            .bg(code_bg),
                    )));
                }
            } else {
                // Code block end
                result.push(Line::from(Span::styled(
                    "  └──────",
                    Style::default()
                        .fg(ratatui::style::Color::Rgb(60, 65, 80))
                        .bg(code_bg),
                )));
            }
            continue;
        }

        if in_code {
            // Inside code block — preserve whitespace, use monospace style
            let code_style = Style::default()
                .fg(ratatui::style::Color::Rgb(190, 200, 220))
                .bg(code_bg);
            let padded = format!("  │ {}", raw);
            result.push(Line::from(Span::styled(padded, code_style)));
            continue;
        }

        // Parse inline markdown
        let (prefix, content, base_style) = if let Some(heading) = raw.strip_prefix("### ") {
            (
                "  ",
                heading,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )
        } else if let Some(heading) = raw.strip_prefix("## ") {
            (
                " ",
                heading,
                Style::default()
                    .fg(color)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )
        } else if let Some(heading) = raw.strip_prefix("# ") {
            (
                "",
                heading,
                Style::default()
                    .fg(color)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )
        } else if raw.starts_with("> ") {
            // Blockquote
            (
                "  │ ",
                &raw[2..],
                Style::default()
                    .fg(ratatui::style::Color::Rgb(140, 150, 170))
                    .add_modifier(Modifier::ITALIC),
            )
        } else if raw.starts_with("- ") || raw.starts_with("* ") {
            ("  • ", &raw[2..], Style::default().fg(color))
        } else if raw.starts_with("---") || raw.starts_with("***") || raw.starts_with("___") {
            // Horizontal rule
            let hr = "─".repeat(width.min(60));
            result.push(Line::from(Span::styled(
                format!("  {}", hr),
                Style::default().fg(ratatui::style::Color::Rgb(60, 65, 80)),
            )));
            continue;
        } else {
            ("  ", raw, Style::default().fg(color))
        };

        // Handle numbered lists (e.g., "1. item")
        let (final_prefix, final_content) = if content.len() > 2
            && content
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
        {
            if let Some(rest) = content.strip_prefix(|c: char| c.is_ascii_digit()) {
                if let Some(rest) = rest.strip_prefix(". ") {
                    let num: String = content.chars().take_while(|c| c.is_ascii_digit()).collect();
                    let new_prefix = format!("{}{}. ", prefix, num);
                    (new_prefix, rest.to_string())
                } else {
                    (prefix.to_string(), content.to_string())
                }
            } else {
                (prefix.to_string(), content.to_string())
            }
        } else {
            (prefix.to_string(), content.to_string())
        };

        let wrap_width = width.saturating_sub(char_count(&final_prefix)).max(1);

        // Parse inline formatting and render
        let spans = parse_inline_markdown(&final_content, base_style, kind);
        if spans.is_empty() {
            result.push(Line::default());
        } else {
            // Wrap the combined text
            let plain_text: String = spans.iter().map(|s| s.content.to_string()).collect();
            let wrapped = wrap_text(&plain_text, wrap_width);

            if wrapped.is_empty() {
                result.push(Line::default());
            } else {
                for (index, line) in wrapped.into_iter().enumerate() {
                    let marker = if index == 0 { &final_prefix } else { "    " };
                    let line_style = if kind == LineStyle::Thinking {
                        base_style.add_modifier(Modifier::ITALIC)
                    } else {
                        base_style
                    };
                    result.push(Line::from(Span::styled(
                        format!("{marker}{line}"),
                        line_style,
                    )));
                }
            }
        }
    }
    result
}

/// Parse inline markdown: **bold**, *italic*, `code`, ~~strikethrough~~
fn parse_inline_markdown(text: &str, base_style: Style, _kind: LineStyle) -> Vec<Span<'static>> {
    // For performance, if there's no markdown formatting, return as-is
    if !text.contains('*') && !text.contains('`') && !text.contains('~') {
        return vec![Span::styled(text.to_string(), base_style)];
    }

    let mut spans = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Bold: **text**
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            if !current.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut current), base_style));
            }
            i += 2;
            let mut bold_text = String::new();
            while i + 1 < len && !(chars[i] == '*' && chars[i + 1] == '*') {
                bold_text.push(chars[i]);
                i += 1;
            }
            if i + 1 < len {
                i += 2; // Skip closing **
            }
            spans.push(Span::styled(
                bold_text,
                base_style.add_modifier(Modifier::BOLD),
            ));
        }
        // Inline code: `text`
        else if chars[i] == '`' {
            if !current.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut current), base_style));
            }
            i += 1;
            let mut code_text = String::new();
            while i < len && chars[i] != '`' {
                code_text.push(chars[i]);
                i += 1;
            }
            if i < len {
                i += 1; // Skip closing `
            }
            let code_bg = ratatui::style::Color::Rgb(35, 38, 55);
            spans.push(Span::styled(
                format!(" {} ", code_text),
                Style::default()
                    .fg(ratatui::style::Color::Rgb(230, 180, 80))
                    .bg(code_bg),
            ));
        }
        // Italic: *text*
        else if chars[i] == '*' {
            if !current.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut current), base_style));
            }
            i += 1;
            let mut italic_text = String::new();
            while i < len && chars[i] != '*' {
                italic_text.push(chars[i]);
                i += 1;
            }
            if i < len {
                i += 1;
            }
            spans.push(Span::styled(
                italic_text,
                base_style.add_modifier(Modifier::ITALIC),
            ));
        }
        // Strikethrough: ~~text~~
        else if i + 1 < len && chars[i] == '~' && chars[i + 1] == '~' {
            if !current.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut current), base_style));
            }
            i += 2;
            let mut strike_text = String::new();
            while i + 1 < len && !(chars[i] == '~' && chars[i + 1] == '~') {
                strike_text.push(chars[i]);
                i += 1;
            }
            if i + 1 < len {
                i += 2;
            }
            spans.push(Span::styled(
                strike_text,
                base_style.add_modifier(Modifier::DIM),
            ));
        } else {
            current.push(chars[i]);
            i += 1;
        }
    }

    if !current.is_empty() {
        spans.push(Span::styled(current, base_style));
    }

    spans
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        if paragraph.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let word_len = char_count(word);
            if word_len > width {
                if !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                }
                let chars: Vec<char> = word.chars().collect();
                for chunk in chars.chunks(width) {
                    lines.push(chunk.iter().collect());
                }
            } else if line.is_empty() {
                line.push_str(word);
            } else if char_count(&line) + 1 + word_len <= width {
                line.push(' ');
                line.push_str(word);
            } else {
                lines.push(std::mem::replace(&mut line, word.to_string()));
            }
        }
        if !line.is_empty() {
            lines.push(line);
        }
    }
    lines
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
    let mut compact: String = one_line.chars().take(max).collect();
    if one_line.chars().count() > max {
        compact.push_str("...");
    }
    compact
}

fn clean_status(status: &str) -> String {
    status
        .replace("💭 ", "")
        .replace("🔧 ", "")
        .replace("✅ ", "")
        .replace("⏹️ ", "")
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

fn extract_streaming_content(json_accumulated: &str, key: &str) -> String {
    let pattern = format!("\"{}\":", key);
    if let Some(pos) = json_accumulated.find(&pattern) {
        let rest = &json_accumulated[pos + pattern.len()..];
        if let Some(quote_start) = rest.find('"') {
            let val_content = &rest[quote_start + 1..];
            let mut result = String::new();
            let mut chars = val_content.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    if let Some(next_c) = chars.peek() {
                        match next_c {
                            'n' => {
                                result.push('\n');
                                chars.next();
                            }
                            't' => {
                                result.push('\t');
                                chars.next();
                            }
                            'r' => {
                                chars.next();
                            }
                            '"' => {
                                result.push('"');
                                chars.next();
                            }
                            '\\' => {
                                result.push('\\');
                                chars.next();
                            }
                            _ => {
                                result.push(c);
                            }
                        }
                    } else {
                        result.push(c);
                    }
                } else if c == '"' {
                    break;
                } else {
                    result.push(c);
                }
            }
            return result;
        }
    }
    String::new()
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

    #[test]
    fn wraps_long_unbroken_words() {
        assert_eq!(wrap_text("abcdefgh", 3), vec!["abc", "def", "gh"]);
    }

    #[test]
    fn layout_is_responsive() {
        let app = App::new("test-model / test-provider", std::path::PathBuf::new());

        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let wide = terminal.backend().buffer().content().to_vec();
        let wide_text: String = wide.iter().map(|cell| cell.symbol()).collect();
        assert!(wide_text.contains("NORVEXUM"));
        assert!(wide_text.contains("ACTIVITY"));

        let backend = TestBackend::new(70, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let narrow = terminal.backend().buffer().content().to_vec();
        let narrow_text: String = narrow.iter().map(|cell| cell.symbol()).collect();
        assert!(narrow_text.contains("NORVEXUM"));
        assert!(!narrow_text.contains("ACTIVITY"));
    }

    #[test]
    fn inline_markdown_parses_bold() {
        let spans =
            parse_inline_markdown("hello **world**", Style::default(), LineStyle::Assistant);
        assert!(spans.len() >= 2);
    }

    #[test]
    fn inline_markdown_parses_code() {
        let spans = parse_inline_markdown("use `println!`", Style::default(), LineStyle::Assistant);
        assert!(spans.len() >= 2);
    }
}
