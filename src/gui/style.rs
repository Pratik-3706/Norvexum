// ===========================================================================
// GUI style — colour tokens, fonts and the few small widgets every panel uses.
//
// One neutral palette per theme plus a single blue accent taken from the logo.
// Panels read colours from `Palette` instead of hard-coding them.
// ===========================================================================

use egui::{
    Color32, FontData, FontDefinitions, FontFamily, FontId, Margin, Response, RichText, Rounding,
    Stroke, Ui, Vec2,
    epaint::Shadow,
    text::{LayoutJob, TextWrapping},
};

/// Reading width of the conversation column and the composer.
pub const MAX_COLUMN: f32 = 780.0;
pub const BODY_SIZE: f32 = 13.5;
const STRONG_FAMILY: &str = "strong";

#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Color32,
    pub panel: Color32,
    pub surface: Color32,
    pub hover: Color32,
    pub border: Color32,
    pub text: Color32,
    pub dim: Color32,
    pub faint: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    pub on_accent: Color32,
    pub success: Color32,
    pub danger: Color32,
    pub warn: Color32,
    pub code_bg: Color32,
    pub code_text: Color32,
}

impl Palette {
    pub fn new(dark: bool) -> Self {
        let rgb = Color32::from_rgb;
        if dark {
            Self {
                bg: rgb(17, 18, 20),
                panel: rgb(21, 22, 25),
                surface: rgb(27, 28, 32),
                hover: rgb(36, 38, 43),
                border: rgb(42, 44, 49),
                text: rgb(228, 229, 231),
                dim: rgb(154, 157, 164),
                faint: rgb(107, 110, 118),
                accent: rgb(122, 146, 240),
                accent_soft: rgb(31, 37, 58),
                on_accent: rgb(12, 14, 22),
                success: rgb(94, 194, 135),
                danger: rgb(232, 98, 98),
                warn: rgb(224, 175, 84),
                code_bg: rgb(23, 24, 27),
                code_text: rgb(214, 186, 150),
            }
        } else {
            Self {
                bg: rgb(250, 250, 250),
                panel: rgb(244, 244, 245),
                surface: rgb(255, 255, 255),
                hover: rgb(234, 234, 237),
                border: rgb(222, 223, 226),
                text: rgb(24, 25, 28),
                dim: rgb(92, 96, 104),
                faint: rgb(140, 144, 152),
                accent: rgb(59, 91, 219),
                accent_soft: rgb(229, 234, 251),
                on_accent: rgb(255, 255, 255),
                success: rgb(22, 140, 80),
                danger: rgb(200, 50, 50),
                warn: rgb(176, 116, 20),
                code_bg: rgb(245, 245, 247),
                code_text: rgb(150, 72, 30),
            }
        }
    }
}

