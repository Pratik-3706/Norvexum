// ===========================================================================
// Settings → MCP servers: see, add, switch off and remove MCP servers.
// ===========================================================================

use super::*;
use crate::tools::mcp::{self, ServerConfig, Status};
use shell::ToastKind;

impl GuiApp {
    pub(super) fn settings_mcp(&mut self, ui: &mut egui::Ui, p: &Palette) {
        mcp::start();
        Self::settings_heading(
            ui,
            "MCP servers",
            "Model Context Protocol servers add tools the agent can use (GitHub, databases, \
             docs, design tools…). You can also just ask the agent to add one.",
            p,
        );
        ui.add_space(6.0);

        let servers = mcp::list();
        let mut action: Option<(String, &str)> = None;
        if servers.is_empty() {
            egui::Frame::none()
                .fill(p.panel)
                .rounding(8.0)
                .inner_margin(egui::Margin::same(12.0))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        egui::RichText::new("No servers yet. Add one below.")
                            .size(12.5)
                            .color(p.dim),
                    );
                });
        }
        for server in &servers {
            style::card(p).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let (color, state) = match &server.status {
                        Status::Connected => (p.success, format!("{} tools", server.tools.len())),
                        Status::Connecting => (p.accent, "connecting…".to_string()),
                        Status::Failed(_) => (p.danger, "failed".to_string()),
                        Status::Disabled => (p.faint, "off".to_string()),
                    };
                    if server.status == Status::Connecting {
                        style::spinner(ui, 12.0, color);
                    } else {
                        style::dot(ui, color);
                    }
                    ui.label(
                        egui::RichText::new(&server.name)
                            .font(style::strong(13.0))
                            .color(p.text),
                    );
                    style::pill(ui, &state, color);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if style::link_button(ui, "Remove", p).clicked() {
                            action = Some((server.name.clone(), "remove"));
                        }
                        let on = server.status != Status::Disabled;
                        if style::link_button(ui, if on { "Turn off" } else { "Turn on" }, p)
                            .clicked()
                        {
                            action =
                                Some((server.name.clone(), if on { "disable" } else { "enable" }));
                        }
                        if on
                            && style::link_button(ui, "Reconnect", p)
                                .on_hover_text("Restart it and reload its tools")
                                .clicked()
                        {
                            action = Some((server.name.clone(), "reconnect"));
                        }
                    });
                });
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(server.config.summary())
                            .font(egui::FontId::monospace(11.5))
                            .color(p.dim),
                    )
                    .truncate(),
                );
                if let Status::Failed(why) = &server.status {
                    ui.label(egui::RichText::new(why).size(11.5).color(p.danger));
                }
                if !server.tools.is_empty() {
                    egui::CollapsingHeader::new(
                        egui::RichText::new(format!("Tools ({})", server.tools.len()))
                            .size(12.0)
                            .color(p.dim),
                    )
                    .id_salt(("mcp_tools", &server.name))
                    .show(ui, |ui| {
                        for tool in &server.tools {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    egui::RichText::new(&tool.name)
                                        .font(egui::FontId::monospace(11.5))
                                        .color(p.text),
                                );
                                ui.label(
                                    egui::RichText::new(crate::util::truncate_chars(
                                        tool.description.lines().next().unwrap_or(""),
                                        110,
                                    ))
                                    .size(11.5)
                                    .color(p.faint),
                                );
                            });
                        }
                    });
                }
            });
            ui.add_space(6.0);
        }
        if servers.iter().any(|s| s.status == Status::Connecting) {
            ui.ctx().request_repaint_after(Duration::from_millis(300));
        }

        if let Some((name, what)) = action {
            let result = match what {
                "remove" => mcp::remove(&name),
                "disable" => mcp::set_enabled(&name, false),
                "enable" => mcp::set_enabled(&name, true),
                _ => mcp::reconnect(&name),
            };
            match result {
                Ok(()) => self.toast(
                    format!("MCP server {name}: {what} done"),
                    ToastKind::Success,
                ),
                Err(e) => self.toast(e, ToastKind::Error),
            }
        }

        // Add a server.
        ui.add_space(10.0);
        style::section_label(ui, "Add a server", p);
        ui.add_space(4.0);
        let field =
            |ui: &mut egui::Ui, label: &str, text: &mut String, hint: &str, multiline: bool| {
                ui.label(egui::RichText::new(label).size(12.0).color(p.dim));
                let edit = if multiline {
                    egui::TextEdit::multiline(text).desired_rows(2)
                } else {
                    egui::TextEdit::singleline(text)
                };
                ui.add(
                    edit.hint_text(egui::RichText::new(hint).color(p.faint))
                        .font(egui::FontId::monospace(12.0))
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(4.0);
            };
        field(ui, "Name", &mut self.shell.mcp_name, "github", false);
        field(
            ui,
            "Command or URL",
            &mut self.shell.mcp_target,
            "npx -y @modelcontextprotocol/server-github   or   https://example.com/mcp",
            false,
        );
        field(
            ui,
            "Environment variables (KEY=value, one per line; for a URL these are HTTP headers)",
            &mut self.shell.mcp_env,
            "GITHUB_PERSONAL_ACCESS_TOKEN=…",
            true,
        );
        if let Some(err) = &self.shell.mcp_error {
            ui.label(egui::RichText::new(err).size(12.0).color(p.danger));
        }
        ui.horizontal(|ui| {
            if style::primary_button(ui, "Add server", p).clicked() {
                match self.add_mcp_from_form() {
                    Ok(name) => {
                        self.shell.mcp_error = None;
                        self.shell.mcp_name.clear();
                        self.shell.mcp_target.clear();
                        self.shell.mcp_env.clear();
                        self.toast(format!("Added MCP server {name}"), ToastKind::Success);
                    }
                    Err(e) => self.shell.mcp_error = Some(e),
                }
            }
            if style::secondary_button(ui, "Open mcp.json", p)
                .on_hover_text("Edit the config by hand (same format as other MCP clients)")
                .clicked()
            {
                let path = mcp::config_path();
                if !path.exists() {
                    if let Some(dir) = path.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    let _ = std::fs::write(&path, "{\n  \"mcpServers\": {}\n}\n");
                }
                let _ = open::that(&path);
            }
        });
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(format!(
                "Saved in {}. Servers start with Norvexum.",
                mcp::config_path().display()
            ))
            .size(11.5)
            .color(p.faint),
        );
    }

    /// Turn the add form into a server; returns its name.
    fn add_mcp_from_form(&mut self) -> Result<String, String> {
        let name = self.shell.mcp_name.trim().to_string();
        let target = self.shell.mcp_target.trim();
        if target.is_empty() {
            return Err("Give a command or a URL.".into());
        }
        let pairs: std::collections::BTreeMap<String, String> = self
            .shell
            .mcp_env
            .lines()
            .filter_map(|line| {
                let (k, v) = line.split_once('=').or_else(|| line.split_once(':'))?;
                let k = k.trim();
                (!k.is_empty()).then(|| (k.to_string(), v.trim().to_string()))
            })
            .collect();
        let config = if target.starts_with("http://") || target.starts_with("https://") {
            ServerConfig {
                url: Some(target.to_string()),
                headers: pairs,
                ..Default::default()
            }
        } else {
            let parts = shlex::split(target).ok_or("The command has unbalanced quotes.")?;
            let (command, args) = parts.split_first().ok_or("Give a command.")?;
            ServerConfig {
                command: Some(command.clone()),
                args: args.to_vec(),
                env: pairs,
                ..Default::default()
            }
        };
        mcp::add(&name, config)?;
        Ok(name)
    }
}
