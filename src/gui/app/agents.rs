// ===========================================================================
// Sub-agents in the GUI — their transcripts, the agent switcher above the
// chat, the side-by-side view and the Agents tab of the right panel.
// ===========================================================================

use super::*;

/// One sub-agent as the GUI shows it.
pub(super) struct SubAgentView {
    pub id: String,
    pub name: String,
    pub task: String,
    pub messages: Vec<GuiMessage>,
    tool_name: Option<String>,
    tool_id: Option<String>,
    tool_args: String,
    /// Its latest status line ("Running read_file", "Thinking (step 2)").
    pub activity: String,
    pub started: std::time::Instant,
    /// (success, summary, when) once it ended.
    pub finished: Option<(bool, String, std::time::Instant)>,
}

/// Which conversation the chat area shows.
#[derive(Clone, PartialEq, Eq)]
pub(super) enum ChatView {
    Main,
    Agent(String),
}

impl SubAgentView {
    pub fn new(id: String, name: String, task: String) -> Self {
        Self {
            id,
            name,
            task,
            messages: Vec::new(),
            tool_name: None,
            tool_id: None,
            tool_args: String::new(),
            activity: "Starting".into(),
            started: std::time::Instant::now(),
            finished: None,
        }
    }

    pub fn running(&self) -> bool {
        self.finished.is_none()
    }

    pub fn elapsed(&self) -> std::time::Duration {
        match &self.finished {
            Some((_, _, at)) => at.duration_since(self.started),
            None => self.started.elapsed(),
        }
    }

    pub(super) fn reply(&mut self) -> &mut GuiMessage {
        if self.messages.last().is_none_or(|m| m.role != "assistant") {
            self.messages.push(GuiMessage {
                role: "assistant".into(),
                content: String::new(),
                thinking: None,
                is_thinking_done: false,
                tool_logs: Vec::new(),
                attachments: Vec::new(),
            });
        }
        self.messages.last_mut().expect("just pushed")
    }

    /// A new step or tool starts a new bubble once the current one has text,
    /// as in the main chat.
    fn split(&mut self) {
        if self
            .messages
            .last()
            .is_some_and(|m| m.role == "assistant" && !m.content.trim().is_empty())
        {
            self.messages.push(GuiMessage {
                role: "assistant".into(),
                content: String::new(),
                thinking: None,
                is_thinking_done: false,
                tool_logs: Vec::new(),
                attachments: Vec::new(),
            });
        }
    }

    /// Fold one of its events into the transcript. `file_change` carries the
    /// project-relative path and diff counts of a `FileWrite`.
    pub fn apply(&mut self, event: &AgentEvent, file_change: Option<(&str, i32, i32)>) {
        match event {
            AgentEvent::Status(s) => {
                if s.starts_with("Thinking (") {
                    self.split();
                }
                self.activity = style::clean_label(s);
                self.reply().tool_logs.push(format!("ℹ️ {s}"));
            }
            AgentEvent::Thinking(t) => {
                let msg = self.reply();
                msg.thinking.get_or_insert_with(String::new).push_str(t);
                msg.tool_logs.push(format!("THINKING_DELTA|{t}"));
            }
            AgentEvent::ThinkingDone => self.reply().is_thinking_done = true,
            AgentEvent::Content(c) => self.reply().content.push_str(c),
            AgentEvent::ToolStart { name, id } => {
                self.split();
                self.activity = format!("Running {}", name.replace('_', " "));
                self.tool_name = Some(name.clone());
                self.tool_id = Some(id.clone());
                self.tool_args.clear();
                self.reply()
                    .tool_logs
                    .push(format!("TOOL_START|{}|{name}", get_tool_icon(name)));
            }
            AgentEvent::ToolArgsDelta { id, delta } => {
                if self.tool_id.as_ref() == Some(id) {
                    self.tool_args.push_str(delta);
                }
            }
            AgentEvent::ToolResult {
                id, name, success, ..
            } => {
                let summary = if self.tool_id.as_ref() == Some(id) {
                    format_tool_args_summary(name, &self.tool_args)
                } else {
                    String::new()
                };
                if self.tool_id.as_ref() == Some(id) {
                    self.tool_name = None;
                    self.tool_id = None;
                    self.tool_args.clear();
                }
                let status = if *success {
                    "✅ Success"
                } else {
                    "❌ Failed"
                };
                self.reply().tool_logs.push(format!(
                    "TOOL_RESULT|{}|{name}|{status}|{summary}",
                    get_tool_icon(name)
                ));
            }
            AgentEvent::FileWrite { .. } => {
                if let Some((path, added, deleted)) = file_change {
                    self.reply().tool_logs.push(format!(
                        "FILE_CHANGE|{}|{path}|+{added}|-{deleted}",
                        get_file_icon(path)
                    ));
                }
            }
            AgentEvent::Warning(w) => {
                let clean = w.replace('\u{fe0f}', "");
                self.reply().tool_logs.push(format!("⚠️ {clean}"));
            }
            AgentEvent::Error(e) => {
                let msg = self.reply();
                if !msg.content.is_empty() {
                    msg.content.push_str("\n\n");
                }
                msg.content.push_str(&format!("**Error:** {e}"));
            }
            _ => {}
        }
    }

