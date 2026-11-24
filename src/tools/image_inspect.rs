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

/// An image the model asked for, read and checked.
pub(super) struct LoadedImage {
    /// The path as the model gave it.
    name: String,
    path: PathBuf,
    data: String,
    mime: String,
}

pub(super) fn model_sees_images(ctx: &ToolContext) -> bool {
    ctx.client.as_ref().is_some_and(|c| c.supports_vision())
}

/// Read one image. The error says what is wrong with it, without its name.
pub(super) fn load_image(name: &str, ctx: &ToolContext) -> Result<LoadedImage, String> {
    let path = super::filesystem::resolve_path(name, ctx)?;
    if !path.exists() {
        return Err("no such file".into());
    }
    let (data, mime) = vision::encode_image_file(&path).map_err(|e| e.to_string())?;
    Ok(LoadedImage {
        name: name.to_string(),
        path,
        data,
        mime,
    })
}

/// Attach the images to the tool result so the model sees them directly.
pub(super) fn attach_images(images: Vec<LoadedImage>, prompt: Option<&str>) -> ToolResult {
    let mut note = match images.as_slice() {
        [image] => format!("Image `{}` is attached. Look at it directly.", image.name),
        _ => {
            let list: Vec<String> = images
                .iter()
                .enumerate()
                .map(|(i, image)| format!("{}. {}", i + 1, image.name))
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
    let encoded = images.into_iter().map(|i| (i.data, i.mime)).collect();
    ToolResult::ok_with_images(note, encoded)
}

/// For a model that can't see images: a Gemini description, else OCR text.
pub(super) async fn describe_for_text_model(
    images: Vec<LoadedImage>,
    prompt: &str,
    ctx: &ToolContext,
) -> ToolResult {
    if let Some(key) = &ctx.settings.google_ai_api_key {
        describe_with_gemini(key, images, prompt).await
    } else if let Some(key) = &ctx.settings.ocr_space_api_key {
        let mut texts = Vec::new();
        for LoadedImage { name, path, .. } in &images {
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

async fn describe_with_gemini(key: &str, images: Vec<LoadedImage>, prompt: &str) -> ToolResult {
    let what = match images.as_slice() {
        [image] => format!("`{}`", image.name),
        _ => format!("{} images", images.len()),
    };
    let several = images.len() > 1;
    let mut msg = Message::user(prompt);
    for image in images {
        if several {
            msg.content.push(ContentPart::Text {
                text: format!("\n--- Image: {} ---", image.name),
            });
        }
        msg.content.push(ContentPart::Image {
            data: image.data,
            mime_type: image.mime,
        });
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
        what
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

        let image = match load_image(path_str, ctx) {
            Ok(image) => image,
            Err(e) => return ToolResult::err(format!("Can't view '{path_str}': {e}")),
        };
        if model_sees_images(ctx) {
            return attach_images(vec![image], prompt);
        }
        describe_for_text_model(
            vec![image],
            prompt.unwrap_or("Describe this image in detail."),
            ctx,
        )
        .await
    }
}
