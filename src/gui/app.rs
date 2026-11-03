use crate::agent::AgentEvent;
use crate::config::providers::build_registry;
use crate::config::{MAX_CONTENT_CHARS, MIN_CONTENT_CHARS, Settings};
use crate::style::{self, Palette};
use notify::{RecursiveMode, Result as NotifyResult, Watcher};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

mod activity;
mod agents;
mod browser_view;
mod chat;
mod drawer;
mod mcp_view;
mod shell;
mod terminal;

use agents::{ChatView, SubAgentView};

const LAST_LAYER_BYTES: &[u8] = include_bytes!("../../assets/loading_animation/last_layer.png");
const MIDDLE_LAYER_BYTES: &[u8] = include_bytes!("../../assets/loading_animation/middle_layer.png");
const TOP_LAYER_BYTES: &[u8] = include_bytes!("../../assets/loading_animation/top_layer.png");
const LOGO_BYTES: &[u8] = include_bytes!("../../assets/cheap_logo.png");

pub struct GuiConfig {
    pub settings: Settings,
    pub initial_message: Option<String>,
    pub agent_rx: mpsc::UnboundedReceiver<AgentEvent>,
    pub user_tx: mpsc::UnboundedSender<String>,
    pub inst_tx: mpsc::UnboundedSender<String>,
    pub cancel_tx: mpsc::UnboundedSender<()>,
    pub approval_tx: mpsc::UnboundedSender<bool>,
}

#[derive(Clone)]
pub struct GuiMessage {
    pub role: String, // "user", "assistant", "status", "error"
    pub content: String,
    pub thinking: Option<String>,
    pub is_thinking_done: bool,
    pub tool_logs: Vec<String>,
    pub attachments: Vec<std::path::PathBuf>,
}

pub struct GuiApp {
    settings: Settings,
    agent_rx: mpsc::UnboundedReceiver<AgentEvent>,
    user_tx: mpsc::UnboundedSender<String>,
    inst_tx: mpsc::UnboundedSender<String>,
    cancel_tx: mpsc::UnboundedSender<()>,
    approval_tx: mpsc::UnboundedSender<bool>,
    cancel_token: CancellationToken,

    // UI state variables
    input_text: String,
    model_search_query: String,
    messages: Vec<GuiMessage>,
    status_text: String,
    is_processing: bool,
    agent_crashed: bool,
    dark_mode: bool,

    // Secure key fields (held in memory only when typing, then sent to keyring)
    key_inputs: KeyInputs,

    // File watcher state
    watcher: Option<notify::RecommendedWatcher>,
    file_rx: Option<std_mpsc::Receiver<String>>,
    workspace_files: Vec<String>,
    folder_input: String,
    folder_error: Option<String>,

    // Approval modal / card state
    approval_pending: Option<crate::ui::ApprovalInfo>,

    // Textures
    last_layer_tex: egui::TextureHandle,
    middle_layer_tex: egui::TextureHandle,
    top_layer_tex: egui::TextureHandle,
    logo_tex: egui::TextureHandle,

    // Settings & Custom Skills Fields
    show_settings_modal: bool,
    custom_skill_name: String,
    custom_skill_desc: String,
    custom_skill_triggers: String,
    custom_skill_instructions: String,
    skills_list: Vec<crate::skills::Skill>,
    new_model_provider: String,
    new_model_id: String,

    // Collapsible sidebars open flags
    left_sidebar_open: bool,
    right_sidebar_open: bool,

    // Image attachment, writing files tracking, and history sessions
    /// Files waiting to go out with the next message.
    attachments: Vec<std::path::PathBuf>,
    session_written_files: Vec<String>,
    current_writing_file: Option<(String, usize, usize)>,
    current_writing_status: Option<String>,
    /// Persisted diff stats per file: filename → (total_added, total_deleted)
    file_diff_stats: std::collections::HashMap<String, (usize, usize)>,
    current_tool_name: Option<String>,
    current_tool_args: String,
    current_tool_id: Option<String>,
    history_sessions: Vec<crate::agent::session::SessionSummary>,
    current_session_id: Option<String>,
    active_messages_backup: Option<Vec<GuiMessage>>,
    selected_file: Option<String>,
    inspected_file_content: String,
    inspector_error: Option<String>,
    workspace_files_rx: Option<std_mpsc::Receiver<Vec<String>>>,
    is_loading_files: bool,

    // Debounce state for the filesystem watcher. When the agent writes a
    // burst of files in quick succession, notify fires dozens of events per
    // second — without debouncing I'd relaunch `list_workspace_files` every
    // frame, which is what was making the app appear to freeze for a few
    // seconds after each batch of file writes.
    last_fs_event: Option<std::time::Instant>,
    pending_fs_reload: bool,

    line_range_enabled: bool,
    line_range_start: usize,
    line_range_end: usize,
    line_range_total_lines: usize,
    manual_edits_enabled: bool,
    inspected_file_edited: bool,
    thinking_start_time: Option<std::time::Instant>,
    last_scrolled_range: Option<(usize, usize)>,
    should_scroll_to_line: Option<usize>,
    agent_active_session_id: Option<String>,
    left_sidebar_width: f32,
    right_sidebar_width: f32,
    ctx: Option<egui::Context>,
    last_active_provider: String,
    last_active_model: String,

    // Terminal panel
    show_terminal_panel: bool,

    // Dynamic model capability probing
    model_reasoning_type: Option<crate::ai::types::ReasoningType>,
    probed_capabilities: std::collections::HashMap<String, Option<crate::ai::types::ReasoningType>>,
    probing_in_progress: bool,
    capabilities_rx: Option<std_mpsc::Receiver<CapabilitiesUpdate>>,
    capabilities_tx: Option<std_mpsc::Sender<CapabilitiesUpdate>>,

    // Presentation-only state
    applied_theme: Option<bool>,
    right_tab: RightTab,
    file_filter: String,
    show_shortcuts: bool,
    focus_composer: bool,
    scroll_to_bottom: bool,
    chat_max_offset: f32,
    settings_section: SettingsSection,
    pending_delete_session: Option<String>,
    expanded_dirs: std::collections::HashSet<String>,
    file_tree: FileTree,
    file_tree_filter: String,
    file_tree_dirty: bool,
    /// The paste key was down last frame (edge detection for media paste).
    paste_key_was_down: bool,
    /// Sub-agents of the current chat, oldest first.
    subagents: Vec<SubAgentView>,
    /// What the chat area shows, and whether a sub-agent sits beside the main chat.
    chat_view: ChatView,
    split_view: bool,
    /// The sub-agent that asked for the pending approval (None: the main agent).
    approval_agent: Option<String>,
    /// Browser tab: the uploaded page picture (frame number, texture) and the address box.
    browser_tex: Option<(u64, egui::TextureHandle)>,
    /// Window size last frame (a change forces a redraw).
    last_window_size: Option<egui::Vec2>,
    /// Maximized / fullscreen from last time, applied once the window shows.
    startup_window_state: Option<(bool, bool)>,
    browser_url: String,
    browser_url_editing: bool,
    /// The redesigned shell's own state (palette, toasts, tool-call details…).
    shell: shell::ShellState,
}

struct CapabilitiesUpdate {
    provider: String,
    model: String,
    reasoning_type: Option<crate::ai::types::ReasoningType>,
}

#[derive(Default)]
struct KeyInputs {
    google: String,
    openai: String,
    anthropic: String,
    tavily: String,
    aicredits: String,
    ocr: String,
    pollinations: String,
}

fn bytes_to_texture(ctx: &egui::Context, name: &str, bytes: &[u8]) -> egui::TextureHandle {
    let image = image::load_from_memory(bytes).unwrap().to_rgba8();
    let size = [image.width() as _, image.height() as _];
    let pixels = image.into_raw();
    let color_image = egui::ColorImage::from_rgba_unmultiplied(size, &pixels);
    ctx.load_texture(name, color_image, Default::default())
}

fn paint_rotated_texture(
    painter: &egui::Painter,
    texture: &egui::TextureHandle,
    rect: egui::Rect,
    angle: f32,
    opacity: u8,
) {
    let center = rect.center();
    let rotate = |point: egui::Pos2| {
        let offset = point - center;
        let (sin, cos) = angle.sin_cos();
        center
            + egui::vec2(
                offset.x * cos - offset.y * sin,
                offset.x * sin + offset.y * cos,
            )
    };
    let tint = egui::Color32::from_white_alpha(opacity);
    let mut mesh = egui::Mesh::with_texture(texture.id());
    mesh.vertices.extend([
        egui::epaint::Vertex {
            pos: rotate(rect.left_top()),
            uv: egui::pos2(0.0, 0.0),
            color: tint,
        },
        egui::epaint::Vertex {
            pos: rotate(rect.right_top()),
            uv: egui::pos2(1.0, 0.0),
            color: tint,
        },
        egui::epaint::Vertex {
            pos: rotate(rect.right_bottom()),
            uv: egui::pos2(1.0, 1.0),
            color: tint,
        },
        egui::epaint::Vertex {
            pos: rotate(rect.left_bottom()),
            uv: egui::pos2(0.0, 1.0),
            color: tint,
        },
    ]);
    mesh.indices.extend([0, 1, 2, 0, 2, 3]);
    painter.add(egui::Shape::mesh(mesh));
}

fn draw_layered_logo_loader(
    ui: &mut egui::Ui,
    last_layer: &egui::TextureHandle,
    middle_layer: &egui::TextureHandle,
    top_layer: &egui::TextureHandle,
    spinning: bool,
    size: f32,
) -> egui::Response {
    let canvas_size = size + 4.0;
    let (canvas, response) =
        ui.allocate_exact_size(egui::vec2(canvas_size, canvas_size), egui::Sense::hover());
    let logo_rect = egui::Rect::from_center_size(canvas.center(), egui::vec2(size, size));
    let painter = ui.painter_at(canvas);
    let time = ui.ctx().input(|input| input.time) as f32;
    let pulse = if spinning {
        0.5 + 0.5 * (time * std::f32::consts::TAU / 3.0).sin()
    } else {
        0.5
    };
    painter.circle_filled(
        canvas.center(),
        size * (0.38 + pulse * 0.05),
        egui::Color32::from_rgba_unmultiplied(80, 60, 200, (12.0 + pulse * 18.0) as u8),
    );

    let (outer_angle, middle_angle, top_angle) = if spinning {
        (
            time * std::f32::consts::TAU / 10.0,
            -time * std::f32::consts::TAU / 6.0,
            time * std::f32::consts::TAU / 4.0,
        )
    } else {
        (0.0, 0.0, 0.0)
    };

    paint_rotated_texture(&painter, last_layer, logo_rect, outer_angle, 230);
    paint_rotated_texture(&painter, middle_layer, logo_rect, middle_angle, 255);
    paint_rotated_texture(&painter, top_layer, logo_rect, top_angle, 217);

    if spinning {
        ui.ctx().request_repaint();
    }
    response
}

fn get_secure_key(name: &str) -> Option<String> {
    if let Ok(entry) = keyring::Entry::new("norvexum", name) {
        if let Ok(pw) = entry.get_password() {
            return Some(pw);
        }
    }
    // Fallback to environment variables which might be loaded from .env
    std::env::var(name.to_uppercase()).ok()
}

fn set_secure_key(name: &str, val: &str) {
    if let Ok(entry) = keyring::Entry::new("norvexum", name) {
        let _ = entry.set_password(val);
    }

    // Also explicitly save to ~/.norvexum/.env as a fallback because keyring can fail silently on some Windows setups
    if let Some(home) = dirs::home_dir() {
        let norv_dir = home.join(".norvexum");
        let _ = std::fs::create_dir_all(&norv_dir);
        let env_path = norv_dir.join(".env");

        let env_key = name.to_uppercase();
        let mut lines = Vec::new();
        if env_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&env_path) {
                lines = content.lines().map(|s| s.to_string()).collect();
            }
        }

        lines.retain(|l| !l.starts_with(&format!("{}=", env_key)));
        lines.push(format!("{}={}", env_key, val));

        let _ = std::fs::write(&env_path, lines.join("\n"));
    }
}

fn delete_secure_key(name: &str) {
    if let Ok(entry) = keyring::Entry::new("norvexum", name) {
        let _ = entry.delete_password();
    }

    if let Some(home) = dirs::home_dir() {
        let env_path = home.join(".norvexum").join(".env");
        if env_path.exists() {
            let env_key = name.to_uppercase();
            if let Ok(content) = std::fs::read_to_string(&env_path) {
                let lines: Vec<String> = content
                    .lines()
                    .filter(|l| !l.starts_with(&format!("{}=", env_key)))
                    .map(|s| s.to_string())
                    .collect();
                let _ = std::fs::write(&env_path, lines.join("\n"));
            }
        }
    }
}

fn list_workspace_files(root: &std::path::Path) -> Vec<String> {
    const SKIP_DIRS: &[&str] = &[
        ".git",
        ".norvexum",
        "target",
        "node_modules",
        "build",
        "dist",
        "deps",
        ".agents",
        ".gemini",
        "__pycache__",
        ".venv",
        "venv",
    ];
    const MAX_DEPTH: usize = 8;
    const MAX_FILES: usize = 5000;
    let mut files = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new(), 0usize)];
    while let Some((dir, prefix, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().to_string();
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if file_type.is_dir() {
                if depth + 1 < MAX_DEPTH && !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push((entry.path(), rel, depth + 1));
                }
            } else if file_type.is_file() {
                if depth == 0 && (name.starts_with('.') || name == "norvexum.log") {
                    continue;
                }
                files.push(rel);
                if files.len() >= MAX_FILES {
                    files.sort();
                    return files;
                }
            }
        }
    }
    files.sort();
    files
}

