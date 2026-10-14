// ===========================================================================
// Gemini Client — Google AI Studio direct (FREE tier)
//
// Features:
//   • SSE streaming via streamGenerateContent
//   • Tool calling (function declarations)
//   • Vision (inline image parts), including images returned by tools
//   • Thinking token extraction
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

pub const GEMINI_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

pub struct GeminiClient {
    client: Client,
    api_key: String,
    model: String,
    model_info: Option<ModelInfo>,
}

impl GeminiClient {
    pub fn new(api_key: String, model: String, model_info: Option<ModelInfo>) -> Self {
        Self {
            client: super::streaming_http_client(),
            api_key,
            model,
            model_info,
        }
    }

    /// Convert messages to Gemini's content format.
    ///
    /// The first system message becomes `systemInstruction`; later ones are sent
    /// as user text. Consecutive same-role turns are merged because Gemini
    /// requires all function responses for a turn in one content entry.
    fn format_contents(
        &self,
        messages: &[Message],
    ) -> (Option<serde_json::Value>, Vec<serde_json::Value>) {
        let mut system_instruction = None;
        let mut contents: Vec<serde_json::Value> = Vec::new();

        for msg in messages {
            let (role, parts): (&str, Vec<serde_json::Value>) = match msg.role {
                Role::System => {
                    let text = msg.text();
                    if system_instruction.is_none() && contents.is_empty() {
                        system_instruction = Some(json!({ "parts": [{ "text": text }] }));
                        continue;
                    }
                    if text.trim().is_empty() {
                        continue;
                    }
                    ("user", vec![json!({ "text": text })])
                }
                Role::Tool => {
                    let mut parts = vec![json!({
                        "functionResponse": {
                            "name": msg.tool_name.clone().unwrap_or_default(),
                            "response": { "result": msg.text() }
                        }
                    })];
                    for part in &msg.content {
                        if let ContentPart::Image { data, mime_type } = part {
                            parts.push(
                                json!({ "inlineData": { "mimeType": mime_type, "data": data } }),
                            );
                        }
                    }
                    ("user", parts)
                }
                Role::User | Role::Assistant => {
                    let role = if msg.role == Role::User {
                        "user"
                    } else {
                        "model"
                    };
                    let mut parts: Vec<serde_json::Value> = msg
                        .content
                        .iter()
                        .filter_map(|part| match part {
                            ContentPart::Text { text } if !text.is_empty() => {
                                Some(json!({ "text": text }))
                            }
                            ContentPart::Text { .. } => None,
                            ContentPart::Image { data, mime_type } => Some(json!({
                                "inlineData": { "mimeType": mime_type, "data": data }
                            })),
                        })
                        .collect();
                    parts.extend(msg.tool_calls.iter().map(|call| {
                        let args = match &call.arguments {
                            serde_json::Value::String(raw) => {
                                serde_json::from_str(raw).unwrap_or_else(|_| json!({}))
                            }
                            other => other.clone(),
                        };
                        let mut fc_part =
                            json!({ "functionCall": { "name": call.name, "args": args } });
                        if let Some(sig) = &call.thought_signature {
                            fc_part["thoughtSignature"] = json!(sig);
                        }
                        fc_part
                    }));
                    (role, parts)
                }
            };

            // Gemini rejects contents with an empty `parts` array.
            if parts.is_empty() {
                continue;
            }
            match contents.last_mut() {
                Some(last) if last["role"] == role => {
                    if let Some(arr) = last["parts"].as_array_mut() {
                        arr.extend(parts);
                    }
                }
                _ => contents.push(json!({ "role": role, "parts": parts })),
            }
        }

        (system_instruction, contents)
    }

    /// Convert tool definitions to Gemini function declarations.
    fn format_tools(&self, tools: &[ToolDef]) -> serde_json::Value {
        fn convert_types_to_uppercase(val: &mut serde_json::Value) {
            match val {
                serde_json::Value::Object(map) => {
                    if let Some(serde_json::Value::String(t)) = map.get_mut("type") {
                        *t = t.to_uppercase();
                    }
                    for v in map.values_mut() {
                        convert_types_to_uppercase(v);
                    }
                }
                serde_json::Value::Array(arr) => {
                    for v in arr {
                        convert_types_to_uppercase(v);
                    }
                }
                _ => {}
            }
        }

        let declarations: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                let mut params = t.parameters.clone();
                convert_types_to_uppercase(&mut params);
                json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": params
                })
            })
            .collect();

        json!([{ "functionDeclarations": declarations }])
    }
}

