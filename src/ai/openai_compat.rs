// ===========================================================================
// OpenAI-Compatible Client — Covers aicredits.in, OpenAI direct, Ollama, and
// any OpenAI-compatible endpoint.
//
// Features:
//   • SSE streaming with live token output
//   • Parallel tool calling
//   • Thinking/reasoning token extraction (reasoning fields and <think> tags)
// ===========================================================================

use async_trait::async_trait;
use eyre::Result;
use futures::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use tokio::sync::mpsc;

use super::AiClient;
use super::sse::{SseLineBuffer, data_payload};
use super::types::*;
use crate::config::providers::ModelInfo;

const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

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
        Self {
            client: super::streaming_http_client(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model,
            model_info,
        }
    }

    fn is_official_openai(&self) -> bool {
        self.base_url.contains("api.openai.com")
    }

    fn is_ollama(&self) -> bool {
        self.base_url.contains("ollama") || self.base_url.contains(":11434")
    }

    fn tool_calls_json(msg: &Message) -> Option<serde_json::Value> {
        if msg.tool_calls.is_empty() {
            return None;
        }
        Some(json!(
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
        ))
    }

    fn image_part(data: &str, mime_type: &str) -> serde_json::Value {
        json!({
            "type": "image_url",
            "image_url": { "url": format!("data:{mime_type};base64,{data}") }
        })
    }

    /// Convert my Message types to OpenAI API format.
    fn format_messages(&self, messages: &[Message]) -> Vec<serde_json::Value> {
        let vision = self.supports_vision();
        let mut out = Vec::with_capacity(messages.len());

        for msg in messages {
            let role = match msg.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };
            let images: Vec<&ContentPart> = msg
                .content
                .iter()
                .filter(|p| matches!(p, ContentPart::Image { .. }))
                .collect();

            let mut obj = if vision && !images.is_empty() && msg.role != Role::Tool {
                let parts: Vec<serde_json::Value> = msg
                    .content
                    .iter()
                    .map(|part| match part {
                        ContentPart::Text { text } => json!({ "type": "text", "text": text }),
                        ContentPart::Image { data, mime_type } => Self::image_part(data, mime_type),
                    })
                    .collect();
                json!({ "role": role, "content": parts })
            } else {
                json!({ "role": role, "content": msg.text() })
            };

            if let Some(id) = &msg.tool_call_id {
                obj["tool_call_id"] = json!(id);
            }
            if let Some(calls) = Self::tool_calls_json(msg) {
                obj["tool_calls"] = calls;
                // Some strict servers reject an empty-string content next to tool calls.
                if msg.text().is_empty() {
                    obj["content"] = serde_json::Value::Null;
                }
            }
            out.push(obj);

            // APIs reject `image_url` inside `tool` messages, so tool-produced
            // images (view_image, screenshots) follow as a user message.
            if vision && msg.role == Role::Tool && !images.is_empty() {
                let mut parts = vec![json!({
                    "type": "text",
                    "text": format!(
                        "Image output of tool `{}`:",
                        msg.tool_name.as_deref().unwrap_or("tool")
                    )
                })];
                for img in images {
                    if let ContentPart::Image { data, mime_type } = img {
                        parts.push(Self::image_part(data, mime_type));
                    }
                }
                out.push(json!({ "role": "user", "content": parts }));
            }
        }
        out
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

    fn build_body(&self, request: &AiRequest) -> serde_json::Value {
        let is_openai_reasoning = request.reasoning_type == Some(ReasoningType::OpenAiEffort);

        let mut body = json!({
            "model": self.model,
            "messages": self.format_messages(&request.messages),
            "stream": true,
            "stream_options": { "include_usage": true },
        });

        // OpenAI's own endpoint validates parameters strictly; `include_reasoning`
        // is a gateway (OpenRouter-style) extension.
        if !self.is_official_openai() {
            body["include_reasoning"] = json!(true);
        }

        // OpenAI reasoning models reject a custom temperature.
        if !is_openai_reasoning {
            body["temperature"] = json!(request.temperature);
        }

        if let Some(effort) = request.thinking_effort.as_deref().filter(|e| *e != "none") {
            match request.reasoning_type {
                Some(ReasoningType::OpenAiEffort) => {
                    // OpenAI accepts low/medium/high; map the extended levels down.
                    let effort = match effort {
                        "xhigh" | "max" => "high",
                        "adaptive" => "medium",
                        other => other,
                    };
                    body["reasoning_effort"] = json!(effort);
                }
                Some(ReasoningType::AnthropicThinking) => {
                    body["thinking"] = if effort == "adaptive" {
                        json!({ "type": "adaptive" })
                    } else {
                        json!({ "type": "enabled", "effort": effort })
                    };
                }
                _ => {}
            }
        }

        if self.is_ollama() {
            body["keep_alive"] = json!(-1);
        }

        let max_tokens = request.max_tokens.unwrap_or(8192);
        if is_openai_reasoning || self.is_official_openai() {
            body["max_completion_tokens"] = json!(max_tokens);
        } else {
            body["max_tokens"] = json!(max_tokens);
        }

        if !request.tools.is_empty() {
            body["tools"] = json!(self.format_tools(&request.tools));
            body["tool_choice"] = json!("auto");
        }
        body
    }
}

