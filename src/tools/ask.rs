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
         2. You finished a piece of work and there is an obvious follow-up the user will likely \
         want next, that you can do (for example commit the change you just made, run the app \
         you just built, fix the test that still fails): give 1-3 of them as `options` and leave \
         out `question`.\n\
         Don't offer next steps when the task is simply done (for example after a push or a \
         plain answer), and never offer filler like \"Nothing else\", \"Done\" or \"No thanks\": \
         the user can just ignore the list. Don't turn warnings or notes from tool results into \
         options. Calling this ends your turn: their pick arrives as their next message. Call it \
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
        let options = match clean_options(&args["options"], question.is_none()) {
            Ok(options) => options,
            // Only filler offered as next steps: nothing worth showing.
            Err(e) if question.is_none() && e == NO_REAL_OPTIONS => {
                return ToolResult::ok(
                    "Nothing was shown: those were not real next steps. Your turn is over; \
                     don't offer anything else.",
                );
            }
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

const NO_REAL_OPTIONS: &str =
    "Give at least one real option (\"Other\" and \"Nothing else\" are not options).";

/// The options as given, trimmed, without blanks, repeats, an "Other" (the UI
/// always adds one) or filler like "Nothing else" (ignoring the list does
/// that), at most [`MAX_OPTIONS`].
fn clean_options(value: &serde_json::Value, next_steps: bool) -> Result<Vec<String>, String> {
    let Some(items) = value.as_array() else {
        return Err("'options' must be an array of short strings".into());
    };
    let mut options: Vec<String> = Vec::new();
    for item in items {
        let Some(text) = item.as_str().map(str::trim).filter(|t| !t.is_empty()) else {
            continue;
        };
        let other = matches!(text.to_lowercase().as_str(), "other" | "something else");
        if other
            || (next_steps && is_filler(text))
            || options.iter().any(|o| o.eq_ignore_ascii_case(text))
        {
            continue;
        }
        options.push(crate::util::truncate_chars(text, MAX_OPTION_CHARS));
    }
    if options.is_empty() {
        return Err(NO_REAL_OPTIONS.into());
    }
    options.truncate(MAX_OPTIONS);
    Ok(options)
}

/// "Other", "Nothing else for now", "No thanks", "Done", … : answers that
/// mean "none of these", which typing or ignoring the list already covers.
fn is_filler(option: &str) -> bool {
    let lower = option
        .to_lowercase()
        .trim_end_matches(['.', '!', '…'])
        .trim()
        .to_string();
    let exact = [
        "other",
        "something else",
        "no",
        "nope",
        "none",
        "no thanks",
        "no, thanks",
        "done",
        "i'm done",
        "that's all",
        "that's it",
        "all good",
        "skip",
        "not now",
        "cancel",
        "stop here",
    ];
    exact.contains(&lower.as_str()) || lower.starts_with("nothing")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_are_cleaned_and_capped() {
        let options = clean_options(
            &json!([
                " Commit and push ",
                "",
                "commit and push",
                "Other",
                "Keep polishing",
                "Run the tests",
                "Write docs",
                "One too many"
            ]),
            true,
        )
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
        assert!(clean_options(&json!([]), true).is_err());
        // Filler is dropped; only filler means nothing to offer.
        assert_eq!(
            clean_options(
                &json!(["Run the app", "Nothing else for now", "No thanks.", "Done"]),
                true
            )
            .unwrap(),
            ["Run the app"]
        );
        assert!(clean_options(&json!(["Nothing else for now", "Other"]), true).is_err());
        // Answering a question, "No" is a real answer.
        assert_eq!(
            clean_options(
                &json!(["Yes, check my .env", "No, push without checking it"]),
                false
            )
            .unwrap()
            .len(),
            2
        );
        assert!(!is_filler("Note the API limits"));
        assert!(clean_options(&json!("Commit"), true).is_err());
    }
}