/// Apply the palette to egui's visuals and spacing. Cheap, but only needed
/// when the theme changes.
pub fn apply(ctx: &egui::Context, p: &Palette, dark: bool) {
    let mut visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    visuals.panel_fill = p.bg;
    visuals.window_fill = p.surface;
    visuals.extreme_bg_color = p.surface;
    visuals.faint_bg_color = p.panel;
    visuals.code_bg_color = p.code_bg;
    visuals.hyperlink_color = p.accent;
    visuals.warn_fg_color = p.warn;
    visuals.error_fg_color = p.danger;
    visuals.window_stroke = Stroke::new(1.0_f32, p.border);
    visuals.window_rounding = Rounding::same(10.0);
    visuals.menu_rounding = Rounding::same(8.0);
    let shadow = Shadow {
        offset: Vec2::new(0.0, 6.0),
        blur: 20.0,
        spread: 0.0,
        color: Color32::from_black_alpha(if dark { 90 } else { 28 }),
    };
    visuals.window_shadow = shadow;
    visuals.popup_shadow = shadow;
    visuals.selection.bg_fill = p.accent.gamma_multiply(0.35);
    visuals.selection.stroke = Stroke::new(1.0_f32, p.accent);
    visuals.striped = false;

    let w = &mut visuals.widgets;
    w.noninteractive.bg_fill = p.surface;
    w.noninteractive.weak_bg_fill = p.surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, p.text);
    w.noninteractive.rounding = Rounding::same(6.0);

    w.inactive.bg_fill = p.hover;
    w.inactive.weak_bg_fill = p.surface;
    w.inactive.bg_stroke = Stroke::new(1.0_f32, p.border);
    w.inactive.fg_stroke = Stroke::new(1.0_f32, p.text);
    w.inactive.rounding = Rounding::same(6.0);

    w.hovered.bg_fill = p.hover;
    w.hovered.weak_bg_fill = p.hover;
    w.hovered.bg_stroke = Stroke::new(1.0_f32, p.border);
    w.hovered.fg_stroke = Stroke::new(1.0_f32, p.text);
    w.hovered.rounding = Rounding::same(6.0);
    w.hovered.expansion = 0.0;

    w.active.bg_fill = p.accent;
    w.active.weak_bg_fill = p.hover;
    w.active.bg_stroke = Stroke::new(1.0_f32, p.accent);
    w.active.fg_stroke = Stroke::new(1.0_f32, p.text);
    w.active.rounding = Rounding::same(6.0);
    w.active.expansion = 0.0;

    w.open.bg_fill = p.hover;
    w.open.weak_bg_fill = p.hover;
    w.open.bg_stroke = Stroke::new(1.0_f32, p.border);
    w.open.fg_stroke = Stroke::new(1.0_f32, p.text);
    w.open.rounding = Rounding::same(6.0);

    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    use egui::TextStyle;
    style.text_styles = [
        (TextStyle::Small, FontId::proportional(11.0)),
        (TextStyle::Body, FontId::proportional(BODY_SIZE)),
        (TextStyle::Button, FontId::proportional(13.0)),
        (TextStyle::Heading, strong(17.0)),
        (TextStyle::Monospace, FontId::monospace(12.5)),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.interact_size.y = 26.0;
    style.spacing.combo_width = 140.0;
    style.spacing.window_margin = Margin::same(16.0);
    style.spacing.menu_margin = Margin::same(6.0);
    // Scroll bars keep a lane of their own and never change size or show a
    // track on hover: a floating bar over the chat grew and faded in its track
    // when the pointer reached it, and on the edge it flickered between that
    // and the message under it.
    style.spacing.scroll = egui::style::ScrollStyle {
        floating: false,
        bar_width: 6.0,
        floating_width: 6.0,
        floating_allocated_width: 0.0,
        handle_min_length: 24.0,
        bar_inner_margin: 2.0,
        bar_outer_margin: 0.0,
        foreground_color: true,
        dormant_background_opacity: 0.0,
        active_background_opacity: 0.0,
        interact_background_opacity: 0.0,
        dormant_handle_opacity: 0.35,
        active_handle_opacity: 0.55,
        interact_handle_opacity: 0.8,
    };
    ctx.set_style(style);
}

/// Prefer the platform UI font (Segoe UI / Cascadia on Windows) and keep
/// egui's bundled fonts as fallbacks for symbols and emoji in model output.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let dirs = font_dirs();

    // Preferred font files, best first; the first one installed wins.
    let regular = find_font(
        &dirs,
        &["segoeui.ttf", "Inter-Regular.ttf", "DejaVuSans.ttf"],
    );
    let bold = find_font(
        &dirs,
        &[
            "segoeuisb.ttf",
            "segoeuib.ttf",
            "Inter-SemiBold.ttf",
            "DejaVuSans-Bold.ttf",
        ],
    );
    let mono = find_font(
        &dirs,
        &["CascadiaMono.ttf", "consola.ttf", "DejaVuSansMono.ttf"],
    );

    let fallback = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    if let Some(bytes) = regular {
        fonts
            .font_data
            .insert("ui-regular".into(), FontData::from_owned(bytes));
        if let Some(list) = fonts.families.get_mut(&FontFamily::Proportional) {
            list.insert(0, "ui-regular".into());
        }
    }
    let mut strong_list = fallback.clone();
    match bold {
        Some(bytes) => {
            fonts
                .font_data
                .insert("ui-strong".into(), FontData::from_owned(bytes));
            strong_list.insert(0, "ui-strong".into());
        }
        None => {
            if let Some(list) = fonts.families.get(&FontFamily::Proportional) {
                strong_list = list.clone();
            }
        }
    }
    // Always registered, so `strong()` can never reference a missing family.
    fonts
        .families
        .insert(FontFamily::Name(STRONG_FAMILY.into()), strong_list);
    if let Some(bytes) = mono {
        fonts
            .font_data
            .insert("ui-mono".into(), FontData::from_owned(bytes));
        if let Some(list) = fonts.families.get_mut(&FontFamily::Monospace) {
            list.insert(0, "ui-mono".into());
        }
    }
    ctx.set_fonts(fonts);
}

