// ===========================================================================
// Image viewing — view_image and the helpers batch_view_images shares
//
// A model that supports images looks at them itself: they are attached to
// the tool result. Only a text-only model gets a description from Gemini, or
// the text read out with OCR.
// ===========================================================================

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::mpsc;

use super::{Tool, ToolContext, ToolResult};
use crate::agent::vision;
use crate::ai::types::{AiRequest, AiStreamEvent, ContentPart, Message};

/// Model that describes images for text-only models.
const DESCRIBER_MODEL: &str = "gemini-2.5-flash";

/// An image the model asked for: the name it used and the resolved file.
pub(super) type NamedImage = (String, PathBuf);

pub(super) fn model_sees_images(ctx: &ToolContext) -> bool {
    ctx.client.as_ref().is_some_and(|c| c.supports_vision())
}

/// Attach the images to the tool result so the model sees them directly.
pub(super) fn attach_images(images: &[NamedImage], prompt: Option<&str>) -> ToolResult {
    let mut encoded = Vec::with_capacity(images.len());
    for (name, path) in images {
        match vision::encode_image_file(path) {
            Ok(pair) => encoded.push(pair),
            Err(e) => return ToolResult::err(format!("Failed to read image '{name}': {e}")),
        }
    }
    let mut note = match images {
        [(name, _)] => format!("Image `{name}` is attached. Look at it directly."),
        _ => {
            let list: Vec<String> = images
                .iter()
                .enumerate()
                .map(|(i, (name, _))| format!("{}. {name}", i + 1))
                .collect();
            format!(
                "{} images are attached, in this order. Look at them directly.\n{}",
                images.len(),
                list.join("\n")
            )
        }
    };
    if let Some(prompt) = prompt.filter(|p| !p.trim().is_empty()) {
        note.push_str(&format!("\n\nWhat to look for: {prompt}"));
    }
    ToolResult::ok_with_images(note, encoded)
}

/// For a model that can't see images: a Gemini description, else OCR text.
pub(super) async fn describe_for_text_model(
    images: &[NamedImage],
    prompt: &str,
    ctx: &ToolContext,
) -> ToolResult {
    if let Some(key) = &ctx.settings.google_ai_api_key {
        describe_with_gemini(key, images, prompt).await
    } else if let Some(key) = &ctx.settings.ocr_space_api_key {
        let mut texts = Vec::new();
        for (name, path) in images {
            match vision::ocr_image(key, path).await {
                Ok(text) => texts.push(format!("--- {name} ---\n{text}")),
                Err(e) if images.len() == 1 => {
                    return ToolResult::err(format!("OCR failed for '{name}': {e}"));
                }
                Err(e) => texts.push(format!("--- {name} ---\nOCR failed: {e}")),
            }
        }
        ToolResult::ok(format!(
            "This model can't see images, so the text in them was read with OCR \
             (OCR only finds text, not what the image shows):\n\n{}",
            texts.join("\n\n")
        ))
    } else {
        ToolResult::err(
            "This model can't see images, and neither GOOGLE_AI_API_KEY (Gemini descriptions) \
             nor OCR_SPACE_API_KEY (text extraction) is set. Switch to a model that supports \
             images, or set one of those keys.",
        )
    }
}

async fn describe_with_gemini(key: &str, images: &[NamedImage], prompt: &str) -> ToolResult {
    let mut msg = Message::user(prompt);
    for (name, path) in images {
        match vision::encode_image_file(path) {
            Ok((data, mime_type)) => {
                if images.len() > 1 {
                    msg.content.push(ContentPart::Text {
                        text: format!("\n--- Image: {name} ---"),
                    });
                }
                msg.content.push(ContentPart::Image { data, mime_type });
            }
            Err(e) => return ToolResult::err(format!("Failed to read image '{name}': {e}")),
        }
    }

    use crate::ai::AiClient;
    let client =
        crate::ai::gemini::GeminiClient::new(key.to_string(), DESCRIBER_MODEL.to_string(), None);
    let request = AiRequest::new(vec![msg]).with_temperature(0.5);
    let (tx, mut rx) = mpsc::unbounded_channel();
    if let Err(e) = client.chat_stream(request, tx).await {
        return ToolResult::err(format!("Gemini image description failed: {e}"));
    }

    let mut description = String::new();
    while let Some(event) = rx.recv().await {
        match event {
            AiStreamEvent::ContentDelta(text) => description.push_str(&text),
            AiStreamEvent::Error(err) => {
                let lower = err.to_lowercase();
                if lower.contains("safety") || lower.contains("blocked") {
                    return ToolResult::err(
                        "Gemini's safety filter refused to describe the image(s). Switch to a \
                         model that supports images to view them directly.",
                    );
                }
                return ToolResult::err(format!("Gemini image description failed: {err}"));
            }
            _ => {}
        }
    }
    if description.trim().is_empty() {
        return ToolResult::err("Gemini returned an empty description.");
    }
    ToolResult::ok(format!(
        "This model can't see images, so Gemini described {}:\n\n{description}",
        match images {
            [(name, _)] => format!("`{name}`"),
            _ => format!("{} images", images.len()),
        }
    ))
}

pub struct InspectImageTool;

#[async_trait]
impl Tool for InspectImageTool {
    fn name(&self) -> &str {
        "view_image"
    }

    fn description(&self) -> &str {
        "Look at an image file in the project: screenshots, generated UI, downloaded assets. \
         If you support images you see it yourself; otherwise it is described for you. \
         Use 'prompt' to say what to look for, e.g. 'is the logo cropped?'."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the image file (relative to CWD or absolute within project)" },
                "prompt": { "type": "string", "description": "What to look for or ask about the image (e.g. 'what character is this?')." }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let path_str = args["path"].as_str().unwrap_or("");
        let prompt = args["prompt"].as_str();

        let path = match super::filesystem::resolve_path(path_str, ctx) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        if !path.exists() {
            return ToolResult::err(format!("Image file not found: {}", path.display()));
        }

        let images = [(path_str.to_string(), path)];
        if model_sees_images(ctx) {
            return attach_images(&images, prompt);
        }
        describe_for_text_model(
            &images,
            prompt.unwrap_or("Describe this image in detail."),
            ctx,
        )
        .await
    }
}
