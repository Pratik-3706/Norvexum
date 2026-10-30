// ===========================================================================
// Browser tab of the right panel — the agent's browser, live, inside the app.
//
// The page is the shared session in `tools::browser` (headless while the GUI
// runs). While the tab is visible the page streams a frame whenever it
// changes (decoded off the UI thread); clicks, the scroll wheel and typing on
// it go to the page.
// ===========================================================================

use super::*;
use crate::tools::browser::{self, UserAction, VIEW_HEIGHT, VIEW_WIDTH};

impl GuiApp {
    pub(super) fn draw_browser_tab(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let timeout = Duration::from_secs(self.settings.browser_timeout_secs.max(5));
        let frame = browser::latest_frame();
        let open = browser::is_open();

        // New picture → update the texture in place.
        if let Some(f) = &frame {
            if self
                .browser_tex
                .as_ref()
                .is_none_or(|(seq, _)| *seq != f.seq)
            {
                let color = egui::ColorImage::from_rgba_unmultiplied([f.width, f.height], &f.rgba);
                match &mut self.browser_tex {
                    Some((seq, tex)) => {
                        tex.set(color, egui::TextureOptions::LINEAR);
                        *seq = f.seq;
                    }
                    None => {
                        let tex = ui.ctx().load_texture(
                            "browser_live",
                            color,
                            egui::TextureOptions::LINEAR,
                        );
                        self.browser_tex = Some((f.seq, tex));
                    }
                }
                if !self.browser_url_editing {
                    self.browser_url = f.url.clone();
                }
            }
        } else if !open {
            self.browser_tex = None;
        }

        // Toolbar: back, forward, reload, address, close.
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let enabled = open;
            for (glyph, hint, action) in [
                ("‹", "Back", UserAction::Back),
                ("›", "Forward", UserAction::Forward),
                ("⟳", "Reload", UserAction::Reload),
            ] {
                let button = egui::Button::new(egui::RichText::new(glyph).size(17.0).color(p.dim))
                    .frame(false)
                    .min_size(egui::vec2(26.0, 26.0));
                if ui
                    .add_enabled(enabled, button)
                    .on_hover_text(hint)
                    .clicked()
                {
                    browser::user_action(action, timeout);
                }
            }
            // An exact size: egui puts a text box's margins outside its desired
            // width, and anything wider than the space left widens the panel a
            // little more every frame.
            let close_w = if open { 26.0 } else { 0.0 };
            let width = (ui.available_width() - close_w).max(60.0);
            let address = ui.add_sized(
                [width, 26.0],
                egui::TextEdit::singleline(&mut self.browser_url)
                    .hint_text(egui::RichText::new("Search or enter a URL").color(p.faint))
                    .margin(egui::Margin::symmetric(8.0, 4.0)),
            );
            self.browser_url_editing = address.has_focus();
            if address.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let url = self.browser_url.trim().to_string();
                if !url.is_empty() {
                    browser::user_action(UserAction::Navigate(url), timeout);
                }
            }
            if open
                && style::close_button(ui, p)
                    .on_hover_text("Close the browser")
                    .clicked()
            {
                browser::user_action(UserAction::Close, timeout);
            }
        });
        ui.add_space(6.0);

        let Some((_, tex)) = &self.browser_tex else {
            empty_hint(
                ui,
                if open {
                    "Loading…"
                } else {
                    "No browser open"
                },
                "Enter an address above, or ask the agent to open a page. It shows up here \
                 instead of in a separate window, and you can click and scroll it.",
                p,
            );
            return;
        };

        // The page, scaled to the panel width.
        let width = ui.available_width();
        let scale = width / VIEW_WIDTH as f32;
        let size = egui::vec2(width, VIEW_HEIGHT as f32 * scale);
        let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
        ui.painter().rect_filled(rect, 6.0, p.code_bg);
        let mut mesh_rect = rect;
        mesh_rect.max.y = mesh_rect.min.y + size.y;
        ui.painter().image(
            tex.id(),
            mesh_rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
        let focused = resp.has_focus();
        ui.painter().rect_stroke(
            rect,
            6.0,
            egui::Stroke::new(
                1.0_f32,
                if focused {
                    p.accent.gamma_multiply(0.6)
                } else {
                    p.border
                },
            ),
        );
        let to_page =
            |pos: egui::Pos2| ((pos.x - rect.left()) / scale, (pos.y - rect.top()) / scale);

        let busy = browser::agent_busy();
        if resp.clicked() {
            resp.request_focus();
            if let Some(pos) = resp.interact_pointer_pos() {
                let (x, y) = to_page(pos);
                browser::user_action(
                    UserAction::Click {
                        x: f64::from(x),
                        y: f64::from(y),
                    },
                    timeout,
                );
            }
        }
        if resp.hovered() {
            let wheel = ui.input(|i| i.raw_scroll_delta.y);
            if wheel.abs() > 0.5 {
                if let Some(pos) = resp.hover_pos() {
                    let (x, y) = to_page(pos);
                    browser::user_action(
                        UserAction::Scroll {
                            x: f64::from(x),
                            y: f64::from(y),
                            dy: f64::from(-wheel / scale),
                        },
                        timeout,
                    );
                }
            }
        }
        if focused {
            // Keys go to the page, not to egui's focus navigation.
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    resp.id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: false,
                    },
                )
            });
            let mut typed = String::new();
            let mut keys = Vec::new();
            ui.input(|i| {
                for event in &i.events {
                    match event {
                        egui::Event::Text(t) => typed.push_str(t),
                        egui::Event::Paste(t) => typed.push_str(t),
                        egui::Event::Key {
                            key, pressed: true, ..
                        } => {
                            let name = match key {
                                egui::Key::Enter => "Enter",
                                egui::Key::Backspace => "Backspace",
                                egui::Key::Delete => "Delete",
                                egui::Key::Tab => "Tab",
                                egui::Key::ArrowUp => "ArrowUp",
                                egui::Key::ArrowDown => "ArrowDown",
                                egui::Key::ArrowLeft => "ArrowLeft",
                                egui::Key::ArrowRight => "ArrowRight",
                                egui::Key::PageUp => "PageUp",
                                egui::Key::PageDown => "PageDown",
                                egui::Key::Home => "Home",
                                egui::Key::End => "End",
                                _ => continue,
                            };
                            keys.push(name.to_string());
                        }
                        _ => {}
                    }
                }
            });
            if !typed.is_empty() {
                browser::user_action(UserAction::Type(typed), timeout);
            }
            for key in keys {
                browser::user_action(UserAction::Key(key), timeout);
            }
        }

        // Status under the page.
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if busy {
                style::dot(ui, p.accent);
                ui.label(
                    egui::RichText::new("Agent is using the browser")
                        .size(11.5)
                        .color(p.accent),
                );
            } else if focused {
                ui.label(
                    egui::RichText::new("Typing goes to the page · Esc to stop")
                        .size(11.5)
                        .color(p.faint),
                );
            } else if let Some(f) = &frame {
                ui.add(
                    egui::Label::new(egui::RichText::new(&f.title).size(11.5).color(p.dim))
                        .truncate(),
                );
            }
        });
        if focused && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            resp.surrender_focus();
        }
        // New frames request a repaint themselves (see on_new_frame).
        ui.ctx().request_repaint_after(Duration::from_millis(1000));
    }
}