    /// The tool it is running right now, with its streamed arguments.
    fn current_tool(&self) -> Option<(&str, &str)> {
        self.tool_name
            .as_deref()
            .filter(|_| self.running())
            .map(|n| (n, self.tool_args.as_str()))
    }
}

/// "42s", "3m 05s".
pub(super) fn format_elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

impl GuiApp {
    /// Top-bar button for the Agents tab, shown even when the right panel is
    /// closed: a small agents glyph whose dot pulses while any agent works,
    /// plus the number of sub-agents running.
    pub(super) fn draw_agents_nav_button(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let running = self.subagents.iter().filter(|s| s.running()).count();
        let busy = self.is_processing || running > 0;
        let active = self.right_sidebar_open && self.right_tab == RightTab::Agents;
        let badge = (running > 0).then(|| running.to_string());
        let badge_w = badge.as_ref().map_or(0.0, |b| 8.0 + 7.0 * b.len() as f32);
        let (rect, resp) =
            ui.allocate_exact_size(egui::vec2(34.0 + badge_w, 28.0), egui::Sense::click());
        let painter = ui.painter();
        if active {
            painter.rect_filled(rect, 6.0, p.hover);
        } else if resp.hovered() {
            painter.rect_filled(rect, 6.0, p.surface);
        }
        let color = if active || resp.hovered() {
            p.text
        } else {
            p.dim
        };
        // Glyph: one agent above two helpers, joined.
        let c = egui::pos2(rect.left() + 17.0, rect.center().y);
        let top = c + egui::vec2(0.0, -5.0);
        let left = c + egui::vec2(-6.0, 5.0);
        let right = c + egui::vec2(6.0, 5.0);
        let stroke = egui::Stroke::new(1.3_f32, color);
        painter.line_segment([top, left], stroke);
        painter.line_segment([top, right], stroke);
        painter.circle_filled(left, 2.4, color);
        painter.circle_filled(right, 2.4, color);
        let top_color = if busy {
            let t = ui.input(|i| i.time) as f32;
            ui.ctx().request_repaint_after(Duration::from_millis(60));
            p.accent
                .gamma_multiply(0.55 + 0.45 * (t * std::f32::consts::TAU / 1.4).sin())
        } else {
            color
        };
        painter.circle_filled(top, 3.0, top_color);
        if let Some(b) = badge {
            let r = egui::Rect::from_min_size(
                egui::pos2(rect.left() + 28.0, rect.center().y - 8.0),
                egui::vec2(badge_w, 16.0),
            );
            painter.rect_filled(r, 8.0, p.accent);
            painter.text(
                r.center(),
                egui::Align2::CENTER_CENTER,
                b,
                egui::FontId::proportional(10.5),
                p.on_accent,
            );
        }
        style::click_flash(ui, &resp, 6.0, p);
        let tip = match running {
            0 if self.is_processing => "Agents · main agent working".to_string(),
            0 => "Agents".to_string(),
            n => format!(
                "Agents · {n} sub-agent{} running",
                if n == 1 { "" } else { "s" }
            ),
        };
        if resp
            .on_hover_text(tip)
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
        {
            if active {
                self.right_sidebar_open = false;
            } else {
                self.right_sidebar_open = true;
                self.right_tab = RightTab::Agents;
            }
        }
    }