impl GuiApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        mut config: GuiConfig,
        cancel_token: CancellationToken,
    ) -> Self {
        // Register custom panic hook to write log to ~/.norvexum
        std::panic::set_hook(Box::new(|info| {
            let log_dir = dirs::home_dir()
                .map(|h| h.join(".norvexum"))
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            let _ = std::fs::create_dir_all(&log_dir);
            let log_file = log_dir.join("norvexum_crash.log");
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_file)
            {
                use std::io::Write;
                let _ = writeln!(file, "Panic: {:?}", info);
            }
        }));

        // Load secure keys from keyring
        if let Some(k) = get_secure_key("google_ai_api_key") {
            config.settings.google_ai_api_key = Some(k);
        }
        if let Some(k) = get_secure_key("openai_api_key") {
            config.settings.openai_api_key = Some(k);
        }
        if let Some(k) = get_secure_key("anthropic_api_key") {
            config.settings.anthropic_api_key = Some(k);
        }
        if let Some(k) = get_secure_key("tavily_api_key") {
            config.settings.tavily_api_key = Some(k);
        }
        if let Some(k) = get_secure_key("aicredits_api_key") {
            config.settings.aicredits_api_key = Some(k);
        }
        if let Some(k) = get_secure_key("ocr_space_api_key") {
            config.settings.ocr_space_api_key = Some(k);
        }
        if let Some(k) = get_secure_key("pollinations_api_key") {
            config.settings.pollinations_api_key = Some(k);
        }

        let last_layer_tex = bytes_to_texture(&cc.egui_ctx, "last_layer", LAST_LAYER_BYTES);
        let middle_layer_tex = bytes_to_texture(&cc.egui_ctx, "middle_layer", MIDDLE_LAYER_BYTES);
        let top_layer_tex = bytes_to_texture(&cc.egui_ctx, "top_layer", TOP_LAYER_BYTES);
        let logo_tex = bytes_to_texture(&cc.egui_ctx, "logo", LOGO_BYTES);
        style::install_fonts(&cc.egui_ctx);

        // Sync process CWD with project_root on startup
        let _ = std::env::set_current_dir(&config.settings.project_root);

        // Initial folder scan (loaded in background)
        let workspace_files = Vec::new();
        let (config_maximized, config_fullscreen) =
            (config.settings.maximized, config.settings.fullscreen);
        let folder_input = config.settings.project_root.to_string_lossy().to_string();
        let skills_list = crate::skills::load_all_skills(&config.settings.project_root);

        let history_sessions = crate::agent::session::Session::list(&config.settings.project_root);
        let (capabilities_tx, capabilities_rx) = std_mpsc::channel();
        let initial_provider = config.settings.active_provider.clone();
        let initial_model = config.settings.active_model.clone();

        let mut app = Self {
            settings: config.settings,
            agent_rx: config.agent_rx,
            user_tx: config.user_tx,
            inst_tx: config.inst_tx,
            cancel_tx: config.cancel_tx,
            approval_tx: config.approval_tx,
            cancel_token,
            input_text: String::new(),
            model_search_query: String::new(),
            messages: Vec::new(),
            status_text: "Idle".to_string(),
            is_processing: false,
            agent_crashed: false,
            dark_mode: true,
            key_inputs: KeyInputs::default(),
            watcher: None,
            file_rx: None,
            workspace_files,
            folder_input,
            folder_error: None,
            approval_pending: None,
            last_layer_tex,
            middle_layer_tex,
            top_layer_tex,
            logo_tex,
            show_settings_modal: false,
            custom_skill_name: String::new(),
            custom_skill_desc: String::new(),
            custom_skill_triggers: String::new(),
            custom_skill_instructions: String::new(),
            skills_list,
            new_model_provider: "openai".to_string(),
            new_model_id: String::new(),
            left_sidebar_open: true,
            right_sidebar_open: true,
            attachments: Vec::new(),
            session_written_files: Vec::new(),
            current_writing_file: None,
            current_writing_status: None,
            file_diff_stats: std::collections::HashMap::new(),
            current_tool_name: None,
            current_tool_args: String::new(),
            current_tool_id: None,
            history_sessions,
            current_session_id: None,
            active_messages_backup: None,
            selected_file: None,
            inspected_file_content: String::new(),
            inspector_error: None,
            workspace_files_rx: None,
            is_loading_files: false,
            last_fs_event: None,
            pending_fs_reload: false,
            line_range_enabled: false,
            line_range_start: 1,
            line_range_end: 100,
            line_range_total_lines: 0,
            manual_edits_enabled: false,
            inspected_file_edited: false,
            thinking_start_time: None,
            last_scrolled_range: None,
            should_scroll_to_line: None,
            agent_active_session_id: None,
            left_sidebar_width: 280.0,
            right_sidebar_width: 380.0,
            ctx: None,
            last_active_provider: initial_provider,
            last_active_model: initial_model,
            show_terminal_panel: false,
            model_reasoning_type: None,
            probed_capabilities: std::collections::HashMap::new(),
            probing_in_progress: false,
            capabilities_rx: Some(capabilities_rx),
            capabilities_tx: Some(capabilities_tx),
            applied_theme: None,
            right_tab: RightTab::Changed,
            file_filter: String::new(),
            show_shortcuts: false,
            focus_composer: true,
            scroll_to_bottom: false,
            chat_max_offset: 0.0,
            settings_section: SettingsSection::General,
            pending_delete_session: None,
            expanded_dirs: std::collections::HashSet::new(),
            file_tree: FileTree::default(),
            file_tree_filter: String::new(),
            file_tree_dirty: true,
            paste_key_was_down: false,
            subagents: Vec::new(),
            chat_view: ChatView::Main,
            split_view: false,
            approval_agent: None,
            browser_tex: None,
            last_window_size: None,
            startup_window_state: Some((config_maximized, config_fullscreen)),
            browser_url: String::new(),
            browser_url_editing: false,
            shell: shell::ShellState::default(),
        };

        // The agent's browser is shown in the Browser tab, not as a separate window.
        crate::tools::browser::show_in_app(true);
        {
            let ctx = cc.egui_ctx.clone();
            crate::tools::browser::on_new_frame(move || ctx.request_repaint());
        }

        app.load_capabilities_cache();
        app.trigger_capabilities_probe();

        if let Some(msg) = config.initial_message {
            app.thinking_start_time = Some(std::time::Instant::now());
            app.messages.push(GuiMessage {
                role: "user".to_string(),
                content: msg,
                thinking: None,
                is_thinking_done: false,
                tool_logs: Vec::new(),
                attachments: Vec::new(),
            });
            app.is_processing = true;
        }

        // Start file watcher
        app.start_file_watcher(cc.egui_ctx.clone());

        // Trigger loading the workspace files list in the background immediately
        app.trigger_load_files();

        // Scan for local Ollama models in background on startup
        {
            let ollama_url = app
                .settings
                .ollama_base_url
                .clone()
                .unwrap_or_else(|| crate::ai::ollama::DEFAULT_OLLAMA_URL.to_string());
            let ctx = cc.egui_ctx.clone();
            std::thread::spawn(move || {
                if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    rt.block_on(async {
                        let _ = crate::ai::ollama::discover_models(&ollama_url).await;
                        ctx.request_repaint();
                    });
                }
            });
        }

        app
    }

    fn trigger_load_files(&mut self) {
        if self.is_loading_files {
            return;
        }
        self.is_loading_files = true;
        let root = self.settings.project_root.clone();
        let (tx, rx) = std_mpsc::channel();
        self.workspace_files_rx = Some(rx);
        std::thread::spawn(move || {
            let files = list_workspace_files(&root);
            let _ = tx.send(files);
        });
    }

    fn start_file_watcher(&mut self, ctx: egui::Context) {
        let (tx, rx) = std_mpsc::channel::<String>();
        self.file_rx = Some(rx);

        let path = self.settings.project_root.clone();
        let ctx_clone = ctx.clone();
        let watcher_res = notify::recommended_watcher(move |res: NotifyResult<notify::Event>| {
            if let Ok(event) = res {
                for path_buf in event.paths {
                    if let Some(filename) = path_buf.file_name().and_then(|s| s.to_str()) {
                        let path_str = path_buf.to_string_lossy();
                        if !path_str.contains(".git")
                            && !path_str.contains(".norvexum")
                            && !path_str.contains("target")
                        {
                            let _ = tx.send(filename.to_string());
                            ctx_clone.request_repaint();
                        }
                    }
                }
            }
        });

        if let Ok(mut watcher) = watcher_res {
            let _ = watcher.watch(&path, RecursiveMode::Recursive);
            self.watcher = Some(watcher);
        }
    }

    fn draw_key_field(&mut self, ui: &mut egui::Ui, field_name: &str, label: &str) {
        let p = self.pal();
        ui.label(egui::RichText::new(label).size(12.5).color(p.dim));

        let is_set = match field_name {
            "google_ai_api_key" => self.settings.google_ai_api_key.is_some(),
            "openai_api_key" => self.settings.openai_api_key.is_some(),
            "anthropic_api_key" => self.settings.anthropic_api_key.is_some(),
            "tavily_api_key" => self.settings.tavily_api_key.is_some(),
            "aicredits_api_key" => self.settings.aicredits_api_key.is_some(),
            "ocr_space_api_key" => self.settings.ocr_space_api_key.is_some(),
            "pollinations_api_key" => self.settings.pollinations_api_key.is_some(),
            _ => false,
        };

        if is_set {
            ui.horizontal(|ui| {
                style::dot(ui, p.success);
                ui.label(egui::RichText::new("Saved").size(12.5).color(p.text));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if style::link_button(ui, "Remove", &p)
                        .on_hover_text("Delete this key from the keyring")
                        .clicked()
                    {
                        delete_secure_key(field_name);
                        match field_name {
                            "google_ai_api_key" => self.settings.google_ai_api_key = None,
                            "openai_api_key" => self.settings.openai_api_key = None,
                            "anthropic_api_key" => self.settings.anthropic_api_key = None,
                            "tavily_api_key" => self.settings.tavily_api_key = None,
                            "aicredits_api_key" => self.settings.aicredits_api_key = None,
                            "ocr_space_api_key" => self.settings.ocr_space_api_key = None,
                            "pollinations_api_key" => self.settings.pollinations_api_key = None,
                            _ => {}
                        }
                        self.sync_settings();
                    }
                });
            });
        } else {
            let mut val = match field_name {
                "google_ai_api_key" => std::mem::take(&mut self.key_inputs.google),
                "openai_api_key" => std::mem::take(&mut self.key_inputs.openai),
                "anthropic_api_key" => std::mem::take(&mut self.key_inputs.anthropic),
                "tavily_api_key" => std::mem::take(&mut self.key_inputs.tavily),
                "aicredits_api_key" => std::mem::take(&mut self.key_inputs.aicredits),
                "ocr_space_api_key" => std::mem::take(&mut self.key_inputs.ocr),
                "pollinations_api_key" => std::mem::take(&mut self.key_inputs.pollinations),
                _ => String::new(),
            };

            let mut saved = false;
            ui.horizontal(|ui| {
                let field_w = (ui.available_width() - 58.0).max(80.0);
                let response = ui.add(
                    egui::TextEdit::singleline(&mut val)
                        .password(true)
                        .hint_text("Paste key")
                        .desired_width(field_w),
                );
                let submitted =
                    response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if style::secondary_button(ui, "Save", &p).clicked() || submitted {
                    let trimmed_val = val.trim().to_string();
                    if !trimmed_val.is_empty() {
                        set_secure_key(field_name, &trimmed_val);
                        let key_val = Some(trimmed_val.clone());
                        match field_name {
                            "google_ai_api_key" => self.settings.google_ai_api_key = key_val,
                            "openai_api_key" => self.settings.openai_api_key = key_val,
                            "anthropic_api_key" => self.settings.anthropic_api_key = key_val,
                            "tavily_api_key" => self.settings.tavily_api_key = key_val,
                            "aicredits_api_key" => self.settings.aicredits_api_key = key_val,
                            "ocr_space_api_key" => self.settings.ocr_space_api_key = key_val,
                            "pollinations_api_key" => self.settings.pollinations_api_key = key_val,
                            _ => {}
                        }
                        val.clear();
                        saved = true;
                    }
                }
            });

            if saved {
                self.sync_settings();
            } else {
                match field_name {
                    "google_ai_api_key" => self.key_inputs.google = val,
                    "openai_api_key" => self.key_inputs.openai = val,
                    "anthropic_api_key" => self.key_inputs.anthropic = val,
                    "tavily_api_key" => self.key_inputs.tavily = val,
                    "aicredits_api_key" => self.key_inputs.aicredits = val,
                    "ocr_space_api_key" => self.key_inputs.ocr = val,
                    "pollinations_api_key" => self.key_inputs.pollinations = val,
                    _ => {}
                }
            }
        }
        ui.add_space(8.0);
    }

    fn sync_settings(&mut self) {
        let _ = self.settings.save();
        if let Ok(json) = serde_json::to_string(&self.settings) {
            let _ = self.user_tx.send(format!("/config_update {json}"));
        }

        // Unload Ollama model from memory if I changed the active provider from Ollama to something else,
        // or if I changed the Ollama model itself.
        let last_provider = self.last_active_provider.clone();
        let last_model = self.last_active_model.clone();
        let current_provider = self.settings.active_provider.clone();
        let current_model = self.settings.active_model.clone();

        if last_provider == "ollama"
            && (current_provider != "ollama" || last_model != current_model)
        {
            let ollama_url = self
                .settings
                .ollama_base_url
                .clone()
                .unwrap_or_else(|| crate::ai::ollama::DEFAULT_OLLAMA_URL.to_string());
            let client = reqwest::Client::new();
            let unload_url = format!("{}/api/generate", ollama_url.trim_end_matches('/'));
            std::thread::spawn(move || {
                if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    rt.block_on(async {
                        let payload = serde_json::json!({
                            "model": last_model,
                            "keep_alive": 0
                        });
                        let _ = client.post(&unload_url).json(&payload).send().await;
                    });
                }
            });
        }

        self.last_active_provider = current_provider;
        self.last_active_model = current_model;
        // Sync local Ollama models in background in case the endpoint URL changed
        let ollama_url = self
            .settings
            .ollama_base_url
            .clone()
            .unwrap_or_else(|| crate::ai::ollama::DEFAULT_OLLAMA_URL.to_string());
        if let Some(ref ctx) = self.ctx {
            let ctx_clone = ctx.clone();
            std::thread::spawn(move || {
                if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    rt.block_on(async {
                        let _ = crate::ai::ollama::discover_models(&ollama_url).await;
                        ctx_clone.request_repaint();
                    });
                }
            });
        } else {
            std::thread::spawn(move || {
                if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    rt.block_on(async {
                        let _ = crate::ai::ollama::discover_models(&ollama_url).await;
                    });
                }
            });
        }
        self.trigger_capabilities_probe();
    }

    fn save_capabilities_cache(&self) {
        let cache_dir = dirs::home_dir()
            .map(|h| h.join(".norvexum"))
            .unwrap_or_else(|| self.settings.project_root.join(crate::config::NORVEXUM_DIR));
        let _ = std::fs::create_dir_all(&cache_dir);
        let cache_path = cache_dir.join("capabilities_cache.json");
        if let Ok(content) = serde_json::to_string_pretty(&self.probed_capabilities) {
            let _ = std::fs::write(cache_path, content);
        }
    }

    fn load_capabilities_cache(&mut self) {
        let cache_dir = dirs::home_dir()
            .map(|h| h.join(".norvexum"))
            .unwrap_or_else(|| self.settings.project_root.join(crate::config::NORVEXUM_DIR));
        let cache_path = cache_dir.join("capabilities_cache.json");
        if cache_path.exists() {
            if let Ok(content) = std::fs::read_to_string(cache_path) {
                if let Ok(map) = serde_json::from_str(&content) {
                    self.probed_capabilities = map;
                }
            }
        }
    }

    fn trigger_capabilities_probe(&mut self) {
        let provider = self.settings.active_provider.clone();
        let model = self.settings.active_model.clone();
        let key = format!("{provider}:{model}");

        // 1. Check cache first
        if let Some(&reasoning_type) = self.probed_capabilities.get(&key) {
            self.model_reasoning_type = reasoning_type;
            self.settings.model_reasoning_type = reasoning_type;
            self.probing_in_progress = false;
            return;
        }

        // 2. Set immediate heuristic guess to keep UI responsive
        let heuristic = crate::ai::guess_reasoning_support_by_name(&provider, &model);
        self.model_reasoning_type = heuristic;
        self.settings.model_reasoning_type = heuristic;

        if provider == "ollama" {
            self.probing_in_progress = false;
            return;
        }

        // 3. Spawn dynamic verification probe in background
        self.probing_in_progress = true;
        let tx = self.capabilities_tx.clone();
        let settings = self.settings.clone();
        let ctx_opt = self.ctx.clone();

        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(r) => r,
                Err(_) => return,
            };

            rt.block_on(async {
                let reasoning_type =
                    crate::ai::probe_model_reasoning(&settings, &provider, &model).await;
                if let Some(ref tx) = tx {
                    let _ = tx.send(CapabilitiesUpdate {
                        provider,
                        model,
                        reasoning_type,
                    });
                }
                if let Some(ref ctx) = ctx_opt {
                    ctx.request_repaint();
                }
            });
        });
    }

    fn poll_capabilities(&mut self) {
        if let Some(ref rx) = self.capabilities_rx {
            while let Ok(update) = rx.try_recv() {
                let key = format!("{}:{}", update.provider, update.model);
                self.probed_capabilities.insert(key, update.reasoning_type);
                if self.settings.active_provider == update.provider
                    && self.settings.active_model == update.model
                {
                    self.model_reasoning_type = update.reasoning_type;
                    self.settings.model_reasoning_type = update.reasoning_type;
                    self.probing_in_progress = false;
                }
                self.save_capabilities_cache();
            }
        }
    }

    fn load_project_folder(&mut self, path: &std::path::Path, ctx: egui::Context) {
        self.folder_error = None;
        let path_buf = path.to_path_buf();

        // Auto-initialize the directory structure synchronously if it is not a Norvexum workspace
        let norvexum_path = path_buf.join(".norvexum");
        if !norvexum_path.exists() {
            let path_clone = path_buf.clone();
            if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                let temp_settings = Settings {
                    project_root: path_clone,
                    ..Settings::default()
                };
                let _ = rt.block_on(crate::commands::init::run(&temp_settings));
            }
        }

        let _ = std::env::set_current_dir(&path_buf);
        let env_path = path_buf.join(".env");
        if env_path.exists() {
            let _ = dotenvy::from_path(&env_path);
        }

        // Reload settings from the new directory
        if let Ok(mut loaded_settings) = crate::config::Settings::load() {
            // Force project_root to the explicitly chosen path (defensive safety net)
            loaded_settings.project_root = path_buf.clone();
            loaded_settings.fullscreen = self.settings.fullscreen;
            loaded_settings.maximized = self.settings.maximized;
            // Retain keyring values in memory
            if let Some(k) = get_secure_key("google_ai_api_key") {
                loaded_settings.google_ai_api_key = Some(k);
            }
            if let Some(k) = get_secure_key("openai_api_key") {
                loaded_settings.openai_api_key = Some(k);
            }
            if let Some(k) = get_secure_key("anthropic_api_key") {
                loaded_settings.anthropic_api_key = Some(k);
            }
            if let Some(k) = get_secure_key("tavily_api_key") {
                loaded_settings.tavily_api_key = Some(k);
            }
            if let Some(k) = get_secure_key("aicredits_api_key") {
                loaded_settings.aicredits_api_key = Some(k);
            }
            if let Some(k) = get_secure_key("ocr_space_api_key") {
                loaded_settings.ocr_space_api_key = Some(k);
            }
            if let Some(k) = get_secure_key("pollinations_api_key") {
                loaded_settings.pollinations_api_key = Some(k);
            }
            self.settings = loaded_settings;
        } else {
            self.settings.project_root = path_buf.clone();
        }

        // Clear chat to prevent LLM memory contamination from the old workspace
        self.messages.clear();
        self.session_written_files.clear();
        self.file_diff_stats.clear();
        self.current_session_id = None;
        self.active_messages_backup = None;
        let _ = self.user_tx.send("/clear".to_string());

        self.trigger_load_files();
        self.start_file_watcher(ctx);
        self.sync_settings();
    }

    fn get_active_editing_range(&self) -> Option<(usize, usize)> {
        let name = self.current_tool_name.as_deref()?;
        let args = &self.current_tool_args;
        if name == "replace_file_content"
            || name == "multi_replace_file_content"
            || name == "edit_file"
        {
            let start_opt = extract_streaming_number(args, "StartLine");
            let end_opt = extract_streaming_number(args, "EndLine");
            if let (Some(start), Some(end)) = (start_opt, end_opt) {
                return Some((start, end));
            }
            let start_opt2 = extract_streaming_number(args, "start_line");
            let end_opt2 = extract_streaming_number(args, "end_line");
            if let (Some(start), Some(end)) = (start_opt2, end_opt2) {
                return Some((start, end));
            }

            let mut target = extract_streaming_content(args, "TargetContent");
            if target.is_empty() {
                target = extract_streaming_content(args, "target");
            }
            if !target.is_empty() {
                if let Some(filename) = self.selected_file.as_ref() {
                    let full_path = self.settings.project_root.join(filename);
                    if let Ok(file_content) = std::fs::read_to_string(&full_path) {
                        if let Some(pos) = file_content.find(&target) {
                            let start_line = file_content[..pos].lines().count() + 1;
                            let end_line = start_line + target.lines().count().max(1) - 1;
                            return Some((start_line, end_line));
                        }
                    }
                }
            }
        }
        None
    }

    fn inspect_file(&mut self, relative_path: &str) {
        const MAX_PREVIEW_BYTES: u64 = 256 * 1024;

        self.selected_file = Some(relative_path.to_string());
        self.inspector_error = None;
        self.inspected_file_content.clear();

        let is_writing = if let Some((ref active_file, _, _)) = self.current_writing_file {
            active_file == relative_path
                && self.current_writing_status.as_deref() == Some("writing")
        } else {
            false
        };

        if is_writing {
            let mut streaming_content =
                extract_streaming_content(&self.current_tool_args, "CodeContent");
            if streaming_content.is_empty() {
                streaming_content = extract_streaming_content(&self.current_tool_args, "content");
            }
            if streaming_content.is_empty() {
                streaming_content =
                    extract_streaming_content(&self.current_tool_args, "ReplacementContent");
            }
            if streaming_content.is_empty() {
                streaming_content =
                    extract_streaming_content(&self.current_tool_args, "TargetContent");
            }

            if !streaming_content.is_empty() {
                self.inspected_file_content = streaming_content;
                return;
            } else {
                let path = self.settings.project_root.join(relative_path);
                if !path.exists() {
                    self.inspected_file_content = "Writing file content...".to_string();
                    return;
                }
            }
        }

        let path = self.settings.project_root.join(relative_path);
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.len() > MAX_PREVIEW_BYTES && !self.line_range_enabled => {
                self.inspector_error = Some(format!(
                    "Preview unavailable: this file is larger than {} KB. Turn on Lines to view part of it.",
                    MAX_PREVIEW_BYTES / 1024
                ));
            }
            Ok(_) => match std::fs::read_to_string(&path) {
                Ok(content) => {
                    let lines: Vec<&str> = content.lines().collect();
                    self.line_range_total_lines = lines.len();
                    if self.line_range_enabled {
                        let start = self.line_range_start.clamp(1, lines.len().max(1)) - 1;
                        let end = self
                            .line_range_end
                            .clamp(1, lines.len().max(1))
                            .max(self.line_range_start)
                            - 1;
                        let mut filtered_content = String::new();
                        for line in lines.iter().take(end + 1).skip(start) {
                            filtered_content.push_str(line);
                            filtered_content.push('\n');
                        }
                        self.inspected_file_content = filtered_content;
                    } else {
                        self.inspected_file_content = content;
                    }
                }
                Err(_) => {
                    self.inspector_error =
                        Some("Preview unavailable for binary or unreadable files.".to_string());
                }
            },
            Err(err) => {
                self.inspector_error = Some(format!("Could not open this file: {err}"));
            }
        }
    }

    fn save_inspected_file_changes(&mut self) -> Result<(), std::io::Error> {
        if let Some(ref filename) = self.selected_file {
            let path = self.settings.project_root.join(filename);
            if self.line_range_enabled {
                let original_content = std::fs::read_to_string(&path)?;
                let original_lines: Vec<&str> = original_content.lines().collect();
                let start = self.line_range_start.clamp(1, original_lines.len().max(1)) - 1;
                let end = self
                    .line_range_end
                    .clamp(1, original_lines.len().max(1))
                    .max(self.line_range_start)
                    - 1;

                let mut new_lines: Vec<String> = original_lines
                    .iter()
                    .take(start)
                    .map(|l| l.to_string())
                    .collect();
                new_lines.extend(self.inspected_file_content.lines().map(str::to_string));
                new_lines.extend(original_lines.iter().skip(end + 1).map(|l| l.to_string()));

                // Keep the file's line endings and trailing newline.
                let eol = if original_content.contains("\r\n") {
                    "\r\n"
                } else {
                    "\n"
                };
                let mut joined = new_lines.join(eol);
                if original_content.ends_with('\n') {
                    joined.push_str(eol);
                }
                std::fs::write(&path, joined)?;
            } else {
                std::fs::write(&path, &self.inspected_file_content)?;
            }
            self.inspected_file_edited = false;
            let file_clone = filename.clone();
            self.inspect_file(&file_clone);
        }
        Ok(())
    }

    fn get_or_create_assistant_message(&mut self) -> &mut GuiMessage {
        let has_assistant = self
            .messages
            .last()
            .map(|m| m.role == "assistant")
            .unwrap_or(false);
        if !has_assistant {
            self.messages.push(GuiMessage {
                role: "assistant".to_string(),
                content: String::new(),
                thinking: None,
                is_thinking_done: false,
                tool_logs: Vec::new(),
                attachments: Vec::new(),
            });
        }
        self.messages.last_mut().unwrap()
    }

    fn check_and_split_assistant_message(&mut self) {
        if let Some(m) = self.messages.last() {
            if m.role == "assistant" && !m.content.trim().is_empty() {
                self.messages.push(GuiMessage {
                    role: "assistant".to_string(),
                    content: String::new(),
                    thinking: None,
                    is_thinking_done: false,
                    tool_logs: Vec::new(),
                    attachments: Vec::new(),
                });
            }
        }
    }

    fn poll_agent_events(&mut self, ctx: &egui::Context) {
        if self.agent_crashed {
            return;
        }

        loop {
            match self.agent_rx.try_recv() {
                Ok(event) => {
                    ctx.request_repaint();
                    match event {
                        AgentEvent::Status(s) => {
                            self.status_text = s.clone();
                            // Only attach status logs to an assistant message if there's an active conversation
                            if self.messages.iter().any(|m| m.role == "user") {
                                // A new step starts a new bubble if the current one already has
                                // text, so the step header and the reasoning streamed under it
                                // stay in the same bubble and render live.
                                if s.starts_with("Thinking (") {
                                    self.check_and_split_assistant_message();
                                }
                                let msg = self.get_or_create_assistant_message();
                                msg.tool_logs.push(format!("ℹ️ {s}"));
                            }
                        }
                        AgentEvent::Thinking(t) => {
                            self.is_processing = true;
                            // Only create assistant messages if there's an active conversation
                            if self.messages.iter().any(|m| m.role == "user") {
                                // Reasoning belongs to the bubble holding the current step
                                // header; splitting here orphaned it from its header.
                                let msg = self.get_or_create_assistant_message();
                                if let Some(ref mut thinking) = msg.thinking {
                                    thinking.push_str(&t);
                                } else {
                                    msg.thinking = Some(t.clone());
                                }
                                msg.tool_logs.push(format!("THINKING_DELTA|{}", t));
                            }
                        }
                        AgentEvent::ThinkingDone => {
                            let msg = self.get_or_create_assistant_message();
                            msg.is_thinking_done = true;
                        }
                        AgentEvent::Content(c) => {
                            self.is_processing = true;
                            let msg = self.get_or_create_assistant_message();
                            msg.content.push_str(&c);
                        }
                        AgentEvent::ToolStart { name, id } => {
                            self.status_text = format!("Running {name}");
                            self.current_tool_name = Some(name.clone());
                            self.current_tool_id = Some(id.clone());
                            self.current_tool_args.clear();
                            self.shell.tool_args.insert(id.clone(), String::new());
                            self.shell
                                .tool_started
                                .insert(id.clone(), std::time::Instant::now());

                            self.check_and_split_assistant_message();
                            let msg = self.get_or_create_assistant_message();
                            msg.tool_logs.push(format!("TOOL_START|{id}|{name}"));

                            if name == "write_to_file"
                                || name == "write_file"
                                || name == "replace_file_content"
                                || name == "multi_replace_file_content"
                                || name == "edit_file"
                            {
                                self.current_writing_status =
                                    Some("Preparing file changes…".to_string());
                                self.current_writing_file = None;
                            }
                        }
                        AgentEvent::ToolArgsDelta { id, delta } => {
                            if let Some(args) = self.shell.tool_args.get_mut(&id) {
                                args.push_str(&delta);
                            }
                            if Some(&id) == self.current_tool_id.as_ref() {
                                self.current_tool_args.push_str(&delta);

                                let name = self.current_tool_name.clone().unwrap_or_default();
                                let is_write = name == "write_file"
                                    || name == "write_to_file"
                                    || name == "edit_file"
                                    || name == "replace_file_content"
                                    || name == "multi_replace_file_content";
                                if is_write {
                                    let mut path =
                                        extract_streaming_content(&self.current_tool_args, "path");
                                    if path.is_empty() {
                                        path = extract_streaming_content(
                                            &self.current_tool_args,
                                            "TargetFile",
                                        );
                                    }
                                    if !path.is_empty() {
                                        let relative_path =
                                            get_relative_path(&path, &self.settings.project_root);

                                        let (added, deleted) = estimate_live_diff_stats(
                                            &name,
                                            &self.current_tool_args,
                                            &self.settings.project_root,
                                        );
                                        self.current_writing_file =
                                            Some((relative_path.clone(), added, deleted));
                                        self.current_writing_status = Some("writing".to_string());

                                        self.selected_file = Some(relative_path.clone());
                                        self.inspect_file(&relative_path);
                                    }
                                }
                            }
                        }
                        AgentEvent::ToolResult {
                            id,
                            name,
                            result,
                            success,
                        } => {
                            let args_summary = if let Some(args) = self.shell.tool_args.get(&id) {
                                format_tool_args_summary(&name, args)
                            } else if Some(&id) == self.current_tool_id.as_ref() {
                                format_tool_args_summary(&name, &self.current_tool_args)
                            } else {
                                String::new()
                            };

                            // Extract target path for file writes before clearing current_tool_args
                            let target_path = if Some(&id) == self.current_tool_id.as_ref() {
                                let mut path =
                                    extract_streaming_content(&self.current_tool_args, "path");
                                if path.is_empty() {
                                    path = extract_streaming_content(
                                        &self.current_tool_args,
                                        "TargetFile",
                                    );
                                }
                                if path.is_empty() { None } else { Some(path) }
                            } else {
                                None
                            };

                            // Keep the call's input and output for its card in the chat.
                            let args = self.shell.tool_args.remove(&id).unwrap_or_default();
                            let duration = self
                                .shell
                                .tool_started
                                .remove(&id)
                                .map(|t| t.elapsed())
                                .unwrap_or_default();
                            self.shell.tool_details.insert(
                                id.clone(),
                                shell::ToolDetail {
                                    name: name.clone(),
                                    args,
                                    result: crate::util::truncate_middle(&result, 20_000),
                                    success,
                                    duration,
                                },
                            );

                            if Some(&id) == self.current_tool_id.as_ref() {
                                self.current_tool_name = None;
                                self.current_tool_id = None;
                                self.current_tool_args.clear();
                            }

                            let status = if success { "✅ Success" } else { "❌ Failed" };
                            self.status_text =
                                format!("{name} {}", if success { "finished" } else { "failed" });
                            let msg = self.get_or_create_assistant_message();
                            msg.tool_logs.push(format!(
                                "TOOL_RESULT|{}|{}|{}|{}",
                                id, name, status, args_summary
                            ));

                            if name == "write_to_file"
                                || name == "write_file"
                                || name == "replace_file_content"
                                || name == "multi_replace_file_content"
                                || name == "edit_file"
                            {
                                if success {
                                    self.current_writing_status =
                                        Some("File changes completed".to_string());
                                    if let Some(path) = target_path {
                                        let relative_path =
                                            get_relative_path(&path, &self.settings.project_root);
                                        if !self.session_written_files.contains(&relative_path) {
                                            self.session_written_files.push(relative_path.clone());
                                        }
                                    }
                                } else {
                                    self.current_writing_status =
                                        Some("File change failed".to_string());
                                }
                            }
                        }
                        AgentEvent::FileWrite { path, .. } => {
                            let relative_path =
                                get_relative_path(&path, &self.settings.project_root);

                            // Prefer live stats from estimate_live_diff_stats (computed during
                            // streaming from TargetContent/ReplacementContent) because they are
                            // more accurate — git diff runs AFTER the write, so for untracked
                            // files it can never see deleted lines.
                            let (added, deleted) =
                                if let Some((ref live_file, live_add, live_del)) =
                                    self.current_writing_file
                                {
                                    if live_file == &relative_path && (live_add > 0 || live_del > 0)
                                    {
                                        (live_add as i32, live_del as i32)
                                    } else {
                                        get_file_diff_stats(
                                            &self.settings.project_root,
                                            &relative_path,
                                        )
                                    }
                                } else {
                                    get_file_diff_stats(&self.settings.project_root, &relative_path)
                                };
                            let icon = get_file_icon(&relative_path);

                            let msg = self.get_or_create_assistant_message();
                            msg.tool_logs.push(format!(
                                "FILE_CHANGE|{}|{}|+{}|-{}",
                                icon, relative_path, added, deleted
                            ));
                            // Don't reload synchronously — the file watcher will
                            // fire events anyway, and `poll_file_events` already
                            // debounces those. Scheduling a reload here would
                            // re-trigger the scanner on every single file write,
                            // which is what was freezing the app for a few
                            // seconds after the agent finished creating files.
                            self.pending_fs_reload = true;
                            self.last_fs_event = Some(std::time::Instant::now());

                            if !self.session_written_files.contains(&relative_path) {
                                self.session_written_files.push(relative_path.clone());
                            }
                            self.current_writing_file =
                                Some((relative_path.clone(), added as usize, deleted as usize));
                            self.current_writing_status = Some("writing".to_string());
                            // Persist diff stats so they remain visible after the edit completes
                            let entry = self.file_diff_stats.entry(relative_path).or_insert((0, 0));
                            entry.0 = added as usize;
                            entry.1 = deleted as usize;

                            if self.selected_file.as_deref()
                                == self
                                    .current_writing_file
                                    .as_ref()
                                    .map(|file| file.0.as_str())
                            {
                                let selected =
                                    self.current_writing_file.as_ref().unwrap().0.clone();
                                self.inspect_file(&selected);
                            }
                        }
                        AgentEvent::Done { .. } => {
                            self.is_processing = false;
                            self.status_text = "Idle".to_string();
                            self.current_writing_status = None;
                            self.current_writing_file = None;
                            self.thinking_start_time = None;
                            self.last_scrolled_range = None;
                            self.history_sessions =
                                crate::agent::session::Session::list(&self.settings.project_root);
                        }
                        AgentEvent::Error(err) => {
                            self.is_processing = false;
                            self.status_text = "Error".to_string();
                            self.thinking_start_time = None;
                            self.last_scrolled_range = None;

                            let display_err = if err.contains("error decoding response body") {
                                "API Provider connection dropped (error decoding response body). Please check your internet connection or try again.".to_string()
                            } else {
                                err
                            };

                            let clean_err = display_err.replace('\u{fe0f}', "");
                            let msg = self.get_or_create_assistant_message();
                            msg.tool_logs.push(format!("⚠️ Error: {clean_err}"));
                            let error_line = format!("⚠️ **Execution Error:** {clean_err}");
                            if !msg.content.contains(&error_line) {
                                if !msg.content.is_empty() {
                                    msg.content.push_str("\n\n");
                                }
                                msg.content.push_str(&error_line);
                            }
                        }
                        AgentEvent::Warning(warning) => {
                            let clean = warning.replace('\u{fe0f}', "");
                            self.status_text = clean.clone();
                            let msg = self.get_or_create_assistant_message();
                            msg.tool_logs.push(format!("⚠️ {clean}"));
                        }
                        AgentEvent::ApprovalRequest {
                            id,
                            tool_name,
                            args_preview,
                        } => {
                            self.approval_pending = Some(crate::ui::ApprovalInfo {
                                id,
                                tool_name,
                                args_preview,
                            });
                            self.approval_agent = None;
                        }
                        AgentEvent::ModelSwitched { model, provider } => {
                            self.settings.active_model = model;
                            self.settings.active_provider = provider;
                            self.status_text = "Model updated".to_string();
                        }
                        AgentEvent::SubAgentStarted { id, name, task } => {
                            self.status_text = format!("Started sub-agent {name}");
                            self.get_or_create_assistant_message()
                                .tool_logs
                                .push(format!(
                                    "AGENT_EVT|started|{name}|{}",
                                    task.replace('|', "/")
                                ));
                            self.subagents.push(SubAgentView::new(id, name, task));
                        }
                        AgentEvent::SubAgent { id, event } => {
                            self.handle_subagent_event(&id, *event);
                        }
                        AgentEvent::AgentMessage { from, to, text } => {
                            self.show_agent_message(&from, &to, &text);
                        }
                        AgentEvent::SubAgentFinished {
                            id,
                            success,
                            summary,
                        } => {
                            if let Some(sub) = self.subagents.iter_mut().find(|s| s.id == id) {
                                let line = format!(
                                    "AGENT_EVT|{}|{}|{}",
                                    if success { "done" } else { "stopped" },
                                    sub.name,
                                    summary.replace('|', "/")
                                );
                                sub.finished = Some((success, summary, std::time::Instant::now()));
                                self.status_text = format!(
                                    "Sub-agent {} {}",
                                    sub.name,
                                    if success { "finished" } else { "stopped" }
                                );
                                let notice = self.status_text.clone();
                                self.get_or_create_assistant_message().tool_logs.push(line);
                                self.toast(
                                    notice,
                                    if success {
                                        shell::ToastKind::Success
                                    } else {
                                        shell::ToastKind::Error
                                    },
                                );
                            }
                            if self.approval_agent.as_deref() == Some(id.as_str()) {
                                self.approval_pending = None;
                                self.approval_agent = None;
                            }
                        }
                        AgentEvent::ClearChat => {
                            self.subagents.clear();
                            self.chat_view = ChatView::Main;
                            self.messages.clear();
                            self.session_written_files.clear();
                            self.file_diff_stats.clear();
                            self.current_session_id = None;
                            self.active_messages_backup = None;
                            self.status_text = "Chat cleared".to_string();
                        }
                        _ => {}
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => {
                    break;
                }
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.agent_crashed = true;
                    self.is_processing = false;
                    self.status_text = "Agent Crashed!".to_string();
                    break;
                }
            }
        }
    }

    fn poll_file_events(&mut self, ctx: &egui::Context) {
        // Drain the background file-scan result if it's ready.
        if let Some(ref rx) = self.workspace_files_rx {
            if let Ok(files) = rx.try_recv() {
                self.workspace_files = files;
                self.file_tree_dirty = true;
                self.workspace_files_rx = None;
                self.is_loading_files = false;
            }
        }

        // Drain the filesystem watcher queue. I coalesce all pending events
        // into a single `pending_fs_reload` flag and only actually re-scan
        // after the user (or agent) has stopped touching the filesystem for
        // FS_DEBOUNCE_MS. This is the difference between the UI stalling for
        // seconds vs. staying at 60fps while the agent writes a dozen files.
        const FS_DEBOUNCE_MS: u64 = 400;

        if let Some(ref rx) = self.file_rx {
            let mut got_any = false;
            while rx.try_recv().is_ok() {
                got_any = true;
            }
            if got_any {
                self.last_fs_event = Some(std::time::Instant::now());
                self.pending_fs_reload = true;
            }
        }

        if self.pending_fs_reload {
            let ready = self
                .last_fs_event
                .map(|t| t.elapsed().as_millis() as u64 >= FS_DEBOUNCE_MS)
                .unwrap_or(true);

            if ready && !self.is_loading_files {
                self.pending_fs_reload = false;
                self.trigger_load_files();
            } else {
                // Keep the UI responsive while I wait for the debounce
                // window to expire — without this the file list would feel
                // like it updates with a stutter.
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
        }
    }

    /// Ctrl+V with an image or copied files on the clipboard attaches them.
    /// egui only turns text pastes into events and swallows the key otherwise,
    /// so the key itself is read from the OS.
    fn handle_media_paste(&mut self, ctx: &egui::Context) {
        let (command, focused, pasted_text) = ctx.input(|i| {
            (
                i.modifiers.command,
                i.focused,
                i.events.iter().any(|e| matches!(e, egui::Event::Paste(_))),
            )
        });
        let down = command && focused && paste_key_down();
        let pressed = down && !self.paste_key_was_down;
        self.paste_key_was_down = down;
        if command && focused {
            // The V press itself produces no event; keep looking while Ctrl is held.
            ctx.request_repaint_after(std::time::Duration::from_millis(30));
        }
        // A text paste was already handled by the focused text box.
        if pressed && !pasted_text {
            self.paste_clipboard_media(false);
        }
    }

    /// Attach the files or image on the clipboard. `report_empty` says so when
    /// there is neither (only when the user explicitly asked to paste).
    fn paste_clipboard_media(&mut self, report_empty: bool) {
        let Ok(mut clipboard) = arboard::Clipboard::new() else {
            return;
        };
        if let Ok(files) = clipboard.get().file_list() {
            let files: Vec<_> = files.into_iter().filter(|f| f.is_file()).collect();
            if !files.is_empty() {
                self.add_attachments(files);
                return;
            }
        }
        match clipboard.get_image() {
            Ok(image) => match save_pasted_image(&self.settings.project_root, &image) {
                Ok(path) => {
                    self.add_attachments(vec![path]);
                    self.trigger_load_files();
                }
                Err(e) => self.status_text = format!("Couldn't paste the image: {e}"),
            },
            Err(_) if report_empty => {
                self.status_text = "No image or files on the clipboard".to_string();
            }
            Err(_) => {}
        }
    }

    /// Answer the pending approval, to the sub-agent that asked if it was one.
    fn answer_approval(&mut self, approved: bool) {
        if self.approval_pending.take().is_none() {
            return;
        }
        match self.approval_agent.take() {
            Some(id) => crate::agent::subagent::respond_approval(&id, approved),
            None => {
                let _ = self.approval_tx.send(approved);
            }
        }
    }

    /// Queue files for the next message, skipping ones already queued.
    fn add_attachments(&mut self, paths: Vec<std::path::PathBuf>) {
        let before = self.attachments.len();
        for path in paths {
            if !self.attachments.contains(&path) {
                self.attachments.push(path);
            }
        }
        let added = self.attachments.len() - before;
        self.status_text = match (added, self.attachments.last()) {
            (0, _) => "Already attached".to_string(),
            (1, Some(path)) => format!(
                "Attached {}",
                path.file_name().and_then(|n| n.to_str()).unwrap_or("file")
            ),
            (n, _) => format!("Attached {n} files"),
        };
        self.focus_composer = true;
    }

    fn send_message(&mut self) {
        if self.input_text.trim().is_empty() && self.attachments.is_empty() {
            return;
        }

        let mut raw_prompt = self.input_text.trim().to_string();
        if raw_prompt.is_empty() {
            raw_prompt = if self.attachments.len() == 1 {
                "Take a look at the attached file.".to_string()
            } else {
                "Take a look at the attached files.".to_string()
            };
        }
        let display_prompt = raw_prompt.clone();

        // Each file is placed inside the project so the sandboxed agent can use it.
        let mut placed = Vec::new();
        let mut unplaced = Vec::new();
        for file_path in std::mem::take(&mut self.attachments) {
            let dest_path = attachment_destination(&self.settings.project_root, &file_path);
            let already_inside = dest_path == file_path;
            if already_inside || std::fs::copy(&file_path, &dest_path).is_ok() {
                placed.push(std::path::absolute(&dest_path).unwrap_or(dest_path));
            } else {
                unplaced.push(file_path);
            }
        }
        if !placed.is_empty() {
            let paths: String = placed
                .iter()
                .map(|p| format!("{}|", p.to_string_lossy()))
                .collect();
            raw_prompt = format!("/image_prompt |{paths} {raw_prompt}");
            self.trigger_load_files();
        }
        for file_path in &unplaced {
            raw_prompt = format!("{} ({})", raw_prompt, file_path.to_string_lossy());
        }

        self.messages.push(GuiMessage {
            role: "user".to_string(),
            content: display_prompt,
            thinking: None,
            is_thinking_done: false,
            tool_logs: Vec::new(),
            attachments: placed.into_iter().chain(unplaced).collect(),
        });

        // If this is the start of a conversation, clear session files
        if self.messages.len() == 1 {
            self.session_written_files.clear();
            self.file_diff_stats.clear();
        }

        if self.is_processing {
            let _ = self.inst_tx.send(raw_prompt);
        } else {
            self.current_writing_file = None;
            self.current_writing_status = None;
            self.current_tool_name = None;
            self.current_tool_id = None;
            self.current_tool_args.clear();
            self.thinking_start_time = Some(std::time::Instant::now());

            // Switched session context if needed
            if self.current_session_id != self.agent_active_session_id {
                if let Some(ref session_id) = self.current_session_id {
                    let _ = self.user_tx.send(format!("/session load {session_id}"));
                }
                self.agent_active_session_id = self.current_session_id.clone();
            }

            let _ = self.user_tx.send(raw_prompt);
            self.is_processing = true;
        }
        self.input_text.clear();
    }

    fn draw_settings_modal(&mut self, ctx: &egui::Context) {
        let p = self.pal();
        let screen = ctx.screen_rect();
        let size = egui::vec2(
            (screen.width() - 80.0).clamp(360.0, 820.0),
            (screen.height() - 120.0).clamp(280.0, 600.0),
        );
        let mut close = false;
        egui::Window::new("Settings")
            .id(egui::Id::new("settings_window"))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .fixed_size(size)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("Settings")
                            .font(style::strong(16.0))
                            .color(p.text),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if style::link_button(ui, "Close", &p)
                            .on_hover_text("Close  (Esc)")
                            .clicked()
                        {
                            close = true;
                        }
                    });
                });
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);

                let body_h = (ui.available_height() - 44.0).max(120.0);
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(150.0);
                        ui.spacing_mut().item_spacing.y = 2.0;
                        for section in SettingsSection::ALL {
                            if menu_row(ui, section.label(), self.settings_section == section, &p)
                                .clicked()
                            {
                                self.settings_section = section;
                            }
                        }
                    });
                    ui.add_space(6.0);
                    ui.separator();
                    ui.add_space(10.0);
                    ui.vertical(|ui| {
                        egui::ScrollArea::vertical()
                            .id_salt(("settings_body", self.settings_section))
                            .max_height(body_h)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.set_width((ui.available_width() - 10.0).max(120.0));
                                let changed = match self.settings_section {
                                    SettingsSection::General => self.settings_general(ui, ctx, &p),
                                    SettingsSection::Permissions => {
                                        self.settings_permissions(ui, &p)
                                    }
                                    SettingsSection::Models => self.settings_models(ui, &p),
                                    SettingsSection::ApiKeys => {
                                        self.settings_api_keys(ui, &p);
                                        false
                                    }
                                    SettingsSection::Mcp => {
                                        self.settings_mcp(ui, &p);
                                        false
                                    }
                                    SettingsSection::Skills => {
                                        self.settings_skills(ui, &p);
                                        false
                                    }
                                    SettingsSection::Advanced => self.settings_advanced(ui, &p),
                                };
                                if changed {
                                    self.sync_settings();
                                }
                            });
                    });
                });

                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("Changes apply immediately.")
                            .size(11.5)
                            .color(p.faint),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if style::primary_button(ui, "Done", &p).clicked() {
                            close = true;
                        }
                    });
                });
            });
        if close {
            self.sync_settings();
            self.show_settings_modal = false;
        }
    }

    fn settings_heading(ui: &mut egui::Ui, title: &str, subtitle: &str, p: &Palette) {
        ui.label(
            egui::RichText::new(title)
                .font(style::strong(14.5))
                .color(p.text),
        );
        if !subtitle.is_empty() {
            ui.label(egui::RichText::new(subtitle).size(12.0).color(p.dim));
        }
        ui.add_space(8.0);
    }

    fn settings_general(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, p: &Palette) -> bool {
        Self::settings_heading(ui, "General", "", p);
        style::section_label(ui, "Appearance", p);
        ui.horizontal(|ui| {
            if ui.selectable_label(self.dark_mode, "Dark").clicked() {
                self.dark_mode = true;
            }
            if ui.selectable_label(!self.dark_mode, "Light").clicked() {
                self.dark_mode = false;
            }
        });
        ui.add_space(14.0);
        style::section_label(ui, "Workspace folder", p);
        self.draw_workspace_picker(ui, ctx, p);
        ui.add_space(14.0);
        style::section_label(ui, "Keyboard", p);
        if style::secondary_button(ui, "Show keyboard shortcuts", p).clicked() {
            self.show_shortcuts = true;
        }
        ui.add_space(14.0);
        style::section_label(ui, "About", p);
        ui.label(
            egui::RichText::new(concat!("Norvexum v", env!("CARGO_PKG_VERSION")))
                .size(12.5)
                .color(p.dim),
        );
        false
    }

    fn settings_permissions(&mut self, ui: &mut egui::Ui, p: &Palette) -> bool {
        Self::settings_heading(
            ui,
            "Permissions",
            "Choose which tools need your approval before they run.",
            p,
        );
        let mut changed = false;
        let mut ask_perms = self.settings.ask_perms;
        if ui
            .checkbox(&mut ask_perms, "Ask before running sensitive tools")
            .changed()
        {
            self.settings.ask_perms = ask_perms;
            changed = true;
        }
        ui.add_space(4.0);

        ui.add_enabled_ui(ask_perms, |ui| {
            let critical_tools = [
                (vec!["run_command", "shell"], "Shell commands"),
                (
                    vec!["write_file", "write_to_file"],
                    "Create or overwrite files",
                ),
                (
                    vec![
                        "edit_file",
                        "replace_file_content",
                        "multi_replace_file_content",
                    ],
                    "Edit files",
                ),
                (vec!["remove", "delete_file"], "Delete files"),
            ];

            for (tool_names, tool_label) in critical_tools {
                let mut requires = tool_names
                    .iter()
                    .any(|name| self.settings.require_approval.iter().any(|t| t == name));
                ui.horizontal(|ui| {
                    ui.add_space(18.0);
                    if ui.checkbox(&mut requires, tool_label).changed() {
                        for name in &tool_names {
                            if requires {
                                if !self.settings.require_approval.contains(&name.to_string()) {
                                    self.settings.require_approval.push(name.to_string());
                                }
                            } else {
                                self.settings.require_approval.retain(|t| t != *name);
                            }
                        }
                        changed = true;
                    }
                    ui.label(
                        egui::RichText::new(tool_names[0])
                            .font(egui::FontId::monospace(11.0))
                            .color(p.faint),
                    );
                });
            }
        });
        changed
    }

    fn settings_models(&mut self, ui: &mut egui::Ui, p: &Palette) -> bool {
        Self::settings_heading(
            ui,
            "Models",
            "Pick the active model from the message box. Add model IDs that aren't in the built-in list here.",
            p,
        );
        let mut changed = false;
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("custom_model_provider_select")
                .selected_text(&self.new_model_provider)
                .width(120.0)
                .show_ui(ui, |ui| {
                    for prov in &[
                        "openai",
                        "google_direct",
                        "anthropic",
                        "aicredits",
                        "ollama",
                    ] {
                        ui.selectable_value(&mut self.new_model_provider, prov.to_string(), *prov);
                    }
                });
            let field_w = (ui.available_width() - 56.0).max(80.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.new_model_id)
                    .hint_text("model-id")
                    .desired_width(field_w),
            );
            if style::secondary_button(ui, "Add", p).clicked()
                && !self.new_model_id.trim().is_empty()
            {
                let full_model_entry =
                    format!("{}:{}", self.new_model_provider, self.new_model_id.trim());
                if !self.settings.custom_models.contains(&full_model_entry) {
                    self.settings.custom_models.push(full_model_entry);
                    self.new_model_id.clear();
                    changed = true;
                }
            }
        });

        ui.add_space(8.0);
        if self.settings.custom_models.is_empty() {
            ui.label(
                egui::RichText::new("No custom models yet.")
                    .size(12.0)
                    .color(p.faint),
            );
        } else {
            let mut to_remove = None;
            for (idx, m) in self.settings.custom_models.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(m)
                            .font(egui::FontId::monospace(12.0))
                            .color(p.text),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if style::link_button(ui, "Remove", p).clicked() {
                            to_remove = Some(idx);
                        }
                    });
                });
            }
            if let Some(idx) = to_remove {
                self.settings.custom_models.remove(idx);
                changed = true;
            }
        }
        changed
    }

    fn settings_api_keys(&mut self, ui: &mut egui::Ui, p: &Palette) {
        Self::settings_heading(
            ui,
            "API keys",
            "Stored in the system keyring, with a fallback in ~/.norvexum/.env.",
            p,
        );
        self.draw_key_field(ui, "openai_api_key", "OpenAI");
        self.draw_key_field(ui, "anthropic_api_key", "Anthropic");
        self.draw_key_field(ui, "google_ai_api_key", "Google AI");
        self.draw_key_field(ui, "aicredits_api_key", "AICredits");
        self.draw_key_field(ui, "tavily_api_key", "Tavily (web search)");
        self.draw_key_field(ui, "ocr_space_api_key", "OCR.space");
        self.draw_key_field(ui, "pollinations_api_key", "Pollinations (images)");
    }

    fn settings_skills(&mut self, ui: &mut egui::Ui, p: &Palette) {
        Self::settings_heading(
            ui,
            "Skills",
            "Reusable instructions the agent loads when a trigger phrase matches.",
            p,
        );
        if style::secondary_button(ui, "Import skill file…", p)
            .on_hover_text("Add a skill from a Markdown file")
            .clicked()
        {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("Markdown", &["md"])
                .pick_file()
            {
                let skill_dir = crate::skills::get_global_skills_dir();
                let _ = std::fs::create_dir_all(&skill_dir);
                if let Some(filename) = path.file_name() {
                    let dest = skill_dir.join(filename);
                    if std::fs::copy(&path, &dest).is_ok() {
                        self.status_text =
                            format!("Skill file {} uploaded!", filename.to_string_lossy());
                        self.skills_list =
                            crate::skills::load_all_skills(&self.settings.project_root);
                    }
                }
            }
        }

        ui.add_space(14.0);
        style::section_label(ui, "New skill", p);
        ui.add(
            egui::TextEdit::singleline(&mut self.custom_skill_name)
                .hint_text("Name")
                .desired_width(f32::INFINITY),
        );
        ui.add(
            egui::TextEdit::singleline(&mut self.custom_skill_desc)
                .hint_text("Description")
                .desired_width(f32::INFINITY),
        );
        ui.add(
            egui::TextEdit::singleline(&mut self.custom_skill_triggers)
                .hint_text("Trigger phrases, comma separated")
                .desired_width(f32::INFINITY),
        );
        ui.add(
            egui::TextEdit::multiline(&mut self.custom_skill_instructions)
                .hint_text("Instructions")
                .desired_rows(4)
                .desired_width(f32::INFINITY),
        );

        if style::secondary_button(ui, "Add skill", p).clicked() {
            let name = self.custom_skill_name.trim().to_string();
            let inst = self.custom_skill_instructions.trim().to_string();
            if !name.is_empty() && !inst.is_empty() {
                let desc = self.custom_skill_desc.trim().to_string();
                let triggers: Vec<String> = self
                    .custom_skill_triggers
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();

                let triggers_str = if triggers.is_empty() {
                    format!("  - {}", name.to_lowercase())
                } else {
                    triggers
                        .iter()
                        .map(|t| format!("  - {}", t))
                        .collect::<Vec<_>>()
                        .join("\n")
                };

                let content = format!(
                    "---\nname: {}\ndescription: {}\ntrigger_patterns:\n{}\n---\n{}",
                    name, desc, triggers_str, inst
                );

                let skill_dir = crate::skills::get_global_skills_dir();
                let _ = std::fs::create_dir_all(&skill_dir);
                let file_path = skill_dir.join(format!("{}.md", name.to_lowercase()));
                if std::fs::write(&file_path, content).is_ok() {
                    self.status_text = format!("Skill {} saved!", name);
                    self.skills_list = crate::skills::load_all_skills(&self.settings.project_root);

                    self.custom_skill_name.clear();
                    self.custom_skill_desc.clear();
                    self.custom_skill_triggers.clear();
                    self.custom_skill_instructions.clear();
                }
            }
        }

        ui.add_space(14.0);
        style::section_label(ui, &format!("Loaded · {}", self.skills_list.len()), p);
        if self.skills_list.is_empty() {
            ui.label(
                egui::RichText::new("No skills loaded.")
                    .size(12.0)
                    .color(p.faint),
            );
        }
        for skill in &self.skills_list {
            ui.label(egui::RichText::new(&skill.name).size(13.0).color(p.text));
            ui.add(
                egui::Label::new(
                    egui::RichText::new(&skill.description)
                        .size(12.0)
                        .color(p.faint),
                )
                .wrap(),
            );
            ui.add_space(4.0);
        }
    }

    fn settings_advanced(&mut self, ui: &mut egui::Ui, p: &Palette) -> bool {
        Self::settings_heading(ui, "Advanced", "", p);
        let mut changed = false;

        style::section_label(ui, "Limits", p);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Tool loops per turn").color(p.text));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut is_infinite = self.settings.max_thinking_loops == 0;
                if ui.checkbox(&mut is_infinite, "Unlimited").changed() {
                    if is_infinite {
                        self.settings.max_thinking_loops = 0;
                    } else {
                        self.settings.max_thinking_loops = 25; // default back to 25
                    }
                    changed = true;
                }
            });
        });
        ui.add_enabled_ui(self.settings.max_thinking_loops != 0, |ui| {
            let mut temp_loops = if self.settings.max_thinking_loops == 0 {
                25
            } else {
                self.settings.max_thinking_loops
            };
            if ui
                .add(egui::Slider::new(&mut temp_loops, 1..=100))
                .changed()
            {
                self.settings.max_thinking_loops = temp_loops;
                changed = true;
            }
        });
        ui.add_space(6.0);

        ui.label(egui::RichText::new("Browser timeout").color(p.text))
            .on_hover_text("How long the browser tool waits for a page to load or respond");
        if ui
            .add(egui::Slider::new(&mut self.settings.browser_timeout_secs, 5..=300).suffix(" s"))
            .changed()
        {
            changed = true;
        }
        ui.add_space(6.0);

        ui.label(egui::RichText::new("Max content characters").color(p.text))
            .on_hover_text("Longest tool output (file reads, command logs) kept for the model; the middle of longer output is cut");
        if ui
            .add(
                egui::Slider::new(
                    &mut self.settings.max_content_chars,
                    MIN_CONTENT_CHARS..=MAX_CONTENT_CHARS,
                )
                .logarithmic(true),
            )
            .changed()
        {
            changed = true;
        }

        ui.add_space(16.0);
        style::section_label(ui, "Custom instructions", p);
        let mut val = self
            .settings
            .custom_system_prompt
            .clone()
            .unwrap_or_default();
        if ui
            .add(
                egui::TextEdit::multiline(&mut val)
                    .hint_text("Optional instructions added to every conversation")
                    .desired_rows(6)
                    .desired_width(f32::INFINITY),
            )
            .changed()
        {
            self.settings.custom_system_prompt = if val.trim().is_empty() {
                None
            } else {
                Some(val)
            };
            changed = true;
        }
        changed
    }

    /// Folder field with Browse/Open, shared by the sidebar and Settings.
    fn draw_workspace_picker(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, p: &Palette) {
        let field = ui.add(
            egui::TextEdit::singleline(&mut self.folder_input)
                .font(egui::TextStyle::Monospace)
                .desired_width(f32::INFINITY),
        );
        let mut open_typed = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        ui.horizontal(|ui| {
            if style::secondary_button(ui, "Browse…", p)
                .on_hover_text("Choose a workspace folder")
                .clicked()
            {
                if let Some(path) = rfd::FileDialog::new().pick_folder() {
                    self.folder_input = path.to_string_lossy().to_string();
                    self.load_project_folder(&path, ctx.clone());
                }
            }
            if style::secondary_button(ui, "Open", p)
                .on_hover_text("Open the folder typed above")
                .clicked()
            {
                open_typed = true;
            }
        });
        if open_typed {
            let path = std::path::PathBuf::from(&self.folder_input);
            if path.exists() && path.is_dir() {
                self.load_project_folder(&path, ctx.clone());
            } else {
                self.folder_error = Some("Directory path not found!".to_string());
            }
        }
        if let Some(ref err) = self.folder_error {
            ui.label(egui::RichText::new(err).size(12.0).color(p.danger));
        }
    }
}