/// System and per-user font folders for the current platform.
fn font_dirs() -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    let mut dirs = Vec::new();
    if let Some(windir) = std::env::var_os("WINDIR") {
        dirs.push(PathBuf::from(windir).join("Fonts"));
    }
    if let Some(local) = dirs::data_local_dir() {
        // Fonts installed "for this user only" on Windows.
        dirs.push(local.join("Microsoft").join("Windows").join("Fonts"));
    }
    if let Some(data) = dirs::data_dir() {
        dirs.push(data.join("fonts"));
    }
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".fonts"));
        dirs.push(home.join("Library").join("Fonts"));
    }
    for dir in [
        "/usr/share/fonts",
        "/usr/local/share/fonts",
        "/Library/Fonts",
    ] {
        dirs.push(PathBuf::from(dir));
    }
    dirs.retain(|d| d.is_dir());
    dirs
}

/// Read the first of `names` found in `dirs` (searched a few levels deep,
/// since Linux distributions nest fonts by family).
fn find_font(dirs: &[std::path::PathBuf], names: &[&str]) -> Option<Vec<u8>> {
    names.iter().find_map(|name| {
        dirs.iter().find_map(|dir| {
            walkdir::WalkDir::new(dir)
                .max_depth(3)
                .into_iter()
                .flatten()
                .find(|e| e.file_type().is_file() && e.file_name().eq_ignore_ascii_case(name))
                .and_then(|e| std::fs::read(e.path()).ok())
        })
    })
}

/// Bold/semibold UI font for titles and emphasis.
pub fn strong(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(STRONG_FAMILY.into()))
}

/// Small uppercase heading above a group of controls.
pub fn section_label(ui: &mut Ui, text: &str, p: &Palette) {
    ui.label(
        RichText::new(text.to_uppercase())
            .font(strong(10.5))
            .color(p.faint),
    );
}

/// How long the glow after a click lasts, in seconds.
const FLASH_SECS: f64 = 0.6;

/// Seconds since the widget `id` was last clicked, while its glow lasts.
fn flash_age(ui: &Ui, id: egui::Id) -> Option<f64> {
    let t0 = ui
        .ctx()
        .data(|d| d.get_temp::<f64>(id.with("click_flash")))?;
    let age = ui.input(|i| i.time) - t0;
    (age < FLASH_SECS).then_some(age)
}

/// A short accent glow over a button that was just clicked, so a click
/// that changes nothing visible (Copy, Undo, …) still shows it landed.
pub fn click_flash(ui: &Ui, resp: &Response, rounding: f32, p: &Palette) {
    if resp.clicked() {
        let now = ui.input(|i| i.time);
        ui.ctx()
            .data_mut(|d| d.insert_temp(resp.id.with("click_flash"), now));
    }
    if let Some(age) = flash_age(ui, resp.id) {
        let fade = (1.0 - age / FLASH_SECS) as f32;
        let painter = ui.painter();
        painter.rect_filled(resp.rect, rounding, p.accent.gamma_multiply(0.22 * fade));
        painter.rect_stroke(
            resp.rect.expand(1.5 * fade),
            rounding,
            Stroke::new(1.0, p.accent.gamma_multiply(0.8 * fade)),
        );
        ui.ctx().request_repaint();
    }
}