    /// Route an event a sub-agent produced.
    pub(super) fn handle_subagent_event(&mut self, id: &str, event: AgentEvent) {
        if let AgentEvent::AgentMessage { from, to, text } = &event {
            self.show_agent_message(from, to, text);
            return;
        }
        // Its file changes count as changes of this chat too.
        let mut file_change = None;
        if let AgentEvent::FileWrite { path, .. } = &event {
            let relative = get_relative_path(path, &self.settings.project_root);
            let (added, deleted) = get_file_diff_stats(&self.settings.project_root, &relative);
            if !self.session_written_files.contains(&relative) {
                self.session_written_files.push(relative.clone());
            }
            self.file_diff_stats.insert(
                relative.clone(),
                (added.max(0) as usize, deleted.max(0) as usize),
            );
            self.pending_fs_reload = true;
            self.last_fs_event = Some(std::time::Instant::now());
            file_change = Some((relative, added, deleted));
        }
        if let AgentEvent::ApprovalRequest {
            id: call_id,
            tool_name,
            args_preview,
        } = &event
        {
            self.approval_pending = Some(crate::ui::ApprovalInfo {
                id: call_id.clone(),
                tool_name: tool_name.clone(),
                args_preview: args_preview.clone(),
            });
            self.approval_agent = Some(id.to_string());
        }
        if let Some(sub) = self.subagents.iter_mut().find(|s| s.id == id) {
            sub.apply(
                &event,
                file_change.as_ref().map(|(p, a, d)| (p.as_str(), *a, *d)),
            );
        }
    }

    /// One agent's note to another, shown in both their transcripts.
    pub(super) fn show_agent_message(&mut self, from: &str, to: &str, text: &str) {
        let line = format!("AGENT_MSG|{from}|{to}|{}", text.replace('|', "/"));
        self.status_text = format!("{from} messaged {to}");
        for name in [from, to] {
            if name == "main" {
                if self.messages.iter().any(|m| m.role == "user") {
                    self.get_or_create_assistant_message()
                        .tool_logs
                        .push(line.clone());
                }
            } else if let Some(sub) = self
                .subagents
                .iter_mut()
                .rev()
                .find(|s| s.name.eq_ignore_ascii_case(name))
            {
                sub.reply().tool_logs.push(line.clone());
            }
        }
    }

