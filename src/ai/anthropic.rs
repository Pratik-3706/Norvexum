// ===========================================================================
// Anthropic Client — Anthropic Messages API with streaming
//
// Features:
//   • SSE streaming with live token output
//   • Adaptive / extended thinking, with signed thinking blocks preserved
//     across tool loops
//   • Tool use (function calling), including images inside tool results
//   • Vision (base64 image input)
//   • Prompt caching (top-level automatic cache breakpoint)
// ===========================================================================

use async_trait::async_trait;
use eyre::Result;
use futures::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;

use super::AiClient;
use super::sse::{SseLineBuffer, data_payload};
use super::types::*;
use crate::config::providers::ModelInfo;

const ANTHROPIC_API_VERSION: &str = "2023-06-01";
const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";

pub struct AnthropicClient {
    client: Client,
    api_key: String,
    model: String,
    model_info: Option<ModelInfo>,
}

/// `(major, minor)` of a Claude model id, e.g. `claude-sonnet-4-5-20250929` → (4, 5),
/// `claude-3-7-sonnet-latest` → (3, 7). Date suffixes are not mistaken for versions.
fn claude_version(model: &str) -> Option<(u32, u32)> {
    let re = regex::Regex::new(
        r"claude-(?:(?:opus|sonnet|haiku|fable|mythos)-)?(\d{1,2})(?:[-.](\d{1,2}))?(?:[^0-9]|$)",
    )
    .ok()?;
    let caps = re.captures(model)?;
    let major = caps.get(1)?.as_str().parse().ok()?;
    let minor = caps
        .get(2)
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(0);
    Some((major, minor))
}

/// Claude 3.7 and every 4.x+ model support extended thinking.
pub fn model_supports_thinking(model: &str) -> bool {
    let lower = model.to_lowercase();
    if lower.contains("fable") || lower.contains("mythos") {
        return true;
    }
    matches!(claude_version(&lower), Some((major, minor)) if major >= 4 || (major == 3 && minor >= 7))
}

/// Opus/Sonnet 4.6 and later use `thinking: {type: "adaptive"}` + `output_config.effort`;
/// older thinking models need an explicit `budget_tokens`.
fn uses_adaptive_thinking(model: &str) -> bool {
    let lower = model.to_lowercase();
    if lower.contains("fable") || lower.contains("mythos") {
        return true;
    }
    matches!(claude_version(&lower), Some((major, minor)) if major >= 5 || (major == 4 && minor >= 6))
}

/// From Claude 4.7 on, sampling parameters are rejected with a 400.
fn accepts_temperature(model: &str) -> bool {
    let lower = model.to_lowercase();
    if lower.contains("fable") || lower.contains("mythos") {
        return false;
    }
    match claude_version(&lower) {
        Some((major, minor)) => major < 4 || (major == 4 && minor <= 6),
        None => true,
    }
}

impl AnthropicClient {
    pub fn new(api_key: String, model: String, model_info: Option<ModelInfo>) -> Self {
        Self {
            client: super::streaming_http_client(),
            api_key,
            model,
            model_info,
        }
    }

    fn adaptive(&self) -> bool {
        self.model_info
            .as_ref()
            .is_some_and(|m| m.adaptive_thinking)
            || uses_adaptive_thinking(&self.model)
    }

