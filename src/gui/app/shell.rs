// ===========================================================================
// The app shell — icon rail, chat header, command palette and toasts.
//
// The rail runs down the left edge (the logo spins while an agent works) and
// opens everything else: chats, files, agents, browser, terminal, settings.
// The header above the chat says what is going on; Ctrl+K opens a palette
// that searches every action and saved chat.
// ===========================================================================

use super::*;
use crate::style::Icon;
use std::collections::{HashMap, HashSet};

/// Presentation state of the shell and the redesigned panels.
#[derive(Default)]
pub(super) struct ShellState {
    pub palette_open: bool,
    pub palette_query: String,
    pub palette_selected: usize,
    pub toasts: Vec<Toast>,
    pub session_filter: String,
    /// Streamed arguments and start time of tool calls, by call id.
    pub tool_args: HashMap<String, String>,
    pub tool_started: HashMap<String, std::time::Instant>,
    /// Finished tool calls, by call id (for the expandable cards).
    pub tool_details: HashMap<String, ToolDetail>,
    /// Expanded tool cards and tool groups.
    pub expanded: HashSet<String>,
    /// Terminal: expanded commands, and the agent filter.
    pub terminal_open: HashSet<u64>,
    pub terminal_agent: Option<String>,
    pub terminal_running_only: bool,
    /// Index of the highlighted `/` command suggestion.
    pub slash_selected: usize,
    /// Settings → MCP: the add form.
    pub mcp_name: String,
    pub mcp_target: String,
    pub mcp_env: String,
    pub mcp_error: Option<String>,
}

#[derive(Clone)]
pub(super) struct ToolDetail {
    pub name: String,
    pub args: String,
    pub result: String,
    pub success: bool,
    pub duration: std::time::Duration,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum ToastKind {
    Info,
    Success,
    Error,
}

pub(super) struct Toast {
    text: String,
    kind: ToastKind,
    born: f64,
}

/// What a palette entry does.
#[derive(Clone)]
enum Command {
    NewChat,
    ToggleChats,
    Files,
    Agents,
    Browser,
    Terminal,
    Settings(SettingsSection),
    Theme,
    Shortcuts,
    Stop,
    Focus,
    Session(String),
}

/// Width of the icon rail.
const RAIL_WIDTH: f32 = 52.0;
/// Height of the chat header (the right panel's tabs line up with it).
pub(super) const HEADER_HEIGHT: f32 = 46.0;

impl GuiApp {
    /// Show a short notice in the corner.
    pub(super) fn toast(&mut self, text: impl Into<String>, kind: ToastKind) {
        let born = self.ctx.as_ref().map_or(0.0, |c| c.input(|i| i.time));
        self.shell.toasts.push(Toast {
            text: text.into(),
            kind,
            born,
        });
        if self.shell.toasts.len() > 4 {
            self.shell.toasts.remove(0);
        }
    }

    /// The chat's title: its first prompt, shortened.
    pub(super) fn chat_title(&self) -> String {
        self.messages
            .iter()
            .find(|m| m.role == "user")
            .map(|m| {
                let line = m
                    .content
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("");
                crate::util::truncate_chars(line.trim(), 70)
            })
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "New chat".to_string())
    }

    fn right_panel_shows(&self, tabs: &[RightTab]) -> bool {
        self.right_sidebar_open && tabs.contains(&self.right_tab)
    }

    fn toggle_right(&mut self, tab: RightTab, group: &[RightTab]) {
        if self.right_panel_shows(group) {
            self.right_sidebar_open = false;
        } else {
            self.right_sidebar_open = true;
            if !group.contains(&self.right_tab) {
                self.right_tab = tab;
            }
        }
    }

