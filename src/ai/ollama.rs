// ===========================================================================
// Ollama — Local model support via Ollama's OpenAI-compatible API
//
// Auto-discovers locally available models by querying the Ollama API.
// Reuses the OpenAI-compatible client since Ollama supports that protocol.
// ===========================================================================

use serde::Deserialize;

use crate::config::providers::ModelInfo;

/// Default Ollama API base URL.
pub const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

pub static DISCOVERED_OLLAMA_MODELS: std::sync::Mutex<Option<Vec<ModelInfo>>> =
    std::sync::Mutex::new(None);

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

    let show_url = format!("{}/api/show", base_url.trim_end_matches('/'));
    let shows = futures::future::join_all(models.iter().map(|model| {
        let client = client.clone();
        let show_url = show_url.clone();
        async move {
            match client
                .post(&show_url)
                .json(&serde_json::json!({ "model": model.name }))
                .send()
                .await
            {
                Ok(resp) => resp.json::<ShowResponse>().await.ok(),
                Err(_) => None,
            }
        }
    }))
    .await;

    let mut model_infos = Vec::new();
    for (model, show) in models.into_iter().zip(shows) {
        let capabilities = show.map(|s| s.capabilities).unwrap_or_default();
        let has = |cap: &str| capabilities.iter().any(|c| c.eq_ignore_ascii_case(cap));
        let name = model.name.to_lowercase();

        let is_vision = has("vision")
            || ["llava", "vision", "bakllava", "moondream"]
                .iter()
                .any(|n| name.contains(n));
        // `capabilities` is reported by Ollama ≥ 0.6; assume tools when it's missing.
        let tool_calling = capabilities.is_empty() || has("tools");

        let family = model
            .details
            .as_ref()
            .and_then(|d| d.family.clone())
            .unwrap_or_else(|| extract_family(&model.name));

        let context_window = estimate_context_window(&model.name, model.size);

        model_infos.push(ModelInfo {
            id: model.name.clone(),
            family,
            vision: if is_vision {
                crate::config::providers::Vision::Yes
            } else {
                crate::config::providers::Vision::No
            },
            adaptive_thinking: false,
            effort_levels: vec![],
            image_gen: false,
            tool_calling,
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

/// Capabilities of a model found by the last `discover_models` call.
pub fn cached_model_info(model: &str) -> Option<ModelInfo> {
    DISCOVERED_OLLAMA_MODELS
        .lock()
        .ok()?
        .as_ref()?
        .iter()
        .find(|m| m.id == model)
        .cloned()
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