/// System messages and empty assistant bubbles (no text, reasoning or tool
/// activity) are not drawn.
/// The agent's `/` commands, for the composer's suggestions.
const SLASH_COMMANDS: [(&str, &str); 11] = [
    ("/help", "Show every command"),
    ("/clear", "Clear the chat and the agent's memory"),
    ("/compact", "Summarize older messages to free context"),
    ("/undo", "Restore the files changed by the last edit"),
    (
        "/mcp",
        "MCP servers: list, add <name> <command|url>, remove",
    ),
    ("/loops", "Set the tool-loop limit (number or unlimited)"),
    ("/model", "List models or switch"),
    ("/provider", "List providers or switch"),
    ("/session", "resume · list · load <id> · clear"),
    ("/ask_perms", "Turn tool approval prompts on or off"),
    ("/copy", "Copy the last answer (or: /copy chat)"),
];

/// Commands matching what is typed, while only a command is typed.
fn slash_suggestions(input: &str) -> Vec<(&'static str, &'static str)> {
    let typed = input.trim_start();
    if !typed.starts_with('/') || typed.contains(char::is_whitespace) {
        return Vec::new();
    }
    let typed = typed.to_lowercase();
    SLASH_COMMANDS
        .iter()
        .filter(|(c, _)| c.starts_with(&typed) || shell::fuzzy_match(&typed[1..], &c[1..]))
        .copied()
        .collect()
}