    pub(super) fn subagent_name(&self, id: &str) -> Option<&str> {
        self.subagents
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.name.as_str())
    }

    /// The sub-agent the chat area or split view is showing, if any.
    fn viewed_subagent(&self) -> Option<&SubAgentView> {
        match &self.chat_view {
            ChatView::Agent(id) => self.subagents.iter().find(|s| &s.id == id),
            ChatView::Main => None,
        }
    }

    /// Switcher above the chat: Main and every sub-agent, plus the
    /// side-by-side toggle. Only shown once there are sub-agents.
    pub(super) fn draw_agents_strip(&mut self, ctx: &egui::Context, p: &Palette) {
        if self.subagents.is_empty() {
            return;
        }
        egui::TopBottomPanel::top("agents_strip")
            .frame(
                egui::Frame::none()
                    .fill(p.bg)
                    .inner_margin(egui::Margin::symmetric(16.0, 8.0))
                    .stroke(egui::Stroke::new(1.0_f32, p.border)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let main_active = self.chat_view == ChatView::Main || self.split_view;
                    if agent_chip(ui, "Main", self.is_processing, None, main_active, p).clicked() {
                        self.chat_view = ChatView::Main;
                        self.split_view = false;
                    }
                    let mut pick = None;
                    for sub in &self.subagents {
                        let active = self.chat_view == ChatView::Agent(sub.id.clone());
                        let done = sub.finished.as_ref().map(|(ok, _, _)| *ok);
                        let chip = agent_chip(ui, &sub.name, sub.running(), done, active, p)
                            .on_hover_text(crate::util::truncate_chars(&sub.task, 300));
                        if chip.clicked() {
                            pick = Some(sub.id.clone());
                        }
                    }
                    if let Some(id) = pick {
                        self.chat_view = ChatView::Agent(id);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let can_split = self.viewed_subagent().is_some()
                            || self.subagents.iter().any(SubAgentView::running);
                        if can_split
                            && style::chip(ui, "Side by side", self.split_view, p)
                                .on_hover_text(
                                    "Show the main chat and a sub-agent next to each other",
                                )
                                .clicked()
                        {
                            self.split_view = !self.split_view;
                            if self.split_view && self.viewed_subagent().is_none() {
                                let latest = self.subagents.iter().rev().find(|s| s.running());
                                if let Some(sub) = latest.or(self.subagents.last()) {
                                    self.chat_view = ChatView::Agent(sub.id.clone());
                                }
                            }
                        }
                    });
                });
            });
    }

    /// Side-by-side: the viewed sub-agent in a panel beside the main chat.
    pub(super) fn draw_subagent_split(&mut self, ctx: &egui::Context, p: &Palette) {
        if !self.split_view || self.viewed_subagent().is_none() {
            return;
        }
        // Half the chat area, leaving the main chat at least 360 px.
        let room = ctx.available_rect().width();
        let max = (room - 360.0).max(280.0);
        egui::SidePanel::right("subagent_split")
            .resizable(true)
            .default_width((room / 2.0).clamp(280.0, max))
            .width_range(280.0..=max)
            .frame(
                egui::Frame::none()
                    .fill(p.bg)
                    .inner_margin(egui::Margin::symmetric(12.0, 0.0))
                    .stroke(egui::Stroke::new(1.0_f32, p.border)),
            )
            .show(ctx, |ui| self.draw_viewed_transcript(ui, p));
    }

    /// The viewed sub-agent instead of the main chat.
    pub(super) fn draw_subagent_central(&mut self, ctx: &egui::Context, p: &Palette) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(p.bg)
                    .inner_margin(egui::Margin::symmetric(16.0, 0.0)),
            )
            .show(ctx, |ui| self.draw_viewed_transcript(ui, p));
    }

    /// True when the chat area should show a sub-agent rather than the main chat.
    pub(super) fn showing_subagent_alone(&self) -> bool {
        !self.split_view && self.viewed_subagent().is_some()
    }

    fn draw_viewed_transcript(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let Some(index) = self
            .subagents
            .iter()
            .position(|s| ChatView::Agent(s.id.clone()) == self.chat_view)
        else {
            return;
        };
        let mut stop = None;
        let sub = &self.subagents[index];
        egui::ScrollArea::vertical()
            .id_salt(("subagent_scroll", &sub.id))
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                let full = ui.available_width();
                let col = full.min(style::MAX_COLUMN);
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    ui.add_space(((full - col) / 2.0).max(0.0));
                    ui.vertical(|ui| {
                        ui.set_width(col);
                        // Header: name, state, stop.
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(&sub.name)
                                    .font(style::strong(15.0))
                                    .color(p.text),
                            );
                            let (text, color) = state_label(sub, p);
                            ui.label(egui::RichText::new(text).size(12.0).color(color));
                            if sub.running() {
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if style::link_button(ui, "Stop", p)
                                            .on_hover_text("Stop this sub-agent")
                                            .clicked()
                                        {
                                            stop = Some(sub.id.clone());
                                        }
                                    },
                                );
                            }
                        });
                        ui.add_space(6.0);
                        egui::Frame::none()
                            .fill(p.surface)
                            .stroke(egui::Stroke::new(1.0_f32, p.border))
                            .rounding(8.0)
                            .inner_margin(egui::Margin::symmetric(12.0, 8.0))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                style::section_label(ui, "Task", p);
                                ui.label(egui::RichText::new(&sub.task).size(12.5).color(p.dim));
                            });
                        ui.add_space(12.0);

                        let last = sub.messages.len().saturating_sub(1);
                        for (i, msg) in sub.messages.iter().enumerate() {
                            let live = sub.running() && i == last;
                            let has_activity = !msg.tool_logs.is_empty() || msg.thinking.is_some();
                            if has_activity {
                                render_chronological_activity(
                                    ui,
                                    &msg.tool_logs,
                                    p,
                                    if live { sub.current_tool() } else { None },
                                    live,
                                    &msg.thinking,
                                    &self.settings.project_root,
                                    // Keeps its collapsible blocks apart from the main chat's.
                                    1_000_000 + index * 10_000 + i,
                                );
                            }
                            if !msg.content.trim().is_empty() {
                                if has_activity {
                                    ui.add_space(6.0);
                                }
                                render_formatted_markdown(ui, &msg.content, p);
                            }
                            ui.add_space(10.0);
                        }

                        match &sub.finished {
                            None => {
                                ui.horizontal(|ui| {
                                    draw_layered_logo_loader(
                                        ui,
                                        &self.last_layer_tex,
                                        &self.middle_layer_tex,
                                        &self.top_layer_tex,
                                        true,
                                        22.0,
                                    );
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{} · {}",
                                            sub.activity,
                                            format_elapsed(sub.elapsed())
                                        ))
                                        .size(12.5)
                                        .color(p.dim),
                                    );
                                });
                                ui.ctx().request_repaint_after(Duration::from_millis(500));
                            }
                            Some((ok, _, _)) => {
                                let (mark, color, text) = if *ok {
                                    (
                                        "✔",
                                        p.success,
                                        "Finished and reported back to the main agent",
                                    )
                                } else {
                                    ("✖", p.danger, "Stopped without finishing")
                                };
                                activity_row(ui, mark, color, text, p);
                            }
                        }
                        ui.add_space(24.0);
                    });
                });
            });
        if let Some(id) = stop {
            crate::agent::subagent::cancel(&id);
        }
    }

    /// Right panel → Agents: the main agent and each sub-agent at a glance.
    pub(super) fn draw_agents_tab(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let mut view = None;
        let mut split = None;
        let mut stop = None;
        egui::ScrollArea::vertical()
            .id_salt("agents_tab")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let main_state = if self.is_processing {
                    format!(
                        "Working · {}",
                        self.thinking_start_time
                            .map(|t| format_elapsed(t.elapsed()))
                            .unwrap_or_default()
                    )
                } else {
                    "Idle".to_string()
                };
                let main = agent_card(
                    ui,
                    "Main agent",
                    &main_state,
                    if self.is_processing {
                        p.accent
                    } else {
                        p.faint
                    },
                    &style::clean_label(&self.status_text),
                    None,
                    self.chat_view == ChatView::Main && !self.split_view,
                    p,
                );
                if main.clicked() {
                    view = Some(ChatView::Main);
                    self.scroll_to_bottom = true;
                }
                ui.add_space(10.0);

                if self.subagents.is_empty() {
                    empty_hint(
                        ui,
                        "No sub-agents yet",
                        "Ask for agents (\"use sub-agents to …\") and the main agent can hand \
                         parts of the job to helpers that work at the same time.",
                        p,
                    );
                    return;
                }
                style::section_label(ui, &format!("Sub-agents · {}", self.subagents.len()), p);
                ui.add_space(4.0);
                for sub in self.subagents.iter().rev() {
                    let (state, color) = state_label(sub, p);
                    let detail = match &sub.finished {
                        Some((_, summary, _)) => summary.clone(),
                        None => sub.activity.clone(),
                    };
                    let selected = self.chat_view == ChatView::Agent(sub.id.clone());
                    let card = agent_card(
                        ui,
                        &sub.name,
                        &state,
                        color,
                        &detail,
                        Some(&sub.task),
                        selected,
                        p,
                    );
                    if card.clicked() {
                        view = Some(ChatView::Agent(sub.id.clone()));
                    }
                    ui.horizontal(|ui| {
                        ui.add_space(10.0);
                        ui.spacing_mut().item_spacing.x = 0.0;
                        if style::link_button(ui, "Open", p).clicked() {
                            view = Some(ChatView::Agent(sub.id.clone()));
                        }
                        if style::link_button(ui, "Side by side", p).clicked() {
                            split = Some(sub.id.clone());
                        }
                        if sub.running()
                            && style::link_button(ui, "Stop", p)
                                .on_hover_text("Stop this sub-agent")
                                .clicked()
                        {
                            stop = Some(sub.id.clone());
                        }
                    });
                    ui.add_space(8.0);
                }
                if self.subagents.iter().any(SubAgentView::running) {
                    ui.ctx().request_repaint_after(Duration::from_millis(500));
                }
            });
        if let Some(v) = view {
            self.chat_view = v;
            self.split_view = false;
        }
        if let Some(id) = split {
            self.chat_view = ChatView::Agent(id);
            self.split_view = true;
        }
        if let Some(id) = stop {
            crate::agent::subagent::cancel(&id);
        }
    }
}

