// ===========================================================================
// Agent Compaction — Context window management
//
// Two tools, cheapest first:
//   • `prune_tool_results` shortens old tool outputs in place (file dumps,
//     command logs) — usually the bulk of the context.
//   • `compact` replaces older exchanges with a short summary.
//
// Both keep every assistant tool call paired with its tool results; APIs
// reject a `tool` message whose call was summarized away.
// ===========================================================================

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
                    // Providers bill images by size; ~1.5k tokens is typical after resizing.
                    ContentPart::Image { .. } => 1_500,
                })
                .sum::<usize>()
                + m.tool_calls
                    .iter()
                    .map(|tc| (tc.name.len() + tc.arguments.to_string().len()) / 4)
                    .sum::<usize>()
                + m.thinking_blocks
                    .iter()
                    .map(|b| b.thinking.len() / 4)
                    .sum::<usize>()
        })
        .sum()
}

/// Returns true when estimated tokens exceed `threshold_pct`% of the context window.
pub fn should_compact(messages: &[Message], context_window: usize, threshold_pct: usize) -> bool {
    if context_window == 0 {
        return false;
    }
    estimate_tokens(messages) > context_window * threshold_pct / 100
}

/// Shorten the text of every tool result except the `keep_recent` most recent
/// ones to at most `max_chars` (head and tail kept). Returns how many changed.
pub fn prune_tool_results(messages: &mut [Message], keep_recent: usize, max_chars: usize) -> usize {
    let tool_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == Role::Tool)
        .map(|(i, _)| i)
        .collect();
    let prune_count = tool_indices.len().saturating_sub(keep_recent);

    let mut changed = 0;
    for &idx in &tool_indices[..prune_count] {
        let msg = &mut messages[idx];
        let mut touched = false;
        for part in &mut msg.content {
            if let ContentPart::Text { text } = part {
                if text.len() > max_chars {
                    *text = format!(
                        "{}\n[older tool output shortened to save context]",
                        crate::util::truncate_middle(text, max_chars)
                    );
                    touched = true;
                }
            }
        }
        let before = msg.content.len();
        msg.strip_images();
        if touched || msg.content.len() != before {
            changed += 1;
        }
    }
    changed
}

/// Upper bound for the message content of one request. Gateways cap the body
/// size independently of the context window (AICredits rejects anything over
/// 5 MiB); this leaves room for the tool schemas and JSON overhead.
pub const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;

/// Browser screenshots kept per request. Older ones show a page that has
/// changed since, and each costs ~100–300 KB on every later request.
const KEEP_SCREENSHOTS: usize = 2;

/// Bytes of message content a request carries (text, images, tool calls).
pub fn content_bytes(messages: &[Message]) -> usize {
    messages.iter().map(message_bytes).sum()
}

fn message_bytes(m: &Message) -> usize {
    m.content
        .iter()
        .map(|p| match p {
            ContentPart::Text { text } => text.len(),
            ContentPart::Image { data, .. } => data.len(),
        })
        .sum::<usize>()
        + m.tool_calls
            .iter()
            .map(|tc| tc.name.len() + tc.arguments.to_string().len())
            .sum::<usize>()
        + m.thinking_blocks
            .iter()
            .map(|b| b.thinking.len())
            .sum::<usize>()
}

/// Replace a message's images with a short note saying why they are gone.
fn drop_images(msg: &mut Message, note: &str) -> bool {
    let before = msg.content.len();
    msg.strip_images();
    if msg.content.len() == before {
        return false;
    }
    msg.content.push(ContentPart::Text {
        text: note.to_string(),
    });
    true
}

