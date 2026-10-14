// ===========================================================================
// Web Fetch — URL content extraction with anti-bot evasion
// ===========================================================================

use async_trait::async_trait;
use reqwest::Client;
use serde_json::json;

use super::{Tool, ToolContext, ToolResult};

fn build_client() -> Client {
    Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .expect("Failed to build HTTP client")
}

/// Extract readable text from HTML using scraper + heuristics.
fn extract_content(html: &str, max_chars: usize) -> String {
    // Try html2text first for clean conversion
    if let Ok(text) = html2text::from_read(html.as_bytes(), 120) {
        if text.trim().len() > 200 {
            return truncate_text(&text, max_chars);
        }
    }

    // Fallback: use scraper to find main content
    let document = scraper::Html::parse_document(html);

    // Try known content selectors
    let content_selectors = [
        "main",
        "article",
        "[role=main]",
        ".content",
        "#content",
        ".article",
        ".post-content",
        ".entry-content",
        ".article-body",
    ];

    for sel_str in &content_selectors {
        if let Ok(selector) = scraper::Selector::parse(sel_str) {
            if let Some(el) = document.select(&selector).next() {
                let text = el.text().collect::<Vec<_>>().join("\n");
                if text.trim().len() > 200 {
                    return truncate_text(&text, max_chars);
                }
            }
        }
    }

    // Last resort: collect all paragraph text
    if let Ok(p_sel) = scraper::Selector::parse("p") {
        let paragraphs: Vec<String> = document
            .select(&p_sel)
            .map(|el| el.text().collect::<String>())
            .filter(|t| t.trim().len() > 50)
            .collect();
        if !paragraphs.is_empty() {
            return truncate_text(&paragraphs.join("\n\n"), max_chars);
        }
    }

    let fallback_text = html2text::from_read(html.as_bytes(), 120).unwrap_or_default();
    truncate_text(&fallback_text, max_chars)
}

fn truncate_text(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        trimmed.to_string()
    } else {
        let truncated: String = trimmed.chars().take(max).collect();
        format!("{}...\n\n(truncated at {} chars)", truncated, max)
    }
}

/// Check if response looks like a Cloudflare/bot block.
fn is_blocked(status: u16, body: &str) -> bool {
    if matches!(status, 403 | 429 | 503) && (body.len() < 2048 || has_challenge_marker(body)) {
        return true;
    }
    status < 400 && body.len() < 4096 && has_challenge_marker(body)
}

fn has_challenge_marker(body: &str) -> bool {
    let cf_signals = [
        "cf-browser-verification",
        "cf_clearance",
        "Just a moment",
        "Enable JavaScript and cookies",
        "Checking your browser",
    ];
    cf_signals.iter().any(|sig| body.contains(sig))
}

pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL and return its readable text (HTML is converted to text; JSON and plain text are returned as-is)."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "URL to fetch" },
                "max_chars": { "type": "integer", "description": "Max characters to return (default: 8000)" }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let url_str = args["url"].as_str().unwrap_or("").trim();
        let max_chars = (args["max_chars"].as_u64().unwrap_or(8000) as usize)
            .clamp(500, ctx.settings.max_content_chars.max(8000));

        if url_str.is_empty() {
            return ToolResult::err("URL cannot be empty");
        }
        match url::Url::parse(url_str) {
            Ok(u) if matches!(u.scheme(), "http" | "https") => {}
            _ => return ToolResult::err("Only http(s) URLs can be fetched"),
        }

        let client = build_client();

        // Random delay
        let delay = rand::Rng::random_range(&mut rand::rng(), 100..500);
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;

        let ua = super::web_search::USER_AGENTS
            [rand::Rng::random_range(&mut rand::rng(), 0..super::web_search::USER_AGENTS.len())];

        let response = match client
            .get(url_str)
            .header("User-Agent", ua)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            // No manual Accept-Encoding: setting it disables reqwest's automatic
            // decompression and the body would arrive as compressed bytes.
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Referer", "https://www.google.com/")
            .header("DNT", "1")
            .header("Sec-Fetch-Dest", "document")
            .header("Sec-Fetch-Mode", "navigate")
            .header("Sec-Fetch-Site", "cross-site")
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return ToolResult::err(format!("Request failed: {}", e)),
        };

        let status = response.status().as_u16();
        let final_url = response.url().to_string();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();

        if content_type.starts_with("image/")
            || content_type.starts_with("video/")
            || content_type.starts_with("audio/")
            || content_type.contains("octet-stream")
            || content_type.contains("application/pdf")
            || content_type.contains("application/zip")
        {
            return ToolResult::err(format!(
                "{url_str} is binary content ({content_type}); use download_image for images."
            ));
        }

        let body = match response.text().await {
            Ok(t) => t,
            Err(e) => return ToolResult::err(format!("Failed to read response: {}", e)),
        };

        if is_blocked(status, &body) {
            return ToolResult::err(format!(
                "Blocked by bot protection (HTTP {status}). Try browser_interact for this URL."
            ));
        }

        let is_html = content_type.contains("html")
            || (content_type.is_empty() && body.trim_start().starts_with('<'));
        let content = if is_html {
            extract_content(&body, max_chars)
        } else {
            truncate_text(&body, max_chars)
        };

        if status >= 400 {
            return ToolResult::err(format!(
                "HTTP {status} from {final_url}\n{}",
                truncate_text(&content, 1000)
            ));
        }
        if content.trim().is_empty() {
            return ToolResult::err("No readable content extracted from this page.");
        }

        let header = if final_url != url_str {
            format!("[{final_url} (redirected), HTTP {status}]\n\n")
        } else {
            format!("[HTTP {status}]\n\n")
        };
        ToolResult::ok(format!("{header}{content}"))
    }
}