fn is_message_shown(msg: &GuiMessage) -> bool {
    msg.role != "system"
        && !(msg.role == "assistant"
            && msg.content.trim().is_empty()
            && msg.thinking.as_ref().is_none_or(|t| t.trim().is_empty())
            && msg.tool_logs.is_empty())
}

/// The text of the whole reply ending at `last`: the consecutive assistant
/// bubbles a tool-using answer is split into.
fn reply_text(messages: &[GuiMessage], shown: &[usize], last: usize) -> String {
    let end = shown.iter().position(|&i| i == last).unwrap_or(0);
    let mut parts: Vec<&str> = shown[..=end]
        .iter()
        .rev()
        .take_while(|&&i| messages[i].role == "assistant")
        .map(|&i| messages[i].content.trim())
        .filter(|text| !text.is_empty())
        .collect();
    parts.reverse();
    parts.join("\n\n")
}

/// "Today, 14:05" / "Yesterday, 09:12" / "Oct 3, 18:40" for a saved chat.
fn format_session_time(rfc3339: &str) -> Option<String> {
    let t = chrono::DateTime::parse_from_rfc3339(rfc3339)
        .ok()?
        .with_timezone(&chrono::Local);
    let today = chrono::Local::now().date_naive();
    Some(if t.date_naive() == today {
        format!("Today, {}", t.format("%H:%M"))
    } else if Some(t.date_naive()) == today.pred_opt() {
        format!("Yesterday, {}", t.format("%H:%M"))
    } else {
        t.format("%b %-d, %H:%M").to_string()
    })
}

struct SessionRow {
    response: egui::Response,
    delete_clicked: bool,
    hovered: bool,
}

/// Two-line chat entry in the sidebar. When `armed`, the delete control
/// reads "Delete?" and the next click on it confirms.
fn session_row(
    ui: &mut egui::Ui,
    title: &str,
    meta: &str,
    selected: bool,
    deletable: bool,
    armed: bool,
    p: &Palette,
) -> SessionRow {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 40.0), egui::Sense::click());
    let del_w = if armed { 58.0 } else { 22.0 };
    let del_rect = egui::Rect::from_center_size(
        egui::pos2(rect.right() - 6.0 - del_w / 2.0, rect.center().y),
        egui::vec2(del_w, 22.0),
    );
    let del =
        deletable.then(|| ui.interact(del_rect, resp.id.with("delete"), egui::Sense::click()));
    let del_hovered = del.as_ref().is_some_and(|d| d.hovered());
    let hovered = resp.hovered() || del_hovered;

    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, 6.0, p.hover);
    } else if hovered {
        painter.rect_filled(rect, 6.0, p.surface);
    }
    let text_w = rect.width() - 20.0 - if deletable { del_w + 4.0 } else { 0.0 };
    let title_g = style::one_line(ui, title, egui::FontId::proportional(13.0), p.text, text_w);
    painter.galley(rect.left_top() + egui::vec2(10.0, 5.0), title_g, p.text);
    let meta_g = style::one_line(ui, meta, egui::FontId::proportional(11.0), p.faint, text_w);
    painter.galley(rect.left_top() + egui::vec2(10.0, 23.0), meta_g, p.faint);
    if deletable && armed {
        painter.rect_filled(del_rect, 5.0, p.danger.gamma_multiply(0.16));
        painter.text(
            del_rect.center(),
            egui::Align2::CENTER_CENTER,
            "Delete?",
            egui::FontId::proportional(11.5),
            p.danger,
        );
    } else if deletable && hovered {
        painter.text(
            del_rect.center(),
            egui::Align2::CENTER_CENTER,
            "×",
            egui::FontId::proportional(16.0),
            if del_hovered { p.danger } else { p.faint },
        );
    }
    let hint = if armed {
        "Click again to delete"
    } else {
        "Delete chat"
    };
    let delete_clicked = del.is_some_and(|d| d.on_hover_text(hint).clicked());
    SessionRow {
        response: resp,
        delete_clicked,
        hovered,
    }
}

/// Folder tree built from relative paths ("src/ui/mod.rs").
#[derive(Default)]
struct FileTree {
    dirs: std::collections::BTreeMap<String, FileTree>,
    /// (file name, full relative path)
    files: Vec<(String, String)>,
}

impl FileTree {
    fn build<'a>(paths: impl IntoIterator<Item = &'a String>) -> Self {
        let mut root = FileTree::default();
        for path in paths {
            let parts: Vec<&str> = path.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
            let Some((name, dirs)) = parts.split_last() else {
                continue;
            };
            let mut node = &mut root;
            for dir in dirs {
                node = node.dirs.entry(dir.to_string()).or_default();
            }
            node.files.push((name.to_string(), path.clone()));
        }
        root.sort();
        root
    }

    fn sort(&mut self) {
        self.files.sort_by_key(|(name, _)| name.to_lowercase());
        for dir in self.dirs.values_mut() {
            dir.sort();
        }
    }

    fn is_empty(&self) -> bool {
        self.files.is_empty() && self.dirs.is_empty()
    }
}

#[derive(Default, Clone, Copy)]
struct FileDecor {
    writing: bool,
    added: usize,
    deleted: usize,
}

struct TreeView<'a> {
    expanded: &'a mut std::collections::HashSet<String>,
    /// Show every folder open (filtered results, the "Changed" list).
    force_open: bool,
    selected: Option<&'a str>,
    decor: &'a dyn Fn(&str) -> FileDecor,
    clicked: Option<String>,
    p: &'a Palette,
    time: f32,
}

fn draw_tree(ui: &mut egui::Ui, node: &FileTree, prefix: &str, depth: usize, view: &mut TreeView) {
    for (name, child) in &node.dirs {
        let mut key = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        // Chains of single folders share one row ("src/engine/physics").
        let mut label = name.clone();
        let mut child = child;
        while child.files.is_empty() && child.dirs.len() == 1 {
            let (next_name, next) = child.dirs.iter().next().expect("one entry");
            label = format!("{label}/{next_name}");
            key = format!("{key}/{next_name}");
            child = next;
        }
        let open = view.force_open || view.expanded.contains(&key);
        let resp = tree_row(
            ui,
            depth,
            Some(open),
            &label,
            false,
            FileDecor::default(),
            view.p,
            view.time,
        );
        if resp.clicked() && !view.force_open {
            if open {
                view.expanded.remove(&key);
            } else {
                view.expanded.insert(key.clone());
            }
        }
        if open {
            draw_tree(ui, child, &key, depth + 1, view);
        }
    }
    for (name, path) in &node.files {
        let decor = (view.decor)(path);
        let selected = view.selected == Some(path.as_str());
        if tree_row(ui, depth, None, name, selected, decor, view.p, view.time)
            .on_hover_text(path.as_str())
            .clicked()
        {
            view.clicked = Some(path.clone());
        }
    }
}

/// One tree line: indent guides, a chevron for folders (or a pulsing dot
/// for a file being written), the name, and diff counts on the right.
#[allow(clippy::too_many_arguments)]
fn tree_row(
    ui: &mut egui::Ui,
    depth: usize,
    dir_open: Option<bool>,
    name: &str,
    selected: bool,
    decor: FileDecor,
    p: &Palette,
    time: f32,
) -> egui::Response {
    const INDENT: f32 = 14.0;
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 24.0), egui::Sense::click());
    let painter = ui.painter();
    if decor.writing {
        painter.rect_filled(rect, 5.0, p.accent_soft);
    } else if selected {
        painter.rect_filled(rect, 5.0, p.hover);
    } else if resp.hovered() {
        painter.rect_filled(rect, 5.0, p.surface);
    }
    if selected {
        painter.rect_filled(
            egui::Rect::from_min_size(
                rect.left_top() + egui::vec2(0.0, 5.0),
                egui::vec2(2.0, 14.0),
            ),
            1.0,
            p.accent,
        );
    }
    for level in 0..depth {
        let x = rect.left() + 11.0 + level as f32 * INDENT;
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            egui::Stroke::new(1.0_f32, p.border),
        );
    }

    let cy = rect.center().y;
    let x0 = rect.left() + 6.0 + depth as f32 * INDENT;
    let mark_x = x0 + 5.0;
    let chevron =
        |points: Vec<egui::Pos2>| egui::Shape::convex_polygon(points, p.faint, egui::Stroke::NONE);
    match dir_open {
        Some(true) => {
            painter.add(chevron(vec![
                egui::pos2(mark_x - 3.5, cy - 1.5),
                egui::pos2(mark_x + 3.5, cy - 1.5),
                egui::pos2(mark_x, cy + 2.5),
            ]));
        }
        Some(false) => {
            painter.add(chevron(vec![
                egui::pos2(mark_x - 1.5, cy - 3.5),
                egui::pos2(mark_x + 2.5, cy),
                egui::pos2(mark_x - 1.5, cy + 3.5),
            ]));
        }
        None if decor.writing => {
            let pulse = 0.55 + 0.45 * (time * std::f32::consts::TAU / 1.4).sin();
            painter.circle_filled(egui::pos2(mark_x, cy), 3.5, p.accent.gamma_multiply(pulse));
        }
        None => {}
    }

    let mut right = rect.right() - 8.0;
    if decor.writing {
        let g =
            painter.layout_no_wrap("writing".into(), egui::FontId::proportional(11.0), p.accent);
        right -= g.size().x;
        painter.galley(egui::pos2(right, cy - g.size().y / 2.0), g, p.accent);
        right -= 8.0;
    }
    for (n, color, sign) in [
        (decor.deleted, p.danger, '-'),
        (decor.added, p.success, '+'),
    ] {
        if n > 0 {
            let g =
                painter.layout_no_wrap(format!("{sign}{n}"), egui::FontId::monospace(11.0), color);
            right -= g.size().x;
            painter.galley(egui::pos2(right, cy - g.size().y / 2.0), g, color);
            right -= 6.0;
        }
    }

    let text_x = x0 + 14.0;
    let color = if dir_open.is_some() { p.dim } else { p.text };
    let name_g = style::one_line(
        ui,
        name,
        egui::FontId::proportional(12.5),
        color,
        right - text_x - 4.0,
    );
    painter.galley(
        egui::pos2(text_x, cy - name_g.size().y / 2.0),
        name_g,
        color,
    );
    resp
}

/// A changed file: name with its folder underneath, diff counts on the right,
/// and a pulsing dot while the agent is writing it.
fn change_row(
    ui: &mut egui::Ui,
    path: &str,
    decor: FileDecor,
    selected: bool,
    p: &Palette,
    time: f32,
) -> egui::Response {
    let (dir, name) = path.rsplit_once(['/', '\\']).unwrap_or(("", path));
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 40.0), egui::Sense::click());
    let painter = ui.painter();
    if decor.writing {
        painter.rect_filled(rect, 7.0, p.accent_soft);
    } else if selected {
        painter.rect_filled(rect, 7.0, p.hover);
    } else if resp.hovered() {
        painter.rect_filled(rect, 7.0, p.surface);
    }
    // Leading mark: writing pulse, or a quiet dot.
    let mark = egui::pos2(rect.left() + 12.0, rect.center().y);
    if decor.writing {
        let pulse = 0.55 + 0.45 * (time * std::f32::consts::TAU / 1.4).sin();
        painter.circle_filled(mark, 4.0, p.accent.gamma_multiply(pulse));
    } else {
        painter.circle_filled(
            mark,
            3.0,
            if decor.deleted > 0 && decor.added == 0 {
                p.danger
            } else {
                p.success
            }
            .gamma_multiply(0.8),
        );
    }

    let mut right = rect.right() - 10.0;
    let mono = egui::FontId::monospace(11.0);
    for (n, color, sign) in [
        (decor.deleted, p.danger, '-'),
        (decor.added, p.success, '+'),
    ] {
        if n > 0 {
            let g = painter.layout_no_wrap(format!("{sign}{n}"), mono.clone(), color);
            right -= g.size().x;
            painter.galley(
                egui::pos2(right, rect.center().y - g.size().y / 2.0),
                g,
                color,
            );
            right -= 6.0;
        }
    }
    let text_x = rect.left() + 26.0;
    let width = right - text_x - 6.0;
    let name_g = style::one_line(ui, name, style::strong(12.5), p.text, width);
    let dir_label = if decor.writing {
        "writing…".to_string()
    } else if dir.is_empty() {
        "project root".to_string()
    } else {
        format!("{dir}/")
    };
    let dir_g = style::one_line(
        ui,
        &dir_label,
        egui::FontId::proportional(11.0),
        if decor.writing { p.accent } else { p.faint },
        width,
    );
    painter.galley(egui::pos2(text_x, rect.top() + 5.0), name_g, p.text);
    painter.galley(egui::pos2(text_x, rect.top() + 22.0), dir_g, p.faint);
    resp
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum SettingsSection {
    General,
    Permissions,
    Models,
    ApiKeys,
    Mcp,
    Skills,
    Advanced,
}

impl SettingsSection {
    const ALL: [Self; 7] = [
        Self::General,
        Self::Permissions,
        Self::Models,
        Self::ApiKeys,
        Self::Mcp,
        Self::Skills,
        Self::Advanced,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Permissions => "Permissions",
            Self::Models => "Models",
            Self::ApiKeys => "API keys",
            Self::Mcp => "MCP servers",
            Self::Skills => "Skills",
            Self::Advanced => "Advanced",
        }
    }
}

/// Full-width navigation entry (settings section list).
fn menu_row(ui: &mut egui::Ui, label: &str, selected: bool, p: &Palette) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::click());
    if selected {
        ui.painter().rect_filled(rect, 6.0, p.hover);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 6.0, p.surface);
    }
    ui.painter().text(
        egui::pos2(rect.left() + 10.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(13.0),
        if selected { p.text } else { p.dim },
    );
    resp
}

fn send_button(ui: &mut egui::Ui, enabled: bool, p: &Palette) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(28.0, 28.0), egui::Sense::click());
    let (fill, fg) = if enabled {
        let fill = if resp.hovered() {
            p.accent.gamma_multiply(0.85)
        } else {
            p.accent
        };
        (fill, p.on_accent)
    } else {
        (p.hover, p.faint)
    };
    let c = rect.center();
    ui.painter().circle_filled(c, 14.0, fill);
    let stroke = egui::Stroke::new(1.8_f32, fg);
    ui.painter().line_segment(
        [c + egui::vec2(0.0, 5.0), c + egui::vec2(0.0, -5.0)],
        stroke,
    );
    ui.painter().line_segment(
        [c + egui::vec2(-4.0, -1.0), c + egui::vec2(0.0, -5.5)],
        stroke,
    );
    ui.painter().line_segment(
        [c + egui::vec2(4.0, -1.0), c + egui::vec2(0.0, -5.5)],
        stroke,
    );
    resp
}