/// Small frameless action link (Copy, Edit, …). "Copy" reads "Copied" for a
/// moment after a click.
pub fn link_button(ui: &mut Ui, label: &str, p: &Palette) -> Response {
    let id = ui.next_auto_id();
    let flashing = flash_age(ui, id).is_some();
    let (text, color) = match (label, flashing) {
        ("Copy", true) => ("Copied", p.accent),
        (_, true) => (label, p.accent),
        _ => (label, p.faint),
    };
    let resp = ui.add(
        egui::Button::new(RichText::new(text).size(11.5).color(color))
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::NONE)
            .rounding(4.0),
    );
    click_flash(ui, &resp, 4.0, p);
    resp
}

pub fn primary_button(ui: &mut Ui, label: &str, p: &Palette) -> Response {
    ui.add(
        egui::Button::new(RichText::new(label).size(12.5).color(p.on_accent))
            .fill(p.accent)
            .stroke(Stroke::NONE)
            .rounding(6.0),
    )
}

pub fn secondary_button(ui: &mut Ui, label: &str, p: &Palette) -> Response {
    ui.add(
        egui::Button::new(RichText::new(label).size(12.5).color(p.text))
            .fill(p.surface)
            .stroke(Stroke::new(1.0_f32, p.border))
            .rounding(6.0),
    )
}

/// A row of equal-width segments, one selected; `badge` is a small count or
/// marker drawn after the label. Returns the segment clicked this frame.
pub fn segmented(
    ui: &mut Ui,
    items: &[(&str, Option<String>)],
    selected: usize,
    p: &Palette,
) -> Option<usize> {
    const HEIGHT: f32 = 30.0;
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), HEIGHT),
        egui::Sense::hover(),
    );
    let painter = ui.painter().clone();
    painter.rect(rect, 8.0, p.surface, Stroke::new(1.0_f32, p.border));
    let width = (rect.width() - 4.0) / items.len().max(1) as f32;
    let mut clicked = None;
    for (i, (label, badge)) in items.iter().enumerate() {
        let seg = egui::Rect::from_min_size(
            rect.min + egui::vec2(2.0 + i as f32 * width, 2.0),
            egui::vec2(width, HEIGHT - 4.0),
        );
        let resp = ui.interact(seg, ui.id().with(("segment", i)), egui::Sense::click());
        let active = i == selected;
        if active {
            painter.rect_filled(seg, 6.0, p.hover);
        } else if resp.hovered() {
            painter.rect_filled(seg, 6.0, p.panel);
        }
        let label_g = painter.layout_no_wrap(
            label.to_string(),
            if active {
                strong(12.5)
            } else {
                FontId::proportional(12.5)
            },
            if active || resp.hovered() {
                p.text
            } else {
                p.dim
            },
        );
        let badge_g = badge
            .as_ref()
            .map(|b| painter.layout_no_wrap(b.clone(), FontId::proportional(11.0), p.faint));
        let total = label_g.size().x + badge_g.as_ref().map_or(0.0, |g| g.size().x + 5.0);
        let mut x = seg.center().x - total / 2.0;
        let label_w = label_g.size().x;
        painter.galley(
            egui::pos2(x, seg.center().y - label_g.size().y / 2.0),
            label_g,
            p.text,
        );
        x += label_w + 5.0;
        if let Some(g) = badge_g {
            painter.galley(
                egui::pos2(x, seg.center().y - g.size().y / 2.0 + 0.5),
                g,
                p.faint,
            );
        }
        if resp.clicked() {
            clicked = Some(i);
        }
    }
    clicked
}

/// Small pill that toggles an option on and off.
pub fn chip(ui: &mut Ui, label: &str, on: bool, p: &Palette) -> Response {
    ui.add(
        egui::Button::new(
            RichText::new(label)
                .size(11.5)
                .color(if on { p.accent } else { p.dim }),
        )
        .fill(if on {
            p.accent_soft
        } else {
            Color32::TRANSPARENT
        })
        .stroke(Stroke::new(
            1.0_f32,
            if on {
                p.accent.gamma_multiply(0.45)
            } else {
                p.border
            },
        ))
        .rounding(11.0)
        .min_size(egui::vec2(0.0, 22.0)),
    )
}

