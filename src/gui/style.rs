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
    pub bubble: Color32,
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
                bubble: rgb(33, 35, 40),
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
                bubble: rgb(238, 239, 242),
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
    style.spacing.scroll.bar_width = 6.0;
    style.spacing.scroll.floating = true;
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

/// Frameless text button; `active` marks a toggled-on panel.
pub fn nav_button(ui: &mut Ui, label: &str, active: bool, p: &Palette) -> Response {
    ui.add(
        egui::Button::new(RichText::new(label).size(12.5).color(if active {
            p.text
        } else {
            p.dim
        }))
        .fill(if active {
            p.hover
        } else {
            Color32::TRANSPARENT
        })
        .stroke(Stroke::NONE)
        .rounding(6.0),
    )
}

/// Small frameless action link (Copy, Edit, …).
pub fn link_button(ui: &mut Ui, label: &str, p: &Palette) -> Response {
    ui.add(
        egui::Button::new(RichText::new(label).size(11.5).color(p.faint))
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::NONE)
            .rounding(4.0),
    )
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
