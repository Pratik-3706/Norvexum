// ═══════════════════════════════════════════════════════════════════════════
// Image Generation Tool
// ═══════════════════════════════════════════════════════════════════════════

use super::{Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use base64::Engine;

pub struct GenerateImageTool;

#[async_trait]
impl Tool for GenerateImageTool {
    fn name(&self) -> &str {
        "generate_image"
    }

    fn description(&self) -> &str {
        "Generate a brand new image using AI based on a detailed text prompt. \
         Selects the best available free or paid provider, and saves the output to the sandbox."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": { 
                    "type": "string", 
                    "description": "Extremely detailed description of the image to generate. Include style, colors, lighting, subject matter, and details." 
                },
                "filename": { 
                    "type": "string", 
                    "description": "Base filename to save the generated image (without extension, e.g., 'furina_artwork')" 
                },
                "output_dir": { 
                    "type": "string", 
                    "description": "Output directory relative to project root (default: current dir)" 
                },
                "width": { "type": "integer", "description": "Width of the generated image (default: 1024)" },
                "height": { "type": "integer", "description": "Height of the generated image (default: 1024)" }
            },
            "required": ["prompt", "filename"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let prompt = args["prompt"].as_str().unwrap_or("");
        let filename = args["filename"].as_str().unwrap_or("generated");
        let output_dir = args["output_dir"].as_str().unwrap_or(".");
        let width = args["width"].as_u64().unwrap_or(1024) as u32;
        let height = args["height"].as_u64().unwrap_or(1024) as u32;

        if prompt.is_empty() {
            return ToolResult::err("Prompt cannot be empty");
        }

        let out_dir = match super::filesystem::resolve_path(output_dir, ctx) {
            Ok(path) => path,
            Err(error) => return ToolResult::err(error),
        };
        if let Err(e) = std::fs::create_dir_all(&out_dir) {
            return ToolResult::err(format!("Failed to create output dir: {}", e));
        }

        let req = crate::ai::types::ImageGenRequest {
            prompt: prompt.to_string(),
            width,
            height,
            num_images: 1,
        };

        match crate::ai::image_gen::generate_image(&ctx.settings, req).await {
            Ok(res) => {
                if res.images.is_empty() {
                    return ToolResult::err("Provider returned no images");
                }
                
                // Get the first generated image
                let (base64_data, mime) = &res.images[0];
                let decoded = match base64::engine::general_purpose::STANDARD.decode(base64_data) {
                    Ok(bytes) => bytes,
                    Err(e) => return ToolResult::err(format!("Failed to decode base64 image data: {}", e)),
                };

                let ext = match mime.as_str() {
                    "image/png" => ".png",
                    "image/webp" => ".webp",
                    "image/gif" => ".gif",
                    _ => ".jpg",
                };

                let safe_name = regex::Regex::new(r#"[\\/:*?"<>|]"#)
                    .unwrap()
                    .replace_all(filename, "_")
                    .trim()
                    .to_string();

                let filepath = out_dir.join(format!("{}{}", safe_name, ext));

                match std::fs::write(&filepath, &decoded) {
                    Ok(_) => {
                        let kb = decoded.len() as f64 / 1024.0;
                        ToolResult::ok_with_data(
                            format!(
                                "✅ Successfully generated image using {} ({}) and saved to {}",
                                res.provider, res.model, filepath.display()
                            ),
                            json!({
                                "path": filepath.to_string_lossy(),
                                "size_kb": (kb * 10.0).round() / 10.0,
                                "mime": mime,
                                "provider": res.provider,
                                "model": res.model
                            }),
                        )
                    }
                    Err(e) => ToolResult::err(format!("Failed to write generated image to disk: {}", e)),
                }
            }
            Err(e) => ToolResult::err(format!("Image generation failed: {}", e)),
        }
    }
}
