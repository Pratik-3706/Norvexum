// ===========================================================================
// Theme — Color system for the TUI
// ===========================================================================

use ratatui::style::Color;

#[derive(Debug, Clone)]
pub struct Theme {
    pub bg: Color,
    pub fg: Color,
    pub accent_color: Color,
    pub border_color: Color,
    pub user_color: Color,
    pub assistant_color: Color,
    pub thinking_color: Color,
    pub tool_color: Color,
    pub success_color: Color,
    pub error_color: Color,
    pub file_color: Color,
    pub input_color: Color,
    pub dim_color: Color,
}

impl Default for Theme {
    fn default() -> Self {
        // Neutral greys with one blue accent (matches the desktop app).
        Self {
            bg: Color::Rgb(17, 18, 20),
            fg: Color::Rgb(228, 229, 231),
            accent_color: Color::Rgb(122, 146, 240),
            border_color: Color::Rgb(48, 50, 56),
            user_color: Color::Rgb(184, 188, 196),
            assistant_color: Color::Rgb(214, 216, 220),
            thinking_color: Color::Rgb(140, 144, 152),
            tool_color: Color::Rgb(224, 175, 84),
            success_color: Color::Rgb(94, 194, 135),
            error_color: Color::Rgb(232, 98, 98),
            file_color: Color::Rgb(110, 190, 150),
            input_color: Color::Rgb(236, 237, 239),
            dim_color: Color::Rgb(107, 110, 118),
        }
    }
}
