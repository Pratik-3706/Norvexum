pub mod theme;

use std::io;

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseEventKind,
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
}

#[derive(Debug, Clone)]
pub struct ChatLine {
    pub content: String,
    pub style: LineStyle,
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
    pub fn new(model_info: &str) -> Self {
        Self {
            chat_lines: vec![ChatLine {
                content: "Ask a question, request a change, or type /help.".into(),
                style: LineStyle::Status,
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
        }
    }

    pub fn add_user_message(&mut self, message: String) {
        self.chat_lines.push(ChatLine {
            content: message,
            style: LineStyle::User,
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
                });
                self.auto_scroll();
            }
            AgentEvent::ToolStart { name, id } => {
                self.status = format!("Running {name}");
                self.tool_log.push(ToolLogEntry {
                    id,
                    name,
                    status: ToolStatus::Running,
                    detail: "Waiting for arguments".into(),
                });
                self.auto_scroll();
            }
            AgentEvent::ToolArgsDelta { id, delta } => {
                if let Some(entry) = self.tool_log.iter_mut().rev().find(|entry| entry.id == id) {
                    if entry.detail == "Waiting for arguments" {
                        entry.detail.clear();
                    }
                    entry.detail.push_str(&delta);
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
                        id,
                        name,
                        status: if success {
                            ToolStatus::Success
                        } else {
                            ToolStatus::Failed
                        },
                        detail: compact_detail(&result, 180),
                    });
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
                    content: format!("Updated {path}\n{}", compact_detail(&content_preview, 160)),
                    style: LineStyle::FileWrite,
                });
                self.auto_scroll();
            }
            AgentEvent::Done { usage } => {
                self.is_processing = false;
                self.is_thinking = false;
                self.status = usage.map_or_else(
                    || "Ready".into(),
                    |u| format!("Done - {} tokens", u.total_tokens),
                );
                self.auto_scroll();
            }
            AgentEvent::Error(error) => {
                self.chat_lines.push(ChatLine {
                    content: error,
                    style: LineStyle::Error,
                });
                self.is_processing = false;
                self.is_thinking = false;
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
        }
    }

    fn auto_scroll(&mut self) {
        self.chat_scroll = 0;
    }

    pub fn handle_key(&mut self, key: KeyCode, modifiers: KeyModifiers) -> Option<String> {
        match key {
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
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
            KeyCode::Char('l') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.clear();
                self.cursor_pos = 0;
                None
            }
            KeyCode::Enter if !self.is_processing => {
                let message = self.input.trim().to_string();
                if message.is_empty() {
                    return None;
                }
                self.input_history.push(message.clone());
                self.history_pos = None;
                self.input.clear();
                self.cursor_pos = 0;
                self.add_user_message(message.clone());
                Some(message)
            }
            KeyCode::Char(c) if !self.is_processing => {
                self.insert_text(&c.to_string());
                None
            }
            KeyCode::Backspace if !self.is_processing && self.cursor_pos > 0 => {
                let start = byte_index(&self.input, self.cursor_pos - 1);
                let end = byte_index(&self.input, self.cursor_pos);
                self.input.replace_range(start..end, "");
                self.cursor_pos -= 1;
                None
            }
            KeyCode::Delete if !self.is_processing && self.cursor_pos < char_count(&self.input) => {
                let start = byte_index(&self.input, self.cursor_pos);
                let end = byte_index(&self.input, self.cursor_pos + 1);
                self.input.replace_range(start..end, "");
                None
            }
            KeyCode::Left if !self.is_processing => {
                self.cursor_pos = self.cursor_pos.saturating_sub(1);
                None
            }
            KeyCode::Right if !self.is_processing => {
                self.cursor_pos = (self.cursor_pos + 1).min(char_count(&self.input));
                None
            }
            KeyCode::Home if !self.is_processing => {
                self.cursor_pos = 0;
                None
            }
            KeyCode::End if !self.is_processing => {
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
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let title = Line::from(vec![
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
    ]);
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
            "REASONING  live",
            Style::default()
                .fg(app.theme.thinking_color)
                .add_modifier(Modifier::BOLD),
        )));
        let recent = wrap_text(&app.thinking_text, width);
        for line in recent.iter().skip(recent.len().saturating_sub(4)) {
            lines.push(Line::from(Span::styled(
                format!("| {line}"),
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

fn draw_tools(frame: &mut Frame, area: Rect, app: &App) {
    let mut lines = vec![Line::from(Span::styled(
        "ACTIVITY",
        Style::default()
            .fg(app.theme.dim_color)
            .add_modifier(Modifier::BOLD),
    ))];
    lines.push(Line::default());

    if app.tool_log.is_empty() {
        lines.push(Line::from(Span::styled(
            "Tool runs will appear here.",
            Style::default().fg(app.theme.dim_color),
        )));
    }

    for entry in app.tool_log.iter().rev().take(8) {
        let (mark, color) = match entry.status {
            ToolStatus::Running => ("~", app.theme.tool_color),
            ToolStatus::Success => ("+", app.theme.success_color),
            ToolStatus::Failed => ("!", app.theme.error_color),
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{mark} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                &entry.name,
                Style::default()
                    .fg(app.theme.fg)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        for detail in wrap_text(&entry.detail, area.width.saturating_sub(5) as usize)
            .into_iter()
            .take(2)
        {
            lines.push(Line::from(Span::styled(
                format!("  {detail}"),
                Style::default().fg(app.theme.dim_color),
            )));
        }
        lines.push(Line::default());
    }

    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::default().padding(Padding::new(2, 1, 1, 0)))
            .wrap(Wrap { trim: true }),
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
    let display = if app.is_processing {
        "Working... press Esc to stop".to_string()
    } else if visible.is_empty() {
        "What should we build?".to_string()
    } else {
        visible
    };
    let color = if app.input.is_empty() || app.is_processing {
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
    } else {
        " Enter send  |  Ctrl+Up history  |  PgUp scroll  |  Ctrl+C quit "
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
) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(event) = event::read() {
            if event_tx.send(event).is_err() {
                break;
            }
        }
    });

    let result = async {
        loop {
            terminal.draw(|frame| draw(frame, &app))?;
            tokio::select! {
                Some(event) = agent_rx.recv() => app.handle_agent_event(event),
                Some(event) = event_rx.recv() => match event {
                    Event::Key(key) if key.kind == event::KeyEventKind::Press => {
                        if key.code == KeyCode::Esc {
                            if app.is_processing {
                                let _ = cancel_tx.send(());
                            } else {
                                app.should_quit = true;
                            }
                        } else if let Some(message) = app.handle_key(key.code, key.modifiers) {
                            let _ = user_tx.send(message);
                        }
                    }
                    Event::Mouse(mouse) => match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            app.chat_scroll = app.chat_scroll.saturating_add(3);
                        }
                        MouseEventKind::ScrollDown => {
                            app.chat_scroll = app.chat_scroll.saturating_sub(3);
                        }
                        _ => {}
                    },
                    _ => {}
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

fn render_message(
    text: &str,
    width: usize,
    color: ratatui::style::Color,
    kind: LineStyle,
) -> Vec<Line<'static>> {
    let mut result = Vec::new();
    let mut in_code = false;
    for raw in text.lines() {
        if raw.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        let (prefix, content, style) = if in_code {
            (
                "  ",
                raw,
                Style::default()
                    .fg(color)
                    .bg(ratatui::style::Color::Rgb(25, 28, 40)),
            )
        } else if let Some(heading) = raw.strip_prefix("### ") {
            (
                "",
                heading,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )
        } else if let Some(heading) = raw.strip_prefix("## ").or_else(|| raw.strip_prefix("# ")) {
            (
                "",
                heading,
                Style::default()
                    .fg(color)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )
        } else if raw.starts_with("- ") || raw.starts_with("* ") {
            ("  - ", &raw[2..], Style::default().fg(color))
        } else {
            ("  ", raw, Style::default().fg(color))
        };
        let wrap_width = width.saturating_sub(char_count(prefix)).max(1);
        let wrapped = wrap_text(content, wrap_width);
        if wrapped.is_empty() {
            result.push(Line::default());
        } else {
            for (index, line) in wrapped.into_iter().enumerate() {
                let marker = if index == 0 { prefix } else { "  " };
                let line_style = if kind == LineStyle::Thinking {
                    style.add_modifier(Modifier::ITALIC)
                } else {
                    style
                };
                result.push(Line::from(Span::styled(
                    format!("{marker}{line}"),
                    line_style,
                )));
            }
        }
    }
    result
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn unicode_input_editing_uses_character_boundaries() {
        let mut app = App::new("test");
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
        let app = App::new("test-model / test-provider");

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
}