// ── Gemini SSE response types ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GeminiChunk {
    candidates: Option<Vec<GeminiCandidate>>,
    #[serde(rename = "usageMetadata")]
    usage_metadata: Option<GeminiUsage>,
    error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidate {
    content: Option<GeminiContent>,
    #[serde(rename = "finishReason")]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiContent {
    parts: Option<Vec<GeminiPart>>,
}

#[derive(Debug, Deserialize)]
struct GeminiPart {
    text: Option<String>,
    thought: Option<bool>,
    #[serde(rename = "functionCall")]
    function_call: Option<GeminiFunctionCall>,
    #[serde(alias = "thought_signature", alias = "thoughtSignature")]
    thought_signature: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiFunctionCall {
    name: String,
    args: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct GeminiUsage {
    #[serde(rename = "promptTokenCount")]
    prompt_token_count: Option<u32>,
    #[serde(rename = "candidatesTokenCount")]
    candidates_token_count: Option<u32>,
    #[serde(rename = "thoughtsTokenCount")]
    thoughts_token_count: Option<u32>,
    #[serde(rename = "totalTokenCount")]
    total_token_count: Option<u32>,
}

#[async_trait]
impl AiClient for GeminiClient {
    async fn chat_stream(
        &self,
        request: AiRequest,
        tx: mpsc::UnboundedSender<AiStreamEvent>,
    ) -> Result<()> {
        // The key goes in a header: in the query string it would leak into
        // reqwest error messages (which include the URL) shown in the UI.
        let url = format!(
            "{GEMINI_BASE}/models/{}:streamGenerateContent?alt=sse",
            self.model
        );

        let (system_instruction, contents) = self.format_contents(&request.messages);

        let mut body = json!({
            "contents": contents,
            "generationConfig": { "temperature": request.temperature },
            "safetySettings": [
                { "category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_NONE" },
                { "category": "HARM_CATEGORY_HATE_SPEECH", "threshold": "BLOCK_NONE" },
                { "category": "HARM_CATEGORY_SEXUALLY_EXPLICIT", "threshold": "BLOCK_NONE" },
                { "category": "HARM_CATEGORY_DANGEROUS_CONTENT", "threshold": "BLOCK_NONE" },
                { "category": "HARM_CATEGORY_CIVIC_INTEGRITY", "threshold": "BLOCK_NONE" }
            ]
        });

        if let Some(sys) = system_instruction {
            body["systemInstruction"] = sys;
        }
        if let Some(max_tokens) = request.max_tokens {
            body["generationConfig"]["maxOutputTokens"] = json!(max_tokens);
        }
        // Thinking configuration is intentionally only sent to Anthropic (project decision).
        if !request.tools.is_empty() {
            body["tools"] = self.format_tools(&request.tools);
        }

        let response = self
            .client
            .post(&url)
            .header("x-goog-api-key", self.api_key.trim())
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let message = super::describe_http_error("Gemini", response).await;
            let _ = tx.send(AiStreamEvent::Error(message));
            return Ok(());
        }

        let mut stream = response.bytes_stream();
        let mut lines = SseLineBuffer::new();
        let mut in_thinking = false;
        let mut finish_reason = String::from("STOP");
        // usageMetadata is cumulative and repeated on every chunk — keep the last.
        let mut usage: Option<UsageStats> = None;

        while let Some(chunk_result) = stream.next().await {
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
                let Ok(gemini_chunk) = serde_json::from_str::<GeminiChunk>(data) else {
                    continue;
                };

                if let Some(err) = gemini_chunk.error {
                    let message = err["message"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| err.to_string());
                    let _ = tx.send(AiStreamEvent::Error(format!(
                        "Gemini stream error: {message}"
                    )));
                    return Ok(());
                }

                for candidate in gemini_chunk.candidates.iter().flatten() {
                    for part in candidate
                        .content
                        .as_ref()
                        .and_then(|c| c.parts.as_ref())
                        .into_iter()
                        .flatten()
                    {
                        if part.thought.unwrap_or(false) {
                            if let Some(text) = part.text.as_deref().filter(|t| !t.is_empty()) {
                                in_thinking = true;
                                let _ = tx.send(AiStreamEvent::ThinkingDelta(text.to_string()));
                            }
                            continue;
                        }

                        if let Some(text) = part.text.as_deref().filter(|t| !t.is_empty()) {
                            if in_thinking {
                                let _ = tx.send(AiStreamEvent::ThinkingDone);
                                in_thinking = false;
                            }
                            let _ = tx.send(AiStreamEvent::ContentDelta(text.to_string()));
                        }

                        if let Some(fc) = &part.function_call {
                            if in_thinking {
                                let _ = tx.send(AiStreamEvent::ThinkingDone);
                                in_thinking = false;
                            }
                            let tc = ToolCall {
                                // Ids must be unique across the whole conversation, not per response.
                                id: format!("call_{}", uuid::Uuid::new_v4().simple()),
                                name: fc.name.clone(),
                                arguments: fc.args.clone().unwrap_or_else(|| json!({})),
                                thought_signature: part.thought_signature.clone(),
                            };
                            let _ = tx.send(AiStreamEvent::ToolCallStart(tc.clone()));
                            let _ = tx.send(AiStreamEvent::ToolCallComplete(tc));
                        }
                    }

                    if let Some(reason) = &candidate.finish_reason {
                        finish_reason = reason.clone();
                    }
                }

                if let Some(u) = &gemini_chunk.usage_metadata {
                    usage = Some(UsageStats {
                        prompt_tokens: u.prompt_token_count.unwrap_or(0),
                        completion_tokens: u.candidates_token_count.unwrap_or(0)
                            + u.thoughts_token_count.unwrap_or(0),
                        total_tokens: u.total_token_count.unwrap_or(0),
                        reasoning_tokens: u.thoughts_token_count,
                    });
                }
            }
        }

        if in_thinking {
            let _ = tx.send(AiStreamEvent::ThinkingDone);
        }

        // Blocked/aborted responses would otherwise look like an empty answer.
        if matches!(
            finish_reason.as_str(),
            "SAFETY" | "RECITATION" | "PROHIBITED_CONTENT" | "BLOCKLIST" | "SPII"
        ) {
            let _ = tx.send(AiStreamEvent::ContentDelta(format!(
                "\n\n⚠️ Gemini stopped the response (finish reason: {finish_reason})."
            )));
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

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "read_file".into(),
            arguments: json!({ "path": "README.md" }),
            thought_signature: None,
        }
    }

