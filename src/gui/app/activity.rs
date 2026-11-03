// ===========================================================================
// A reply's activity — reasoning, tool calls and file changes, in order.
//
// Each finished tool call is a card: what it did and to what, how long it
// took, and (click) its input and output. File changes show their +/- lines
// and open the file. Long runs of calls fold into one summary card. The call
// in progress shows a spinner and a running timer.
// ===========================================================================

use super::*;
use crate::style::Icon;
use shell::ToolDetail;
use std::collections::{HashMap, HashSet};

/// What the activity list asks the app to do after drawing.
pub(super) enum ActivityAction {
    OpenFile(String),
}

/// One thing in a reply's activity.
enum Item<'a> {
    Reasoning {
        title: String,
        text: String,
    },
    Tool {
        id: &'a str,
        name: &'a str,
        success: bool,
        summary: &'a str,
    },
    File {
        path: &'a str,
        added: usize,
        deleted: usize,
    },
    Agent {
        state: &'a str,
        name: &'a str,
        text: &'a str,
    },
    Message {
        from: &'a str,
        to: &'a str,
        text: &'a str,
    },
    Warning(&'a str),
}

impl Item<'_> {
    /// Tool calls and file changes; runs of them can fold into one card.
    fn is_step(&self) -> bool {
        matches!(self, Item::Tool { .. } | Item::File { .. })
    }
}

/// Runs of at least this many steps fold into a summary card.
const FOLD_AT: usize = 4;

/// Split a message's activity log into items.
fn parse<'a>(
    logs: &'a [String],
    fallback_thinking: &Option<String>,
) -> (Vec<Item<'a>>, Option<(String, String)>) {
    let mut items = Vec::new();
    let has_deltas = logs.iter().any(|l| l.starts_with("THINKING_DELTA|"));
    if !has_deltas {
        if let Some(t) = fallback_thinking.as_ref().filter(|t| !t.trim().is_empty()) {
            items.push(Item::Reasoning {
                title: "Reasoning".into(),
                text: t.clone(),
            });
        }
    }
    let mut title: Option<String> = None;
    let mut thinking = String::new();
    let flush = |items: &mut Vec<Item>, title: &mut Option<String>, thinking: &mut String| {
        if !thinking.trim().is_empty() {
            items.push(Item::Reasoning {
                title: title.take().unwrap_or_else(|| "Reasoning".into()),
                text: std::mem::take(thinking),
            });
        }
        thinking.clear();
        *title = None;
    };
    for log in logs {
        if let Some(delta) = log.strip_prefix("THINKING_DELTA|") {
            thinking.push_str(delta);
        } else if log.starts_with("ℹ️ Thinking (loop ") || log.starts_with("ℹ️ Thinking (step ")
        {
            flush(&mut items, &mut title, &mut thinking);
            title = Some(
                log.trim_start_matches("ℹ️ ")
                    .trim_end_matches("...")
                    .to_string(),
            );
        } else if let Some(payload) = log.strip_prefix("TOOL_RESULT|") {
            flush(&mut items, &mut title, &mut thinking);
            let mut parts = payload.splitn(4, '|');
            let (id, name, status, summary) = (
                parts.next().unwrap_or(""),
                parts.next().unwrap_or(""),
                parts.next().unwrap_or(""),
                parts.next().unwrap_or(""),
            );
            items.push(Item::Tool {
                id,
                name,
                success: status.contains("Success") || status.contains('✅'),
                summary,
            });
        } else if let Some(payload) = log.strip_prefix("FILE_CHANGE|") {
            flush(&mut items, &mut title, &mut thinking);
            // <icon>|<path>|+<added>|-<deleted>
            let parts: Vec<&str> = payload.split('|').collect();
            if parts.len() >= 4 {
                items.push(Item::File {
                    path: parts[1],
                    added: parts[2].trim_start_matches('+').parse().unwrap_or(0),
                    deleted: parts[3].trim_start_matches('-').parse().unwrap_or(0),
                });
            }
        } else if let Some(payload) = log.strip_prefix("AGENT_EVT|") {
            let mut parts = payload.splitn(3, '|');
            items.push(Item::Agent {
                state: parts.next().unwrap_or(""),
                name: parts.next().unwrap_or(""),
                text: parts.next().unwrap_or(""),
            });
        } else if let Some(payload) = log.strip_prefix("AGENT_MSG|") {
            let mut parts = payload.splitn(3, '|');
            items.push(Item::Message {
                from: parts.next().unwrap_or(""),
                to: parts.next().unwrap_or(""),
                text: parts.next().unwrap_or(""),
            });
        } else if let Some(warning) = log.strip_prefix("⚠️ ") {
            // Errors are already appended to the message body.
            if !warning.starts_with("Error:") {
                items.push(Item::Warning(warning));
            }
        }
    }
    // Reasoning not yet followed by a tool call (live while the agent thinks).
    let trailing = (!thinking.trim().is_empty())
        .then(|| (title.unwrap_or_else(|| "Reasoning".into()), thinking));
    (items, trailing)
}

