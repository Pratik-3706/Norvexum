// ═══════════════════════════════════════════════════════════════════════════
// Agent — Core reasoning loop with thinking + truly parallel tool calling
//
// Loop:
//   1. Send conversation + tools to AI model (streaming)
//   2. Display thinking tokens live in TUI
//   3. On tool calls → execute in parallel (futures::join_all)
//   4. Append results → repeat
//   5. On content → stream text live to TUI
//   6. On "done" → break
//   7. Auto-compact context when approaching window limit
//   8. Auto-save session after each turn
// ═══════════════════════════════════════════════════════════════════════════

pub mod checkpoint;
pub mod compaction;
pub mod history;
pub mod session;
pub mod vision;

use eyre::Result;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::ai::types::*;
use crate::ai::{self, AiClient};
use crate::config::Settings;
use crate::tools::{ToolContext, ToolRegistry, ToolResult};

/// Events the agent sends to the UI for live rendering.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Thinking text (streamed token by token)
    Thinking(String),
    /// End of thinking block
    ThinkingDone,
    /// Response text (streamed token by token)
    Content(String),
    /// A tool is being called
    ToolStart { name: String, id: String },
    /// Tool call arguments being streamed
    ToolArgsDelta { id: String, delta: String },
    /// Tool execution complete
    ToolResult {
        id: String,
        name: String,
        result: String,
        success: bool,
    },
    /// File write detected (for live file streaming display)
    FileWrite {
        path: String,
        content_preview: String,
    },
    /// Agent turn complete
    Done { usage: Option<UsageStats> },
    /// Error
    Error(String),
    /// Status message
    Status(String),
    /// Model/Provider switch event
    ModelSwitched { model: String, provider: String },
    /// Clear chat history event
    ClearChat,
    /// Quit application event
    Quit,
    /// Tool approval request — requires user Y/N before executing
    ApprovalRequest {
        id: String,
        tool_name: String,
        args_preview: String,
    },
}

/// The core agent that orchestrates AI ↔ Tool interaction.
pub struct Agent {
    client: Arc<dyn AiClient>,
    tools: Arc<ToolRegistry>,
    settings: Arc<Settings>,
    messages: Vec<Message>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    session: Option<session::Session>,
    /// Channel for receiving approval responses from the UI
    approval_rx: Option<mpsc::UnboundedReceiver<bool>>,
    /// Sender side stored so the UI can clone it
    approval_tx: mpsc::UnboundedSender<bool>,
}

impl Agent {
    pub fn new(settings: Settings, event_tx: mpsc::UnboundedSender<AgentEvent>) -> Result<Self> {
        let client = Arc::from(ai::build_client(&settings)?);
        let tools = Arc::new(ToolRegistry::new(&settings));
        let settings = Arc::new(settings);

        // Build system prompt with project context
        let system = build_system_prompt(&settings, &tools);
        let messages = vec![Message::system(system)];

        let (approval_tx, approval_rx) = mpsc::unbounded_channel();

        Ok(Self {
            client,
            tools,
            settings,
            messages,
            event_tx,
            session: None,
            approval_rx: Some(approval_rx),
            approval_tx,
        })
    }

    /// Get the approval sender so the UI can respond to approval requests.
    pub fn approval_sender(&self) -> mpsc::UnboundedSender<bool> {
        self.approval_tx.clone()
    }