fn stop_button(ui: &mut egui::Ui, p: &Palette) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(28.0, 28.0), egui::Sense::click());
    let c = rect.center();
    let hovered = resp.hovered();
    ui.painter().circle_filled(c, 14.0, p.hover);
    ui.painter().rect_filled(
        egui::Rect::from_center_size(c, egui::vec2(9.0, 9.0)),
        2.0,
        if hovered { p.danger } else { p.text },
    );
    resp.on_hover_text("Stop the current response  (Esc)")
}

const SUGGESTIONS: &[&str] = &[
    "Explain how this project is organized",
    "Find and fix a bug in the code",
    "Write tests for the core module",
    "Review my uncommitted changes",
];

const SHORTCUTS: &[(&str, &str)] = &[
    ("Enter", "Send message"),
    ("Shift+Enter", "New line"),
    ("Up (empty box)", "Recall last prompt"),
    ("Esc", "Stop response, deny approval, close dialog"),
    ("Ctrl+N", "New chat"),
    ("Ctrl+L", "Focus the message box"),
    ("Ctrl+B", "Show or hide chats"),
    ("Ctrl+E", "Show or hide files"),
    ("Ctrl+J", "Show or hide terminal"),
    ("Ctrl+,", "Settings"),
    ("Ctrl+/", "Keyboard shortcuts"),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum RightTab {
    Changed,
    All,
    Browser,
    Agents,
}

/// Inner margin of the files panel; its divider runs out to the panel edges.
const RIGHT_PAD: f32 = 12.0;

/// Quiet placeholder for an empty list.
fn empty_hint(ui: &mut egui::Ui, title: &str, body: &str, p: &Palette) {
    ui.add_space(20.0);
    ui.vertical_centered(|ui| {
        ui.label(
            egui::RichText::new(title)
                .font(style::strong(12.5))
                .color(p.dim),
        );
        if !body.is_empty() {
            ui.add_space(2.0);
            ui.label(egui::RichText::new(body).size(12.0).color(p.faint));
        }
    });
}

impl GuiApp {
    fn pal(&self) -> Palette {
        Palette::new(self.dark_mode)
    }

    fn project_name(&self) -> String {
        self.settings
            .project_root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| self.settings.project_root.display().to_string())
    }

    fn new_chat(&mut self) {
        let _ = self.user_tx.send("/clear".to_string());
        self.messages.clear();
        self.session_written_files.clear();
        self.file_diff_stats.clear();
        self.current_session_id = None;
        self.active_messages_backup = None;
        self.selected_file = None;
        self.status_text = "Chat cleared".to_string();
        self.focus_composer = true;
    }

    fn stop_run(&mut self) {
        let _ = self.cancel_tx.send(());
        self.is_processing = false;
        self.status_text = "Stopped".to_string();
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        use egui::{Key, KeyboardShortcut, Modifiers};
        let shortcut = |key: Key| {
            ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, key)))
        };
        if shortcut(Key::N) && !self.is_processing {
            self.new_chat();
        }
        if shortcut(Key::B) {
            self.left_sidebar_open = !self.left_sidebar_open;
        }
        if shortcut(Key::E) {
            self.right_sidebar_open = !self.right_sidebar_open;
        }
        if shortcut(Key::J) {
            self.show_terminal_panel = !self.show_terminal_panel;
        }
        if shortcut(Key::Comma) {
            self.show_settings_modal = !self.show_settings_modal;
        }
        if shortcut(Key::Slash) {
            self.show_shortcuts = !self.show_shortcuts;
        }
        if shortcut(Key::L) {
            self.focus_composer = true;
        }
        if shortcut(Key::K) {
            self.shell.palette_open = !self.shell.palette_open;
            self.shell.palette_query.clear();
            self.shell.palette_selected = 0;
        }
        if self.shell.palette_open {
            return; // the palette takes the keyboard
        }

        // A pending approval takes Enter (when the box is empty) and Esc.
        if self.approval_pending.is_some() {
            let composer_empty = self.input_text.trim().is_empty();
            let answer = ctx.input_mut(|i| {
                if composer_empty && i.consume_key(Modifiers::NONE, Key::Enter) {
                    Some(true)
                } else if i.consume_key(Modifiers::NONE, Key::Escape) {
                    Some(false)
                } else {
                    None
                }
            });
            if let Some(approved) = answer {
                self.answer_approval(approved);
            }
            return;
        }

        let wants_escape = self.show_shortcuts || self.show_settings_modal || self.is_processing;
        if wants_escape
            && !ctx.memory(|m| m.any_popup_open())
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
        {
            if self.show_shortcuts {
                self.show_shortcuts = false;
            } else if self.show_settings_modal {
                self.show_settings_modal = false;
            } else if self.is_processing {
                self.stop_run();
            }
        }
    }

    fn draw_right_sidebar(&mut self, ctx: &egui::Context, p: &Palette) {
        // The page picture only needs refreshing while someone can see it.
        crate::tools::browser::set_live_view(
            self.right_sidebar_open && self.right_tab == RightTab::Browser,
        );
        let target_right_t = if self.right_sidebar_open { 1.0 } else { 0.0 };
        let right_t_raw =
            ctx.animate_value_with_time(egui::Id::new("right_sidebar_anim"), target_right_t, 0.18);
        let right_t = 1.0 - (1.0 - right_t_raw.clamp(0.0, 1.0)).powi(3);
        let right_animating = (right_t_raw - target_right_t).abs() > 0.005;
        if right_t <= 0.005 {
            return;
        }

        let mut right_panel = egui::SidePanel::right("right_sidebar")
            .frame(
                egui::Frame::none()
                    .fill(p.panel)
                    .inner_margin(egui::Margin::same(RIGHT_PAD)),
            )
            .resizable(!right_animating && self.right_sidebar_open)
            .width_range(60.0..=(ctx.screen_rect().width() * 0.6).max(320.0));
        if right_animating {
            right_panel = right_panel.exact_width(self.right_sidebar_width * right_t);
        } else {
            right_panel = right_panel.default_width(self.right_sidebar_width);
        }

        let panel_response = right_panel.show(ctx, |ui| {
            self.draw_background_processes(ui, p);
            self.draw_panel_tabs(ui, p);
            match self.right_tab {
                RightTab::Browser => self.draw_browser_tab(ui, p),
                RightTab::Agents => self.draw_agents_tab(ui, p),
                RightTab::Changed | RightTab::All => match self.selected_file.clone() {
                    // An open file takes the whole panel; × goes back to the list.
                    Some(filename) => self.draw_file_preview(ui, p, &filename),
                    None => {
                        let height = ui.available_height();
                        if let Some(filename) = self.draw_file_list(ui, p, height) {
                            self.inspect_file(&filename);
                        }
                    }
                },
            }
        });
        if !right_animating && self.right_sidebar_open {
            // Never let the panel crowd out the chat.
            let max = (ctx.screen_rect().width() * 0.6).max(320.0);
            self.right_sidebar_width = panel_response.response.rect.width().min(max);
        }
    }

    /// Commands the agent left running, each with a way to stop it.
    fn draw_background_processes(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let procs = crate::tools::shell::get_background_processes();
        if procs.is_empty() {
            return;
        }
        style::section_label(ui, &format!("Running · {}", procs.len()), p);
        ui.add_space(2.0);
        for proc in procs {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                style::dot(ui, p.success);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if style::link_button(ui, "Stop", p)
                        .on_hover_text("Stop this background process")
                        .clicked()
                    {
                        crate::tools::shell::kill_background_process(proc.pid);
                    }
                    ui.label(
                        egui::RichText::new(proc.pid.to_string())
                            .font(egui::FontId::monospace(11.0))
                            .color(p.faint),
                    );
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&proc.command)
                                    .font(egui::FontId::monospace(12.0))
                                    .color(p.text),
                            )
                            .truncate(),
                        )
                        .on_hover_text(&proc.command);
                    });
                });
            });
        }
        ui.add_space(14.0);
    }

    /// The panel's sections as one segmented control.
    fn draw_panel_tabs(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let tabs = [
            RightTab::Changed,
            RightTab::All,
            RightTab::Browser,
            RightTab::Agents,
        ];
        let changes = self.session_written_files.len();
        let running = self.subagents.iter().filter(|s| s.running()).count();
        let items = [
            ("Changes", (changes > 0).then(|| changes.to_string())),
            ("Files", None),
            (
                "Browser",
                crate::tools::browser::is_open().then(|| "●".to_string()),
            ),
            (
                "Agents",
                if running > 0 {
                    Some(running.to_string())
                } else {
                    (!self.subagents.is_empty()).then(|| self.subagents.len().to_string())
                },
            ),
        ];
        let selected = tabs.iter().position(|t| *t == self.right_tab).unwrap_or(0);
        if let Some(i) = style::segmented(ui, &items, selected, p) {
            if tabs[i] == self.right_tab && self.selected_file.is_some() {
                // Clicking the current tab again leaves an open file.
                self.selected_file = None;
            }
            self.right_tab = tabs[i];
        }
        ui.add_space(10.0);
    }

    /// The Changes or Files tree, `list_h` tall at most. Returns a clicked file.
    fn draw_file_list(&mut self, ui: &mut egui::Ui, p: &Palette, list_h: f32) -> Option<String> {
        let top = ui.cursor().top();
        let time = ui.input(|i| i.time) as f32;
        let selected = self.selected_file.clone();

        // Live diff counts for the file being written, persisted counts otherwise.
        let writing_file = self
            .current_writing_file
            .as_ref()
            .filter(|_| self.current_writing_status.as_deref() == Some("writing"))
            .map(|(f, _, _)| f.clone());
        let live = self.current_writing_file.clone();
        let stats = &self.file_diff_stats;
        let decor = |path: &str| -> FileDecor {
            let (added, deleted) = match &live {
                Some((f, a, d)) if f == path => (*a, *d),
                _ => stats.get(path).copied().unwrap_or((0, 0)),
            };
            FileDecor {
                writing: writing_file.as_deref() == Some(path),
                added,
                deleted,
            }
        };
        if writing_file.is_some() {
            ui.ctx().request_repaint();
        }

        match self.right_tab {
            RightTab::Changed => {
                let mut display_files = self.session_written_files.clone();
                if let Some(ref active_file) = writing_file {
                    if !active_file.is_empty() && !display_files.contains(active_file) {
                        display_files.push(active_file.clone());
                    }
                }
                if display_files.is_empty() {
                    empty_hint(
                        ui,
                        "No changes yet",
                        "Files the agent creates or edits in this chat appear here as it writes them.",
                        p,
                    );
                    return None;
                }

                let (added, deleted) = display_files.iter().fold((0, 0), |(a, d), f| {
                    let x = decor(f);
                    (a + x.added, d + x.deleted)
                });
                ui.horizontal(|ui| {
                    let n = display_files.len();
                    ui.label(
                        egui::RichText::new(format!(
                            "{n} file{} changed",
                            if n == 1 { "" } else { "s" }
                        ))
                        .size(12.0)
                        .color(p.dim),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        // Right-to-left: deletions first so they read "+a −d".
                        for (n, sign, color) in [(deleted, '-', p.danger), (added, '+', p.success)]
                        {
                            if n > 0 {
                                ui.label(
                                    egui::RichText::new(format!("{sign}{n}"))
                                        .font(egui::FontId::monospace(11.5))
                                        .color(color),
                                );
                            }
                        }
                    });
                });
                ui.add_space(4.0);

                let used = ui.cursor().top() - top;
                let mut clicked = None;
                egui::ScrollArea::vertical()
                    .id_salt("workspace_file_list")
                    .max_height((list_h - used).max(60.0))
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        for path in &display_files {
                            let row = change_row(
                                ui,
                                path,
                                decor(path),
                                selected.as_deref() == Some(path),
                                p,
                                time,
                            );
                            if row.on_hover_text(path.as_str()).clicked() {
                                clicked = Some(path.clone());
                            }
                        }
                    });
                clicked
            }
            RightTab::Browser | RightTab::Agents => None,
            RightTab::All => {
                style::search_field(ui, &mut self.file_filter, "Search files", p);
                if !self.expanded_dirs.is_empty() && self.file_filter.is_empty() {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if style::link_button(ui, "Collapse all", p).clicked() {
                            self.expanded_dirs.clear();
                        }
                    });
                }
                ui.add_space(6.0);
                if self.file_tree_dirty || self.file_tree_filter != self.file_filter {
                    let query = self.file_filter.to_lowercase();
                    self.file_tree = FileTree::build(
                        self.workspace_files
                            .iter()
                            .filter(|f| query.is_empty() || f.to_lowercase().contains(&query)),
                    );
                    self.file_tree_filter = self.file_filter.clone();
                    self.file_tree_dirty = false;
                }
                if self.is_loading_files && self.workspace_files.is_empty() {
                    empty_hint(ui, "Scanning…", "", p);
                    return None;
                }
                if self.file_tree.is_empty() {
                    let hint = if self.file_filter.is_empty() {
                        "This folder has no files yet."
                    } else {
                        "Nothing matches that search."
                    };
                    empty_hint(ui, "No files", hint, p);
                    return None;
                }

                let used = ui.cursor().top() - top;
                let mut view = TreeView {
                    expanded: &mut self.expanded_dirs,
                    force_open: !self.file_filter.is_empty(),
                    selected: selected.as_deref(),
                    decor: &decor,
                    clicked: None,
                    p,
                    time,
                };
                let tree = &self.file_tree;
                egui::ScrollArea::vertical()
                    .id_salt("all_files_list")
                    .max_height((list_h - used).max(60.0))
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        draw_tree(ui, tree, "", 0, &mut view);
                    });
                view.clicked.take()
            }
        }
    }

    /// The selected file as a card: name and actions, view options, contents.
    fn draw_file_preview(&mut self, ui: &mut egui::Ui, p: &Palette, filename: &str) {
        let (dir, name) = filename.rsplit_once(['/', '\\']).unwrap_or(("", filename));
        let is_active_writing = self
            .current_writing_file
            .as_ref()
            .is_some_and(|(f, _, _)| f == filename);
        let writing_counts = self
            .current_writing_file
            .as_ref()
            .filter(|(f, _, _)| {
                f == filename && self.current_writing_status.as_deref() == Some("writing")
            })
            .map(|(_, a, d)| (*a, *d));

        egui::Frame::none()
            .fill(p.code_bg)
            .stroke(egui::Stroke::new(1.0_f32, p.border))
            .rounding(8.0)
            .show(ui, |ui| {
                egui::Frame::none()
                    .inner_margin(egui::Margin {
                        left: 10.0,
                        right: 4.0,
                        top: 6.0,
                        bottom: 8.0,
                    })
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if style::close_button(ui, p)
                                        .on_hover_text("Close preview")
                                        .clicked()
                                    {
                                        self.selected_file = None;
                                        self.inspected_file_content.clear();
                                        self.inspector_error = None;
                                    }
                                    if style::link_button(ui, "Open", p)
                                        .on_hover_text("Open in the default app")
                                        .clicked()
                                    {
                                        let _ =
                                            open::that(self.settings.project_root.join(filename));
                                    }
                                    if style::link_button(ui, "Copy path", p).clicked() {
                                        ui.ctx().copy_text(filename.to_string());
                                    }
                                    if is_raster_image(filename)
                                        && style::link_button(ui, "Copy image", p)
                                            .on_hover_text("Copy the picture to paste it elsewhere")
                                            .clicked()
                                    {
                                        let path = self.settings.project_root.join(filename);
                                        self.status_text = match copy_image_to_clipboard(&path) {
                                            Ok(()) => "Image copied".to_string(),
                                            Err(e) => format!("Couldn't copy the image: {e}"),
                                        };
                                    }
                                    ui.with_layout(
                                        egui::Layout::left_to_right(egui::Align::Center),
                                        |ui| {
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(name)
                                                        .font(style::strong(13.0))
                                                        .color(p.text),
                                                )
                                                .truncate(),
                                            )
                                            .on_hover_text(filename);
                                        },
                                    );
                                },
                            );
                        });

                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            if let Some((added, deleted)) = writing_counts {
                                style::dot(ui, p.accent);
                                ui.label(
                                    egui::RichText::new("Agent is writing")
                                        .size(11.5)
                                        .color(p.accent),
                                );
                                diff_counts(ui, added, deleted, p);
                            } else {
                                let location = if dir.is_empty() {
                                    "Project root".to_string()
                                } else {
                                    format!("{dir}/")
                                };
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(location).size(11.5).color(p.faint),
                                    )
                                    .truncate(),
                                );
                            }
                        });
                        ui.add_space(4.0);

                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            if style::chip(ui, "Lines", self.line_range_enabled, p)
                                .on_hover_text("Show only a range of lines")
                                .clicked()
                            {
                                self.line_range_enabled = !self.line_range_enabled;
                                if self.line_range_enabled && self.line_range_end == 0 {
                                    self.line_range_end = self.line_range_total_lines.min(100);
                                }
                                self.inspect_file(filename);
                            }
                            if self.line_range_enabled {
                                let max = self.line_range_total_lines.max(1);
                                let mut start = self.line_range_start;
                                if ui
                                    .add(egui::DragValue::new(&mut start).range(1..=max))
                                    .changed()
                                {
                                    self.line_range_start = start;
                                    self.inspect_file(filename);
                                }
                                ui.label(egui::RichText::new("to").size(11.5).color(p.faint));
                                let mut end = self.line_range_end;
                                if ui
                                    .add(egui::DragValue::new(&mut end).range(1..=max))
                                    .changed()
                                {
                                    self.line_range_end = end;
                                    self.inspect_file(filename);
                                }
                                ui.label(
                                    egui::RichText::new(format!(
                                        "of {}",
                                        self.line_range_total_lines
                                    ))
                                    .size(11.5)
                                    .color(p.faint),
                                );
                            }
                            if style::chip(ui, "Edit", self.manual_edits_enabled, p)
                                .on_hover_text("Edit this file by hand")
                                .clicked()
                            {
                                self.manual_edits_enabled = !self.manual_edits_enabled;
                                if !self.manual_edits_enabled {
                                    self.inspected_file_edited = false;
                                    self.inspect_file(filename);
                                }
                            }
                            if self.manual_edits_enabled {
                                let save = if self.inspected_file_edited {
                                    style::primary_button(ui, "Save", p)
                                } else {
                                    style::secondary_button(ui, "Save", p)
                                };
                                if save.clicked() {
                                    if let Err(e) = self.save_inspected_file_changes() {
                                        self.inspector_error = Some(format!("Failed to save: {e}"));
                                    } else {
                                        self.status_text = "Saved!".to_string();
                                    }
                                }
                            }
                        });
                    });

                ui.painter().hline(
                    ui.max_rect().x_range(),
                    ui.cursor().top(),
                    egui::Stroke::new(1.0_f32, p.border),
                );

                if let Some(error) = &self.inspector_error {
                    egui::Frame::none()
                        .inner_margin(egui::Margin::symmetric(10.0, 10.0))
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                style::dot(ui, p.danger);
                                ui.label(egui::RichText::new(error).size(12.0).color(p.dim));
                            });
                        });
                    return;
                }

                egui::Frame::none()
                    .inner_margin(egui::Margin {
                        left: 0.0,
                        right: 4.0,
                        top: 4.0,
                        bottom: 4.0,
                    })
                    .show(ui, |ui| {
                        if self.manual_edits_enabled {
                            egui::ScrollArea::both()
                                .id_salt("inline_file_inspector")
                                .auto_shrink([false, false])
                                .stick_to_bottom(is_active_writing)
                                .show(ui, |ui| {
                                    ui.add_space(2.0);
                                    let edit = ui.add(
                                        egui::TextEdit::multiline(&mut self.inspected_file_content)
                                            .font(egui::TextStyle::Monospace)
                                            .frame(false)
                                            .margin(egui::Margin::symmetric(10.0, 2.0))
                                            .desired_width(f32::INFINITY)
                                            .interactive(true),
                                    );
                                    if edit.changed() {
                                        self.inspected_file_edited = true;
                                    }
                                });
                        } else {
                            self.draw_code_lines(ui, p, is_active_writing);
                        }
                    });
            });
    }

    /// Read-only file contents with a line-number gutter. Only the visible rows
    /// are laid out, so long files scroll smoothly.
    fn draw_code_lines(&self, ui: &mut egui::Ui, p: &Palette, stick_to_bottom: bool) {
        let lines: Vec<&str> = self.inspected_file_content.lines().collect();
        if lines.is_empty() {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.label(egui::RichText::new("Empty file").size(12.0).color(p.faint));
            });
            return;
        }
        let first_line = if self.line_range_enabled {
            self.line_range_start
        } else {
            1
        };
        let mono = egui::FontId::monospace(11.5);
        let row_h = ui.fonts(|f| f.row_height(&mono)) + 3.0;
        let digits = (first_line + lines.len()).to_string().len().max(2);
        let gutter = digits as f32 * ui.fonts(|f| f.glyph_width(&mono, '0')) + 22.0;

        let active_range = self.get_active_editing_range();
        let is_active_file = self.selected_file.is_some()
            && self.selected_file.as_ref() == self.current_writing_file.as_ref().map(|(n, _, _)| n);

        ui.spacing_mut().item_spacing.y = 0.0;
        // Measured outside: a two-way scroll area reports unlimited width inside.
        let view_w = ui.available_width();
        let mut area = egui::ScrollArea::both()
            .id_salt("inline_file_inspector")
            .auto_shrink([false, false])
            .stick_to_bottom(stick_to_bottom);
        // Bring the range the agent is editing into view, once per change.
        if let (true, Some((start, _))) = (is_active_file, active_range) {
            if self.should_scroll_to_line == Some(start)
                && (first_line..first_line + lines.len()).contains(&start)
            {
                let y = (start - first_line) as f32 * row_h - ui.available_height() / 2.0;
                area = area.vertical_scroll_offset(y.max(0.0));
            }
        }
        area.show_rows(ui, row_h, lines.len(), |ui, rows| {
            for idx in rows {
                let line_num = first_line + idx;
                let highlighted = is_active_file
                    && active_range
                        .is_some_and(|(start, end)| line_num >= start && line_num <= end);
                let galley = ui.painter().layout_no_wrap(
                    lines[idx].replace('\t', "    "),
                    mono.clone(),
                    p.text,
                );
                let width = (gutter + galley.size().x + 12.0).max(view_w);
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(width, row_h), egui::Sense::hover());
                let painter = ui.painter();
                if highlighted {
                    painter.rect_filled(rect, 0.0, p.accent_soft);
                }
                painter.text(
                    egui::pos2(rect.left() + gutter - 12.0, rect.center().y),
                    egui::Align2::RIGHT_CENTER,
                    line_num.to_string(),
                    mono.clone(),
                    if highlighted { p.accent } else { p.faint },
                );
                painter.galley(
                    egui::pos2(
                        rect.left() + gutter,
                        rect.center().y - galley.size().y / 2.0,
                    ),
                    galley,
                    p.text,
                );
            }
        });
    }

    fn draw_composer(&mut self, ctx: &egui::Context, p: &Palette) {
        egui::TopBottomPanel::bottom("bottom_input_panel")
            .frame(egui::Frame::none().fill(p.bg).inner_margin(egui::Margin {
                left: 16.0,
                right: 16.0,
                top: 6.0,
                bottom: 12.0,
            }))
            .show_separator_line(false)
            .resizable(false)
            .show(ctx, |ui| {
                let full = ui.available_width();
                let col = full.min(style::MAX_COLUMN);
                let pad = ((full - col) / 2.0).max(0.0);
                egui::Frame::none()
                    .inner_margin(egui::Margin {
                        left: pad,
                        right: pad,
                        top: 0.0,
                        bottom: 0.0,
                    })
                    .show(ui, |ui| {
                        ui.set_width(col);
                        self.draw_composer_column(ui, p);
                    });
            });
    }

    fn draw_composer_column(&mut self, ui: &mut egui::Ui, p: &Palette) {
        if self.current_session_id.is_some() {
            ui.horizontal(|ui| {
                style::dot(ui, p.accent);
                ui.label(
                    egui::RichText::new("Viewing a saved chat. New messages continue it.")
                        .size(12.0)
                        .color(p.dim),
                );
            });
            ui.add_space(4.0);
        }

        // Tool approval card, right above the message box
        let mut user_response = None;
        if let Some(approval) = &self.approval_pending {
            let asking_agent = self
                .approval_agent
                .as_deref()
                .map(|id| self.subagent_name(id).unwrap_or(id).to_string());
            let tool_name = approval.tool_name.clone();
            let args_preview = approval.args_preview.clone();

            let mut target_summary = String::new();
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&args_preview) {
                if let Some(path) = v
                    .get("path")
                    .or_else(|| v.get("TargetFile"))
                    .and_then(|x| x.as_str())
                {
                    target_summary = format!("File: {path}");
                } else if let Some(cmd) = v.get("CommandLine").and_then(|x| x.as_str()) {
                    target_summary = format!("Command: {cmd}");
                } else if let Some(url) = v.get("Url").and_then(|x| x.as_str()) {
                    target_summary = format!("URL: {url}");
                } else if let Some(obj) = v.as_object() {
                    let mut parts = Vec::new();
                    for (k, val) in obj {
                        if k != "content"
                            && k != "ReplacementContent"
                            && k != "TargetContent"
                            && k != "CodeContent"
                        {
                            let val_str = val.to_string();
                            if val_str.len() > 60 {
                                parts.push(format!(
                                    "{}: {}",
                                    k,
                                    crate::util::truncate_chars(&val_str, 60)
                                ));
                            } else {
                                parts.push(format!("{}: {}", k, val_str));
                            }
                        }
                    }
                    target_summary = parts.join(", ");
                }
            }
            if target_summary.is_empty() {
                target_summary = args_preview.clone();
            }
            if target_summary.len() > 120 {
                target_summary = crate::util::truncate_chars(&target_summary, 120);
            }

            egui::Frame::none()
                .fill(p.surface)
                .stroke(egui::Stroke::new(1.0_f32, p.warn.gamma_multiply(0.7)))
                .rounding(10.0)
                .inner_margin(egui::Margin::same(12.0))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        style::dot(ui, p.warn);
                        let question = match asking_agent.as_deref() {
                            Some(name) => format!("Sub-agent '{name}' wants to run a tool"),
                            None => "Allow this tool call?".to_string(),
                        };
                        ui.label(
                            egui::RichText::new(question)
                                .font(style::strong(13.0))
                                .color(p.text),
                        );
                        ui.label(
                            egui::RichText::new(&tool_name)
                                .font(egui::FontId::monospace(12.5))
                                .color(p.dim),
                        );
                    });
                    ui.add_space(4.0);
                    egui::Frame::none()
                        .fill(p.code_bg)
                        .rounding(6.0)
                        .inner_margin(egui::Margin::symmetric(8.0, 6.0))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&target_summary)
                                        .font(egui::FontId::monospace(12.0))
                                        .color(p.text),
                                )
                                .wrap(),
                            );
                        });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if style::primary_button(ui, "Allow", p).clicked() {
                            user_response = Some(true);
                        }
                        if style::secondary_button(ui, "Deny", p).clicked() {
                            user_response = Some(false);
                        }
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new("Enter to allow · Esc to deny")
                                .size(11.5)
                                .color(p.faint),
                        );
                    });
                });
            ui.add_space(8.0);
        }

        if let Some(approved) = user_response {
            self.answer_approval(approved);
        }

        // Attachment chips
        let mut remove_attachment = None;
        let mut clear_attachments = false;
        if !self.attachments.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                for (idx, path) in self.attachments.iter().enumerate() {
                    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
                    egui::Frame::none()
                        .fill(p.surface)
                        .stroke(egui::Stroke::new(1.0_f32, p.border))
                        .rounding(6.0)
                        .inner_margin(egui::Margin::symmetric(8.0, 2.0))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                ui.label(egui::RichText::new(filename).size(12.0).color(p.text))
                                    .on_hover_text(path.to_string_lossy());
                                if style::link_button(ui, "×", p)
                                    .on_hover_text("Remove attachment")
                                    .clicked()
                                {
                                    remove_attachment = Some(idx);
                                }
                            });
                        });
                }
                if self.attachments.len() > 1 && style::link_button(ui, "Remove all", p).clicked() {
                    clear_attachments = true;
                }
            });
            ui.add_space(4.0);
        }
        if let Some(idx) = remove_attachment {
            self.attachments.remove(idx);
        }
        if clear_attachments {
            self.attachments.clear();
        }

        // Message box
        let composer_id = egui::Id::new("composer_input");
        let focused = ui.memory(|m| m.has_focus(composer_id));

        // `/` commands: suggestions above the box while a command is typed.
        let suggestions = slash_suggestions(&self.input_text);
        if !suggestions.is_empty() {
            let count = suggestions.len();
            let selected = self.shell.slash_selected.min(count - 1);
            let mut complete = None;
            if focused {
                ui.input_mut(|i| {
                    if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
                        self.shell.slash_selected = selected.saturating_sub(1);
                    }
                    if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
                        self.shell.slash_selected = (selected + 1).min(count - 1);
                    }
                    let exact = suggestions[selected].0 == self.input_text.trim();
                    if i.consume_key(egui::Modifiers::NONE, egui::Key::Tab)
                        || (!exact && i.consume_key(egui::Modifiers::NONE, egui::Key::Enter))
                    {
                        complete = Some(suggestions[selected].0);
                    }
                });
            }
            let selected = self.shell.slash_selected.min(count - 1);
            style::card(p).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 2.0;
                for (i, (command, about)) in suggestions.iter().enumerate() {
                    let (rect, resp) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 26.0),
                        egui::Sense::click(),
                    );
                    if i == selected || resp.hovered() {
                        ui.painter().rect_filled(
                            rect,
                            6.0,
                            if i == selected {
                                p.accent_soft
                            } else {
                                p.hover
                            },
                        );
                    }
                    ui.painter().text(
                        egui::pos2(rect.left() + 10.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        command,
                        egui::FontId::monospace(12.5),
                        if i == selected { p.accent } else { p.text },
                    );
                    ui.painter().text(
                        egui::pos2(rect.left() + 130.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        about,
                        egui::FontId::proportional(12.0),
                        p.faint,
                    );
                    if resp.clicked() {
                        complete = Some(command);
                    }
                }
                ui.label(
                    egui::RichText::new("↑↓ choose · Tab or Enter to complete")
                        .size(10.5)
                        .color(p.faint),
                );
            });
            ui.add_space(6.0);
            if let Some(command) = complete {
                self.input_text = format!("{command} ");
                self.shell.slash_selected = 0;
                self.focus_composer = true;
                // Put the cursor after the command.
                let mut state =
                    egui::text_edit::TextEditState::load(ui.ctx(), composer_id).unwrap_or_default();
                let end = egui::text::CCursor::new(self.input_text.chars().count());
                state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::one(end)));
                state.store(ui.ctx(), composer_id);
            }
        } else {
            self.shell.slash_selected = 0;
        }

        let glow = ui
            .ctx()
            .animate_bool_with_time(composer_id.with("glow"), focused, 0.15);
        egui::Frame::none()
            .fill(p.surface)
            .stroke(egui::Stroke::new(
                1.0_f32,
                egui::Color32::from(egui::lerp(
                    egui::Rgba::from(p.border)..=egui::Rgba::from(p.accent.gamma_multiply(0.6)),
                    glow,
                )),
            ))
            .shadow(egui::epaint::Shadow {
                offset: egui::vec2(0.0, 4.0),
                blur: 18.0,
                spread: 0.0,
                color: egui::Color32::from_black_alpha(if self.dark_mode { 70 } else { 18 }),
            })
            .rounding(14.0)
            .inner_margin(egui::Margin {
                left: 14.0,
                right: 8.0,
                top: 11.0,
                bottom: 7.0,
            })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let mut edit_response_opt = None;
                egui::ScrollArea::vertical()
                    .max_height(180.0)
                    .id_salt("prompt_input_scroll")
                    .show(ui, |ui| {
                        let hint = if self.is_processing {
                            "Queue a follow-up…"
                        } else {
                            "Message Norvexum…"
                        };
                        let resp = ui.add(
                            egui::TextEdit::multiline(&mut self.input_text)
                                .id(composer_id)
                                .hint_text(egui::RichText::new(hint).color(p.faint))
                                .text_color(p.text)
                                .font(egui::FontId::proportional(14.0))
                                .desired_rows(1)
                                .margin(egui::vec2(0.0, 2.0))
                                .frame(false)
                                .desired_width(f32::INFINITY),
                        );
                        edit_response_opt = Some(resp);
                    });

                let edit_response = edit_response_opt.unwrap();
                if std::mem::take(&mut self.focus_composer) {
                    edit_response.request_focus();
                }

                // Up in an empty box recalls the last prompt.
                if edit_response.has_focus()
                    && self.input_text.is_empty()
                    && ui.input(|i| i.key_pressed(egui::Key::ArrowUp))
                {
                    if let Some(last) = self.messages.iter().rev().find(|m| m.role == "user") {
                        self.input_text = last.content.clone();
                    }
                }

                if edit_response.has_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift)
                {
                    self.send_message();
                    edit_response.request_focus();
                }

                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.style_mut().spacing.interact_size.y = 24.0;
                    let attach = style::icon_button(ui, style::Icon::Clip, 28.0, false, p)
                        .on_hover_text("Attach files  (or drop them here, or paste with Ctrl+V)");
                    if attach.clicked() {
                        if let Some(paths) = rfd::FileDialog::new().pick_files() {
                            self.add_attachments(paths);
                        }
                    }
                    attach.context_menu(|ui| {
                        if ui.button("Paste image or files").clicked() {
                            self.paste_clipboard_media(true);
                            ui.close_menu();
                        }
                    });

                    let registry = build_registry();
                    let mut current_provider_idx = registry
                        .iter()
                        .position(|p| p.name == self.settings.active_provider)
                        .unwrap_or(0);

                    egui::ComboBox::from_id_salt("provider_select_bottom")
                        .selected_text(
                            egui::RichText::new(&registry[current_provider_idx].display_name)
                                .size(12.0)
                                .color(p.dim),
                        )
                        .width(120.0)
                        .show_ui(ui, |ui| {
                            for (idx, provider) in registry.iter().enumerate() {
                                if ui.selectable_value(&mut current_provider_idx, idx, &provider.display_name).clicked() {
                                    self.settings.active_provider = provider.name.clone();
                                    if let Some(first_model) = provider.models.first() {
                                        self.settings.active_model = first_model.id.clone();
                                    }
                                    self.sync_settings();
                                }
                            }
                        });

                    let active_provider = &registry[current_provider_idx];
                    let mut model_display = self.settings.active_model.clone();
                    if model_display.chars().count() > 30 {
                        model_display = crate::util::truncate_chars(&model_display, 27);
                    }

                    let popup_id = ui.make_persistent_id("model_search_popup");
                    let button_res = ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new(model_display).size(12.0).color(p.dim),
                            )
                            .fill(egui::Color32::TRANSPARENT)
                            .stroke(egui::Stroke::new(1.0_f32, p.border))
                            .rounding(6.0),
                        )
                        .on_hover_text("Choose a model");
                    if button_res.clicked() {
                        ui.memory_mut(|mem| mem.toggle_popup(popup_id));
                    }
                    egui::popup::popup_above_or_below_widget(ui, popup_id, &button_res, egui::AboveOrBelow::Above, egui::PopupCloseBehavior::CloseOnClickOutside, |ui| {
                        ui.set_min_width(320.0);
                        ui.set_min_height(250.0); // Prevent the Above popup bug where it gets stuck at a small height
                        ui.add(
                            egui::TextEdit::singleline(&mut self.model_search_query)
                                .hint_text("Search models")
                                .desired_width(f32::INFINITY),
                        );
                        ui.add_space(4.0);
                        egui::ScrollArea::vertical().max_height(250.0).show(ui, |ui| {
                            let mut starred = Vec::new();
                            let mut unstarred = Vec::new();
                            let query = self.model_search_query.to_lowercase();
                            for model in &active_provider.models {
                                if !query.is_empty() && !model.id.to_lowercase().contains(&query) {
                                    continue;
                                }
                                if self.settings.starred_models.contains(&model.id) {
                                    starred.push(model);
                                } else {
                                    unstarred.push(model);
                                }
                            }
                            if starred.is_empty() && unstarred.is_empty() {
                                ui.label(egui::RichText::new("No models match").size(12.5).color(p.faint));
                            }

                            let mut render_model = |ui: &mut egui::Ui, model: &crate::config::providers::ModelInfo| {
                                ui.horizontal(|ui| {
                                    let is_starred = self.settings.starred_models.contains(&model.id);
                                    let star_icon = if is_starred { "⭐" } else { "☆" };
                                    let star_color = if is_starred { p.warn } else { p.faint };
                                    if ui
                                        .add(egui::Button::new(egui::RichText::new(star_icon).color(star_color)).frame(false))
                                        .on_hover_text(if is_starred { "Unpin" } else { "Pin to top" })
                                        .clicked()
                                    {
                                        if is_starred {
                                            self.settings.starred_models.retain(|id| id != &model.id);
                                        } else {
                                            self.settings.starred_models.push(model.id.clone());
                                        }
                                        self.sync_settings();
                                    }

                                    if ui
                                        .selectable_value(
                                            &mut self.settings.active_model,
                                            model.id.clone(),
                                            egui::RichText::new(&model.id).size(12.5),
                                        )
                                        .clicked()
                                    {
                                        self.sync_settings();
                                        ui.memory_mut(|mem| mem.close_popup());
                                    }

                                    // Capability badges
                                    let mut badges = Vec::new();
                                    if matches!(model.vision, crate::config::providers::Vision::Yes) {
                                        badges.push("vision".to_string());
                                    }
                                    if model.tool_calling {
                                        badges.push("tools".to_string());
                                    }
                                    let ctx_k = model.context_window / 1000;
                                    if ctx_k > 0 {
                                        badges.push(format!("{ctx_k}k"));
                                    }
                                    if !badges.is_empty() {
                                        ui.label(
                                            egui::RichText::new(badges.join(" · "))
                                                .size(11.0)
                                                .color(p.faint),
                                        );
                                    }
                                });
                            };

                            if !starred.is_empty() {
                                for model in starred { render_model(ui, model); }
                                ui.separator();
                            }
                            for model in unstarred { render_model(ui, model); }
                        });
                    });

                    if self.settings.active_provider.to_lowercase().contains("anthropic") {
                        if let Some(model) = active_provider.models.iter().find(|m| m.id == self.settings.active_model) {
                            if !model.effort_levels.is_empty() || model.adaptive_thinking {
                                let current_effort = self.settings.thinking_effort.clone().unwrap_or_else(|| "none".to_string());
                                let label = if current_effort == "none" {
                                    "Reasoning: off".to_string()
                                } else {
                                    format!("Reasoning: {}", current_effort)
                                };
                                egui::ComboBox::from_id_salt("thinking_select_bottom")
                                    .selected_text(egui::RichText::new(label).size(12.0).color(p.dim))
                                    .width(120.0)
                                    .show_ui(ui, |ui| {
                                        if ui.selectable_value(&mut self.settings.thinking_effort, Some("none".to_string()), "Off").clicked() {
                                            self.sync_settings();
                                        }
                                        for lvl in &model.effort_levels {
                                            if ui.selectable_value(&mut self.settings.thinking_effort, Some(lvl.clone()), lvl).clicked() {
                                                self.sync_settings();
                                            }
                                        }
                                        if model.adaptive_thinking
                                            && ui.selectable_value(&mut self.settings.thinking_effort, Some("adaptive".to_string()), "Adaptive").clicked() {
                                                self.sync_settings();
                                            }
                                    });
                            }
                        }
                    } else if self.probing_in_progress {
                        ui.label(egui::RichText::new("Checking model…").size(11.5).color(p.faint));
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Attachments alone are enough to send.
                        let has_text =
                            !self.input_text.trim().is_empty() || !self.attachments.is_empty();
                        let send_hint = if self.is_processing {
                            "Queue this message  (Enter)"
                        } else {
                            "Send  (Enter)"
                        };
                        if send_button(ui, has_text, p).on_hover_text(send_hint).clicked() && has_text {
                            self.send_message();
                            edit_response.request_focus();
                        }
                        if self.is_processing && stop_button(ui, p).clicked() {
                            self.stop_run();
                        }
                        if ui.available_width() > 230.0 {
                            ui.label(
                                egui::RichText::new("Shift+Enter for a new line")
                                    .size(11.0)
                                    .color(p.faint),
                            );
                        }
                    });
                });
            });
    }

    fn draw_empty_state(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let avail = ui.available_height();
        ui.add_space((avail * 0.22).clamp(24.0, 170.0));
        ui.vertical_centered(|ui| {
            ui.add(egui::Image::new(&self.logo_tex).fit_to_exact_size(egui::vec2(48.0, 48.0)));
            ui.add_space(14.0);
            ui.label(
                egui::RichText::new("What should we build?")
                    .font(style::strong(22.0))
                    .color(p.text),
            );
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new(format!("Working in {}", self.project_name()))
                    .size(13.0)
                    .color(p.dim),
            );
            ui.add_space(22.0);
            let w = ui.available_width().min(400.0);
            for suggestion in SUGGESTIONS {
                let resp = ui.add_sized(
                    [w, 34.0],
                    egui::Button::new(egui::RichText::new(*suggestion).size(13.0).color(p.dim))
                        .fill(p.surface)
                        .stroke(egui::Stroke::new(1.0_f32, p.border))
                        .rounding(8.0),
                );
                if resp.clicked() {
                    self.input_text = suggestion.to_string();
                    self.focus_composer = true;
                }
            }
            for msg in &self.messages {
                if msg.content.contains("🧹") || msg.content.contains("cleared") {
                    ui.add_space(14.0);
                    ui.label(
                        egui::RichText::new(style::clean_label(&msg.content))
                            .size(12.0)
                            .color(p.faint),
                    );
                }
            }
            ui.add_space(18.0);
            ui.label(
                egui::RichText::new("Drop a folder here to open it  ·  Ctrl+/ for shortcuts")
                    .size(11.5)
                    .color(p.faint),
            );
        });
    }

    fn draw_shortcuts(&mut self, ctx: &egui::Context, p: &Palette) {
        let mut open = self.show_shortcuts;
        egui::Window::new(egui::RichText::new("Keyboard shortcuts").font(style::strong(15.0)))
            .id(egui::Id::new("shortcuts_window"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.screen_rect().center())
            .show(ctx, |ui| {
                egui::Grid::new("shortcut_grid")
                    .num_columns(2)
                    .spacing([28.0, 8.0])
                    .show(ui, |ui| {
                        for (keys, action) in SHORTCUTS {
                            ui.label(
                                egui::RichText::new(*keys)
                                    .font(egui::FontId::monospace(12.0))
                                    .color(p.text),
                            );
                            ui.label(egui::RichText::new(*action).size(12.5).color(p.dim));
                            ui.end_row();
                        }
                    });
            });
        self.show_shortcuts = open;
    }

    /// Dims everything behind a dialog; returns true when it was clicked.
    fn draw_modal_backdrop(&self, ctx: &egui::Context) -> bool {
        let screen = ctx.screen_rect();
        let mut clicked = false;
        egui::Area::new(egui::Id::new("modal_backdrop"))
            // Above the panels, below dialog windows.
            .order(egui::Order::PanelResizeLine)
            .fixed_pos(screen.min)
            .show(ctx, |ui| {
                let (rect, resp) = ui.allocate_exact_size(screen.size(), egui::Sense::click());
                ui.painter().rect_filled(
                    rect,
                    0.0,
                    egui::Color32::from_black_alpha(if self.dark_mode { 120 } else { 70 }),
                );
                clicked = resp.clicked();
            });
        clicked
    }

    fn draw_drop_overlay(&self, ctx: &egui::Context) {
        let (total, dirs) = ctx.input(|i| {
            let files = &i.raw.hovered_files;
            let dirs = files
                .iter()
                .filter(|f| f.path.as_ref().is_some_and(|p| p.is_dir()))
                .count();
            (files.len(), dirs)
        });
        if total == 0 {
            return;
        }
        // Mirrors the drop handling in `update`.
        let message = match total - dirs {
            0 => "Drop to open this folder as the workspace".to_string(),
            1 => "Drop to attach".to_string(),
            n => format!("Drop to attach {n} files"),
        };
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("drop_overlay"),
        ));
        let rect = ctx.screen_rect();
        painter.rect_filled(rect, 0.0, egui::Color32::from_black_alpha(140));
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            message,
            style::strong(18.0),
            egui::Color32::WHITE,
        );
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.ctx.is_none() {
            self.ctx = Some(ctx.clone());
        }
        // eframe shows the window only after its first frame, and Windows then
        // leaves it inactive: the first click merely activated it. Take focus
        // while it appears so it is usable (and movable) straight away.
        if ctx.cumulative_pass_nr() < 4 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        // Maximizing a window that is still hidden (eframe shows it after the
        // first frame) left the renderer at the old size until the window was
        // moved: a strip stayed undrawn. Maximize once it is on screen, which
        // resizes it like the user would.
        if ctx.cumulative_pass_nr() >= 2 {
            if let Some((maximized, fullscreen)) = self.startup_window_state.take() {
                if fullscreen {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
                } else if maximized {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
                }
            }
        }
        // Redraw for the first moments and on every size change.
        let size = ctx.input(|i| i.viewport().inner_rect.map(|r| r.size()));
        if ctx.input(|i| i.time) < 2.0 || size != self.last_window_size {
            self.last_window_size = size;
            ctx.request_repaint();
        }
        let active_range = self.get_active_editing_range();
        if let Some((start, end)) = active_range {
            if self.last_scrolled_range != Some((start, end)) {
                self.last_scrolled_range = Some((start, end));
                self.should_scroll_to_line = Some(start);
            }
        } else {
            self.last_scrolled_range = None;
        }

        let is_fs = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
        let is_max = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        let mut changed = false;
        // Until the startup state has been applied and settled, the window
        // is in between: don't save that.
        let settled = self.startup_window_state.is_none() && ctx.cumulative_pass_nr() > 10;
        if settled && is_fs != self.settings.fullscreen {
            self.settings.fullscreen = is_fs;
            changed = true;
        }
        if settled && is_max != self.settings.maximized {
            self.settings.maximized = is_max;
            changed = true;
        }
        if changed {
            self.sync_settings();
        }

        self.poll_agent_events(ctx);
        self.poll_file_events(ctx);
        self.poll_capabilities();

        // Dropped files are all attached; a folder dropped on its own becomes
        // the workspace.
        let dropped: Vec<std::path::PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped.is_empty() {
            let (dirs, files): (Vec<_>, Vec<_>) = dropped.into_iter().partition(|p| p.is_dir());
            if files.is_empty() {
                let path = &dirs[0];
                self.folder_input = path.to_string_lossy().to_string();
                self.load_project_folder(path, ctx.clone());
            } else {
                self.add_attachments(files);
                if !dirs.is_empty() {
                    self.status_text.push_str(" (folders can't be attached)");
                }
            }
        }

        self.handle_media_paste(ctx);

        if self.applied_theme != Some(self.dark_mode) {
            style::apply(ctx, &self.pal(), self.dark_mode);
            self.applied_theme = Some(self.dark_mode);
        }
        let p = self.pal();

        self.handle_shortcuts(ctx);

        // Panel order matters: the rail and the side panels run the full
        // height; the header, terminal and composer sit between them.
        self.draw_rail(ctx, &p);
        self.draw_left_sidebar(ctx, &p);
        self.draw_right_sidebar(ctx, &p);
        self.draw_chat_header(ctx, &p);
        if self.show_terminal_panel {
            self.draw_terminal(ctx, &p);
        }
        self.draw_composer(ctx, &p);
        self.draw_agents_strip(ctx, &p);
        self.draw_subagent_split(ctx, &p);
        if self.showing_subagent_alone() {
            self.draw_subagent_central(ctx, &p);
        } else {
            self.draw_conversation(ctx, &p);
        }

        if (self.show_settings_modal || self.show_shortcuts) && self.draw_modal_backdrop(ctx) {
            self.show_settings_modal = false;
            self.show_shortcuts = false;
        }
        if self.show_settings_modal {
            self.draw_settings_modal(ctx);
        }
        if self.show_shortcuts {
            self.draw_shortcuts(ctx, &p);
        }
        self.draw_command_palette(ctx, &p);
        self.draw_toasts(ctx, &p);
        self.draw_drop_overlay(ctx);

        self.should_scroll_to_line = None;
    }

    fn on_exit(&mut self) {
        println!("Shutting down GUI app cleanly...");
        // 1. Trigger Cancellation Token
        self.cancel_token.cancel();

        // 2. Kill spawned subprocesses tracked by agent (if any) or background process cleanly
        let _ = self.cancel_tx.send(());

        // 3. Grace sleep period
        std::thread::sleep(Duration::from_millis(200));

        // 4. Force cleanly exit the OS process
        std::process::exit(0);
    }
}

