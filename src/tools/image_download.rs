// ═══════════════════════════════════════════════════════════════════════════
// Image Download — Single + batch download with anti-bot evasion
// ═══════════════════════════════════════════════════════════════════════════

use super::{Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde_json::json;

fn ext_from_mime(mime: &str) -> &str {
    match mime {
        "image/jpeg" | "image/jpg" => ".jpg",
        "image/png" => ".png",
        "image/webp" => ".webp",
        "image/gif" => ".gif",
        "image/avif" => ".avif",
        "image/bmp" => ".bmp",
        _ => ".jpg",
    }
}

fn safe_filename(name: &str) -> String {
    regex::Regex::new(r#"[\\/:*?"<>|]"#)
        .unwrap()
        .replace_all(name, "_")
        .trim()
        .to_string()
}

async fn download_image_bytes(url: &str, referer: &str) -> eyre::Result<(Vec<u8>, String)> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let ua = super::web_search::USER_AGENTS
        [rand::Rng::random_range(&mut rand::rng(), 0..super::web_search::USER_AGENTS.len())];

    let response = client
        .get(url)
        .header("User-Agent", ua)
        .header(
            "Referer",
            if referer.is_empty() {
                "https://www.google.com/"
            } else {
                referer
            },
        )
        .header(
            "Accept",
            "image/avif,image/webp,image/apng,image/*,*/*;q=0.8",
        )
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Sec-Fetch-Dest", "image")
        .header("Sec-Fetch-Mode", "no-cors")
        .send()
        .await?;

    if !response.status().is_success() {
        eyre::bail!("HTTP {} for {}", response.status(), url);
    }

    let mime = response
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
        eyre::bail!("Empty response body");
    }

    Ok((bytes.to_vec(), mime))
}

// ── Download Single Image ─────────────────────────────────────────────────

pub struct DownloadImageTool;

#[async_trait]
impl Tool for DownloadImageTool {
    fn name(&self) -> &str {
        "download_image"
    }

    fn description(&self) -> &str {
        "Download a single image from a URL. Bypasses common bot protections. \
         Saves to the project directory with auto-detected extension."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "Direct image URL" },
                "filename": { "type": "string", "description": "Base filename without extension" },
                "output_dir": { "type": "string", "description": "Output directory (default: current dir)" }
            },
            "required": ["url", "filename"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let url = args["url"].as_str().unwrap_or("");
        let filename = args["filename"].as_str().unwrap_or("image");
        let output_dir = args["output_dir"].as_str().unwrap_or(".");

        if url.is_empty() {
            return ToolResult::err("URL cannot be empty");
        }

        let out_dir = match super::filesystem::resolve_path(output_dir, ctx) {
            Ok(path) => path,
            Err(error) => return ToolResult::err(error),
        };
        if let Err(e) = std::fs::create_dir_all(&out_dir) {
            return ToolResult::err(format!("Failed to create output dir: {}", e));
        }

        match download_image_bytes(url, "").await {
            Ok((bytes, mime)) => {
                let ext = ext_from_mime(&mime);
                let safe_name = safe_filename(filename);
                let filepath = out_dir.join(format!("{}{}", safe_name, ext));

                match std::fs::write(&filepath, &bytes) {
                    Ok(_) => {
                        let kb = bytes.len() as f64 / 1024.0;
                        ToolResult::ok_with_data(
                            format!("✅ Downloaded {:.1} KB → {}", kb, filepath.display()),
                            json!({
                                "path": filepath.to_string_lossy(),
                                "size_kb": (kb * 10.0).round() / 10.0,
                                "mime": mime
                            }),
                        )
                    }
                    Err(e) => ToolResult::err(format!("Failed to write file: {}", e)),
                }
            }
            Err(e) => ToolResult::err(format!("Download failed: {}", e)),
        }
    }
}

// ── Batch Download Images ─────────────────────────────────────────────────

pub struct BatchDownloadImageTool;

#[async_trait]
impl Tool for BatchDownloadImageTool {
    fn name(&self) -> &str {
        "batch_download_images"
    }

    fn description(&self) -> &str {
        "Download multiple images from a list of URLs. Pass the 'images' array from image_search. \
         Downloads the top-N highest-scoring images automatically."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "images": {
                    "type": "array",
                    "description": "Array of image objects with 'image_url' field",
                    "items": { "type": "object" }
                },
                "top_n": { "type": "integer", "description": "How many to download (default: 3, max: 10)" },
                "output_dir": { "type": "string", "description": "Output directory (default: current dir)" },
                "prefix": { "type": "string", "description": "Filename prefix (default: 'image')" }
            },
            "required": ["images"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let images = match args["images"].as_array() {
            Some(arr) => arr,
            None => return ToolResult::err("'images' must be an array"),
        };

        let top_n = args["top_n"].as_u64().unwrap_or(3).min(10) as usize;
        let output_dir = args["output_dir"].as_str().unwrap_or(".");
        let prefix = args["prefix"].as_str().unwrap_or("image");

        let out_dir = match super::filesystem::resolve_path(output_dir, ctx) {
            Ok(path) => path,
            Err(error) => return ToolResult::err(error),
        };
        if let Err(e) = std::fs::create_dir_all(&out_dir) {
            return ToolResult::err(format!("Failed to create output dir: {}", e));
        }

        let mut downloaded = Vec::new();
        let mut failed = Vec::new();

        for (i, img) in images.iter().take(top_n).enumerate() {
            let url = img["image_url"]
                .as_str()
                .or_else(|| img["src"].as_str())
                .unwrap_or("");

            if url.is_empty() {
                failed.push(format!("[{}] No URL", i + 1));
                continue;
            }

            let source_url = img["source_url"].as_str().unwrap_or("");

            match download_image_bytes(url, source_url).await {
                Ok((bytes, mime)) => {
                    let ext = ext_from_mime(&mime);
                    let filename = format!("{}_{:02}{}", safe_filename(prefix), i + 1, ext);
                    let filepath = out_dir.join(&filename);

                    match std::fs::write(&filepath, &bytes) {
                        Ok(_) => {
                            let kb = bytes.len() as f64 / 1024.0;
                            downloaded.push(format!("[{}] ✅ {} ({:.1} KB)", i + 1, filename, kb));
                        }
                        Err(e) => failed.push(format!("[{}] Write failed: {}", i + 1, e)),
                    }
                }
                Err(e) => failed.push(format!("[{}] Download failed: {}", i + 1, e)),
            }

            // Small delay between downloads
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        let mut output = format!(
            "Downloaded {}/{} images to {}:\n",
            downloaded.len(),
            top_n,
            out_dir.display()
        );
        for d in &downloaded {
            output.push_str(&format!("  {}\n", d));
        }
        if !failed.is_empty() {
            output.push_str(&format!("\n{} failed:\n", failed.len()));
            for f in &failed {
                output.push_str(&format!("  {}\n", f));
            }
        }

        if downloaded.is_empty() {
            ToolResult::err(output)
        } else {
            ToolResult::ok_with_data(
                output,
                json!({
                    "downloaded": downloaded.len(),
                    "failed": failed.len(),
                    "output_dir": out_dir.to_string_lossy()
                }),
            )
        }
    }
}
