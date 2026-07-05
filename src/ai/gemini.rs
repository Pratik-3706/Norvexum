// ═══════════════════════════════════════════════════════════════════════════
// Gemini Client — Google AI Studio direct (FREE tier)
//
// Features:
//   • SSE streaming via streamGenerateContent
//   • Tool calling (function declarations)
//   • Vision (inline image parts)
//   • Image generation via gemini-3.1-flash-image
//   • Thinking token extraction
//   • Free-tier rate limit awareness
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

const GEMINI_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

pub struct GeminiClient {
    client: Client,
    api_key: String,
    model: String,
    model_info: Option<ModelInfo>,
}

impl GeminiClient {
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

    /// Convert messages to Gemini's content format.
    fn format_contents(
        &self,
        messages: &[Message],
    ) -> (Option<serde_json::Value>, Vec<serde_json::Value>) {
        let mut system_instruction = None;
        let mut contents = Vec::new();

        for msg in messages {
            let role = match msg.role {
                Role::System => {
                    // Gemini uses systemInstruction separately
                    let text = msg.text();
                    system_instruction = Some(json!({
                        "parts": [{ "text": text }]
                    }));
                    continue;
                }
                Role::User => "user",
                Role::Assistant => "model",
                Role::Tool => {
                    let text = msg.text();
                    let tool_name = msg.tool_name.clone().unwrap_or_default();
                    contents.push(json!({
                        "role": "user",
                        "parts": [{
                            "functionResponse": {
                                "name": tool_name,
                                "response": {
                                    "result": text
                                }
                            }
                        }]
                    }));
                    continue;
                }
            };

            let mut parts: Vec<serde_json::Value> = msg
                .content
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text { text } if !text.is_empty() => Some(json!({ "text": text })),
                    ContentPart::Text { .. } => None,
                    ContentPart::Image { data, mime_type } => Some(json!({
                        "inlineData": {
                            "mimeType": mime_type,
                            "data": data
                        }
                    })),
                })
                .collect();
            parts.extend(msg.tool_calls.iter().map(|call| {
                json!({
                    "functionCall": {
                        "name": call.name,
                        "args": call.arguments
                    }
                })
            }));

            contents.push(json!({
                "role": role,
                "parts": parts
            }));
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

        json!([{
            "functionDeclarations": declarations
        }])
    }
}

// ── Gemini SSE response types ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GeminiChunk {
    candidates: Option<Vec<GeminiCandidate>>,
    #[serde(rename = "usageMetadata")]
    usage_metadata: Option<GeminiUsage>,
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
    #[serde(rename = "inlineData")]
    inline_data: Option<GeminiInlineData>,
}

#[derive(Debug, Deserialize)]
struct GeminiFunctionCall {
    name: String,
    args: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct GeminiInlineData {
    #[serde(rename = "mimeType")]
    mime_type: String,
    data: String,
}

#[derive(Debug, Deserialize)]
struct GeminiUsage {
    #[serde(rename = "promptTokenCount")]
    prompt_token_count: Option<u32>,
    #[serde(rename = "candidatesTokenCount")]
    candidates_token_count: Option<u32>,
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
        let url = format!(
            "{}/models/{}:streamGenerateContent?alt=sse&key={}",
            GEMINI_BASE, self.model, self.api_key
        );

        let (system_instruction, contents) = self.format_contents(&request.messages);

        let mut body = json!({
            "contents": contents,
            "generationConfig": {
                "temperature": request.temperature,
                "thinkingConfig": {
                    "thinkingBudget": 2048
                }
            }
        });

        if let Some(sys) = system_instruction {
            body["systemInstruction"] = sys;
        }

        if let Some(max_tokens) = request.max_tokens {
            body["generationConfig"]["maxOutputTokens"] = json!(max_tokens);
        }

        if !request.tools.is_empty() {
            body["tools"] = self.format_tools(&request.tools);
        }

        let response = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response.text().await.unwrap_or_default();
            let _ = tx.send(AiStreamEvent::Error(format!(
                "Gemini API error {}: {}",
                status, error_body
            )));
            return Ok(());
        }

        // Parse SSE stream
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut in_thinking = false;
        let mut tool_call_counter = 0u32;

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

