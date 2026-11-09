// ===========================================================================
// The screen, top to bottom:
//
//   tab bar     the logo, one tab per agent, project and layout
//   timeline    the start screen, or the agents in focus, split or grid layout
//   composer    a rule that carries the run's state, then the message being typed
//   status      model, tokens, cost, background work, then key hints or a notice
// ===========================================================================

use super::*;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    text::Text,
    widgets::{
        Block, BorderType, Borders, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
};

/// The wordmark on the start screen.
const LOGO: [&str; 2] = [
    "█▄ █ █▀█ █▀█ █ █ █▀▀ ▀▄▀ █ █ █▀▄▀█",
    "█ ▀█ █▄█ █▀▄ ▀▄▀ ██▄ █ █ █▄█ █ ▀ █",
];
/// Rows a card needs to be worth drawing in the grid.
const MIN_CARD_ROWS: u16 = 7;
/// Text wider than this is hard to read, however wide the terminal is.
const MAX_TEXT_WIDTH: usize = 140;
/// Recent sessions listed on the start screen.
const START_SESSIONS: usize = 5;

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(app.theme.bg).fg(app.theme.fg)),
        area,
    );

    let input_width = area.width.saturating_sub(4).max(1) as usize;
    let input_rows = (input_line_count(&app.input, input_width) as u16).clamp(1, MAX_INPUT_ROWS);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(input_rows + 1),
            Constraint::Length(1),
        ])
        .split(area);

    for pane in &app.panes {
        pane.outer.set(Rect::default());
        pane.area.set(Rect::default());
        pane.pill.set(Rect::default());
    }
    app.start_rows.borrow_mut().clear();

    draw_tab_bar(frame, rows[0], app);
    if app.show_start_screen() {
        draw_start(frame, rows[1], app);
    } else {
        draw_agents(frame, rows[1], app);
    }
    draw_composer(frame, rows[2], app);
    draw_status(frame, rows[3], app);

    if app.approvals.is_empty() && app.overlay.is_none() {
        draw_suggestions(frame, rows[2], app);
    }
    if let (Some(start), Some(end)) = (app.selection_start, app.selection_end) {
        highlight_selection(frame, start, end, app.panes[app.selection_pane].area.get());
    }
    if let Some(overlay) = &app.overlay {
        overlay::draw(frame, area, app, overlay);
    }
    if let Some(pending) = app.approvals.front() {
        draw_approval(frame, rows[1], app, pending);
    }
}

/// `text` cut to `width` display columns, ending in … when it had to be cut.
pub(super) fn fit(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// "Enter send · Esc stop" with the keys brighter than their actions.
fn hint_spans(pairs: &[(&str, &str)], theme: &Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, (key, action)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", Style::default().fg(theme.border_color)));
        }
        spans.push(Span::styled(key.to_string(), Style::default().fg(theme.fg)));
        spans.push(Span::styled(
            format!(" {action}"),
            Style::default().fg(theme.dim_color),
        ));
    }
    spans
}

