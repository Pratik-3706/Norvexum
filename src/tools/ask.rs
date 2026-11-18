// ===========================================================================
// ask_user — answers the user can pick instead of typing
//
// Two uses: a question with options when a request is ambiguous, and next
// steps (commit, push, keep polishing, …) after a piece of work. The UI shows
// the options as a numbered list with "type your own" as the last choice; a
// pick is sent as the user's next message. Calling it ends the agent's turn.
// ===========================================================================

use async_trait::async_trait;
use serde_json::json;

use super::{Tool, ToolContext, ToolResult};
use crate::agent::AgentEvent;

pub const NAME: &str = "ask_user";
/// More than this is a menu, not a choice.
pub const MAX_OPTIONS: usize = 4;
/// Options are labels for a button, not paragraphs.
const MAX_OPTION_CHARS: usize = 80;

pub struct AskUserTool;

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        "Offer the user answers to pick instead of typing. Use it in two cases:\n\
         1. The request is ambiguous and the answer changes what you would do: give a short \
         `question` and 2-4 concrete `options`. Ask instead of guessing.\n\
         2. You finished a piece of work and there are clear next steps (for example commit, \
         push, keep polishing, run it): give them as `options` and leave out `question`.\n\
         The user can always type their own answer instead. Calling this ends your turn: their \
         pick arrives as their next message. Don't use it for trivial confirmations, and call it \
         at most once per turn."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question, in one short sentence. Leave out when offering next steps."
                },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "1-4 short answers, each a few words (e.g. \"Commit and push\", \"Keep polishing\"). Don't add an \"Other\" option: one is always shown."
                }
            },
            "required": ["options"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let question = args["question"]
            .as_str()
            .map(|q| q.trim().to_string())
            .filter(|q| !q.is_empty());
        let options = match clean_options(&args["options"]) {
            Ok(options) => options,
            Err(e) => return ToolResult::err(e),
        };
        if let Some(events) = &ctx.events {
            let _ = events.send(AgentEvent::Choices {
                question: question.clone(),
                options: options.clone(),
            });
        }
        let listed: Vec<String> = options
            .iter()
            .enumerate()
            .map(|(i, o)| format!("{}. {o}", i + 1))
            .collect();
        ToolResult::ok(format!(
            "{}Shown to the user:\n{}\nThey can also type their own answer. Stop here: their \
             choice arrives as their next message.",
            question
                .map(|q| format!("Asked: {q}\n"))
                .unwrap_or_default(),
            listed.join("\n")
        ))
    }
}

/// The options as given, trimmed, without blanks, repeats or an "Other"
/// (the UI always adds one), at most [`MAX_OPTIONS`].
fn clean_options(value: &serde_json::Value) -> Result<Vec<String>, String> {
    let Some(items) = value.as_array() else {
        return Err("'options' must be an array of short strings".into());
    };
    let mut options: Vec<String> = Vec::new();
    for item in items {
        let Some(text) = item.as_str().map(str::trim).filter(|t| !t.is_empty()) else {
            continue;
        };
        let lower = text.to_lowercase();
        if matches!(
            lower.as_str(),
            "other" | "other…" | "other..." | "something else"
        ) {
            continue;
        }
        if options.iter().any(|o| o.eq_ignore_ascii_case(text)) {
            continue;
        }
        options.push(crate::util::truncate_chars(text, MAX_OPTION_CHARS));
    }
    if options.is_empty() {
        return Err("Give at least one option.".into());
    }
    options.truncate(MAX_OPTIONS);
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_are_cleaned_and_capped() {
        let options = clean_options(&json!([
            " Commit and push ",
            "",
            "commit and push",
            "Other",
            "Keep polishing",
            "Run the tests",
            "Write docs",
            "One too many"
        ]))
        .unwrap();
        assert_eq!(
            options,
            [
                "Commit and push",
                "Keep polishing",
                "Run the tests",
                "Write docs"
            ]
        );
        assert!(clean_options(&json!([])).is_err());
        assert!(clean_options(&json!("Commit")).is_err());
    }
}