/// Fit a request under [`MAX_REQUEST_BYTES`], least useful content first:
/// browser screenshots beyond the newest few, then images oldest first, then
/// old tool output. Meant for the copy sent to the provider, not the history.
pub fn fit_request_size(messages: &mut [Message]) {
    let mut screenshots = 0;
    for msg in messages.iter_mut().rev() {
        let is_screenshot = msg.role == Role::Tool
            && msg
                .tool_name
                .as_deref()
                .is_some_and(|n| crate::tools::canonical_tool_name(n) == "browser_interact")
            && msg
                .content
                .iter()
                .any(|p| matches!(p, ContentPart::Image { .. }));
        if is_screenshot {
            screenshots += 1;
            if screenshots > KEEP_SCREENSHOTS {
                drop_images(
                    msg,
                    "[Older screenshot removed; later ones show the current page.]",
                );
            }
        }
    }

    let mut total = content_bytes(messages);
    for msg in messages.iter_mut() {
        if total <= MAX_REQUEST_BYTES {
            return;
        }
        let before = message_bytes(msg);
        if drop_images(
            msg,
            "[Image removed to keep the request under the provider's size limit.]",
        ) {
            total = total - before + message_bytes(msg);
        }
    }

    if total > MAX_REQUEST_BYTES {
        prune_tool_results(messages, 6, 1_500);
    }
    if content_bytes(messages) > MAX_REQUEST_BYTES {
        prune_tool_results(messages, 0, 400);
    }
}

/// Compact the conversation by summarizing old messages.
/// Keeps: the system prompt, roughly the last `keep_recent` user exchanges,
/// and a summary message for everything in between.
pub fn compact(messages: &[Message], keep_recent: usize) -> Vec<Message> {
    let start = usize::from(messages.first().is_some_and(|m| m.role == Role::System));

    // Safe cut points are user messages: everything before them is a complete
    // exchange, so no tool result can be orphaned.
    let user_starts: Vec<usize> = (start..messages.len())
        .filter(|&i| messages[i].role == Role::User)
        .collect();
    if user_starts.len() <= keep_recent {
        return messages.to_vec();
    }
    let keep_from = user_starts[user_starts.len() - keep_recent.max(1)];
    if keep_from <= start {
        return messages.to_vec();
    }

    let mut result = Vec::with_capacity(messages.len() - keep_from + 2);
    if start == 1 {
        result.push(messages[0].clone());
    }
    result.push(Message::system(format!(
        "[Conversation summary — older messages were compacted to save context]\n{}",
        build_summary(&messages[start..keep_from])
    )));
    result.extend_from_slice(&messages[keep_from..]);
    result
}

