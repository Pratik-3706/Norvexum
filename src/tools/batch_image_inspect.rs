// ===========================================================================
// Batch image viewing — look at up to 14 images in one call
//
// Same rules as view_image: a model that supports images sees them all
// directly; a text-only model gets one Gemini description, or OCR text.
// ===========================================================================

use async_trait::async_trait;
use serde_json::json;

use super::image_inspect::{attach_images, describe_for_text_model, load_image, model_sees_images};
use super::{Tool, ToolContext, ToolResult};

const MAX_BATCH_SIZE: usize = 14;

pub struct BatchViewImageTool;

#[async_trait]
impl Tool for BatchViewImageTool {
    fn name(&self) -> &str {
        "batch_view_images"
    }

    fn description(&self) -> &str {
        "Look at several image files at once (max 14), e.g. to compare them. \
         If you support images you see them all yourself; otherwise they are described for you. \
         Use 'prompt' to say what to look for, e.g. 'which one has the bug?'."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Image file paths (relative to CWD or absolute within project). Maximum 14."
                },
                "prompt": {
                    "type": "string",
                    "description": "What to look for or compare (e.g. 'what are the differences?')."
                }
            },
            "required": ["paths"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let Some(paths) = args["paths"].as_array() else {
            return ToolResult::err("'paths' must be an array of file path strings");
        };
        if paths.is_empty() {
            return ToolResult::err("No image paths provided");
        }

        // One bad file must not cost the others: show what can be shown and
        // say what is wrong with the rest.
        let mut images = Vec::new();
        let mut problems = Vec::new();
        for path in &paths[..paths.len().min(MAX_BATCH_SIZE)] {
            match path.as_str() {
                Some(name) => match load_image(name, ctx) {
                    Ok(image) => images.push(image),
                    Err(e) => problems.push(format!("- {name}: {e}")),
                },
                None => problems.push(format!("- {path}: not a path (a string)")),
            }
        }

        let mut notes = String::new();
        if paths.len() > MAX_BATCH_SIZE {
            notes.push_str(&format!(
                "Warning: {} images given, but at most {MAX_BATCH_SIZE} fit in one call. \
                 Only the first {MAX_BATCH_SIZE} are included.\n\n",
                paths.len()
            ));
        }
        if !problems.is_empty() {
            notes.push_str(&format!(
                "{} of the files can't be shown:\n{}\n\n",
                problems.len(),
                problems.join("\n")
            ));
        }
        if images.is_empty() {
            return ToolResult::err(notes.trim_end());
        }

        let prompt = args["prompt"].as_str();
        let mut result = if model_sees_images(ctx) {
            attach_images(images, prompt)
        } else {
            describe_for_text_model(
                images,
                prompt.unwrap_or(
                    "Describe each image in detail and note any similarities or differences.",
                ),
                ctx,
            )
            .await
        };
        if result.success {
            result.output = format!("{notes}{}", result.output);
        }
        result
    }
}