/// Past and present tense of what a tool does.
pub(super) fn tool_verbs(name: &str) -> (String, String) {
    let (past, now) = match name {
        "read_file" | "cat" | "view_file" | "read_multiple_files" => ("Read", "Reading"),
        "write_file" | "write_to_file" => ("Wrote", "Writing"),
        "edit_file" | "replace_file_content" | "multi_replace_file_content" | "multi_edit" => {
            ("Edited", "Editing")
        }
        "run_command" | "shell" | "shell_command" | "execute" => ("Ran", "Running"),
        "manage_processes" => ("Checked processes", "Checking processes"),
        "web_search" | "search_web" => ("Searched the web", "Searching the web"),
        "web_fetch" => ("Fetched", "Fetching"),
        "browser_interact" => ("Browser", "Using the browser"),
        "list_dir" | "list_directory" => ("Listed", "Listing"),
        "grep" | "grep_search" | "search_files" => ("Searched", "Searching"),
        "glob" | "find_files" => ("Found files", "Finding files"),
        "spawn_agent" => ("Started sub-agent", "Starting sub-agent"),
        "message_agent" => ("Messaged", "Messaging"),
        "wait_for_agents" => ("Waited for sub-agents", "Waiting for sub-agents"),
        "read_skill" => ("Read skill", "Reading skill"),
        "check_package" => ("Checked package", "Checking package"),
        "mcp_manage" => ("MCP servers", "Managing MCP servers"),
        _ => {
            if let Some(rest) = name.strip_prefix(crate::tools::mcp::PREFIX) {
                let (server, tool) = rest.split_once("__").unwrap_or((rest, ""));
                let label = format!("{server} · {}", tool.replace('_', " "));
                return (label.clone(), label);
            }
            let label = style::sentence_case(&name.replace('_', " "));
            return (label.clone(), label);
        }
    };
    (past.to_string(), now.to_string())
}

/// The thing a call acted on: a path, command, URL, query… from its summary
/// or, failing that, its first short string argument.
fn tool_target(summary: &str, args: &str) -> String {
    if !summary.trim().is_empty() {
        return summary.trim().to_string();
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(args) else {
        return String::new();
    };
    const KEYS: [&str; 10] = [
        "path", "command", "url", "query", "pattern", "name", "action", "to", "task", "text",
    ];
    for key in KEYS {
        if let Some(s) = value[key].as_str().filter(|s| !s.trim().is_empty()) {
            return crate::util::truncate_chars(s.lines().next().unwrap_or(s), 90);
        }
    }
    String::new()
}

fn format_duration(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        super::agents::format_elapsed(d)
    }
}

/// Pretty-print JSON arguments (or return them as they are).
fn pretty_json(args: &str) -> String {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| args.to_string())
}

