// ===========================================================================
// Chats drawer — saved chats of this folder, grouped by day, and the
// workspace. Opens beside the rail (Ctrl+B).
// ===========================================================================

use super::*;
use crate::style::Icon;

impl GuiApp {
    pub(super) fn draw_left_sidebar(&mut self, ctx: &egui::Context, p: &Palette) {
        let target = if self.left_sidebar_open { 1.0 } else { 0.0 };
        let raw = ctx.animate_value_with_time(egui::Id::new("left_sidebar_anim"), target, 0.18);
        let t = 1.0 - (1.0 - raw.clamp(0.0, 1.0)).powi(3);
        let animating = (raw - target).abs() > 0.005;
        if t <= 0.005 {
            return;
        }
        let max = (ctx.screen_rect().width() * 0.4).max(240.0);
        let mut panel = egui::SidePanel::left("left_sidebar")
            .show_separator_line(true)
            .frame(
                egui::Frame::none()
                    .fill(p.panel)
                    .inner_margin(egui::Margin::symmetric(12.0, 12.0)),
            )
            .resizable(!animating && self.left_sidebar_open)
            .width_range(220.0..=max);
        panel = if animating {
            panel.exact_width(self.left_sidebar_width.clamp(220.0, max) * t)
        } else {
            panel.default_width(self.left_sidebar_width.clamp(220.0, max))
        };

        let response = panel.show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Chats")
                        .font(style::strong(15.0))
                        .color(p.text),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let new_chat = style::icon_button(ui, Icon::Plus, 28.0, false, p)
                        .on_hover_text("New chat  (Ctrl+N)");
                    if new_chat.clicked() && !self.is_processing {
                        self.new_chat();
                    }
                });
            });
            ui.add_space(8.0);
            style::search_field(ui, &mut self.shell.session_filter, "Search chats", p);
            ui.add_space(10.0);

            // Workspace at the bottom; the list takes the rest.
            let footer_h = 118.0;
            let list_h = (ui.available_height() - footer_h).max(80.0);
            let mut load = None;
            let mut delete = None;
            egui::ScrollArea::vertical()
                .id_salt("left_sidebar_scroll")
                .max_height(list_h)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let filter = self.shell.session_filter.to_lowercase();
                    // The chat in progress.
                    if filter.is_empty() {
                        let count = self
                            .active_messages_backup
                            .as_ref()
                            .map_or(self.messages.len(), |m| m.len());
                        let title = if self.current_session_id.is_none() {
                            self.chat_title()
                        } else {
                            "Current chat".to_string()
                        };
                        let row = session_row(
                            ui,
                            &title,
                            &format!("now · {count} messages"),
                            self.current_session_id.is_none(),
                            false,
                            false,
                            p,
                        );
                        if row.response.clicked() {
                            if let Some(active) = self.active_messages_backup.take() {
                                self.messages = active;
                            }
                            self.current_session_id = None;
                        }
                    }

                    let mut group = "";
                    let mut armed_next = self.pending_delete_session.clone();
                    let mut any = false;
                    for summary in &self.history_sessions {
                        let when = format_session_time(&summary.updated_at).unwrap_or_default();
                        let title = if summary.title.is_empty() {
                            format!("Chat {}", &summary.id[..6.min(summary.id.len())])
                        } else {
                            summary.title.clone()
                        };
                        if !filter.is_empty()
                            && !title.to_lowercase().contains(&filter)
                            && !when.to_lowercase().contains(&filter)
                            && !summary.model.to_lowercase().contains(&filter)
                        {
                            continue;
                        }
                        any = true;
                        let this_group = day_group(&summary.updated_at);
                        if this_group != group {
                            group = this_group;
                            ui.add_space(10.0);
                            style::section_label(ui, group, p);
                            ui.add_space(2.0);
                        }
                        let time = when.rsplit(", ").next().unwrap_or("").to_string();
                        let meta = format!(
                            "{time} · {} messages · {}",
                            summary.message_count, summary.model
                        );
                        let armed =
                            self.pending_delete_session.as_deref() == Some(summary.id.as_str());
                        let selected = self.current_session_id.as_deref() == Some(&summary.id);
                        let row = session_row(ui, &title, &meta, selected, true, armed, p);
                        if row.delete_clicked {
                            if armed {
                                delete = Some(summary.id.clone());
                                armed_next = None;
                            } else {
                                armed_next = Some(summary.id.clone());
                            }
                        } else if row.response.clicked() {
                            load = Some(summary.id.clone());
                            armed_next = None;
                        } else if armed && !row.hovered {
                            armed_next = None;
                        }
                    }
                    self.pending_delete_session = armed_next;
                    if !any {
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new(if filter.is_empty() {
                                "Saved chats for this folder appear here."
                            } else {
                                "No chat matches."
                            })
                            .size(12.0)
                            .color(p.faint),
                        );
                    }
                });
            if let Some(id) = load {
                self.load_session(&id, ui.ctx());
            }
            if let Some(id) = delete {
                self.delete_session(&id);
            }

            // Workspace.
            ui.add_space(8.0);
            let r = ui.available_rect_before_wrap();
            ui.painter()
                .hline(r.x_range(), r.top(), egui::Stroke::new(1.0_f32, p.border));
            ui.add_space(10.0);
            style::section_label(ui, "Workspace", p);
            ui.add_space(4.0);
            self.draw_workspace_picker(ui, ctx, p);
        });
        if !animating && self.left_sidebar_open {
            self.left_sidebar_width = response.response.rect.width();
        }
    }

    /// Open saved chat `id` (switching to its folder when it has one).
    pub(super) fn load_session(&mut self, id: &str, ctx: &egui::Context) {
        let Some(loaded) =
            crate::agent::session::Session::load_by_id(&self.settings.project_root, id)
        else {
            self.toast("That chat could not be opened.", shell::ToastKind::Error);
            return;
        };
        if let Some(saved_root) = &loaded.project_root {
            let saved = std::path::PathBuf::from(saved_root);
            if saved.is_dir() && saved != self.settings.project_root {
                self.load_project_folder(&saved, ctx.clone());
            }
        }
        if self.current_session_id.is_none() {
            self.active_messages_backup = Some(self.messages.clone());
        }
        self.current_session_id = Some(id.to_string());
        self.messages = loaded
            .messages
            .iter()
            .filter_map(|m| {
                let role = match m.role {
                    crate::ai::types::Role::User => "user",
                    crate::ai::types::Role::Assistant => "assistant",
                    crate::ai::types::Role::System | crate::ai::types::Role::Tool => return None,
                };
                let mut content = m.text();
                // The agent appends a path note to attachments; the bubble shows the prompt only.
                if let Some(cut) = content.find("\n\n[Attached file: `") {
                    content.truncate(cut);
                }
                if content.trim().is_empty() {
                    return None;
                }
                Some(GuiMessage {
                    role: role.to_string(),
                    content,
                    thinking: None,
                    is_thinking_done: true,
                    tool_logs: Vec::new(),
                    attachments: Vec::new(),
                })
            })
            .collect();
        self.session_written_files.clear();
        self.file_diff_stats.clear();
        self.status_text = "Opened a saved chat".to_string();
        self.scroll_to_bottom = true;
    }

    fn delete_session(&mut self, id: &str) {
        let file = self
            .settings
            .project_root
            .join(".norvexum")
            .join("sessions")
            .join(format!("{id}.json"));
        if std::fs::remove_file(&file).is_err() {
            self.toast("Couldn't delete that chat.", shell::ToastKind::Error);
            return;
        }
        if self.current_session_id.as_deref() == Some(id) {
            if let Some(active) = self.active_messages_backup.take() {
                self.messages = active;
            }
            self.current_session_id = None;
            self.session_written_files.clear();
            self.file_diff_stats.clear();
        }
        self.history_sessions = crate::agent::session::Session::list(&self.settings.project_root);
        self.toast("Chat deleted", shell::ToastKind::Info);
    }
}

/// "Today", "Yesterday", "This week" or "Earlier" for an RFC 3339 time.
fn day_group(rfc3339: &str) -> &'static str {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(rfc3339) else {
        return "Earlier";
    };
    let days = (chrono::Local::now().date_naive() - t.with_timezone(&chrono::Local).date_naive())
        .num_days();
    match days {
        0 => "Today",
        1 => "Yesterday",
        2..=6 => "This week",
        _ => "Earlier",
    }
}