/// Square glyph button drawing a ×, for closing a pane.
pub fn close_button(ui: &mut Ui, p: &Palette) -> Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::click());
    let hovered = resp.hovered();
    if hovered {
        ui.painter().rect_filled(rect, 5.0, p.hover);
    }
    let c = rect.center();
    let stroke = Stroke::new(1.4_f32, if hovered { p.text } else { p.faint });
    for (a, b) in [
        (egui::vec2(-3.5, -3.5), egui::vec2(3.5, 3.5)),
        (egui::vec2(-3.5, 3.5), egui::vec2(3.5, -3.5)),
    ] {
        ui.painter().line_segment([c + a, c + b], stroke);
    }
    resp
}

/// Single-line search box with a magnifier and a clear button.
pub fn search_field(ui: &mut Ui, text: &mut String, hint: &str, p: &Palette) -> Response {
    let mut response = None;
    egui::Frame::none()
        .fill(p.surface)
        .stroke(Stroke::new(1.0_f32, p.border))
        .rounding(7.0)
        .inner_margin(Margin::symmetric(8.0, 3.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let (r, _) = ui.allocate_exact_size(egui::vec2(12.0, 20.0), egui::Sense::hover());
                let c = r.center() - egui::vec2(1.0, 1.0);
                let stroke = Stroke::new(1.3_f32, p.faint);
                ui.painter().circle_stroke(c, 4.0, stroke);
                ui.painter()
                    .line_segment([c + egui::vec2(3.0, 3.0), c + egui::vec2(6.0, 6.0)], stroke);

                let clear_w = if text.is_empty() { 0.0 } else { 22.0 };
                let edit = ui.add(
                    egui::TextEdit::singleline(text)
                        .hint_text(RichText::new(hint).color(p.faint))
                        .frame(false)
                        .margin(Margin::symmetric(0.0, 2.0))
                        .desired_width((ui.available_width() - clear_w).max(20.0)),
                );
                if !text.is_empty() && close_button(ui, p).on_hover_text("Clear").clicked() {
                    text.clear();
                }
                response = Some(edit);
            });
        });
    response.expect("search field row always renders")
}

/// A small filled status dot, vertically centred in the current row.
pub fn dot(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.5, color);
}