fn state_label(sub: &SubAgentView, p: &Palette) -> (String, egui::Color32) {
    let elapsed = format_elapsed(sub.elapsed());
    match &sub.finished {
        None => (format!("Running · {elapsed}"), p.accent),
        Some((true, _, _)) => (format!("Done · {elapsed}"), p.success),
        Some((false, _, _)) => (format!("Stopped · {elapsed}"), p.danger),
    }
}

/// A pill in the agent switcher: a status dot and the agent's name.
fn agent_chip(
    ui: &mut egui::Ui,
    label: &str,
    running: bool,
    done: Option<bool>,
    active: bool,
    p: &Palette,
) -> egui::Response {
    let text = crate::util::truncate_chars(label, 24);
    let galley = ui
        .painter()
        .layout_no_wrap(text, egui::FontId::proportional(12.5), p.text);
    let size = egui::vec2(galley.size().x + 32.0, 26.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let painter = ui.painter();
    if active {
        painter.rect_filled(rect, 13.0, p.hover);
    } else if resp.hovered() {
        painter.rect_filled(rect, 13.0, p.surface);
    }
    let dot_color = match (running, done) {
        (true, _) => {
            let t = ui.input(|i| i.time) as f32;
            ui.ctx().request_repaint_after(Duration::from_millis(60));
            p.accent
                .gamma_multiply(0.55 + 0.45 * (t * std::f32::consts::TAU / 1.4).sin())
        }
        (false, Some(true)) => p.success,
        (false, Some(false)) => p.danger,
        (false, None) => p.faint,
    };
    painter.circle_filled(
        egui::pos2(rect.left() + 13.0, rect.center().y),
        3.5,
        dot_color,
    );
    let color = if active { p.text } else { p.dim };
    painter.galley_with_override_text_color(
        egui::pos2(rect.left() + 22.0, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
    resp
}

/// A clickable summary card in the Agents tab.
#[allow(clippy::too_many_arguments)]
fn agent_card(
    ui: &mut egui::Ui,
    title: &str,
    state: &str,
    state_color: egui::Color32,
    detail: &str,
    task: Option<&str>,
    selected: bool,
    p: &Palette,
) -> egui::Response {
    let id = ui.next_auto_id().with("agent_card");
    let hovered = ui.ctx().read_response(id).is_some_and(|r| r.hovered());
    let frame = egui::Frame::none()
        .fill(if selected || hovered {
            p.hover
        } else {
            p.surface
        })
        .stroke(egui::Stroke::new(
            1.0_f32,
            if hovered { p.faint } else { p.border },
        ))
        .rounding(8.0)
        .inner_margin(egui::Margin::symmetric(12.0, 10.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(title)
                        .font(style::strong(13.0))
                        .color(p.text),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(state).size(11.5).color(state_color));
                });
            });
            if let Some(task) = task {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(crate::util::truncate_chars(task, 160))
                            .size(12.0)
                            .color(p.dim),
                    )
                    .wrap(),
                );
            }
            if !detail.trim().is_empty() {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(crate::util::truncate_chars(detail.trim(), 200))
                            .size(11.5)
                            .color(p.faint),
                    )
                    .wrap(),
                );
            }
        });
    ui.interact(frame.response.rect, id, egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subagent_transcript_follows_its_events() {
        let mut sub = SubAgentView::new("agent-1".into(), "tests".into(), "write tests".into());
        sub.apply(&AgentEvent::Status("Thinking (step 1)...".into()), None);
        sub.apply(&AgentEvent::Content("Looking at the code.".into()), None);
        sub.apply(
            &AgentEvent::ToolStart {
                name: "read_file".into(),
                id: "c1".into(),
            },
            None,
        );
        assert_eq!(
            sub.messages.len(),
            2,
            "a tool after text starts a new bubble"
        );
        assert_eq!(sub.current_tool().map(|t| t.0), Some("read_file"));
        sub.apply(
            &AgentEvent::ToolResult {
                id: "c1".into(),
                name: "read_file".into(),
                result: "ok".into(),
                success: true,
            },
            None,
        );
        assert!(sub.current_tool().is_none());
        assert!(
            sub.messages[1]
                .tool_logs
                .iter()
                .any(|l| l.starts_with("TOOL_RESULT|"))
        );
        assert_eq!(sub.activity, "Running read file");
        sub.finished = Some((true, "done".into(), std::time::Instant::now()));
        assert!(!sub.running());
    }

    #[test]
    fn elapsed_is_short() {
        assert_eq!(format_elapsed(Duration::from_secs(42)), "42s");
        assert_eq!(format_elapsed(Duration::from_secs(185)), "3m 05s");
    }
}