pub(super) struct ActivityContext<'a> {
    pub details: &'a HashMap<String, ToolDetail>,
    pub expanded: &'a mut HashSet<String>,
    /// The call in progress: (id, name, streamed args, started).
    pub live_tool: Option<(&'a str, &'a str, &'a str, Option<std::time::Instant>)>,
    /// The reply is still being written.
    pub live: bool,
    pub project_root: &'a std::path::Path,
    pub msg_idx: usize,
}

/// Draw a message's activity. Returns what the user asked for, if anything.
pub(super) fn draw_activity(
    ui: &mut egui::Ui,
    logs: &[String],
    fallback_thinking: &Option<String>,
    cx: ActivityContext,
    p: &Palette,
) -> Option<ActivityAction> {
    let (items, trailing) = parse(logs, fallback_thinking);
    let mut action = None;
    let ActivityContext {
        details,
        expanded,
        live_tool,
        live,
        project_root,
        msg_idx,
    } = cx;

    let mut i = 0;
    let mut block = 0;
    while i < items.len() {
        // A run of steps.
        if items[i].is_step() {
            let end = items[i..]
                .iter()
                .position(|it| !it.is_step())
                .map_or(items.len(), |n| i + n);
            let run = &items[i..end];
            let key = format!("group:{msg_idx}:{i}");
            // The run that is still growing stays open while the reply streams.
            let growing = live && end == items.len();
            if run.len() >= FOLD_AT && !growing && !expanded.contains(&key) {
                if group_card(ui, run, details, p).clicked() {
                    expanded.insert(key);
                }
            } else {
                if run.len() >= FOLD_AT
                    && !growing
                    && style::icon_link(ui, Icon::Chevron, &format!("Hide {} steps", run.len()), p)
                        .clicked()
                {
                    expanded.remove(&key);
                }
                for item in run {
                    if let Some(a) = step_card(ui, item, details, expanded, p) {
                        action = Some(a);
                    }
                    ui.add_space(4.0);
                }
            }
            ui.add_space(2.0);
            i = end;
            continue;
        }
        match &items[i] {
            Item::Reasoning { title, text } => {
                let (title, text) = (title.as_str(), text.as_str());
                render_reasoning_block(ui, title, text, p, (msg_idx, block), false);
                ui.add_space(2.0);
                block += 1;
            }
            Item::Agent { state, name, text } => agent_row(ui, state, name, text, p),
            Item::Message { from, to, text } => message_row(ui, from, to, text, p),
            Item::Warning(text) => {
                ui.horizontal(|ui| {
                    style::pill(ui, "warning", p.warn);
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(style::clean_label(text))
                                .size(12.0)
                                .color(p.dim),
                        )
                        .wrap(),
                    );
                });
                ui.add_space(2.0);
            }
            Item::Tool { .. } | Item::File { .. } => {}
        }
        i += 1;
    }

    if let Some((title, text)) = &trailing {
        let still_thinking = live && live_tool.is_none();
        render_reasoning_block(ui, title, text, p, (msg_idx, block), still_thinking);
    }
    if live {
        if let Some((id, name, args, started)) = live_tool {
            live_card(ui, id, name, args, started, project_root, p);
        }
    }
    action
}

/// The frame every step card uses.
fn step_frame(p: &Palette, accent: Option<egui::Color32>) -> egui::Frame {
    egui::Frame::none()
        .fill(p.surface)
        .stroke(egui::Stroke::new(
            1.0_f32,
            accent.map_or(p.border, |c| c.gamma_multiply(0.45)),
        ))
        .rounding(8.0)
        .inner_margin(egui::Margin::symmetric(10.0, 5.0))
}

