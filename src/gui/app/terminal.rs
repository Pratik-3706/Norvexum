// ===========================================================================
// Terminal panel — every command any agent runs, with its live output.
//
// Reads the shared command feed (tools::shell::command_feed): the main agent
// and every sub-agent, foreground and background. Running commands show
// their latest output as it arrives and can be stopped from here.
// ===========================================================================

use super::*;
use crate::style::Icon;
use crate::tools::shell::{self, CommandRun};
use shell_ui::ToastKind;

use super::shell as shell_ui;

/// Output shown per command (the tail of its log).
const TAIL_BYTES: u64 = 6_000;

impl GuiApp {
    pub(super) fn draw_terminal(&mut self, ctx: &egui::Context, p: &Palette) {
        let feed = shell::command_feed();
        let running: Vec<&CommandRun> = feed.iter().filter(|r| r.ended.is_none()).collect();
        let mut agents: Vec<String> = feed.iter().map(|r| r.agent.clone()).collect();
        agents.sort();
        agents.dedup();

        egui::TopBottomPanel::bottom("terminal_panel")
            .frame(
                egui::Frame::none()
                    .fill(p.panel)
                    .inner_margin(egui::Margin::symmetric(14.0, 8.0)),
            )
            .resizable(true)
            .default_height(240.0)
            .height_range(110.0..=(ctx.screen_rect().height() * 0.7).max(200.0))
            .show(ctx, |ui| {
                let top = ui.max_rect().top() - 8.0;
                ui.painter().hline(
                    ui.clip_rect().x_range(),
                    top,
                    egui::Stroke::new(1.0_f32, p.border),
                );

                // Header: title, counts, filters, actions.
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let (r, _) =
                        ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                    style::paint_icon(ui.painter(), r, Icon::Terminal, p.dim);
                    ui.label(
                        egui::RichText::new("Terminal")
                            .font(style::strong(13.0))
                            .color(p.text),
                    );
                    if running.is_empty() {
                        style::pill(ui, &format!("{} commands", feed.len()), p.faint);
                    } else {
                        style::pill(ui, &format!("{} running", running.len()), p.accent);
                    }
                    ui.add_space(6.0);
                    if style::chip(ui, "Running", self.shell.terminal_running_only, p).clicked() {
                        self.shell.terminal_running_only = !self.shell.terminal_running_only;
                    }
                    if agents.len() > 1 {
                        if style::chip(ui, "All agents", self.shell.terminal_agent.is_none(), p)
                            .clicked()
                        {
                            self.shell.terminal_agent = None;
                        }
                        for agent in &agents {
                            let on = self.shell.terminal_agent.as_deref() == Some(agent.as_str());
                            if style::chip(ui, agent, on, p).clicked() {
                                self.shell.terminal_agent = (!on).then(|| agent.clone());
                            }
                        }
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if style::close_button(ui, p)
                            .on_hover_text("Hide the terminal  (Ctrl+J)")
                            .clicked()
                        {
                            self.show_terminal_panel = false;
                        }
                        if style::link_button(ui, "Clear", p)
                            .on_hover_text("Forget finished commands")
                            .clicked()
                        {
                            shell::clear_command_feed();
                            self.shell.terminal_open.clear();
                        }
                        if !running.is_empty()
                            && style::link_button(ui, "Stop all", p)
                                .on_hover_text("Stop every running command")
                                .clicked()
                        {
                            for run in &running {
                                shell::stop_command(run.id);
                            }
                            self.toast(
                                format!("Stopped {} commands", running.len()),
                                ToastKind::Info,
                            );
                        }
                    });
                });
                ui.add_space(6.0);

                let shown: Vec<&CommandRun> = feed
                    .iter()
                    .filter(|r| !self.shell.terminal_running_only || r.ended.is_none())
                    .filter(|r| {
                        self.shell
                            .terminal_agent
                            .as_deref()
                            .is_none_or(|a| a == r.agent)
                    })
                    .collect();

