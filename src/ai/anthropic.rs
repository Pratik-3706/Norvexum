// ═══════════════════════════════════════════════════════════════════════════
// Anthropic Client — Anthropic Messages API with streaming
//
// Features:
//   • SSE streaming with live token output
//   • Thinking / extended thinking support
//   • Tool use (function calling)
//   • Vision (base64 image input)
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

const ANTHROPIC_API_VERSION: &str = "2023-06-01";

pub struct AnthropicClient {
    client: Client,
    api_key: String,
    model: String,
    model_info: Option<ModelInfo>,
}

impl AnthropicClient {
    pub fn new(api_key: String, model: String, model_info: Option<ModelInfo>) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .tcp_keepalive(Some(std::time::Duration::from_secs(30)))
            .pool_idle_timeout(Some(std::time::Duration::from_secs(10)))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            client,
            api_key,
            model,
            model_info,
        }
    }

    fn format_messages(&self, messages: &[Message]) -> (Option<String>, Vec<serde_json::Value>) {
        let mut system_prompt = None;
        let mut formatted = Vec::new();

        for msg in messages {
            match msg.role {
                Role::System => {
                    system_prompt = Some(msg.text());
                }
                Role::User => {
                    let content: Vec<serde_json::Value> = msg
                        .content
                        .iter()
                        .map(|part| match part {
                            ContentPart::Text { text } => json!({
                                "type": "text",
                                "text": text
                            }),
                            ContentPart::Image { data, mime_type } => json!({
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": mime_type,
                                    "data": data
                                }
                            }),
                        })
                        .collect();

                    formatted.push(json!({
                        "role": "user",
                        "content": content
                    }));
                }
                Role::Assistant => {
                    let mut content = Vec::new();
                    let text = msg.text();
                    if !text.is_empty() {
                        content.push(json!({ "type": "text", "text": text }));
                    }
                    content.extend(msg.tool_calls.iter().map(|call| {
                        json!({
                            "type": "tool_use",
                            "id": call.id,
                            "name": call.name,
                            "input": call.arguments
                        })
                    }));
                    formatted.push(json!({
                        "role": "assistant",
                        "content": content
                    }));
                }
                Role::Tool => {
                    formatted.push(json!({
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": msg.tool_call_id.clone().unwrap_or_default(),
                            "content": msg.text()
                        }]
                    }));
                }
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
}

// ── Anthropic SSE event types ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct AnthropicEvent {
    #[serde(rename = "type")]
    event_type: String,
    index: Option<usize>,
    content_block: Option<AnthropicContentBlock>,
    delta: Option<AnthropicDelta>,
    message: Option<AnthropicMessage>,
}

