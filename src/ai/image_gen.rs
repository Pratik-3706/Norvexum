// ═══════════════════════════════════════════════════════════════════════════
// Image Generation — Unified dispatcher across multiple free/paid providers
//
// Priority:
//   1. Gemini 3.1 Flash Image (free, Google AI Studio)
//   2. Pollinations / Flux (free, no key needed)
//   3. OpenAI DALL-E 3 (paid, if key available)
//
// The agent decides WHEN to generate vs scrape:
//   • "create/generate/make an image" → image generation
//   • "find/search/get an image of X" → image_search tool
// ═══════════════════════════════════════════════════════════════════════════

use eyre::Result;
use reqwest::Client;
use serde_json::json;

use super::types::{ImageGenRequest, ImageGenResult};
use crate::config::Settings;

/// Generate an image using the best available provider.
pub async fn generate_image(
    settings: &Settings,
    request: ImageGenRequest,
) -> Result<ImageGenResult> {
    // 1. Try Gemini 3.1 Flash Image (free)
    if settings.google_ai_api_key.is_some() {
        match generate_via_gemini(settings, &request).await {
            Ok(result) => return Ok(result),
            Err(e) => {
                tracing::warn!("Gemini image gen failed, trying fallback: {}", e);
            }
        }
    }

    // 2. Try Pollinations / Flux (free, no key needed)
    match generate_via_pollinations(&request).await {
        Ok(result) => return Ok(result),
        Err(e) => {
            tracing::warn!("Pollinations image gen failed: {}", e);
        }
    }

    // 3. Try OpenAI DALL-E 3 (paid)
    if settings.openai_api_key.is_some() {
        match generate_via_openai(settings, &request).await {
            Ok(result) => return Ok(result),
            Err(e) => {
                tracing::warn!("OpenAI image gen failed: {}", e);
            }
        }
    }

    eyre::bail!(
        "No image generation provider available. \
         Set GOOGLE_AI_API_KEY for free Gemini image gen, \
         or Pollinations API may be temporarily unavailable."
    )
}

/// Generate via Google Gemini 3.1 Flash Image (FREE).
async fn generate_via_gemini(
    settings: &Settings,
    request: &ImageGenRequest,
) -> Result<ImageGenResult> {
    let api_key = settings
        .google_ai_api_key
        .as_ref()
        .ok_or_else(|| eyre::eyre!("No Google AI API key"))?;

    let client = Client::new();
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.1-flash-image:generateContent?key={}",
        api_key
    );

    let body = json!({
        "contents": [{
            "parts": [{
                "text": format!("Generate an image: {}", request.prompt)
            }]
        }],
        "generationConfig": {
            "responseModalities": ["TEXT", "IMAGE"]
        }
    });

    let response = client
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;

    if !response.status().is_success() {
        let error = response.text().await.unwrap_or_default();
        eyre::bail!("Gemini image gen error: {}", error);
    }

    let result: serde_json::Value = response.json().await?;
    let mut images = Vec::new();

    if let Some(candidates) = result["candidates"].as_array() {
        for candidate in candidates {
            if let Some(parts) = candidate["content"]["parts"].as_array() {
                for part in parts {
                    if let Some(inline_data) = part.get("inlineData") {
                        let mime = inline_data["mimeType"].as_str().unwrap_or("image/png");
                        let data = inline_data["data"].as_str().unwrap_or("");
                        if !data.is_empty() {
                            images.push((data.to_string(), mime.to_string()));
                        }
                    }
                }
            }
        }
    }

    if images.is_empty() {
        eyre::bail!("Gemini returned no image data");
    }

    Ok(ImageGenResult {
        images,
        provider: "google_direct".into(),
        model: "gemini-3.1-flash-image".into(),
    })
}

/// Generate via Pollinations / Flux (FREE, no API key needed).
async fn generate_via_pollinations(request: &ImageGenRequest) -> Result<ImageGenResult> {
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;

    let encoded_prompt = urlencoding::encode(&request.prompt);
    let url = format!(
        "https://image.pollinations.ai/prompt/{}?width={}&height={}&model=flux&nologo=true",
        encoded_prompt, request.width, request.height
    );

    let response = client
        .get(&url)
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
        )
        .send()
        .await?;

    if !response.status().is_success() {
        eyre::bail!("Pollinations returned HTTP {}", response.status());
    }

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg")
        .split(';')
        .next()
        .unwrap_or("image/jpeg")
        .to_string();

    let bytes = response.bytes().await?;
    if bytes.is_empty() {
        eyre::bail!("Pollinations returned empty response");
    }

    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

    Ok(ImageGenResult {
        images: vec![(b64, content_type)],
        provider: "pollinations".into(),
        model: "flux".into(),
    })
}

/// Generate via OpenAI DALL-E 3 (paid).
async fn generate_via_openai(
    settings: &Settings,
    request: &ImageGenRequest,
) -> Result<ImageGenResult> {
    let api_key = settings
        .openai_api_key
        .as_ref()
        .ok_or_else(|| eyre::eyre!("No OpenAI API key"))?;

    let client = Client::new();
    let body = json!({
        "model": "dall-e-3",
        "prompt": request.prompt,
        "n": 1,
        "size": format!("{}x{}", request.width, request.height),
        "response_format": "b64_json",
    });

    let response = client
        .post("https://api.openai.com/v1/images/generations")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;

    if !response.status().is_success() {
        let error = response.text().await.unwrap_or_default();
        eyre::bail!("OpenAI DALL-E error: {}", error);
    }

    let result: serde_json::Value = response.json().await?;
    let mut images = Vec::new();

    if let Some(data) = result["data"].as_array() {
        for item in data {
            if let Some(b64) = item["b64_json"].as_str() {
                images.push((b64.to_string(), "image/png".to_string()));
            }
        }
    }

    if images.is_empty() {
        eyre::bail!("DALL-E returned no images");
    }

    Ok(ImageGenResult {
        images,
        provider: "openai".into(),
        model: "dall-e-3".into(),
    })
}