pub fn run_gui(config: GuiConfig) -> Result<(), eframe::Error> {
    let cancel_token = CancellationToken::new();
    let app_cancel_token = cancel_token.clone();

    // Decode logo to build taskbar and window titlebar icon
    let icon_image = image::load_from_memory(LOGO_BYTES).unwrap().to_rgba8();
    let (width, height) = icon_image.dimensions();
    let icon_data = egui::IconData {
        rgba: icon_image.into_raw(),
        width,
        height,
    };

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Norvexum")
            .with_active(true)
            .with_inner_size([1100.0, 750.0])
            .with_icon(icon_data),
        ..Default::default()
    };

    eframe::run_native(
        "Norvexum",
        native_options,
        Box::new(move |cc| Ok(Box::new(GuiApp::new(cc, config, app_cancel_token)))),
    )
}

fn get_file_diff_stats(project_root: &std::path::Path, file_path: &str) -> (i32, i32) {
    // Helper: run `git diff` with the given arguments and parse numstat output
    let try_git_diff = |extra_args: &[&str]| -> Option<(i32, i32)> {
        let mut args = vec!["diff", "--numstat"];
        args.extend_from_slice(extra_args);
        args.push("--");
        args.push(file_path);

        let output_result = spawn_silent_command("git", &args, project_root);

        if let Ok(output) = output_result {
            let out_str = String::from_utf8_lossy(&output.stdout);
            let parts: Vec<&str> = out_str.split_whitespace().collect();
            if parts.len() >= 2 {
                if let (Ok(add), Ok(del)) = (parts[0].parse::<i32>(), parts[1].parse::<i32>()) {
                    if add > 0 || del > 0 {
                        return Some((add, del));
                    }
                }
            }
        }
        None
    };

    // 1. Try unstaged changes (working tree vs index)
    if let Some(stats) = try_git_diff(&[]) {
        return stats;
    }

    // 2. Try vs HEAD (catches staged changes and edits to committed files)
    if let Some(stats) = try_git_diff(&["HEAD"]) {
        return stats;
    }

    // 3. Fallback for untracked/new files — count current lines as added
    let full_path = project_root.join(file_path);
    if let Ok(content) = std::fs::read_to_string(&full_path) {
        let lines = content.lines().count() as i32;
        return (lines, 0);
    }
    (0, 0)
}