// ── SSE response chunk types ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SseChunk {
    choices: Option<Vec<SseChoice>>,
    usage: Option<SseUsage>,
    error: Option<serde_json::Value>,
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
    completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Debug, Deserialize)]
struct CompletionTokensDetails {
    reasoning_tokens: Option<u32>,
}

/// Splits streamed content into thinking vs. answer text for models that wrap
/// their reasoning in `<think>…</think>` tags instead of a separate field.
#[derive(Default)]
struct ThinkTagSplitter {
    buf: String,
    in_thinking: bool,
    /// Models emit at most one leading think block; after it closes (or once
    /// real content starts) everything is plain content.
    done: bool,
}

impl ThinkTagSplitter {
    fn push(&mut self, text: &str, tx: &mpsc::UnboundedSender<AiStreamEvent>) {
        if self.done {
            let _ = tx.send(AiStreamEvent::ContentDelta(text.to_string()));
            return;
        }
        self.buf.push_str(text);
        loop {
            if !self.in_thinking {
                if let Some(pos) = self.buf.find(THINK_OPEN) {
                    let before = self.buf[..pos].to_string();
                    if !before.trim().is_empty() {
                        let _ = tx.send(AiStreamEvent::ContentDelta(before));
                    }
                    self.buf.drain(..pos + THINK_OPEN.len());
                    self.in_thinking = true;
                    continue;
                }
                // Hold back a possible partial "<think>" at the end of the buffer.
                let keep = partial_tag_suffix(&self.buf, THINK_OPEN);
                let emit_to = self.buf.len() - keep;
                if emit_to > 0 {
                    let emitted: String = self.buf.drain(..emit_to).collect();
                    if !emitted.trim().is_empty() {
                        // Real content arrived before any think block.
                        self.done = true;
                    }
                    let _ = tx.send(AiStreamEvent::ContentDelta(emitted));
                    if self.done && !self.buf.is_empty() {
                        let rest = std::mem::take(&mut self.buf);
                        let _ = tx.send(AiStreamEvent::ContentDelta(rest));
                    }
                }
                return;
            }

            if let Some(pos) = self.buf.find(THINK_CLOSE) {
                if pos > 0 {
                    let _ = tx.send(AiStreamEvent::ThinkingDelta(self.buf[..pos].to_string()));
                }
                self.buf.drain(..pos + THINK_CLOSE.len());
                let _ = tx.send(AiStreamEvent::ThinkingDone);
                self.in_thinking = false;
                self.done = true;
                let rest = std::mem::take(&mut self.buf);
                let rest = rest.trim_start_matches(['\n', '\r']);
                if !rest.is_empty() {
                    let _ = tx.send(AiStreamEvent::ContentDelta(rest.to_string()));
                }
                return;
            }
            let keep = partial_tag_suffix(&self.buf, THINK_CLOSE);
            let emit_to = self.buf.len() - keep;
            if emit_to > 0 {
                let emitted: String = self.buf.drain(..emit_to).collect();
                let _ = tx.send(AiStreamEvent::ThinkingDelta(emitted));
            }
            return;
        }
    }

    /// Flush anything still buffered when the stream ends.
    fn finish(&mut self, tx: &mpsc::UnboundedSender<AiStreamEvent>) {
        let rest = std::mem::take(&mut self.buf);
        if self.in_thinking {
            if !rest.is_empty() {
                let _ = tx.send(AiStreamEvent::ThinkingDelta(rest));
            }
            let _ = tx.send(AiStreamEvent::ThinkingDone);
            self.in_thinking = false;
        } else if !rest.is_empty() {
            let _ = tx.send(AiStreamEvent::ContentDelta(rest));
        }
        self.done = true;
    }
}

