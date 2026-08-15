use reqwest::Client;
use serde_json::Value;

use crate::config::providers::{ModelInfo, Vision};

const ANTHROPIC_BASE: &str = "https://api.anthropic.com/v1";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";
const AICREDITS_BASE: &str = "https://api.aicredits.in/api";
const GEMINI_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

pub async fn fetch_anthropic_models(
    client: &Client,
    api_key: &str,
) -> eyre::Result<Vec<ModelInfo>> {
    let mut out = Vec::new();
    let mut after_id: Option<String> = None;

    loop {
        let mut url = format!("{ANTHROPIC_BASE}/models?limit=100");
        if let Some(id) = &after_id {
            url.push_str(&format!("&after_id={id}"));
        }

        let list: Value = client
            .get(&url)
            .header("x-api-key", api_key)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .send()
            .await?
            .json()
            .await?;

        let data = list["data"].as_array().cloned().unwrap_or_default();
        if data.is_empty() {
            break;
        }

        for entry in &data {
            let id = entry["id"].as_str().unwrap_or_default().to_string();

            // Fetch detail to get capabilities
            let detail: Value = client
                .get(format!("{ANTHROPIC_BASE}/models/{id}"))
                .header("x-api-key", api_key)
                .header("anthropic-version", ANTHROPIC_API_VERSION)
                .send()
                .await?
                .json()
                .await?;

            let caps = &detail["capabilities"];

            let vision = match caps["image_input"]["supported"].as_bool() {
                Some(true) => Vision::Yes,
                Some(false) => Vision::No,
                None => Vision::Unknown,
            };

            let adaptive_thinking = caps["thinking"]["types"]["adaptive"]["supported"]
                .as_bool()
                .unwrap_or(false);

            let effort_levels = ["low", "medium", "high", "xhigh", "max"]
                .iter()
                .filter(|lvl| {
                    caps["effort"][**lvl]["supported"]
                        .as_bool()
                        .unwrap_or(false)
                })
                .map(|s| s.to_string())
                .collect();

            out.push(ModelInfo {
                id,
                family: "claude".into(),
                vision,
                image_gen: false,
                tool_calling: true, // Most Claude models support tool calling
                streaming: true,
                context_window: detail["max_input_tokens"].as_u64().unwrap_or(200_000) as usize,
                cost_per_1k_input: 0.0,
                cost_per_1k_output: 0.0,
                adaptive_thinking,
                effort_levels,
            });
        }

        let has_more = list["has_more"].as_bool().unwrap_or(false);
        if !has_more {
            break;
        }
        after_id = data.last().and_then(|e| e["id"].as_str()).map(String::from);
    }

    Ok(out)
}

pub async fn fetch_aicredits_models(client: &Client) -> eyre::Result<Vec<ModelInfo>> {
    let resp: Value = client
        .get(format!("{AICREDITS_BASE}/models"))
        .send()
        .await?
        .json()
        .await?;

    let mut out = Vec::new();
    if let Some(data) = resp["data"].as_array() {
        for m in data {
            let modalities: Vec<&str> = m["input_modalities"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();

            let vision = if modalities.is_empty() {
                Vision::Unknown
            } else if modalities.contains(&"image") {
                Vision::Yes
            } else {
                Vision::No
            };

            let id = m["id"].as_str().unwrap_or_default().to_string();
            let family = if id.contains("claude") {
                "claude"
            } else if id.contains("gpt") {
                "gpt"
            } else if id.contains("gemini") {
                "gemini"
            } else {
                "other"
            };

            out.push(ModelInfo {
                id,
                family: family.into(),
                vision,
                image_gen: false, // Could infer from modalities if aicredits supports it
                tool_calling: true,
                streaming: true,
                context_window: m["context_length"].as_u64().unwrap_or(128_000) as usize,
                cost_per_1k_input: m["pricing"]["prompt"].as_f64().unwrap_or(0.0) * 1000.0,
                cost_per_1k_output: m["pricing"]["completion"].as_f64().unwrap_or(0.0) * 1000.0,
                adaptive_thinking: false,
                effort_levels: vec![],
            });
        }
    }
    Ok(out)
}

pub async fn fetch_gemini_models(client: &Client, api_key: &str) -> eyre::Result<Vec<ModelInfo>> {
    let mut out = Vec::new();
    let mut page_token: Option<String> = None;

    loop {
        let mut url = format!("{GEMINI_BASE}/models?pageSize=100");
        if let Some(t) = &page_token {
            url.push_str(&format!("&pageToken={t}"));
        }

        let resp: Value = client
            .get(&url)
            .header("x-goog-api-key", api_key)
            .send()
            .await?
            .json()
            .await?;

        for m in resp["models"].as_array().unwrap_or(&vec![]) {
            let name = m["name"].as_str().unwrap_or_default();
            let id = name.trim_start_matches("models/").to_string();

            let methods: Vec<&str> = m["supportedGenerationMethods"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();

            let vision = if !methods.contains(&"generateContent") {
                Vision::No
            } else if id.starts_with("gemini-") {
                Vision::Yes
            } else {
                Vision::Unknown
            };

            out.push(ModelInfo {
                id,
                family: "gemini".into(),
                vision,
                image_gen: false,
                tool_calling: true,
                streaming: true,
                context_window: m["inputTokenLimit"].as_u64().unwrap_or(128_000) as usize,
                cost_per_1k_input: 0.0,
                cost_per_1k_output: 0.0,
                adaptive_thinking: false,
                effort_levels: vec![],
            });
        }

        page_token = resp["nextPageToken"].as_str().map(String::from);
        if page_token.is_none() {
            break;
        }
    }

    Ok(out)
}

pub fn spawn_background_fetch(settings: &crate::config::Settings) {
    let anthropic_key = settings.anthropic_api_key.clone();
    let gemini_key = settings.google_ai_api_key.clone();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let client = Client::new();
            let mut all_providers = crate::config::providers::build_static_registry();

            // AICredits
            if let Ok(models) = fetch_aicredits_models(&client).await {
                if let Some(p) = all_providers.iter_mut().find(|p| p.name == "aicredits") {
                    p.models = models;
                }
            }

            // Anthropic
            if let Some(key) = anthropic_key.filter(|k| !k.is_empty()) {
                if let Ok(models) = fetch_anthropic_models(&client, &key).await {
                    if let Some(p) = all_providers.iter_mut().find(|p| p.name == "anthropic") {
                        p.models = models;
                    }
                }
            }

            // Gemini
            if let Some(key) = gemini_key.filter(|k| !k.is_empty()) {
                if let Ok(models) = fetch_gemini_models(&client, &key).await {
                    if let Some(p) = all_providers.iter_mut().find(|p| p.name == "google_direct") {
                        p.models = models;
                    }
                }
            }

            crate::config::providers::update_dynamic_registry(all_providers);
        });
    });
}