/// Run an external command; on Windows, without flashing a console window.
/// This is the same helper the agent's `run_command` tool should use to
/// keep cmd.exe from popping up on every tool call.
fn spawn_silent_command(
    program: &str,
    args: &[&str],
    cwd: &std::path::Path,
) -> std::io::Result<std::process::Output> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args).current_dir(cwd);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW = 0x08000000 — prevents the cmd.exe black box
        // from briefly appearing and stealing focus when the bot runs a
        // shell command in the background.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    cmd.output()
}

fn get_relative_path(path: &str, project_root: &std::path::Path) -> String {
    let p = std::path::Path::new(path);
    let clean_p = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let clean_root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());

    let p_str = clean_p.to_string_lossy().replace("\\\\?\\", "");
    let root_str = clean_root.to_string_lossy().replace("\\\\?\\", "");

    let p_path = std::path::Path::new(&p_str);
    let root_path = std::path::Path::new(&root_str);

    if let Ok(rel) = p_path.strip_prefix(root_path) {
        rel.to_string_lossy().to_string()
    } else {
        let p_lower = p_str.to_lowercase();
        let root_lower = root_str.to_lowercase();
        if p_lower.starts_with(&root_lower) {
            let offset = root_lower.len();
            p_str[offset..].trim_start_matches(['/', '\\']).to_string()
        } else {
            path.to_string()
        }
    }
}

fn render_formatted_markdown(ui: &mut egui::Ui, text: &str, p: &Palette) {
    let sanitized_text = text.replace('\u{fe0f}', "");
    let mut lines = sanitized_text.lines().peekable();
    let body = style::BODY_SIZE;

    while let Some(line) = lines.next() {
        let trimmed = line.trim();

        // Fenced code block (an unterminated fence renders what has streamed so far)
        if trimmed.starts_with("```") {
            let lang = trimmed.trim_start_matches('`').trim().to_string();
            let mut code = Vec::new();
            for next in lines.by_ref() {
                if next.trim_start().starts_with("```") {
                    break;
                }
                code.push(next);
            }
            render_code_block(ui, &lang, &code.join("\n"), p);
            ui.add_space(6.0);
            continue;
        }

        // Table: collect all consecutive table lines
        if trimmed.starts_with('|') {
            let mut table_lines = vec![trimmed.to_string()];
            while let Some(next_line) = lines.peek() {
                let next_trimmed = next_line.trim();
                if next_trimmed.starts_with('|') {
                    table_lines.push(next_trimmed.to_string());
                    lines.next();
                } else {
                    break;
                }
            }
            render_markdown_table(ui, &table_lines, p);
            ui.add_space(6.0);
            continue;
        }

        // Horizontal rule
        if trimmed.len() >= 3
            && (trimmed.chars().all(|c| c == '-')
                || trimmed.chars().all(|c| c == '*')
                || trimmed.chars().all(|c| c == '_'))
        {
            ui.add_space(4.0);
            ui.separator();
            ui.add_space(4.0);
            continue;
        }

        // Headings
        let heading = [("### ", 14.5, 4.0), ("## ", 16.5, 6.0), ("# ", 19.0, 8.0)]
            .into_iter()
            .find_map(|(prefix, size, space)| {
                trimmed
                    .strip_prefix(prefix)
                    .map(|h| (h.trim(), size, space))
            });
        if let Some((heading_text, size, space)) = heading {
            ui.add_space(space);
            ui.horizontal_wrapped(|ui| {
                ui.style_mut().spacing.item_spacing.x = 0.0;
                render_styled_line_parts(ui, heading_text, p.text, size, true, p);
            });
            ui.add_space(2.0);
            continue;
        }

        // Block quote
        if let Some(quote) = trimmed
            .strip_prefix("> ")
            .or_else(|| trimmed.strip_prefix('>'))
        {
            let response = ui
                .horizontal_wrapped(|ui| {
                    ui.add_space(12.0);
                    ui.style_mut().spacing.item_spacing.x = 0.0;
                    render_styled_line_parts(ui, quote.trim(), p.dim, body, false, p);
                })
                .response;
            let r = response.rect;
            ui.painter().line_segment(
                [
                    r.left_top() + egui::vec2(3.0, 1.0),
                    r.left_bottom() + egui::vec2(3.0, -1.0),
                ],
                egui::Stroke::new(2.0_f32, p.border),
            );
            continue;
        }

        let indent = (line.len() - line.trim_start().len()).min(12) as f32 * 5.0;

        // Bullet point
        if let Some(content) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))
        {
            ui.horizontal_wrapped(|ui| {
                ui.style_mut().spacing.item_spacing.x = 0.0;
                ui.add_space(6.0 + indent);
                ui.label(egui::RichText::new("•   ").size(body).color(p.faint));
                render_styled_line_parts(ui, content, p.text, body, false, p);
            });
            continue;
        }

        // Ordered list: "12. item"
        let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
        if (1..=3).contains(&digits) {
            if let Some(content) = trimmed[digits..].strip_prefix(". ") {
                ui.horizontal_wrapped(|ui| {
                    ui.style_mut().spacing.item_spacing.x = 0.0;
                    ui.add_space(6.0 + indent);
                    ui.label(
                        egui::RichText::new(format!("{}.  ", &trimmed[..digits]))
                            .size(body)
                            .color(p.dim),
                    );
                    render_styled_line_parts(ui, content, p.text, body, false, p);
                });
                continue;
            }
        }

        // Plain line
        if !trimmed.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.style_mut().spacing.item_spacing.x = 0.0;
                render_styled_line_parts(ui, trimmed, p.text, body, false, p);
            });
        } else {
            ui.add_space(6.0);
        }
    }
}

fn render_code_block(ui: &mut egui::Ui, lang: &str, code: &str, p: &Palette) {
    egui::Frame::none()
        .fill(p.code_bg)
        .stroke(egui::Stroke::new(1.0_f32, p.border))
        .rounding(8.0)
        .inner_margin(egui::Margin::symmetric(12.0, 8.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(if lang.is_empty() { "code" } else { lang })
                        .size(11.0)
                        .color(p.faint),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if style::link_button(ui, "Copy", p)
                        .on_hover_text("Copy code")
                        .clicked()
                    {
                        ui.ctx().copy_text(code.to_string());
                    }
                });
            });
            egui::ScrollArea::horizontal()
                .id_salt(ui.next_auto_id())
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(code)
                                .font(egui::FontId::monospace(12.5))
                                .color(p.text),
                        )
                        .extend(),
                    );
                });
        });
}

fn render_styled_line_parts(
    ui: &mut egui::Ui,
    text: &str,
    text_color: egui::Color32,
    size: f32,
    force_bold: bool,
    p: &Palette,
) {
    let mut parts = vec![(text.to_string(), false, false)];

    let mut temp_parts = Vec::new();
    for (part, is_bold, is_code) in parts {
        if is_code {
            temp_parts.push((part, is_bold, is_code));
            continue;
        }
        let split: Vec<&str> = part.split("**").collect();
        for (idx, sub) in split.into_iter().enumerate() {
            if !sub.is_empty() {
                temp_parts.push((sub.to_string(), idx % 2 == 1, false));
            }
        }
    }
    parts = temp_parts;

    let mut temp_parts = Vec::new();
    for (part, is_bold, is_code) in parts {
        if is_bold {
            temp_parts.push((part, is_bold, is_code));
            continue;
        }
        let split: Vec<&str> = part.split('`').collect();
        for (idx, sub) in split.into_iter().enumerate() {
            if !sub.is_empty() {
                temp_parts.push((sub.to_string(), false, idx % 2 == 1));
            }
        }
    }
    parts = temp_parts;

    for (part, is_bold, is_code) in parts {
        let text_item = if is_code {
            egui::RichText::new(&part)
                .font(egui::FontId::monospace(size - 1.0))
                .color(p.code_text)
                .background_color(p.code_bg)
        } else if force_bold || is_bold {
            egui::RichText::new(&part)
                .font(style::strong(size))
                .color(text_color)
        } else {
            egui::RichText::new(&part).size(size).color(text_color)
        };
        ui.add(egui::Label::new(text_item));
    }
}

fn render_markdown_table(ui: &mut egui::Ui, lines: &[String], p: &Palette) {
    let is_separator = |line: &str| {
        line.chars()
            .all(|c| c == '-' || c == '|' || c == ':' || c == ' ' || c == '\t')
    };

    let mut rows: Vec<Vec<String>> = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut cells: Vec<String> = trimmed.split('|').map(|s| s.trim().to_string()).collect();
        if cells.len() > 1 {
            if cells[0].is_empty() {
                cells.remove(0);
            }
            if !cells.is_empty() && cells[cells.len() - 1].is_empty() {
                cells.pop();
            }
        }
        if cells.is_empty() {
            continue;
        }

        let joined = cells.join("");
        if is_separator(&joined) {
            continue;
        }
        rows.push(cells);
    }

    if rows.is_empty() {
        return;
    }

    // Normalize column count across all rows so the grid doesn't warp.
    let max_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if max_cols == 0 {
        return;
    }
    for row in &mut rows {
        while row.len() < max_cols {
            row.push(String::new());
        }
    }

    // Per-column width so columns line up; capped so one giant cell can't
    // blow up the layout.
    let mut col_widths = vec![0.0_f32; max_cols];
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            let w = (cell.chars().count() as f32 * 7.2).max(40.0);
            if w > col_widths[i] {
                col_widths[i] = w;
            }
        }
    }
    for w in &mut col_widths {
        *w = w.min(320.0);
    }

    let total_width: f32 = col_widths.iter().sum::<f32>() + (max_cols as f32) * 24.0;

    egui::Frame::none()
        .stroke(egui::Stroke::new(1.0_f32, p.border))
        .rounding(8.0)
        .show(ui, |ui| {
            ui.set_min_width(total_width.min(ui.available_width()));
            egui::Grid::new(ui.next_auto_id())
                .num_columns(max_cols)
                .spacing([0.0, 0.0])
                .min_col_width(40.0)
                .show(ui, |ui| {
                    for (i, cell) in rows[0].iter().enumerate() {
                        let w = col_widths[i];
                        egui::Frame::none()
                            .fill(p.panel)
                            .inner_margin(egui::Margin::symmetric(12.0, 8.0))
                            .show(ui, |ui| {
                                ui.set_min_width(w);
                                ui.set_max_width(w);
                                ui.horizontal_wrapped(|ui| {
                                    ui.style_mut().spacing.item_spacing.x = 0.0;
                                    render_styled_line_parts(ui, cell, p.text, 12.5, true, p);
                                });
                            });
                    }
                    ui.end_row();

                    for (row_idx, row) in rows[1..].iter().enumerate() {
                        let row_bg = if row_idx % 2 == 0 {
                            egui::Color32::TRANSPARENT
                        } else {
                            p.panel.gamma_multiply(0.5)
                        };
                        for (i, cell) in row.iter().enumerate() {
                            let w = col_widths[i];
                            egui::Frame::none()
                                .fill(row_bg)
                                .inner_margin(egui::Margin::symmetric(12.0, 7.0))
                                .show(ui, |ui| {
                                    ui.set_min_width(w);
                                    ui.set_max_width(w);
                                    ui.horizontal_wrapped(|ui| {
                                        ui.style_mut().spacing.item_spacing.x = 0.0;
                                        render_styled_line_parts(ui, cell, p.text, 12.5, false, p);
                                    });
                                });
                        }
                        ui.end_row();
                    }
                });
        });
    ui.add_space(2.0);
}

fn get_file_icon(filename: &str) -> &'static str {
    let lower = filename.to_lowercase();
    if lower.ends_with(".rs") {
        "🦀"
    } else if lower.ends_with(".toml") {
        "⚙️"
    } else if lower.ends_with(".json") {
        "🗄️"
    } else if lower.ends_with(".md") {
        "📝"
    } else if lower.contains(".env") || lower.starts_with(".env") {
        "🔐"
    } else if lower.ends_with(".png")
        || lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".ico")
        || lower.ends_with(".gif")
        || lower.ends_with(".svg")
    {
        "🖼️"
    } else if lower.ends_with(".lock") {
        "🔒"
    } else if lower.ends_with(".txt")
        || lower.ends_with(".pdf")
        || lower.ends_with(".doc")
        || lower.ends_with(".docx")
    {
        "📄"
    } else if lower.ends_with(".py") || lower.ends_with(".pyw") {
        "🐍"
    } else if lower.ends_with(".java") || lower.ends_with(".class") || lower.ends_with(".jar") {
        "☕"
    } else if lower.ends_with(".js")
        || lower.ends_with(".ts")
        || lower.ends_with(".jsx")
        || lower.ends_with(".tsx")
    {
        "🟨"
    } else if lower.ends_with(".html") || lower.ends_with(".htm") {
        "🌐"
    } else if lower.ends_with(".css") || lower.ends_with(".scss") {
        "🎨"
    } else if lower.ends_with(".c")
        || lower.ends_with(".cpp")
        || lower.ends_with(".cc")
        || lower.ends_with(".h")
        || lower.ends_with(".hpp")
    {
        "🔌"
    } else if lower.ends_with(".sh") || lower.ends_with(".bat") || lower.ends_with(".ps1") {
        "🐚"
    } else if lower.ends_with(".go") {
        "🐹"
    } else if lower.contains('/') || lower.contains('\\') {
        "📁"
    } else {
        "📄"
    }
}

fn get_tool_icon(tool_name: &str) -> &'static str {
    match tool_name {
        "run_command" | "execute" => "🐚",
        "search_web" | "web_search" | "tavily" => "🔍",
        "browser_subagent" | "read_url_content" | "read_browser_page" | "open_browser_url" => "🌐",
        "write_to_file" | "write_file" => "📝",
        "replace_file_content" | "multi_replace_file_content" | "edit_file" => "✏",
        "view_file" | "read_file" => "📖",
        "list_dir" | "list_permissions" => "📁",
        "remove" | "delete_file" => "🗑",
        _ => "🛠",
    }
}

fn count_lines(s: &str) -> usize {
    if s.is_empty() { 0 } else { s.lines().count() }
}

fn extract_streaming_number(json_accumulated: &str, key: &str) -> Option<usize> {
    let key_pattern = format!("\"{}\"", key);
    if let Some(key_pos) = json_accumulated.find(&key_pattern) {
        let after_key = &json_accumulated[key_pos + key_pattern.len()..];
        if let Some(colon_pos) = after_key.find(':') {
            let after_colon = &after_key[colon_pos + 1..];
            let mut val_str = String::new();
            for c in after_colon.chars() {
                if c.is_ascii_digit() {
                    val_str.push(c);
                } else if !val_str.is_empty() {
                    break;
                }
            }
            if let Ok(num) = val_str.parse::<usize>() {
                return Some(num);
            }
        }
    }
    None
}

fn extract_streaming_content(json_accumulated: &str, key: &str) -> String {
    let key_pattern = format!("\"{}\"", key);
    if let Some(key_pos) = json_accumulated.find(&key_pattern) {
        let after_key = &json_accumulated[key_pos + key_pattern.len()..];
        if let Some(colon_pos) = after_key.find(':') {
            let after_colon = &after_key[colon_pos + 1..];
            if let Some(quote_start) = after_colon.find('"') {
                let val_content = &after_colon[quote_start + 1..];
                let mut result = String::new();
                let mut chars = val_content.chars().peekable();
                while let Some(c) = chars.next() {
                    if c == '\\' {
                        if let Some(next_c) = chars.peek() {
                            match next_c {
                                'n' => {
                                    result.push('\n');
                                    chars.next();
                                }
                                't' => {
                                    result.push('\t');
                                    chars.next();
                                }
                                'r' => {
                                    chars.next();
                                }
                                '"' => {
                                    result.push('"');
                                    chars.next();
                                }
                                '\\' => {
                                    result.push('\\');
                                    chars.next();
                                }
                                _ => {
                                    result.push(c);
                                }
                            }
                        } else {
                            result.push(c);
                        }
                    } else if c == '"' {
                        break;
                    } else {
                        result.push(c);
                    }
                }
                return result;
            }
        }
    }
    String::new()
}

