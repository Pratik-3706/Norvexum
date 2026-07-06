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
        "Search for real-world images, photos, and general web graphics using DuckDuckGo. Be specific with your query."
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

        // Build a dedicated reqwest client with cookie store enabled for DuckDuckGo
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .cookie_store(true)
            .build()
            .unwrap_or_else(|_| super::web_search::build_stealth_client());

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
                    return format_image_results(query, &top, "ddg");
                }
                _ => {} // Fall through to HTML parsing
            }
        }

        // Fallback: extract from HTML page directly
        let images = extract_images_from_html(&html, num);
        if images.is_empty() {
            return ToolResult::err(format!("No image results found for: {}", query));
        }

        format_image_results(query, &images, "ddg")
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
        for item in items {
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

fn score_images(query: &str, images: Vec<ImageResult>) -> Vec<ImageResult> {
    let mut scored = images;
    let query_words: Vec<String> = query
        .to_lowercase()
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
        .filter(|w| !w.is_empty())
        .collect();

    for img in &mut scored {
        let title_lower = img.title.to_lowercase();
        let mut score = 0.0;
        for word in &query_words {
            if title_lower.contains(word) {
                score += 1.0;
            }
        }
        img.score = score;
    }

    scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    scored
}

fn extract_images_from_html(html: &str, count: usize) -> Vec<ImageResult> {
    let mut results = Vec::new();
    let re = regex::Regex::new(r#"image"\s*:\s*"([^"]+)""#).ok();
    if let Some(r) = re {
        for cap in r.captures_iter(html).take(count) {
            let img_url = cap[1].to_string();
            results.push(ImageResult {
                image_url: img_url.clone(),
                title: String::new(),
                source_url: img_url,
                width: 0,
                height: 0,
                score: 0.0,
            });
        }
    }
    results
}

fn format_image_results(query: &str, results: &[ImageResult], source: &str) -> ToolResult {
    let mut lines = Vec::new();
    lines.push(format!(
        "Found {} image(s) from {} for '{}':",
        results.len(),
        source,
        query
    ));

    for (i, img) in results.iter().enumerate() {
        lines.push(format!(
            "{}. [{}]({}) (Size: {}x{}, Source: {})",
            i + 1,
            if img.title.is_empty() {
                format!("Image {}", i + 1)
            } else {
                img.title.clone()
            },
            img.image_url,
            img.width,
            img.height,
            img.source_url
        ));
    }

    ToolResult::ok_with_data(
        lines.join("\n"),
        json!({ "images": results, "count": results.len(), "source": source }),
    )
}

fn capitalize_tag(s: &str) -> String {
    s.split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
            }
        })
        .collect::<Vec<String>>()
        .join(" ")
}

