// ===========================================================================
// AI — Multi-provider AI client layer
//
// Unified trait for chat completions, tool calling, streaming, vision,
// and image generation across all providers including Ollama (local).
// ===========================================================================

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
        .unwrap_or_default()
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

/// Guess reasoning support based on model/provider name (heuristic fallback)
pub fn guess_reasoning_support_by_name(provider: &str, model: &str) -> Option<types::ReasoningType> {
    let model_lower = model.to_lowercase();
    let provider_lower = provider.to_lowercase();

    // Google Gemini (Gemini 2.0+ support thinking config)
    if provider_lower.contains("google") || model_lower.contains("gemini") {
        if model_lower.contains("gemini-2.0") || model_lower.contains("gemini-2.5") || model_lower.contains("gemini-3") || model_lower.contains("gemini-pro") {
            return Some(types::ReasoningType::GeminiThinking);
        }
    }

    // Anthropic Claude (Claude 3.7+ support extended thinking)
    if provider_lower.contains("anthropic") || model_lower.contains("claude") {
        if model_lower.contains("claude-3-7") || model_lower.contains("claude-3.7") || model_lower.contains("claude-4") {
            return Some(types::ReasoningType::AnthropicThinking);
        }
    }

    // OpenAI o1 / o3 / o4
    if provider_lower.contains("openai") || model_lower.contains("o1") || model_lower.contains("o3") || model_lower.contains("o4") {
        if model_lower.contains("o1-") || model_lower.contains("o3-") || model_lower.contains("o4-") || model_lower == "o1" || model_lower == "o3" || model_lower == "o4" {
            return Some(types::ReasoningType::OpenAiEffort);
        }
    }

    // DeepSeek R1 / Reasoner / Think models
    if model_lower.contains("-r1") || model_lower.contains("r1-") || model_lower.contains("reasoner") || model_lower.contains("-think") || model_lower.contains("thinking") {
        return Some(types::ReasoningType::OpenAiEffort);
    }

    None
}

/// Dynamically probe if a model supports reasoning by sending a 1-token test prompt
pub async fn probe_model_reasoning(
    settings: &Settings,
    provider_name: &str,
    model_name: &str,
) -> Option<types::ReasoningType> {
    let mut temp_settings = settings.clone();
    temp_settings.active_provider = provider_name.to_string();
    temp_settings.active_model = model_name.to_string();

    let client = match build_client(&temp_settings) {
        Ok(c) => c,
        Err(_) => return None,
    };

    let provider = match find_provider(provider_name) {
        Some(p) => p,
        None => return None,
    };

    match provider.api_style {
        ApiStyle::GeminiDirect => {
            if test_gemini_thinking(&*client).await {
                Some(types::ReasoningType::GeminiThinking)
            } else {
                None
            }
        }
        ApiStyle::Anthropic => {
            if test_anthropic_thinking(&*client).await {
                Some(types::ReasoningType::AnthropicThinking)
            } else {
                None
            }
        }
        ApiStyle::OpenAiCompat => {
            // First try OpenAI-style reasoning_effort
            if test_openai_effort(&*client).await {
                Some(types::ReasoningType::OpenAiEffort)
            } else if test_anthropic_thinking(&*client).await {
                // Then try Anthropic/DeepSeek-style thinking configuration
                Some(types::ReasoningType::AnthropicThinking)
            } else {
                None
            }
        }
    }
}

async fn test_openai_effort(client: &dyn AiClient) -> bool {
    let request = types::AiRequest::new(vec![types::Message::user("h")])
        .with_temperature(1.0)
        .with_max_tokens(1)
        .with_thinking_effort(Some("low".to_string()))
        .with_reasoning_type(Some(types::ReasoningType::OpenAiEffort));
    run_test_request(client, request).await
}

async fn test_anthropic_thinking(client: &dyn AiClient) -> bool {
    let request = types::AiRequest::new(vec![types::Message::user("h")])
        .with_temperature(1.0)
        // Anthropic requires max_tokens to be strictly greater than budget_tokens
        .with_max_tokens(2048)
        .with_thinking_effort(Some("low".to_string()))
        .with_reasoning_type(Some(types::ReasoningType::AnthropicThinking));
    run_test_request(client, request).await
}

async fn test_gemini_thinking(client: &dyn AiClient) -> bool {
    let request = types::AiRequest::new(vec![types::Message::user("h")])
        .with_temperature(1.0)
        .with_max_tokens(1)
        .with_thinking_effort(Some("low".to_string()))
        .with_reasoning_type(Some(types::ReasoningType::GeminiThinking));
    run_test_request(client, request).await
}

async fn run_test_request(client: &dyn AiClient, request: types::AiRequest) -> bool {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = client.chat_stream(request, tx);

    let mut has_reasoning = false;
    let mut is_unsupported_param_error = false;
    let mut has_any_error = false;
    let mut stream_succeeded = false;

    let receive_future = async {
        while let Some(event) = rx.recv().await {
            match event {
                types::AiStreamEvent::Error(err) => {
                    has_any_error = true;
                    let err_lower = err.to_lowercase();
                    if err_lower.contains("parameter") || err_lower.contains("unknown") || err_lower.contains("support") || err_lower.contains("invalid") || err_lower.contains("400") {
                        is_unsupported_param_error = true;
                    }
                }
                types::AiStreamEvent::ThinkingDelta(_) | types::AiStreamEvent::ThinkingDone => {
                    has_reasoning = true;
                    stream_succeeded = true;
                }
                types::AiStreamEvent::ContentDelta(_) => {
                    stream_succeeded = true;
                }
                types::AiStreamEvent::Done { usage, .. } => {
                    stream_succeeded = true;
                    if let Some(u) = usage {
                        if let Some(r_tokens) = u.reasoning_tokens {
                            if r_tokens > 0 {
                                has_reasoning = true;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    };

    let timeout_result = tokio::time::timeout(std::time::Duration::from_secs(5), receive_future).await;
    if timeout_result.is_err() {
        has_any_error = true; // Mark as error if it timed out
    }
    
    // Drop rx so the background stream task will naturally terminate if it tries to send more
    drop(rx);

    if is_unsupported_param_error {
        return false;
    }

    if has_any_error && !stream_succeeded {
        // Rate limits, auth errors, etc. keep the check optimistic
        return true;
    }

    // If stream succeeded without error, it must have actually used reasoning.
    // This prevents false positives on aggregators that silently strip the parameter.
    has_reasoning
}
