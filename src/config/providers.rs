// ═══════════════════════════════════════════════════════════════════════════
// Provider Registry — AI provider catalog with model capabilities
// ═══════════════════════════════════════════════════════════════════════════

use serde::{Deserialize, Serialize};

/// Describes a single AI model's capabilities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub family: String,
    pub multimodal: bool,        // Can accept image input (vision)
    pub image_gen: bool,         // Can generate images
    pub tool_calling: bool,      // Supports function/tool calling
    pub streaming: bool,         // Supports streaming responses
    pub context_window: usize,   // Max tokens in context
    pub cost_per_1k_input: f64,  // Cost per 1K input tokens (USD)
    pub cost_per_1k_output: f64, // Cost per 1K output tokens (USD)
}

/// Describes an AI provider endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub name: String,
    pub display_name: String,
    pub base_url: String,
    pub api_style: ApiStyle,
    pub env_key: String,
    pub models: Vec<ModelInfo>,
}

/// The API protocol used by a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApiStyle {
    /// OpenAI-compatible /v1/chat/completions
    OpenAiCompat,
    /// Google AI Studio REST API
    GeminiDirect,
    /// Anthropic Messages API
    Anthropic,
}

/// Build the complete provider registry with all known models.
pub fn build_registry() -> Vec<ProviderInfo> {
    vec![
        // ── aicredits.in (OpenAI-compatible gateway) ─────────────────────
        ProviderInfo {
            name: "aicredits".into(),
            display_name: "AICredits.in".into(),
            base_url: "https://api.aicredits.in/v1".into(),
            api_style: ApiStyle::OpenAiCompat,
            env_key: "AICREDITS_API_KEY".into(),
            models: vec![
                ModelInfo {
                    id: "anthropic/claude-3-5-sonnet".into(),
                    family: "claude".into(),
                    multimodal: true,
                    image_gen: false,
                    tool_calling: true,
                    streaming: true,
                    context_window: 200_000,
                    cost_per_1k_input: 0.003,
                    cost_per_1k_output: 0.015,
                },
                ModelInfo {
                    id: "anthropic/claude-3-5-haiku".into(),
                    family: "claude".into(),
                    multimodal: true,
                    image_gen: false,
                    tool_calling: true,
                    streaming: true,
                    context_window: 200_000,
                    cost_per_1k_input: 0.0008,
                    cost_per_1k_output: 0.004,
                },
                ModelInfo {
                    id: "anthropic/claude-3-haiku".into(),
                    family: "claude".into(),
                    multimodal: true,
                    image_gen: false,
                    tool_calling: true,
                    streaming: true,
                    context_window: 200_000,
                    cost_per_1k_input: 0.00025,
                    cost_per_1k_output: 0.00125,
                },
                ModelInfo {
                    id: "moonshotai/kimi-k2.6".into(),
                    family: "kimi".into(),
                    multimodal: true,
                    image_gen: false,
                    tool_calling: true,
                    streaming: true,
                    context_window: 128_000,
                    cost_per_1k_input: 0.001,
                    cost_per_1k_output: 0.002,
                },
                ModelInfo {
                    id: "deepseek/deepseek-v4-pro".into(),
                    family: "deepseek".into(),
                    multimodal: false,
                    image_gen: false,
                    tool_calling: true,
                    streaming: true,
                    context_window: 128_000,
                    cost_per_1k_input: 0.00014,
                    cost_per_1k_output: 0.00028,
                },
                ModelInfo {
                    id: "minimax/minimax-m3".into(),
                    family: "minimax".into(),
                    multimodal: true,
                    image_gen: false,
                    tool_calling: true,
                    streaming: true,
                    context_window: 128_000,
                    cost_per_1k_input: 0.00015,
                    cost_per_1k_output: 0.0003,
                },
                ModelInfo {
                    id: "google/gemini-2.5-flash".into(),
                    family: "gemini".into(),
                    multimodal: true,
                    image_gen: false,
                    tool_calling: true,
                    streaming: true,
                    context_window: 1_000_000,
                    cost_per_1k_input: 0.000075,
                    cost_per_1k_output: 0.0003,
                },
                ModelInfo {
                    id: "z-ai/glm-5.1".into(),
                    family: "glm".into(),
                    multimodal: false,
                    image_gen: false,
                    tool_calling: true,
                    streaming: true,
                    context_window: 128_000,
                    cost_per_1k_input: 0.0001,
                    cost_per_1k_output: 0.0002,
                },
                ModelInfo {
                    id: "z-ai/glm-4.5".into(),
                    family: "glm".into(),
                    multimodal: false,
                    image_gen: false,
                    tool_calling: true,
                    streaming: true,
                    context_window: 128_000,
                    cost_per_1k_input: 0.0001,
                    cost_per_1k_output: 0.0002,
                },
            ],
        },
        // ── Google AI Studio direct (FREE tier) ──────────────────────────
        ProviderInfo {
            name: "google_direct".into(),
            display_name: "Google AI Studio (Free)".into(),
            base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
            api_style: ApiStyle::GeminiDirect,
            env_key: "GOOGLE_AI_API_KEY".into(),
            models: vec![
                ModelInfo {
                    id: "gemini-3-flash".into(),
                    family: "gemini".into(),
                    multimodal: true,
                    image_gen: false,
                    tool_calling: true,
                    streaming: true,
                    context_window: 1_000_000,
                    cost_per_1k_input: 0.0,
                    cost_per_1k_output: 0.0,
                },
                ModelInfo {
                    id: "gemini-3.1-flash-lite".into(),
                    family: "gemini".into(),
                    multimodal: true,
                    image_gen: false,
                    tool_calling: true,
                    streaming: true,
                    context_window: 1_000_000,
                    cost_per_1k_input: 0.0,
                    cost_per_1k_output: 0.0,
                },
                ModelInfo {
                    id: "gemini-2.5-flash".into(),
                    family: "gemini".into(),
                    multimodal: true,
                    image_gen: false,
                    tool_calling: true,
                    streaming: true,
                    context_window: 1_000_000,
                    cost_per_1k_input: 0.0,
                    cost_per_1k_output: 0.0,
                },
                ModelInfo {
                    id: "gemini-3.1-flash-image".into(),
                    family: "gemini".into(),
                    multimodal: true,
                    image_gen: true,
                    tool_calling: true,
                    streaming: true,
                    context_window: 1_000_000,
                    cost_per_1k_input: 0.0,
                    cost_per_1k_output: 0.0,
                },
            ],
        },
        // ── OpenAI direct ────────────────────────────────────────────────
        ProviderInfo {
            name: "openai".into(),
            display_name: "OpenAI".into(),
            base_url: "https://api.openai.com/v1".into(),
            api_style: ApiStyle::OpenAiCompat,
            env_key: "OPENAI_API_KEY".into(),
            models: vec![
                ModelInfo {
                    id: "gpt-4o".into(),
                    family: "gpt".into(),
                    multimodal: true,
                    image_gen: false,
                    tool_calling: true,
                    streaming: true,
                    context_window: 128_000,
                    cost_per_1k_input: 0.0025,
                    cost_per_1k_output: 0.010,
                },
                ModelInfo {
                    id: "gpt-4o-mini".into(),
                    family: "gpt".into(),
                    multimodal: true,
                    image_gen: false,
                    tool_calling: true,
                    streaming: true,
                    context_window: 128_000,
                    cost_per_1k_input: 0.00015,
                    cost_per_1k_output: 0.0006,
                },
            ],
        },
        // ── Anthropic direct ─────────────────────────────────────────────
        ProviderInfo {
            name: "anthropic".into(),
            display_name: "Anthropic".into(),
            base_url: "https://api.anthropic.com/v1".into(),
            api_style: ApiStyle::Anthropic,
            env_key: "ANTHROPIC_API_KEY".into(),
            models: vec![
                ModelInfo {
                    id: "claude-sonnet-4-20250514".into(),
                    family: "claude".into(),
                    multimodal: true,
                    image_gen: false,
                    tool_calling: true,
                    streaming: true,
                    context_window: 200_000,
                    cost_per_1k_input: 0.003,
                    cost_per_1k_output: 0.015,
                },
                ModelInfo {
                    id: "claude-3-5-haiku-latest".into(),
                    family: "claude".into(),
                    multimodal: true,
                    image_gen: false,
                    tool_calling: true,
                    streaming: true,
                    context_window: 200_000,
                    cost_per_1k_input: 0.0008,
                    cost_per_1k_output: 0.004,
                },
            ],
        },
        // ── Ollama local ─────────────────────────────────────────────────
        ProviderInfo {
            name: "ollama".into(),
            display_name: "Ollama (Local)".into(),
            base_url: "http://localhost:11434/v1".into(),
            api_style: ApiStyle::OpenAiCompat,
            env_key: "OLLAMA_BASE_URL".into(),
            models: vec![
                // Default models that will be shown if query fails
                ModelInfo {
                    id: "llama3".into(),
                    family: "llama".into(),
                    multimodal: false,
                    image_gen: false,
                    tool_calling: true,
                    streaming: true,
                    context_window: 8192,
                    cost_per_1k_input: 0.0,
                    cost_per_1k_output: 0.0,
                },
                ModelInfo {
                    id: "mistral".into(),
                    family: "mistral".into(),
                    multimodal: false,
                    image_gen: false,
                    tool_calling: true,
                    streaming: true,
                    context_window: 32768,
                    cost_per_1k_input: 0.0,
                    cost_per_1k_output: 0.0,
                },
            ],
        },
    ]
}

/// Look up a provider by name.
pub fn find_provider(name: &str) -> Option<ProviderInfo> {
    build_registry().into_iter().find(|p| p.name == name)
}

/// Look up a model across all providers. Returns (provider, model).
pub fn find_model(model_id: &str) -> Option<(ProviderInfo, ModelInfo)> {
    for provider in build_registry() {
        if let Some(model) = provider.models.iter().find(|m| m.id == model_id) {
            return Some((provider.clone(), model.clone()));
        }
    }
    None
}

/// List all providers that have the given env key set.
pub fn available_providers() -> Vec<ProviderInfo> {
    build_registry()
        .into_iter()
        .filter(|p| {
            if p.name == "ollama" {
                // Ollama is always available locally
                true
            } else {
                std::env::var(&p.env_key)
                    .ok()
                    .filter(|s| !s.is_empty())
                    .is_some()
            }
        })
        .collect()
}
