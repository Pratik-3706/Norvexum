// ═══════════════════════════════════════════════════════════════════════════
// Agent Compaction — Context window management with auto-summarization
//
// Prevents context overflow by summarizing older messages when the
// conversation approaches 80% of the model's context window.
// ═══════════════════════════════════════════════════════════════════════════

use crate::ai::types::{ContentPart, Message, Role};

/// Rough token estimation: ~4 characters per token (industry standard heuristic).
pub fn estimate_tokens(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|m| {
            m.content
                .iter()
                .map(|p| match p {
                    ContentPart::Text { text } => text.len() / 4,
                    ContentPart::Image { data, .. } => data.len() / 100, // Images are heavily compressed in token count
                })
                .sum::<usize>()
                + m.tool_calls
                    .iter()
                    .map(|tc| tc.name.len() / 4 + tc.arguments.to_string().len() / 4)
                    .sum::<usize>()
        })
        .sum()
}

/// Check if we should compact the context.
/// Returns true when estimated tokens exceed `threshold_pct`% of the context window.
pub fn should_compact(messages: &[Message], context_window: usize, threshold_pct: usize) -> bool {
    if context_window == 0 {
        return false;
    }
    let estimated = estimate_tokens(messages);
    let threshold = context_window * threshold_pct / 100;
    estimated > threshold
}

/// Compact the conversation by summarizing old messages.
/// Keeps: system prompt (index 0), last `keep_recent` user+assistant exchanges,
/// and creates a summary message for everything in between.
pub fn compact(messages: &[Message], keep_recent: usize) -> Vec<Message> {
    if messages.len() <= keep_recent * 2 + 2 {
        // Not enough messages to compact
        return messages.to_vec();
    }

    let mut result = Vec::new();

    // Always keep the system prompt
    if let Some(system) = messages.first() {
        if system.role == Role::System {
            result.push(system.clone());
        }
    }

    // Find the boundary: keep last `keep_recent` exchanges (user + assistant pairs)
    let total = messages.len();
    let start_of_system = if matches!(messages.first().map(|m| m.role), Some(Role::System)) {
        1
    } else {
        0
    };

    // Count backwards to find keep boundary
    let keep_from = total.saturating_sub(keep_recent * 2);
    let keep_from = keep_from.max(start_of_system);

    // Summarize the middle section
    if keep_from > start_of_system {
        let middle = &messages[start_of_system..keep_from];
        let summary = build_summary(middle);
        result.push(Message::system(format!(
            "[Conversation Summary — older messages compacted]\n{}",
            summary
        )));
    }

    // Keep recent messages
    result.extend_from_slice(&messages[keep_from..]);

    result
}

/// Build a text summary of a slice of messages.
fn build_summary(messages: &[Message]) -> String {
    let mut summary = String::new();
    let mut msg_count = 0;
    let mut tool_calls_made = Vec::new();

    for msg in messages {
        match msg.role {
            Role::User => {
                msg_count += 1;
                let text = msg.text();
                let preview: String = text.chars().take(120).collect();
                summary.push_str(&format!("• User asked: {}", preview));
                if text.len() > 120 {
                    summary.push_str("...");
                }
                summary.push('\n');
            }
            Role::Assistant => {
                let text = msg.text();
                if !text.is_empty() {
                    let preview: String = text.chars().take(200).collect();
                    summary.push_str(&format!("• Assistant responded: {}", preview));
                    if text.len() > 200 {
                        summary.push_str("...");
                    }
                    summary.push('\n');
                }
                for tc in &msg.tool_calls {
                    tool_calls_made.push(tc.name.clone());
                }
            }
            Role::Tool => {
                // Summarize tool results briefly
                let text = msg.text();
                if let Some(name) = &msg.tool_name {
                    let preview: String = text.chars().take(80).collect();
                    summary.push_str(&format!("• Tool `{}` returned: {}", name, preview));
                    if text.len() > 80 {
                        summary.push_str("...");
                    }
                    summary.push('\n');
                }
            }
            _ => {}
        }
    }

    if !tool_calls_made.is_empty() {
        summary.push_str(&format!(
            "\nTools used in this section: {}\n",
            tool_calls_made.join(", ")
        ));
    }

    summary.push_str(&format!("({} exchanges compacted)\n", msg_count));
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_basic() {
        let messages = vec![
            Message::system("You are an assistant."),
            Message::user("Hello world, how are you doing today?"),
        ];
        let tokens = estimate_tokens(&messages);
        assert!(tokens > 0);
        assert!(tokens < 100);
    }

    #[test]
    fn should_compact_returns_false_when_small() {
        let messages = vec![
            Message::system("System prompt"),
            Message::user("Hello"),
            Message::assistant("Hi there!"),
        ];
        assert!(!should_compact(&messages, 100_000, 80));
    }

    #[test]
    fn compact_preserves_system_and_recent() {
        let mut messages = vec![Message::system("System prompt")];
        for i in 0..20 {
            messages.push(Message::user(format!("Question {}", i)));
            messages.push(Message::assistant(format!("Answer {}", i)));
        }
        let compacted = compact(&messages, 3);
        // Should have: system + summary + last 6 messages (3 pairs)
        assert!(compacted.len() <= 8);
        assert_eq!(compacted[0].role, Role::System);
        assert_eq!(compacted[0].text(), "System prompt");
    }
}
