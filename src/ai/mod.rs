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
pub mod sse;
pub mod types;

use async_trait::async_trait;
use eyre::Result;
use tokio::sync::mpsc;

use crate::config::Settings;
use crate::config::providers::{ApiStyle, find_provider};
use types::{AiRequest, AiStreamEvent};

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
}

/// HTTP client for streaming model APIs.
///
/// A total-request `timeout` would abort long answers mid-stream, so only the
/// connection and the gap between received chunks are bounded.
pub fn streaming_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .read_timeout(std::time::Duration::from_secs(300))
        .tcp_keepalive(Some(std::time::Duration::from_secs(30)))
        .pool_idle_timeout(Some(std::time::Duration::from_secs(90)))
        .build()
        .unwrap_or_default()
}

/// Turn an HTTP error response into a one-line message: the provider's own
/// `error.message` instead of the whole JSON body, plus any `retry-after` hint
/// (the agent's retry logic reads "retry in Ns").
pub async fn describe_http_error(provider: &str, response: reqwest::Response) -> String {
    let status = response.status();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let body = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            let err = if v["error"].is_object() {
                &v["error"]
            } else {
                &v
            };
            err["message"]
                .as_str()
                .or_else(|| v["error"].as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.trim().to_string());
    let mut out = format!(
        "{provider} API error {status}: {}",
        crate::util::truncate_chars(&message, 600)
    );
    if let Some(secs) = retry_after {
        out.push_str(&format!(" (retry in {secs}s)"));
    }
    out
}

