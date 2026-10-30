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
pub mod subagent;
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
    /// The agent started a sub-agent (spawn_agent).
    SubAgentStarted {
        id: String,
        name: String,
        task: String,
    },
    /// Something a sub-agent did; `event` is its own event, as the main agent's would be.
    SubAgent { id: String, event: Box<AgentEvent> },
    /// A sub-agent ended; `summary` starts its report (or says why it failed).
    SubAgentFinished {
        id: String,
        success: bool,
        summary: String,
    },
    /// One agent messaged another (message_agent). `from`/`to` are "main" or a sub-agent's name.
    AgentMessage {
        from: String,
        to: String,
        text: String,
    },
}

const FILE_WRITE_TOOLS: &[&str] = &["write_file", "edit_file", "multi_edit"];

/// Output budget for models whose limit isn't known. Providers that allow less
/// say so in their error, and the request is retried with their figure.
const DEFAULT_OUTPUT_TOKENS: u32 = 32_768;

/// Runaway guard for resuming one cut-off reply; each resume is a full output
/// budget, so this is never reached by a reply that is making progress.
const MAX_RESUMES: usize = 16;

const RESUME_PROMPT: &str = "Your previous reply was cut off before it was finished. Continue it from \
     exactly where it stopped, even mid-sentence, mid-word or inside a code block. Do not repeat \
     anything already written and do not add a preamble.";

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

impl ModelTurn {
    /// Append the response that resumed this cut-off one.
    fn absorb(&mut self, next: ModelTurn) {
        self.text.push_str(&next.text);
        self.tool_calls = next.tool_calls;
        self.thinking_blocks.extend(next.thinking_blocks);
        self.finish_reason = next.finish_reason;
        self.streamed_args.extend(next.streamed_args);
        self.error = next.error;
        self.cancelled = next.cancelled;
        self.thinking_chars += next.thinking_chars;
    }

