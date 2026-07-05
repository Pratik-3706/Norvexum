// ═══════════════════════════════════════════════════════════════════════════
// Chat rendering helpers — Markdown rendering for terminal display
//
// Handles inline formatting, code blocks, tables, and structural elements.
// Used by the TUI's render_message() for rich text display.
// ═══════════════════════════════════════════════════════════════════════════

/// Format markdown text for terminal display.
/// Returns the processed text with terminal-friendly formatting markers.
pub fn format_markdown(text: &str) -> String {
    let mut output = String::new();
    let mut in_code_block = false;

    for line in text.lines() {
        let trimmed = line.trim_start();

        // Handle code block delimiters
        if trimmed.starts_with("```") {
            in_code_block = !in_code_block;
            if in_code_block {
                let lang = trimmed.strip_prefix("```").unwrap_or("");
                if !lang.is_empty() {
                    output.push_str(&format!("┌─ {} ─\n", lang));
                } else {
                    output.push_str("┌──────\n");
                }
            } else {
                output.push_str("└──────\n");
            }
            continue;
        }

        if in_code_block {
            output.push_str(&format!("│ {}\n", line));
            continue;
        }

        // Headers
        if let Some(heading) = trimmed.strip_prefix("### ") {
            output.push_str(&format!("   {}\n", heading.to_uppercase()));
        } else if let Some(heading) = trimmed.strip_prefix("## ") {
            output.push_str(&format!("  ═ {} ═\n", heading.to_uppercase()));
        } else if let Some(heading) = trimmed.strip_prefix("# ") {
            output.push_str(&format!(" ══ {} ══\n", heading.to_uppercase()));
        }
        // Blockquotes
        else if let Some(quote) = trimmed.strip_prefix("> ") {
            output.push_str(&format!("  │ {}\n", quote));
        }
        // Unordered lists
        else if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            output.push_str(&format!("  • {}\n", &trimmed[2..]));
        }
        // Horizontal rules
        else if trimmed.starts_with("---") || trimmed.starts_with("***") {
            output.push_str(&"─".repeat(40));
            output.push('\n');
        }
        // Regular text — process inline formatting
        else {
            output.push_str(&format_inline(line));
            output.push('\n');
        }
    }

    output
}

/// Process inline markdown formatting.
fn format_inline(text: &str) -> String {
    // Simple pass-through for now — inline formatting is handled
    // by the TUI's parse_inline_markdown() function with Span styles.
    // This function is for plain-text (headless) output.
    let mut result = text.to_string();
    
    // Strip markdown bold markers for plain text
    result = result.replace("**", "");
    
    // Strip inline code markers but keep the content
    // (backtick content is kept as-is in plain text)
    
    result
}

/// Check if a line is a table row (contains pipe characters).
pub fn is_table_row(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.matches('|').count() >= 2
}

/// Check if a line is a table separator (e.g., |---|---|).
pub fn is_table_separator(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('|')
        && trimmed.ends_with('|')
        && trimmed.chars().all(|c| c == '|' || c == '-' || c == ':' || c == ' ')
}

/// Parse a table row into cells.
pub fn parse_table_row(line: &str) -> Vec<String> {
    line.trim()
        .trim_start_matches('|')
        .trim_end_matches('|')
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}
