// ═══════════════════════════════════════════════════════════════════════════
// Web Search — Tavily (primary) + DuckDuckGo (fallback)
//
// Anti-bot:
//   • Realistic User-Agent rotation
//   • Random delays between requests
//   • Proper referer chains
//   • DDG HTML endpoint (less detection than API)
// ═══════════════════════════════════════════════════════════════════════════

use async_trait::async_trait;
use eyre::Result;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;

use super::{Tool, ToolContext, ToolResult};

/// Rotating User-Agent strings to avoid fingerprinting
pub const USER_AGENTS: &[&str] = &[
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:128.0) Gecko/20100101 Firefox/128.0",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15",
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36 Edg/124.0.0.0",
];

fn random_ua() -> &'static str {
    use rand::Rng;
    let idx = rand::rng().random_range(0..USER_AGENTS.len());
    USER_AGENTS[idx]
}

pub fn build_stealth_client() -> Client {
    Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .cookie_store(true)
        .user_agent(random_ua())
        .build()
        .expect("Failed to build HTTP client")
}

// ── Tavily Search ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TavilyResponse {
    results: Option<Vec<TavilyResult>>,
    answer: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TavilyResult {
    title: Option<String>,
    url: Option<String>,
    content: Option<String>,
    score: Option<f64>,
}

async fn search_tavily(
    api_key: &str,
    query: &str,
    num_results: usize,
) -> Result<Vec<SearchResult>> {
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| Client::new());
    let body = json!({
        "api_key": api_key,
        "query": query,
        "max_results": num_results,
        "include_answer": true,
        "search_depth": "basic"
    });

    let response = client
        .post("https://api.tavily.com/search")
        .json(&body)
        .send()
        .await?;

    let tavily: TavilyResponse = response.json().await?;
    let mut results = Vec::new();

    if let Some(items) = tavily.results {
        for item in items {
            results.push(SearchResult {
                title: item.title.unwrap_or_default(),
                url: item.url.unwrap_or_default(),
                snippet: item.content.unwrap_or_default(),
                score: item.score.unwrap_or(0.0),
            });
        }
    }

    Ok(results)
}

// ── DuckDuckGo HTML Search (fallback) ─────────────────────────────────────

async fn search_ddg(query: &str, num_results: usize) -> Result<Vec<SearchResult>> {
    let client = build_stealth_client();

    // Small random delay to avoid rate limiting
    let delay = rand::Rng::random_range(&mut rand::rng(), 200..800);
    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;

    let response = client
        .post("https://html.duckduckgo.com/html/")
        .header("Referer", "https://duckduckgo.com/")
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Accept-Encoding", "gzip, deflate, br")
        .header("Origin", "https://duckduckgo.com")
        .form(&[("q", query), ("kp", "-2")]) // kp=-2 = safe-search OFF
        .send()
        .await?;

    let html = response.text().await?;
    parse_ddg_results(&html, num_results)
}

fn parse_ddg_results(html: &str, max: usize) -> Result<Vec<SearchResult>> {
    let document = scraper::Html::parse_document(html);
    let result_selector = scraper::Selector::parse(".result").unwrap();
    let title_selector = scraper::Selector::parse(".result__title a").unwrap();
    let snippet_selector = scraper::Selector::parse(".result__snippet").unwrap();

    let mut results = Vec::new();

    for element in document.select(&result_selector).take(max * 2) {
        let title_el = element.select(&title_selector).next();
        let snippet_el = element.select(&snippet_selector).next();

        if let Some(title_a) = title_el {
            let title = title_a.text().collect::<String>().trim().to_string();
            let href = title_a.value().attr("href").unwrap_or("");

            // Decode DDG redirect URL
            let url = if href.contains("uddg=") {
                url::Url::parse(href)
                    .ok()
                    .and_then(|u| {
                        u.query_pairs()
                            .find(|(k, _)| k == "uddg")
                            .map(|(_, v)| v.to_string())
                    })
                    .unwrap_or_else(|| href.to_string())
            } else {
                href.to_string()
            };

            let snippet = snippet_el
                .map(|el| el.text().collect::<String>().trim().to_string())
                .unwrap_or_default();

            if !title.is_empty() && url.starts_with("http") {
                results.push(SearchResult {
                    title,
                    url,
                    snippet,
                    score: 0.0,
                });
            }
        }

        if results.len() >= max {
            break;
        }
    }

    Ok(results)
}