    #[test]
    fn tool_response_uses_function_name() {
        let client = GeminiClient::new("test".into(), "test".into(), None);
        let (_, contents) = client.format_contents(&[
            Message::assistant_with_tool_calls("", vec![call("call-1")]),
            Message::tool_result("call-1", "read_file", "contents"),
        ]);

        assert_eq!(contents[0]["parts"][0]["functionCall"]["name"], "read_file");
        assert_eq!(
            contents[1]["parts"][0]["functionResponse"]["name"],
            "read_file"
        );
        assert_eq!(contents[1]["role"], "user");
    }

    #[test]
    fn parallel_function_responses_are_merged_and_system_kept() {
        let client = GeminiClient::new("test".into(), "test".into(), None);
        let (system, contents) = client.format_contents(&[
            Message::system("main"),
            Message::user("go"),
            Message::assistant_with_tool_calls("", vec![call("a"), call("b")]),
            Message::tool_result("a", "read_file", "1"),
            Message::tool_result("b", "read_file", "2"),
            Message::system("[summary]"),
        ]);
        assert_eq!(system.unwrap()["parts"][0]["text"], "main");
        assert_eq!(contents.len(), 3);
        // Both function responses + the later system note share one user turn.
        assert_eq!(contents[2]["parts"].as_array().unwrap().len(), 3);
    }
}
