// ===========================================================================
// Agent — Core reasoning loop with thinking + truly parallel tool calling
//
// Loop:
//   1. Send conversation + tools to AI model (streaming)
//   2. Display thinking tokens live in the UI
//   3. On tool calls → ask for approval where required, execute in parallel
//   4. Append results → repeat
//   5. On content → stream text live to the UI
//   6. On "done" → break
//   7. Auto-compact context when approaching the window limit
//   8. Auto-save session after each step
// ===========================================================================

pub mod checkpoint;
pub mod compaction;
pub mod history;
pub mod session;
pub mod vision;

use eyre::Result;
use serde_json::json;
use std::collections::HashSet;
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
    /// Error (a `Done` always follows when the run ends)
    Error(String),
    /// A problem worth showing that does not stop the run
    Warning(String),
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

const FILE_WRITE_TOOLS: &[&str] = &["write_file", "edit_file", "multi_edit"];

/// Everything one model response produced.
#[derive(Default)]
struct ModelTurn {
    text: String,
    tool_calls: Vec<ToolCall>,
    thinking_blocks: Vec<ThinkingBlock>,
    finish_reason: String,
    /// Tool call ids whose arguments were already streamed as deltas.
    streamed_args: HashSet<String>,
    error: Option<String>,
    cancelled: bool,
    thinking_chars: usize,
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
    approval_rx: mpsc::UnboundedReceiver<bool>,
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
    /// Skills whose guidance has already been added to this conversation.
    injected_skills: HashSet<String>,
    /// Whether the current turn has already executed tools (affects error recovery).
    turn_ran_tools: bool,
}

fn build_client_or_placeholder(settings: &Settings) -> Arc<dyn AiClient> {
    match ai::build_client(settings) {
        Ok(client) => Arc::from(client),
        Err(e) => Arc::new(ai::UnavailableClient {
            reason: e.to_string(),
        }),
    }
}

impl Agent {
    /// Never fails: a misconfigured provider yields a client that reports the
    /// problem on use, so the UI can still start and let the user fix it.
    pub fn new(mut settings: Settings, event_tx: mpsc::UnboundedSender<AgentEvent>) -> Self {
        if settings.model_reasoning_type.is_none() {
            settings.model_reasoning_type = ai::guess_reasoning_support_by_name(
                &settings.active_provider,
                &settings.active_model,
            );
        }
        let client = build_client_or_placeholder(&settings);
        let tools = Arc::new(ToolRegistry::new(&settings));
        let settings = Arc::new(settings);

        let system = build_system_prompt(&settings);
        let messages = vec![Message::system(system)];

        let (approval_tx, approval_rx) = mpsc::unbounded_channel();
        let max_loops = settings.max_thinking_loops;

        Self {
            client,
            tools,
            settings,
            messages,
            event_tx,
            session: None,
            approval_rx,
            approval_tx,
            approval_disabled: false,
            max_loops,
            instruction_rx: None,
            temp_memory: Arc::new(tokio::sync::RwLock::new(
                crate::tools::memory::TempMemory::new(),
            )),
            injected_skills: HashSet::new(),
            turn_ran_tools: false,
        }
    }

    pub fn set_instruction_receiver(&mut self, rx: mpsc::UnboundedReceiver<String>) {
        self.instruction_rx = Some(rx);
    }

    /// Get the approval sender so the UI can respond to approval requests.
    pub fn approval_sender(&self) -> mpsc::UnboundedSender<bool> {
        self.approval_tx.clone()
    }

    /// Update the settings, AiClient, tools, and system prompt dynamically without losing message history.
    pub fn update_settings(&mut self, mut settings: Settings) -> Result<()> {
        let _ = std::env::set_current_dir(&settings.project_root);
        if settings.model_reasoning_type.is_none() {
            settings.model_reasoning_type = ai::guess_reasoning_support_by_name(
                &settings.active_provider,
                &settings.active_model,
            );
        }
        let built = ai::build_client(&settings);
        let tools = Arc::new(ToolRegistry::new(&settings));
        let settings = Arc::new(settings);

        let system = build_system_prompt(&settings);

        self.client = match &built {
            Ok(_) => build_client_or_placeholder(&settings),
            Err(e) => Arc::new(ai::UnavailableClient {
                reason: e.to_string(),
            }),
        };
        self.tools = tools;
        self.max_loops = settings.max_thinking_loops;
        self.settings = settings;

        // Replace the first message (which is always the system prompt)
        if self
            .messages
            .first()
            .is_some_and(|m| m.role == Role::System)
        {
            self.messages[0] = Message::system(system);
        }

        built.map(|_| ())
    }

    pub async fn process_message(
        &mut self,
        user_input: &str,
        cancel_rx: &mut mpsc::UnboundedReceiver<()>,
    ) -> Result<Vec<String>> {
        // Earlier images were already seen by the model; resending them on every
        // request would multiply the payload size.
        for msg in &mut self.messages {
            msg.strip_images();
        }

        let backup_messages = self.messages.clone();
        self.turn_ran_tools = false;

        let result = self.process_message_internal(user_input, cancel_rx).await;

        // Roll back a failed turn so a broken request (e.g. an oversized image)
        // doesn't poison every later one — unless tools already ran, in which
        // case their effects are real and the model must keep seeing them.
        if result.is_err() && !self.turn_ran_tools {
            self.messages = backup_messages;
        }

        result
    }

    fn done(&self, usage: Option<UsageStats>) {
        let _ = self.event_tx.send(AgentEvent::Done { usage });
    }

    fn reply(&self, text: impl Into<String>) {
        let _ = self.event_tx.send(AgentEvent::Content(text.into()));
    }

    fn status(&self, text: impl Into<String>) {
        let _ = self.event_tx.send(AgentEvent::Status(text.into()));
    }

