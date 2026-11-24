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

/// Encode a local image file to base64 for the AI model, downscaling and
/// re-encoding large images first.
///
/// The type comes from the content, not the extension, and anything that isn't
/// an image is refused: providers reject the whole request over one bad image,
/// and the image stays in the conversation.
pub fn encode_image_file(path: &std::path::Path) -> Result<(String, String)> {
    use image::ImageFormat;

    if path.is_dir() {
        eyre::bail!("it is a folder");
    }
    let bytes = std::fs::read(path)?;
    let reader = image::ImageReader::new(std::io::Cursor::new(&bytes)).with_guessed_format()?;
    let mime = match reader.format() {
        Some(ImageFormat::Png) => "image/png",
        Some(ImageFormat::Jpeg) => "image/jpeg",
        Some(ImageFormat::Gif) => "image/gif",
        Some(ImageFormat::WebP) => "image/webp",
        Some(ImageFormat::Bmp) => "image/bmp",
        Some(other) => {
            let kind = other.extensions_str().first().copied().unwrap_or("these");
            eyre::bail!(
                "{} images can't be viewed; convert it to PNG or JPEG first",
                kind.to_uppercase()
            )
        }
        None => eyre::bail!("it is not an image (PNG, JPEG, GIF, WebP or BMP)"),
    };
    if let Err(e) = reader.into_dimensions() {
        eyre::bail!("the image is damaged: {e}");
    }
    let encode = |data: &[u8]| base64::engine::general_purpose::STANDARD.encode(data);

    // Providers don't take BMP.
    if mime == "image/bmp" {
        let mut png = std::io::Cursor::new(Vec::new());
        image::load_from_memory(&bytes)?.write_to(&mut png, ImageFormat::Png)?;
        let png = png.into_inner();
        return Ok(match shrink_if_needed(&png) {
            Some((data, new_mime)) => (encode(&data), new_mime.into()),
            None => (encode(&png), "image/png".into()),
        });
    }
    // GIFs would lose their animation; other formats are safe to re-encode.
    if mime != "image/gif" {
        if let Some((data, new_mime)) = shrink_if_needed(&bytes) {
            return Ok((encode(&data), new_mime.into()));
        }
    }
    Ok((encode(&bytes), mime.to_string()))
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
        Some(e) if !e.is_empty() => Err(eyre::eyre!("OCR error: {e}")),
        _ => Err(eyre::eyre!("OCR returned no text")),
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
    fn the_content_decides_the_type() {
        let dir = std::env::temp_dir().join(format!("norvexum_vision_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let img = image::RgbImage::from_pixel(4, 4, image::Rgb([200, 30, 30]));
        let encode = |name: &str| encode_image_file(&dir.join(name));

        let png = dir.join("x.png");
        img.save(&png).unwrap();
        std::fs::copy(&png, dir.join("wrong.jpg")).unwrap();
        std::fs::copy(&png, dir.join("none")).unwrap();
        assert_eq!(encode("wrong.jpg").unwrap().1, "image/png");
        assert_eq!(encode("none").unwrap().1, "image/png");
        img.save(dir.join("x.bmp")).unwrap();
        assert_eq!(encode("x.bmp").unwrap().1, "image/png");

        let bytes = std::fs::read(&png).unwrap();
        std::fs::write(dir.join("cut.png"), &bytes[..12]).unwrap();
        std::fs::write(dir.join("text.png"), "hello").unwrap();
        std::fs::write(dir.join("empty.png"), "").unwrap();
        std::fs::write(dir.join("x.avif"), b"\0\0\0\x20ftypavif\0\0\0\0").unwrap();
        let why = |name: &str| encode(name).unwrap_err().to_string();
        assert!(why("cut.png").contains("damaged"));
        assert!(why("text.png").contains("not an image"));
        assert!(why("empty.png").contains("not an image"));
        assert!(why("x.avif").contains("AVIF"), "{}", why("x.avif"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
