// ===========================================================================
// Ollama — Local model support via Ollama's OpenAI-compatible API
//
// Auto-discovers locally available models by querying the Ollama API.
// Reuses the OpenAI-compatible client since Ollama supports that protocol.
// ===========================================================================

use serde::Deserialize;

use crate::config::providers::{ApiStyle, ModelInfo, ProviderInfo};

/// Default Ollama API base URL.
pub const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

pub static DISCOVERED_OLLAMA_MODELS: std::sync::Mutex<Option<Vec<ModelInfo>>> = std::sync::Mutex::new(None);

#[derive(Debug, Deserialize)]
struct TagsResponse {
    models: Option<Vec<ModelTag>>,
}

#[derive(Debug, Deserialize)]
struct ModelTag {
    name: String,
    size: u64,
    details: Option<OllamaModelDetails>,
}

#[derive(Debug, Deserialize)]
struct OllamaModelDetails {
    family: Option<String>,
    parameter_size: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ShowResponse {
    #[serde(default)]
    capabilities: Vec<String>,
}

/// Discover locally available Ollama models.
/// Returns None if Ollama is not running or unreachable.
pub async fn discover_models(base_url: &str) -> Option<Vec<ModelInfo>> {
    let tags_url = format!("{}/api/tags", base_url.trim_end_matches('/'));

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .ok()?;

    let response = client.get(&tags_url).send().await.ok()?;

    if !response.status().is_success() {
        return None;
    }

    let tags: TagsResponse = response.json().await.ok()?;
    let models = tags.models?;

    if models.is_empty() {
        return None;
    }

    let mut model_infos = Vec::new();
    for model in models {
        let show_url = format!("{}/api/show", base_url.trim_end_matches('/'));
        let show: ShowResponse = if let Ok(resp) = client
            .post(&show_url)
            .json(&serde_json::json!({ "model": model.name }))
            .send()
            .await
        {
            resp.json().await.unwrap_or(ShowResponse { capabilities: vec![] })
        } else {
            ShowResponse { capabilities: vec![] }
        };

        let is_vision = show.capabilities.iter().any(|c| c.to_lowercase() == "vision")
            || model.name.contains("llava")
            || model.name.contains("vision")
            || model.name.contains("bakllava")
            || model.name.contains("moondream");

        let family = model
            .details
            .as_ref()
            .and_then(|d| d.family.clone())
            .unwrap_or_else(|| extract_family(&model.name));

        let context_window = estimate_context_window(&model.name, model.size);

        model_infos.push(ModelInfo {
            id: model.name.clone(),
            family,
            vision: if is_vision { crate::config::providers::Vision::Yes } else { crate::config::providers::Vision::No },
                adaptive_thinking: false,
                effort_levels: vec![],
            image_gen: false,
            tool_calling: true,
            streaming: true,
            context_window,
            cost_per_1k_input: 0.0,
            cost_per_1k_output: 0.0,
        });
    }

    // Cache the discovered models globally
    if let Ok(mut cache) = DISCOVERED_OLLAMA_MODELS.lock() {
        *cache = Some(model_infos.clone());
    }

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
