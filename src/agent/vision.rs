// ═══════════════════════════════════════════════════════════════════════════
// Vision — Image understanding with OCR fallback
// ═══════════════════════════════════════════════════════════════════════════

use base64::Engine;
use eyre::Result;

/// Encode a local image file to base64 for the AI model.
/// Compresses and resizes large images to speed up upload for vision models.
pub fn encode_image_file(path: &std::path::Path) -> Result<(String, String)> {
    let bytes = std::fs::read(path)?;
    let mime = match path.extension().and_then(|e| e.to_str()) {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("bmp") => "image/bmp",
        _ => "image/jpeg",
    };

    // If the image is large (> 300 KB) and not a gif (which loses animation on resize), resize/compress it
    if bytes.len() > 300_000 && mime != "image/gif" {
        if let Ok(img) = image::load_from_memory(&bytes) {
            let max_dim = 1024;
            let w = img.width();
            let h = img.height();

            if w > max_dim || h > max_dim {
                let resized = img.resize(max_dim, max_dim, image::imageops::FilterType::Triangle);
                let mut compressed = std::io::Cursor::new(Vec::new());
                if resized.write_to(&mut compressed, image::ImageFormat::Jpeg).is_ok() {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(compressed.get_ref());
                    return Ok((b64, "image/jpeg".to_string()));
                }
            } else {
                let mut compressed = std::io::Cursor::new(Vec::new());
                if img.write_to(&mut compressed, image::ImageFormat::Jpeg).is_ok() {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(compressed.get_ref());
                    return Ok((b64, "image/jpeg".to_string()));
                }
            }
        }
    }

    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok((b64, mime.to_string()))
}

/// Use OCR.space API to extract text from an image when the model lacks vision.
pub async fn ocr_image(api_key: &str, image_path: &std::path::Path) -> Result<String> {
    let bytes = std::fs::read(image_path)?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

    let mime = match image_path.extension().and_then(|e| e.to_str()) {
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        _ => "image/jpeg",
    };

    let client = reqwest::Client::new();
    let response = client
        .post("https://api.ocr.space/parse/image")
        .header("apikey", api_key)
        .form(&[
            ("base64Image", &format!("data:{};base64,{}", mime, b64)),
            ("language", &"eng".to_string()),
            ("isOverlayRequired", &"false".to_string()),
            ("OCREngine", &"2".to_string()), // Engine 2 = better accuracy
        ])
        .send()
        .await?;

    let data: serde_json::Value = response.json().await?;

    if let Some(results) = data["ParsedResults"].as_array() {
        let texts: Vec<&str> = results
            .iter()
            .filter_map(|r| r["ParsedText"].as_str())
            .collect();
        if !texts.is_empty() {
            return Ok(texts.join("\n"));
        }
    }

    if let Some(error) = data["ErrorMessage"].as_str() {
        eyre::bail!("OCR error: {}", error);
    }

    eyre::bail!("OCR returned no text")
}

/// OCR an image from a URL (no download needed).
pub async fn ocr_url(api_key: &str, image_url: &str) -> Result<String> {
    let client = reqwest::Client::new();
    let response = client
        .post("https://api.ocr.space/parse/image")
        .header("apikey", api_key)
        .form(&[
            ("url", image_url),
            ("language", "eng"),
            ("isOverlayRequired", "false"),
            ("OCREngine", "2"),
        ])
        .send()
        .await?;

    let data: serde_json::Value = response.json().await?;

    if let Some(results) = data["ParsedResults"].as_array() {
        let texts: Vec<&str> = results
            .iter()
            .filter_map(|r| r["ParsedText"].as_str())
            .collect();
        if !texts.is_empty() {
            return Ok(texts.join("\n"));
        }
    }

    eyre::bail!("OCR returned no text from URL")
}
