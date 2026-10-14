// ===========================================================================
// Vision — Image understanding with OCR fallback
// ===========================================================================

use base64::Engine;
use eyre::Result;

/// Longest edge sent to vision models. Larger images cost more tokens without
/// helping (providers downscale them anyway) and can exceed request limits.
const MAX_DIMENSION: u32 = 2048;
/// Raw-size threshold above which an image is always re-encoded.
const MAX_BYTES: usize = 3_500_000;

pub fn mime_for_path(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("bmp") => "image/bmp",
        _ => "image/jpeg",
    }
}

/// Encode a local image file to base64 for the AI model, downscaling and
/// re-encoding large images first.
pub fn encode_image_file(path: &std::path::Path) -> Result<(String, String)> {
    let bytes = std::fs::read(path)?;
    let mime = mime_for_path(path);

    // GIFs would lose their animation; other formats are safe to re-encode.
    if mime != "image/gif" {
        if let Some((data, new_mime)) = shrink_if_needed(&bytes) {
            return Ok((
                base64::engine::general_purpose::STANDARD.encode(data),
                new_mime.into(),
            ));
        }
    }

    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok((b64, mime.to_string()))
}

/// Re-encode when the image is too large in bytes or pixels. `None` means the
/// original bytes are fine (or couldn't be decoded — let the provider decide).
fn shrink_if_needed(bytes: &[u8]) -> Option<(Vec<u8>, &'static str)> {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let (w, h) = reader.into_dimensions().ok()?;
    if bytes.len() <= MAX_BYTES && w <= MAX_DIMENSION && h <= MAX_DIMENSION {
        return None;
    }

    let img = image::load_from_memory(bytes).ok()?;
    let img = if w > MAX_DIMENSION || h > MAX_DIMENSION {
        img.resize(
            MAX_DIMENSION,
            MAX_DIMENSION,
            image::imageops::FilterType::Triangle,
        )
    } else {
        img
    };

    let mut out = std::io::Cursor::new(Vec::new());
    if img.color().has_alpha() {
        // Keep transparency (UI screenshots, logos).
        img.write_to(&mut out, image::ImageFormat::Png).ok()?;
        let data = out.into_inner();
        if data.len() < bytes.len() || w > MAX_DIMENSION || h > MAX_DIMENSION {
            return Some((data, "image/png"));
        }
        return None;
    }
    img.to_rgb8()
        .write_to(&mut out, image::ImageFormat::Jpeg)
        .ok()?;
    Some((out.into_inner(), "image/jpeg"))
}

/// Use OCR.space API to extract text from an image when the model lacks vision.
pub async fn ocr_image(api_key: &str, image_path: &std::path::Path) -> Result<String> {
    let (b64, mime) = encode_image_file(image_path)?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let data_uri = format!("data:{mime};base64,{b64}");
    let response = client
        .post("https://api.ocr.space/parse/image")
        .header("apikey", api_key.trim())
        .form(&[
            ("base64Image", data_uri.as_str()),
            ("language", "eng"),
            ("isOverlayRequired", "false"),
            ("OCREngine", "2"), // Engine 2 = better accuracy
        ])
        .send()
        .await?;

    let data: serde_json::Value = response.json().await?;

    if let Some(results) = data["ParsedResults"].as_array() {
        let texts: Vec<&str> = results
            .iter()
            .filter_map(|r| r["ParsedText"].as_str())
            .filter(|t| !t.trim().is_empty())
            .collect();
        if !texts.is_empty() {
            return Ok(texts.join("\n"));
        }
    }

    // ErrorMessage is a string or an array of strings depending on the error.
    let error = match &data["ErrorMessage"] {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(items) => Some(
            items
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join("; "),
        ),
        _ => None,
    };
    match error {
        Some(e) if !e.is_empty() => eyre::bail!("OCR error: {e}"),
        _ => eyre::bail!("OCR returned no text"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_images_are_downscaled() {
        let img = image::RgbImage::from_pixel(3000, 1000, image::Rgb([200, 30, 30]));
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let (data, mime) = shrink_if_needed(png.get_ref()).expect("should re-encode");
        assert_eq!(mime, "image/jpeg");
        let decoded = image::load_from_memory(&data).unwrap();
        assert_eq!(decoded.width(), MAX_DIMENSION);
    }

    #[test]
    fn mime_detection_ignores_case() {
        assert_eq!(mime_for_path(std::path::Path::new("SHOT.PNG")), "image/png");
        assert_eq!(mime_for_path(std::path::Path::new("a.JpEg")), "image/jpeg");
    }
}