/// One finished tool call or file change.
fn step_card(
    ui: &mut egui::Ui,
    item: &Item,
    details: &HashMap<String, ToolDetail>,
    expanded: &mut HashSet<String>,
    p: &Palette,
) -> Option<ActivityAction> {
    match item {
        Item::File {
            path,
            added,
            deleted,
        } => {
            let verb = if *deleted == 0 && *added > 0 {
                "Created"
            } else if *added == 0 && *deleted > 0 {
                "Deleted lines in"
            } else {
                "Edited"
            };
            let resp = step_frame(p, None)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 7.0;
                        mark(ui, Some(true), p);
                        ui.label(
                            egui::RichText::new(verb)
                                .font(style::strong(12.5))
                                .color(p.text),
                        );
                        let w = (ui.available_width() - 90.0).max(40.0);
                        ui.label(style::one_line(
                            ui,
                            path,
                            egui::FontId::monospace(12.0),
                            p.accent,
                            w,
                        ));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            // Laid out left to right, so "+12 -3" reads in order.
                            ui.horizontal(|ui| diff_counts(ui, *added, *deleted, p));
                        });
                    });
                })
                .response
                .interact(egui::Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text("Open the file");
            resp.clicked()
                .then(|| ActivityAction::OpenFile(path.to_string()))
        }
        Item::Tool {
            id,
            name,
            success,
            summary,
        } => {
            let detail = details.get(*id);
            let (verb, _) = tool_verbs(name);
            let target = tool_target(summary, detail.map_or("", |d| d.args.as_str()));
            let key = format!("tool:{id}");
            let open = expanded.contains(&key);
            let resp = step_frame(p, (!success).then_some(p.danger)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                let header = ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 7.0;
                    mark(ui, Some(*success), p);
                    ui.label(
                        egui::RichText::new(&verb)
                            .font(style::strong(12.5))
                            .color(p.text),
                    );
                    let right_w = 86.0;
                    let w = (ui.available_width() - right_w).max(40.0);
                    if !target.is_empty() {
                        let font = if matches!(*name, "web_search" | "search_web" | "spawn_agent") {
                            egui::FontId::proportional(12.5)
                        } else {
                            egui::FontId::monospace(12.0)
                        };
                        ui.label(style::one_line(ui, &target, font, p.dim, w))
                            .on_hover_text(&target);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        if detail.is_some() {
                            let (r, _) = ui
                                .allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                            // Chevron points down when open.
                            let r = if open {
                                egui::Rect::from_center_size(r.center(), r.size())
                            } else {
                                r
                            };
                            if open {
                                let c = r.center();
                                ui.painter().add(egui::Shape::line(
                                    vec![
                                        c + egui::vec2(-4.0, -2.0),
                                        c + egui::vec2(0.0, 2.0),
                                        c + egui::vec2(4.0, -2.0),
                                    ],
                                    egui::Stroke::new(1.4_f32, p.faint),
                                ));
                            } else {
                                style::paint_icon(ui.painter(), r, Icon::Chevron, p.faint);
                            }
                        }
                        if let Some(d) = detail {
                            ui.label(
                                egui::RichText::new(format_duration(d.duration))
                                    .size(11.0)
                                    .color(p.faint),
                            );
                        }
                    });
                });
                if open {
                    if let Some(d) = detail {
                        tool_body(ui, id, d, p);
                    }
                }
                header.response
            });
            let clicked = resp
                .inner
                .interact(egui::Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked();
            if clicked && detail.is_some() {
                if open {
                    expanded.remove(&key);
                } else {
                    expanded.insert(key);
                }
            }
            None
        }
        _ => None,
    }
}