    fn produced_output(&self) -> bool {
        !self.text.is_empty() || !self.tool_calls.is_empty()
    }
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
    /// Output limit a provider reported for a model, overriding the built-in one.
    learned_output_cap: Option<(String, u32)>,
    /// Set for a sub-agent: its label. Sub-agents can't start sub-agents.
    subagent_name: Option<String>,
    /// Messages other agents send this one (message_agent), read at each step.
    inbox_tx: mpsc::UnboundedSender<String>,
    inbox_rx: mpsc::UnboundedReceiver<String>,
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
        let (inbox_tx, inbox_rx) = mpsc::unbounded_channel();
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
            learned_output_cap: None,
            subagent_name: None,
            inbox_tx,
            inbox_rx,
        }
    }

    /// Turn this agent into a sub-agent called `name`: no spawn_agent tool,
    /// and a system prompt saying it works on a delegated task.
    pub(crate) fn make_subagent(&mut self, name: &str) {
        self.subagent_name = Some(name.to_string());
        self.tools = Arc::new(subagent_tools(ToolRegistry::new(&self.settings)));
        if let Some(system) = self.messages.first_mut().filter(|m| m.role == Role::System) {
            *system = Message::system(format!(
                "{}

# You are a sub-agent (\"{name}\")
Another agent gave you one task. Work on it independently with your tools; the user can't answer \
                 questions mid-task (if you are truly blocked, ask \"main\" with message_agent and keep going). \
                 When done, reply with a concise report: what you did, files changed, results, anything left open. \
                 Other agents may be working at the same time: message_agent reaches \"main\" \
                 (who started you) or another sub-agent by name, and their notes reach you as \
                 \"[Message from agent ...]\".",
                system.text()
            ));
        }
    }

    pub fn set_instruction_receiver(&mut self, rx: mpsc::UnboundedReceiver<String>) {
        self.instruction_rx = Some(rx);
    }

    /// Get the approval sender so the UI can respond to approval requests.
    pub fn approval_sender(&self) -> mpsc::UnboundedSender<bool> {
        self.approval_tx.clone()
    }

    /// Where other agents' messages for this one go.
    pub fn inbox_sender(&self) -> mpsc::UnboundedSender<String> {
        self.inbox_tx.clone()
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
        let mut tools = ToolRegistry::new(&settings);
        if self.subagent_name.is_some() {
            tools = subagent_tools(tools);
        }
        let tools = Arc::new(tools);
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
                        "  `/loops <num|unlimited>` Set the tool-loop limit for one request",
                        "  `/clear`                Clear the chat and conversation memory",
                        "  `/compact`              Summarize older messages to free context",
                        "  `/copy` / `/copy chat`  Copy the last answer / whole chat",
                        "  `/undo`                 Restore the files changed by the last edit",
                        "  `/session resume|list|load <id>|clear`",
                        "  `/provider [id]`        List providers or switch",
                        "  `/model [id]`           List models or switch",
                        "  `/exit`                 Quit",
                        "",
                        "**Terminal UI only**",
                        "  `/agents [N|name|main|split|stop N]`  Show or switch sub-agents",
                        "  `/msg <agent> <text>`    Send a note to a running sub-agent",
                        "  `/copy code`             Copy the last code block",
                        "  `/keys`                  Keyboard shortcuts (also F1)",
                        "",
                        "💡 *Ask for sub-agents (\"use sub-agents to …\") to split a job between agents that work at the same time.*",
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
            "/loops" => match arg.map(crate::config::parse_loop_limit) {
                Some(Some(num)) => {
                    self.max_loops = num;
                    self.reply(format!(
                        "🔄 Tool-loop limit set to {}.",
                        crate::config::describe_loop_limit(num)
                    ));
                }
                Some(None) => {
                    let _ = self.event_tx.send(AgentEvent::Error(
                        "Usage: `/loops <num>` or `/loops unlimited`".into(),
                    ));
                }
                None => self.reply(format!(
                    "Current tool-loop limit: {}\nUsage: `/loops <num>` or `/loops unlimited`",
                    crate::config::describe_loop_limit(self.max_loops)
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
        images.extend(explicit_images);
        // The same file can be found both ways (typed path and attachment).
        let mut seen = std::collections::HashSet::new();
        images.retain(|img| seen.insert(img.canonicalize().unwrap_or_else(|_| img.clone())));
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
        let (attachments, mut text) = split_attachments(user_input);
        // Only real images are sent as pictures; anything else would be
        // rejected by the provider as a broken image.
        let explicit_images: Vec<_> = attachments
            .iter()
            .filter(|p| is_image_path(p))
            .cloned()
            .collect();

        if text.starts_with('/') && self.handle_slash_command(text).await {
            return Ok(self.drain_instructions());
        }

        // Tell the model where the files are so its tools can use them.
        let with_note;
        if !attachments.is_empty() {
            with_note = format!(
                "{text}\n\n{}",
                attachment_notes(&attachments, &self.settings.project_root, true)
            );
            text = &with_note;
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

            // Gateways cap the request body; screenshots pile up within a turn.
            compaction::fit_request_size(&mut active_messages);
            let request = self.build_request(active_messages);
            let turn = self.stream_full_reply(request, cancel_rx, &mut usage).await;

            if turn.cancelled {
                if !turn.text.is_empty() {
                    self.messages.push(Message::assistant(&turn.text));
                }
                self.status("⏹️ Run stopped");
                self.stop_subagents();
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
                // The main agent answers only once its sub-agents are done:
                // wait for them, then have it summarize everyone's work.
                if self.subagent_name.is_none() && subagent::outstanding(&self.inbox_tx) {
                    if !self.wait_for_subagents(cancel_rx).await {
                        self.status("⏹️ Run stopped");
                        self.save_session();
                        self.done(Some(usage));
                        return Ok(self.drain_instructions());
                    }
                    loop_num += 1;
                    continue;
                }
                if is_truncated(&turn.finish_reason) {
                    let _ = self.event_tx.send(AgentEvent::Warning(
                        "The reply hit the output token limit and may be cut off — type 'continue' to get the rest."
                            .into(),
                    ));
                } else if turn.finish_reason == FINISH_INCOMPLETE && !turn.text.is_empty() {
                    let _ = self.event_tx.send(AgentEvent::Warning(
                        "The connection dropped before the reply finished — type 'continue' to get the rest."
                            .into(),
                    ));
                }
                self.save_session();
                self.done(Some(usage));
                return Ok(self.drain_instructions());
            }

            if let Some(cut) = turn
                .tool_calls
                .iter()
                .find(|tc| parse_tool_args(&tc.arguments).is_none())
            {
                let why = if is_truncated(&turn.finish_reason) {
                    "the reply hit the output token limit"
                } else {
                    "its arguments were not valid JSON"
                };
                let _ = self.event_tx.send(AgentEvent::Warning(format!(
                    "The `{}` call was cut off ({why}); asking the model to redo it in smaller parts.",
                    crate::tools::canonical_tool_name(&cut.name)
                )));
            }

            // Tool APIs require the assistant call message immediately before results.
            let calls = turn.tool_calls;
            let mut assistant = Message::assistant_with_tool_calls(&turn.text, calls.clone());
            assistant.thinking_blocks = turn.thinking_blocks;
            self.messages.push(assistant);

            let Some(approvals) = self.collect_approvals(&calls, cancel_rx).await else {
                self.finish_cancelled_tools(&calls, "Cancelled by the user before running.");
                self.stop_subagents();
                self.done(Some(usage));
                return Ok(self.drain_instructions());
            };

            let Some(results) = self.execute_tools(&calls, &approvals, cancel_rx).await else {
                self.finish_cancelled_tools(&calls, "Cancelled by the user while running.");
                self.stop_subagents();
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

    /// Output budget for a request over `messages`: the model's whole output
    /// limit, trimmed so prompt + reply still fit in the context window (some
    /// APIs reject requests whose sum overflows it).
    fn output_budget(&self, messages: &[Message]) -> u32 {
        let model = &self.settings.active_model;
        let cap = match &self.learned_output_cap {
            Some((m, cap)) if m == model => *cap,
            _ => ai::max_output_tokens(model).unwrap_or(DEFAULT_OUTPUT_TOKENS),
        };
        let window = self.get_context_window();
        // Small or missing windows in model listings are usually wrong.
        if window < 32_000 {
            return cap;
        }
        // The estimate runs low on code; leave slack for it and the tool schemas.
        let estimate = compaction::estimate_tokens(messages);
        let prompt = estimate + estimate / 8 + 6_000;
        let room = window.saturating_sub(prompt).max(8_192);
        cap.min(u32::try_from(room).unwrap_or(u32::MAX))
    }

    fn build_request(&self, messages: Vec<Message>) -> AiRequest {
        let max_tokens = self.output_budget(&messages);
        let mut request = AiRequest::new(messages)
            .with_tools(self.tools.tool_defs())
            .with_max_tokens(max_tokens);
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
            max_tokens = ?request.max_tokens,
            "model request"
        );
        request.with_temperature(temperature)
    }

    /// One complete reply. A reply cut off by the output limit or a dropped
    /// connection is resumed in follow-up requests and stitched back together,
    /// so it reaches the user and the history as one uninterrupted answer.
    async fn stream_full_reply(
        &mut self,
        request: AiRequest,
        cancel_rx: &mut mpsc::UnboundedReceiver<()>,
        usage: &mut UsageStats,
    ) -> ModelTurn {
        let mut turn = self
            .stream_with_retry(request.clone(), cancel_rx, usage)
            .await;
        let mut resumes = 0;
        // A cut-off tool call can't be resumed; its error result asks the model
        // to redo it in smaller parts instead.
        while resumes < MAX_RESUMES
            && !turn.cancelled
            && turn.error.is_none()
            && turn.tool_calls.is_empty()
            && !turn.text.trim().is_empty()
        {
            let why = if is_truncated(&turn.finish_reason) {
                "output limit"
            } else if turn.finish_reason == FINISH_INCOMPLETE {
                "connection dropped"
            } else {
                break;
            };
            resumes += 1;
            // Not a "Thinking (step …)" status: the UIs keep the same bubble.
            self.status(format!("Reply was cut off ({why}), continuing it"));

            let mut resume = request.clone();
            let mut partial = Message::assistant(&turn.text);
            partial.thinking_blocks = turn.thinking_blocks.clone();
            resume.messages.push(partial);
            resume.messages.push(Message::user(RESUME_PROMPT));
            resume.max_tokens = Some(self.output_budget(&resume.messages));

            let next = self.stream_with_retry(resume, cancel_rx, usage).await;
            if let Some(err) = next.error.as_deref().filter(|_| !next.produced_output()) {
                // Keep what already arrived; the caller warns that it is incomplete.
                self.status(format!("Couldn't continue the reply: {}", short_error(err)));
                break;
            }
            let progressed = next.produced_output();
            turn.absorb(next);
            if !progressed {
                break;
            }
        }
        turn
    }

    /// `stream_model_turn`, retrying transient failures (rate limits, overload,
    /// dropped connections) with backoff before anything was streamed, and
    /// shrinking the output budget when the provider says it is too large.
    async fn stream_with_retry(
        &mut self,
        mut request: AiRequest,
        cancel_rx: &mut mpsc::UnboundedReceiver<()>,
        usage: &mut UsageStats,
    ) -> ModelTurn {
        const MAX_RETRIES: u32 = 3;
        let mut attempt = 0;
        let mut budget_retries = 0;
        loop {
            let mut turn = self
                .stream_model_turn(request.clone(), cancel_rx, usage)
                .await;
            let Some(err) = turn.error.clone() else {
                return turn;
            };
            if turn.produced_output() {
                // A connection that drops mid-reply keeps what arrived;
                // `stream_full_reply` picks up from there.
                if retry_delay(&err, 0).is_some() {
                    tracing::warn!("Stream interrupted mid-reply: {err}");
                    turn.error = None;
                    turn.finish_reason = FINISH_INCOMPLETE.to_string();
                }
                return turn;
            }
            if let Some(requested) = request.max_tokens.filter(|_| budget_retries < 3) {
                if let Some(limit) = output_limit_from_error(&err, requested) {
                    budget_retries += 1;
                    tracing::info!(
                        requested,
                        limit = limit.tokens,
                        "provider lowered the output budget"
                    );
                    if limit.model_wide {
                        self.learned_output_cap =
                            Some((self.settings.active_model.clone(), limit.tokens));
                    }
                    request.max_tokens = Some(limit.tokens);
                    continue;
                }
            }
            let delay = if attempt < MAX_RETRIES {
                retry_delay(&err, attempt)
            } else {
                None
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
            // Unparseable arguments never execute (the model gets an error back),
            // so there is nothing meaningful to approve.
            let Some(args) = parse_tool_args(&tc.arguments) else {
                approvals.push(true);
                continue;
            };
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
            events: Some(self.event_tx.clone()),
            approvals_off: self.approval_disabled,
            agent_name: self.subagent_name.clone(),
            inbox: Some(self.inbox_tx.clone()),
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
        let max_chars = self.settings.max_content_chars.clamp(
            crate::config::MIN_CONTENT_CHARS,
            crate::config::MAX_CONTENT_CHARS,
        );
        let content = crate::util::truncate_middle(&result.to_message_content(), max_chars);
        let mut msg = Message::tool_result(&call.id, &call.name, content);
        if let Some((data, mime)) = result.base64_image {
            msg = msg.with_image(data, mime);
        }
        msg.thought_signature = call.thought_signature.clone();
        msg
    }

    /// Fold instructions typed during a run into the conversation.
    /// Stopping the main agent stops the sub-agents it started.
    fn stop_subagents(&self) {
        if self.subagent_name.is_none() {
            subagent::cancel_owned(&self.inbox_tx);
        }
    }

    /// Wait for running sub-agents, then hand their reports to the main agent
    /// with a request to summarize. False when the user stopped the run.
    async fn wait_for_subagents(&mut self, cancel_rx: &mut mpsc::UnboundedReceiver<()>) -> bool {
        let running = subagent::running_count(&self.inbox_tx);
        if running > 0 {
            self.status(format!(
                "Waiting for {running} sub-agent{} to finish...",
                if running == 1 { "" } else { "s" }
            ));
            tokio::select! {
                _ = subagent::wait_until_done(&self.inbox_tx, None) => {}
                _ = cancel_rx.recv() => {
                    subagent::cancel_owned(&self.inbox_tx);
                    return false;
                }
            }
        }
        let notes: Vec<String> = subagent::take_reports(&self.inbox_tx)
            .iter()
            .map(subagent::Report::note)
            .collect();
        let text = format!(
            "{}\n\n[All sub-agents have finished. Now answer the user: summarize what each agent did, \
             the files changed, the combined result and anything left open.]",
            notes.join("\n\n")
        );
        match self.messages.last_mut() {
            Some(last) if last.role == Role::User => last.content.push(ContentPart::Text {
                text: format!("\n\n{text}"),
            }),
            _ => self.messages.push(Message::user(text)),
        }
        true
    }

    fn absorb_instructions(&mut self) {
        let mut incoming = Vec::new();
        if let Some(rx) = self.instruction_rx.as_mut() {
            while let Ok(text) = rx.try_recv() {
                incoming.push(text);
            }
        }
        // Sub-agents that finished while the main agent kept working.
        if self.subagent_name.is_none() {
            incoming.extend(
                subagent::take_reports(&self.inbox_tx)
                    .iter()
                    .map(subagent::Report::note),
            );
        }
        // Messages from other agents read like instructions.
        while let Ok(text) = self.inbox_rx.try_recv() {
            incoming.push(text);
        }
        for raw in incoming {
            // Files attached to a queued message arrive in the GUI protocol too.
            let (attachments, prompt) = split_attachments(&raw);
            let mut parts = Vec::new();
            if self.client.supports_vision() {
                for img in attachments.iter().filter(|p| is_image_path(p)) {
                    if let Ok((data, mime_type)) = vision::encode_image_file(img) {
                        parts.push(ContentPart::Image { data, mime_type });
                    }
                }
            }
            let mut text = prompt.to_string();
            if !attachments.is_empty() {
                let inline = !parts.is_empty();
                text = format!(
                    "{text}\n\n{}",
                    attachment_notes(&attachments, &self.settings.project_root, inline)
                );
            }
            self.status(format!(
                "📥 Received instruction: {}",
                crate::util::truncate_chars(prompt, 80)
            ));
            match self.messages.last_mut() {
                Some(last) if last.role == Role::User => {
                    last.content.push(ContentPart::Text {
                        text: format!("\n\n{text}"),
                    });
                    last.content.extend(parts);
                }
                _ => {
                    let mut msg = Message::user(text);
                    msg.content.extend(parts);
                    self.messages.push(msg);
                }
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
        while let Ok(msg) = self.inbox_rx.try_recv() {
            unhandled.push(msg);
        }
        unhandled
    }

    /// Get the current conversation history.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Save current session to disk.
    fn save_session(&mut self) {
        // Nothing to save until the user has said something. Sub-agents live
        // inside the main agent's chat and never get a history entry of their own.
        if self.messages.len() <= 1 || self.subagent_name.is_some() {
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

/// An output budget a provider will accept, read from its rejection.
#[derive(Debug, PartialEq)]
struct OutputLimit {
    tokens: u32,
    /// The model's own ceiling (worth remembering), rather than what happened
    /// to fit beside this particular prompt.
    model_wide: bool,
}

/// When `error` rejects the requested output budget (`requested` tokens), the
/// budget to retry with. Covers the shapes providers use:
///   "max_tokens: 128000 > 64000, which is the maximum allowed …" (Anthropic)
///   "input length and `max_tokens` exceed context limit: 180000 + 64000 > 200000"
///   "This model supports at most 16384 completion tokens" (OpenAI)
///   "maximum context length is 200000 tokens … (166000 of text input, …)"
///   "maximum context length is 128000 tokens … (76000 in the messages, …)"
fn output_limit_from_error(error: &str, requested: u32) -> Option<OutputLimit> {
    let lower = error.to_lowercase();
    let about_output = [
        "max_tokens",
        "max_completion_tokens",
        "max_output_tokens",
        "maxoutputtokens",
        "max tokens",
        "output tokens",
        "completion tokens",
    ]
    .iter()
    .any(|k| lower.contains(k))
        || (lower.contains("context length")
            && (lower.contains("output") || lower.contains("completion")));
    // Rate limits also quote token counts ("8,000 output tokens per minute").
    let rate_limited = ["rate limit", "429", "per minute", "per day", "quota"]
        .iter()
        .any(|k| lower.contains(k));
    if !about_output || rate_limited {
        return None;
    }

    let plain = lower.replace(',', "");
    let num = |re: &str| -> Option<Vec<u64>> {
        let caps = regex::Regex::new(re).ok()?.captures(&plain)?;
        caps.iter()
            .skip(1)
            .map(|m| m?.as_str().parse().ok())
            .collect()
    };
    let fit = |window: u64, input: u64| {
        let room = window.saturating_sub(input + 1_024);
        (room >= 1_024).then(|| OutputLimit {
            tokens: u32::try_from(room).unwrap_or(u32::MAX).min(requested - 1),
            model_wide: false,
        })
    };

    if let Some(n) = num(r"(\d+)\s*\+\s*\d+\s*>\s*(\d+)") {
        return fit(n[1], n[0]);
    }
    if let (Some(window), Some(input)) = (
        num(r"context length is (\d+)"),
        num(r"(\d+) (?:tokens )?(?:of text input|in the messages)"),
    ) {
        return fit(window[0], input[0]);
    }
    // Otherwise the largest figure below what was asked is the ceiling.
    let ceiling = regex::Regex::new(r"\d+")
        .ok()?
        .find_iter(&plain)
        .filter_map(|m| m.as_str().parse::<u64>().ok())
        .filter(|&n| n >= 1_024 && n < u64::from(requested))
        .max();
    match ceiling {
        Some(n) => Some(OutputLimit {
            tokens: n as u32,
            model_wide: true,
        }),
        // No figure to go on: halve, down to the old fixed 8k.
        None => (requested > 8_192).then(|| OutputLimit {
            tokens: (requested / 2).max(8_192),
            model_wide: false,
        }),
    }
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

/// A sub-agent's tools: everything except starting or waiting for sub-agents.
fn subagent_tools(tools: ToolRegistry) -> ToolRegistry {
    tools
        .without(subagent::TOOL_NAME)
        .without(subagent::WAIT_TOOL_NAME)
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

const IMAGE_EXTS: &[&str] = &[".png", ".jpg", ".jpeg", ".webp", ".gif", ".bmp", ".avif"];

fn is_image_path(path: &std::path::Path) -> bool {
    let lower = path.to_string_lossy().to_lowercase();
    IMAGE_EXTS.iter().any(|ext| lower.ends_with(ext))
}

/// Split the GUI attachment protocol, "/image_prompt |<path>|<path>…| <prompt>",
/// into the attached files and the prompt. The GUI has already placed each
/// file inside the project (in place, or copied to .norvexum/attachments/) and
/// sends absolute paths; the first segment is always a path (older format).
fn split_attachments(text: &str) -> (Vec<std::path::PathBuf>, &str) {
    let Some(mut rest) = text.strip_prefix("/image_prompt |") else {
        return (Vec::new(), text);
    };
    let mut files = Vec::new();
    let mut first = true;
    while let Some((segment, tail)) = rest.split_once('|') {
        let path = std::path::Path::new(segment);
        if !first && !path.is_absolute() {
            break;
        }
        first = false;
        if path.is_file() {
            files.push(path.to_path_buf());
        }
        rest = tail;
    }
    (files, rest.trim())
}

fn attachment_notes(
    paths: &[std::path::PathBuf],
    project_root: &std::path::Path,
    images_inline: bool,
) -> String {
    paths
        .iter()
        .map(|p| attachment_note(p, project_root, images_inline))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The note added to a prompt for a GUI attachment: where the file is (relative
/// to the project), how big it is, and how the agent can use it.
/// `image_inline` says whether an image also went along as a picture.
fn attachment_note(
    path: &std::path::Path,
    project_root: &std::path::Path,
    image_inline: bool,
) -> String {
    let root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let full = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let shown = full
        .strip_prefix(&root)
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().to_string());
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let size = if size >= 1024 * 1024 {
        format!("{:.1} MB", size as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} KB", size as f64 / 1024.0)
    };
    let how = if is_image_path(path) && image_inline {
        "The image is attached to this message; view_image shows it again"
    } else if is_image_path(path) {
        "Look at it with view_image"
    } else {
        "Open it with read_file"
    };
    format!(
        "[Attached file: `{shown}` ({size}). {how}. If it belongs in the project \
         (e.g. an asset the code should use), copy it to the right place with cp \
         and reference the new path.]"
    )
}

fn detect_images(user_input: &str, project_root: &std::path::Path) -> Vec<std::path::PathBuf> {
    const EXTS: &[&str] = IMAGE_EXTS;
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
    fn attachment_note_gives_a_project_relative_path() {
        let root = std::env::temp_dir().join(format!("norvexum_attach_{}", std::process::id()));
        let dir = root.join(".norvexum").join("attachments");
        std::fs::create_dir_all(&dir).unwrap();
        let pdf = dir.join("spec.pdf");
        std::fs::write(&pdf, vec![0u8; 2048]).unwrap();
        let png = root.join("logo.png");
        std::fs::write(&png, b"x").unwrap();

        let note = attachment_note(&pdf, &root, true);
        assert!(note.contains("`.norvexum/attachments/spec.pdf`"), "{note}");
        assert!(note.contains("2.0 KB"), "{note}");
        assert!(note.contains("read_file") && note.contains("cp"), "{note}");
        assert!(!is_image_path(&pdf));

        let note = attachment_note(&png, &root, true);
        assert!(
            note.contains("`logo.png`") && note.contains("view_image"),
            "{note}"
        );
        assert!(is_image_path(std::path::Path::new("A.JPG")));

        // Several files in one message, prompt text containing '|' kept whole.
        let msg = format!(
            "/image_prompt |{}|{}| compare | contrast",
            pdf.display(),
            png.display()
        );
        let (files, prompt) = split_attachments(&msg);
        assert_eq!(files, vec![pdf.clone(), png.clone()]);
        assert_eq!(prompt, "compare | contrast");
        // The older single-file form still parses.
        let single = format!("/image_prompt |{}| hi", png.display());
        let (files, prompt) = split_attachments(&single);
        assert_eq!((files, prompt), (vec![png.clone()], "hi"));
        let (files, prompt) = split_attachments("plain | text");
        assert!(files.is_empty());
        assert_eq!(prompt, "plain | text");
        std::fs::remove_dir_all(&root).unwrap();
    }

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
    fn output_budget_follows_provider_rejections() {
        let anthropic = "Anthropic API error 400: max_tokens: 128000 > 64000, which is the maximum \
                         allowed number of output tokens for claude-opus-5-5";
        assert_eq!(
            output_limit_from_error(anthropic, 128_000),
            Some(OutputLimit {
                tokens: 64_000,
                model_wide: true
            })
        );
        let overflow = "input length and `max_tokens` exceed context limit: 180000 + 64000 > 200000, \
                        decrease input length or `max_tokens` and try again";
        assert_eq!(
            output_limit_from_error(overflow, 64_000),
            Some(OutputLimit {
                tokens: 18_976,
                model_wide: false
            })
        );
        let openai = "max_tokens is too large: 32768. This model supports at most 16,384 completion \
                      tokens, whereas you provided 32768.";
        assert_eq!(
            output_limit_from_error(openai, 32_768).unwrap().tokens,
            16_384
        );
        let gateway = "This endpoint's maximum context length is 200000 tokens. However, you requested \
                       about 230000 tokens (166000 of text input, 64000 in the output).";
        assert_eq!(
            output_limit_from_error(gateway, 64_000).unwrap().tokens,
            32_976
        );
        let deepseek = "Invalid max_tokens value, the valid range of max_tokens is [1, 8192]";
        assert_eq!(
            output_limit_from_error(deepseek, 32_768).unwrap().tokens,
            8_192
        );

        // Unrelated failures keep their normal handling.
        let rate = "429: This request would exceed your limit of 8,000 output tokens per minute";
        assert_eq!(output_limit_from_error(rate, 64_000), None);
        assert_eq!(
            output_limit_from_error("401 invalid x-api-key", 64_000),
            None
        );
        assert_eq!(
            output_limit_from_error("prompt is too long: 210000 tokens > 200000 maximum", 64_000),
            None
        );
    }

    /// Plays back one scripted response per request and records the requests.
    struct ScriptedClient {
        replies: std::sync::Mutex<std::collections::VecDeque<Vec<AiStreamEvent>>>,
        requests: Arc<std::sync::Mutex<Vec<AiRequest>>>,
    }

    #[async_trait::async_trait]
    impl AiClient for ScriptedClient {
        async fn chat_stream(
            &self,
            request: AiRequest,
            tx: mpsc::UnboundedSender<AiStreamEvent>,
        ) -> Result<()> {
            self.requests.lock().unwrap().push(request);
            let events = self.replies.lock().unwrap().pop_front().unwrap_or_default();
            for event in events {
                let _ = tx.send(event);
            }
            Ok(())
        }

        fn supports_vision(&self) -> bool {
            false
        }
    }

    fn done(reason: &str) -> AiStreamEvent {
        AiStreamEvent::Done {
            finish_reason: reason.into(),
            usage: None,
        }
    }

    /// Run one user message against scripted replies; returns the agent, the
    /// requests it sent and the warnings it showed.
    async fn run_scripted(
        replies: Vec<Vec<AiStreamEvent>>,
    ) -> (Agent, Vec<AiRequest>, Vec<String>) {
        let root = std::env::temp_dir().join(format!("nvx-resume-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let settings = Settings {
            project_root: root.clone(),
            ..Settings::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut agent = Agent::new(settings, tx);
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        agent.client = Arc::new(ScriptedClient {
            replies: std::sync::Mutex::new(replies.into()),
            requests: requests.clone(),
        });
        let (_cancel_tx, mut cancel_rx) = mpsc::unbounded_channel();
        agent
            .process_message("write it", &mut cancel_rx)
            .await
            .unwrap();

        let mut warnings = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::Warning(w) = event {
                warnings.push(w);
            }
        }
        std::fs::remove_dir_all(&root).ok();
        let requests = requests.lock().unwrap().clone();
        (agent, requests, warnings)
    }

    fn last_reply(agent: &Agent) -> String {
        let last = agent.messages.last().unwrap();
        assert_eq!(last.role, Role::Assistant);
        last.text()
    }

    #[tokio::test]
    async fn a_reply_cut_off_by_the_output_limit_is_resumed_seamlessly() {
        let (agent, requests, warnings) = run_scripted(vec![
            vec![
                AiStreamEvent::ContentDelta("The quick brown fo".into()),
                done("length"),
            ],
            vec![AiStreamEvent::ContentDelta("x jumps.".into()), done("stop")],
        ])
        .await;

        assert_eq!(last_reply(&agent), "The quick brown fox jumps.");
        assert_eq!(requests.len(), 2);
        assert!(requests[0].max_tokens.unwrap() > 8_192);
        // The resume request carries the partial reply and the instruction, but
        // neither ends up in the saved history.
        let resume = &requests[1].messages;
        assert_eq!(resume[resume.len() - 2].text(), "The quick brown fo");
        assert_eq!(resume[resume.len() - 1].text(), RESUME_PROMPT);
        assert!(!agent.messages.iter().any(|m| m.text() == RESUME_PROMPT));
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[tokio::test]
    async fn a_dropped_connection_keeps_the_text_and_continues() {
        let (agent, requests, warnings) = run_scripted(vec![
            vec![
                AiStreamEvent::ContentDelta("Step one, ".into()),
                AiStreamEvent::Error("Stream error: error decoding response body".into()),
                done(FINISH_INCOMPLETE),
            ],
            vec![
                AiStreamEvent::ContentDelta("step two.".into()),
                done("stop"),
            ],
        ])
        .await;

        assert_eq!(last_reply(&agent), "Step one, step two.");
        assert_eq!(requests.len(), 2);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[tokio::test]
    async fn a_reply_that_stops_making_progress_is_left_with_a_warning() {
        let (agent, requests, warnings) = run_scripted(vec![
            vec![
                AiStreamEvent::ContentDelta("Partial".into()),
                done("length"),
            ],
            vec![done("length")],
        ])
        .await;

        assert_eq!(last_reply(&agent), "Partial");
        assert_eq!(requests.len(), 2);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    #[tokio::test]
    async fn an_oversized_output_budget_is_lowered_and_remembered() {
        let (agent, requests, _) = run_scripted(vec![
            vec![AiStreamEvent::Error(
                "API error 400: max_tokens is too large. This model supports at most 12000 \
                 completion tokens."
                    .into(),
            )],
            vec![AiStreamEvent::ContentDelta("ok".into()), done("stop")],
        ])
        .await;

        assert_eq!(last_reply(&agent), "ok");
        assert_eq!(requests[1].max_tokens, Some(12_000));
        assert_eq!(agent.output_budget(&agent.messages), 12_000);
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
