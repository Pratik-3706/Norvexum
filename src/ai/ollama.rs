// ═══════════════════════════════════════════════════════════════════════════
// Ollama — Local model support via Ollama's OpenAI-compatible API
//
// Auto-discovers locally available models by querying the Ollama API.
// Reuses the OpenAI-compatible client since Ollama supports that protocol.
// ═══════════════════════════════════════════════════════════════════════════

use serde::Deserialize;

use crate::config::providers::{ApiStyle, ModelInfo, ProviderInfo};

/// Default Ollama API base URL.
pub const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

/// Response from Ollama's /api/tags endpoint.
#[derive(Debug, Deserialize)]
struct OllamaTagsResponse {
    models: Option<Vec<OllamaModel>>,
}

#[derive(Debug, Deserialize)]
struct OllamaModel {
    name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    details: Option<OllamaModelDetails>,
}

#[derive(Debug, Deserialize)]
struct OllamaModelDetails {
    family: Option<String>,
    parameter_size: Option<String>,
}

/// Discover locally available Ollama models.
/// Returns None if Ollama is not running or unreachable.
pub async fn discover_models(base_url: &str) -> Option<Vec<ModelInfo>> {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .ok()?;

    let response = client.get(&url).send().await.ok()?;

    if !response.status().is_success() {
        return None;
    }

    let tags: OllamaTagsResponse = response.json().await.ok()?;
    let models = tags.models?;

    if models.is_empty() {
        return None;
    }

    let model_infos: Vec<ModelInfo> = models
        .into_iter()
        .map(|m| {
            let family = m
                .details
                .as_ref()
                .and_then(|d| d.family.clone())
                .unwrap_or_else(|| extract_family(&m.name));

            let is_vision = m.name.contains("llava")
                || m.name.contains("vision")
                || m.name.contains("bakllava")
                || m.name.contains("moondream");

            let context_window = estimate_context_window(&m.name, m.size);

            ModelInfo {
                id: m.name,
                family,
                multimodal: is_vision,
                image_gen: false,
                tool_calling: true, // Most recent Ollama models support tool calling
                streaming: true,
                context_window,
                cost_per_1k_input: 0.0,
                cost_per_1k_output: 0.0,
            }
        })
        .collect();

    Some(model_infos)
}

/// Build an Ollama provider entry with discovered models.
pub async fn build_ollama_provider(base_url: &str) -> Option<ProviderInfo> {
    let models = discover_models(base_url).await?;

    Some(ProviderInfo {
        name: "ollama".into(),
        display_name: "Ollama (Local)".into(),
        base_url: format!("{}/v1", base_url.trim_end_matches('/')),
        api_style: ApiStyle::OpenAiCompat,
        env_key: "OLLAMA_BASE_URL".into(), // Not really a key, just for display
        models,
    })
}

/// Check if Ollama is running and accessible.
pub async fn is_available(base_url: &str) -> bool {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build();

    match client {
        Ok(c) => c
            .get(&url)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false),
        Err(_) => false,
    }
}

fn extract_family(name: &str) -> String {
    // Extract family from model name like "llama3.1:latest" → "llama"
    let base = name.split(':').next().unwrap_or(name);
    let family = base
        .chars()
        .take_while(|c| c.is_alphabetic())
        .collect::<String>();
    if family.is_empty() {
        base.to_string()
    } else {
        family
    }
}

fn estimate_context_window(name: &str, _size: u64) -> usize {
    // Rough estimates based on common model names
    if name.contains("llama3") || name.contains("llama-3") {
        128_000
    } else if name.contains("mistral") || name.contains("mixtral") {
        32_000
    } else if name.contains("gemma") {
        8_192
    } else if name.contains("phi") {
        4_096
    } else if name.contains("qwen") {
        32_000
    } else if name.contains("deepseek") {
        64_000
    } else {
        4_096 // Conservative default
    }
}