/// The input and output of a call, under its card.
fn tool_body(ui: &mut egui::Ui, id: &str, d: &ToolDetail, p: &Palette) {
    ui.add_space(6.0);
    let section =
        |ui: &mut egui::Ui, label: &str, text: &str, salt: &str, max_h: f32, copy: bool| {
            ui.horizontal(|ui| {
                style::section_label(ui, label, p);
                if copy {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if style::icon_link(ui, Icon::Copy, "Copy", p).clicked() {
                            ui.ctx().copy_text(text.to_string());
                        }
                    });
                }
            });
            egui::Frame::none()
                .fill(p.code_bg)
                .rounding(6.0)
                .inner_margin(egui::Margin::symmetric(8.0, 6.0))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical()
                        .id_salt((salt, id))
                        .max_height(max_h)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(text)
                                        .font(egui::FontId::monospace(11.5))
                                        .color(p.code_text),
                                )
                                .wrap(),
                            );
                        });
                });
            ui.add_space(4.0);
        };
    let args = pretty_json(&d.args);
    if !args.trim().is_empty() && args.trim() != "{}" {
        section(
            ui,
            "Input",
            &crate::util::truncate_middle(&args, 6_000),
            "tool_in",
            160.0,
            false,
        );
    }
    let output = if d.result.trim().is_empty() {
        "(no output)".to_string()
    } else {
        d.result.trim_end().to_string()
    };
    section(
        ui,
        if d.success { "Output" } else { "Error" },
        &output,
        "tool_out",
        260.0,
        true,
    );
    let _ = &d.name;
}

/// A folded run of steps: how many, how many failed, which files changed.
fn group_card(
    ui: &mut egui::Ui,
    run: &[Item],
    details: &HashMap<String, ToolDetail>,
    p: &Palette,
) -> egui::Response {
    let failed = run
        .iter()
        .filter(|it| matches!(it, Item::Tool { success: false, .. }))
        .count();
    let files = run
        .iter()
        .filter(|it| matches!(it, Item::File { .. }))
        .count();
    let total: std::time::Duration = run
        .iter()
        .filter_map(|it| match it {
            Item::Tool { id, .. } => details.get(*id).map(|d| d.duration),
            _ => None,
        })
        .sum();
    // The kinds of work, in order of first appearance: "Read, Ran, Edited".
    let mut kinds: Vec<String> = Vec::new();
    for it in run {
        let verb = match it {
            Item::Tool { name, .. } => tool_verbs(name).0,
            Item::File { .. } => "Edited".to_string(),
            _ => continue,
        };
        if !kinds.contains(&verb) {
            kinds.push(verb);
        }
    }
    step_frame(p, None)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 7.0;
                mark(ui, Some(failed == 0), p);
                ui.label(
                    egui::RichText::new(format!("{} steps", run.len()))
                        .font(style::strong(12.5))
                        .color(p.text),
                );
                let w = (ui.available_width() - 150.0).max(40.0);
                ui.label(style::one_line(
                    ui,
                    &kinds.join(" · "),
                    egui::FontId::proportional(12.0),
                    p.dim,
                    w,
                ));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let (r, _) =
                        ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                    style::paint_icon(ui.painter(), r, Icon::Chevron, p.faint);
                    if total.as_millis() > 0 {
                        ui.label(
                            egui::RichText::new(format_duration(total))
                                .size(11.0)
                                .color(p.faint),
                        );
                    }
                    if failed > 0 {
                        style::pill(ui, &format!("{failed} failed"), p.danger);
                    }
                    if files > 0 {
                        style::pill(
                            ui,
                            &format!("{files} file{}", if files == 1 { "" } else { "s" }),
                            p.accent,
                        );
                    }
                });
            });
        })
        .response
        .interact(egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("Show every step")
}

/// The call in progress.
fn live_card(
    ui: &mut egui::Ui,
    _id: &str,
    name: &str,
    args: &str,
    started: Option<std::time::Instant>,
    project_root: &std::path::Path,
    p: &Palette,
) {
    let (_, now) = tool_verbs(name);
    let summary = format_tool_args_summary(name, args);
    let target = tool_target(&summary, args);
    let is_write = matches!(
        name,
        "write_file"
            | "write_to_file"
            | "edit_file"
            | "replace_file_content"
            | "multi_replace_file_content"
    );
    step_frame(p, Some(p.accent)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 7.0;
            mark(ui, None, p);
            ui.label(
                egui::RichText::new(format!("{now}…"))
                    .font(style::strong(12.5))
                    .color(p.text),
            );
            let w = (ui.available_width() - 100.0).max(40.0);
            if !target.is_empty() {
                ui.label(style::one_line(
                    ui,
                    &target,
                    egui::FontId::monospace(12.0),
                    p.dim,
                    w,
                ));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if let Some(t) = started {
                    ui.label(
                        egui::RichText::new(super::agents::format_elapsed(t.elapsed()))
                            .size(11.0)
                            .color(p.accent),
                    );
                }
                if is_write {
                    let (added, deleted) = estimate_live_diff_stats(name, args, project_root);
                    diff_counts(ui, added, deleted, p);
                }
            });
        });
    });
    ui.ctx().request_repaint();
}