async fn fetch_zerochan_images(
    client: &reqwest::Client,
    query: &str,
    limit: usize,
) -> Option<Vec<ImageResult>> {
    let stop_words: std::collections::HashSet<&str> = [
        "a",
        "an",
        "the",
        "cool",
        "website",
        "images",
        "image",
        "photo",
        "photos",
        "with",
        "having",
        "and",
        "or",
        "in",
        "on",
        "at",
        "to",
        "for",
        "make",
        "create",
        "find",
        "search",
        "show",
        "me",
        "beautiful",
        "elegant",
        "cute",
        "of",
        "some",
        "nice",
        "designed",
    ]
    .iter()
    .cloned()
    .collect();

    // Clean query of search operators, domain names, and the word "zerochan" itself
    let cleaned = query
        .to_lowercase()
        .replace("site:zerochan.net", "")
        .replace("site:zerochan.org", "")
        .replace("site:zerochan.com", "")
        .replace("zerochan.net", "")
        .replace("zerochan.org", "")
        .replace("zerochan.com", "")
        .replace("zerochan", "")
        .replace("site:", "");

    // 1. Parse and format tags: split by comma if present, otherwise split by whitespace.
    let has_commas = cleaned.contains(',');
    let raw_tags: Vec<&str> = if has_commas {
        cleaned.split(',').collect()
    } else {
        cleaned.split_whitespace().collect()
    };

    let tags: Vec<String> = raw_tags
        .into_iter()
        .map(|w| w.trim())
        .filter(|w| {
            !w.is_empty() && w.len() > 1 && !stop_words.contains(&w.to_lowercase().as_str())
        })
        .map(|w| capitalize_tag(w))
        .collect();

    if tags.is_empty() {
        return None;
    }

    // 2. Query Zerochan using tags joined by commas
    let search_path = tags.join(",");
    let mut items = query_zerochan_raw(client, &search_path, limit).await;

    // Fallback: if multi-tag search returns nothing, try querying the first tag alone
    if items.as_ref().map_or(true, |it| it.is_empty()) && tags.len() > 1 {
        items = query_zerochan_raw(client, &tags[0], limit).await;
    }

    let items = items?;
    let mut results = Vec::new();

    for item in items {
        let id = item["id"].as_u64().unwrap_or(0);
        let mut img_url = item["thumbnail"].as_str().unwrap_or("").to_string();
        if img_url.is_empty() {
            continue;
        }

        // 1. Change extension to .jpg
        if img_url.ends_with(".avif") {
            img_url = img_url[..img_url.len() - 5].to_string() + ".jpg";
        }
        // 2. Change /240/ to /600/
        if img_url.contains("/240/") {
            img_url = img_url.replace("/240/", "/600/");
        }
        // 3. Normalize CDN domain to s1.zerochan.net for JPG hosting
        if img_url.contains("s2.zerochan.net") {
            img_url = img_url.replace("s2.zerochan.net", "s1.zerochan.net");
        } else if img_url.contains("s3.zerochan.net") {
            img_url = img_url.replace("s3.zerochan.net", "s1.zerochan.net");
        } else if img_url.contains("s4.zerochan.net") {
            img_url = img_url.replace("s4.zerochan.net", "s1.zerochan.net");
        }

        let tags_list: Vec<String> = item["tags"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let title = if tags_list.is_empty() {
            format!("Zerochan Image #{}", id)
        } else {
            tags_list.join(", ")
        };

        results.push(ImageResult {
            image_url: img_url,
            title,
            source_url: format!("https://www.zerochan.net/{}", id),
            width: item["width"].as_u64().unwrap_or(0) as u32,
            height: item["height"].as_u64().unwrap_or(0) as u32,
            score: 0.0,
        });
    }

    if results.is_empty() {
        None
    } else {
        Some(results)
    }
}

async fn query_zerochan_raw(
    client: &reqwest::Client,
    search_path: &str,
    limit: usize,
) -> Option<Vec<serde_json::Value>> {
    // Zerochan API docs: User-Agent MUST be "ProjectName - Username", NOT a browser UA.
    // Browser-like UAs trigger their nginx anti-bot challenge (503).
    let user_agent = "Norvexum - ZerochanAPIUser";

    // Encode spaces as + for Zerochan tag URLs, but keep commas intact for multi-tag search
    let encoded_path = search_path.replace(' ', "+");
    let mut current_url = format!(
        "https://www.zerochan.net/{}?json&l={}&s=fav",
        encoded_path, limit
    );

    let mut attempts = 0;
    while attempts < 3 {
        let response = client
            .get(&current_url)
            .header("User-Agent", user_agent)
            .header("Accept", "application/json")
            .send()
            .await
            .ok()?;

        let status = response.status();
        if status.is_success() {
            let data: serde_json::Value = response.json().await.ok()?;
            return data["items"].as_array().cloned();
        } else if status.is_redirection() {
            if let Some(loc) = response.headers().get("location") {
                if let Ok(loc_str) = loc.to_str() {
                    let absolute_loc = if loc_str.starts_with('/') {
                        format!("https://www.zerochan.net{}", loc_str)
                    } else {
                        loc_str.to_string()
                    };

                    // Zerochan redirects strip query parameters, so we must re-append them
                    let base_url = absolute_loc.split('?').next().unwrap_or(&absolute_loc);
                    current_url = format!("{}?json&l={}&s=fav", base_url, limit);
                    attempts += 1;
                    continue;
                }
            }
        }
        break;
    }

    None
}

pub struct ZerochanSearchTool;

#[async_trait]
impl Tool for ZerochanSearchTool {
    fn name(&self) -> &str {
        "zerochan_search"
    }

    fn description(&self) -> &str {
        "Search for high-quality artwork of anime and anime games characters only (like gacha games) on Zerochan. \
         Supports comma-separated tag search (e.g. 'Genshin Impact, Furina')."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Anime/anime game character search query (e.g. 'Genshin Impact, Furina' or just 'Furina')" },
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

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        if let Some(zerochan_results) = fetch_zerochan_images(&client, query, num).await {
            if !zerochan_results.is_empty() {
                let scored = score_images(query, zerochan_results);
                let top: Vec<_> = scored.into_iter().take(num).collect();
                return format_image_results(query, &top, "zerochan");
            }
        }

        ToolResult::err(format!("No image results found on Zerochan for: {}", query))
    }
}