                if let Some(data) = line.strip_prefix("data: ") {
                    let parsed: Result<GeminiChunk, _> = serde_json::from_str(data);
                    let gemini_chunk = match parsed {
                        Ok(c) => c,
                        Err(_) => continue,
                    };

                    if let Some(candidates) = &gemini_chunk.candidates {
                        for candidate in candidates {
                            // Check finish reason
                            if let Some(reason) = &candidate.finish_reason {
                                if in_thinking {
                                    let _ = tx.send(AiStreamEvent::ThinkingDone);
                                    in_thinking = false;
                                }
                                if reason != "STOP" {
                                    // Could be SAFETY, MAX_TOKENS, etc.
                                    tracing::warn!("Gemini finish reason: {}", reason);
                                }
                            }

                            if let Some(content) = &candidate.content {
                                if let Some(parts) = &content.parts {
                                    for part in parts {
                                        // ── Thinking tokens ─────────
                                        if part.thought.unwrap_or(false) {
                                            if let Some(text) = &part.text {
                                                if !text.is_empty() {
                                                    if !in_thinking {
                                                        in_thinking = true;
                                                    }
                                                    let _ = tx.send(AiStreamEvent::ThinkingDelta(
                                                        text.clone(),
                                                    ));
                                                }
                                            }
                                            continue;
                                        }

                                        // ── Content text ────────────
                                        if let Some(text) = &part.text {
                                            if !text.is_empty() {
                                                if in_thinking {
                                                    let _ = tx.send(AiStreamEvent::ThinkingDone);
                                                    in_thinking = false;
                                                }
                                                let _ = tx.send(AiStreamEvent::ContentDelta(
                                                    text.clone(),
                                                ));
                                            }
                                        }

                                        // ── Function calls ──────────
                                        if let Some(fc) = &part.function_call {
                                            if in_thinking {
                                                let _ = tx.send(AiStreamEvent::ThinkingDone);
                                                in_thinking = false;
                                            }
                                            tool_call_counter += 1;
                                            let tc = ToolCall {
                                                id: format!("call_{}", tool_call_counter),
                                                name: fc.name.clone(),
                                                arguments: fc.args.clone().unwrap_or(
                                                    serde_json::Value::Object(
                                                        serde_json::Map::new(),
                                                    ),
                                                ),
                                            };
                                            let _ =
                                                tx.send(AiStreamEvent::ToolCallStart(tc.clone()));
                                            let _ = tx.send(AiStreamEvent::ToolCallComplete(tc));
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Usage stats
                    if let Some(usage) = &gemini_chunk.usage_metadata {
                        let _ = tx.send(AiStreamEvent::Done {
                            finish_reason: "stop".into(),
                            usage: Some(UsageStats {
                                prompt_tokens: usage.prompt_token_count.unwrap_or(0),
                                completion_tokens: usage.candidates_token_count.unwrap_or(0),
                                total_tokens: usage.total_token_count.unwrap_or(0),
                            }),
                        });
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

    async fn generate_image(&self, request: ImageGenRequest) -> Result<ImageGenResult> {
        // Use gemini-3.1-flash-image for image generation
        let image_model = if self.model.contains("image") {
            self.model.clone()
        } else {
            "gemini-3.1-flash-image".into()
        };

        let url = format!(
            "{}/models/{}:generateContent?key={}",
            GEMINI_BASE, image_model, self.api_key
        );

        let body = json!({
            "contents": [{
                "parts": [{
                    "text": request.prompt
                }]
            }],
            "generationConfig": {
                "responseModalities": ["TEXT", "IMAGE"],
                "imageSizes": [format!("{}x{}", request.width, request.height)]
            }
        });

        let response = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let error = response.text().await.unwrap_or_default();
            eyre::bail!("Gemini image generation failed: {}", error);
        }

        let result: serde_json::Value = response.json().await?;

        let mut images = Vec::new();

        if let Some(candidates) = result["candidates"].as_array() {
            for candidate in candidates {
                if let Some(parts) = candidate["content"]["parts"].as_array() {
                    for part in parts {
                        if let Some(inline_data) = part.get("inlineData") {
                            let mime = inline_data["mimeType"].as_str().unwrap_or("image/png");
                            let data = inline_data["data"].as_str().unwrap_or("");
                            if !data.is_empty() {
                                images.push((data.to_string(), mime.to_string()));
                            }
                        }
                    }
                }
            }
        }

        if images.is_empty() {
            eyre::bail!("Gemini returned no images");
        }

        Ok(ImageGenResult {
            images,
            provider: "google_direct".into(),
            model: image_model,
        })
    }

    fn provider_name(&self) -> &str {
        "google_direct"
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_response_uses_function_name() {
        let client = GeminiClient::new("test".into(), "test".into(), None);
        let call = ToolCall {
            id: "call-1".into(),
            name: "read_file".into(),
            arguments: json!({ "path": "README.md" }),
        };
        let (_, contents) = client.format_contents(&[
            Message::assistant_with_tool_calls("", vec![call]),
            Message::tool_result("call-1", "read_file", "contents"),
        ]);

        assert_eq!(contents[0]["parts"][0]["functionCall"]["name"], "read_file");
        assert_eq!(
            contents[1]["parts"][0]["functionResponse"]["name"],
            "read_file"
        );
        assert_eq!(contents[1]["role"], "user");
    }
}