/// Lay out text on a single line, cutting it with an ellipsis at `max_width`.
pub fn one_line(
    ui: &Ui,
    text: &str,
    font: FontId,
    color: Color32,
    max_width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap = TextWrapping {
        max_width: max_width.max(10.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    ui.fonts(|f| f.layout_job(job))
}

// ---------------------------------------------------------------------------
// Icons — drawn with the painter so they look the same with every font.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Chats,
    Plus,
    Search,
    Agents,
    Terminal,
    Globe,
    Files,
    Gear,
    Sun,
    Moon,
    Panel,
    Plug,
    Chevron,
    Check,
    Cross,
    Copy,
    Stop,
    Refresh,
    Undo,
    Edit,
    Clip,
}

/// Draw `icon` centred in `rect` (designed on a 16×16 grid).
pub fn paint_icon(painter: &egui::Painter, rect: egui::Rect, icon: Icon, color: Color32) {
    use egui::{Pos2, vec2};
    let s = rect.width().min(rect.height()) / 16.0;
    let o = rect.center() - vec2(8.0, 8.0) * s;
    let p = |x: f32, y: f32| -> Pos2 { o + vec2(x, y) * s };
    let stroke = Stroke::new(1.4 * s.max(0.8), color);
    let line = |a: Pos2, b: Pos2| painter.line_segment([a, b], stroke);
    let poly = |pts: Vec<Pos2>| painter.add(egui::Shape::line(pts, stroke));
    match icon {
        Icon::Chats => {
            painter.rect_stroke(
                egui::Rect::from_min_max(p(2.0, 3.0), p(14.0, 11.5)),
                2.5 * s,
                stroke,
            );
            poly(vec![p(5.0, 11.5), p(4.5, 14.0), p(8.0, 11.5)]);
        }
        Icon::Plus => {
            line(p(8.0, 3.0), p(8.0, 13.0));
            line(p(3.0, 8.0), p(13.0, 8.0));
        }
        Icon::Search => {
            painter.circle_stroke(p(7.0, 7.0), 4.2 * s, stroke);
            line(p(10.2, 10.2), p(13.5, 13.5));
        }
        Icon::Agents => {
            line(p(8.0, 5.0), p(4.0, 11.0));
            line(p(8.0, 5.0), p(12.0, 11.0));
            painter.circle_filled(p(8.0, 4.5), 2.2 * s, color);
            painter.circle_filled(p(4.0, 11.5), 2.0 * s, color);
            painter.circle_filled(p(12.0, 11.5), 2.0 * s, color);
        }
        Icon::Terminal => {
            painter.rect_stroke(
                egui::Rect::from_min_max(p(1.5, 2.5), p(14.5, 13.5)),
                2.0 * s,
                stroke,
            );
            poly(vec![p(4.5, 6.0), p(7.0, 8.0), p(4.5, 10.0)]);
            line(p(8.5, 10.5), p(11.5, 10.5));
        }
        Icon::Globe => {
            painter.circle_stroke(p(8.0, 8.0), 6.0 * s, stroke);
            line(p(2.0, 8.0), p(14.0, 8.0));
            painter.add(egui::Shape::ellipse_stroke(
                p(8.0, 8.0),
                vec2(2.6, 6.0) * s,
                stroke,
            ));
        }
        Icon::Files => {
            poly(vec![
                p(2.0, 4.0),
                p(6.0, 4.0),
                p(7.5, 5.5),
                p(14.0, 5.5),
                p(14.0, 13.0),
                p(2.0, 13.0),
                p(2.0, 4.0),
            ]);
        }
        Icon::Gear => {
            painter.circle_stroke(p(8.0, 8.0), 2.2 * s, stroke);
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::TAU / 8.0;
                let (sin, cos) = a.sin_cos();
                line(
                    p(8.0 + cos * 4.4, 8.0 + sin * 4.4),
                    p(8.0 + cos * 6.2, 8.0 + sin * 6.2),
                );
            }
            painter.circle_stroke(p(8.0, 8.0), 4.4 * s, stroke);
        }
        Icon::Sun => {
            painter.circle_stroke(p(8.0, 8.0), 3.0 * s, stroke);
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::TAU / 8.0;
                let (sin, cos) = a.sin_cos();
                line(
                    p(8.0 + cos * 5.0, 8.0 + sin * 5.0),
                    p(8.0 + cos * 6.6, 8.0 + sin * 6.6),
                );
            }
        }
        Icon::Moon => {
            let pts: Vec<Pos2> = (0..=24)
                .map(|i| {
                    let a = -1.9 + i as f32 * (3.8 / 24.0);
                    p(8.0 - a.cos() * 6.0, 8.0 + a.sin() * 6.0)
                })
                .chain((0..=24).map(|i| {
                    let a = 1.4 - i as f32 * (2.8 / 24.0);
                    p(6.5 - a.cos() * 4.6, 8.0 + a.sin() * 4.6)
                }))
                .collect();
            poly(pts);
        }
        Icon::Panel => {
            painter.rect_stroke(
                egui::Rect::from_min_max(p(1.5, 2.5), p(14.5, 13.5)),
                2.0 * s,
                stroke,
            );
            line(p(10.0, 2.5), p(10.0, 13.5));
        }
        Icon::Plug => {
            line(p(6.0, 2.0), p(6.0, 5.0));
            line(p(10.0, 2.0), p(10.0, 5.0));
            painter.rect_stroke(
                egui::Rect::from_min_max(p(3.5, 5.0), p(12.5, 9.5)),
                2.0 * s,
                stroke,
            );
            poly(vec![p(8.0, 9.5), p(8.0, 12.0), p(10.0, 14.0)]);
        }
        Icon::Chevron => {
            poly(vec![p(6.0, 4.0), p(10.0, 8.0), p(6.0, 12.0)]);
        }
        Icon::Check => {
            poly(vec![p(3.5, 8.5), p(6.5, 11.5), p(12.5, 4.5)]);
        }
        Icon::Cross => {
            line(p(4.0, 4.0), p(12.0, 12.0));
            line(p(4.0, 12.0), p(12.0, 4.0));
        }
        Icon::Copy => {
            painter.rect_stroke(
                egui::Rect::from_min_max(p(5.5, 5.5), p(13.5, 13.5)),
                1.8 * s,
                stroke,
            );
            poly(vec![p(10.5, 3.0), p(2.5, 3.0), p(2.5, 10.5)]);
        }
        Icon::Stop => {
            painter.rect_filled(
                egui::Rect::from_min_max(p(4.5, 4.5), p(11.5, 11.5)),
                1.5 * s,
                color,
            );
        }
        Icon::Refresh => {
            let pts: Vec<Pos2> = (0..=20)
                .map(|i| {
                    let a = -0.6 + i as f32 * (4.9 / 20.0);
                    p(8.0 + a.cos() * 5.0, 8.0 + a.sin() * 5.0)
                })
                .collect();
            poly(pts);
            poly(vec![p(13.6, 2.4), p(13.2, 5.6), p(10.0, 5.2)]);
        }
        Icon::Undo => {
            poly(vec![p(5.5, 3.5), p(2.5, 6.5), p(5.5, 9.5)]);
            let mut pts = vec![p(2.5, 6.5)];
            pts.extend((0..=12).map(|i| {
                let a = -std::f32::consts::FRAC_PI_2 + i as f32 * (std::f32::consts::PI / 12.0);
                p(9.0 + a.cos() * 4.0, 10.5 + a.sin() * 4.0)
            }));
            pts.push(p(6.0, 14.5));
            poly(pts);
        }
        Icon::Edit => {
            poly(vec![
                p(3.0, 13.0),
                p(3.5, 10.0),
                p(11.0, 2.5),
                p(13.5, 5.0),
                p(6.0, 12.5),
                p(3.0, 13.0),
            ]);
        }
        Icon::Clip => {
            let pts: Vec<Pos2> = [
                (10.5, 6.0),
                (6.0, 10.5),
                (6.0, 12.0),
                (7.5, 12.5),
                (13.0, 7.0),
                (13.0, 4.0),
                (11.0, 2.5),
                (8.5, 2.5),
                (3.0, 8.0),
                (3.0, 12.5),
                (5.5, 14.5),
                (8.5, 14.0),
                (12.5, 10.0),
            ]
            .iter()
            .map(|&(x, y)| p(x, y))
            .collect();
            poly(pts);
        }
    }
}