fn highlight_selection(frame: &mut Frame, start: (u16, u16), end: (u16, u16), bounds: Rect) {
    if bounds.width == 0 || bounds.height == 0 {
        return;
    }
    let (p1, p2) = if (start.1, start.0) <= (end.1, end.0) {
        (start, end)
    } else {
        (end, start)
    };
    let style = Style::default()
        .bg(Color::Rgb(46, 56, 92))
        .fg(Color::Rgb(240, 242, 248));
    let buffer = frame.buffer_mut();
    let right = bounds.x + bounds.width - 1;
    for row in p1.1.max(bounds.y)..=p2.1.min(bounds.y + bounds.height - 1) {
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

// ── Tab bar ───────────────────────────────────────────────────────────────

/// The glyph and color that say how an agent is doing.
fn state_mark(app: &App, view: Option<usize>) -> (&'static str, Color) {
    let theme = &app.theme;
    let spinner = SPINNER[app.spinner_frame % SPINNER.len()];
    let waiting = app.approvals.iter().any(|a| match (&a.agent, view) {
        (None, None) => true,
        (Some((id, _)), Some(i)) => app.subagents.get(i).is_some_and(|s| &s.id == id),
        _ => false,
    });
    if waiting {
        return ("!", theme.tool_color);
    }
    match view.and_then(|i| app.subagents.get(i)) {
        Some(sub) => match sub.state {
            AgentState::Running => (spinner, theme.tool_color),
            AgentState::Done => ("✓", theme.success_color),
            AgentState::Failed => ("✗", theme.error_color),
        },
        None if app.is_processing => (spinner, theme.tool_color),
        None if app.status == "Error" => ("✗", theme.error_color),
        None => ("●", theme.success_color),
    }
}

fn agent_title(app: &App, view: Option<usize>) -> String {
    match view.and_then(|i| app.subagents.get(i)) {
        Some(sub) => sub.name.clone(),
        None => "main".to_string(),
    }
}

/// What an agent is doing right now, in a few words.
fn agent_activity(app: &App, view: Option<usize>) -> String {
    match view.and_then(|i| app.subagents.get(i)) {
        Some(sub) => sub.current_activity(&app.project_root),
        None => {
            let running = app
                .main
                .tool_log
                .iter()
                .rev()
                .find(|t| t.status == ToolStatus::Running);
            match running {
                Some(t) => {
                    let what = clean_paths(&summarize_args(&t.name, &t.args), &app.project_root);
                    format!("{} {}", t.name, what).trim().to_string()
                }
                None => app.status.clone(),
            }
        }
    }
}

fn draw_tab_bar(frame: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let mut tabs = app.tabs.borrow_mut();
    tabs.clear();

    let badge = Span::styled(
        " NORVEXUM ",
        Style::default()
            .fg(theme.bg)
            .bg(theme.accent_color)
            .add_modifier(Modifier::BOLD),
    );
    let mut used = badge.width() as u16;
    let mut spans = vec![badge];

    // On the right: where we are and how the agents are laid out.
    let mut right: Vec<Span<'static>> = Vec::new();
    let dim = Style::default().fg(theme.dim_color);
    let project = app
        .project_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if !project.is_empty() {
        right.push(Span::styled(project, dim));
    }
    if !app.subagents.is_empty() {
        let layout = match app.effective_layout() {
            AgentLayout::Focus => "focus",
            AgentLayout::Split => "split",
            AgentLayout::Grid => "grid",
        };
        right.push(Span::styled(format!("  ▦ {layout}"), dim));
    }
    let right_w = right.iter().map(|s| s.width()).sum::<usize>() as u16 + 1;
    let show_right = area.width >= right_w + 40;
    let limit = area
        .width
        .saturating_sub(if show_right { right_w } else { 0 });

    if !app.subagents.is_empty() {
        used += 1;
        spans.push(Span::raw(" "));
        // Start late enough that the focused tab fits.
        let label = |view: Option<usize>| {
            let number = view.map_or(String::new(), |i| format!("{} ", i + 1));
            format!(" {{}} {number}{} ", fit(&agent_title(app, view), 16))
        };
        let slots: Vec<Option<usize>> = std::iter::once(None)
            .chain((0..app.subagents.len()).map(Some))
            .collect();
        let width_of = |view: Option<usize>| label(view).replace("{}", "x").width() as u16;
        let focus_slot = app.focus.map_or(0, |i| i + 1);
        let room = limit.saturating_sub(used + 5);
        let mut first = 0;
        while first < focus_slot
            && slots[first..=focus_slot]
                .iter()
                .map(|v| width_of(*v))
                .sum::<u16>()
                > room
        {
            first += 1;
        }
        if first > 0 {
            let more = format!("‹{first} ");
            used += more.width() as u16;
            spans.push(Span::styled(more, dim));
        }
        let mut last = first;
        for &view in &slots[first..] {
            let w = width_of(view);
            if used + w + 3 > limit {
                break;
            }
            let focused = view == app.focus;
            let (mark, color) = state_mark(app, view);
            let bg = if focused { theme.surface } else { theme.bg };
            let text = label(view);
            let (head, tail) = text.split_once("{}").unwrap_or(("", ""));
            let name_style = if focused {
                Style::default()
                    .fg(theme.fg)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.dim_color).bg(bg)
            };
            spans.push(Span::styled(head.to_string(), name_style));
            spans.push(Span::styled(mark, Style::default().fg(color).bg(bg)));
            spans.push(Span::styled(tail.to_string(), name_style));
            tabs.push((Rect::new(area.x + used, area.y, w, 1), view));
            used += w;
            last += 1;
        }
        if last < slots.len() {
            spans.push(Span::styled(format!(" +{}›", slots.len() - last), dim));
        }
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    if show_right {
        frame.render_widget(
            Paragraph::new(Line::from(right)),
            Rect::new(
                area.x + area.width.saturating_sub(right_w),
                area.y,
                right_w.min(area.width),
                1,
            ),
        );
    }
}

// ── Start screen ──────────────────────────────────────────────────────────

/// "3h ago" for an RFC 3339 timestamp.
pub(super) fn relative_time(timestamp: &str) -> String {
    let Ok(then) = chrono::DateTime::parse_from_rfc3339(timestamp) else {
        return String::new();
    };
    let secs = (chrono::Utc::now() - then.with_timezone(&chrono::Utc))
        .num_seconds()
        .max(0);
    match secs {
        0..60 => "just now".to_string(),
        60..3_600 => format!("{}m ago", secs / 60),
        3_600..86_400 => format!("{}h ago", secs / 3_600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

fn draw_start(frame: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let width = area.width.saturating_sub(4).min(76);
    let dim = Style::default().fg(theme.dim_color);
    let label = |text: &str| Span::styled(format!("{text:<10}"), dim);
    let inner_w = width as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();

    if inner_w >= LOGO[0].width() {
        for row in LOGO {
            lines.push(Line::from(Span::styled(
                row,
                Style::default().fg(theme.accent_color),
            )));
        }
    } else {
        lines.push(Line::from(Span::styled(
            "NORVEXUM",
            Style::default()
                .fg(theme.accent_color)
                .add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::default());

    let room = inner_w.saturating_sub(10);
    lines.push(Line::from(vec![
        label("project"),
        Span::styled(
            fit(&app.project_root.display().to_string(), room),
            Style::default().fg(theme.fg),
        ),
    ]));
    let (model, provider) = app
        .model_info
        .rsplit_once(" / ")
        .unwrap_or((app.model_info.as_str(), ""));
    let model_row = lines.len();
    let provider_text = if provider.is_empty() {
        String::new()
    } else {
        format!("  {provider}")
    };
    lines.push(Line::from(vec![
        label("model"),
        Span::styled(
            fit(model, room.saturating_sub(provider_text.width())),
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
        ),
        Span::styled(provider_text, dim),
    ]));
    let servers = crate::tools::mcp::list();
    if !servers.is_empty() {
        let connected = servers
            .iter()
            .filter(|s| matches!(s.status, crate::tools::mcp::Status::Connected))
            .count();
        lines.push(Line::from(vec![
            label("mcp"),
            Span::styled(
                format!("{connected} of {} connected", servers.len()),
                Style::default().fg(theme.fg),
            ),
        ]));
    }
    lines.push(Line::default());

    let sessions_first = lines.len();
    let shown = app.recent_sessions.len().min(START_SESSIONS);
    // Skip the list when it would push the keys off a short terminal.
    let show_sessions = shown > 0 && area.height as usize >= lines.len() + shown + 4;
    if show_sessions {
        lines.push(Line::from(Span::styled("recent", dim)));
        for session in &app.recent_sessions[..shown] {
            let when = relative_time(&session.updated_at);
            let title = if session.title.is_empty() {
                format!("{} messages", session.message_count)
            } else {
                session.title.clone()
            };
            let room = inner_w.saturating_sub(when.width() + 4);
            let title = fit(&title, room);
            let gap = room.saturating_sub(title.width());
            lines.push(Line::from(vec![
                Span::styled("  ", dim),
                Span::styled(title, Style::default().fg(theme.fg)),
                Span::raw(" ".repeat(gap + 2)),
                Span::styled(when, dim),
            ]));
        }
        lines.push(Line::default());
    }

    lines.push(Line::from(Span::styled(
        "Ask for a change, or type / for commands.",
        Style::default().fg(theme.fg),
    )));
    let pairs: &[(&str, &str)] = if inner_w >= 60 {
        &[
            ("Ctrl+O", "model"),
            ("Ctrl+G", "sessions"),
            ("Ctrl+K", "everything"),
            ("F1", "keys"),
        ]
    } else {
        &[("Ctrl+O", "model"), ("Ctrl+K", "everything")]
    };
    lines.push(Line::from(hint_spans(pairs, theme)));

    let height = (lines.len() as u16).min(area.height);
    let top = area.y + area.height.saturating_sub(height) / 2;
    let left = area.x + area.width.saturating_sub(width) / 2;
    let body = Rect::new(left, top, width, height);
    frame.render_widget(Paragraph::new(Text::from(lines)), body);

    if model_row < height as usize {
        app.model_chip
            .set(Rect::new(left, top + model_row as u16, width, 1));
    }
    if show_sessions {
        let mut rows = app.start_rows.borrow_mut();
        for i in 0..shown {
            let y = sessions_first + 1 + i;
            if y < height as usize {
                rows.push((Rect::new(left, top + y as u16, width, 1), i));
            }
        }
    }
}

// ── Agents ────────────────────────────────────────────────────────────────

fn draw_agents(frame: &mut Frame, area: Rect, app: &App) {
    match app.effective_layout() {
        AgentLayout::Focus => draw_focus(frame, area, app),
        AgentLayout::Split => {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(area);
            draw_card(frame, columns[0], app, 0);
            draw_card(frame, columns[1], app, 1);
        }
        AgentLayout::Grid => draw_grid(frame, area, app),
    }
}

/// One agent, full width. A sub-agent gets a header row saying which it is.
fn draw_focus(frame: &mut Frame, area: Rect, app: &App) {
    let pane = &app.panes[0];
    pane.outer.set(area);
    let mut body = area;
    if let Some(i) = app.focus.filter(|&i| i < app.subagents.len()) {
        if area.height > 2 {
            let sub = &app.subagents[i];
            let theme = &app.theme;
            let dim = Style::default().fg(theme.dim_color);
            let (mark, color) = state_mark(app, app.focus);
            let mut facts = format!("{} · {}", sub.state_word(), format_duration(sub.elapsed()));
            if sub.tokens > 0 {
                facts.push_str(&format!(" · {} tok", format_tokens(sub.tokens)));
            }
            let head = format!("agent {} · {}", i + 1, sub.name);
            let room = (area.width as usize).saturating_sub(head.width() + facts.width() + 10);
            let line = Line::from(vec![
                Span::styled(format!(" {mark} "), Style::default().fg(color)),
                Span::styled(
                    head,
                    Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {facts}  "), dim),
                Span::styled(
                    fit(
                        &compact_detail(&sub.current_activity(&app.project_root), 300),
                        room,
                    ),
                    dim,
                ),
            ]);
            frame.render_widget(
                Paragraph::new(line).style(Style::default().bg(theme.surface)),
                Rect::new(area.x, area.y, area.width, 1),
            );
            body = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
        }
    }
    let text = Rect::new(
        body.x + 1,
        body.y,
        body.width.saturating_sub(3),
        body.height,
    );
    draw_transcript(frame, text, body.x + body.width.saturating_sub(1), app, 0);
}

/// An agent in a rounded card: state and name above, activity below.
fn draw_card(frame: &mut Frame, area: Rect, app: &App, pane_idx: usize) {
    let theme = &app.theme;
    let view = app.pane_view(pane_idx);
    let focused = app.focused_pane() == pane_idx;
    let (mark, color) = state_mark(app, view);
    let dim = Style::default().fg(theme.dim_color);
    let number = view.map_or(0, |i| i + 1);
    let title = Line::from(vec![
        Span::raw(" "),
        Span::styled(mark, Style::default().fg(color)),
        Span::styled(
            format!(" {number} {} ", agent_title(app, view)),
            if focused {
                Style::default()
                    .fg(theme.accent_color)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.fg)
            },
        ),
    ]);
    let activity = compact_detail(&agent_activity(app, view), 300);
    let room = (area.width as usize).saturating_sub(6);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused {
            theme.accent_color
        } else {
            theme.border_color
        }))
        .title(title)
        .title_bottom(Line::from(Span::styled(
            format!(" {} ", fit(&activity, room)),
            dim,
        )));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.panes[pane_idx].outer.set(area);
    let text = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );
    // The scrollbar thumb runs along the card's right border.
    draw_transcript(
        frame,
        text,
        area.x + area.width.saturating_sub(1),
        app,
        pane_idx,
    );
}

/// Every agent as a card. What doesn't fit goes on further pages, and the page
/// follows the focused agent.
fn draw_grid(frame: &mut Frame, area: Rect, app: &App) {
    let slots = app.subagents.len() + 1;
    let columns = match area.width {
        0..90 => 1,
        90..160 => 2,
        _ => 3,
    };
    let max_rows = (area.height / MIN_CARD_ROWS).max(1) as usize;
    let capacity = (columns * max_rows).min(MAX_PANES);
    app.grid_capacity.set(capacity);
    let first = app.grid_page_first();
    let shown = (slots - first).min(capacity);
    let rows = shown.div_ceil(columns);

    let row_areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints((0..rows).map(|_| Constraint::Ratio(1, rows as u32)))
        .split(area);
    for (r, row_area) in row_areas.iter().enumerate() {
        let in_row = (shown - r * columns).min(columns);
        let cells = Layout::default()
            .direction(Direction::Horizontal)
            .constraints((0..in_row).map(|_| Constraint::Ratio(1, in_row as u32)))
            .split(*row_area);
        for (c, cell) in cells.iter().enumerate() {
            draw_card(frame, *cell, app, r * columns + c);
        }
    }
}

/// The visible rows of the transcript in pane `pane_idx`, with a scrollbar in
/// column `bar_x` when it doesn't all fit.
fn draw_transcript(frame: &mut Frame, area: Rect, bar_x: u16, app: &App, pane_idx: usize) {
    let theme = &app.theme;
    let pane = &app.panes[pane_idx];
    let view = app.pane_view(pane_idx);
    let transcript = app.transcript(view);
    pane.area.set(area);

    let ctx = RenderCtx {
        width: (area.width as usize).clamp(10, MAX_TEXT_WIDTH),
        theme,
        expanded: app.show_reasoning,
        frame: app.spinner_frame,
        root: &app.project_root,
    };
    let view_height = area.height as usize;

    // Heights of every message (cached), then only the visible slice is cloned.
    let heights: Vec<usize> = (0..transcript.lines.len())
        .map(|i| transcript.message_height(i, &ctx))
        .collect();

    let mut extra: Vec<Line<'static>> = Vec::new();
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

    // While scrolled up, rows arriving below don't move what's being read.
    let (drawn_width, drawn_total) = transcript.drawn.get();
    if transcript.scroll.get() > 0 && drawn_width == ctx.width && total > drawn_total {
        transcript
            .scroll
            .set(transcript.scroll.get() + total - drawn_total);
    }
    transcript.drawn.set((ctx.width, total));

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
    let scroll = transcript.scroll.get().min(max_scroll);
    let start = total.saturating_sub(view_height + scroll);
    let end = (start + view_height).min(total);

    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut metas: Vec<RowMeta> = Vec::new();
    let mut tools: Vec<Option<String>> = Vec::new();
    let mut offset = 0;
    for (i, h) in heights.iter().enumerate() {
        if offset + h > start && offset < end {
            let from = start.saturating_sub(offset);
            let to = (end - offset).min(*h);
            let (lines, meta) = transcript.message_slice(i, from, to, &ctx);
            let line = &transcript.lines[i];
            let tool = line
                .tool_call_id
                .clone()
                .filter(|_| matches!(line.style, LineStyle::Tool | LineStyle::FileWrite));
            tools.extend(std::iter::repeat_n(tool, lines.len()));
            rows.extend(lines);
            metas.extend(meta);
        }
        offset += h;
        if offset >= end {
            break;
        }
    }
    if offset < end {
        let from = start.saturating_sub(offset);
        rows.extend(extra.into_iter().skip(from).take(end - offset.max(start)));
    }

    *pane.visible_plain.borrow_mut() = rows
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    metas.resize_with(rows.len(), RowMeta::default);
    tools.resize(rows.len(), None);
    *pane.visible_meta.borrow_mut() = metas;
    *pane.visible_tools.borrow_mut() = tools;

    frame.render_widget(Paragraph::new(Text::from(rows)), area);

    if max_scroll > 0 && area.height > 0 {
        let bar = Rect::new(bar_x, area.y, 1, area.height);
        let mut state = ScrollbarState::new(max_scroll)
            .viewport_content_length(view_height)
            .position(max_scroll - scroll);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(None)
                .thumb_symbol("┃")
                .thumb_style(Style::default().fg(theme.dim_color)),
            bar,
            &mut state,
        );
    }

    if scroll > 0 && area.height > 0 {
        let hint = format!(" ↓ {scroll} more ");
        let w = (hint.width() as u16).min(area.width);
        let rect = Rect::new(
            area.x + area.width.saturating_sub(w),
            area.y + area.height - 1,
            w,
            1,
        );
        frame.render_widget(
            Paragraph::new(hint).style(Style::default().fg(theme.bg).bg(theme.dim_color)),
            rect,
        );
        pane.pill.set(rect);
    }
}

// ── Composer ──────────────────────────────────────────────────────────────

/// What the rule above the text says, and in which color.
fn composer_title(app: &App) -> (String, Color) {
    let theme = &app.theme;
    if let Some(search) = &app.history_search {
        return (
            format!(
                "History search: {}{}",
                search.query,
                if search.found { "" } else { "  (no match)" }
            ),
            theme.accent_color,
        );
    }
    if app.is_processing {
        let mut text = format!(
            "{} {}",
            SPINNER[app.spinner_frame % SPINNER.len()],
            compact_detail(&agent_activity(app, None), 200)
        );
        if let Some(started) = app.run_started {
            text.push_str(&format!(" · {}", format_duration(started.elapsed())));
        }
        return (text, theme.tool_color);
    }
    if app.status == "Error" {
        return ("✗ Error".to_string(), theme.error_color);
    }
    if app.focus.is_some() {
        return ("Messages go to main".to_string(), theme.dim_color);
    }
    if app.status == "Ready" {
        return (String::new(), theme.accent_color);
    }
    (app.status.clone(), theme.dim_color)
}

fn draw_composer(frame: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    if area.height == 0 || area.width < 4 {
        return;
    }
    let rule_color = if app.is_processing {
        theme.border_color
    } else {
        theme.accent_color
    };

    // The rule, with the run's state on the left and what's queued on the right.
    let (title, title_color) = composer_title(app);
    let right = if app.user_backlog.is_empty() {
        String::new()
    } else {
        format!(" {} queued ─", app.user_backlog.len())
    };
    let width = area.width as usize;
    let title = fit(&title, width.saturating_sub(right.width() + 5));
    let mut spans = vec![Span::styled("─", Style::default().fg(rule_color))];
    let mut used = 1;
    if !title.is_empty() {
        let shown = format!(" {title} ");
        used += shown.width();
        spans.push(Span::styled(
            shown,
            Style::default()
                .fg(title_color)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let fill = width.saturating_sub(used + right.width());
    spans.push(Span::styled(
        "─".repeat(fill),
        Style::default().fg(rule_color),
    ));
    spans.push(Span::styled(right, Style::default().fg(theme.tool_color)));
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(area.x, area.y, area.width, 1),
    );

    let rows = area.height - 1;
    if rows == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Span::styled(
            "›",
            Style::default().fg(rule_color).add_modifier(Modifier::BOLD),
        )),
        Rect::new(area.x + 1, area.y + 1, 1, 1),
    );
    let inner = Rect::new(area.x + 3, area.y + 1, area.width - 4, rows);
    app.input_box.set(area);
    app.input_area.set(inner);
    app.input_scroll.set(0);
    let takes_cursor = app.overlay.is_none() && app.approvals.is_empty();

    if app.input.is_empty() {
        let placeholder = if app.history_search.is_some() {
            "Type to search earlier prompts · Ctrl+R older · Esc cancel"
        } else if app.is_processing {
            "Type to queue a follow-up · Esc to stop"
        } else {
            "What should we build?"
        };
        frame.render_widget(
            Paragraph::new(fit(placeholder, inner.width as usize))
                .style(Style::default().fg(theme.dim_color)),
            inner,
        );
        if takes_cursor {
            frame.set_cursor_position((inner.x, inner.y));
        }
        return;
    }

    let text_width = inner.width.max(1) as usize;
    let (lines, cursor_row, cursor_col) = layout_input(&app.input, app.cursor_pos, text_width);
    let height = inner.height as usize;
    let first = (cursor_row + 1).saturating_sub(height);
    app.input_scroll.set(first);
    let shown: Vec<Line> = lines
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
    frame.render_widget(Paragraph::new(Text::from(shown)), inner);
    if takes_cursor {
        frame.set_cursor_position((
            inner.x + cursor_col as u16,
            inner.y + (cursor_row - first) as u16,
        ));
    }
}

fn draw_suggestions(frame: &mut Frame, composer: Rect, app: &App) {
    let suggestions = app.command_suggestions();
    if suggestions.is_empty() {
        return;
    }
    let theme = &app.theme;
    let shown = suggestions.len().min(7);
    let selected = app.palette_index.min(suggestions.len() - 1);
    let first = (selected + 1).saturating_sub(shown);
    let width = composer.width.saturating_sub(2).min(72);
    let height = (shown as u16 + 2).min(composer.y);
    let rect = Rect::new(composer.x + 1, composer.y - height, width, height);
    let lines: Vec<Line> = suggestions
        .iter()
        .enumerate()
        .skip(first)
        .take(shown)
        .map(|(i, (cmd, desc))| {
            let style = if i == selected {
                Style::default().fg(theme.fg).bg(theme.selection)
            } else {
                Style::default().fg(theme.fg)
            };
            Line::from(vec![
                Span::styled(format!(" {cmd:<17}"), style.add_modifier(Modifier::BOLD)),
                Span::styled(format!(" {desc}"), Style::default().fg(theme.dim_color)),
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
        Paragraph::new(Text::from(lines))
            .style(Style::default().bg(theme.surface))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(theme.border_color))
                    .title(Span::styled(hint, Style::default().fg(theme.dim_color))),
            ),
        rect,
    );
}

// ── Status line ───────────────────────────────────────────────────────────

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let dim = Style::default().fg(theme.dim_color);
    let (model, provider) = app
        .model_info
        .rsplit_once(" / ")
        .unwrap_or((app.model_info.as_str(), ""));

    // Left: model, then what this session has used and what runs in the background.
    let model_text = fit(model, (area.width as usize / 2).clamp(8, 44));
    let mut left = vec![
        Span::raw(" "),
        Span::styled(
            model_text.clone(),
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ▾", dim),
    ];
    let chip_w = model_text.width() as u16 + 3;
    if !app.show_start_screen() {
        app.model_chip
            .set(Rect::new(area.x, area.y, chip_w.min(area.width), 1));
    }
    if area.width >= 100 && !provider.is_empty() {
        left.push(Span::styled(format!(" {provider}"), dim));
    }
    let mut facts: Vec<String> = Vec::new();
    if app.total_tokens_used > 0 {
        facts.push(format!(
            "↑{} ↓{}",
            format_tokens(app.prompt_tokens_used),
            format_tokens(app.completion_tokens_used)
        ));
        if let Some((input, output)) = app.model_cost.filter(|(i, o)| *i > 0.0 || *o > 0.0) {
            let cost = app.prompt_tokens_used as f64 / 1000.0 * input
                + app.completion_tokens_used as f64 / 1000.0 * output;
            facts.push(format!("~${cost:.2}"));
        }
    }
    let running = app.subagents.iter().filter(|s| s.is_running()).count();
    if running > 0 {
        facts.push(format!(
            "{running} agent{} running",
            if running == 1 { "" } else { "s" }
        ));
    }
    if app.running_commands > 0 {
        facts.push(format!(
            "{} process{}",
            app.running_commands,
            if app.running_commands == 1 { "" } else { "es" }
        ));
    }
    if crate::tools::browser::is_open() {
        facts.push(if crate::tools::browser::agent_busy() {
            "browser in use".to_string()
        } else {
            "browser open".to_string()
        });
    }
    if let Some(skill) = &app.active_skill {
        facts.push(format!("✦ {skill}"));
    }
    for fact in facts {
        left.push(Span::styled("  ", dim));
        left.push(Span::styled(fact, dim));
    }
    let left_w: usize = left.iter().map(|s| s.width()).sum();
    frame.render_widget(Paragraph::new(Line::from(left)), area);

    // Right: a notice while one is fresh, otherwise the keys that matter now.
    let width = area.width as usize;
    let room = width.saturating_sub(left_w + 2);
    let right: Vec<Span<'static>> = match app
        .notice
        .as_ref()
        .filter(|(_, at)| at.elapsed() < NOTICE_DURATION)
    {
        Some((notice, _)) => vec![Span::styled(
            fit(notice, room),
            Style::default().fg(theme.accent_color),
        )],
        None => {
            let options = status_hints(app);
            let mut chosen = Vec::new();
            for pairs in options {
                let spans = hint_spans(pairs, theme);
                if spans.iter().map(|s| s.width()).sum::<usize>() <= room {
                    chosen = spans;
                    break;
                }
            }
            chosen
        }
    };
    let right_w = right.iter().map(|s| s.width()).sum::<usize>() as u16;
    if right_w > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(right)),
            Rect::new(
                area.x + area.width.saturating_sub(right_w + 1),
                area.y,
                right_w,
                1,
            ),
        );
    }
}

/// The keys that matter right now, longest first; the first set that fits is shown.
fn status_hints(app: &App) -> Vec<&'static [(&'static str, &'static str)]> {
    if !app.approvals.is_empty() {
        return vec![&[("y", "allow"), ("n", "deny")]];
    }
    if app.selection_start.is_some() {
        return vec![
            &[("Ctrl+C", "copy again"), ("Esc", "clear")],
            &[("Esc", "clear")],
        ];
    }
    if app.history_search.is_some() {
        return vec![
            &[
                ("Ctrl+R", "older"),
                ("Enter", "send"),
                ("→", "edit"),
                ("Esc", "cancel"),
            ],
            &[("Esc", "cancel")],
        ];
    }
    match app.effective_layout() {
        AgentLayout::Grid => vec![
            &[
                ("Alt+←→", "move"),
                ("Enter", "open"),
                ("Ctrl+X", "stop agent"),
                ("Alt+G", "close"),
                ("F1", "keys"),
            ],
            &[("Alt+←→", "move"), ("Enter", "open")],
            &[("F1", "keys")],
        ],
        _ if app.focus.is_some() => vec![
            &[
                ("Alt+0", "main"),
                ("Alt+←→", "switch"),
                ("Ctrl+X", "stop agent"),
                ("Ctrl+S", "split"),
                ("Alt+G", "grid"),
                ("F1", "keys"),
            ],
            &[("Alt+0", "main"), ("Ctrl+X", "stop"), ("F1", "keys")],
            &[("F1", "keys")],
        ],
        _ if app.is_processing => vec![
            &[
                ("Esc", "stop"),
                ("Enter", "queue"),
                ("Tab", "tools"),
                ("Ctrl+B", "processes"),
                ("Ctrl+K", "more"),
            ],
            &[("Esc", "stop"), ("Enter", "queue"), ("F1", "keys")],
            &[("Esc", "stop")],
        ],
        _ => vec![
            &[
                ("Enter", "send"),
                ("Alt+Enter", "new line"),
                ("/", "commands"),
                ("Tab", "tools"),
                ("Ctrl+K", "everything"),
                ("F1", "keys"),
            ],
            &[("Enter", "send"), ("Ctrl+K", "everything"), ("F1", "keys")],
            &[("F1", "keys")],
        ],
    }
}

// ── Approval ──────────────────────────────────────────────────────────────

/// A sheet rising from the composer, so the conversation above stays readable.
fn draw_approval(frame: &mut Frame, body: Rect, app: &App, pending: &PendingApproval) {
    let theme = &app.theme;
    let info = &pending.info;
    let args: serde_json::Value = serde_json::from_str(&info.args_preview).unwrap_or_default();
    let dim = Style::default().fg(theme.dim_color);

    let mut lines = vec![Line::from(vec![
        Span::styled("tool  ", dim),
        Span::styled(
            info.tool_name.clone(),
            Style::default()
                .fg(theme.tool_color)
                .add_modifier(Modifier::BOLD),
        ),
    ])];
    if let Some(cmd) = args.get("command").and_then(|c| c.as_str()) {
        for l in cmd.lines() {
            lines.push(Line::from(Span::styled(
                format!("$ {l}"),
                Style::default()
                    .fg(theme.input_color)
                    .add_modifier(Modifier::BOLD),
            )));
        }
        if args.get("background").and_then(|b| b.as_bool()) == Some(true) {
            lines.push(Line::from(Span::styled("(runs in the background)", dim)));
        }
    } else {
        for l in crate::util::truncate_chars(&info.args_preview, 1500).lines() {
            lines.push(Line::from(l.to_string()));
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
        Span::styled("/ask_perms off turns these prompts off", dim),
    ]));

    let text_width = body.width.saturating_sub(4).max(1) as usize;
    let wanted: usize = lines
        .iter()
        .map(|l| l.width().div_ceil(text_width).max(1))
        .sum();
    let height = (wanted as u16 + 2).min(body.height);
    let sheet = Rect::new(body.x, body.y + body.height - height, body.width, height);
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
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.tool_color))
        .padding(Padding::horizontal(1));
    if app.approvals.len() > 1 {
        block = block.title_bottom(Span::styled(
            format!(" {} more waiting ", app.approvals.len() - 1),
            dim,
        ));
    }
    frame.render_widget(Clear, sheet);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .block(block)
            .style(Style::default().fg(theme.fg).bg(theme.surface)),
        sheet,
    );
}