/// Status mark: spinner (None), check or cross.
fn mark(ui: &mut egui::Ui, success: Option<bool>, p: &Palette) {
    let (r, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
    match success {
        None => style::paint_spinner(ui, r, p.accent),
        Some(true) => {
            ui.painter()
                .circle_filled(r.center(), 7.0, p.success.gamma_multiply(0.16));
            style::paint_icon(ui.painter(), r.shrink(2.0), Icon::Check, p.success);
        }
        Some(false) => {
            ui.painter()
                .circle_filled(r.center(), 7.0, p.danger.gamma_multiply(0.16));
            style::paint_icon(ui.painter(), r.shrink(3.0), Icon::Cross, p.danger);
        }
    }
}

fn agent_row(ui: &mut egui::Ui, state: &str, name: &str, text: &str, p: &Palette) {
    let (verb, color) = match state {
        "started" => ("started", p.accent),
        "done" => ("finished", p.success),
        _ => ("stopped", p.danger),
    };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let (r, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
        style::paint_icon(ui.painter(), r, Icon::Agents, color);
        style::pill(ui, name, style::name_color(name, p));
        ui.label(egui::RichText::new(verb).size(12.0).color(color));
        let w = ui.available_width().max(40.0);
        ui.label(style::one_line(
            ui,
            &text.replace('\n', " "),
            egui::FontId::proportional(12.0),
            p.dim,
            w,
        ))
        .on_hover_text(text);
    });
    ui.add_space(3.0);
}

fn message_row(ui: &mut egui::Ui, from: &str, to: &str, text: &str, p: &Palette) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        style::pill(ui, from, style::name_color(from, p));
        ui.label(egui::RichText::new("→").size(12.0).color(p.faint));
        style::pill(ui, to, style::name_color(to, p));
        let w = ui.available_width().max(40.0);
        ui.label(style::one_line(
            ui,
            &text.replace('\n', " "),
            egui::FontId::proportional(12.0),
            p.dim,
            w,
        ))
        .on_hover_text(text);
    });
    ui.add_space(3.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logs_become_items_with_pipes_kept_in_summaries() {
        let logs = vec![
            "THINKING_DELTA|let me look".to_string(),
            "TOOL_RESULT|c1|run_command|✅ Success|ls | wc -l".to_string(),
            "FILE_CHANGE|x|src/a.rs|+3|-1".to_string(),
        ];
        let (items, trailing) = parse(&logs, &None);
        assert!(trailing.is_none());
        assert!(matches!(items[0], Item::Reasoning { .. }));
        match &items[1] {
            Item::Tool {
                id,
                summary,
                success,
                ..
            } => {
                assert_eq!(*id, "c1");
                assert_eq!(*summary, "ls | wc -l");
                assert!(success);
            }
            _ => panic!("expected a tool"),
        }
        assert!(matches!(
            items[2],
            Item::File {
                added: 3,
                deleted: 1,
                ..
            }
        ));
    }

    #[test]
    fn mcp_tools_read_as_server_and_tool() {
        assert_eq!(
            tool_verbs("mcp__github__search_repos").0,
            "github · search repos"
        );
        assert_eq!(tool_verbs("read_file").1, "Reading");
    }

    #[test]
    fn targets_fall_back_to_the_arguments() {
        assert_eq!(
            tool_target("", r#"{"url": "https://x.dev"}"#),
            "https://x.dev"
        );
        assert_eq!(tool_target("main.rs", "{}"), "main.rs");
    }
}