    /// Convert history to Anthropic's format.
    ///
    /// * The first system message becomes the top-level `system` prompt; later
    ///   ones (compaction summaries, loop-limit warnings) are sent as user text
    ///   so they never overwrite it.
    /// * Consecutive same-role turns are merged — all tool results for one
    ///   assistant turn must arrive in a single user message.
    /// * Thinking blocks are only replayed for the turn still in progress
    ///   (after the last real user message); older ones are not needed.
    fn format_messages(&self, messages: &[Message]) -> (Option<String>, Vec<serde_json::Value>) {
        let mut system_prompt: Option<String> = None;
        let mut formatted: Vec<serde_json::Value> = Vec::new();

        let last_user_text = messages
            .iter()
            .rposition(|m| m.role == Role::User)
            .unwrap_or(0);

        for (idx, msg) in messages.iter().enumerate() {
            let (role, blocks): (&str, Vec<serde_json::Value>) = match msg.role {
                Role::System => {
                    if system_prompt.is_none() && formatted.is_empty() {
                        system_prompt = Some(msg.text());
                        continue;
                    }
                    let text = msg.text();
                    if text.trim().is_empty() {
                        continue;
                    }
                    (
                        "user",
                        vec![
                            json!({ "type": "text", "text": format!("<system-reminder>\n{text}\n</system-reminder>") }),
                        ],
                    )
                }
                Role::User => ("user", content_blocks(&msg.content)),
                Role::Assistant => {
                    let mut content = Vec::new();
                    if idx > last_user_text {
                        for block in &msg.thinking_blocks {
                            content.push(match &block.redacted_data {
                                Some(data) => json!({ "type": "redacted_thinking", "data": data }),
                                None => json!({
                                    "type": "thinking",
                                    "thinking": block.thinking,
                                    "signature": block.signature,
                                }),
                            });
                        }
                    }
                    let text = msg.text();
                    if !text.trim().is_empty() {
                        content.push(json!({ "type": "text", "text": text }));
                    }
                    content.extend(msg.tool_calls.iter().map(|call| {
                        let input = match &call.arguments {
                            serde_json::Value::Object(_) => call.arguments.clone(),
                            serde_json::Value::String(raw) => {
                                serde_json::from_str(raw).unwrap_or_else(|_| json!({}))
                            }
                            _ => json!({}),
                        };
                        json!({
                            "type": "tool_use",
                            "id": call.id,
                            "name": call.name,
                            "input": input
                        })
                    }));
                    ("assistant", content)
                }
                Role::Tool => {
                    let text = msg.text();
                    let mut inner = vec![json!({
                        "type": "text",
                        "text": if text.is_empty() { "(no output)".to_string() } else { text.clone() }
                    })];
                    for part in &msg.content {
                        if let ContentPart::Image { data, mime_type } = part {
                            inner.push(image_block(data, mime_type));
                        }
                    }
                    (
                        "user",
                        vec![json!({
                            "type": "tool_result",
                            "tool_use_id": msg.tool_call_id.clone().unwrap_or_default(),
                            "content": inner,
                            "is_error": text.starts_with("ERROR:"),
                        })],
                    )
                }
            };

            if blocks.is_empty() {
                continue;
            }
            match formatted.last_mut() {
                Some(last) if last["role"] == role => {
                    if let Some(arr) = last["content"].as_array_mut() {
                        arr.extend(blocks);
                    }
                }
                _ => formatted.push(json!({ "role": role, "content": blocks })),
            }
        }

        (system_prompt, formatted)
    }

    fn format_tools(&self, tools: &[ToolDef]) -> Vec<serde_json::Value> {
        tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters
                })
            })
            .collect()
    }

    fn build_body(&self, request: &AiRequest) -> serde_json::Value {
        let (system_prompt, messages) = self.format_messages(&request.messages);
        let adaptive = self.adaptive();

        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            // Automatic prompt caching: the stable system prompt + tool list + earlier
            // turns are billed at the cached rate on every subsequent loop.
            "cache_control": { "type": "ephemeral" },
        });

        let effort = request
            .thinking_effort
            .as_deref()
            .filter(|e| *e != "none" && !e.is_empty());
        let thinking_on = effort.is_some() && model_supports_thinking(&self.model);
        let default_max = if adaptive { 32_000 } else { 8_192 };
        let mut max_tokens = request.max_tokens.unwrap_or(default_max);

        if thinking_on {
            let effort = effort.unwrap_or("medium");
            if adaptive {
                body["thinking"] = json!({ "type": "adaptive", "display": "summarized" });
                if effort != "adaptive" {
                    body["output_config"] = json!({ "effort": effort });
                }
            } else {
                let budget: u32 = match effort {
                    "low" => 2_048,
                    "high" => 16_000,
                    "xhigh" | "max" => 24_000,
                    _ => 6_000,
                };
                // budget_tokens must be >= 1024 and strictly below max_tokens.
                max_tokens = max_tokens.max(budget + 4_096);
                body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
            }
        } else if accepts_temperature(&self.model) {
            body["temperature"] = json!(request.temperature);
        }

        body["max_tokens"] = json!(max_tokens);

        if let Some(sys) = system_prompt {
            body["system"] = json!(sys);
        }
        if !request.tools.is_empty() {
            body["tools"] = json!(self.format_tools(&request.tools));
        }
        body
    }
}