/// A square button showing `icon`; `active` marks a toggled-on panel.
pub fn icon_button(ui: &mut Ui, icon: Icon, size: f32, active: bool, p: &Palette) -> Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let hovered = resp.hovered();
    if active {
        ui.painter().rect_filled(rect, 8.0, p.accent_soft);
    } else if hovered {
        ui.painter().rect_filled(rect, 8.0, p.hover);
    }
    let color = if active {
        p.accent
    } else if hovered {
        p.text
    } else {
        p.dim
    };
    let glyph = egui::Rect::from_center_size(rect.center(), egui::vec2(size * 0.5, size * 0.5));
    paint_icon(ui.painter(), glyph, icon, color);
    click_flash(ui, &resp, 8.0, p);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A small inline action: icon plus optional label, frameless.
pub fn icon_link(ui: &mut Ui, icon: Icon, label: &str, p: &Palette) -> Response {
    let id = ui.next_auto_id();
    let flashing = flash_age(ui, id).is_some();
    let text = match (label, flashing) {
        ("Copy", true) => "Copied",
        _ => label,
    };
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_string(), FontId::proportional(11.5), p.faint);
    let width = 22.0
        + if text.is_empty() {
            0.0
        } else {
            galley.size().x + 4.0
        };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 22.0), egui::Sense::click());
    let hovered = resp.hovered();
    if hovered {
        ui.painter().rect_filled(rect, 5.0, p.hover);
    }
    let color = if flashing {
        p.accent
    } else if hovered {
        p.text
    } else {
        p.faint
    };
    let glyph = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 11.0, rect.center().y),
        egui::vec2(12.0, 12.0),
    );
    paint_icon(ui.painter(), glyph, icon, color);
    if !text.is_empty() {
        ui.painter().galley_with_override_text_color(
            egui::pos2(rect.left() + 21.0, rect.center().y - galley.size().y / 2.0),
            galley,
            color,
        );
    }
    click_flash(ui, &resp, 5.0, p);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A spinning arc, for work in progress.