    fn run_command(&mut self, command: Command, ctx: &egui::Context) {
        match command {
            Command::NewChat => {
                if !self.is_processing {
                    self.new_chat();
                }
            }
            Command::ToggleChats => self.left_sidebar_open = !self.left_sidebar_open,
            Command::Files => {
                self.toggle_right(RightTab::Changed, &[RightTab::Changed, RightTab::All])
            }
            Command::Agents => self.toggle_right(RightTab::Agents, &[RightTab::Agents]),
            Command::Browser => self.toggle_right(RightTab::Browser, &[RightTab::Browser]),
            Command::Terminal => self.show_terminal_panel = !self.show_terminal_panel,
            Command::Settings(section) => {
                self.settings_section = section;
                self.show_settings_modal = true;
            }
            Command::Theme => self.dark_mode = !self.dark_mode,
            Command::Shortcuts => self.show_shortcuts = true,
            Command::Stop => self.stop_run(),
            Command::Focus => self.focus_composer = true,
            Command::Session(id) => self.load_session(&id, ctx),
        }
    }

    // -----------------------------------------------------------------------
    // Rail
    // -----------------------------------------------------------------------

    pub(super) fn draw_rail(&mut self, ctx: &egui::Context, p: &Palette) {
        let mut chosen = None;
        egui::SidePanel::left("rail")
            .exact_width(RAIL_WIDTH)
            .resizable(false)
            .show_separator_line(false)
            .frame(
                egui::Frame::none()
                    .fill(p.panel)
                    .inner_margin(egui::Margin::symmetric(8.0, 10.0)),
            )
            .show(ctx, |ui| {
                let rail = ui.max_rect();
                ui.painter().vline(
                    rail.right() + 8.0,
                    ui.clip_rect().y_range(),
                    egui::Stroke::new(1.0_f32, p.border),
                );
                ui.vertical_centered(|ui| {
                    ui.spacing_mut().item_spacing.y = 6.0;
                    let logo = draw_layered_logo_loader(
                        ui,
                        &self.last_layer_tex,
                        &self.middle_layer_tex,
                        &self.top_layer_tex,
                        self.is_processing || self.subagents.iter().any(|s| s.running()),
                        30.0,
                    );
                    logo.on_hover_text(if self.is_processing {
                        "Working…"
                    } else {
                        "Norvexum"
                    });
                    ui.add_space(10.0);

                    let running_agents = self.subagents.iter().filter(|s| s.running()).count();
                    let running_commands = crate::tools::shell::command_feed()
                        .iter()
                        .filter(|r| r.ended.is_none())
                        .count();
                    let items: [(Icon, &str, bool, usize, Command); 7] = [
                        (
                            Icon::Chats,
                            "Chats  (Ctrl+B)",
                            self.left_sidebar_open,
                            0,
                            Command::ToggleChats,
                        ),
                        (Icon::Plus, "New chat  (Ctrl+N)", false, 0, Command::NewChat),
                        (
                            Icon::Search,
                            "Search everything  (Ctrl+K)",
                            self.shell.palette_open,
                            0,
                            Command::Focus,
                        ),
                        (
                            Icon::Files,
                            "Changes and files  (Ctrl+E)",
                            self.right_panel_shows(&[RightTab::Changed, RightTab::All]),
                            0,
                            Command::Files,
                        ),
                        (
                            Icon::Agents,
                            "Agents",
                            self.right_panel_shows(&[RightTab::Agents]),
                            running_agents,
                            Command::Agents,
                        ),
                        (
                            Icon::Globe,
                            "Browser",
                            self.right_panel_shows(&[RightTab::Browser]),
                            0,
                            Command::Browser,
                        ),
                        (
                            Icon::Terminal,
                            "Terminal  (Ctrl+J)",
                            self.show_terminal_panel,
                            running_commands,
                            Command::Terminal,
                        ),
                    ];
                    for (i, (icon, tip, active, badge, command)) in items.into_iter().enumerate() {
                        if i == 3 {
                            ui.add_space(4.0);
                            let r = ui.available_rect_before_wrap();
                            ui.painter().hline(
                                r.center().x - 12.0..=r.center().x + 12.0,
                                r.top(),
                                egui::Stroke::new(1.0_f32, p.border),
                            );
                            ui.add_space(8.0);
                        }
                        let resp = style::icon_button(ui, icon, 36.0, active, p);
                        if badge > 0 {
                            paint_badge(ui, resp.rect, badge, p);
                        }
                        if resp.on_hover_text(tip).clicked() {
                            chosen = Some(if icon == Icon::Search {
                                self.shell.palette_open = true;
                                self.shell.palette_query.clear();
                                self.shell.palette_selected = 0;
                                Command::Focus
                            } else {
                                command
                            });
                        }
                    }
                });

                // Settings at the bottom.
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.y = 6.0;
                    if style::icon_button(ui, Icon::Gear, 36.0, self.show_settings_modal, p)
                        .on_hover_text("Settings  (Ctrl+,)")
                        .clicked()
                    {
                        chosen = Some(Command::Settings(SettingsSection::General));
                    }
                    let theme = if self.dark_mode {
                        Icon::Sun
                    } else {
                        Icon::Moon
                    };
                    if style::icon_button(ui, theme, 36.0, false, p)
                        .on_hover_text("Switch theme")
                        .clicked()
                    {
                        chosen = Some(Command::Theme);
                    }
                    let servers = crate::tools::mcp::list();
                    let failing = servers
                        .iter()
                        .any(|s| matches!(s.status, crate::tools::mcp::Status::Failed(_)));
                    let plug = style::icon_button(ui, Icon::Plug, 36.0, false, p);
                    if failing {
                        ui.painter().circle_filled(
                            plug.rect.right_top() + egui::vec2(-8.0, 8.0),
                            3.5,
                            p.danger,
                        );
                    }
                    let connected = servers
                        .iter()
                        .filter(|s| s.status == crate::tools::mcp::Status::Connected)
                        .count();
                    if plug
                        .on_hover_text(format!(
                            "MCP servers · {connected} connected of {}",
                            servers.len()
                        ))
                        .clicked()
                    {
                        chosen = Some(Command::Settings(SettingsSection::Mcp));
                    }
                });
            });
        if let Some(command) = chosen {
            self.run_command(command, ctx);
        }
    }

    // -----------------------------------------------------------------------
    // Header
    // -----------------------------------------------------------------------

    pub(super) fn draw_chat_header(&mut self, ctx: &egui::Context, p: &Palette) {
        egui::TopBottomPanel::top("chat_header")
            .exact_height(HEADER_HEIGHT)
            .show_separator_line(false)
            .frame(
                egui::Frame::none()
                    .fill(p.bg)
                    .inner_margin(egui::Margin::symmetric(18.0, 0.0)),
            )
            .show(ctx, |ui| {
                let full = ui.max_rect();
                ui.painter().hline(
                    ui.clip_rect().x_range(),
                    full.bottom(),
                    egui::Stroke::new(1.0_f32, p.border),
                );
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    // Status on the right first, so the title gets what is left.
                    let right_w = 260.0_f32.min(ui.available_width() * 0.45);
                    let title_w = (ui.available_width() - right_w).max(80.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(title_w, HEADER_HEIGHT),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            let title = self.chat_title();
                            let galley = style::one_line(
                                ui,
                                &title,
                                style::strong(14.0),
                                p.text,
                                (title_w - 140.0).max(80.0),
                            );
                            ui.label(galley).on_hover_text(&title);
                            if self.current_session_id.is_some() {
                                style::pill(ui, "saved chat", p.dim);
                            }
                            let project = self.project_name();
                            let chip = ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new(format!("in {project}"))
                                            .size(12.0)
                                            .color(p.faint),
                                    )
                                    .frame(false),
                                )
                                .on_hover_text(format!(
                                    "{}\nClick to change the workspace",
                                    self.settings.project_root.display()
                                ));
                            if chip.clicked() {
                                self.left_sidebar_open = true;
                            }
                        },
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if style::icon_button(ui, Icon::Panel, 30.0, self.right_sidebar_open, p)
                            .on_hover_text("Show or hide the side panel  (Ctrl+E)")
                            .clicked()
                        {
                            self.right_sidebar_open = !self.right_sidebar_open;
                        }
                        ui.add_space(4.0);
                        self.draw_status_pill(ui, p);
                    });
                });
            });
    }

    /// What the agent is doing, as a pill: a spinner while it works.
    fn draw_status_pill(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let label = style::clean_label(&self.status_text);
        let elapsed = self
            .thinking_start_time
            .map(|t| super::agents::format_elapsed(t.elapsed()))
            .unwrap_or_default();
        let (color, text, busy) = if self.agent_crashed {
            (
                p.danger,
                "Agent stopped — restart Norvexum".to_string(),
                false,
            )
        } else if self.approval_pending.is_some() {
            (p.warn, "Waiting for your approval".to_string(), false)
        } else if self.is_processing {
            let what = if label.is_empty() || label == "Idle" {
                "Working".to_string()
            } else {
                crate::util::truncate_chars(&label, 34)
            };
            (p.accent, format!("{what} · {elapsed}"), true)
        } else if self.status_text == "Error" {
            (p.danger, "Last run ended with an error".to_string(), false)
        } else {
            (p.success, "Ready".to_string(), false)
        };
        let font = egui::FontId::proportional(12.0);
        let galley = ui.painter().layout_no_wrap(text.clone(), font, color);
        let size = egui::vec2(galley.size().x + 34.0, 26.0);
        let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::hover());
        ui.painter()
            .rect_filled(rect, 13.0, color.gamma_multiply(0.12));
        ui.painter().rect_stroke(
            rect,
            13.0,
            egui::Stroke::new(1.0_f32, color.gamma_multiply(0.25)),
        );
        let mark = egui::Rect::from_center_size(
            egui::pos2(rect.left() + 15.0, rect.center().y),
            egui::vec2(12.0, 12.0),
        );
        if busy {
            style::paint_spinner(ui, mark, color);
        } else {
            ui.painter().circle_filled(mark.center(), 3.5, color);
        }
        ui.painter().galley(
            egui::pos2(rect.left() + 26.0, rect.center().y - galley.size().y / 2.0),
            galley,
            color,
        );
        resp.on_hover_text(format!(
            "{}\n{} · {}",
            label, self.settings.active_provider, self.settings.active_model
        ));
    }

    // -----------------------------------------------------------------------
    // Command palette
    // -----------------------------------------------------------------------

    fn palette_entries(&self) -> Vec<(String, String, Command)> {
        let mut entries: Vec<(String, String, Command)> = vec![
            ("New chat".into(), "Ctrl+N".into(), Command::NewChat),
            (
                "Show or hide chats".into(),
                "Ctrl+B".into(),
                Command::ToggleChats,
            ),
            ("Changes and files".into(), "Ctrl+E".into(), Command::Files),
            ("Agents".into(), String::new(), Command::Agents),
            ("Browser".into(), String::new(), Command::Browser),
            (
                "Terminal and processes".into(),
                "Ctrl+J".into(),
                Command::Terminal,
            ),
            (
                "Settings".into(),
                "Ctrl+,".into(),
                Command::Settings(SettingsSection::General),
            ),
            (
                "MCP servers".into(),
                "Settings".into(),
                Command::Settings(SettingsSection::Mcp),
            ),
            (
                "Models".into(),
                "Settings".into(),
                Command::Settings(SettingsSection::Models),
            ),
            (
                "API keys".into(),
                "Settings".into(),
                Command::Settings(SettingsSection::ApiKeys),
            ),
            (
                "Skills".into(),
                "Settings".into(),
                Command::Settings(SettingsSection::Skills),
            ),
            (
                "Permissions".into(),
                "Settings".into(),
                Command::Settings(SettingsSection::Permissions),
            ),
            (
                if self.dark_mode {
                    "Light theme"
                } else {
                    "Dark theme"
                }
                .into(),
                String::new(),
                Command::Theme,
            ),
            (
                "Keyboard shortcuts".into(),
                "Ctrl+/".into(),
                Command::Shortcuts,
            ),
            (
                "Focus the message box".into(),
                "Ctrl+L".into(),
                Command::Focus,
            ),
        ];
        if self.is_processing {
            entries.insert(0, ("Stop the agent".into(), "Esc".into(), Command::Stop));
        }
        for s in &self.history_sessions {
            let when = format_session_time(&s.updated_at).unwrap_or_default();
            entries.push((
                format!("Open chat · {when}"),
                format!("{} messages", s.message_count),
                Command::Session(s.id.clone()),
            ));
        }
        entries
    }

    pub(super) fn draw_command_palette(&mut self, ctx: &egui::Context, p: &Palette) {
        if !self.shell.palette_open {
            return;
        }
        let query = self.shell.palette_query.to_lowercase();
        let entries: Vec<(String, String, Command)> = self
            .palette_entries()
            .into_iter()
            .filter(|(label, hint, _)| {
                fuzzy_match(&query, &label.to_lowercase())
                    || (!query.is_empty() && hint.to_lowercase().contains(&query))
            })
            .collect();
        let count = entries.len();
        if self.shell.palette_selected >= count.max(1) {
            self.shell.palette_selected = count.saturating_sub(1);
        }

        // Keys: arrows move, Enter runs, Esc closes.
        let (up, down, enter, escape) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                i.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
                i.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
            )
        });
        if up {
            self.shell.palette_selected = self.shell.palette_selected.saturating_sub(1);
        }
        if down && self.shell.palette_selected + 1 < count {
            self.shell.palette_selected += 1;
        }
        let mut run = enter
            .then(|| {
                entries
                    .get(self.shell.palette_selected)
                    .map(|e| e.2.clone())
            })
            .flatten();
        let mut close = escape || enter;

        // Dim the app behind it; a click there closes the palette.
        let screen = ctx.screen_rect();
        egui::Area::new(egui::Id::new("palette_backdrop"))
            .order(egui::Order::Middle)
            .fixed_pos(screen.min)
            .show(ctx, |ui| {
                let resp = ui.allocate_rect(screen, egui::Sense::click());
                ui.painter()
                    .rect_filled(screen, 0.0, egui::Color32::from_black_alpha(110));
                if resp.clicked() {
                    close = true;
                }
            });

        let width = 560.0_f32.min(screen.width() - 40.0);
        egui::Area::new(egui::Id::new("command_palette"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(
                screen.center().x - width / 2.0,
                screen.top() + screen.height() * 0.14,
            ))
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(p.surface)
                    .stroke(egui::Stroke::new(1.0_f32, p.border))
                    .rounding(12.0)
                    .shadow(egui::epaint::Shadow {
                        offset: egui::vec2(0.0, 10.0),
                        blur: 30.0,
                        spread: 0.0,
                        color: egui::Color32::from_black_alpha(120),
                    })
                    .inner_margin(egui::Margin::same(10.0))
                    .show(ui, |ui| {
                        ui.set_width(width - 20.0);
                        let field = style::search_field(
                            ui,
                            &mut self.shell.palette_query,
                            "Search actions and chats…",
                            p,
                        );
                        field.request_focus();
                        if field.changed() {
                            self.shell.palette_selected = 0;
                        }
                        ui.add_space(6.0);
                        egui::ScrollArea::vertical()
                            .id_salt("palette_list")
                            .max_height(360.0)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                if entries.is_empty() {
                                    ui.add_space(8.0);
                                    ui.label(
                                        egui::RichText::new("Nothing matches.")
                                            .size(12.5)
                                            .color(p.faint),
                                    );
                                }
                                for (i, (label, hint, command)) in entries.iter().enumerate() {
                                    let selected = i == self.shell.palette_selected;
                                    let (rect, resp) = ui.allocate_exact_size(
                                        egui::vec2(ui.available_width(), 32.0),
                                        egui::Sense::click(),
                                    );
                                    if selected || resp.hovered() {
                                        ui.painter().rect_filled(
                                            rect,
                                            7.0,
                                            if selected { p.accent_soft } else { p.hover },
                                        );
                                    }
                                    if selected {
                                        ui.scroll_to_rect(rect, None);
                                    }
                                    ui.painter().text(
                                        egui::pos2(rect.left() + 12.0, rect.center().y),
                                        egui::Align2::LEFT_CENTER,
                                        label,
                                        egui::FontId::proportional(13.0),
                                        if selected { p.text } else { p.dim },
                                    );
                                    ui.painter().text(
                                        egui::pos2(rect.right() - 12.0, rect.center().y),
                                        egui::Align2::RIGHT_CENTER,
                                        hint,
                                        egui::FontId::proportional(11.5),
                                        p.faint,
                                    );
                                    if resp.clicked() {
                                        run = Some(command.clone());
                                        close = true;
                                    }
                                    if resp.hovered()
                                        && ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO)
                                    {
                                        self.shell.palette_selected = i;
                                    }
                                }
                            });
                    });
            });

        if close {
            self.shell.palette_open = false;
        }
        if let Some(command) = run {
            self.run_command(command, ctx);
        }
    }

    // -----------------------------------------------------------------------
    // Toasts
    // -----------------------------------------------------------------------

    pub(super) fn draw_toasts(&mut self, ctx: &egui::Context, p: &Palette) {
        const LIFE: f64 = 3.5;
        let now = ctx.input(|i| i.time);
        self.shell.toasts.retain(|t| now - t.born < LIFE);
        if self.shell.toasts.is_empty() {
            return;
        }
        ctx.request_repaint();
        let screen = ctx.screen_rect();
        egui::Area::new(egui::Id::new("toasts"))
            .order(egui::Order::Tooltip)
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-18.0, -120.0))
            .interactable(false)
            .show(ctx, |ui| {
                ui.set_max_width(340.0_f32.min(screen.width() - 40.0));
                for toast in &self.shell.toasts {
                    let age = now - toast.born;
                    // Slide in, fade out.
                    let fade = ((LIFE - age) / 0.4).clamp(0.0, 1.0) as f32;
                    let enter = (age / 0.18).clamp(0.0, 1.0) as f32;
                    let color = match toast.kind {
                        ToastKind::Info => p.accent,
                        ToastKind::Success => p.success,
                        ToastKind::Error => p.danger,
                    };
                    ui.add_space(6.0);
                    egui::Frame::none()
                        .fill(p.surface.gamma_multiply(fade))
                        .stroke(egui::Stroke::new(1.0_f32, p.border.gamma_multiply(fade)))
                        .rounding(9.0)
                        .outer_margin(egui::Margin {
                            left: (1.0 - enter) * 30.0,
                            ..Default::default()
                        })
                        .inner_margin(egui::Margin::symmetric(12.0, 8.0))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let icon = match toast.kind {
                                    ToastKind::Error => Icon::Cross,
                                    _ => Icon::Check,
                                };
                                let (r, _) = ui.allocate_exact_size(
                                    egui::vec2(14.0, 14.0),
                                    egui::Sense::hover(),
                                );
                                style::paint_icon(
                                    ui.painter(),
                                    r,
                                    icon,
                                    color.gamma_multiply(fade),
                                );
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&toast.text)
                                            .size(12.5)
                                            .color(p.text.gamma_multiply(fade)),
                                    )
                                    .wrap(),
                                );
                            });
                        });
                }
            });
    }
}

/// A small count in the top-right corner of a rail button.
fn paint_badge(ui: &egui::Ui, rect: egui::Rect, count: usize, p: &Palette) {
    let text = if count > 9 {
        "9+".to_string()
    } else {
        count.to_string()
    };
    let center = rect.right_top() + egui::vec2(-6.0, 6.0);
    ui.painter().circle_filled(center, 7.5, p.accent);
    ui.painter().text(
        center,
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(9.5),
        p.on_accent,
    );
}

/// Whether every character of `query` appears in `text`, in order.
pub(super) fn fuzzy_match(query: &str, text: &str) -> bool {
    let mut chars = text.chars();
    query
        .chars()
        .filter(|c| !c.is_whitespace())
        .all(|q| chars.any(|c| c == q))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_matching_follows_the_order() {
        assert!(fuzzy_match("", "anything"));
        assert!(fuzzy_match("term", "terminal and processes"));
        assert!(fuzzy_match("ncht", "new chat"));
        assert!(!fuzzy_match("tn", "new chat"));
    }
}