fn image_block(data: &str, mime_type: &str) -> serde_json::Value {
    json!({
        "type": "image",
        "source": { "type": "base64", "media_type": mime_type, "data": data }
    })
}

fn content_blocks(parts: &[ContentPart]) -> Vec<serde_json::Value> {
    parts
        .iter()
        .filter_map(|part| match part {
            // Empty text blocks are rejected by the API.
            ContentPart::Text { text } if text.trim().is_empty() => None,
            ContentPart::Text { text } => Some(json!({ "type": "text", "text": text })),
            ContentPart::Image { data, mime_type } => Some(image_block(data, mime_type)),
        })
        .collect()
}

// ── Anthropic SSE event types ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct AnthropicEvent {
    #[serde(rename = "type")]
    event_type: String,
    content_block: Option<AnthropicContentBlock>,
    delta: Option<AnthropicDelta>,
    message: Option<AnthropicMessage>,
    usage: Option<AnthropicUsage>,
    error: Option<AnthropicError>,
}

#[derive(Debug, Deserialize)]
struct AnthropicContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    id: Option<String>,
    name: Option<String>,
    data: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicDelta {
    text: Option<String>,
    thinking: Option<String>,
    signature: Option<String>,
    partial_json: Option<String>,
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicMessage {
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize, Default, Clone, Copy)]
struct AnthropicUsage {
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    cache_read_input_tokens: Option<u32>,
    cache_creation_input_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct AnthropicError {
    #[serde(rename = "type")]
    error_type: Option<String>,
    message: Option<String>,
}

/// The content block currently being streamed.
enum OpenBlock {
    None,
    Text,
    Thinking {
        text: String,
        signature: String,
    },
    Tool {
        id: String,
        name: String,
        args: String,
    },
}

#[async_trait]
impl AiClient for AnthropicClient {
    async fn chat_stream(
        &self,
        request: AiRequest,
        tx: mpsc::UnboundedSender<AiStreamEvent>,
    ) -> Result<()> {
        let body = self.build_body(&request);

        let response = self
            .client
            .post(ANTHROPIC_URL)
            .header("x-api-key", self.api_key.trim())
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let message = super::describe_http_error("Anthropic", response).await;
            let _ = tx.send(AiStreamEvent::Error(message));
            return Ok(());
        }

        let mut stream = response.bytes_stream();
        let mut lines = SseLineBuffer::new();
        let mut block = OpenBlock::None;
        let mut input_tokens = 0u32;
        let mut output_tokens = 0u32;
        let mut stop_reason = String::from("end_turn");

        'outer: while let Some(chunk_result) = stream.next().await {
            let chunk = match chunk_result {
                Ok(bytes) => bytes,
                Err(e) => {
                    let _ = tx.send(AiStreamEvent::Error(format!("Stream error: {e}")));
                    break;
                }
            };