/// Build a text summary of a slice of messages.
fn build_summary(messages: &[Message]) -> String {
    let mut summary = String::new();
    let mut exchanges = 0;
    let mut tool_calls_made: Vec<String> = Vec::new();
    let mut files_touched: Vec<String> = Vec::new();

    for msg in messages {
        match msg.role {
            Role::User => {
                exchanges += 1;
                summary.push_str(&format!(
                    "• User asked: {}\n",
                    crate::util::truncate_chars(msg.text().trim(), 200)
                ));
            }
            Role::Assistant => {
                let text = msg.text();
                if !text.trim().is_empty() {
                    summary.push_str(&format!(
                        "• Assistant: {}\n",
                        crate::util::truncate_chars(text.trim(), 300)
                    ));
                }
                for tc in &msg.tool_calls {
                    tool_calls_made.push(tc.name.clone());
                    let args = match &tc.arguments {
                        serde_json::Value::String(s) => serde_json::from_str(s).unwrap_or_default(),
                        other => other.clone(),
                    };
                    if let Some(path) = args["path"].as_str() {
                        if matches!(tc.name.as_str(), "write_file" | "edit_file" | "multi_edit")
                            && !files_touched.iter().any(|f| f == path)
                        {
                            files_touched.push(path.to_string());
                        }
                    }
                }
            }
            Role::Tool => {
                let text = msg.text();
                if text.starts_with("ERROR") {
                    summary.push_str(&format!(
                        "• Tool `{}` failed: {}\n",
                        msg.tool_name.as_deref().unwrap_or("tool"),
                        crate::util::truncate_chars(text.trim(), 120)
                    ));
                }
            }
            Role::System => {
                // An earlier summary: carry it forward.
                let text = msg.text();
                if text.starts_with("[Conversation summary") {
                    summary.push_str(text.lines().skip(1).collect::<Vec<_>>().join("\n").trim());
                    summary.push('\n');
                }
            }
        }
    }

    if !files_touched.is_empty() {
        summary.push_str(&format!("\nFiles modified: {}\n", files_touched.join(", ")));
    }
    if !tool_calls_made.is_empty() {
        tool_calls_made.sort();
        tool_calls_made.dedup();
        summary.push_str(&format!("Tools used: {}\n", tool_calls_made.join(", ")));
    }
    summary.push_str(&format!("({exchanges} exchanges compacted)\n"));
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::types::ToolCall;
    use serde_json::json;

    fn screenshot(id: &str, kb: usize) -> Message {
        Message::tool_result(id, "browser_interact", "Action 'click' completed.")
            .with_image("x".repeat(kb * 1024), "image/jpeg".into())
    }

    fn image_count(messages: &[Message]) -> usize {
        messages
            .iter()
            .flat_map(|m| &m.content)
            .filter(|p| matches!(p, ContentPart::Image { .. }))
            .count()
    }

    #[test]
    fn requests_keep_only_the_newest_screenshots() {
        let mut messages = vec![Message::system("sys"), Message::user("test the page")];
        for i in 0..6 {
            messages.push(screenshot(&format!("c{i}"), 200));
        }
        fit_request_size(&mut messages);
        assert_eq!(image_count(&messages), KEEP_SCREENSHOTS);
        // The newest two keep their images; older ones say why theirs went.
        assert!(matches!(
            messages[7].content.last(),
            Some(ContentPart::Image { .. })
        ));
        assert!(messages[2].text().contains("Older screenshot removed"));
    }

    #[test]
    fn oversized_requests_drop_images_oldest_first() {
        let big =
            |kb: usize| Message::user_with_image("look", "x".repeat(kb * 1024), "image/png".into());
        let mut messages = vec![Message::system("sys"), big(1_800), big(1_800), big(1_800)];
        assert!(content_bytes(&messages) > MAX_REQUEST_BYTES);
        fit_request_size(&mut messages);
        assert!(content_bytes(&messages) <= MAX_REQUEST_BYTES);
        assert_eq!(image_count(&messages), 2);
        assert!(messages[1].text().contains("size limit"));
    }

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
            messages.push(Message::user(format!("Question {i}")));
            messages.push(Message::assistant(format!("Answer {i}")));
        }
        let compacted = compact(&messages, 3);
        // system + summary + last 3 exchanges
        assert_eq!(compacted.len(), 8);
        assert_eq!(compacted[0].text(), "System prompt");
        assert_eq!(compacted[2].text(), "Question 17");
    }

    #[test]
    fn compact_never_orphans_tool_results() {
        let call = |id: &str| ToolCall {
            id: id.into(),
            name: "read_file".into(),
            arguments: json!({"path": "a.rs"}),
            thought_signature: None,
        };
        let mut messages = vec![Message::system("sys")];
        for i in 0..6 {
            messages.push(Message::user(format!("task {i}")));
            messages.push(Message::assistant_with_tool_calls(
                "",
                vec![call(&format!("c{i}"))],
            ));
            messages.push(Message::tool_result(
                format!("c{i}"),
                "read_file",
                "x".repeat(50),
            ));
            messages.push(Message::assistant("done"));
        }
        let compacted = compact(&messages, 2);
        for (i, msg) in compacted.iter().enumerate() {
            if msg.role == Role::Tool {
                let id = msg.tool_call_id.as_deref().unwrap();
                assert!(
                    compacted[..i]
                        .iter()
                        .any(|m| m.tool_calls.iter().any(|tc| tc.id == id)),
                    "tool result {id} lost its call"
                );
            }
        }
        assert_eq!(compacted[2].role, Role::User);
    }

    #[test]
    fn prune_keeps_recent_tool_outputs() {
        let mut messages = vec![
            Message::tool_result("a", "read_file", "a".repeat(5000)),
            Message::tool_result("b", "read_file", "b".repeat(5000)),
        ];
        assert_eq!(prune_tool_results(&mut messages, 1, 500), 1);
        assert!(messages[0].text().len() < 1000);
        assert_eq!(messages[1].text().len(), 5000);
    }
}
