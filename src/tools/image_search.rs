// ═══════════════════════════════════════════════════════════════════════════
// Image Search — DDG image search with relevance scoring
// ═══════════════════════════════════════════════════════════════════════════

use async_trait::async_trait;
use serde_json::json;

use super::{Tool, ToolContext, ToolResult};

pub struct ImageSearchTool;

#[async_trait]
impl Tool for ImageSearchTool {
    fn name(&self) -> &str {
        "image_search"
    }

    fn description(&self) -> &str {
        "Search for images using DuckDuckGo. Returns direct image URLs ranked by relevance. \
         Be specific: include subject AND context (e.g. 'sunset over ocean landscape photo')."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Image search query (be specific)" },
                "num_results": { "type": "integer", "description": "Results to return (default: 5, max: 20)" }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let query = args["query"].as_str().unwrap_or("");
        let num = args["num_results"].as_u64().unwrap_or(5).min(20) as usize;

        if query.is_empty() {
            return ToolResult::err("Search query cannot be empty");
        }

        // Use DDG image search HTML page and extract image URLs
        let client = super::web_search::build_stealth_client();

        let delay = rand::Rng::random_range(&mut rand::rng(), 300..900);
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;

        let encoded = urlencoding::encode(query);
        let url = format!(
            "https://duckduckgo.com/?q={}&iax=images&ia=images&kp=-2",
            encoded
        );

        let response = match client
            .get(&url)
            .header("Referer", "https://duckduckgo.com/")
            .header("Accept", "text/html,application/xhtml+xml")
            .header("Accept-Language", "en-US,en;q=0.9")
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return ToolResult::err(format!("Image search failed: {}", e)),
        };

        let html = match response.text().await {
            Ok(t) => t,
            Err(e) => return ToolResult::err(format!("Failed to read response: {}", e)),
        };

        // Extract vqd token for /i.js API
        let vqd = extract_vqd(&html);

        if let Some(vqd_token) = vqd {
            // Use DDG's JSON image API
            match fetch_image_json(&client, query, &vqd_token, num * 4).await {
                Ok(results) if !results.is_empty() => {
                    let scored = score_images(query, results);
                    let top: Vec<_> = scored.into_iter().take(num).collect();
                    return format_image_results(query, &top);
                }
                _ => {} // Fall through to HTML parsing
            }
        }

        // Fallback: extract from HTML page directly
        let images = extract_images_from_html(&html, num);
        if images.is_empty() {
            return ToolResult::err(format!("No image results found for: {}", query));
        }

        format_image_results(query, &images)
    }
}

fn extract_vqd(html: &str) -> Option<String> {
    let re = regex::Regex::new(r#"vqd[='":\s]+([0-9-]+)"#).ok()?;
    re.captures(html).map(|c| c[1].to_string())
}

#[derive(Debug, Clone, serde::Serialize)]
struct ImageResult {
    image_url: String,
    title: String,
    source_url: String,
    width: u32,
    height: u32,
    score: f64,
}

async fn fetch_image_json(
    client: &reqwest::Client,
    query: &str,
    vqd: &str,
    count: usize,
) -> eyre::Result<Vec<ImageResult>> {
    let encoded = urlencoding::encode(query);
    let api_url = format!(
        "https://duckduckgo.com/i.js?q={}&vqd={}&p=-1&o=json&s=0&u=bing&f=,,,,,&l=us-en",
        encoded, vqd
    );

    let response = client
        .get(&api_url)
        .header("Accept", "application/json")
        .header("Referer", "https://duckduckgo.com/")
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .await?;

    let data: serde_json::Value = response.json().await?;

    let mut results = Vec::new();
    if let Some(items) = data["results"].as_array() {
        for item in items.iter().take(count) {
            let img_url = item["image"].as_str().unwrap_or("").to_string();
            if img_url.is_empty() {
                continue;
            }

            results.push(ImageResult {
                image_url: img_url,
                title: item["title"].as_str().unwrap_or("").to_string(),
                source_url: item["url"].as_str().unwrap_or("").to_string(),
                width: item["width"].as_u64().unwrap_or(0) as u32,
                height: item["height"].as_u64().unwrap_or(0) as u32,
                score: 0.0,
            });
        }
    }

    Ok(results)
}

fn score_images(query: &str, mut results: Vec<ImageResult>) -> Vec<ImageResult> {
    let query_words: std::collections::HashSet<String> = query
        .to_lowercase()
        .split_whitespace()
        .filter(|w| w.len() > 2)
        .map(|w| w.to_string())
        .collect();

    let total = query_words.len() as f64;
    if total == 0.0 {
        return results;
    }

    for r in &mut results {
        let title_words: std::collections::HashSet<String> = r
            .title
            .to_lowercase()
            .split_whitespace()
            .map(|w| w.to_string())
            .collect();
        let url_words: std::collections::HashSet<String> = r
            .image_url
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .map(|w| w.to_string())
            .collect();

        let title_hits = query_words.intersection(&title_words).count() as f64 / total;
        let url_hits = query_words.intersection(&url_words).count() as f64 / total;

        let relevance = (title_hits * 65.0) + (url_hits * 20.0);

        let pixels = (r.width as u64) * (r.height as u64);
        let res_score = if pixels >= 1920 * 1080 {
            100.0
        } else if pixels >= 1280 * 720 {
            75.0
        } else if pixels >= 800 * 600 {
            50.0
        } else {
            25.0
        };

        r.score = (relevance * 0.8) + (res_score * 0.2);
    }

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results
}

fn extract_images_from_html(html: &str, max: usize) -> Vec<ImageResult> {
    let document = scraper::Html::parse_document(html);
    let img_selector = scraper::Selector::parse("img").unwrap();
    let mut results = Vec::new();

    for img in document.select(&img_selector).take(max * 3) {
        let src = img
            .value()
            .attr("data-src")
            .or_else(|| img.value().attr("src"))
            .unwrap_or("");

        if src.starts_with("http") && src.len() > 20 {
            results.push(ImageResult {
                image_url: src.to_string(),
                title: img.value().attr("alt").unwrap_or("").to_string(),
                source_url: String::new(),
                width: 0,
                height: 0,
                score: 0.0,
            });
        }

        if results.len() >= max {
            break;
        }
    }

    results
}

fn format_image_results(query: &str, results: &[ImageResult]) -> ToolResult {
    let mut lines = vec![format!(
        "Found {} image(s) for '{}':\n",
        results.len(),
        query
    )];
    for (i, img) in results.iter().enumerate() {
        let dims = if img.width > 0 && img.height > 0 {
            format!("{}×{}px", img.width, img.height)
        } else {
            "dims unknown".to_string()
        };
        lines.push(format!(
            "[{}] score={:.1}  {}\n     Image : {}\n     Title : {}",
            i,
            img.score,
            dims,
            img.image_url,
            if img.title.is_empty() {
                "(no title)"
            } else {
                &img.title
            }
        ));
    }

    ToolResult::ok_with_data(
        lines.join("\n"),
        json!({ "images": results, "count": results.len() }),
    )
}