/// Length of the longest suffix of `buf` that is a proper prefix of `tag`
/// (always on a char boundary, since tags are ASCII).
fn partial_tag_suffix(buf: &str, tag: &str) -> usize {
    (1..tag.len())
        .rev()
        .find(|&n| {
            buf.len() >= n
                && buf.is_char_boundary(buf.len() - n)
                && tag.starts_with(&buf[buf.len() - n..])
        })
        .unwrap_or(0)
}

#[async_trait]
impl AiClient for OpenAiCompatClient {
    async fn chat_stream(
        &self,
        request: AiRequest,
        tx: mpsc::UnboundedSender<AiStreamEvent>,
    ) -> Result<()> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = self.build_body(&request);

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key.trim()))
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let message = super::describe_http_error("OpenAI-compatible", response).await;
            let _ = tx.send(AiStreamEvent::Error(message));
            return Ok(());
        }

        let mut stream = response.bytes_stream();
        let mut lines = SseLineBuffer::new();
        let mut splitter = ThinkTagSplitter::default();
        let mut in_reasoning_field = false;
        let mut finish_reason = String::from("stop");
        let mut usage: Option<UsageStats> = None;
        // Ordered by index so parallel calls keep the model's order.
        let mut pending_tool_calls: BTreeMap<usize, ToolCall> = BTreeMap::new();

        'outer: while let Some(chunk_result) = stream.next().await {
            let chunk = match chunk_result {
                Ok(bytes) => bytes,
                Err(e) => {
                    let _ = tx.send(AiStreamEvent::Error(format!("Stream error: {e}")));
                    break;
                }
            };

            for line in lines.push(&chunk) {
                let Some(data) = data_payload(line.trim()) else {
                    continue;
                };
                if data == "[DONE]" {
                    break 'outer;
                }
                let Ok(sse) = serde_json::from_str::<SseChunk>(data) else {
                    continue;
                };

                if let Some(err) = sse.error {
                    let message = err["message"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| err.to_string());
                    let _ = tx.send(AiStreamEvent::Error(format!("API stream error: {message}")));
                    return Ok(());
                }

                if let Some(u) = &sse.usage {
                    usage = Some(UsageStats {
                        prompt_tokens: u.prompt_tokens.unwrap_or(0),
                        completion_tokens: u.completion_tokens.unwrap_or(0),
                        total_tokens: u.total_tokens.unwrap_or(0),
                        reasoning_tokens: u
                            .completion_tokens_details
                            .as_ref()
                            .and_then(|d| d.reasoning_tokens),
                    });
                }

                for choice in sse.choices.iter().flatten() {
                    if let Some(delta) = &choice.delta {
                        if let Some(reasoning) =
                            delta.reasoning_content.as_deref().filter(|r| !r.is_empty())
                        {
                            in_reasoning_field = true;
                            let _ = tx.send(AiStreamEvent::ThinkingDelta(reasoning.to_string()));
                        }

                        if let Some(content) = delta.content.as_deref().filter(|c| !c.is_empty()) {
                            if in_reasoning_field {
                                let _ = tx.send(AiStreamEvent::ThinkingDone);
                                in_reasoning_field = false;
                                splitter.done = true;
                            }
                            splitter.push(content, &tx);
                        }

                        if let Some(tool_calls) = &delta.tool_calls {
                            if in_reasoning_field {
                                let _ = tx.send(AiStreamEvent::ThinkingDone);
                                in_reasoning_field = false;
                            }
                            for tc_delta in tool_calls {
                                let idx = tc_delta.index.unwrap_or(pending_tool_calls.len());
                                let entry =
                                    pending_tool_calls.entry(idx).or_insert_with(|| ToolCall {
                                        id: String::new(),
                                        name: String::new(),
                                        arguments: serde_json::Value::String(String::new()),
                                        thought_signature: None,
                                    });

                                if let Some(id) = tc_delta.id.as_deref().filter(|id| !id.is_empty())
                                {
                                    entry.id = id.to_string();
                                }
                                if let Some(func) = &tc_delta.function {
                                    if let Some(name) =
                                        func.name.as_deref().filter(|n| !n.is_empty())
                                    {
                                        let first = entry.name.is_empty();
                                        entry.name = name.to_string();
                                        if entry.id.is_empty() {
                                            // Some servers (Ollama, Gemini-compat) omit ids.
                                            entry.id =
                                                format!("call_{}", uuid::Uuid::new_v4().simple());
                                        }
                                        if first {
                                            let _ = tx
                                                .send(AiStreamEvent::ToolCallStart(entry.clone()));
                                        }
                                    }
                                    if let Some(args) =
                                        func.arguments.as_deref().filter(|a| !a.is_empty())
                                    {
                                        if let serde_json::Value::String(acc) = &mut entry.arguments
                                        {
                                            acc.push_str(args);
                                        }
                                        let _ = tx.send(AiStreamEvent::ToolCallDelta {
                                            id: entry.id.clone(),
                                            arguments_delta: args.to_string(),
                                        });
                                    }
                                }
                            }
                        }
                    }

                    if let Some(reason) = &choice.finish_reason {
                        finish_reason = reason.clone();
                    }
                }
            }
        }

        if let Some(rest) = lines.finish() {
            tracing::debug!("Ignoring trailing SSE data: {rest}");
        }
        if in_reasoning_field {
            let _ = tx.send(AiStreamEvent::ThinkingDone);
        }
        splitter.finish(&tx);

        for (_, mut tc) in pending_tool_calls {
            if tc.name.is_empty() {
                continue;
            }
            if tc.id.is_empty() {
                tc.id = format!("call_{}", uuid::Uuid::new_v4().simple());
            }
            let _ = tx.send(AiStreamEvent::ToolCallComplete(tc));
        }

        let _ = tx.send(AiStreamEvent::Done {
            finish_reason,
            usage,
        });
        Ok(())
    }

    fn supports_vision(&self) -> bool {
        self.model_info
            .as_ref()
            .is_some_and(|m| matches!(m.vision, crate::config::providers::Vision::Yes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> OpenAiCompatClient {
        OpenAiCompatClient::new(
            "https://example.invalid".into(),
            "test".into(),
            "test".into(),
            None,
        )
    }

    #[test]
    fn formats_complete_tool_exchange() {
        let call = ToolCall {
            id: "call-1".into(),
            name: "read_file".into(),
            arguments: json!({ "path": "README.md" }),
            thought_signature: None,
        };
        let messages = vec![
            Message::assistant_with_tool_calls("", vec![call]),
            Message::tool_result("call-1", "read_file", "contents"),
        ];
        let formatted = client().format_messages(&messages);

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

    fn collect(chunks: &[&str]) -> (String, String) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut splitter = ThinkTagSplitter::default();
        for c in chunks {
            splitter.push(c, &tx);
        }
        splitter.finish(&tx);
        drop(tx);
        let (mut thinking, mut content) = (String::new(), String::new());
        while let Ok(ev) = rx.try_recv() {
            match ev {
                AiStreamEvent::ThinkingDelta(t) => thinking.push_str(&t),
                AiStreamEvent::ContentDelta(c) => content.push_str(&c),
                _ => {}
            }
        }
        (thinking, content)
    }

    #[test]
    fn think_tags_split_across_chunks_with_unicode() {
        // Multi-byte characters right before a partial tag used to panic.
        let (thinking, content) =
            collect(&["<thi", "nk>plan ✅ step", "s</th", "ink>\nAnswer: 日本"]);
        assert_eq!(thinking, "plan ✅ steps");
        assert_eq!(content, "Answer: 日本");

        let (thinking, content) = collect(&["héllo wörld ✅✅✅ no tags here"]);
        assert!(thinking.is_empty());
        assert_eq!(content, "héllo wörld ✅✅✅ no tags here");
    }

    #[test]
    fn think_splitter_survives_any_chunk_boundary() {
        // A reply with an em dash crashed old builds mid-stream; split it at
        // every character position, inside and outside a think block.
        for text in [
            "Here's what I can do for you — build, fix, test.",
            "<think>weigh A — B</think>Answer — done",
        ] {
            let chars: Vec<(usize, char)> = text.char_indices().collect();
            for &(cut, _) in &chars {
                let (a, b) = text.split_at(cut);
                let (thinking, content) = collect(&[a, b]);
                let joined = format!("{thinking}{content}");
                let expected = text.replace("<think>", "").replace("</think>", "");
                assert_eq!(joined, expected, "split at byte {cut}");
            }
        }
    }

    #[test]
    fn official_openai_gets_no_gateway_params() {
        let c = OpenAiCompatClient::new(
            "https://api.openai.com/v1".into(),
            "k".into(),
            "gpt-4o".into(),
            None,
        );
        let body = c.build_body(&AiRequest::new(vec![Message::user("hi")]));
        assert!(body.get("include_reasoning").is_none());
        assert!(body.get("max_completion_tokens").is_some());
    }
}