pub fn spinner(ui: &mut Ui, size: f32, color: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    paint_spinner(ui, rect, color);
    resp
}

pub fn paint_spinner(ui: &Ui, rect: egui::Rect, color: Color32) {
    let t = ui.input(|i| i.time) as f32;
    let r = rect.width().min(rect.height()) / 2.0 - 1.5;
    let start = t * 5.0;
    let pts: Vec<egui::Pos2> = (0..=20)
        .map(|i| {
            let a = start + i as f32 * (4.2 / 20.0);
            rect.center() + egui::vec2(a.cos(), a.sin()) * r
        })
        .collect();
    ui.painter()
        .add(egui::Shape::line(pts, Stroke::new(1.6, color)));
    ui.ctx().request_repaint();
}

/// A rounded label with a soft tint of `color`.
pub fn pill(ui: &mut Ui, text: &str, color: Color32) -> Response {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_string(), FontId::proportional(11.0), color);
    let size = egui::vec2(galley.size().x + 14.0, 19.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, 9.5, color.gamma_multiply(0.16));
    ui.painter().galley(
        egui::pos2(rect.left() + 7.0, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
    resp
}

/// A stable colour for a name (sub-agents in the terminal and agent views).
pub fn name_color(name: &str, p: &Palette) -> Color32 {
    if name == "main" {
        return p.accent;
    }
    const HUES: [(u8, u8, u8); 6] = [
        (94, 194, 135),
        (224, 175, 84),
        (196, 132, 236),
        (86, 190, 214),
        (236, 128, 160),
        (170, 196, 90),
    ];
    let hash = name
        .bytes()
        .fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    let (r, g, b) = HUES[(hash as usize) % HUES.len()];
    Color32::from_rgb(r, g, b)
}

/// The frame of a raised card.
pub fn card(p: &Palette) -> egui::Frame {
    egui::Frame::none()
        .fill(p.surface)
        .stroke(Stroke::new(1.0_f32, p.border))
        .rounding(10.0)
        .inner_margin(Margin::symmetric(12.0, 10.0))
}

/// Strip emoji and decorative symbols from agent-provided labels
/// ("🔧 Tool: x" → "Tool: x") so the chrome stays typographic.
pub fn clean_label(text: &str) -> String {
    let decorative = |c: char| {
        matches!(c as u32,
            0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0x2B00..=0x2BFF | 0xFE00..=0xFE0F | 0x200D | 0x2139)
    };
    let stripped: String = text.chars().filter(|c| !decorative(*c)).collect();
    stripped.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Capitalise the first letter of a phrase.
pub fn sentence_case(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_font_searches_nested_folders_in_preference_order() {
        let root = std::env::temp_dir().join(format!("norvexum_fonts_{}", std::process::id()));
        let nested = root.join("truetype").join("dejavu");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("DejaVuSans.ttf"), b"dejavu").unwrap();
        std::fs::write(root.join("Fallback.ttf"), b"fallback").unwrap();
        let dirs = vec![root.clone()];
        assert_eq!(
            find_font(&dirs, &["missing.ttf", "dejavusans.ttf", "Fallback.ttf"]).as_deref(),
            Some(&b"dejavu"[..])
        );
        assert!(find_font(&dirs, &["missing.ttf"]).is_none());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn clean_label_drops_emoji_and_extra_spaces() {
        assert_eq!(clean_label("🔧 Tool: read_file..."), "Tool: read_file...");
        assert_eq!(
            clean_label("ℹ️ Thinking (step 1/25)"),
            "Thinking (step 1/25)"
        );
        assert_eq!(clean_label("plain"), "plain");
    }
}
