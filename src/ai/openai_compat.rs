// ═══════════════════════════════════════════════════════════════════════════
// OpenAI-Compatible Client — Covers aicredits.in, OpenAI direct, and any
// OpenAI-compatible endpoint.
//
// Features:
//   • SSE streaming with live token output
//   • Parallel tool calling
//   • Thinking/reasoning token extraction
//   • Anti-bot headers for all HTTP requests
// ═══════════════════════════════════════════════════════════════════════════

use async_trait::async_trait;
use eyre::Result;
use futures::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;

use super::AiClient;
use super::types::*;
use crate::config::providers::ModelInfo;

pub struct OpenAiCompatClient {
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
    model_info: Option<ModelInfo>,
}

impl OpenAiCompatClient {
    pub fn new(
        base_url: String,
        api_key: String,
        model: String,
        model_info: Option<ModelInfo>,
    ) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .tcp_keepalive(Some(std::time::Duration::from_secs(30)))
            .pool_idle_timeout(Some(std::time::Duration::from_secs(10)))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            client,
            base_url,
            api_key,
            model,
            model_info,
        }
    }

    /// Convert our Message types to OpenAI API format.
    fn format_messages(&self, messages: &[Message]) -> Vec<serde_json::Value> {
        messages
            .iter()
            .map(|msg| {
                let role = match msg.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => "tool",
                };

                // Check if message has image content
                let has_image = msg
                    .content
                    .iter()
                    .any(|p| matches!(p, ContentPart::Image { .. }));

                if has_image {
                    // Multi-modal content array
                    let content_parts: Vec<serde_json::Value> = msg
                        .content
                        .iter()
                        .map(|part| match part {
                            ContentPart::Text { text } => json!({
                                "type": "text",
                                "text": text
                            }),
                            ContentPart::Image { data, mime_type } => json!({
                                "type": "image_url",
                                "image_url": {
                                    "url": format!("data:{};base64,{}", mime_type, data)
                                }
                            }),
                        })
                        .collect();

                    let mut obj = json!({
                        "role": role,
                        "content": content_parts
                    });

                    if let Some(id) = &msg.tool_call_id {
                        obj["tool_call_id"] = json!(id);
                    }
                    if !msg.tool_calls.is_empty() {
                        obj["tool_calls"] = json!(
                            msg.tool_calls
                                .iter()
                                .map(|call| json!({
                                    "id": call.id,
                                    "type": "function",
                                    "function": {
                                        "name": call.name,
                                        "arguments": Self::tool_arguments(call)
                                    }
                                }))
                                .collect::<Vec<_>>()
                        );
                    }
                    obj
                } else {
                    // Simple text content
                    let text = msg.text();
                    let mut obj = json!({
                        "role": role,
                        "content": text
                    });

                    if let Some(id) = &msg.tool_call_id {
                        obj["tool_call_id"] = json!(id);
                    }
                    if !msg.tool_calls.is_empty() {
                        obj["tool_calls"] = json!(
                            msg.tool_calls
                                .iter()
                                .map(|call| json!({
                                    "id": call.id,
                                    "type": "function",
                                    "function": {
                                        "name": call.name,
                                        "arguments": Self::tool_arguments(call)
                                    }
                                }))
                                .collect::<Vec<_>>()
                        );
                    }
                    obj
                }
            })
            .collect()
    }

    /// Convert tool definitions to OpenAI function format.
    fn format_tools(&self, tools: &[ToolDef]) -> Vec<serde_json::Value> {
        tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters
                    }
                })
            })
            .collect()
    }

    fn tool_arguments(call: &ToolCall) -> String {
        match &call.arguments {
            serde_json::Value::String(raw) => raw.clone(),
            value => serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string()),
        }
    }
}

// ── SSE response chunk types ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SseChunk {
    choices: Option<Vec<SseChoice>>,
    usage: Option<SseUsage>,
}