            for line in lines.push(&chunk) {
                let Some(data) = data_payload(&line) else {
                    continue; // blank lines, comments and `event:` lines
                };
                let Ok(evt) = serde_json::from_str::<AnthropicEvent>(data) else {
                    continue;
                };

                match evt.event_type.as_str() {
                    "message_start" => {
                        if let Some(u) = evt.message.and_then(|m| m.usage) {
                            input_tokens = u.input_tokens.unwrap_or(0)
                                + u.cache_read_input_tokens.unwrap_or(0)
                                + u.cache_creation_input_tokens.unwrap_or(0);
                            output_tokens = u.output_tokens.unwrap_or(0);
                        }
                    }
                    "content_block_start" => {
                        let Some(cb) = evt.content_block else {
                            continue;
                        };
                        block = match cb.block_type.as_str() {
                            "thinking" => OpenBlock::Thinking {
                                text: String::new(),
                                signature: String::new(),
                            },
                            "redacted_thinking" => {
                                let _ = tx.send(AiStreamEvent::ThinkingBlock(ThinkingBlock {
                                    thinking: String::new(),
                                    signature: String::new(),
                                    redacted_data: cb.data,
                                }));
                                OpenBlock::None
                            }
                            "tool_use" => {
                                let id = cb.id.unwrap_or_default();
                                let name = cb.name.unwrap_or_default();
                                let _ = tx.send(AiStreamEvent::ToolCallStart(ToolCall {
                                    id: id.clone(),
                                    name: name.clone(),
                                    arguments: serde_json::Value::Null,
                                    thought_signature: None,
                                }));
                                OpenBlock::Tool {
                                    id,
                                    name,
                                    args: String::new(),
                                }
                            }
                            _ => OpenBlock::Text,
                        };
                    }
                    "content_block_delta" => {
                        let Some(delta) = evt.delta else { continue };
                        match &mut block {
                            OpenBlock::Thinking { text, signature } => {
                                if let Some(t) = delta.thinking.filter(|t| !t.is_empty()) {
                                    text.push_str(&t);
                                    let _ = tx.send(AiStreamEvent::ThinkingDelta(t));
                                }
                                if let Some(sig) = delta.signature {
                                    signature.push_str(&sig);
                                }
                            }
                            OpenBlock::Tool { id, args, .. } => {
                                if let Some(partial) = delta.partial_json {
                                    args.push_str(&partial);
                                    let _ = tx.send(AiStreamEvent::ToolCallDelta {
                                        id: id.clone(),
                                        arguments_delta: partial,
                                    });
                                }
                            }
                            OpenBlock::Text | OpenBlock::None => {
                                if let Some(text) = delta.text.filter(|t| !t.is_empty()) {
                                    let _ = tx.send(AiStreamEvent::ContentDelta(text));
                                }
                            }
                        }
                    }
                    "content_block_stop" => {
                        match std::mem::replace(&mut block, OpenBlock::None) {
                            OpenBlock::Thinking { text, signature } => {
                                let _ = tx.send(AiStreamEvent::ThinkingDone);
                                let _ = tx.send(AiStreamEvent::ThinkingBlock(ThinkingBlock {
                                    thinking: text,
                                    signature,
                                    redacted_data: None,
                                }));
                            }
                            OpenBlock::Tool { id, name, args } => {
                                let arguments = if args.trim().is_empty() {
                                    json!({})
                                } else {
                                    // Keep the raw string if it doesn't parse (e.g. cut off by
                                    // max_tokens) so the agent can report it to the model.
                                    serde_json::from_str(&args)
                                        .unwrap_or(serde_json::Value::String(args))
                                };
                                let _ = tx.send(AiStreamEvent::ToolCallComplete(ToolCall {
                                    id,
                                    name,
                                    arguments,
                                    thought_signature: None,
                                }));
                            }
                            OpenBlock::Text | OpenBlock::None => {}
                        }
                    }
                    "message_delta" => {
                        if let Some(reason) = evt.delta.and_then(|d| d.stop_reason) {
                            stop_reason = reason;
                        }
                        if let Some(u) = evt.usage {
                            output_tokens = u.output_tokens.unwrap_or(output_tokens);
                        }
                    }
                    "message_stop" => break 'outer,
                    "error" => {
                        let (kind, message) = evt
                            .error
                            .map(|e| {
                                (
                                    e.error_type.unwrap_or_default(),
                                    e.message.unwrap_or_default(),
                                )
                            })
                            .unwrap_or_default();
                        let _ = tx.send(AiStreamEvent::Error(format!(
                            "Anthropic stream error ({kind}): {message}"
                        )));
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }

        if stop_reason == "refusal" {
            let _ = tx.send(AiStreamEvent::ContentDelta(
                "\n\n⚠️ The model declined to continue this request.".into(),
            ));
        }

        let _ = tx.send(AiStreamEvent::Done {
            finish_reason: stop_reason,
            usage: Some(UsageStats {
                prompt_tokens: input_tokens,
                completion_tokens: output_tokens,
                total_tokens: input_tokens + output_tokens,
                reasoning_tokens: None,
            }),
        });
        Ok(())
    }

    fn supports_vision(&self) -> bool {
        match self.model_info.as_ref().map(|m| m.vision) {
            Some(crate::config::providers::Vision::No) => false,
            // Every Claude model since Claude 3 accepts images.
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(model: &str) -> AnthropicClient {
        AnthropicClient::new("test".into(), model.into(), None)
    }

    fn call() -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: "read_file".into(),
            arguments: json!({ "path": "README.md" }),
            thought_signature: None,
        }
    }

    #[test]
    fn assistant_tool_use_precedes_tool_result() {
        let (_, messages) = client("test").format_messages(&[
            Message::assistant_with_tool_calls("", vec![call()]),
            Message::tool_result("call-1", "read_file", "contents"),
        ]);

        assert_eq!(messages[0]["content"][0]["type"], "tool_use");
        assert_eq!(messages[0]["content"][0]["id"], "call-1");
        assert_eq!(messages[1]["content"][0]["type"], "tool_result");
        assert_eq!(messages[1]["content"][0]["tool_use_id"], "call-1");
    }

    #[test]
    fn later_system_messages_do_not_replace_the_system_prompt() {
        let (system, messages) = client("test").format_messages(&[
            Message::system("main prompt"),
            Message::system("[summary]"),
            Message::user("hi"),
        ]);
        assert_eq!(system.as_deref(), Some("main prompt"));
        // Summary and user text are merged into one user turn.
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn parallel_tool_results_share_one_user_message() {
        let mut second = call();
        second.id = "call-2".into();
        let (_, messages) = client("test").format_messages(&[
            Message::user("go"),
            Message::assistant_with_tool_calls("", vec![call(), second]),
            Message::tool_result("call-1", "read_file", "a"),
            Message::tool_result("call-2", "read_file", "b"),
        ]);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn thinking_blocks_replayed_only_for_current_turn() {
        let mut old = Message::assistant("old answer");
        old.thinking_blocks = vec![ThinkingBlock {
            thinking: "old".into(),
            signature: "s0".into(),
            redacted_data: None,
        }];
        let mut current = Message::assistant_with_tool_calls("", vec![call()]);
        current.thinking_blocks = vec![ThinkingBlock {
            thinking: "plan".into(),
            signature: "s1".into(),
            redacted_data: None,
        }];
        let (_, messages) = client("claude-sonnet-4-5").format_messages(&[
            Message::user("first"),
            old,
            Message::user("second"),
            current,
            Message::tool_result("call-1", "read_file", "x"),
        ]);
        assert_eq!(messages[1]["content"][0]["type"], "text");
        assert_eq!(messages[3]["content"][0]["type"], "thinking");
        assert_eq!(messages[3]["content"][0]["signature"], "s1");
    }

    #[test]
    fn thinking_request_shape_depends_on_model_generation() {
        let req =
            AiRequest::new(vec![Message::user("hi")]).with_thinking_effort(Some("high".into()));

        let modern = client("claude-opus-5-5").build_body(&req);
        assert_eq!(modern["thinking"]["type"], "adaptive");
        assert_eq!(modern["output_config"]["effort"], "high");
        assert!(modern.get("temperature").is_none());
        assert!(modern["thinking"].get("budget_tokens").is_none());

        let legacy = client("claude-sonnet-4-5-20250929").build_body(&req);
        assert_eq!(legacy["thinking"]["type"], "enabled");
        let budget = legacy["thinking"]["budget_tokens"].as_u64().unwrap();
        assert!(legacy["max_tokens"].as_u64().unwrap() > budget);

        let plain = client("claude-3-5-haiku-latest")
            .build_body(&AiRequest::new(vec![Message::user("hi")]));
        assert!(plain.get("thinking").is_none());
        assert!(plain.get("temperature").is_some());
    }

    #[test]
    fn version_parsing_ignores_date_suffixes() {
        assert_eq!(claude_version("claude-sonnet-4-20250514"), Some((4, 0)));
        assert_eq!(claude_version("claude-sonnet-4-5-20250929"), Some((4, 5)));
        assert_eq!(claude_version("claude-3-7-sonnet-latest"), Some((3, 7)));
        assert_eq!(claude_version("claude-haiku-4.5"), Some((4, 5)));
    }
}
