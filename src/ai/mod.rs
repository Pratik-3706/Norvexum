// ═══════════════════════════════════════════════════════════════════════════
// AI — Multi-provider AI client layer
//
// Unified trait for chat completions, tool calling, streaming, vision,
// and image generation across all providers including Ollama (local).
// ═══════════════════════════════════════════════════════════════════════════

pub mod anthropic;
pub mod gemini;
pub mod image_gen;
pub mod ollama;
pub mod openai_compat;
pub mod types;

use async_trait::async_trait;
use eyre::Result;
use tokio::sync::mpsc;

use crate::config::Settings;
use crate::config::providers::{ApiStyle, find_provider};
use types::{AiRequest, AiStreamEvent, ImageGenRequest, ImageGenResult};

/// Unified AI client interface.
/// All providers implement this trait so the agent loop is provider-agnostic.
#[async_trait]
pub trait AiClient: Send + Sync {
    /// Send a chat request and receive a streaming response.
    /// Each token/event is sent through the channel as it arrives.
    async fn chat_stream(
        &self,
        request: AiRequest,
        tx: mpsc::UnboundedSender<AiStreamEvent>,
    ) -> Result<()>;

    /// Whether this model supports vision (image input).
    fn supports_vision(&self) -> bool;

    /// Whether this model supports image generation.
    fn supports_image_gen(&self) -> bool;

    /// Generate an image (only if supports_image_gen() is true).
    async fn generate_image(&self, _request: ImageGenRequest) -> Result<ImageGenResult> {
        eyre::bail!("Image generation not supported by this provider/model")
    }

    /// Provider name for display purposes.
    fn provider_name(&self) -> &str;

    /// Model ID for display purposes.
    fn model_id(&self) -> &str;
}

/// Build the appropriate AI client based on settings.
pub fn build_client(settings: &Settings) -> Result<Box<dyn AiClient>> {
    // Special handling for Ollama — doesn't need a traditional provider lookup
    if settings.active_provider == "ollama" {
        let base_url = settings
            .ollama_base_url
            .as_deref()
            .unwrap_or(ollama::DEFAULT_OLLAMA_URL);

        // Ollama uses OpenAI-compatible API at /v1
        let api_url = format!("{}/v1", base_url.trim_end_matches('/'));
        return Ok(Box::new(openai_compat::OpenAiCompatClient::new(
            api_url,
            "ollama".to_string(), // Ollama doesn't need a real API key
            settings.active_model.clone(),
            None, // Model info will be discovered dynamically
        )));
    }

    let provider = find_provider(&settings.active_provider)
        .ok_or_else(|| eyre::eyre!("Unknown provider: {}", settings.active_provider))?;

    let api_key = settings
        .active_api_key()
        .ok_or_else(|| {
            eyre::eyre!(
                "No API key found for provider '{}'. Set {} in your .env file.",
                provider.name,
                provider.env_key
            )
        })?
        .to_string();

    let model_info = provider
        .models
        .iter()
        .find(|m| m.id == settings.active_model)
        .cloned();

    match provider.api_style {
        ApiStyle::OpenAiCompat => Ok(Box::new(openai_compat::OpenAiCompatClient::new(
            provider.base_url.clone(),
            api_key,
            settings.active_model.clone(),
            model_info,
        ))),
        ApiStyle::GeminiDirect => Ok(Box::new(gemini::GeminiClient::new(
            api_key,
            settings.active_model.clone(),
            model_info,
        ))),
        ApiStyle::Anthropic => Ok(Box::new(anthropic::AnthropicClient::new(
            api_key,
            settings.active_model.clone(),
            model_info,
        ))),
    }
}