                egui::ScrollArea::vertical()
                    .id_salt("terminal_scroll")
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if shown.is_empty() {
                            ui.add_space(10.0);
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    egui::RichText::new(if feed.is_empty() {
                                        "Commands the agents run show up here, with live output."
                                    } else {
                                        "Nothing matches the filter."
                                    })
                                    .size(12.5)
                                    .color(p.faint),
                                );
                            });
                            return;
                        }
                        for run in shown {
                            self.command_row(ui, run, p);
                            ui.add_space(4.0);
                        }
                    });
                if !running.is_empty() {
                    ui.ctx().request_repaint_after(Duration::from_millis(400));
                }
            });
    }

    /// One command: a summary line, and its output when open or running.
    fn command_row(&mut self, ui: &mut egui::Ui, run: &CommandRun, p: &Palette) {
        let live = run.ended.is_none();
        let open = live || self.shell.terminal_open.contains(&run.id);
        let agent_color = style::name_color(&run.agent, p);

        egui::Frame::none()
            .fill(p.code_bg)
            .stroke(egui::Stroke::new(
                1.0_f32,
                if live {
                    p.accent.gamma_multiply(0.35)
                } else {
                    p.border
                },
            ))
            .rounding(8.0)
            .inner_margin(egui::Margin::symmetric(10.0, 6.0))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let header = ui
                    .horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        // Status mark.
                        let (r, _) =
                            ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
                        match run.ended {
                            None => style::paint_spinner(ui, r, p.accent),
                            Some((Some(0), _)) => {
                                style::paint_icon(ui.painter(), r, Icon::Check, p.success)
                            }
                            Some(_) => style::paint_icon(ui.painter(), r, Icon::Cross, p.danger),
                        }
                        style::pill(ui, &run.agent, agent_color);
                        let elapsed = match run.ended {
                            Some((_, took)) => took,
                            None => run.started.elapsed(),
                        };
                        let meta = {
                            let mut parts = vec![super::agents::format_elapsed(elapsed)];
                            match run.ended {
                                Some((Some(code), _)) if code != 0 => {
                                    parts.push(format!("exit {code}"))
                                }
                                Some((None, _)) => parts.push("stopped".into()),
                                _ => {}
                            }
                            if run.background {
                                parts.push("background".into());
                            }
                            if let Some(pid) = run.pid {
                                parts.push(format!("pid {pid}"));
                            }
                            parts.join(" · ")
                        };
                        let meta_w = 230.0;
                        let cmd_w = (ui.available_width() - meta_w).max(60.0);
                        let cmd = style::one_line(
                            ui,
                            &format!("$ {}", run.command),
                            egui::FontId::monospace(12.0),
                            p.text,
                            cmd_w,
                        );
                        ui.label(cmd).on_hover_text(&run.command);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            if live {
                                if style::icon_link(ui, Icon::Stop, "Stop", p)
                                    .on_hover_text("Stop this command and what it started")
                                    .clicked()
                                {
                                    shell::stop_command(run.id);
                                    self.toast(
                                        format!(
                                            "Stopped {}",
                                            crate::util::truncate_chars(&run.command, 40)
                                        ),
                                        ToastKind::Info,
                                    );
                                }
                            } else {
                                let chevron =
                                    style::icon_link(ui, Icon::Chevron, "", p).on_hover_text(
                                        if open { "Hide output" } else { "Show output" },
                                    );
                                if chevron.clicked() {
                                    if open {
                                        self.shell.terminal_open.remove(&run.id);
                                    } else {
                                        self.shell.terminal_open.insert(run.id);
                                    }
                                }
                            }
                            if style::icon_link(ui, Icon::Copy, "", p)
                                .on_hover_text("Copy the command")
                                .clicked()
                            {
                                ui.ctx().copy_text(run.command.clone());
                            }
                            ui.label(egui::RichText::new(meta).size(11.0).color(p.faint));
                        });
                    })
                    .response;
                if !live && header.interact(egui::Sense::click()).clicked() {
                    if open {
                        self.shell.terminal_open.remove(&run.id);
                    } else {
                        self.shell.terminal_open.insert(run.id);
                    }
                }

                if open {
                    let tail = shell::read_log_tail(&run.log_path, TAIL_BYTES);
                    let text = tail.trim_end();
                    ui.add_space(4.0);
                    if text.is_empty() {
                        ui.label(
                            egui::RichText::new(if live {
                                "waiting for output…"
                            } else {
                                "(no output)"
                            })
                            .font(egui::FontId::monospace(11.5))
                            .color(p.faint),
                        );
                    } else {
                        egui::ScrollArea::vertical()
                            .id_salt(("command_output", run.id))
                            .max_height(if live { 140.0 } else { 220.0 })
                            .stick_to_bottom(true)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(text)
                                            .font(egui::FontId::monospace(11.5))
                                            .color(p.dim),
                                    )
                                    .wrap(),
                                );
                            });
                    }
                }
            });
    }
}
