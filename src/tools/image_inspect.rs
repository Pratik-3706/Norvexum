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
         Use this to inspect screenshots, generated UI mockups, or downloaded assets."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the image file (relative to CWD or absolute within project)" },
                "prompt": { "type": "string", "description": "Specific question or instruction for analyzing the image (default: 'Describe this image in detail')" }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let path_str = args["path"].as_str().unwrap_or("");
        let prompt_str = args["prompt"].as_str().unwrap_or("Describe this image in detail");

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

            // Call model directly with vision request
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
        } else if let Some(ocr_key) = &ctx.settings.ocr_space_api_key {
            // Fallback: OCR space
            match vision::ocr_image(ocr_key, &path).await {
                Ok(text) => ToolResult::ok_with_data(
                    format!(
                        "🖼️ Image OCR text extracted from {}:\n\n{}",
                        path_str, text
                    ),
                    json!({ "path": path.to_string_lossy(), "ocr_text": text }),
                ),
                Err(e) => ToolResult::err(format!("OCR failed: {}", e)),
            }
        } else {
            ToolResult::err(
                "The current AI model does not support vision (image inputs), \
                 and no OCR_SPACE_API_KEY is configured in .env. Cannot inspect image.",
            )
        }
    }
}
