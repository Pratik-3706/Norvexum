// ===========================================================================
// The conversation — the user's prompts as bubbles, the agent's replies with
// their activity cards, hover actions and the live "working" row.
// ===========================================================================

use super::*;
use crate::style::Icon;
use activity::{ActivityAction, ActivityContext};

enum PendingAction {
    Edit(String),
    Reroll(String),
    Undo,
    Open(String),
}

impl GuiApp {
    pub(super) fn draw_conversation(&mut self, ctx: &egui::Context, p: &Palette) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(p.bg)
                    .inner_margin(egui::Margin::symmetric(16.0, 0.0)),
            )
            .show(ctx, |ui| {
                let has_user_messages = self.messages.iter().any(|m| m.role == "user");
                if !has_user_messages && !self.is_processing {
                    self.draw_empty_state(ui, p);
                    return;
                }
                let mut pending = None;
                let mut scroll = egui::ScrollArea::vertical()
                    .id_salt("chat_scroll")
                    .auto_shrink([false, false])
                    .stick_to_bottom(true);
                if std::mem::take(&mut self.scroll_to_bottom) {
                    scroll = scroll.vertical_scroll_offset(self.chat_max_offset);
                }
                let output = scroll.show(ui, |ui| {
                    let full = ui.available_width();
                    let col = full.min(style::MAX_COLUMN);
                    let pad = ((full - col) / 2.0).max(0.0);
                    egui::Frame::none()
                        .inner_margin(egui::Margin {
                            left: pad,
                            right: pad,
                            top: 22.0,
                            bottom: 18.0,
                        })
                        .show(ui, |ui| {
                            ui.set_width(col);
                            pending = self.draw_messages(ui, col, p);
                        });
                });

                // "Latest" when scrolled away from the bottom.
                let max_offset = (output.content_size.y - output.inner_rect.height()).max(0.0);
                self.chat_max_offset = max_offset;
                if max_offset - output.state.offset.y > 160.0 {
                    let pos = egui::pos2(
                        output.inner_rect.center().x,
                        output.inner_rect.bottom() - 14.0,
                    );
                    egui::Area::new(egui::Id::new("jump_to_latest"))
                        .order(egui::Order::Foreground)
                        .pivot(egui::Align2::CENTER_BOTTOM)
                        .fixed_pos(pos)
                        .show(ui.ctx(), |ui| {
                            let resp = ui.add(
                                egui::Button::new(
                                    egui::RichText::new("↓  Latest").size(12.0).color(p.text),
                                )
                                .fill(p.surface)
                                .stroke(egui::Stroke::new(1.0_f32, p.border))
                                .rounding(14.0)
                                .min_size(egui::vec2(84.0, 28.0)),
                            );
                            if resp.clicked() {
                                self.scroll_to_bottom = true;
                            }
                        });
                }

                match pending {
                    Some(PendingAction::Edit(text)) => {
                        self.input_text = text;
                        self.focus_composer = true;
                    }
                    Some(PendingAction::Reroll(text)) => {
                        self.input_text = text;
                        self.send_message();
                    }
                    Some(PendingAction::Undo) => {
                        if crate::agent::checkpoint::has_checkpoints(&self.settings.project_root) {
                            let _ = self.user_tx.send("/undo".to_string());
                            self.toast("Undoing the last change…", shell::ToastKind::Info);
                        } else {
                            self.toast("Nothing to undo.", shell::ToastKind::Error);
                        }
                    }
                    Some(PendingAction::Open(path)) => {
                        let relative = get_relative_path(&path, &self.settings.project_root);
                        self.right_sidebar_open = true;
                        self.right_tab = RightTab::Changed;
                        self.selected_file = Some(relative.clone());
                        self.inspect_file(&relative);
                    }
                    None => {}
                }
            });
    }

    fn draw_messages(&mut self, ui: &mut egui::Ui, col: f32, p: &Palette) -> Option<PendingAction> {
        let mut pending = None;
        let last_agent = self.messages.iter().rposition(|m| m.role == "assistant");
        // A reply is split into several messages around tool calls; it gets
        // one header at its start and one action row at its end.
        let shown: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| is_message_shown(m))
            .map(|(i, _)| i)
            .collect();
        let mut continues = vec![false; self.messages.len()];
        let mut starts = vec![false; self.messages.len()];
        let mut prev_assistant = false;
        for (n, &i) in shown.iter().enumerate() {
            let assistant = self.messages[i].role == "assistant";
            starts[i] = assistant && !prev_assistant;
            if assistant {
                if let Some(&next) = shown.get(n + 1) {
                    continues[i] = self.messages[next].role == "assistant";
                }
            }
            prev_assistant = assistant;
        }
        let live_tool_started = self
            .current_tool_id
            .as_ref()
            .and_then(|id| self.shell.tool_started.get(id).copied());

        for &idx in &shown {
            let msg = &self.messages[idx];
            match msg.role.as_str() {
                "status" => {
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new(style::clean_label(&msg.content))
                                .size(11.5)
                                .color(p.faint),
                        );
                    });
                    ui.add_space(10.0);
                }
                "error" => {
                    egui::Frame::none()
                        .fill(p.danger.gamma_multiply(0.08))
                        .stroke(egui::Stroke::new(1.0_f32, p.danger.gamma_multiply(0.4)))
                        .rounding(8.0)
                        .inner_margin(egui::Margin::symmetric(12.0, 8.0))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal_wrapped(|ui| {
                                let (r, _) = ui.allocate_exact_size(
                                    egui::vec2(14.0, 14.0),
                                    egui::Sense::hover(),
                                );
                                style::paint_icon(ui.painter(), r, Icon::Cross, p.danger);
                                ui.label(
                                    egui::RichText::new(style::clean_label(&msg.content))
                                        .size(12.5)
                                        .color(p.danger),
                                );
                            });
                        });
                    ui.add_space(12.0);
                }
                "user" => {
                    let resp = ui
                        .with_layout(egui::Layout::top_down(egui::Align::RIGHT), |ui| {
                            user_bubble(ui, msg, idx, col, p);
                        })
                        .response;
                    let hovered = row_hovered(ui, resp.rect);
                    message_action_row(
                        ui,
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            if !hovered {
                                return;
                            }
                            if style::icon_link(ui, Icon::Copy, "Copy", p).clicked() {
                                ui.ctx().copy_text(msg.content.clone());
                            }
                            if style::icon_link(ui, Icon::Edit, "Edit", p)
                                .on_hover_text("Put this prompt back in the message box")
                                .clicked()
                            {
                                pending = Some(PendingAction::Edit(msg.content.clone()));
                            }
                            if style::icon_link(ui, Icon::Refresh, "Retry", p)
                                .on_hover_text("Send this prompt again")
                                .clicked()
                            {
                                pending = Some(PendingAction::Reroll(msg.content.clone()));
                            }
                        },
                    );
                    ui.add_space(10.0);
                }
                _ => {
                    let is_latest = Some(idx) == last_agent;
                    let live = is_latest && self.is_processing;
                    let resp = ui
                        .vertical(|ui| {
                            ui.set_width(col);
                            if starts[idx] {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 6.0;
                                    draw_layered_logo_loader(
                                        ui,
                                        &self.last_layer_tex,
                                        &self.middle_layer_tex,
                                        &self.top_layer_tex,
                                        live,
                                        18.0,
                                    );
                                    ui.label(
                                        egui::RichText::new("Norvexum")
                                            .font(style::strong(12.5))
                                            .color(p.dim),
                                    );
                                });
                                ui.add_space(4.0);
                            }
                            let has_activity = !msg.tool_logs.is_empty()
                                || msg.thinking.as_ref().is_some_and(|t| !t.trim().is_empty())
                                || live;
                            if has_activity {
                                let live_tool = if live {
                                    match (&self.current_tool_id, &self.current_tool_name) {
                                        (Some(id), Some(name)) => Some((
                                            id.as_str(),
                                            name.as_str(),
                                            self.current_tool_args.as_str(),
                                            live_tool_started,
                                        )),
                                        _ => None,
                                    }
                                } else {
                                    None
                                };
                                let action = activity::draw_activity(
                                    ui,
                                    &msg.tool_logs,
                                    &msg.thinking,
                                    ActivityContext {
                                        details: &self.shell.tool_details,
                                        expanded: &mut self.shell.expanded,
                                        live_tool,
                                        live,
                                        project_root: &self.settings.project_root,
                                        msg_idx: idx,
                                    },
                                    p,
                                );
                                if let Some(ActivityAction::OpenFile(path)) = action {
                                    pending = Some(PendingAction::Open(path));
                                }
                            }
                            if !msg.content.is_empty() {
                                if has_activity {
                                    ui.add_space(6.0);
                                }
                                render_formatted_markdown(ui, &msg.content, p);
                            }
                        })
                        .response;
                    if !continues[idx] {
                        let hovered = row_hovered(ui, resp.rect);
                        message_action_row(
                            ui,
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                if !hovered || live {
                                    return;
                                }
                                if style::icon_link(ui, Icon::Copy, "Copy", p)
                                    .on_hover_text("Copy this reply")
                                    .clicked()
                                {
                                    ui.ctx().copy_text(reply_text(&self.messages, &shown, idx));
                                }
                                if style::icon_link(ui, Icon::Undo, "Undo", p)
                                    .on_hover_text("Undo the agent's last file change")
                                    .clicked()
                                {
                                    pending = Some(PendingAction::Undo);
                                }
                            },
                        );
                        ui.add_space(14.0);
                    } else {
                        ui.add_space(4.0);
                    }
                }
            }
        }

        // While the agent thinks between tool calls.
        if self.is_processing && self.current_tool_name.is_none() {
            let secs = self
                .thinking_start_time
                .map(|s| s.elapsed())
                .unwrap_or_default();
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                style::spinner(ui, 14.0, p.accent);
                ui.label(
                    egui::RichText::new(format!(
                        "Thinking · {}",
                        super::agents::format_elapsed(secs)
                    ))
                    .size(12.5)
                    .color(p.dim),
                );
            });
        }
        pending
    }
}