/// Build the appropriate AI client based on settings.
pub fn build_client(settings: &Settings) -> Result<Box<dyn AiClient>> {
    // Special handling for Ollama — doesn't need a traditional provider lookup
    if settings.active_provider == "ollama" {
        let base_url = settings
            .ollama_base_url
            .as_deref()
            .unwrap_or(ollama::DEFAULT_OLLAMA_URL);

        let model_info = ollama::cached_model_info(&settings.active_model);
        // Ollama uses OpenAI-compatible API at /v1
        let api_url = format!("{}/v1", base_url.trim_end_matches('/'));
        return Ok(Box::new(openai_compat::OpenAiCompatClient::new(
            api_url,
            "ollama".to_string(), // Ollama doesn't need a real API key
            settings.active_model.clone(),
            model_info,
        )));
    }

    let provider = find_provider(&settings.active_provider)
        .ok_or_else(|| eyre::eyre!("Unknown provider: {}", settings.active_provider))?;

    let api_key = settings
        .active_api_key()
        .unwrap_or_default()
        .trim()
        .to_string();

    let model_info = crate::config::providers::build_registry_with_settings(settings)
        .into_iter()
        .find(|p| p.name == provider.name)
        .and_then(|p| p.models.into_iter().find(|m| m.id == settings.active_model));

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

/// Stand-in used when the configured provider cannot be built (e.g. a typo in
/// `active_provider`). The app still starts; every request explains the problem.
pub struct UnavailableClient {
    pub reason: String,
}

#[async_trait]
impl AiClient for UnavailableClient {
    async fn chat_stream(
        &self,
        _request: AiRequest,
        tx: mpsc::UnboundedSender<AiStreamEvent>,
    ) -> Result<()> {
        let _ = tx.send(AiStreamEvent::Error(format!(
            "{} — pick a valid provider/model with /provider and /model, or `norvexum config set`.",
            self.reason
        )));
        Ok(())
    }

    fn supports_vision(&self) -> bool {
        false
    }
}

/// Guess reasoning support from the model/provider name (used until a probe
/// result is cached, and whenever probing is inconclusive).
pub fn guess_reasoning_support_by_name(
    provider: &str,
    model: &str,
) -> Option<types::ReasoningType> {
    let model_lower = model.to_lowercase();
    let provider_lower = provider.to_lowercase();
    // Gateways prefix ids with the vendor, e.g. "openai/o3-mini".
    let bare = model_lower.rsplit('/').next().unwrap_or(&model_lower);

    if provider_lower == "anthropic" || bare.starts_with("claude") {
        return anthropic::model_supports_thinking(bare)
            .then_some(types::ReasoningType::AnthropicThinking);
    }

    if provider_lower.contains("google") || bare.starts_with("gemini") {
        let thinking = ["gemini-2.5", "gemini-3", "gemini-pro"]
            .iter()
            .any(|p| bare.starts_with(p))
            && !bare.contains("image");
        return thinking.then_some(types::ReasoningType::GeminiThinking);
    }

    // OpenAI o-series and gpt-5 family accept `reasoning_effort`.
    let o_series = ["o1", "o3", "o4"]
        .iter()
        .any(|p| bare == *p || bare.starts_with(&format!("{p}-")));
    if o_series || bare.starts_with("gpt-5") {
        return Some(types::ReasoningType::OpenAiEffort);
    }

    // DeepSeek V3.1+ / V4 are hybrid models: they only stream reasoning when
    // `reasoning_effort` is sent.
    let deepseek_hybrid =
        regex::Regex::new(r"^deepseek-v(?:3\.[1-9]|[4-9])").is_ok_and(|re| re.is_match(bare));

    // DeepSeek R1 / reasoner / "thinking" variants
    if deepseek_hybrid
        || bare.contains("-r1")
        || bare.starts_with("r1-")
        || bare.contains("reasoner")
        || bare.contains("-think")
        || bare.contains("thinking")
    {
        return Some(types::ReasoningType::OpenAiEffort);
    }

    None
}

/// Probe whether a model accepts reasoning parameters by sending a tiny request.
/// Falls back to the name heuristic when the probe is inconclusive (auth
/// errors, rate limits, timeouts).
pub async fn probe_model_reasoning(
    settings: &Settings,
    provider_name: &str,
    model_name: &str,
) -> Option<types::ReasoningType> {
    let heuristic = guess_reasoning_support_by_name(provider_name, model_name);

    let mut temp_settings = settings.clone();
    temp_settings.active_provider = provider_name.to_string();
    temp_settings.active_model = model_name.to_string();

    let Ok(client) = build_client(&temp_settings) else {
        return heuristic;
    };
    let Some(provider) = find_provider(provider_name) else {
        return heuristic;
    };

    let candidates: &[types::ReasoningType] = match provider.api_style {
        ApiStyle::GeminiDirect => &[types::ReasoningType::GeminiThinking],
        ApiStyle::Anthropic => &[types::ReasoningType::AnthropicThinking],
        ApiStyle::OpenAiCompat => &[
            types::ReasoningType::OpenAiEffort,
            types::ReasoningType::AnthropicThinking,
        ],
    };

    for &kind in candidates {
        match probe_once(&*client, kind).await {
            ProbeOutcome::Supported => return Some(kind),
            ProbeOutcome::Unsupported => continue,
            ProbeOutcome::Inconclusive => return heuristic,
        }
    }
    None
}

enum ProbeOutcome {
    Supported,
    Unsupported,
    Inconclusive,
}

async fn probe_once(client: &dyn AiClient, kind: types::ReasoningType) -> ProbeOutcome {
    // Anthropic budgets must stay below max_tokens; reasoning models also need
    // room to emit at least a few reasoning tokens.
    let max_tokens = match kind {
        types::ReasoningType::AnthropicThinking => 2048,
        _ => 256,
    };
    let request = types::AiRequest::new(vec![types::Message::user("Reply with: ok")])
        .with_temperature(1.0)
        .with_max_tokens(max_tokens)
        .with_thinking_effort(Some("low".to_string()))
        .with_reasoning_type(Some(kind));

    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut has_reasoning = false;
    let mut succeeded = false;
    let mut errors: Vec<String> = Vec::new();

    // The request future must actually be polled — it is joined with the
    // receiver so events are consumed while it streams.
    let run = async {
        let stream = client.chat_stream(request, tx);
        let receive = async {
            while let Some(event) = rx.recv().await {
                match event {
                    AiStreamEvent::Error(err) => errors.push(err),
                    AiStreamEvent::ThinkingDelta(_)
                    | AiStreamEvent::ThinkingDone
                    | AiStreamEvent::ThinkingBlock(_) => {
                        has_reasoning = true;
                        succeeded = true;
                    }
                    AiStreamEvent::ContentDelta(_) => succeeded = true,
                    AiStreamEvent::Done { usage, .. } => {
                        succeeded = true;
                        if usage
                            .and_then(|u| u.reasoning_tokens)
                            .is_some_and(|r| r > 0)
                        {
                            has_reasoning = true;
                        }
                    }
                    _ => {}
                }
            }
        };
        let (result, ()) = tokio::join!(stream, receive);
        result
    };

    match tokio::time::timeout(std::time::Duration::from_secs(20), run).await {
        Err(_) => return ProbeOutcome::Inconclusive,
        Ok(Err(e)) => errors.push(e.to_string()),
        Ok(Ok(())) => {}
    }

    if let Some(err) = errors.first() {
        let lower = err.to_lowercase();
        let rejected_param = lower.contains("400")
            || lower.contains("unsupported")
            || lower.contains("not supported")
            || lower.contains("unrecognized")
            || lower.contains("unknown parameter")
            || lower.contains("extra inputs");
        return if rejected_param {
            ProbeOutcome::Unsupported
        } else {
            ProbeOutcome::Inconclusive
        };
    }

    // Aggregators sometimes silently drop the parameter, so a plain success
    // without any reasoning output counts as "unsupported".
    if has_reasoning {
        ProbeOutcome::Supported
    } else if succeeded {
        ProbeOutcome::Unsupported
    } else {
        ProbeOutcome::Inconclusive
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use types::ReasoningType::*;

    #[test]
    fn reasoning_heuristic_matches_current_model_names() {
        assert_eq!(
            guess_reasoning_support_by_name("anthropic", "claude-sonnet-4-5-20250929"),
            Some(AnthropicThinking)
        );
        assert_eq!(
            guess_reasoning_support_by_name("anthropic", "claude-opus-5-5"),
            Some(AnthropicThinking)
        );
        assert_eq!(
            guess_reasoning_support_by_name("anthropic", "claude-3-5-haiku-latest"),
            None
        );
        assert_eq!(
            guess_reasoning_support_by_name("openai", "o3-mini"),
            Some(OpenAiEffort)
        );
        assert_eq!(
            guess_reasoning_support_by_name("openai", "gpt-5"),
            Some(OpenAiEffort)
        );
        assert_eq!(guess_reasoning_support_by_name("openai", "gpt-4o"), None);
        assert_eq!(
            guess_reasoning_support_by_name("aicredits", "openai/o4-mini"),
            Some(OpenAiEffort)
        );
        assert_eq!(
            guess_reasoning_support_by_name("google_direct", "gemini-2.5-flash"),
            Some(GeminiThinking)
        );
        assert_eq!(
            guess_reasoning_support_by_name("aicredits", "deepseek/deepseek-r1"),
            Some(OpenAiEffort)
        );
    }
}