// ── Result scoring ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
    score: f64,
}

fn score_results(query: &str, results: &mut Vec<SearchResult>) {
    let query_words: std::collections::HashSet<String> = query
        .to_lowercase()
        .split_whitespace()
        .filter(|w| w.len() > 2)
        .map(|w| w.to_string())
        .collect();

    if query_words.is_empty() {
        return;
    }

    for result in results.iter_mut() {
        if result.score > 0.0 {
            continue; // Already scored (Tavily)
        }

        let title_words: std::collections::HashSet<String> = result
            .title
            .to_lowercase()
            .split_whitespace()
            .map(|w| w.to_string())
            .collect();

        let snippet_words: std::collections::HashSet<String> = result
            .snippet
            .to_lowercase()
            .split_whitespace()
            .map(|w| w.to_string())
            .collect();

        let title_matches = query_words.intersection(&title_words).count() as f64;
        let snippet_matches = query_words.intersection(&snippet_words).count() as f64;

        result.score = (title_matches * 3.0) + snippet_matches;

        // Penalize junk domains
        let junk = [
            "pinterest",
            "quora",
            "reddit.com/search",
            "shutterstock",
            "gettyimages",
        ];
        if junk.iter().any(|j| result.url.contains(j)) {
            result.score -= 2.0;
        }
    }

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

// ── Tool Implementation ──────────────────────────────────────────────────

pub struct WebSearchTool;

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web. Uses Tavily (if API key set) or DuckDuckGo (free fallback). \
         Returns page titles, URLs, and snippets ranked by relevance. \
         Follow up with web_fetch to read full page content."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" },
                "num_results": { "type": "integer", "description": "Number of results (default: 5, max: 10)" }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let query = args["query"].as_str().unwrap_or("");
        let num = args["num_results"].as_u64().unwrap_or(5).min(10) as usize;

        if query.is_empty() {
            return ToolResult::err("Search query cannot be empty");
        }

        let mut results: Vec<SearchResult>;

        // Try Tavily first
        if let Some(api_key) = &ctx.settings.tavily_api_key {
            match search_tavily(api_key, query, num).await {
                Ok(r) if !r.is_empty() => {
                    results = r;
                    score_results(query, &mut results);
                    let output = format_results(&results[..num.min(results.len())], "Tavily");
                    return ToolResult::ok_with_data(
                        output,
                        json!({
                            "query": query, "count": results.len(), "source": "tavily"
                        }),
                    );
                }
                Ok(_) => {} // Empty results, fall through
                Err(e) => {
                    tracing::warn!("Tavily search failed: {}, falling back to DDG", e);
                }
            }
        }

        // Fallback: DuckDuckGo
        match search_ddg(query, num * 3).await {
            Ok(r) if !r.is_empty() => {
                results = r;
                score_results(query, &mut results);
                let output = format_results(&results[..num.min(results.len())], "DuckDuckGo");
                ToolResult::ok_with_data(
                    output,
                    json!({
                        "query": query, "count": results.len(), "source": "duckduckgo"
                    }),
                )
            }
            Ok(_) => ToolResult::err(format!("No results found for: {}", query)),
            Err(e) => ToolResult::err(format!("Search failed: {}", e)),
        }
    }
}

fn format_results(results: &[SearchResult], source: &str) -> String {
    let mut lines = vec![format!("Search results [{}]:\n", source)];
    for (i, r) in results.iter().enumerate() {
        let snippet: String = r.snippet.chars().take(200).collect();
        lines.push(format!(
            "[{}] **{}**\n    {}\n    {}",
            i, r.title, r.url, snippet
        ));
    }
    lines.join("\n\n")
}
