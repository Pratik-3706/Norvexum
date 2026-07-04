// ═══════════════════════════════════════════════════════════════════════════
// Theme — Color system for the TUI
// ═══════════════════════════════════════════════════════════════════════════

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
        Self {
            bg: Color::Rgb(15, 17, 26),                 // Deep navy
            fg: Color::Rgb(220, 225, 235),              // Light gray
            accent_color: Color::Rgb(0, 210, 190),      // Teal/cyan
            border_color: Color::Rgb(60, 65, 80),       // Subtle border
            user_color: Color::Rgb(100, 180, 255),      // Blue
            assistant_color: Color::Rgb(200, 220, 240), // Light blue-white
            thinking_color: Color::Rgb(160, 130, 210),  // Purple
            tool_color: Color::Rgb(255, 190, 60),       // Amber
            success_color: Color::Rgb(80, 220, 120),    // Green
            error_color: Color::Rgb(255, 90, 90),       // Red
            file_color: Color::Rgb(120, 210, 160),      // Mint green
            input_color: Color::Rgb(230, 235, 245),     // Bright white
            dim_color: Color::Rgb(90, 95, 110),         // Dimmed text
        }
    }
}