#[derive(Debug, Deserialize)]
struct AnthropicContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    id: Option<String>,
    name: Option<String>,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicDelta {
    #[serde(rename = "type")]
    delta_type: Option<String>,
    text: Option<String>,
    partial_json: Option<String>,
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicMessage {
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
}

#[async_trait]
impl AiClient for AnthropicClient {
    async fn chat_stream(
        &self,
        request: AiRequest,
        tx: mpsc::UnboundedSender<AiStreamEvent>,
    ) -> Result<()> {
        let url = "https://api.anthropic.com/v1/messages";

        let (system_prompt, messages) = self.format_messages(&request.messages);

        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "max_tokens": request.max_tokens.unwrap_or(8192),
            "stream": true,
            "temperature": request.temperature,
        });

        if let Some(sys) = system_prompt {
            body["system"] = json!(sys);
        }

        if !request.tools.is_empty() {
            body["tools"] = json!(self.format_tools(&request.tools));
        }

        let response = self
            .client
            .post(url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response.text().await.unwrap_or_default();
            let _ = tx.send(AiStreamEvent::Error(format!(
                "Anthropic API error {}: {}",
                status, error_body
            )));
            return Ok(());
        }

        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut in_thinking = false;
        let mut current_tool_id = String::new();
        let mut current_tool_name = String::new();
        let mut current_tool_args = String::new();

        while let Some(chunk_result) = stream.next().await {
            let chunk = match chunk_result {
                Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                Err(e) => {
                    let _ = tx.send(AiStreamEvent::Error(format!("Stream error: {}", e)));
                    break;
                }
            };

            buffer.push_str(&chunk);

            while let Some(line_end) = buffer.find('\n') {
                let line = buffer[..line_end].trim().to_string();
                buffer = buffer[line_end + 1..].to_string();

                if line.is_empty() || line.starts_with(':') {
                    continue;
                }

                // Anthropic uses "event: type\ndata: json" format
                if line.starts_with("event:") {
                    continue; // We parse the data line next
                }

                if let Some(data) = line.strip_prefix("data: ") {
                    let event: Result<AnthropicEvent, _> = serde_json::from_str(data);
                    let evt = match event {
                        Ok(e) => e,
                        Err(_) => continue,
                    };

                    match evt.event_type.as_str() {
                        "content_block_start" => {
                            if let Some(block) = &evt.content_block {
                                match block.block_type.as_str() {
                                    "thinking" => {
                                        in_thinking = true;
                                    }
                                    "tool_use" => {
                                        current_tool_id = block.id.clone().unwrap_or_default();
                                        current_tool_name = block.name.clone().unwrap_or_default();
                                        current_tool_args.clear();
                                        let tc = ToolCall {
                                            id: current_tool_id.clone(),
                                            name: current_tool_name.clone(),
                                            arguments: serde_json::Value::Null,
                                        };
                                        let _ = tx.send(AiStreamEvent::ToolCallStart(tc));
                                    }
                                    _ => {}
                                }
                            }
                        }
                        "content_block_delta" => {
                            if let Some(delta) = &evt.delta {
                                // Thinking delta
                                if in_thinking {
                                    if let Some(text) = &delta.text {
                                        if !text.is_empty() {
                                            let _ =
                                                tx.send(AiStreamEvent::ThinkingDelta(text.clone()));
                                        }
                                    }
                                }
                                // Text delta (content)
                                else if let Some(text) = &delta.text {
                                    if !text.is_empty() {
                                        let _ = tx.send(AiStreamEvent::ContentDelta(text.clone()));
                                    }
                                }
                                // Tool use input delta
                                if let Some(partial) = &delta.partial_json {
                                    current_tool_args.push_str(partial);
                                    let _ = tx.send(AiStreamEvent::ToolCallDelta {
                                        id: current_tool_id.clone(),
                                        arguments_delta: partial.clone(),
                                    });
                                }
                            }
                        }
                        "content_block_stop" => {
                            if in_thinking {
                                let _ = tx.send(AiStreamEvent::ThinkingDone);
                                in_thinking = false;
                            }
                            if !current_tool_id.is_empty() {
                                let args: serde_json::Value =
                                    serde_json::from_str(&current_tool_args).unwrap_or(json!({}));
                                let tc = ToolCall {
                                    id: current_tool_id.clone(),
                                    name: current_tool_name.clone(),
                                    arguments: args,
                                };
                                let _ = tx.send(AiStreamEvent::ToolCallComplete(tc));
                                current_tool_id.clear();
                                current_tool_name.clear();
                                current_tool_args.clear();
                            }
                        }
                        "message_delta" => {
                            if let Some(delta) = &evt.delta {
                                if let Some(reason) = &delta.stop_reason {
                                    let _ = tx.send(AiStreamEvent::Done {
                                        finish_reason: reason.clone(),
                                        usage: None,
                                    });
                                }
                            }
                        }
                        "message_stop" => {
                            // Final usage from message
                            if let Some(msg) = &evt.message {
                                if let Some(usage) = &msg.usage {
                                    let _ = tx.send(AiStreamEvent::Done {
                                        finish_reason: "end_turn".into(),
                                        usage: Some(UsageStats {
                                            prompt_tokens: usage.input_tokens.unwrap_or(0),
                                            completion_tokens: usage.output_tokens.unwrap_or(0),
                                            total_tokens: usage.input_tokens.unwrap_or(0)
                                                + usage.output_tokens.unwrap_or(0),
                                        }),
                                    });
                                }
                            }
                        }
                        _ => {}
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
        false
    }

    fn provider_name(&self) -> &str {
        "anthropic"
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_tool_use_precedes_tool_result() {
        let client = AnthropicClient::new("test".into(), "test".into(), None);
        let call = ToolCall {
            id: "call-1".into(),
            name: "read_file".into(),
            arguments: json!({ "path": "README.md" }),
        };
        let (_, messages) = client.format_messages(&[
            Message::assistant_with_tool_calls("", vec![call]),
            Message::tool_result("call-1", "read_file", "contents"),
        ]);

        assert_eq!(messages[0]["content"][0]["type"], "tool_use");
        assert_eq!(messages[0]["content"][0]["id"], "call-1");
        assert_eq!(messages[1]["content"][0]["type"], "tool_result");
        assert_eq!(messages[1]["content"][0]["tool_use_id"], "call-1");
    }
}
