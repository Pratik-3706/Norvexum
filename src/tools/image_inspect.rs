use async_trait::async_trait;
use serde_json::json;
use tokio::sync::mpsc;

use super::{Tool, ToolContext, ToolResult};
use crate::agent::vision;
use crate::ai::types::{AiRequest, AiStreamEvent, Message};

pub struct InspectImageTool;

#[async_trait]
impl Tool for InspectImageTool {
    fn name(&self) -> &str {
        "view_image"
    }

    fn description(&self) -> &str {
        "View and analyze the contents of an image file within the project sandbox. \
         Use this to inspect screenshots, generated UI mockups, or downloaded assets. \
         IMPORTANT: You can pass custom questions via the 'prompt' parameter to ask \
         specific things like 'what character is this?' or 'what is odd about this image?'"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the image file (relative to CWD or absolute within project)" },
                "prompt": { "type": "string", "description": "Specific question or instruction for analyzing the image (e.g., 'what character is this?'). Highly recommended to customize this instead of using the default." },
                "attach_natively": { "type": "boolean", "description": "If true, attaches the image natively to the chat history so the model can see it in context. If false, makes a separate sub-request to save payload size and only returns text. Defaults to false." }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let path_str = args["path"].as_str().unwrap_or("");
        let prompt_str = args["prompt"]
            .as_str()
            .unwrap_or("Describe this image in detail");
        let attach_natively = args["attach_natively"].as_bool().unwrap_or(false);

        let path = match super::filesystem::resolve_path(path_str, ctx) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };

        if !path.exists() {
            return ToolResult::err(format!("Image file not found: {}", path.display()));
        }

        // Determine if active client supports vision
        let supports_vision = ctx
            .client
            .as_ref()
            .map(|c| c.supports_vision())
            .unwrap_or(false);

        if supports_vision {
            // Load and base64-encode image
            let (b64, mime) = match vision::encode_image_file(&path) {
                Ok(data) => data,
                Err(e) => return ToolResult::err(format!("Failed to read/encode image: {}", e)),
            };

            if attach_natively {
                // Call model directly with vision request attached natively
                let msg = format!(
                    "🖼️ Image {} attached natively for analysis.\n\nPrompt: {}",
                    path_str, prompt_str
                );
                return ToolResult::ok_with_image(msg, b64, mime);
            }

            // Otherwise, make a separate sub-request to save payload size
            if let Some(client) = &ctx.client {
                let msg = Message::user_with_image(prompt_str, b64, mime);
                let request = AiRequest::new(vec![msg]).with_temperature(0.5);

                let (tx, mut rx) = mpsc::unbounded_channel();
                if let Err(e) = client.chat_stream(request, tx).await {
                    return ToolResult::err(format!("Vision AI client request failed: {}", e));
                }

                let mut description = String::new();
                while let Some(event) = rx.recv().await {
                    match event {
                        AiStreamEvent::ContentDelta(text) => description.push_str(&text),
                        AiStreamEvent::Error(err) => {
                            return ToolResult::err(format!("AI stream error: {}", err));
                        }
                        _ => {}
                    }
                }

                if description.trim().is_empty() {
                    return ToolResult::err("AI returned an empty description for the image.");
                }

                ToolResult::ok_with_data(
                    format!("🖼️ Image Analysis for {}:\n\n{}", path_str, description),
                    json!({ "path": path.to_string_lossy(), "analysis": description }),
                )
            } else {
                ToolResult::err("No active AI client found in tool context.")
            }
        } else if let Some(gemini_key) = &ctx.settings.google_ai_api_key {
            // Fallback: Gemini SDK for SFW images
            let (b64, mime) = match vision::encode_image_file(&path) {
                Ok(data) => data,
                Err(e) => return ToolResult::err(format!("Failed to read/encode image: {}", e)),
            };

            let client = crate::ai::gemini::GeminiClient::new(
                gemini_key.clone(),
                "gemini-2.5-flash".to_string(),
                None,
            );
            use crate::ai::AiClient;

            let msg = Message::user_with_image(prompt_str, b64, mime);
            let request = AiRequest::new(vec![msg]).with_temperature(0.5);
            let (tx, mut rx) = mpsc::unbounded_channel();

            if let Err(e) = client.chat_stream(request, tx).await {
                // If it fails immediately
                return ToolResult::err(format!("Gemini fallback request failed: {}", e));
            }

            let mut description = String::new();
            let mut safety_blocked = false;
            while let Some(event) = rx.recv().await {
                match event {
                    AiStreamEvent::ContentDelta(text) => description.push_str(&text),
                    AiStreamEvent::Error(err) => {
                        if err.to_lowercase().contains("safety")
                            || err.to_lowercase().contains("blocked")
                        {
                            safety_blocked = true;
                        } else {
                            return ToolResult::err(format!("Gemini stream error: {}", err));
                        }
                    }
                    _ => {}
                }
            }

            if safety_blocked {
                return ToolResult::err(
                    "The image may contain NSFW/unsafe content. You must rely on your own native vision capabilities to view it directly.",
                );
            }

            if description.trim().is_empty() {
                return ToolResult::err("Gemini returned an empty description for the image.");
            }

            ToolResult::ok_with_data(
                format!(
                    "🖼️ Image Analysis for {} (via Gemini Fallback):\n\n{}",
                    path_str, description
                ),
                json!({ "path": path.to_string_lossy(), "analysis": description, "method": "gemini_fallback" }),
            )
        } else if let Some(ocr_key) = &ctx.settings.ocr_space_api_key {
            // Fallback: OCR space
            match vision::ocr_image(ocr_key, &path).await {
                Ok(text) => ToolResult::ok_with_data(
                    format!("🖼️ Image OCR text extracted from {}:\n\n{}", path_str, text),
                    json!({ "path": path.to_string_lossy(), "ocr_text": text }),
                ),
                Err(e) => ToolResult::err(format!("OCR failed: {}", e)),
            }
        } else {
            ToolResult::err(
                "The current AI model does not support vision (image inputs), no GOOGLE_AI_API_KEY is available for SDK fallback, and no OCR API key is set.",
            )
        }
    }
}
