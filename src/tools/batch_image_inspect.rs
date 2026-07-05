// ═══════════════════════════════════════════════════════════════════════════
// Batch Image Inspect — View and analyze up to 10 images at once
//
// Sends multiple images in a single vision request for efficient batch
// analysis. Falls back to sequential OCR for non-vision models.
// ═══════════════════════════════════════════════════════════════════════════

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::mpsc;

use super::{Tool, ToolContext, ToolResult};
use crate::agent::vision;
use crate::ai::types::{AiRequest, AiStreamEvent, ContentPart, Message, Role};

const MAX_BATCH_SIZE: usize = 10;

pub struct BatchViewImageTool;

#[async_trait]
impl Tool for BatchViewImageTool {
    fn name(&self) -> &str {
        "batch_view_images"
    }

    fn description(&self) -> &str {
        "View and analyze multiple image files at once (max 10). Sends all images \
         to the vision model in a single request for efficient batch analysis. \
         Use this when you need to compare or review multiple images together."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Array of image file paths (relative to CWD or absolute within project). Maximum 10 images."
                },
                "prompt": {
                    "type": "string",
                    "description": "Question or instruction for analyzing the images (default: 'Describe each image and compare them')"
                }
            },
            "required": ["paths"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let paths: Vec<String> = match args["paths"].as_array() {
            Some(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            None => return ToolResult::err("'paths' must be an array of file path strings"),
        };

        if paths.is_empty() {
            return ToolResult::err("No image paths provided");
        }

        if paths.len() > MAX_BATCH_SIZE {
            return ToolResult::err(format!(
                "Too many images: {} provided, maximum is {}",
                paths.len(),
                MAX_BATCH_SIZE
            ));
        }

        let prompt = args["prompt"]
            .as_str()
            .unwrap_or("Describe each image in detail and note any similarities or differences");

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
                                "🖼️ Batch Image Analysis ({} images):\n\n{}",
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

        // If vision failed or wasn't supported, fall back to OCR if key is set
        if !supports_vision || ocr_fallback {
            if let Some(ocr_key) = &ctx.settings.ocr_space_api_key {
                let prefix = if ocr_fallback {
                    format!(
                        "⚠️ Vision API failed ({}), falling back to OCR...\n\n",
                        vision_error
                    )
                } else {
                    String::new()
                };

                let mut results = Vec::new();
                for (name, path) in &resolved_paths {
                    match vision::ocr_image(ocr_key, path).await {
                        Ok(text) => {
                            results.push(format!("--- {} ---\n{}", name, text));
                        }
                        Err(e) => {
                            results.push(format!("--- {} ---\nOCR failed: {}", name, e));
                        }
                    }
                }

                return ToolResult::ok_with_data(
                    format!(
                        "{}🖼️ Batch OCR Results ({} images):\n\n{}",
                        prefix,
                        paths.len(),
                        results.join("\n\n")
                    ),
                    json!({
                        "images": paths,
                        "count": paths.len(),
                        "method": "ocr",
                    }),
                );
            }
        }

        // If both failed or ocr key is missing
        let err_reason = if ocr_fallback {
            format!(
                "Vision API failed: {}. No OCR_SPACE_API_KEY set for fallback.",
                vision_error
            )
        } else {
            "The current AI model does not support vision (image inputs), and no OCR_SPACE_API_KEY is configured.".to_string()
        };
        ToolResult::err(err_reason)
    }
}