    /// Handle `/command` input. Returns `true` when the input was a command.
    async fn handle_slash_command(&mut self, input: &str) -> bool {
        let parts: Vec<&str> = input.split_whitespace().collect();
        let Some(&command) = parts.first() else {
            return false;
        };
        let arg = parts.get(1).copied();

        match command {
            "/help" => {
                self.reply(
                    [
                        "📖 **Norvexum commands**",
                        "  `/help`                 Show this help",
                        "  `/ask_perms <on/off>`   Toggle tool approval prompts",
                        "  `/loops <num>`          Set the tool-loop limit for one request",
                        "  `/clear`                Clear the chat and conversation memory",
                        "  `/compact`              Summarize older messages to free context",
                        "  `/copy` / `/copy chat`  Copy the last answer / whole chat",
                        "  `/undo`                 Restore the files changed by the last edit",
                        "  `/session resume|list|load <id>|clear`",
                        "  `/provider [id]`        List providers or switch",
                        "  `/model [id]`           List models or switch",
                        "  `/exit`                 Quit",
                        "",
                        "💡 *Esc stops a running request. Messages typed while it runs are queued.*",
                    ]
                    .join("\n"),
                );
            }
            "/ask_perms" => match arg {
                Some("off") => {
                    self.approval_disabled = true;
                    self.reply(
                        "🔓 Tool approval prompts disabled — commands will run automatically.",
                    );
                }
                Some("on") => {
                    self.approval_disabled = false;
                    self.reply("🔒 Tool approval prompts enabled.");
                }
                _ => {
                    let state = if self.approval_disabled {
                        "OFF (auto-approve)"
                    } else {
                        "ON (interactive)"
                    };
                    self.reply(format!(
                        "Tool approval: {state}\nUsage: `/ask_perms <on/off>`"
                    ));
                }
            },
            "/loops" => match arg.map(str::parse::<usize>) {
                Some(Ok(num)) if num > 0 => {
                    self.max_loops = num;
                    self.reply(format!("🔄 Tool-loop limit set to {num}."));
                }
                Some(_) => {
                    let _ = self.event_tx.send(AgentEvent::Error(
                        "Usage: `/loops <num>` (greater than 0)".into(),
                    ));
                }
                None => self.reply(format!(
                    "Current tool-loop limit: {}\nUsage: `/loops <num>`",
                    self.max_loops
                )),
            },
            "/clear" => {
                self.messages.truncate(1); // keep the system prompt
                self.injected_skills.clear();
                self.session = None;
                let _ = self.event_tx.send(AgentEvent::ClearChat);
            }
            "/compact" => {
                let before = compaction::estimate_tokens(&self.messages);
                self.messages = compaction::compact(&self.messages, 4);
                compaction::prune_tool_results(&mut self.messages, 2, 600);
                let after = compaction::estimate_tokens(&self.messages);
                self.save_session();
                self.reply(format!(
                    "🗜️ Compacted context: ~{before} → ~{after} tokens."
                ));
            }
            "/undo" => match checkpoint::undo_last(&self.settings.project_root) {
                Ok(restored) if restored.is_empty() => self.reply("No files were restored."),
                Ok(restored) => self.reply(format!(
                    "✅ Restored {} file(s):\n{}",
                    restored.len(),
                    restored
                        .iter()
                        .map(|f| format!("  • {f}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                )),
                Err(e) => {
                    let _ = self
                        .event_tx
                        .send(AgentEvent::Error(format!("Undo failed: {e}")));
                }
            },
            "/session" => self.handle_session_command(arg, parts.get(2).copied()),
            "/copy" => self.handle_copy(arg == Some("chat")),
            "/exit" | "/quit" => {
                self.save_session();
                let _ = self.event_tx.send(AgentEvent::Quit);
                return true;
            }
            "/stop" => self.reply("⏹️ Nothing is running."),
            "/model" => self.handle_model_command(arg).await,
            "/provider" => self.handle_provider_command(arg).await,
            _ => {
                let _ = self.event_tx.send(AgentEvent::Error(format!(
                    "❌ Unknown command **{command}**. Type `/help` for the list."
                )));
            }
        }
        self.done(None);
        true
    }

    fn handle_session_command(&mut self, sub: Option<&str>, id: Option<&str>) {
        let root = self.settings.project_root.clone();
        match sub {
            Some("load") => match id.and_then(|id| session::Session::load_by_id(&root, id)) {
                Some(sess) => {
                    self.messages = sess.messages.clone();
                    self.injected_skills.clear();
                    let _ = self.event_tx.send(AgentEvent::ModelSwitched {
                        model: self.settings.active_model.clone(),
                        provider: self.settings.active_provider.clone(),
                    });
                    self.reply(format!(
                        "✅ Loaded session {} ({} messages)",
                        crate::util::truncate_chars(&sess.id, 6),
                        self.messages.len()
                    ));
                    self.session = Some(sess);
                }
                None => self.reply(match id {
                    Some(id) => format!("Session {id} not found."),
                    None => "Usage: `/session load <id>`".into(),
                }),
            },
            Some("resume") => match session::Session::load_latest(&root) {
                Some(sess) => {
                    self.messages = sess.messages.clone();
                    self.injected_skills.clear();
                    self.reply(format!("✅ Resumed session ({} messages)", self.messages.len()));
                    self.session = Some(sess);
                }
                None => self.reply("No saved sessions found."),
            },
            Some("list") => {
                let sessions = session::Session::list(&root);
                if sessions.is_empty() {
                    self.reply("No saved sessions.");
                } else {
                    let list: Vec<String> = sessions
                        .iter()
                        .take(10)
                        .map(|s| {
                            format!(
                                "  • `{}` — {} msgs — {} — {}",
                                s.id, s.message_count, s.model, s.updated_at
                            )
                        })
                        .collect();
                    self.reply(format!("Saved sessions:\n{}", list.join("\n")));
                }
            }
            Some("clear") => {
                let _ = session::Session::clear_all(&root);
                self.session = None;
                self.reply("✅ All sessions cleared.");
            }
            _ => self.reply("Usage: `/session resume` | `/session list` | `/session load <id>` | `/session clear`"),
        }
    }

    fn handle_copy(&mut self, whole_chat: bool) {
        let content = if whole_chat {
            self.messages
                .iter()
                .filter_map(|msg| {
                    let who = match msg.role {
                        Role::User => "You",
                        Role::Assistant => "AI",
                        Role::Tool => "Tool",
                        Role::System => return None,
                    };
                    Some(format!("{who}:\n{}\n", msg.text()))
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            self.messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Assistant && !m.text().trim().is_empty())
                .map(|m| m.text())
                .unwrap_or_default()
        };

        if content.is_empty() {
            let _ = self
                .event_tx
                .send(AgentEvent::Error("Nothing to copy!".into()));
            return;
        }
        match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(content)) {
            Ok(()) => self.reply(if whole_chat {
                "📋 Copied the whole chat to the clipboard."
            } else {
                "📋 Copied the last answer to the clipboard."
            }),
            Err(e) => {
                let _ = self
                    .event_tx
                    .send(AgentEvent::Error(format!("Clipboard error: {e}")));
            }
        }
    }

    async fn models_for(&self, provider: &str) -> Vec<crate::config::providers::ModelInfo> {
        if provider == "ollama" {
            let url = self
                .settings
                .ollama_base_url
                .as_deref()
                .unwrap_or(crate::ai::ollama::DEFAULT_OLLAMA_URL);
            return crate::ai::ollama::discover_models(url)
                .await
                .unwrap_or_default();
        }
        crate::config::providers::build_registry_with_settings(&self.settings)
            .into_iter()
            .find(|p| p.name == provider)
            .map(|p| p.models)
            .unwrap_or_default()
    }

    fn switch_to(&mut self, mut settings: Settings, what: &str) {
        settings.model_reasoning_type =
            ai::guess_reasoning_support_by_name(&settings.active_provider, &settings.active_model);
        match ai::build_client(&settings) {
            Ok(client) => {
                self.client = Arc::from(client);
                self.settings = Arc::new(settings);
                if let Err(e) = self.settings.save() {
                    let _ = self
                        .event_tx
                        .send(AgentEvent::Warning(format!("Failed to save config: {e}")));
                }
                let _ = self.event_tx.send(AgentEvent::ModelSwitched {
                    model: self.settings.active_model.clone(),
                    provider: self.settings.active_provider.clone(),
                });
                self.reply(format!(
                    "Switched {what}. Now using **{}** on **{}**.",
                    self.settings.active_model, self.settings.active_provider
                ));
            }
            Err(e) => {
                let _ = self
                    .event_tx
                    .send(AgentEvent::Error(format!("Failed to switch {what}: {e}")));
            }
        }
    }

    async fn handle_model_command(&mut self, arg: Option<&str>) {
        let provider = self.settings.active_provider.clone();
        let Some(model_id) = arg else {
            let models = self.models_for(&provider).await;
            let mut list = vec![format!("Models for **{provider}**:")];
            if models.is_empty() {
                list.push("  (none found — for Ollama, check that it is running)".into());
            }
            for model in &models {
                let caps = format!(
                    "{}{}{}",
                    if matches!(model.vision, crate::config::providers::Vision::Yes) {
                        " 👁"
                    } else {
                        ""
                    },
                    if model.tool_calling { " 🔧" } else { "" },
                    if model.image_gen { " 🎨" } else { "" }
                );
                let marker = if model.id == self.settings.active_model {
                    "▶"
                } else {
                    "•"
                };
                list.push(format!("  {marker} {} ({}{caps})", model.id, model.family));
            }
            list.push("\nSwitch with `/model <id>`.".into());
            self.reply(list.join("\n"));
            return;
        };

        let mut settings = (*self.settings).clone();
        settings.active_model = model_id.to_string();
        self.switch_to(settings, "model");
    }

    async fn handle_provider_command(&mut self, arg: Option<&str>) {
        let Some(provider_id) = arg else {
            let mut list = vec!["Providers:".to_string()];
            for p in crate::config::providers::build_registry() {
                let marker = if p.name == self.settings.active_provider {
                    "▶"
                } else {
                    "•"
                };
                list.push(format!("  {marker} {} — {}", p.name, p.display_name));
            }
            list.push("\nSwitch with `/provider <id>`.".into());
            self.reply(list.join("\n"));
            return;
        };

        let mut settings = (*self.settings).clone();
        settings.active_provider = provider_id.to_string();

        // Keep the current model only if the new provider actually offers it.
        let models = self.models_for(provider_id).await;
        if !models.iter().any(|m| m.id == settings.active_model) {
            let preferred = match provider_id {
                "google_direct" => Some("gemini-2.5-flash"),
                "aicredits" => Some("google/gemini-2.5-flash"),
                "anthropic" => Some("claude-sonnet-5-5"),
                "openai" => Some("gpt-4o"),
                _ => None,
            };
            if let Some(model) = preferred
                .filter(|p| models.iter().any(|m| m.id == *p))
                .map(str::to_string)
                .or_else(|| models.first().map(|m| m.id.clone()))
                .or_else(|| preferred.map(str::to_string))
            {
                settings.active_model = model;
            }
        }
        self.switch_to(settings, "provider");
    }

    /// Turn the user's input (and any referenced images) into a message.
    async fn build_user_message(
        &mut self,
        text: &str,
        explicit_images: Vec<std::path::PathBuf>,
    ) -> Message {
        let mut images = detect_images(text, &self.settings.project_root);
        for img in explicit_images {
            if !images.contains(&img) {
                images.push(img);
            }
        }
        images.retain(|img| {
            let ok = self.settings.is_in_sandbox(img);
            if !ok {
                let _ = self.event_tx.send(AgentEvent::Warning(format!(
                    "Image '{}' is outside the project sandbox and was not attached",
                    img.display()
                )));
            }
            ok
        });
        if images.is_empty() {
            return Message::user(text);
        }

        if self.client.supports_vision() {
            self.status(format!("📸 Loading {} image(s)...", images.len()));
            let mut encoded = Vec::new();
            for img in &images {
                match vision::encode_image_file(img) {
                    Ok(pair) => encoded.push(pair),
                    Err(e) => {
                        let _ = self.event_tx.send(AgentEvent::Warning(format!(
                            "Failed to load image '{}': {e}",
                            img.display()
                        )));
                    }
                }
            }
            if !encoded.is_empty() {
                return Message::user_with_images(text, encoded);
            }
            return Message::user(text);
        }

        // The active model is text-only: describe the images first.
        let descriptions = if let Some(key) = self.settings.google_ai_api_key.clone() {
            self.status(format!(
                "📸 Describing {} image(s) via Gemini...",
                images.len()
            ));
            self.describe_images_with_gemini(&key, &images).await
        } else if let Some(key) = self.settings.ocr_space_api_key.clone() {
            self.status(format!("📸 Running OCR on {} image(s)...", images.len()));
            let mut texts = Vec::new();
            for img in &images {
                match vision::ocr_image(&key, img).await {
                    Ok(t) => texts.push(format!(
                        "--- {} ---\n{t}",
                        img.file_name().unwrap_or_default().to_string_lossy()
                    )),
                    Err(e) => {
                        let _ = self.event_tx.send(AgentEvent::Warning(format!(
                            "OCR failed for '{}': {e}",
                            img.display()
                        )));
                    }
                }
            }
            (!texts.is_empty())
                .then(|| format!("[Images analyzed via OCR]:\n{}", texts.join("\n\n")))
        } else {
            let _ = self.event_tx.send(AgentEvent::Warning(
                "This model can't see images and no GOOGLE_AI_API_KEY / OCR_SPACE_API_KEY is set; sending text only."
                    .into(),
            ));
            None
        };

        match descriptions {
            Some(desc) => Message::user(format!("{desc}\n\n{text}")),
            None => Message::user(text),
        }
    }

    async fn describe_images_with_gemini(
        &self,
        key: &str,
        images: &[std::path::PathBuf],
    ) -> Option<String> {
        let mut msg = Message::user(format!(
            "Describe these {} images in detail so a text-only assistant can understand them:",
            images.len()
        ));
        for img in images {
            match vision::encode_image_file(img) {
                Ok((data, mime_type)) => {
                    msg.content.push(ContentPart::Text {
                        text: format!(
                            "\n--- Image: {} ---",
                            img.file_name().unwrap_or_default().to_string_lossy()
                        ),
                    });
                    msg.content.push(ContentPart::Image { data, mime_type });
                }
                Err(e) => {
                    let _ = self.event_tx.send(AgentEvent::Warning(format!(
                        "Failed to load image '{}': {e}",
                        img.display()
                    )));
                }
            }
        }

        let client =
            crate::ai::gemini::GeminiClient::new(key.to_string(), "gemini-2.5-flash".into(), None);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let request = AiRequest::new(vec![msg]).with_temperature(0.5);
        if let Err(e) = client.chat_stream(request, tx).await {
            let _ = self.event_tx.send(AgentEvent::Warning(format!(
                "Gemini image description failed: {e}"
            )));
            return None;
        }
        let mut description = String::new();
        while let Some(event) = rx.recv().await {
            match event {
                AiStreamEvent::ContentDelta(text) => description.push_str(&text),
                AiStreamEvent::Error(e) => {
                    let _ = self.event_tx.send(AgentEvent::Warning(format!(
                        "Gemini image description failed: {e}"
                    )));
                }
                _ => {}
            }
        }
        (!description.trim().is_empty())
            .then(|| format!("[Image content described by Gemini]:\n{description}"))
    }

    async fn process_message_internal(
        &mut self,
        user_input: &str,
        cancel_rx: &mut mpsc::UnboundedReceiver<()>,
    ) -> Result<Vec<String>> {
        let mut text = user_input;
        let mut explicit_images = Vec::new();

        // GUI attachment protocol: "/image_prompt |<path>| <prompt>"
        if let Some(rest) = text.strip_prefix("/image_prompt |") {
            if let Some((img_path, prompt)) = rest.split_once('|') {
                let path = std::path::PathBuf::from(img_path);
                if path.exists() {
                    explicit_images.push(path);
                }
                text = prompt.trim();
            }
        }

        if text.starts_with('/') && self.handle_slash_command(text).await {
            return Ok(self.drain_instructions());
        }

        let mut user_msg = self.build_user_message(text, explicit_images).await;

        // Skill guidance goes into the user turn, once per conversation. Editing
        // the system prompt instead would invalidate the provider's prompt cache
        // on every trigger and grow without bound.
        if let Some(skill) = crate::skills::find_matching_skill(text, &self.settings.project_root) {
            let _ = self
                .event_tx
                .send(AgentEvent::TriggeredSkill(skill.name.clone()));
            if self.injected_skills.insert(skill.name.clone()) {
                self.status(format!("✨ Triggered skill: {}", skill.name));
                user_msg.content.push(ContentPart::Text {
                    text: format!(
                        "\n\n[Norvexum skill guidance — {}]\n{}",
                        skill.name, skill.system_instructions
                    ),
                });
            }
        }

        // Consecutive user turns are rejected by some APIs; merge instead.
        match self.messages.last_mut() {
            Some(last) if last.role == Role::User => last.content.extend(user_msg.content),
            _ => self.messages.push(user_msg),
        }

        self.compact_if_needed(false);

        let max_loops = self.max_loops;
        let mut usage = UsageStats {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            reasoning_tokens: None,
        };

        let mut loop_num = 0;
        let mut malformed_retries = 0;
        loop {
            if max_loops > 0 && loop_num >= max_loops {
                let _ = self.event_tx.send(AgentEvent::Warning(format!(
                    "Reached the tool-loop limit ({max_loops}). Type 'continue' to let the agent keep going, or raise it with /loops."
                )));
                self.save_session();
                self.done(Some(usage));
                return Ok(self.drain_instructions());
            }

            self.absorb_instructions();

            let loop_display = if max_loops > 0 {
                format!("{}/{}", loop_num + 1, max_loops)
            } else {
                format!("{}", loop_num + 1)
            };
            self.status(format!("Thinking (step {loop_display})..."));

            if loop_num > 0 {
                self.compact_if_needed(true);
            }

            let mut active_messages = self.messages.clone();
            if max_loops > 1 && loop_num + 1 == max_loops {
                active_messages.push(Message::system(format!(
                    "This is the final step ({} of {max_loops}) before the tool-loop limit. Do not call more tools: \
                     summarize what is done, what remains, and tell the user to type 'continue' to proceed.",
                    loop_num + 1
                )));
            }

            let request = self.build_request(active_messages);
            let turn = self.stream_with_retry(request, cancel_rx, &mut usage).await;

            if turn.cancelled {
                if !turn.text.is_empty() {
                    self.messages.push(Message::assistant(&turn.text));
                }
                self.status("⏹️ Run stopped");
                self.save_session();
                self.done(Some(usage));
                return Ok(self.drain_instructions());
            }
            if let Some(error) = turn.error {
                eyre::bail!("{error}");
            }

            // Gemini sometimes emits a broken function call; ask it to try again.
            if turn.tool_calls.is_empty()
                && turn.finish_reason == "MALFORMED_FUNCTION_CALL"
                && malformed_retries < 2
            {
                malformed_retries += 1;
                self.status("Model produced a malformed tool call — retrying");
                self.messages.push(Message::system(
                    "Your previous function call was malformed and was not executed. Call the tool again \
                     with valid JSON arguments; if the arguments are large, split the work into smaller calls \
                     (for example one edit_file call per change).",
                ));
                continue;
            }

            if turn.tool_calls.is_empty() {
                if turn.finish_reason == "MALFORMED_FUNCTION_CALL" {
                    let _ = self.event_tx.send(AgentEvent::Warning(
                        "The model kept producing malformed tool calls. Try rephrasing, or switch models with /model."
                            .into(),
                    ));
                }
                if !turn.text.trim().is_empty() {
                    let mut msg = Message::assistant(&turn.text);
                    msg.thinking_blocks = turn.thinking_blocks;
                    self.messages.push(msg);
                }
                if is_truncated(&turn.finish_reason) {
                    let _ = self.event_tx.send(AgentEvent::Warning(
                        "The reply hit the output token limit and may be cut off — type 'continue' to get the rest."
                            .into(),
                    ));
                }
                self.save_session();
                self.done(Some(usage));
                return Ok(self.drain_instructions());
            }

            // Tool APIs require the assistant call message immediately before results.
            let calls = turn.tool_calls;
            let mut assistant = Message::assistant_with_tool_calls(&turn.text, calls.clone());
            assistant.thinking_blocks = turn.thinking_blocks;
            self.messages.push(assistant);

            let Some(approvals) = self.collect_approvals(&calls, cancel_rx).await else {
                self.finish_cancelled_tools(&calls, "Cancelled by the user before running.");
                self.done(Some(usage));
                return Ok(self.drain_instructions());
            };

            let Some(results) = self.execute_tools(&calls, &approvals, cancel_rx).await else {
                self.finish_cancelled_tools(&calls, "Cancelled by the user while running.");
                self.done(Some(usage));
                return Ok(self.drain_instructions());
            };

            self.turn_ran_tools = true;
            for (call, result) in calls.iter().zip(results) {
                self.messages.push(self.tool_result_message(call, result));
            }

            self.save_session();
            loop_num += 1;
        }
    }

    fn build_request(&self, messages: Vec<Message>) -> AiRequest {
        let mut request = AiRequest::new(messages).with_tools(self.tools.tool_defs());
        let mut temperature = 0.7;
        // Models using `reasoning_effort` (o-series, DeepSeek V4, …) stay silent
        // unless asked, so with no effort chosen they get the medium default and
        // still stream their reasoning. Other APIs only think when opted in.
        let effort = match self.settings.thinking_effort.as_deref() {
            Some(e) if e != "none" && !e.is_empty() => Some(e),
            _ if self.settings.model_reasoning_type == Some(ReasoningType::OpenAiEffort) => {
                Some("medium")
            }
            _ => None,
        };
        if let Some(effort) = effort {
            request = request
                .with_thinking_effort(Some(effort.to_string()))
                .with_reasoning_type(self.settings.model_reasoning_type);
            if self.settings.model_reasoning_type == Some(ReasoningType::AnthropicThinking) {
                temperature = 1.0;
            }
        }
        tracing::info!(
            model = %self.settings.active_model,
            effort = ?request.thinking_effort,
            reasoning = ?request.reasoning_type,
            "model request"
        );
        request.with_temperature(temperature)
    }

    /// `stream_model_turn`, retrying transient failures (rate limits, overload,
    /// dropped connections) with backoff before anything was streamed.
    async fn stream_with_retry(
        &self,
        request: AiRequest,
        cancel_rx: &mut mpsc::UnboundedReceiver<()>,
        usage: &mut UsageStats,
    ) -> ModelTurn {
        const MAX_RETRIES: u32 = 3;
        let mut attempt = 0;
        loop {
            let turn = self
                .stream_model_turn(request.clone(), cancel_rx, usage)
                .await;
            let produced_output = !turn.text.is_empty() || !turn.tool_calls.is_empty();
            let delay = match &turn.error {
                Some(err) if !produced_output && attempt < MAX_RETRIES => retry_delay(err, attempt),
                _ => None,
            };
            let Some(delay) = delay else {
                return turn;
            };
            attempt += 1;
            self.status(format!(
                "⏳ {} — retrying in {}s (attempt {attempt}/{MAX_RETRIES})",
                short_error(turn.error.as_deref().unwrap_or_default()),
                delay.as_secs()
            ));
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = cancel_rx.recv() => {
                    return ModelTurn { cancelled: true, ..ModelTurn::default() };
                }
            }
        }
    }

    /// Stream one model response, forwarding live events to the UI.
    async fn stream_model_turn(
        &self,
        request: AiRequest,
        cancel_rx: &mut mpsc::UnboundedReceiver<()>,
        usage: &mut UsageStats,
    ) -> ModelTurn {
        let (stream_tx, mut stream_rx) = mpsc::unbounded_channel::<AiStreamEvent>();
        let client = self.client.clone();
        let stream = async move { client.chat_stream(request, stream_tx).await };
        tokio::pin!(stream);

        let mut turn = ModelTurn::default();
        let mut stream_finished = false;
        loop {
            tokio::select! {
                _ = cancel_rx.recv() => {
                    turn.cancelled = true;
                    break;
                }
                result = &mut stream, if !stream_finished => {
                    stream_finished = true;
                    // Connection-level failures (DNS, TLS, timeouts) land here.
                    if let Err(e) = result {
                        turn.error.get_or_insert(format!("Request failed: {e}"));
                    }
                }
                event = stream_rx.recv() => match event {
                    Some(event) => self.apply_stream_event(event, &mut turn, usage),
                    // The sender is dropped once the stream future completes.
                    None => break,
                },
            }
        }
        tracing::info!(
            thinking_chars = turn.thinking_chars,
            text_chars = turn.text.len(),
            tool_calls = turn.tool_calls.len(),
            error = ?turn.error,
            "model turn finished"
        );
        turn
    }

    fn apply_stream_event(
        &self,
        event: AiStreamEvent,
        turn: &mut ModelTurn,
        usage: &mut UsageStats,
    ) {
        let tx = &self.event_tx;
        match event {
            AiStreamEvent::ThinkingDelta(text) => {
                turn.thinking_chars += text.len();
                let _ = tx.send(AgentEvent::Thinking(text));
            }
            AiStreamEvent::ThinkingDone => {
                let _ = tx.send(AgentEvent::ThinkingDone);
            }
            AiStreamEvent::ThinkingBlock(block) => turn.thinking_blocks.push(block),
            AiStreamEvent::ContentDelta(text) => {
                turn.text.push_str(&text);
                let _ = tx.send(AgentEvent::Content(text));
            }
            AiStreamEvent::ToolCallStart(tc) => {
                let _ = tx.send(AgentEvent::ToolStart {
                    name: tc.name,
                    id: tc.id,
                });
            }
            AiStreamEvent::ToolCallDelta {
                id,
                arguments_delta,
            } => {
                turn.streamed_args.insert(id.clone());
                let _ = tx.send(AgentEvent::ToolArgsDelta {
                    id,
                    delta: arguments_delta,
                });
            }
            AiStreamEvent::ToolCallComplete(tc) => {
                // Providers that deliver arguments in one piece still need the UI
                // to see them (live file previews, approval context).
                if !turn.streamed_args.contains(&tc.id) {
                    let delta = match &tc.arguments {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    let _ = tx.send(AgentEvent::ToolArgsDelta {
                        id: tc.id.clone(),
                        delta,
                    });
                }
                turn.tool_calls.push(tc);
            }
            AiStreamEvent::Done {
                finish_reason,
                usage: turn_usage,
            } => {
                if let Some(u) = turn_usage {
                    usage.prompt_tokens += u.prompt_tokens;
                    usage.completion_tokens += u.completion_tokens;
                    usage.total_tokens += u.total_tokens;
                    if let Some(r) = u.reasoning_tokens {
                        *usage.reasoning_tokens.get_or_insert(0) += r;
                    }
                }
                turn.finish_reason = finish_reason;
            }
            AiStreamEvent::Error(e) => turn.error = Some(e),
        }
    }

    /// Ask the user about every call that needs approval.
    /// Returns `None` if the run was cancelled while waiting.
    async fn collect_approvals(
        &mut self,
        calls: &[ToolCall],
        cancel_rx: &mut mpsc::UnboundedReceiver<()>,
    ) -> Option<Vec<bool>> {
        // Ignore stray answers left over from an earlier prompt.
        while self.approval_rx.try_recv().is_ok() {}

        let mut approvals = Vec::with_capacity(calls.len());
        for tc in calls {
            if self.approval_disabled || !tool_needs_approval(&self.settings, &tc.name) {
                approvals.push(true);
                continue;
            }
            let args = parse_tool_args(&tc.arguments).unwrap_or_else(|| json!({}));
            let _ = self.event_tx.send(AgentEvent::ApprovalRequest {
                id: tc.id.clone(),
                tool_name: tc.name.clone(),
                args_preview: serde_json::to_string_pretty(&args).unwrap_or_default(),
            });
            tokio::select! {
                answer = self.approval_rx.recv() => approvals.push(answer.unwrap_or(false)),
                _ = cancel_rx.recv() => return None,
            }
        }
        Some(approvals)
    }

    /// Run all calls in parallel. Results come back in the same order as `calls`.
    /// Returns `None` if the run was cancelled (running tools are aborted).
    async fn execute_tools(
        &self,
        calls: &[ToolCall],
        approvals: &[bool],
        cancel_rx: &mut mpsc::UnboundedReceiver<()>,
    ) -> Option<Vec<ToolResult>> {
        let ctx = ToolContext {
            settings: self.settings.clone(),
            cwd: self.settings.project_root.clone(),
            client: Some(self.client.clone()),
            temp_memory: self.temp_memory.clone(),
        };

        let mut handles = Vec::with_capacity(calls.len());
        for (tc, &approved) in calls.iter().zip(approvals) {
            let tc = tc.clone();
            let ctx = ctx.clone();
            let tools = self.tools.clone();
            let ev_tx = self.event_tx.clone();

            handles.push(tokio::spawn(async move {
                let result = if !approved {
                    ToolResult::err("The user denied this tool call. Do not retry it; ask the user how to proceed if needed.")
                } else if let Some(args) = parse_tool_args(&tc.arguments) {
                    let name = crate::tools::canonical_tool_name(&tc.name);
                    let writes_file = FILE_WRITE_TOOLS.contains(&name);
                    let target = args["path"].as_str().map(|p| ctx.cwd.join(p));

                    if writes_file {
                        if let Some(path) = &target {
                            if let Err(e) = checkpoint::snapshot_file(&ctx.cwd, path) {
                                tracing::warn!("Checkpoint failed for {}: {e}", path.display());
                            }
                        }
                    }

                    let result = tools.execute(&tc.name, args, &ctx).await;

                    if result.success && writes_file {
                        if let Some(path) = target {
                            let _ = ev_tx.send(AgentEvent::FileWrite {
                                path: path.to_string_lossy().to_string(),
                                content_preview: crate::util::truncate_chars(&result.output, 200),
                            });
                        }
                    }
                    result
                } else {
                    ToolResult::err(
                        "The tool arguments were not valid JSON (often because the reply hit the output \
                         token limit). Re-issue the call with complete arguments; for large files, write \
                         them in smaller parts with write_file + edit_file.",
                    )
                };

                let _ = ev_tx.send(AgentEvent::ToolResult {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    result: result.to_message_content(),
                    success: result.success,
                });
                result
            }));
        }

        let joined = tokio::select! {
            res = futures::future::join_all(handles.iter_mut()) => res,
            _ = cancel_rx.recv() => {
                for handle in &handles {
                    handle.abort();
                }
                return None;
            }
        };

        Some(
            joined
                .into_iter()
                .zip(calls)
                .map(|(res, tc)| match res {
                    Ok(result) => result,
                    Err(e) => {
                        let msg = format!("Tool `{}` crashed: {e}", tc.name);
                        let _ = self.event_tx.send(AgentEvent::Warning(msg.clone()));
                        let _ = self.event_tx.send(AgentEvent::ToolResult {
                            id: tc.id.clone(),
                            name: tc.name.clone(),
                            result: msg.clone(),
                            success: false,
                        });
                        ToolResult::err(msg)
                    }
                })
                .collect(),
        )
    }

    /// Every tool call must be answered, or the next request is rejected.
    fn finish_cancelled_tools(&mut self, calls: &[ToolCall], reason: &str) {
        for tc in calls {
            self.messages.push(Message::tool_result(
                &tc.id,
                &tc.name,
                format!("ERROR: {reason}"),
            ));
        }
        self.status("⏹️ Run stopped");
        self.save_session();
    }

    fn tool_result_message(&self, call: &ToolCall, result: ToolResult) -> Message {
        // Large outputs are cut in the middle: command errors and summaries live
        // at the end, file headers at the start.
        let max_chars = self.settings.max_content_chars.clamp(2_000, 40_000);
        let content = crate::util::truncate_middle(&result.to_message_content(), max_chars);
        let mut msg = Message::tool_result(&call.id, &call.name, content);
        if let Some((data, mime)) = result.base64_image {
            msg = msg.with_image(data, mime);
        }
        msg.thought_signature = call.thought_signature.clone();
        msg
    }

    /// Fold instructions typed during a run into the conversation.
    fn absorb_instructions(&mut self) {
        let Some(rx) = self.instruction_rx.as_mut() else {
            return;
        };
        let mut incoming = Vec::new();
        while let Ok(text) = rx.try_recv() {
            incoming.push(text);
        }
        for text in incoming {
            self.status(format!(
                "📥 Received instruction: {}",
                crate::util::truncate_chars(&text, 80)
            ));
            match self.messages.last_mut() {
                Some(last) if last.role == Role::User => last.content.push(ContentPart::Text {
                    text: format!("\n\n{text}"),
                }),
                _ => self.messages.push(Message::user(text)),
            }
        }
    }

    /// Shrink the conversation when it nears the model's context window.
    ///
    /// Mid-turn edits change the history that already-received thinking blocks
    /// were signed against, so those blocks are dropped in that case.
    fn compact_if_needed(&mut self, mid_turn: bool) {
        let window = self.get_context_window();
        if !compaction::should_compact(&self.messages, window, 75) {
            return;
        }
        let before = compaction::estimate_tokens(&self.messages);
        self.status("🗜️ Compacting context...");

        // 1. Cheap: shorten old tool outputs, keeping the conversation structure.
        compaction::prune_tool_results(&mut self.messages, 6, 1_500);
        // 2. If that wasn't enough, summarize older exchanges.
        if compaction::should_compact(&self.messages, window, 75) {
            self.messages = compaction::compact(&self.messages, 4);
        }
        // 3. Still too big (one enormous turn): shorten every tool output.
        if compaction::should_compact(&self.messages, window, 90) {
            compaction::prune_tool_results(&mut self.messages, 0, 400);
        }

        if mid_turn {
            for msg in &mut self.messages {
                msg.thinking_blocks.clear();
            }
        }
        let after = compaction::estimate_tokens(&self.messages);
        self.status(format!("🗜️ Context compacted: ~{before} → ~{after} tokens"));
    }

    pub fn drain_instructions(&mut self) -> Vec<String> {
        let mut unhandled = Vec::new();
        if let Some(ref mut rx) = self.instruction_rx {
            while let Ok(msg) = rx.try_recv() {
                unhandled.push(msg);
            }
        }
        unhandled
    }

    /// Get the current conversation history.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Save current session to disk.
    fn save_session(&mut self) {
        // Nothing to save until the user has said something.
        if self.messages.len() <= 1 {
            return;
        }
        let root = self.settings.project_root.clone();
        let session = self.session.get_or_insert_with(|| {
            session::Session::new(
                Vec::new(),
                &self.settings.active_model,
                &self.settings.active_provider,
            )
        });
        session.messages = self.messages.clone();
        session.model = self.settings.active_model.clone();
        session.provider = self.settings.active_provider.clone();
        if let Err(e) = session.save(&root) {
            tracing::warn!("Failed to save session: {e}");
        }
    }

    /// Get the context window size for the current model.
    fn get_context_window(&self) -> usize {
        if self.settings.active_provider == "ollama" {
            return crate::ai::ollama::cached_model_info(&self.settings.active_model)
                .map(|m| m.context_window)
                .unwrap_or(8_192);
        }
        crate::config::providers::build_registry_with_settings(&self.settings)
            .iter()
            .flat_map(|p| p.models.iter())
            .find(|m| m.id == self.settings.active_model)
            .map(|m| m.context_window)
            .unwrap_or(128_000)
    }
}

/// How long to wait before retrying `error`, or `None` if it isn't transient.
fn retry_delay(error: &str, attempt: u32) -> Option<std::time::Duration> {
    let lower = error.to_lowercase();
    let transient = [
        "429",
        "500",
        "502",
        "503",
        "504",
        "529",
        "overloaded",
        "resource_exhausted",
        "rate limit",
        "timed out",
        "timeout",
        "connection reset",
        "connection closed",
        "error sending request",
        "stream error",
        "temporarily unavailable",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    if !transient
        || lower.contains("per day")
        || (lower.contains("billing") && !lower.contains("retry"))
    {
        return None;
    }
    // Honour the server's hint ("retry in 37.6s", "retryDelay": "37s").
    let hinted =
        regex::Regex::new(r#"retry(?:\s+in|delay"?\s*:\s*"?)\s*([0-9]+(?:\.[0-9]+)?)\s*s"#)
            .ok()
            .and_then(|re| re.captures(&lower))
            .and_then(|c| c[1].parse::<f64>().ok());
    let secs = match hinted {
        Some(s) if s > 90.0 => return None,
        Some(s) => s.ceil() as u64 + 1,
        None => 2u64 << attempt, // 2s, 4s, 8s
    };
    Some(std::time::Duration::from_secs(secs))
}

/// First meaningful line of a provider error, for status messages.
fn short_error(error: &str) -> String {
    let line = error
        .lines()
        .next()
        .unwrap_or(error)
        .trim_end_matches(['{', ' ', ':']);
    crate::util::truncate_chars(line, 80)
}

fn is_truncated(finish_reason: &str) -> bool {
    matches!(finish_reason, "length" | "max_tokens" | "MAX_TOKENS")
}

/// Tool arguments arrive either as a JSON object or as a JSON-encoded string.
/// `None` means the string was not valid JSON (e.g. cut off mid-stream).
fn parse_tool_args(arguments: &serde_json::Value) -> Option<serde_json::Value> {
    match arguments {
        serde_json::Value::String(raw) if raw.trim().is_empty() => Some(json!({})),
        serde_json::Value::String(raw) => serde_json::from_str(raw).ok(),
        serde_json::Value::Null => Some(json!({})),
        other => Some(other.clone()),
    }
}

fn build_system_prompt(settings: &Settings) -> String {
    let skills = crate::skills::load_all_skills(&settings.project_root);
    let mut skills_info = String::new();
    if !skills.is_empty() {
        skills_info.push_str("\nSPECIALIZED SKILLS (guidance is added automatically when relevant; read_skill shows the full text):\n");
        for skill in &skills {
            skills_info.push_str(&format!("- {}: {}\n", skill.name, skill.description));
        }
    }

    let project_ctx = history::ProjectContext::load(&settings.project_root)
        .unwrap_or_else(|_| history::ProjectContext::scan(&settings.project_root));
    let project_info = project_ctx.to_prompt_summary();
    let readme = load_readme_summary(&settings.project_root);
    let platform = if cfg!(windows) {
        "Windows (run_command uses cmd.exe for shell syntax; prefer cross-platform commands)"
    } else if cfg!(target_os = "macos") {
        "macOS (run_command uses sh)"
    } else {
        "Linux (run_command uses sh)"
    };

    let mut prompt = format!(
        "You are Norvexum, an AI coding agent working inside the user's project directory. You were created by Pratik (GitHub: Pratik-3706).\n\n\
         {project_info}\
         Project root: {root}\n\
         Platform: {platform}\n\
         {skills_info}\
         {readme}\n\
         GUIDELINES:\n\
         - Work autonomously: use tools to make changes and run commands instead of describing what you would do. Ask the user only when a decision is genuinely theirs.\n\
         - Think step by step before acting: reason through the problem before calling tools or answering.\n\
         - You can only access files inside the project directory.\n\
         - Prefer targeted edits (edit_file / multi_edit) over rewriting whole files. Read only the line ranges you need, and don't re-read a file you already have unless it changed.\n\
         - Don't repeat file contents in your reply after writing them — the user sees file changes in the UI. Summarize what changed instead.\n\
         - Keep replies concise.\n\
         - Web: web_search finds pages, web_fetch reads them; use browser_interact only when fetching is blocked.\n\
         - Check packages with check_package before installing. Python dependencies always go in a project-local virtual environment: create `.venv` if it's missing and call its executables directly (`.venv\\\\Scripts\\\\pip` on Windows, `.venv/bin/pip` elsewhere). Never run `pip install` globally.\n\
         - Long-running servers: start them with run_command and background=true, and stop them with stop_process using the PID it returned. Never kill processes by name (e.g. `taskkill /IM python.exe`).\n\
         - Images: generate_image creates new ones; image_search finds real-world photos and graphics; zerochan_search finds existing anime / gacha-game characters (comma-separated tags, e.g. 'Genshin Impact, Furina'). Save them where the user asks, otherwise in the current directory (media/ or static/ subfolders are good practice).\n\
         - When a page or app you built shows images, check them with view_image / batch_view_images (alignment, cropping, aspect ratio) and fix problems, e.g. with a Pillow script.\n\
         - Version control: git_status, git_diff, git_log, git_commit. Commit only when the user asks.\n\
         - Do not create NSFW content or images yourself, but do fully help build code for sites or apps that may host NSFW material; for such requests, output only code.\n",
        root = settings.project_root.display(),
    );

    if let Some(custom) = settings
        .custom_system_prompt
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        prompt.push_str("\nADDITIONAL INSTRUCTIONS FROM THE USER:\n");
        prompt.push_str(custom);
        prompt.push('\n');
    }
    prompt
}

fn load_readme_summary(root: &std::path::Path) -> String {
    for name in ["README.md", "readme.md", "README.txt", "README"] {
        if let Ok(content) = std::fs::read_to_string(root.join(name)) {
            let summary: String = content.chars().take(500).collect();
            let more = if content.chars().nth(500).is_some() {
                "..."
            } else {
                ""
            };
            return format!("\nPROJECT README (first 500 chars):\n{summary}{more}\n");
        }
    }
    String::new()
}

fn detect_images(user_input: &str, project_root: &std::path::Path) -> Vec<std::path::PathBuf> {
    const EXTS: &[&str] = &[".png", ".jpg", ".jpeg", ".webp", ".gif", ".bmp", ".avif"];
    let cleaned: String = user_input.replace(['(', ')', '[', ']', '`', '"', '\''], " ");
    let mut images = Vec::new();
    for word in cleaned.split_whitespace() {
        let word = word.trim_end_matches([',', '.', ';', ':', '!', '?']);
        let lower = word.to_lowercase();
        if !EXTS.iter().any(|ext| lower.ends_with(ext)) {
            continue;
        }
        let path = std::path::Path::new(word);
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            project_root.join(path)
        };
        if resolved.is_file() && !images.contains(&resolved) {
            images.push(resolved);
        }
    }
    images
}

fn tool_needs_approval(settings: &Settings, tool_name: &str) -> bool {
    settings.ask_perms && settings.needs_approval(crate::tools::canonical_tool_name(tool_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_follows_settings() {
        let mut settings = Settings {
            require_approval: vec!["run_command".to_string()],
            ..Settings::default()
        };
        assert!(tool_needs_approval(&settings, "run_command"));
        assert!(!tool_needs_approval(&settings, "read_file"));

        settings.require_approval.clear();
        assert!(!tool_needs_approval(&settings, "run_command"));

        settings.require_approval = vec!["run_command".to_string()];
        settings.ask_perms = false;
        assert!(!tool_needs_approval(&settings, "run_command"));
    }

    #[test]
    fn retry_policy() {
        let rate = "Gemini API error 429 Too Many Requests: Please retry in 37.628484036s.";
        assert_eq!(
            retry_delay(rate, 0),
            Some(std::time::Duration::from_secs(39))
        );
        assert_eq!(
            retry_delay("Anthropic API error 529: overloaded_error", 1),
            Some(std::time::Duration::from_secs(4))
        );
        assert_eq!(
            retry_delay("API error 401 Unauthorized: invalid key", 0),
            None
        );
        assert_eq!(
            retry_delay("429: quota exceeded for requests per day", 0),
            None
        );
        assert_eq!(retry_delay("429 Please retry in 3600s", 0), None);
    }

    #[test]
    fn tool_args_parsing_reports_truncated_json() {
        assert_eq!(parse_tool_args(&json!({"a": 1})), Some(json!({"a": 1})));
        assert_eq!(parse_tool_args(&json!("{\"a\": 1}")), Some(json!({"a": 1})));
        assert_eq!(parse_tool_args(&json!("")), Some(json!({})));
        assert_eq!(
            parse_tool_args(&json!("{\"path\": \"x.rs\", \"content\": \"fn ma")),
            None
        );
    }

    #[test]
    fn detects_images_with_trailing_punctuation() {
        let root = std::env::temp_dir().join(format!("nvx-img-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("shot.png"), b"x").unwrap();
        let found = detect_images("look at shot.png, please", &root);
        assert_eq!(found, vec![root.join("shot.png")]);
        std::fs::remove_dir_all(root).unwrap();
    }
}