    /// Process a user message through the full agent loop.
    pub async fn process_message(&mut self, user_input: &str) -> Result<()> {
        if user_input.starts_with('/') {
            let parts: Vec<&str> = user_input.split_whitespace().collect();
            if !parts.is_empty() {
                match parts[0] {
                    "/help" => {
                        let msg = vec![
                            "📖 **Norvexum Interactive Chat Commands:**",
                            "  `/help`                 - Show this help message",
                            "  `/clear`                - Clear TUI chat log & conversation memory",
                            "  `/copy`                 - Copy the last AI response to system clipboard",
                            "  `/copy chat`            - Copy the entire conversation history to clipboard",
                            "  `/undo`                 - Undo the last file write/edit (restore from checkpoint)",
                            "  `/session resume`       - Resume the last saved session",
                            "  `/session list`         - List saved sessions",
                            "  `/session clear`        - Delete all saved sessions",
                            "  `/provider`             - List available AI providers",
                            "  `/provider <id>`        - Switch to specified AI provider",
                            "  `/model`                - List models for the active provider",
                            "  `/model <id>`           - Switch to specified AI model",
                            "  `/exit` or `/quit`      - Quit the program cleanly",
                            "\n💡 *Tip: If the agent is executing, press `Esc` to cancel the current run immediately.*",
                            "💡 *Tip: Drag-select text in the chat to copy it to clipboard.*",
                        ].join("\n");
                        let _ = self.event_tx.send(AgentEvent::Content(msg));
                        let _ = self.event_tx.send(AgentEvent::Done { usage: None });
                        return Ok(());
                    }
                    "/clear" => {
                        if self.messages.len() > 1 {
                            self.messages.truncate(1); // Retain only system prompt
                        }
                        let _ = self.event_tx.send(AgentEvent::ClearChat);
                        let _ = self
                            .event_tx
                            .send(AgentEvent::Status("🧹 Chat cleared".into()));
                        let _ = self.event_tx.send(AgentEvent::Done { usage: None });
                        return Ok(());
                    }
                    "/undo" => {
                        match checkpoint::undo_last(&self.settings.project_root) {
                            Ok(restored) => {
                                let msg = if restored.is_empty() {
                                    "No files were restored.".to_string()
                                } else {
                                    format!(
                                        "✅ Restored {} file(s):\n{}",
                                        restored.len(),
                                        restored
                                            .iter()
                                            .map(|f| format!("  • {}", f))
                                            .collect::<Vec<_>>()
                                            .join("\n")
                                    )
                                };
                                let _ = self.event_tx.send(AgentEvent::Content(msg));
                            }
                            Err(e) => {
                                let _ = self
                                    .event_tx
                                    .send(AgentEvent::Error(format!("Undo failed: {}", e)));
                            }
                        }
                        let _ = self.event_tx.send(AgentEvent::Done { usage: None });
                        return Ok(());
                    }
                    "/session" => {
                        if parts.len() < 2 {
                            let _ = self.event_tx.send(AgentEvent::Content(
                                "Usage: `/session resume` | `/session list` | `/session clear`"
                                    .into(),
                            ));
                            let _ = self.event_tx.send(AgentEvent::Done { usage: None });
                            return Ok(());
                        }
                        match parts[1] {
                            "resume" => {
                                if let Some(sess) =
                                    session::Session::load_latest(&self.settings.project_root)
                                {
                                    self.messages = sess.messages.clone();
                                    self.session = Some(sess);
                                    let _ = self.event_tx.send(AgentEvent::Content(format!(
                                        "✅ Resumed session ({} messages)",
                                        self.messages.len()
                                    )));
                                } else {
                                    let _ = self.event_tx.send(AgentEvent::Content(
                                        "No saved sessions found.".into(),
                                    ));
                                }
                            }
                            "list" => {
                                let sessions =
                                    session::Session::list(&self.settings.project_root);
                                if sessions.is_empty() {
                                    let _ = self.event_tx.send(AgentEvent::Content(
                                        "No saved sessions.".into(),
                                    ));
                                } else {
                                    let list: Vec<String> = sessions
                                        .iter()
                                        .take(10)
                                        .map(|s| {
                                            format!(
                                                "  • {} — {} msgs — {} — {}",
                                                s.id, s.message_count, s.model, s.updated_at
                                            )
                                        })
                                        .collect();
                                    let _ = self.event_tx.send(AgentEvent::Content(format!(
                                        "Saved Sessions:\n{}",
                                        list.join("\n")
                                    )));
                                }
                            }
                            "clear" => {
                                let _ =
                                    session::Session::clear_all(&self.settings.project_root);
                                let _ = self.event_tx.send(AgentEvent::Content(
                                    "✅ All sessions cleared.".into(),
                                ));
                            }
                            _ => {
                                let _ = self.event_tx.send(AgentEvent::Content(
                                    "Unknown session command. Use: resume, list, clear".into(),
                                ));
                            }
                        }
                        let _ = self.event_tx.send(AgentEvent::Done { usage: None });
                        return Ok(());
                    }
                    "/copy" => {
                        let mut content_to_copy = String::new();
                        if parts.len() > 1 && parts[1] == "chat" {
                            let mut full_chat = Vec::new();
                            for msg in &self.messages {
                                if matches!(msg.role, crate::ai::types::Role::System) {
                                    continue;
                                }
                                let role_prefix = match msg.role {
                                    crate::ai::types::Role::User => "You",
                                    crate::ai::types::Role::Assistant => "AI",
                                    crate::ai::types::Role::Tool => "Tool",
                                    _ => "Other",
                                };
                                full_chat.push(format!("{}:\n{}\n", role_prefix, msg.text()));
                            }
                            content_to_copy = full_chat.join("\n");
                        } else {
                            if let Some(msg) = self
                                .messages
                                .iter()
                                .rev()
                                .find(|m| matches!(m.role, crate::ai::types::Role::Assistant))
                            {
                                content_to_copy = msg.text();
                            }
                        }

                        if content_to_copy.is_empty() {
                            let _ = self
                                .event_tx
                                .send(AgentEvent::Error("Nothing to copy!".into()));
                        } else {
                            match arboard::Clipboard::new() {
                                Ok(mut ctx) => {
                                    if let Err(e) = ctx.set_text(content_to_copy) {
                                        let _ = self.event_tx.send(AgentEvent::Error(format!(
                                            "Failed to copy to clipboard: {}",
                                            e
                                        )));
                                    } else {
                                        let feedback = if parts.len() > 1 && parts[1] == "chat" {
                                            "📋 Copied entire chat history to clipboard!"
                                        } else {
                                            "📋 Copied last AI response to clipboard!"
                                        };
                                        let _ = self
                                            .event_tx
                                            .send(AgentEvent::Status(feedback.to_string()));
                                        let _ = self
                                            .event_tx
                                            .send(AgentEvent::Content(feedback.to_string()));
                                    }
                                }
                                Err(e) => {
                                    let _ = self.event_tx.send(AgentEvent::Error(format!(
                                        "Failed to initialize clipboard: {}",
                                        e
                                    )));
                                }
                            }
                        }
                        let _ = self.event_tx.send(AgentEvent::Done { usage: None });
                        return Ok(());
                    }
                    "/exit" | "/quit" => {
                        // Save session before quitting
                        self.save_session();
                        let _ = self.event_tx.send(AgentEvent::Quit);
                        return Ok(());
                    }
                    "/stop" => {
                        let _ = self
                            .event_tx
                            .send(AgentEvent::Status("⏹️ Already stopped".into()));
                        let _ = self.event_tx.send(AgentEvent::Done { usage: None });
                        return Ok(());
                    }
                    "/model" => {
                        if parts.len() < 2 {
                            let current_provider = &self.settings.active_provider;
                            let mut list =
                                vec![format!("Available Models for {}:", current_provider)];

                            if current_provider == "ollama" {
                                let ollama_url = self.settings
                                    .ollama_base_url
                                    .as_deref()
                                    .unwrap_or(crate::ai::ollama::DEFAULT_OLLAMA_URL);
                                if let Some(models) = crate::ai::ollama::discover_models(ollama_url).await {
                                    for model in &models {
                                        let capabilities = format!(
                                            "{}{}{}",
                                            if model.multimodal { " 👁️" } else { "" },
                                            if model.tool_calling { " 🔧" } else { "" },
                                            if model.image_gen { " 🎨" } else { "" }
                                        );
                                        list.push(format!(
                                            "  • {} ({}{})",
                                            model.id, model.family, capabilities
                                        ));
                                    }
                                } else {
                                    list.push("  (Ollama is unreachable or no local models installed)".to_string());
                                }
                            } else {
                                let registry = crate::config::providers::build_registry();
                                if let Some(provider) =
                                    registry.iter().find(|p| &p.name == current_provider)
                                {
                                    for model in &provider.models {
                                        let capabilities = format!(
                                            "{}{}{}",
                                            if model.multimodal { " 👁️" } else { "" },
                                            if model.tool_calling { " 🔧" } else { "" },
                                            if model.image_gen { " 🎨" } else { "" }
                                        );
                                        list.push(format!(
                                            "  • {} ({}{})",
                                            model.id, model.family, capabilities
                                        ));
                                    }
                                } else {
                                    list.push("  (No models listed for this provider)".to_string());
                                }
                            }

                            list.push(format!("\nActive: {}", self.settings.active_model));
                            list.push("To switch: /model <model_id>".to_string());

                            let _ = self.event_tx.send(AgentEvent::Content(list.join("\n")));
                            let _ = self.event_tx.send(AgentEvent::Done { usage: None });
                            return Ok(());
                        }
                        let model_id = parts[1].to_string();

                        let mut settings = (*self.settings).clone();
                        settings.active_model = model_id.clone();

                        match ai::build_client(&settings) {
                            Ok(new_client) => {
                                self.client = Arc::from(new_client);
                                self.settings = Arc::new(settings);
                                if let Err(e) = self.settings.save() {
                                    let _ = self.event_tx.send(AgentEvent::Error(format!(
                                        "Failed to save config: {}",
                                        e
                                    )));
                                }
                                let _ = self.event_tx.send(AgentEvent::ModelSwitched {
                                    model: model_id.clone(),
                                    provider: self.settings.active_provider.clone(),
                                });
                                let _ = self.event_tx.send(AgentEvent::Content(format!(
                                    "Switched active model to **{}**.",
                                    model_id
                                )));
                                let _ = self.event_tx.send(AgentEvent::Status(format!(
                                    "Model switched to: {}",
                                    model_id
                                )));
                            }
                            Err(e) => {
                                let _ = self.event_tx.send(AgentEvent::Error(format!(
                                    "Failed to switch model: {}",
                                    e
                                )));
                            }
                        }
                        let _ = self.event_tx.send(AgentEvent::Done { usage: None });
                        return Ok(());
                    }
                    "/provider" => {
                        if parts.len() < 2 {
                            let registry = crate::config::providers::build_registry();
                            let mut list = vec!["Available Providers:".to_string()];
                            for provider in &registry {
                                list.push(format!(
                                    "  • {} - {}",
                                    provider.name, provider.display_name
                                ));
                            }
                            list.push(format!("\nActive: {}", self.settings.active_provider));
                            list.push("To switch: /provider <provider_id>".to_string());

                            let _ = self.event_tx.send(AgentEvent::Content(list.join("\n")));
                            let _ = self.event_tx.send(AgentEvent::Done { usage: None });
                            return Ok(());
                        }
                        let provider_id = parts[1].to_string();
                        let mut settings = (*self.settings).clone();
                        settings.active_provider = provider_id.clone();

                        // Try to auto-select a model for the new provider
                        if provider_id == "google_direct" {
                            settings.active_model = "gemini-2.5-flash".into();
                        } else if provider_id == "aicredits" {
                            settings.active_model = "google/gemini-2.5-flash".into();
                        } else if provider_id == "ollama" {
                            // For Ollama, try to pick the first available model
                            let ollama_url = settings
                                .ollama_base_url
                                .as_deref()
                                .unwrap_or(crate::ai::ollama::DEFAULT_OLLAMA_URL);
                            if let Some(models) =
                                crate::ai::ollama::discover_models(ollama_url).await
                            {
                                if let Some(first) = models.first() {
                                    settings.active_model = first.id.clone();
                                }
                            }
                        }

                        match ai::build_client(&settings) {
                            Ok(new_client) => {
                                self.client = Arc::from(new_client);
                                self.settings = Arc::new(settings.clone());
                                if let Err(e) = self.settings.save() {
                                    let _ = self.event_tx.send(AgentEvent::Error(format!(
                                        "Failed to save config: {}",
                                        e
                                    )));
                                }
                                let _ = self.event_tx.send(AgentEvent::ModelSwitched {
                                    model: self.settings.active_model.clone(),
                                    provider: provider_id.clone(),
                                });
                                let _ = self.event_tx.send(AgentEvent::Content(format!(
                                    "Switched provider to **{}**.\nActive model: **{}**",
                                    provider_id, settings.active_model
                                )));
                                let _ = self.event_tx.send(AgentEvent::Status(format!(
                                    "Provider switched to: {}",
                                    provider_id
                                )));
                            }
                            Err(e) => {
                                let _ = self.event_tx.send(AgentEvent::Error(format!(
                                    "Failed to switch provider: {}",
                                    e
                                )));
                            }
                        }
                        let _ = self.event_tx.send(AgentEvent::Done { usage: None });
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }

        let mut user_msg = None;

        if let Some(img_path) = detect_image(user_input, &self.settings.project_root) {
            if self.settings.is_in_sandbox(&img_path) {
                if self.client.supports_vision() {
                    let _ = self.event_tx.send(AgentEvent::Status(format!(
                        "📸 Loading image: {}...",
                        img_path.file_name().unwrap_or_default().to_string_lossy()
                    )));
                    match vision::encode_image_file(&img_path) {
                        Ok((b64, mime)) => {
                            user_msg = Some(Message::user_with_image(user_input, b64, mime));
                        }
                        Err(e) => {
                            let _ = self.event_tx.send(AgentEvent::Error(format!(
                                "Failed to load image: {}",
                                e
                            )));
                        }
                    }
                } else if let Some(ocr_key) = &self.settings.ocr_space_api_key {
                    let _ = self.event_tx.send(AgentEvent::Status(format!(
                        "📸 Performing OCR on {}...",
                        img_path.file_name().unwrap_or_default().to_string_lossy()
                    )));
                    match vision::ocr_image(ocr_key, &img_path).await {
                        Ok(text) => {
                            let enriched_prompt = format!(
                                "[Image content detected via OCR]:\n{}\n\n{}",
                                text, user_input
                            );
                            user_msg = Some(Message::user(enriched_prompt));
                        }
                        Err(e) => {
                            let _ = self.event_tx.send(AgentEvent::Error(format!(
                                "OCR failed: {}. Sending message as plain text.",
                                e
                            )));
                        }
                    }
                } else {
                    let _ = self.event_tx.send(AgentEvent::Status(
                        "⚠️ Model does not support vision, and no OCR_SPACE_API_KEY is configured. Sending as plain text.".into()
                    ));
                }
            } else {
                let _ = self.event_tx.send(AgentEvent::Error(format!(
                    "Access denied: image file '{}' is outside the sandbox",
                    img_path.display()
                )));
            }
        }

        let final_msg = user_msg.unwrap_or_else(|| Message::user(user_input));

        // Check if user input triggers a skill template
        if let Some(skill) = crate::skills::find_matching_skill(user_input, &self.settings.project_root) {
            let _ = self.event_tx.send(AgentEvent::Status(format!("✨ Triggered skill: {}", skill.name)));
            if let Some(system_msg) = self.messages.first_mut() {
                if system_msg.role == Role::System {
                    let mut text = system_msg.text();
                    text.push_str("\n\n=== TRIGGERED SKILL: ");
                    text.push_str(&skill.name);
                    text.push_str(" ===\n");
                    text.push_str(&skill.system_instructions);
                    text.push_str("\n====================================\n");
                    system_msg.content = vec![ContentPart::Text { text }];
                }
            }
        }

        self.messages.push(final_msg);

        // ── Context compaction check ─────────────────────────────────────
        let context_window = self.get_context_window();
        if compaction::should_compact(&self.messages, context_window, 80) {
            let _ = self
                .event_tx
                .send(AgentEvent::Status("🗜️ Compacting context...".into()));
            self.messages = compaction::compact(&self.messages, 4);
        }

        let max_loops = self.settings.max_thinking_loops;

        for loop_num in 0..max_loops {
            let _ = self.event_tx.send(AgentEvent::Status(format!(
                "Thinking (loop {}/{})...",
                loop_num + 1,
                max_loops
            )));

            // Create AI request with current conversation + tools
            let request = AiRequest::new(self.messages.clone())
                .with_tools(self.tools.tool_defs())
                .with_temperature(0.7);

            // Stream response from AI
            let (stream_tx, mut stream_rx) = mpsc::unbounded_channel::<AiStreamEvent>();

            let client = self.client.clone();
            let stream_handle = async move {
                client.chat_stream(request, stream_tx).await
            };

            // Process stream in background
            let event_tx = self.event_tx.clone();
            let mut assistant_text = String::new();
            let mut pending_tool_calls: Vec<ToolCall> = Vec::new();
            let mut has_tool_calls = false;
            let mut final_usage = None;
            let mut stream_error = None;

            // Run the stream and process events concurrently
            tokio::pin!(stream_handle);
            loop {
                tokio::select! {
                    result = &mut stream_handle => {
                        if let Err(e) = result {
                            let _ = self.event_tx.send(AgentEvent::Error(format!("AI stream error: {}", e)));
                            return Err(e);
                        }
                        break;
                    }
                    Some(event) = stream_rx.recv() => {
                        match event {
                            AiStreamEvent::ThinkingDelta(text) => {
                                let _ = event_tx.send(AgentEvent::Thinking(text));
                            }
                            AiStreamEvent::ThinkingDone => {
                                let _ = event_tx.send(AgentEvent::ThinkingDone);
                            }
                            AiStreamEvent::ContentDelta(text) => {
                                assistant_text.push_str(&text);
                                let _ = event_tx.send(AgentEvent::Content(text));
                            }
                            AiStreamEvent::ToolCallStart(tc) => {
                                let _ = event_tx.send(AgentEvent::ToolStart {
                                    name: tc.name.clone(),
                                    id: tc.id.clone(),
                                });
                            }
                            AiStreamEvent::ToolCallDelta { id, arguments_delta } => {
                                let _ = event_tx.send(AgentEvent::ToolArgsDelta {
                                    id, delta: arguments_delta
                                });
                            }
                            AiStreamEvent::ToolCallComplete(tc) => {
                                has_tool_calls = true;
                                pending_tool_calls.push(tc);
                            }
                            AiStreamEvent::Done { finish_reason: _, usage } => {
                                final_usage = usage;
                            }
                            AiStreamEvent::Error(e) => {
                                stream_error = Some(e);
                            }
                        }
                    }
                }
            }

            // Drain any remaining events from the channel
            while let Ok(event) = stream_rx.try_recv() {
                match event {
                    AiStreamEvent::ThinkingDelta(text) => {
                        let _ = event_tx.send(AgentEvent::Thinking(text));
                    }
                    AiStreamEvent::ThinkingDone => {
                        let _ = event_tx.send(AgentEvent::ThinkingDone);
                    }
                    AiStreamEvent::ContentDelta(text) => {
                        assistant_text.push_str(&text);
                        let _ = event_tx.send(AgentEvent::Content(text));
                    }
                    AiStreamEvent::ToolCallStart(tc) => {
                        let _ = event_tx.send(AgentEvent::ToolStart {
                            name: tc.name.clone(),
                            id: tc.id.clone(),
                        });
                    }
                    AiStreamEvent::ToolCallDelta {
                        id,
                        arguments_delta,
                    } => {
                        let _ = event_tx.send(AgentEvent::ToolArgsDelta {
                            id,
                            delta: arguments_delta,
                        });
                    }
                    AiStreamEvent::ToolCallComplete(tc) => {
                        has_tool_calls = true;
                        pending_tool_calls.push(tc);
                    }
                    AiStreamEvent::Done {
                        finish_reason: _,
                        usage,
                    } => {
                        final_usage = usage;
                    }
                    AiStreamEvent::Error(e) => {
                        stream_error = Some(e);
                    }
                }
            }

            if let Some(error) = stream_error {
                eyre::bail!("{}", error);
            }

            // Tool APIs require the assistant call message immediately before results.
            if has_tool_calls {
                self.messages.push(Message::assistant_with_tool_calls(
                    &assistant_text,
                    pending_tool_calls.clone(),
                ));
            } else if !assistant_text.is_empty() {
                self.messages.push(Message::assistant(&assistant_text));
            }

            // If no tool calls, we're done
            if !has_tool_calls {
                let _ = self.event_tx.send(AgentEvent::Done { usage: final_usage });
                self.save_session();
                return Ok(());
            }

            // ── Check approvals ──────────────────────────────────────────
            let mut tool_approvals = std::collections::HashMap::new();
            if let Some(ref mut approval_rx) = self.approval_rx {
                for tc in &pending_tool_calls {
                    let name = tc.name.clone();
                    let id = tc.id.clone();
                    let args = match &tc.arguments {
                        serde_json::Value::String(s) => serde_json::from_str(s).unwrap_or(json!({})),
                        other => other.clone(),
                    };

                    if self.settings.needs_approval(&name) {
                        let _ = self.event_tx.send(AgentEvent::ApprovalRequest {
                            id: id.clone(),
                            tool_name: name.clone(),
                            args_preview: args.to_string(),
                        });

                        // Wait for user input
                        if let Some(approved) = approval_rx.recv().await {
                            tool_approvals.insert(id, approved);
                        } else {
                            tool_approvals.insert(id, false);
                        }
                    } else {
                        tool_approvals.insert(id, true);
                    }
                }
            } else {
                for tc in &pending_tool_calls {
                    tool_approvals.insert(tc.id.clone(), true);
                }
            }

            // ── Execute tool calls in PARALLEL ───────────────────────────
            let tool_ctx = ToolContext {
                settings: self.settings.clone(),
                cwd: self.settings.project_root.clone(),
                client: Some(self.client.clone()),
            };

            let mut handles = Vec::new();

            for tc in &pending_tool_calls {
                let name = tc.name.clone();
                let id = tc.id.clone();
                let args = match &tc.arguments {
                    serde_json::Value::String(s) => serde_json::from_str(s).unwrap_or(json!({})),
                    other => other.clone(),
                };
                let ctx = tool_ctx.clone();
                let tools = self.tools.clone();
                let ev_tx = self.event_tx.clone();
                let project_root = self.settings.project_root.clone();
                let is_approved = tool_approvals.get(&id).cloned().unwrap_or(true);

                handles.push(tokio::spawn(async move {
                    if !is_approved {
                        let res = ToolResult::err("⚠️ User denied execution of this tool call.");
                        let _ = ev_tx.send(AgentEvent::ToolResult {
                            id: id.clone(),
                            name: name.clone(),
                            result: res.to_message_content(),
                            success: false,
                        });
                        return (id, name, res);
                    }

                    // Snapshot files before write/edit operations
                    if name == "write_file" || name == "edit_file" {
                        if let Some(path_str) = args["path"].as_str() {
                            let full_path = ctx.cwd.join(path_str);
                            let _ = checkpoint::snapshot_file(&project_root, &full_path);
                        }
                    }

                    // Execute the tool
                    let result = tools.execute(&name, args.clone(), &ctx).await;

                    // Detect file writes for live streaming
                    if name == "write_file" || name == "edit_file" {
                        if let Some(data) = &result.data {
                            if let Some(path) = data["path"].as_str() {
                                let preview =
                                    result.output.chars().take(200).collect::<String>();
                                let _ = ev_tx.send(AgentEvent::FileWrite {
                                    path: path.to_string(),
                                    content_preview: preview,
                                });
                            }
                        }
                    }

                    let _ = ev_tx.send(AgentEvent::ToolResult {
                        id: id.clone(),
                        name: name.clone(),
                        result: result.to_message_content(),
                        success: result.success,
                    });

                    (id, name, result)
                }));
            }

            // Wait for ALL tool calls to complete in parallel
            let results = futures::future::join_all(handles).await;

            // Assemble tool result messages in the original order
            for result in results {
                match result {
                    Ok((id, name, tool_result)) => {
                        self.messages.push(Message::tool_result(
                            &id,
                            &name,
                            tool_result.to_message_content(),
                        ));
                    }
                    Err(e) => {
                        let err_msg = format!("Tool execution panicked: {}", e);
                        let _ = self.event_tx.send(AgentEvent::Error(err_msg.clone()));
                        // Still add a result so the model doesn't get confused
                        self.messages.push(Message::tool_result(
                            "error",
                            "internal",
                            err_msg,
                        ));
                    }
                }
            }

            // Continue the loop — the model will see tool results and continue
        }

        let _ = self.event_tx.send(AgentEvent::Error(format!(
            "Max thinking loops ({}) reached",
            max_loops
        )));
        let _ = self.event_tx.send(AgentEvent::Done { usage: None });
        self.save_session();

        Ok(())
    }

    /// Get the current conversation history.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Save current session to disk.
    fn save_session(&mut self) {
        if let Some(ref mut session) = self.session {
            session.messages = self.messages.clone();
            let _ = session.save(&self.settings.project_root);
        } else {
            let mut sess = session::Session::new(
                self.messages.clone(),
                &self.settings.active_model,
                &self.settings.active_provider,
            );
            let _ = sess.save(&self.settings.project_root);
            self.session = Some(sess);
        }
    }

    /// Get the context window size for the current model.
    fn get_context_window(&self) -> usize {
        let registry = crate::config::providers::build_registry();
        for provider in &registry {
            for model in &provider.models {
                if model.id == self.settings.active_model {
                    return model.context_window;
                }
            }
        }
        // Default fallback
        128_000
    }
}

fn build_system_prompt(settings: &Settings, tools: &ToolRegistry) -> String {
    let tool_names = tools.tool_names().join(", ");

    // Try to load project context for richer prompts
    let project_ctx = history::ProjectContext::load(&settings.project_root)
        .or_else(|_| Ok::<_, eyre::Report>(history::ProjectContext::scan(&settings.project_root)))
        .unwrap_or_else(|_| history::ProjectContext::scan(&settings.project_root));

    let project_info = project_ctx.to_prompt_summary();

    // Try to include README summary
    let readme_summary = load_readme_summary(&settings.project_root);

    format!(
        "You are Norvexum, an advanced AI coding assistant running inside a project directory.\n\n\
         {project_info}\n\
         Project root: {root}\n\
         Available tools: {tool_names}\n\
         {readme}\n\
         RULES:\n\
         - You can ONLY access files within the project directory (sandbox)\n\
         - Think step by step before acting\n\
         - When writing files, always show what you're writing\n\
         - Use tools to accomplish tasks — don't just describe what you'd do\n\
         - For web content, prefer web_fetch. Use browser_open only when blocked\n\
         - Check packages for safety before installing (check_package tool)\n\
         - Create Python venvs when pip packages are needed\n\
         - When an image is relevant, check if you can see it (vision) or use OCR\n\
         - For batch image analysis, use batch_view_images (up to 10 at once)\n\
         - Use git tools (git_status, git_diff, git_commit, git_log) for version control\n\
         - Be concise but thorough in your responses\n\
         - If generating images, decide whether to generate (create) or search (find existing)\n\
         - For fictional/anime/game character images, use zerochan_search. Format the query with comma-separated tags (e.g., 'Genshin Impact, Furina') to target Zerochan's tagging system precisely.\n\
         - For other standard images, real-world photos, and standard web graphics, use image_search.\n\
         - Save/download images to the user's desired/requested folder or the current working directory, not a hardcoded 'assets' folder\n",
        root = settings.project_root.display(),
        readme = readme_summary,
    )
}

fn load_readme_summary(root: &std::path::Path) -> String {
    for name in &["README.md", "readme.md", "README.txt", "README"] {
        let path = root.join(name);
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                let summary: String = content.chars().take(500).collect();
                return format!(
                    "\nPROJECT README (first 500 chars):\n{}{}\n",
                    summary,
                    if content.len() > 500 { "..." } else { "" }
                );
            }
        }
    }
    String::new()
}

fn detect_image(user_input: &str, project_root: &std::path::Path) -> Option<std::path::PathBuf> {
    let cleaned: String = user_input.replace(['(', ')', '[', ']', '`', '"', '\''], " ");
    for word in cleaned.split_whitespace() {
        let lower = word.to_lowercase();
        if lower.ends_with(".png")
            || lower.ends_with(".jpg")
            || lower.ends_with(".jpeg")
            || lower.ends_with(".webp")
            || lower.ends_with(".gif")
            || lower.ends_with(".bmp")
            || lower.ends_with(".avif")
        {
            let path = std::path::Path::new(word);
            let resolved = if path.is_absolute() {
                path.to_path_buf()
            } else {
                project_root.join(path)
            };
            if resolved.exists() && resolved.is_file() {
                return Some(resolved);
            }
        }
    }
    None
}

use serde_json::json;