/// Whether the pointer is over a message (or just below it, on its actions).
fn row_hovered(ui: &egui::Ui, rect: egui::Rect) -> bool {
    let mut area = rect;
    area.min.x = ui.max_rect().min.x;
    area.max.x = ui.max_rect().max.x;
    let area = area.expand2(egui::vec2(0.0, 28.0));
    ui.ctx()
        .pointer_hover_pos()
        .is_some_and(|pos| area.contains(pos))
}

/// The user's prompt, right-aligned in a tinted bubble.
fn user_bubble(ui: &mut egui::Ui, msg: &GuiMessage, idx: usize, col: f32, p: &Palette) {
    if !msg.attachments.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
            for file in &msg.attachments {
                let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("file");
                style::pill(ui, name, p.dim).on_hover_text(file.to_string_lossy());
            }
        });
        ui.add_space(4.0);
    }
    let max_w = (col * 0.8).clamp(220.0, 660.0);
    let pad = 14.0;
    let galley = ui.fonts(|f| {
        f.layout(
            msg.content.clone(),
            egui::FontId::proportional(style::BODY_SIZE),
            p.text,
            max_w - pad * 2.0,
        )
    });
    let width = (galley.size().x.ceil() + pad * 2.0 + 2.0).min(max_w);
    // Fade a new bubble in.
    let t = ui
        .ctx()
        .animate_value_with_time(egui::Id::new(("user_bubble_in", idx)), 1.0, 0.16)
        .clamp(0.0, 1.0);
    let alpha = 1.0 - (1.0 - t).powi(3);
    ui.allocate_ui_with_layout(
        egui::vec2(width, 0.0),
        egui::Layout::top_down(egui::Align::LEFT),
        |ui| {
            ui.set_max_width(width);
            egui::Frame::none()
                .fill(p.accent_soft.linear_multiply(alpha))
                .stroke(egui::Stroke::new(
                    1.0_f32,
                    p.accent.gamma_multiply(0.18 * alpha),
                ))
                .inner_margin(egui::Margin::symmetric(pad, 10.0))
                .rounding(egui::Rounding {
                    nw: 16.0,
                    ne: 16.0,
                    sw: 16.0,
                    se: 5.0,
                })
                .show(ui, |ui| {
                    if msg.content.len() > 1200 {
                        egui::ScrollArea::vertical()
                            .max_height(240.0)
                            .id_salt(("user_scroll", idx))
                            .show(ui, |ui| render_formatted_markdown(ui, &msg.content, p));
                    } else {
                        render_formatted_markdown(ui, &msg.content, p);
                    }
                });
        },
    );
}
