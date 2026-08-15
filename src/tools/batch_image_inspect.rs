// ====================================================================
// Batch Image Inspect — View and analyze up to 10 images at once
//
// Sends multiple images in a single vision request for efficient batch
// analysis. Falls back to sequential OCR for non-vision models.
// =====================================================================

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::mpsc;

use super::{Tool, ToolContext, ToolResult};
use crate::agent::vision;
use crate::ai::types::{AiRequest, AiStreamEvent, ContentPart, Message, Role};

const MAX_BATCH_SIZE: usize = 14;

pub struct BatchViewImageTool;

#[async_trait]
impl Tool for BatchViewImageTool {
    fn name(&self) -> &str {
        "batch_view_images"
    }

    fn description(&self) -> &str {
        "View and analyze multiple image files at once (max 14). Sends all images \
         to the vision model in a single request for efficient batch analysis. \
         IMPORTANT: You can pass custom questions via the 'prompt' parameter to ask \
         specific things like 'what are the differences?' or 'which image has the bug?'"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Array of image file paths (relative to CWD or absolute within project). Maximum 14 images."
                },
                "prompt": {
                    "type": "string",
                    "description": "Question or instruction for analyzing the images (e.g., 'what are the differences?'). Highly recommended to customize this instead of using the default."
                }
            },
            "required": ["paths"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let mut paths: Vec<String> = match args["paths"].as_array() {
            Some(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            None => return ToolResult::err("'paths' must be an array of file path strings"),
        };

        if paths.is_empty() {
            return ToolResult::err("No image paths provided");
        }

        let mut warning = String::new();
        if paths.len() > MAX_BATCH_SIZE {
            warning = format!("Warning: {} images provided, but the maximum batch size is {}. Only the first {} images will be analyzed.\n\n", paths.len(), MAX_BATCH_SIZE, MAX_BATCH_SIZE);
            paths.truncate(MAX_BATCH_SIZE);
        }

        let mut prompt = args["prompt"]
            .as_str()
            .unwrap_or("Describe each image in detail and note any similarities or differences")
            .to_string();

        let context_path = ctx.settings.project_root.join(crate::config::NORVEXUM_DIR).join(crate::config::CONTEXT_FILE);
        if let Ok(context_json) = std::fs::read_to_string(&context_path) {
            prompt.push_str(&format!("\n\n--- Project Context ---\n{}\n-----------------------", context_json));
        }

        // Resolve and validate all paths first
        let mut resolved_paths = Vec::new();
        for path_str in &paths {
            let path = match super::filesystem::resolve_path(path_str, ctx) {
                Ok(p) => p,
                Err(e) => return ToolResult::err(format!("Path '{}': {}", path_str, e)),
            };
            if !path.exists() {
                return ToolResult::err(format!("Image not found: {}", path.display()));
            }
            resolved_paths.push((path_str.clone(), path));
        }

        let supports_vision = ctx
            .client
            .as_ref()
            .map(|c| c.supports_vision())
            .unwrap_or(false);

        let mut ocr_fallback = false;
        let mut vision_error = String::new();

        if supports_vision {
            // Build a multi-image message
            let mut content_parts = vec![ContentPart::Text {
                text: format!("{}\n\nAnalyzing {} images:", prompt, resolved_paths.len()),
            }];

            for (name, path) in &resolved_paths {
                match vision::encode_image_file(path) {
                    Ok((b64, mime)) => {
                        // Add a label before each image
                        content_parts.push(ContentPart::Text {
                            text: format!("\n--- Image: {} ---", name),
                        });
                        content_parts.push(ContentPart::Image {
                            data: b64,
                            mime_type: mime,
                        });
                    }
                    Err(e) => {
                        return ToolResult::err(format!(
                            "Failed to encode image '{}': {}",
                            name, e
                        ));
                    }
                }
            }

            let msg = Message {
                role: Role::User,
                content: content_parts,
                tool_calls: vec![],
                tool_call_id: None,
                tool_name: None,
                thought_signature: None,
            };

            if let Some(client) = &ctx.client {
                let request = AiRequest::new(vec![msg]).with_temperature(0.5);
                let (tx, mut rx) = mpsc::unbounded_channel();

                if let Err(e) = client.chat_stream(request, tx).await {
                    vision_error = format!("Vision request failed: {}", e);
                    ocr_fallback = true;
                } else {
                    let mut description = String::new();
                    let mut has_error = false;
                    while let Some(event) = rx.recv().await {
                        match event {
                            AiStreamEvent::ContentDelta(text) => description.push_str(&text),
                            AiStreamEvent::Error(err) => {
                                vision_error = format!("Stream error: {}", err);
                                has_error = true;
                            }
                            _ => {}
                        }
                    }

                    if has_error {
                        ocr_fallback = true;
                    } else if description.trim().is_empty() {
                        vision_error = "AI returned empty analysis for the images".to_string();
                        ocr_fallback = true;
                    } else {
                        let image_names: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
                        return ToolResult::ok_with_data(
                            format!(
                                "{}🖼️ Batch Image Analysis ({} images):\n\n{}",
                                warning,
                                paths.len(),
                                description
                            ),
                            json!({
                                "images": image_names,
                                "count": paths.len(),
                                "analysis": description,
                            }),
                        );
                    }
                }
            } else {
                return ToolResult::err("No active AI client found in tool context");
            }
        }

        if let Some(gemini_key) = &ctx.settings.google_ai_api_key {
            // Fallback: Gemini SDK for SFW images
            let mut content_parts = vec![ContentPart::Text {
                text: format!("{}\n\nAnalyzing {} images:", prompt, resolved_paths.len()),
            }];

            for (name, path) in &resolved_paths {
                match vision::encode_image_file(path) {
                    Ok((b64, mime)) => {
                        content_parts.push(ContentPart::Text {
                            text: format!("\n--- Image: {} ---", name),
                        });
                        content_parts.push(ContentPart::Image {
                            data: b64,
                            mime_type: mime,
                        });
                    }
                    Err(e) => return ToolResult::err(format!("Failed to encode image '{}': {}", name, e)),
                }
            }

            let client = crate::ai::gemini::GeminiClient::new(
                gemini_key.clone(),
                "gemini-2.5-flash".to_string(),
                None,
            );
            use crate::ai::AiClient;
            
            let msg = Message {
                role: Role::User,
                content: content_parts,
                tool_calls: vec![],
                tool_call_id: None,
                tool_name: None,
                thought_signature: None,
            };
            
            let request = AiRequest::new(vec![msg]).with_temperature(0.5);
            let (tx, mut rx) = mpsc::unbounded_channel();
            
            if let Err(e) = client.chat_stream(request, tx).await {
                return ToolResult::err(format!("Gemini fallback request failed: {}", e));
            }

            let mut description = String::new();
            let mut safety_blocked = false;
            while let Some(event) = rx.recv().await {
                match event {
                    AiStreamEvent::ContentDelta(text) => description.push_str(&text),
                    AiStreamEvent::Error(err) => {
                        if err.to_lowercase().contains("safety") || err.to_lowercase().contains("blocked") {
                            safety_blocked = true;
                        } else {
                            return ToolResult::err(format!("Gemini stream error: {}", err));
                        }
                    }
                    _ => {}
                }
            }

            if safety_blocked {
                return ToolResult::err("One or more images may contain NSFW/unsafe content. You must rely on your own native vision capabilities to view them directly.");
            }

            if description.trim().is_empty() {
                return ToolResult::err("Gemini returned an empty analysis.");
            }

            let image_names: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
            ToolResult::ok_with_data(
                format!("{warning}🖼️ Batch Image Analysis ({} images via Gemini Fallback):\n\n{}", paths.len(), description),
                json!({ "images": image_names, "count": paths.len(), "analysis": description, "method": "gemini_fallback" }),
            )
        } else if let Some(ocr_key) = &ctx.settings.ocr_space_api_key {
            let prefix = if ocr_fallback {
                format!("⚠️ Vision API failed ({}), falling back to OCR...\n\n", vision_error)
            } else {
                String::new()
            };

            let mut results = Vec::new();
            for (name, path) in &resolved_paths {
                match vision::ocr_image(ocr_key, path).await {
                    Ok(text) => results.push(format!("--- {} ---\n{}", name, text)),
                    Err(e) => results.push(format!("--- {} ---\nOCR failed: {}", name, e)),
                }
            }

            ToolResult::ok_with_data(
                format!("{}{prefix}🖼️ Batch OCR Results ({} images):\n\n{}", warning, paths.len(), results.join("\n\n")),
                json!({ "images": paths, "count": paths.len(), "method": "ocr" }),
            )
        } else {
            let err_reason = if ocr_fallback {
                format!("Vision API failed: {}. No GOOGLE_AI_API_KEY available for SDK fallback, and no OCR API key set.", vision_error)
            } else {
                "The current AI model does not support vision, no GOOGLE_AI_API_KEY is available for SDK fallback, and no OCR API key is set.".to_string()
            };
            ToolResult::err(err_reason)
        }
    }
}
