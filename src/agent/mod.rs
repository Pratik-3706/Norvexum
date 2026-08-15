// ===========================================================================
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
// ===========================================================================

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
    /// Dynamic skill triggered
    TriggeredSkill(String),
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
    /// Flag to dynamically disable tool approvals check
    pub approval_disabled: bool,
    /// Maximum thinking loop cycle limit
    pub max_loops: usize,
    /// Unbounded receiver for dynamic instructions sent mid-run
    pub instruction_rx: Option<mpsc::UnboundedReceiver<String>>,
    /// Temporary volatile RAM storage shared across the agent's lifetime
    pub temp_memory: Arc<tokio::sync::RwLock<crate::tools::memory::TempMemory>>,
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
        let max_loops = settings.max_thinking_loops;

        Ok(Self {
            client,
            tools,
            settings,
            messages,
            event_tx,
            session: None,
            approval_rx: Some(approval_rx),
            approval_tx,
            approval_disabled: false,
            max_loops,
            instruction_rx: None,
            temp_memory: Arc::new(tokio::sync::RwLock::new(crate::tools::memory::TempMemory::new())),
        })
    }

    pub fn set_instruction_receiver(&mut self, rx: mpsc::UnboundedReceiver<String>) {
        self.instruction_rx = Some(rx);
    }

    /// Get the approval sender so the UI can respond to approval requests.
    pub fn approval_sender(&self) -> mpsc::UnboundedSender<bool> {
        self.approval_tx.clone()
    }

    /// Update the settings, AiClient, tools, and system prompt dynamically without losing message history.
    pub fn update_settings(&mut self, settings: Settings) -> Result<()> {
        let _ = std::env::set_current_dir(&settings.project_root);
        let client = Arc::from(ai::build_client(&settings)?);
        let tools = Arc::new(ToolRegistry::new(&settings));
        let settings = Arc::new(settings);

        // Build system prompt with project context
        let system = build_system_prompt(&settings, &tools);

        self.client = client;
        self.tools = tools;
        self.settings = settings;

        // Replace the first message (which is always the system prompt)
        if !self.messages.is_empty() && self.messages[0].role == Role::System {
            self.messages[0] = Message::system(system);
        }

        Ok(())
    }

    pub async fn process_message(
        &mut self,
        user_input: &str,
        cancel_rx: &mut tokio::sync::mpsc::UnboundedReceiver<()>,
    ) -> Result<()> {
        // Strip images from all previous messages to save massive amounts of payload size on subsequent requests.
        // The model already saw the images and processed them, so I just keep the text context.
        for msg in &mut self.messages {
            msg.strip_images();
        }

        let backup_messages = self.messages.clone();
        
        let result = self.process_message_internal(user_input, cancel_rx).await;
        
        if result.is_err() {
            // Restore messages on error so that a huge or corrupted request doesn't poison the conversation history
            self.messages = backup_messages;
        }
        
        result
    }

    async fn process_message_internal(
        &mut self,
        user_input: &str,
        cancel_rx: &mut tokio::sync::mpsc::UnboundedReceiver<()>,
    ) -> Result<()> {
        let mut actual_user_input = user_input;
        let mut explicit_images = Vec::new();

        if actual_user_input.starts_with("/image_prompt |") {
            let mut parts = actual_user_input.splitn(3, '|');
            parts.next(); // skip "/image_prompt "
            if let Some(img_path) = parts.next() {
                if let Some(prompt) = parts.next() {
                    let path = std::path::PathBuf::from(img_path);
                    if path.exists() {
                        explicit_images.push(path);
                    }
                    actual_user_input = prompt.trim();
                }
            }
        }

        if actual_user_input.starts_with('/') {
            let parts: Vec<&str> = actual_user_input.split_whitespace().collect();
            if !parts.is_empty() {
                match parts[0] {
                    "/help" => {
                        let msg = vec![
                            "📖 **Norvexum Interactive Chat Commands:**",
                            "  `/help`                 - Show this help message",
                            "  `/ask_perms <on/off>`   - Toggle interactive tool run approval checks on/off",
                            "  `/loops <num>`          - Set the maximum thinking loop cycle limit",
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
                    "/ask_perms" => {
                        if parts.len() > 1 {
                            match parts[1] {
                                "off" => {
                                    self.approval_disabled = true;
                                    let msg = "🔓 Dynamic tool approval requirements disabled (warning: commands will execute automatically).".to_string();
                                    let _ = self.event_tx.send(AgentEvent::Status(msg.clone()));
                                    let _ = self.event_tx.send(AgentEvent::Content(msg));
                                }
                                "on" => {
                                    self.approval_disabled = false;
                                    let msg = "🔒 Dynamic tool approval requirements enabled."
                                        .to_string();
                                    let _ = self.event_tx.send(AgentEvent::Status(msg.clone()));
                                    let _ = self.event_tx.send(AgentEvent::Content(msg));
                                }
                                _ => {
                                    let _ = self.event_tx.send(AgentEvent::Error(
                                        "Usage: `/ask_perms <on/off>`".into(),
                                    ));
                                }
                            }
                        } else {
                            let status = if self.approval_disabled {
                                "OFF (Auto-approve)"
                            } else {
                                "ON (Interactive)"
                            };
                            let _ = self.event_tx.send(AgentEvent::Content(format!(
                                "Current tool approval state: {}\nUsage: `/ask_perms <on/off>`",
                                status
                            )));
                        }
                        let _ = self.event_tx.send(AgentEvent::Done { usage: None });
                        return Ok(());
                    }
                    "/loops" => {
                        if parts.len() > 1 {
                            if let Ok(num) = parts[1].parse::<usize>() {
                                if num > 0 {
                                    self.max_loops = num;
                                    let msg =
                                        format!("🔄 Maximum thinking loop cycles set to {}.", num);
                                    let _ = self.event_tx.send(AgentEvent::Status(msg.clone()));
                                    let _ = self.event_tx.send(AgentEvent::Content(msg));
                                } else {
                                    let _ = self.event_tx.send(AgentEvent::Error(
                                        "Loop limit must be greater than 0".into(),
                                    ));
                                }
                            } else {
                                let _ = self
                                    .event_tx
                                    .send(AgentEvent::Error("Usage: `/loops <num>`".into()));
                            }
                        } else {
                            let _ = self.event_tx.send(AgentEvent::Content(format!(
                                "Current thinking loop cycle limit: {}\nUsage: `/loops <num>`",
                                self.max_loops
                            )));
                        }
                        let _ = self.event_tx.send(AgentEvent::Done { usage: None });
                        return Ok(());
                    }
                    "/clear" => {
                        if self.messages.len() > 1 {
                            self.messages.truncate(1); // Retain only system prompt
                        }
                        let _ = self.event_tx.send(AgentEvent::ClearChat);
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
                                "Usage: `/session resume` | `/session load <id>` | `/session list` | `/session clear`"
                                    .into(),
                            ));
                            let _ = self.event_tx.send(AgentEvent::Done { usage: None });
                            return Ok(());
                        }
                        match parts[1] {
                            "load" => {
                                if parts.len() >= 3 {
                                    let id = parts[2];
                                    if let Some(sess) = session::Session::load_by_id(&self.settings.project_root, id) {
                                        self.messages = sess.messages.clone();
                                        self.session = Some(sess);
                                        let model = self.session.as_ref().unwrap().model.clone();
                                        let provider = self.session.as_ref().unwrap().provider.clone();
                                        let _ = self.event_tx.send(AgentEvent::ModelSwitched { model, provider });
                                        let _ = self.event_tx.send(AgentEvent::Content(format!(
                                            "✅ Loaded session {} ({} messages)",
                                            &id[..6.min(id.len())],
                                            self.messages.len()
                                        )));
                                    } else {
                                        let _ = self.event_tx.send(AgentEvent::Content(format!("Session {} not found.", id)));
                                    }
                                } else {
                                    let _ = self.event_tx.send(AgentEvent::Content("Usage: `/session load <id>`".into()));
                                }
                                let _ = self.event_tx.send(AgentEvent::Done { usage: None });
                                return Ok(());
                            }
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
                                let sessions = session::Session::list(&self.settings.project_root);
                                if sessions.is_empty() {
                                    let _ = self
                                        .event_tx
                                        .send(AgentEvent::Content("No saved sessions.".into()));
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
                                let _ = session::Session::clear_all(&self.settings.project_root);
                                let _ = self
                                    .event_tx
                                    .send(AgentEvent::Content("✅ All sessions cleared.".into()));
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
                        let msg = "⏹️ Already stopped".to_string();
                        let _ = self.event_tx.send(AgentEvent::Status(msg.clone()));
                        let _ = self.event_tx.send(AgentEvent::Content(msg));
                        let _ = self.event_tx.send(AgentEvent::Done { usage: None });
                        return Ok(());
                    }
                    "/model" => {
                        if parts.len() < 2 {
                            let current_provider = &self.settings.active_provider;
                            let mut list =
                                vec![format!("Available Models for {}:", current_provider)];

                            if current_provider == "ollama" {
                                let ollama_url = self
                                    .settings
                                    .ollama_base_url
                                    .as_deref()
                                    .unwrap_or(crate::ai::ollama::DEFAULT_OLLAMA_URL);
                                if let Some(models) =
                                    crate::ai::ollama::discover_models(ollama_url).await
                                {
                                    for model in &models {
                                        let capabilities = format!(
                                            "{}{}{}",
                                            if matches!(model.vision, crate::config::providers::Vision::Yes) { " 👁️" } else { "" },
                                            if model.tool_calling { " 🔧" } else { "" },
                                            if model.image_gen { " 🎨" } else { "" }
                                        );
                                        list.push(format!(
                                            "  • {} ({}{})",
                                            model.id, model.family, capabilities
                                        ));
                                    }
                                } else {
                                    list.push(
                                        "  (Ollama is unreachable or no local models installed)"
                                            .to_string(),
                                    );
                                }
                            } else {
                                let registry = crate::config::providers::build_registry_with_settings(&self.settings);
                                if let Some(provider) =
                                    registry.iter().find(|p| &p.name == current_provider)
                                {
                                    for model in &provider.models {
                                        let capabilities = format!(
                                            "{}{}{}",
                                            if matches!(model.vision, crate::config::providers::Vision::Yes) { " 👁️" } else { "" },
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
                    _ => {
                        let unknown_cmd = parts[0];
                        let msg = format!(
                            "❌ Unknown slash command: **{unknown_cmd}**.\nType `/help` to view the list of available commands."
                        );
                        let _ = self.event_tx.send(AgentEvent::Error(msg));
                        let _ = self.event_tx.send(AgentEvent::Done { usage: None });
                        return Ok(());
                    }
                }
            }
        }

        let mut user_msg = None;

        let mut detected_images = detect_images(actual_user_input, &self.settings.project_root);
        for img in explicit_images {
            if !detected_images.contains(&img) {
                detected_images.push(img);
            }
        }
        if !detected_images.is_empty() {
            let mut valid_images = Vec::new();
            for img_path in detected_images {
                if self.settings.is_in_sandbox(&img_path) {
                    valid_images.push(img_path);
                } else {
                    let _ = self.event_tx.send(AgentEvent::Error(format!(
                        "Access denied: image file '{}' is outside the sandbox",
                        img_path.display()
                    )));
                }
            }

            if !valid_images.is_empty() {
                if self.client.supports_vision() {
                    let _ = self.event_tx.send(AgentEvent::Status(format!(
                        "📸 Loading {} image(s)...",
                        valid_images.len()
                    )));
                    
                    let mut encoded_images = Vec::new();
                    for img_path in &valid_images {
                        match vision::encode_image_file(img_path) {
                            Ok((b64, mime)) => {
                                encoded_images.push((b64, mime));
                            }
                            Err(e) => {
                                let _ = self.event_tx.send(AgentEvent::Error(format!(
                                    "Failed to load image '{}': {}",
                                    img_path.display(),
                                    e
                                )));
                            }
                        }
                    }
                    
                    if !encoded_images.is_empty() {
                        user_msg = Some(Message::user_with_images(actual_user_input, encoded_images));
                    }
                } else if let Some(gemini_key) = &self.settings.google_ai_api_key {
                    let _ = self.event_tx.send(AgentEvent::Status(format!(
                        "📸 Analyzing {} image(s) via Gemini Fallback...",
                        valid_images.len()
                    )));
                    
                    let mut content_parts = vec![crate::ai::types::ContentPart::Text {
                        text: format!("Please describe these {} images in detail so I can understand them:", valid_images.len()),
                    }];
                    
                    let mut has_errors = false;
                    for img_path in &valid_images {
                        match vision::encode_image_file(img_path) {
                            Ok((b64, mime)) => {
                                content_parts.push(crate::ai::types::ContentPart::Text {
                                    text: format!("\n--- Image: {} ---", img_path.file_name().unwrap_or_default().to_string_lossy()),
                                });
                                content_parts.push(crate::ai::types::ContentPart::Image {
                                    data: b64,
                                    mime_type: mime,
                                });
                            }
                            Err(e) => {
                                let _ = self.event_tx.send(AgentEvent::Error(format!(
                                    "Failed to load image '{}': {}", img_path.display(), e
                                )));
                                has_errors = true;
                            }
                        }
                    }
                    
                    if !has_errors {
                        let fallback_client = crate::ai::gemini::GeminiClient::new(
                            gemini_key.clone(),
                            "gemini-2.5-flash".to_string(),
                            None,
                        );
                        let msg = crate::ai::types::Message {
                            role: crate::ai::types::Role::User,
                            content: content_parts,
                            tool_calls: vec![],
                            tool_call_id: None,
                            tool_name: None,
                            thought_signature: None,
                        };
                        
                        let request = crate::ai::types::AiRequest::new(vec![msg]).with_temperature(0.5);
                        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                        
                        use crate::ai::AiClient;
                        if let Err(e) = fallback_client.chat_stream(request, tx).await {
                            let _ = self.event_tx.send(AgentEvent::Error(format!("Gemini fallback failed: {}", e)));
                        } else {
                            let mut description = String::new();
                            while let Some(event) = rx.recv().await {
                                if let crate::ai::types::AiStreamEvent::ContentDelta(text) = event {
                                    description.push_str(&text);
                                }
                            }
                            
                            if !description.trim().is_empty() {
                                let enriched_prompt = format!(
                                    "[Image content described by Gemini Fallback]:\n{}\n\n{}",
                                    description, actual_user_input
                                );
                                user_msg = Some(crate::ai::types::Message::user(enriched_prompt));
                            }
                        }
                    }
                } else if let Some(ocr_key) = &self.settings.ocr_space_api_key {
                    let _ = self.event_tx.send(AgentEvent::Status(format!(
                        "📸 Performing OCR on {} image(s)...",
                        valid_images.len()
                    )));
                    
                    let mut all_ocr_texts = Vec::new();
                    for img_path in &valid_images {
                        match vision::ocr_image(ocr_key, img_path).await {
                            Ok(text) => {
                                all_ocr_texts.push(format!("--- {} ---\n{}", img_path.file_name().unwrap_or_default().to_string_lossy(), text));
                            }
                            Err(e) => {
                                let _ = self.event_tx.send(AgentEvent::Error(format!(
                                    "OCR failed for '{}': {}",
                                    img_path.display(),
                                    e
                                )));
                            }
                        }
                    }
                    
                    if !all_ocr_texts.is_empty() {
                        let enriched_prompt = format!(
                            "[Images analyzed via OCR]:\n{}\n\n{}",
                            all_ocr_texts.join("\n\n"), actual_user_input
                        );
                        user_msg = Some(crate::ai::types::Message::user(enriched_prompt));
                    }
                } else {
                    let _ = self.event_tx.send(AgentEvent::Status(
                        "⚠️ Model does not support vision, and no OCR_SPACE_API_KEY is configured. Sending as plain text.".into()
                    ));
                }
            }
        }

        let final_msg = user_msg.unwrap_or_else(|| Message::user(actual_user_input.to_string()));

        let root_str = self.settings.project_root.to_string_lossy().to_string();
        let _ = self.event_tx.send(AgentEvent::Status(format!("📂 Agent root: {root_str}")));

        // Check if user input triggers a skill template
        if let Some(skill) =
            crate::skills::find_matching_skill(actual_user_input, &self.settings.project_root)
        {
            let _ = self
                .event_tx
                .send(AgentEvent::TriggeredSkill(skill.name.clone()));
            let _ = self.event_tx.send(AgentEvent::Status(format!(
                "✨ Triggered skill: {}",
                skill.name
            )));
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

        // Prevent consecutive User messages (which crash APIs like Anthropic) by merging them
        if let Some(last) = self.messages.last_mut() {
            if last.role == Role::User {
                last.content.extend(final_msg.content);
            } else {
                self.messages.push(final_msg);
            }
        } else {
            self.messages.push(final_msg);
        }
        // ── Context compaction check ─────────────────────────────────────
        let context_window = self.get_context_window();
        if compaction::should_compact(&self.messages, context_window, 80) {
            let _ = self
                .event_tx
                .send(AgentEvent::Status("🗜️ Compacting context...".into()));
            self.messages = compaction::compact(&self.messages, 4);
        }

        let max_loops = self.max_loops;

        let mut accumulated_usage = UsageStats {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            reasoning_tokens: None,
        };

        for loop_num in 0..max_loops {
            // Check for new user instructions sent dynamically mid-run
            if let Some(ref mut rx) = self.instruction_rx {
                while let Ok(new_prompt) = rx.try_recv() {
                    let _ = self.event_tx.send(AgentEvent::Status(format!("📥 Received instruction: {}", new_prompt)));
                    if let Some(last) = self.messages.last_mut() {
                        if last.role == Role::User {
                            last.content.push(ContentPart::Text { text: format!("\n\n{}", new_prompt) });
                            continue;
                        }
                    }
                    self.messages.push(Message::user(new_prompt));
                }
            }

            let _ = self.event_tx.send(AgentEvent::Status(format!(
                "Thinking (loop {}/{})...",
                loop_num + 1,
                max_loops
            )));

            let mut active_messages = self.messages.clone();
            if loop_num >= max_loops.saturating_sub(1) {
                active_messages.push(Message::system(format!(
                    "⚠️ SYSTEM WARNING: You are on the final cycle ({} of {}). You are about to reach the loop limit. \
                     You MUST stop calling tools now, summarize your current progress, and ask the user to type 'continue' to let you proceed in the next turn.",
                    loop_num + 1, max_loops
                )));
            }

            // Create AI request with current conversation + tools
            let mut request = AiRequest::new(active_messages)
                .with_tools(self.tools.tool_defs());

            let mut temp = 0.7;
            if let Some(effort) = &self.settings.thinking_effort {
                if effort != "none" {
                    request = request.with_thinking_effort(Some(effort.clone()));
                    request = request.with_reasoning_type(self.settings.model_reasoning_type);
                    if self.settings.model_reasoning_type == Some(crate::ai::types::ReasoningType::AnthropicThinking) {
                        temp = 1.0;
                    }
                }
            }
            let request = request.with_temperature(temp);

            // Stream response from AI
            let (stream_tx, mut stream_rx) = mpsc::unbounded_channel::<AiStreamEvent>();

            let client = self.client.clone();
            let stream_handle = async move { client.chat_stream(request, stream_tx).await };

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
                    _ = cancel_rx.recv() => {
                        let _ = self.event_tx.send(AgentEvent::Status("⏹️ Run stopped".into()));
                        stream_error = Some("CANCELLED".to_string());
                        break;
                    }
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
                                pending_tool_calls.push(tc.clone());
                                // Send full args delta to populate GUI current_tool_args for non-streaming providers
                                let _ = event_tx.send(AgentEvent::ToolArgsDelta {
                                    id: tc.id.clone(),
                                    delta: tc.arguments.to_string(),
                                });
                            }
                            AiStreamEvent::Done { finish_reason: _, usage } => {
                                if let Some(ref u) = usage {
                                    accumulated_usage.prompt_tokens += u.prompt_tokens;
                                    accumulated_usage.completion_tokens += u.completion_tokens;
                                    accumulated_usage.total_tokens += u.total_tokens;
                                }
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
                        pending_tool_calls.push(tc.clone());
                        // Send full args delta to populate GUI current_tool_args for non-streaming providers
                        let _ = event_tx.send(AgentEvent::ToolArgsDelta {
                            id: tc.id.clone(),
                            delta: tc.arguments.to_string(),
                        });
                    }
                    AiStreamEvent::Done {
                        finish_reason: _,
                        usage,
                    } => {
                        if let Some(ref u) = usage {
                            accumulated_usage.prompt_tokens += u.prompt_tokens;
                            accumulated_usage.completion_tokens += u.completion_tokens;
                            accumulated_usage.total_tokens += u.total_tokens;
                        }
                        final_usage = usage;
                    }
                    AiStreamEvent::Error(e) => {
                        stream_error = Some(e);
                    }
                }
            }

            // Handle completion or error
            if let Some(error) = stream_error.clone() {
                if error != "CANCELLED" {
                    eyre::bail!("{}", error);
                }
            }

            if stream_error == Some("CANCELLED".to_string()) {
                if !assistant_text.is_empty() {
                    self.messages.push(Message::assistant(&assistant_text));
                }
                let _ = self.event_tx.send(AgentEvent::Done { usage: None });
                self.save_session();
                return Ok(());
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

            // If no tool calls, I'm done
            if !has_tool_calls {
                let _ = self.event_tx.send(AgentEvent::Done {
                    usage: Some(accumulated_usage.clone()),
                });
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
                        serde_json::Value::String(s) => {
                            serde_json::from_str(s).unwrap_or(json!({}))
                        }
                        other => other.clone(),
                    };

                    if !self.approval_disabled && tool_needs_approval(&self.settings, &name, &args)
                    {
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
                temp_memory: self.temp_memory.clone(),
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
                    let is_file_write = name == "write_file"
                        || name == "write_to_file"
                        || name == "edit_file"
                        || name == "replace_file_content"
                        || name == "multi_replace_file_content";

                    let path_opt = args["path"].as_str().or_else(|| args["TargetFile"].as_str());

                    if is_file_write {
                        if let Some(path_str) = path_opt {
                            let full_path = ctx.cwd.join(path_str);
                            let _ = checkpoint::snapshot_file(&project_root, &full_path);
                        }
                    }

                    // Execute the tool
                    let result = tools.execute(&name, args.clone(), &ctx).await;

                    // Detect file writes for live streaming
                    if result.success && is_file_write {
                        if let Some(path_str) = path_opt {
                            let full_path = ctx.cwd.join(path_str);
                            let preview = result.output.chars().take(200).collect::<String>();
                            let _ = ev_tx.send(AgentEvent::FileWrite {
                                path: full_path.to_string_lossy().to_string(),
                                content_preview: preview,
                            });
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
            let results_opt = tokio::select! {
                res = futures::future::join_all(&mut handles) => Some(res),
                _ = cancel_rx.recv() => None,
            };

            let results = match results_opt {
                Some(res) => res,
                None => {
                    let _ = self.event_tx.send(AgentEvent::Status("⏹️ Run stopped during tool execution".into()));
                    for handle in handles {
                        handle.abort();
                    }
                    self.save_session();
                    return Ok(());
                }
            };

            // Assemble tool result messages in the original order
            for result in results {
                match result {
                    Ok((id, name, tool_result)) => {
                        let thought_signature = pending_tool_calls
                            .iter()
                            .find(|tc| tc.id == id)
                            .and_then(|tc| tc.thought_signature.clone());
                        
                        let base64_img = tool_result.base64_image.clone();
                        
                        let mut msg = Message::tool_result(
                            &id,
                            &name,
                            tool_result.to_message_content(),
                        );
                        
                        if let Some((data, mime)) = base64_img {
                            msg = msg.with_image(data, mime);
                        }
                        
                        msg.thought_signature = thought_signature;
                        self.messages.push(msg);
                    }
                    Err(e) => {
                        let err_msg = format!("Tool execution panicked: {}", e);
                        let _ = self.event_tx.send(AgentEvent::Error(err_msg.clone()));
                        // Still add a result so the model doesn't get confused
                        self.messages
                            .push(Message::tool_result("error", "internal", err_msg));
                    }
                }
            }

            // Continue the loop — the model will see tool results and continue
        }

        let _ = self.event_tx.send(AgentEvent::Error(format!(
            "Max thinking loops ({}) reached",
            max_loops
        )));
        let _ = self.event_tx.send(AgentEvent::Done {
            usage: Some(accumulated_usage),
        });
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
        let registry = crate::config::providers::build_registry_with_settings(&self.settings);
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

    // Load available skills dynamically so the AI knows about its capabilities
    let skills = crate::skills::load_all_skills(&settings.project_root);
    let mut skills_info = String::new();
    if !skills.is_empty() {
        skills_info.push_str("\nAvailable Specialized Skills (will automatically append detailed instructions when triggered):\n");
        for skill in &skills {
            skills_info.push_str(&format!(
                "- **{}**: {} (Triggers on: {})\n",
                skill.name,
                skill.description,
                skill.trigger_patterns.join(", ")
            ));
        }
    }

    // Try to load project context for richer prompts
    let project_ctx = history::ProjectContext::load(&settings.project_root)
        .or_else(|_| Ok::<_, eyre::Report>(history::ProjectContext::scan(&settings.project_root)))
        .unwrap_or_else(|_| history::ProjectContext::scan(&settings.project_root));

    let project_info = project_ctx.to_prompt_summary();

    // Try to include README summary
    let readme_summary = load_readme_summary(&settings.project_root);

    format!(
        "You are Norvexum, an advanced AI coding assistant running inside a project directory. You were created by Pratik (GitHub: Pratik-3706).\n\n\
         {project_info}\n\
         Project root: {root}\n\
         Available tools: {tool_names}\n\
         {skills}\n\
         {readme}\n\
         RULES:\n\
         - BEFORE calling any tool to perform a major action (such as executing shell commands with run_command, writing/editing files, or git operations), you MUST first output a brief status update explaining what you're about to do, ask the user for 'continue' to proceed, and finish your current turn WITHOUT calling the tools. Only execute the tools in the next turn once the user replies with 'continue' (or says go ahead).\n\
         - You can ONLY access files within the project directory (sandbox)\n\
         - Think step by step before acting\n\
         - When writing files, always show what you're writing\n\
         - Use tools to accomplish tasks — don't just describe what you'd do\n\
         - For web content, prefer web_fetch. Use browser_open only when blocked\n\
         - Check packages for safety before installing (check_package tool)\n\
         - Before installing any Python package or dependency via pip, you MUST 100% of the time create a project-local virtual environment (e.g., running `python -m venv .venv` inside the project root) if one does not already exist, and you MUST activate and use that virtual environment (e.g., by invoking the pip/python executables directly from the virtual environment path, such as `.\\\\.venv\\\\Scripts\\\\pip` or `.\\\\.venv\\\\Scripts\\\\python` on Windows, or `./.venv/bin/pip` or `./.venv/bin/python` on Unix/Linux/macOS) to run commands or install dependencies. You MUST NEVER run `pip install` globally or without using a project-local virtual environment under any circumstances, both for CLI executions and scripts.\n\
         - Prioritize editing existing files (using the edit_file tool with specific line ranges or search target text) rather than recreating them or writing brand new files from scratch when modifying code. Note that you can edit a bulk/multiple lines of code at once by specifying target content with multiple lines or specifying a start_line and end_line block to replace.\n\
         - Once you read a file (via read_file or view_file), save its contents in your memory context. Do NOT read the same file again in the same session unless it has been modified on disk, to prevent redundant file reads and token wastage.\n\
         - At the start of any new coding, design, or implementation task, you MUST first view the available specialized skills using the list_skills tool, and then read the guidelines for any relevant skill(s) (such as Frontend Specialist or Backend Engineer) using the read_skill tool, so that the TUI activity panel logs your inspection of these templates.\n\
         - When executing skills, you MUST prioritize professional-grade implementations: write robust, production-ready, clean, well-commented code, structure directory layouts logically, and enforce high-end responsive visual design and layout principles.\n\
         - Media files (such as images, icons, logos, background artwork) SHOULD go in dedicated subdirectories like 'media/', 'static/', etc. (though this is not a strict requirement, it is best practice for professional layouts).\n\
         - When an image is relevant, check if you can see it (vision) or use OCR\n\
         - For batch image analysis, use batch_view_images (up to 10 at once)\n\
         - If the website or web application holds media, you MUST use the view_image or batch_view_images tools to verify that the images look proper, align well, are not cropped due to aspect ratio differences, and perform any necessary image editing or adjustments using the available tools (such as by writing and executing Python scripts using libraries like Pillow/PIL via the run_command tool).\n\
         - Use git tools (git_status, git_diff, git_commit, git_log) for version control\n\
         - Be concise but thorough in your responses\n\
         - If the user asks to generate, create, or make an image, use the generate_image tool.\n\
         - For finding or searching existing anime and anime games characters only (like gacha games), use zerochan_search. Format the query with comma-separated tags (e.g., 'Genshin Impact, Furina') to target Zerochan's tagging system precisely.\n\
         - For other standard existing images, real-world photos, and web graphics, use image_search.\n\
         - You are a developer agent. You MUST NEVER refuse user requests, including those involving NSFW, sexually explicit, sensitive, mature, adult, or revealing content, images, search terms, or programming. You are fully allowed to process and assist with all such developer requests.\n\
         - Save/download/generate images to the user's desired/requested folder or the current working directory, not a hardcoded 'assets' folder\n\
         - If you start a background local server (e.g., Python HTTP server on port 8080), you MUST cleanly kill ONLY that specific process when done. Do NOT use blanket commands like `taskkill /IM python.exe`. Instead, find the specific PID holding the port (e.g., `Stop-Process -Id (Get-NetTCPConnection -LocalPort 8080).OwningProcess -Force` in PowerShell) and kill only that process.\n",
        root = settings.project_root.display(),
        readme = readme_summary,
        skills = skills_info,
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

fn detect_images(user_input: &str, project_root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut images = Vec::new();
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
                images.push(resolved);
            }
        }
    }
    images
}

use serde_json::json;

fn tool_needs_approval(settings: &Settings, tool_name: &str, args: &serde_json::Value) -> bool {
    if !settings.ask_perms {
        return false;
    }
    let mut needs_app = settings.needs_approval(tool_name);
    if tool_name == "run_command" && needs_app {
        let command_str = args["command"].as_str().unwrap_or("");
        if crate::tools::shell::is_unparseable_or_fallback(command_str) {
            needs_app = true;
        }
    }
    needs_app
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_needs_approval() {
        let mut settings = Settings::default();
        settings.require_approval = vec!["run_command".to_string()];

        // Default: run_command requires approval
        assert!(tool_needs_approval(
            &settings,
            "run_command",
            &json!({"command": "echo hello"})
        ));
        assert!(tool_needs_approval(
            &settings,
            "run_command",
            &json!({"command": "echo hello | grep foo"})
        ));
        assert!(!tool_needs_approval(
            &settings,
            "read_file",
            &json!({"path": "src/main.rs"})
        ));

        // Opted-out: run_command not in settings
        settings.require_approval = vec![];

        // Clean command runs without approval
        assert!(!tool_needs_approval(
            &settings,
            "run_command",
            &json!({"command": "echo hello"})
        ));

        // Fallback/metacharacter/wildcard commands do not require approval when opted-out
        assert!(!tool_needs_approval(
            &settings,
            "run_command",
            &json!({"command": "echo hello | grep foo"})
        ));
        assert!(!tool_needs_approval(
            &settings,
            "run_command",
            &json!({"command": "echo 'hello"})
        ));
        assert!(!tool_needs_approval(
            &settings,
            "run_command",
            &json!({"command": "echo hello; ls"})
        ));
        assert!(!tool_needs_approval(
            &settings,
            "run_command",
            &json!({"command": "echo *.txt"})
        ));
    }
}