#[derive(Debug, Deserialize)]
struct SseChoice {
    delta: Option<SseDelta>,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SseDelta {
    content: Option<String>,
    #[serde(
        alias = "reasoning",
        alias = "thought",
        alias = "thoughts",
        alias = "thinking",
        alias = "thinking_content"
    )]
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<SseToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct SseToolCallDelta {
    index: Option<usize>,
    id: Option<String>,
    function: Option<SseFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct SseFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SseUsage {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
    total_tokens: Option<u32>,
}

#[async_trait]
impl AiClient for OpenAiCompatClient {
    async fn chat_stream(
        &self,
        request: AiRequest,
        tx: mpsc::UnboundedSender<AiStreamEvent>,
    ) -> Result<()> {
        let url = format!("{}/chat/completions", self.base_url);

        let mut body = json!({
            "model": self.model,
            "messages": self.format_messages(&request.messages),
            "stream": true,
            "temperature": request.temperature,
            "stream_options": { "include_usage": true },
            "include_reasoning": true,
        });

        if let Some(max_tokens) = request.max_tokens {
            body["max_tokens"] = json!(max_tokens);
        }

        if !request.tools.is_empty() {
            body["tools"] = json!(self.format_tools(&request.tools));
            body["tool_choice"] = json!("auto");
        }

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response.text().await.unwrap_or_default();
            let _ = tx.send(AiStreamEvent::Error(format!(
                "API error {}: {}",
                status, error_body
            )));
            return Ok(());
        }

        // Parse SSE stream
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut in_thinking = false;
        let mut think_buf = String::new();
        let mut scan_think_tags = true;
        let mut using_reasoning_field = false;

        // Track tool calls being assembled across chunks
        let mut pending_tool_calls: std::collections::HashMap<usize, ToolCall> =
            std::collections::HashMap::new();

        while let Some(chunk_result) = stream.next().await {
            let chunk = match chunk_result {
                Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                Err(e) => {
                    let _ = tx.send(AiStreamEvent::Error(format!("Stream error: {}", e)));
                    break;
                }
            };

            buffer.push_str(&chunk);

            // Process complete SSE lines
            while let Some(line_end) = buffer.find('\n') {
                let line = buffer[..line_end].trim().to_string();
                buffer = buffer[line_end + 1..].to_string();

                if line.is_empty() || line.starts_with(':') {
                    continue;
                }

                if line == "data: [DONE]" {
                    if !think_buf.is_empty() {
                        if in_thinking {
                            let _ = tx.send(AiStreamEvent::ThinkingDelta(think_buf.clone()));
                            let _ = tx.send(AiStreamEvent::ThinkingDone);
                        } else {
                            let _ = tx.send(AiStreamEvent::ContentDelta(think_buf.clone()));
                        }
                        think_buf.clear();
                    }
                    if in_thinking {
                        let _ = tx.send(AiStreamEvent::ThinkingDone);
                    }
                    // Emit any remaining pending tool calls
                    for (_, tc) in pending_tool_calls.drain() {
                        let _ = tx.send(AiStreamEvent::ToolCallComplete(tc));
                    }
                    let _ = tx.send(AiStreamEvent::Done {
                        finish_reason: "stop".into(),
                        usage: None,
                    });
                    return Ok(());
                }

                if let Some(data) = line.strip_prefix("data: ") {
                    let parsed: Result<SseChunk, _> = serde_json::from_str(data);
                    let sse = match parsed {
                        Ok(c) => c,
                        Err(_) => continue,
                    };

                    // Emit usage if present
                    if let Some(usage) = &sse.usage {
                        let _ = tx.send(AiStreamEvent::Done {
                            finish_reason: "stop".into(),
                            usage: Some(UsageStats {
                                prompt_tokens: usage.prompt_tokens.unwrap_or(0),
                                completion_tokens: usage.completion_tokens.unwrap_or(0),
                                total_tokens: usage.total_tokens.unwrap_or(0),
                            }),
                        });
                    }

                    if let Some(choices) = &sse.choices {
                        for choice in choices {
                            // Handle finish reason
                            if let Some(reason) = &choice.finish_reason {
                                if !think_buf.is_empty() {
                                    if in_thinking {
                                        let _ = tx.send(AiStreamEvent::ThinkingDelta(think_buf.clone()));
                                        let _ = tx.send(AiStreamEvent::ThinkingDone);
                                    } else {
                                        let _ = tx.send(AiStreamEvent::ContentDelta(think_buf.clone()));
                                    }
                                    think_buf.clear();
                                }
                                if in_thinking {
                                    let _ = tx.send(AiStreamEvent::ThinkingDone);
                                    in_thinking = false;
                                }
                                // Emit pending tool calls on finish
                                if reason == "tool_calls" {
                                    for (_, tc) in pending_tool_calls.drain() {
                                        let _ = tx.send(AiStreamEvent::ToolCallComplete(tc));
                                    }
                                }
                                continue;
                            }

                            if let Some(delta) = &choice.delta {
                                // ── Thinking / reasoning tokens ──────
                                if let Some(reasoning) = &delta.reasoning_content {
                                    if !reasoning.is_empty() {
                                        using_reasoning_field = true;
                                        if !in_thinking {
                                            in_thinking = true;
                                        }
                                        let _ = tx
                                            .send(AiStreamEvent::ThinkingDelta(reasoning.clone()));
                                    }
                                }

                                 // ── Content tokens ───────────────────
                                 if let Some(content) = &delta.content {
                                     if !content.is_empty() {
                                         if using_reasoning_field {
                                             if in_thinking {
                                                 let _ = tx.send(AiStreamEvent::ThinkingDone);
                                                 in_thinking = false;
                                             }
                                             let _ = tx.send(AiStreamEvent::ContentDelta(content.clone()));
                                         } else if !scan_think_tags {
                                             // Already past the thinking phase — emit as content
                                             let _ = tx.send(AiStreamEvent::ContentDelta(content.clone()));
                                         } else {
                                             think_buf.push_str(content);
                                             // Try to drain as much as we can safely classify.
                                             loop {
                                                 if !in_thinking {
                                                     if let Some(pos) = think_buf.find("<think>") {
                                                         if pos > 0 {
                                                             let _ = tx.send(AiStreamEvent::ContentDelta(
                                                                 think_buf[..pos].to_string()
                                                             ));
                                                         }
                                                         think_buf.drain(..pos + 7);
                                                         in_thinking = true;
                                                     } else if think_buf.len() > 7 {
                                                         // Keep the last 7 chars (could be start of "<think>")
                                                         let safe = think_buf.len() - 7;
                                                         let _ = tx.send(AiStreamEvent::ContentDelta(
                                                             think_buf[..safe].to_string()
                                                         ));
                                                         think_buf.drain(..safe);
                                                         break;
                                                     } else {
                                                         break; // wait for more data
                                                     }
                                                 } else {
                                                     if let Some(pos) = think_buf.find("</think>") {
                                                         if pos > 0 {
                                                             let _ = tx.send(AiStreamEvent::ThinkingDelta(
                                                                 think_buf[..pos].to_string()
                                                             ));
                                                         }
                                                         think_buf.drain(..pos + 8);
                                                         let _ = tx.send(AiStreamEvent::ThinkingDone);
                                                         in_thinking = false;
                                                         scan_think_tags = false; // models only emit one think block
                                                     } else if think_buf.len() > 8 {
                                                         let safe = think_buf.len() - 8;
                                                         let _ = tx.send(AiStreamEvent::ThinkingDelta(
                                                             think_buf[..safe].to_string()
                                                         ));
                                                         think_buf.drain(..safe);
                                                         break;
                                                     } else {
                                                         break;
                                                     }
                                                 }
                                             }
                                         }
                                     }
                                 }

                                // ── Tool calls ──────────────────────
                                if let Some(tool_calls) = &delta.tool_calls {
                                    if in_thinking {
                                        let _ = tx.send(AiStreamEvent::ThinkingDone);
                                        in_thinking = false;
                                    }
                                    for tc_delta in tool_calls {
                                        let idx = tc_delta.index.unwrap_or(0);

                                        let entry =
                                            pending_tool_calls.entry(idx).or_insert_with(|| {
                                                ToolCall {
                                                    id: String::new(),
                                                    name: String::new(),
                                                    arguments: serde_json::Value::Null,
                                                }
                                            });

                                        if let Some(id) = &tc_delta.id {
                                            entry.id = id.clone();
                                        }

                                        if let Some(func) = &tc_delta.function {
                                            if let Some(name) = &func.name {
                                                entry.name = name.clone();
                                                let _ = tx.send(AiStreamEvent::ToolCallStart(
                                                    entry.clone(),
                                                ));
                                            }
                                            if let Some(args) = &func.arguments {
                                                // Accumulate argument JSON
                                                let current = match &entry.arguments {
                                                    serde_json::Value::String(s) => s.clone(),
                                                    serde_json::Value::Null => String::new(),
                                                    other => other.to_string(),
                                                };
                                                let new_args = format!("{}{}", current, args);
                                                entry.arguments =
                                                    serde_json::Value::String(new_args.clone());

                                                let _ = tx.send(AiStreamEvent::ToolCallDelta {
                                                    id: entry.id.clone(),
                                                    arguments_delta: args.clone(),
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn supports_vision(&self) -> bool {
        self.model_info.as_ref().is_some_and(|m| m.multimodal)
    }

    fn supports_image_gen(&self) -> bool {
        self.model_info.as_ref().is_some_and(|m| m.image_gen)
    }

    fn provider_name(&self) -> &str {
        "openai_compat"
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_complete_tool_exchange() {
        let client = OpenAiCompatClient::new(
            "https://example.invalid".into(),
            "test".into(),
            "test".into(),
            None,
        );
        let call = ToolCall {
            id: "call-1".into(),
            name: "read_file".into(),
            arguments: json!({ "path": "README.md" }),
        };
        let messages = vec![
            Message::assistant_with_tool_calls("", vec![call]),
            Message::tool_result("call-1", "read_file", "contents"),
        ];
        let formatted = client.format_messages(&messages);

        assert_eq!(formatted[0]["tool_calls"][0]["id"], "call-1");
        assert_eq!(
            formatted[0]["tool_calls"][0]["function"]["name"],
            "read_file"
        );
        assert_eq!(formatted[1]["tool_call_id"], "call-1");
    }

    #[test]
    fn test_sse_delta_deserialization() {
        let cases = vec![
            (r#"{"content": "hello", "reasoning_content": "why"}"#, "why"),
            (r#"{"content": "hello", "reasoning": "why"}"#, "why"),
            (r#"{"content": "hello", "thought": "why"}"#, "why"),
        ];

        for (json_str, expected) in cases {
            let delta: SseDelta = serde_json::from_str(json_str).unwrap();
            assert_eq!(delta.reasoning_content.as_deref(), Some(expected));
        }
    }
}