fn extract_all_values_for_key(json_accumulated: &str, key: &str) -> Vec<String> {
    let mut results = Vec::new();
    let pattern = format!("\"{}\":", key);
    let mut search_start = 0;
    while let Some(pos) = json_accumulated[search_start..].find(&pattern) {
        let actual_pos = search_start + pos;
        let rest = &json_accumulated[actual_pos + pattern.len()..];
        if let Some(quote_start) = rest.find('"') {
            let val_content = &rest[quote_start + 1..];
            let mut result = String::new();
            let mut chars = val_content.chars().peekable();
            let mut consumed_bytes = quote_start + 1;
            while let Some(c) = chars.next() {
                consumed_bytes += c.len_utf8();
                if c == '\\' {
                    if let Some(next_c) = chars.peek() {
                        consumed_bytes += next_c.len_utf8();
                        match next_c {
                            'n' => {
                                result.push('\n');
                                chars.next();
                            }
                            't' => {
                                result.push('\t');
                                chars.next();
                            }
                            'r' => {
                                chars.next();
                            }
                            '"' => {
                                result.push('"');
                                chars.next();
                            }
                            '\\' => {
                                result.push('\\');
                                chars.next();
                            }
                            _ => {
                                result.push(c);
                            }
                        }
                    } else {
                        result.push(c);
                    }
                } else if c == '"' {
                    break;
                } else {
                    result.push(c);
                }
            }
            results.push(result);
            search_start = actual_pos + pattern.len() + consumed_bytes;
        } else {
            break;
        }
    }
    results
}

fn estimate_live_diff_stats(
    name: &str,
    args_json: &str,
    project_root: &std::path::Path,
) -> (usize, usize) {
    match name {
        "write_file" | "write_to_file" => {
            let mut path = extract_streaming_content(args_json, "path");
            if path.is_empty() {
                path = extract_streaming_content(args_json, "TargetFile");
            }
            let mut content = extract_streaming_content(args_json, "content");
            if content.is_empty() {
                content = extract_streaming_content(args_json, "CodeContent");
            }
            let added = count_lines(&content);
            if !path.is_empty() {
                let full_path = project_root.join(get_relative_path(&path, project_root));
                if full_path.exists() {
                    if let Ok(orig) = std::fs::read_to_string(&full_path) {
                        return (added, count_lines(&orig));
                    }
                }
            }
            (added, 0)
        }
        "replace_file_content" => {
            let target = extract_streaming_content(args_json, "TargetContent");
            let replacement = extract_streaming_content(args_json, "ReplacementContent");
            (count_lines(&replacement), count_lines(&target))
        }
        "edit_file" => {
            let target = extract_streaming_content(args_json, "target");
            let replacement = extract_streaming_content(args_json, "replacement");
            let start_line = extract_streaming_number(args_json, "start_line");
            let end_line = extract_streaming_number(args_json, "end_line");

            if let (Some(start), Some(end)) = (start_line, end_line) {
                if end >= start {
                    (count_lines(&replacement), end - start + 1)
                } else {
                    (count_lines(&replacement), 0)
                }
            } else if !target.is_empty() || !replacement.is_empty() {
                (count_lines(&replacement), count_lines(&target))
            } else {
                let mut content = extract_streaming_content(args_json, "content");
                if content.is_empty() {
                    content = extract_streaming_content(args_json, "CodeContent");
                }
                (count_lines(&content), 0)
            }
        }
        "multi_replace_file_content" => {
            let targets = extract_all_values_for_key(args_json, "TargetContent");
            let replacements = extract_all_values_for_key(args_json, "ReplacementContent");
            let deleted: usize = targets.iter().map(|s| count_lines(s)).sum();
            let added: usize = replacements.iter().map(|s| count_lines(s)).sum();
            (added, deleted)
        }
        _ => (0, 0),
    }
}

fn format_tool_args_summary(name: &str, args_json: &str) -> String {
    match name {
        "read_file" | "cat" | "view_file" => {
            let mut path = extract_streaming_content(args_json, "path");
            if path.is_empty() {
                path = extract_streaming_content(args_json, "AbsolutePath");
            }
            if path.is_empty() {
                "file".to_string()
            } else {
                std::path::Path::new(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or(path)
            }
        }
        "write_file"
        | "write_to_file"
        | "edit_file"
        | "replace_file_content"
        | "multi_replace_file_content" => {
            let mut path = extract_streaming_content(args_json, "path");
            if path.is_empty() {
                path = extract_streaming_content(args_json, "TargetFile");
            }
            if path.is_empty() {
                "file".to_string()
            } else {
                std::path::Path::new(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or(path)
            }
        }
        "shell" | "run_command" | "shell_command" => {
            let mut cmd = extract_streaming_content(args_json, "command");
            if cmd.is_empty() {
                cmd = extract_streaming_content(args_json, "CommandLine");
            }
            if cmd.is_empty() {
                "command".to_string()
            } else {
                if cmd.chars().count() > 60 {
                    crate::util::truncate_chars(&cmd, 57)
                } else {
                    cmd
                }
            }
        }
        "web_search" | "search_web" => {
            let query = extract_streaming_content(args_json, "query");
            if query.is_empty() {
                "web query".to_string()
            } else {
                format!("\"{}\"", query)
            }
        }
        "read_skill" => {
            let name = extract_streaming_content(args_json, "name");
            if name.is_empty() {
                "skill".to_string()
            } else {
                name
            }
        }
        _ => String::new(),
    }
}

fn format_tool_text(name: &str, success: bool, summary: &str) -> String {
    let prefix = if success { "" } else { "Failed to " };
    match name {
        "read_file" | "cat" | "view_file" => {
            if summary.is_empty() {
                format!("{}read file", prefix)
            } else {
                format!("{}read {}", prefix, summary)
            }
        }
        "write_file" | "write_to_file" => {
            if summary.is_empty() {
                format!("{}write file", prefix)
            } else {
                format!("{}wrote {}", prefix, summary)
            }
        }
        "edit_file" | "replace_file_content" | "multi_replace_file_content" => {
            if summary.is_empty() {
                format!("{}edit file", prefix)
            } else {
                format!("{}edited {}", prefix, summary)
            }
        }
        "web_search" | "search_web" => {
            if summary.is_empty() {
                format!("{}search the web", prefix)
            } else {
                format!("{}searched the web for {}", prefix, summary)
            }
        }
        "web_fetch" => {
            if summary.is_empty() {
                format!("{}fetch page", prefix)
            } else {
                format!("{}fetched {}", prefix, summary)
            }
        }
        "shell" | "run_command" | "shell_command" => {
            if summary.is_empty() {
                format!("{}run command", prefix)
            } else {
                format!("{}ran `{}`", prefix, summary)
            }
        }
        "read_skill" => {
            if summary.is_empty() {
                format!("{}read skill", prefix)
            } else {
                format!("{}reading skill {}", prefix, summary)
            }
        }
        _ => {
            let display_name = name.replace('_', " ");
            format!("{}{}", prefix, display_name)
        }
    }
}

/// One reasoning step. Live reasoning is shown open and follows the stream;
/// once the step is over it gets a new id and starts collapsed.
fn render_reasoning_block(
    ui: &mut egui::Ui,
    title: &str,
    thinking: &str,
    p: &Palette,
    key: (usize, usize),
    live: bool,
) {
    if thinking.trim().is_empty() {
        return;
    }
    let title = style::clean_label(title.trim_end_matches("..."));
    let title = if title.is_empty() || title == "Thinking" {
        "Reasoning".to_string()
    } else {
        title.replacen("Thinking", "Reasoning", 1)
    };
    egui::CollapsingHeader::new(egui::RichText::new(title).size(12.5).color(p.dim))
        .id_salt(("reasoning", key.0, key.1, live))
        .default_open(live)
        .show(ui, |ui| {
            let response = egui::Frame::none()
                .inner_margin(egui::Margin {
                    left: 12.0,
                    right: 4.0,
                    top: 2.0,
                    bottom: 2.0,
                })
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt(("reasoning_scroll", key.0, key.1))
                        .max_height(240.0)
                        .stick_to_bottom(live)
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(thinking.trim())
                                        .size(12.5)
                                        .italics()
                                        .color(p.faint),
                                )
                                .wrap(),
                            );
                        });
                })
                .response;
            let r = response.rect;
            ui.painter().line_segment(
                [
                    r.left_top() + egui::vec2(2.0, 2.0),
                    r.left_bottom() + egui::vec2(2.0, -2.0),
                ],
                egui::Stroke::new(1.5_f32, p.border),
            );
        });
    ui.add_space(2.0);
}

/// "✔ read main.rs" style row used for finished tool calls.
/// The row under a message that holds its hover actions (Copy, Edit, ...).
/// Its size is fixed and its contents are clipped to it, so the actions
/// appearing on hover never move anything. In the chat, which sticks to the
/// bottom, a row that grew on hover shifted the view, lost the hover and
/// shrank again, flickering every frame.
fn message_action_row(ui: &mut egui::Ui, layout: egui::Layout, add: impl FnOnce(&mut egui::Ui)) {
    const HEIGHT: f32 = 22.0;
    let (_, rect) = ui.allocate_space(egui::vec2(ui.available_width(), HEIGHT));
    let mut row = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(layout));
    row.set_clip_rect(rect.intersect(ui.clip_rect()));
    row.spacing_mut().item_spacing.x = 0.0;
    row.spacing_mut().interact_size.y = HEIGHT;
    row.spacing_mut().button_padding.y = 2.0;
    add(&mut row);
}

fn activity_row(ui: &mut egui::Ui, mark: &str, mark_color: egui::Color32, text: &str, p: &Palette) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.add_space(2.0);
        ui.label(egui::RichText::new(mark).size(11.0).color(mark_color));
        ui.add(egui::Label::new(egui::RichText::new(text).size(12.5).color(p.dim)).truncate());
    });
}

fn diff_counts(ui: &mut egui::Ui, added: usize, deleted: usize, p: &Palette) {
    if added > 0 {
        ui.label(
            egui::RichText::new(format!("+{added}"))
                .font(egui::FontId::monospace(11.5))
                .color(p.success),
        );
    }
    if deleted > 0 {
        ui.label(
            egui::RichText::new(format!("-{deleted}"))
                .font(egui::FontId::monospace(11.5))
                .color(p.danger),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn render_chronological_activity(
    ui: &mut egui::Ui,
    logs: &[String],
    p: &Palette,
    // (name, streamed JSON args) of the tool currently running, if any
    current_tool: Option<(&str, &str)>,
    is_processing: bool,
    fallback_thinking: &Option<String>,
    project_root: &std::path::Path,
    msg_idx: usize,
) {
    let has_thinking_deltas = logs.iter().any(|log| log.starts_with("THINKING_DELTA|"));
    let mut block = 0usize;

    if !has_thinking_deltas {
        if let Some(thinking) = fallback_thinking {
            render_reasoning_block(ui, "Reasoning", thinking, p, (msg_idx, block), false);
            block += 1;
        }
    }

    let mut current_loop_title: Option<String> = None;
    let mut current_loop_thinking = String::new();

    for log in logs {
        if let Some(delta) = log.strip_prefix("THINKING_DELTA|") {
            current_loop_thinking.push_str(delta);
        } else if log.starts_with("ℹ️ Thinking (loop ") || log.starts_with("ℹ️ Thinking (step ")
        {
            if let Some(ref title) = current_loop_title {
                render_reasoning_block(
                    ui,
                    title,
                    &current_loop_thinking,
                    p,
                    (msg_idx, block),
                    false,
                );
                block += 1;
            }
            let raw_title = log.trim_start_matches("ℹ️ ").trim_end_matches("...");
            current_loop_title = Some(raw_title.to_string());
            current_loop_thinking.clear();
        } else if log.starts_with("TOOL_RESULT|") {
            let title = current_loop_title
                .take()
                .unwrap_or_else(|| "Thinking".to_string());
            render_reasoning_block(
                ui,
                &title,
                &current_loop_thinking,
                p,
                (msg_idx, block),
                false,
            );
            block += 1;
            current_loop_thinking.clear();

            if let Some(payload) = log.strip_prefix("TOOL_RESULT|") {
                let parts: Vec<&str> = payload.split('|').collect();
                if parts.len() >= 3 {
                    let name = parts[1];
                    let success = parts[2].contains("Success") || parts[2].contains('✅');
                    let summary = parts.get(3).cloned().unwrap_or("");
                    let text = style::sentence_case(&format_tool_text(name, success, summary));
                    if success {
                        activity_row(ui, "✔", p.success, &text, p);
                    } else {
                        activity_row(ui, "✖", p.danger, &text, p);
                    }
                }
            }
        } else if log.starts_with("FILE_CHANGE|") {
            let title = current_loop_title
                .take()
                .unwrap_or_else(|| "Thinking".to_string());
            render_reasoning_block(
                ui,
                &title,
                &current_loop_thinking,
                p,
                (msg_idx, block),
                false,
            );
            block += 1;
            current_loop_thinking.clear();

            if let Some(payload) = log.strip_prefix("FILE_CHANGE|") {
                // Payload format: <icon>|<path>|+<added>|-<deleted>
                let parts: Vec<&str> = payload.split('|').collect();
                if parts.len() >= 4 {
                    let path = parts[1];
                    // Strip the leading + / - signs so usize::parse succeeds.
                    let added: usize = parts[2].trim_start_matches('+').parse().unwrap_or(0);
                    let deleted: usize = parts[3].trim_start_matches('-').parse().unwrap_or(0);
                    let action = if deleted == 0 && added > 0 {
                        "Created"
                    } else if added == 0 && deleted > 0 {
                        "Deleted"
                    } else {
                        "Edited"
                    };
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        ui.add_space(2.0);
                        ui.label(egui::RichText::new("✔").size(11.0).color(p.success));
                        ui.label(egui::RichText::new(action).size(12.5).color(p.dim));
                        ui.label(
                            egui::RichText::new(path)
                                .font(egui::FontId::monospace(12.0))
                                .color(p.text),
                        )
                        .on_hover_text(path);
                        diff_counts(ui, added, deleted, p);
                    });
                }
            }
        } else if let Some(payload) = log.strip_prefix("AGENT_EVT|") {
            // Payload: <started|done|stopped>|<name>|<task or summary>
            let mut parts = payload.splitn(3, '|');
            let (state, name, text) = (
                parts.next().unwrap_or(""),
                parts.next().unwrap_or(""),
                parts.next().unwrap_or(""),
            );
            let (mark, color, verb) = match state {
                "started" => ("◆", p.accent, "started"),
                "done" => ("✓", p.success, "finished"),
                _ => ("✕", p.danger, "stopped"),
            };
            let text = crate::util::truncate_chars(&text.replace('\n', " "), 160);
            activity_row(
                ui,
                mark,
                color,
                &format!("Sub-agent {name} {verb} · {text}"),
                p,
            );
        } else if let Some(payload) = log.strip_prefix("AGENT_MSG|") {
            // Payload: <from>|<to>|<text>
            let mut parts = payload.splitn(3, '|');
            let (from, to, text) = (
                parts.next().unwrap_or(""),
                parts.next().unwrap_or(""),
                parts.next().unwrap_or(""),
            );
            activity_row(
                ui,
                "↔",
                p.accent,
                &format!("{from} → {to}: {}", text.replace('\n', " ")),
                p,
            );
        } else if let Some(warning) = log.strip_prefix("⚠️ ") {
            // Errors are already appended to the message body.
            if !warning.starts_with("Error:") {
                activity_row(ui, "!", p.warn, &style::clean_label(warning), p);
            }
        }
    }

    // Reasoning that hasn't been followed by a tool call yet is still live.
    let trailing_live = is_processing && current_tool.is_none();
    match current_loop_title {
        Some(ref title) => render_reasoning_block(
            ui,
            title,
            &current_loop_thinking,
            p,
            (msg_idx, block),
            trailing_live,
        ),
        None => render_reasoning_block(
            ui,
            "Thinking",
            &current_loop_thinking,
            p,
            (msg_idx, block),
            trailing_live,
        ),
    }

    if is_processing {
        if let Some((name, current_tool_args)) = current_tool {
            let args_summary = format_tool_args_summary(name, current_tool_args);
            let is_write = name == "write_file"
                || name == "write_to_file"
                || name == "edit_file"
                || name == "replace_file_content"
                || name == "multi_replace_file_content";

            // Live row: a softly pulsing accent dot marks the running call.
            let time = ui.ctx().input(|input| input.time) as f32;
            let pulse = 0.55 + 0.45 * (time * std::f32::consts::TAU / 1.4).sin();
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.add_space(2.0);
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                ui.painter()
                    .circle_filled(rect.center(), 3.5, p.accent.gamma_multiply(pulse));
                if is_write {
                    let action = if name == "write_file" || name == "write_to_file" {
                        "Writing"
                    } else {
                        "Editing"
                    };
                    ui.label(egui::RichText::new(action).size(12.5).color(p.text));
                    ui.label(
                        egui::RichText::new(args_summary)
                            .font(egui::FontId::monospace(12.0))
                            .color(p.text),
                    );
                    let (added, deleted) =
                        estimate_live_diff_stats(name, current_tool_args, project_root);
                    diff_counts(ui, added, deleted, p);
                } else {
                    let text = style::sentence_case(&format_tool_text(name, true, &args_summary));
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!("{text}…"))
                                .size(12.5)
                                .color(p.text),
                        )
                        .truncate(),
                    );
                }
            });
            ui.ctx().request_repaint();
        }
    }
}

/// Whether the V key is held right now (the paste key with Ctrl).
#[cfg(windows)]
fn paste_key_down() -> bool {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetAsyncKeyState(vkey: i32) -> i16;
    }
    const VK_V: i32 = 0x56;
    // The high bit is set while the key is down.
    unsafe { GetAsyncKeyState(VK_V) < 0 }
}

/// Elsewhere media can be pasted from the paperclip's right-click menu.
#[cfg(not(windows))]
fn paste_key_down() -> bool {
    false
}

/// Images the `image` crate can decode (for copying them to the clipboard).
fn is_raster_image(path: &str) -> bool {
    let lower = path.to_lowercase();
    [".png", ".jpg", ".jpeg", ".webp", ".gif", ".bmp"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

fn copy_image_to_clipboard(path: &std::path::Path) -> Result<(), String> {
    let image = image::open(path).map_err(|e| e.to_string())?.to_rgba8();
    let (width, height) = image.dimensions();
    arboard::Clipboard::new()
        .and_then(|mut cb| {
            cb.set_image(arboard::ImageData {
                width: width as usize,
                height: height as usize,
                bytes: std::borrow::Cow::Owned(image.into_raw()),
            })
        })
        .map_err(|e| e.to_string())
}

/// Save a pasted clipboard image as a PNG in `.norvexum/attachments/`.
fn save_pasted_image(
    project_root: &std::path::Path,
    image: &arboard::ImageData,
) -> Result<std::path::PathBuf, String> {
    let buffer = image::RgbaImage::from_raw(
        image.width as u32,
        image.height as u32,
        image.bytes.to_vec(),
    )
    .ok_or("the clipboard image has an unexpected size")?;
    let name = format!(
        "pasted-{}.png",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    );
    let path = attachment_destination(project_root, std::path::Path::new(&name));
    buffer.save(&path).map_err(|e| e.to_string())?;
    Ok(path)
}

/// Where an attached file is placed so the sandboxed agent can read it.
/// Files already inside the project are used in place; others are copied to
/// `.norvexum/attachments/` under a name that never overwrites anything.
fn attachment_destination(
    project_root: &std::path::Path,
    source: &std::path::Path,
) -> std::path::PathBuf {
    let root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    if let Ok(canonical) = source.canonicalize() {
        if canonical.starts_with(&root) {
            return source.to_path_buf();
        }
    }
    let dir = project_root
        .join(crate::config::NORVEXUM_DIR)
        .join("attachments");
    let _ = std::fs::create_dir_all(&dir);
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("attachment");
    let ext = source
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let mut candidate = dir.join(format!("{stem}{ext}"));
    let mut n = 1;
    while candidate.exists() {
        candidate = dir.join(format!("{stem}-{n}{ext}"));
        n += 1;
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_tree_groups_paths_into_folders() {
        let paths: Vec<String> = [
            "src/main.rs",
            "src/ui/mod.rs",
            "Cargo.toml",
            r"src\gui\app.rs",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let tree = FileTree::build(&paths);
        assert_eq!(tree.files.len(), 1);
        assert_eq!(tree.files[0].0, "Cargo.toml");
        let src = &tree.dirs["src"];
        assert_eq!(
            src.files[0],
            ("main.rs".to_string(), "src/main.rs".to_string())
        );
        assert!(src.dirs.contains_key("ui"));
        assert!(src.dirs.contains_key("gui"));
        assert_eq!(src.dirs["gui"].files[0].1, r"src\gui\app.rs");
    }

    fn message(role: &str, content: &str, tool_logs: &[&str]) -> GuiMessage {
        GuiMessage {
            role: role.into(),
            content: content.into(),
            thinking: None,
            is_thinking_done: true,
            tool_logs: tool_logs.iter().map(|s| s.to_string()).collect(),
            attachments: Vec::new(),
        }
    }

    #[test]
    fn pasted_images_are_saved_as_attachments() {
        let root = std::env::temp_dir().join(format!("norvexum_paste_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let pixels = [
            255u8, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];
        let image = arboard::ImageData {
            width: 2,
            height: 2,
            bytes: std::borrow::Cow::Borrowed(&pixels),
        };
        let first = save_pasted_image(&root, &image).unwrap();
        let second = save_pasted_image(&root, &image).unwrap();
        assert_ne!(first, second, "a second paste must not overwrite the first");
        assert!(first.starts_with(root.join(".norvexum").join("attachments")));
        let decoded = image::open(&first).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), (2, 2));
        assert_eq!(decoded.get_pixel(1, 0).0, [0, 255, 0, 255]);
        assert!(is_raster_image(&first.to_string_lossy()));
        assert!(!is_raster_image("notes.txt"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn reply_text_joins_the_bubbles_of_one_answer() {
        let messages = vec![
            message("user", "make a game", &[]),
            message("assistant", "I'll build it.", &["TOOL_START|x|write_file"]),
            message(
                "assistant",
                "",
                &["TOOL_RESULT|x|write_file|Success|game.html"],
            ),
            message("assistant", "Done — open game.html.", &[]),
            message("assistant", "", &[]), // ghost bubble, not shown
        ];
        let shown: Vec<usize> = (0..messages.len())
            .filter(|&i| is_message_shown(&messages[i]))
            .collect();
        assert_eq!(shown, vec![0, 1, 2, 3]);
        assert_eq!(
            reply_text(&messages, &shown, 3),
            "I'll build it.\n\nDone — open game.html."
        );
        assert_eq!(reply_text(&messages, &shown, 1), "I'll build it.");
    }

    #[test]
    fn workspace_scan_recurses_and_skips_build_dirs() {
        let root = std::env::temp_dir().join(format!("norvexum_tree_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src/deep/deeper")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::write(root.join("README.md"), "x").unwrap();
        std::fs::write(root.join(".hidden"), "x").unwrap();
        std::fs::write(root.join("src/deep/deeper/a.rs"), "x").unwrap();
        std::fs::write(root.join("target/debug/out"), "x").unwrap();
        let files = list_workspace_files(&root);
        assert_eq!(
            files,
            vec!["README.md".to_string(), "src/deep/deeper/a.rs".to_string()]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
