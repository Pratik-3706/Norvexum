use crate::agent::AgentEvent;
use crate::config::Settings;
use crate::config::providers::build_registry;
use notify::{RecursiveMode, Result as NotifyResult, Watcher};
use std::sync::OnceLock;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub static GUI_SENDER: OnceLock<std_mpsc::Sender<GuiConfig>> = OnceLock::new();

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
}

#[derive(Clone)]
pub struct TerminalEntry {
    pub command: String,
    pub output: String,
    pub success: bool,
    pub timestamp: std::time::Instant,
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
    show_custom_model_input: bool,
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
    attached_image: Option<std::path::PathBuf>,
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
    active_written_files_backup: Option<Vec<String>>,
    selected_file: Option<String>,
    inspected_file_content: String,
    inspector_error: Option<String>,
    workspace_files_rx: Option<std_mpsc::Receiver<Vec<String>>>,
    is_loading_files: bool,

    // Debounce state for the filesystem watcher. When the agent writes a
    // burst of files in quick succession, notify fires dozens of events per
    // second — without debouncing we'd relaunch `list_workspace_files` every
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
    terminal_entries: Vec<TerminalEntry>,
    pending_terminal_command: Option<String>,

    // Dynamic model capability probing
    model_reasoning_type: Option<crate::ai::types::ReasoningType>,
    probed_capabilities: std::collections::HashMap<String, Option<crate::ai::types::ReasoningType>>,
    probing_in_progress: bool,
    capabilities_rx: Option<std_mpsc::Receiver<CapabilitiesUpdate>>,
    capabilities_tx: Option<std_mpsc::Sender<CapabilitiesUpdate>>,
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
) {
    let canvas_size = size + 4.0;
    let (canvas, _) =
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
}

fn attachment_button(ui: &mut egui::Ui, dark_mode: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(44.0, 40.0), egui::Sense::click());
    let fill = if response.hovered() {
        egui::Color32::from_rgb(54, 42, 82)
    } else if dark_mode {
        egui::Color32::from_rgb(38, 31, 58)
    } else {
        egui::Color32::from_rgb(237, 233, 254)
    };
    let stroke = if response.hovered() {
        egui::Stroke::new(1.0, egui::Color32::from_rgb(167, 139, 250))
    } else {
        egui::Stroke::new(1.0, egui::Color32::from_rgb(91, 70, 145))
    };
    ui.painter().rect(rect, 10.0, fill, stroke);

    let center = rect.center();
    let angle = -0.65_f32;
    let (sin, cos) = angle.sin_cos();
    let transform = |x: f32, y: f32| center + egui::vec2(x * cos - y * sin, x * sin + y * cos);
    let points = [
        (-2.0, -8.0),
        (3.0, -8.0),
        (6.0, -5.0),
        (6.0, 4.0),
        (3.0, 8.0),
        (-1.0, 8.0),
        (-5.0, 4.0),
        (-5.0, -3.0),
        (-2.0, -6.0),
        (2.0, -6.0),
        (4.0, -3.0),
        (4.0, 3.0),
        (2.0, 5.0),
        (0.0, 5.0),
        (-2.0, 3.0),
        (-2.0, -2.0),
    ]
    .into_iter()
    .map(|(x, y)| transform(x, y))
    .collect();
    ui.painter().add(egui::Shape::line(
        points,
        egui::Stroke::new(
            2.0,
            if dark_mode {
                egui::Color32::from_rgb(216, 206, 255)
            } else {
                egui::Color32::from_rgb(91, 33, 182)
            },
        ),
    ));

    response.on_hover_text("Attach a file")
}

fn get_secure_key(name: &str) -> Option<String> {
    if let Ok(entry) = keyring::Entry::new("norvexum", name) {
        entry.get_password().ok()
    } else {
        None
    }
}

fn set_secure_key(name: &str, val: &str) {
    if let Ok(entry) = keyring::Entry::new("norvexum", name) {
        let _ = entry.set_password(val);
    }
}

fn delete_secure_key(name: &str) {
    if let Ok(entry) = keyring::Entry::new("norvexum", name) {
        let _ = entry.delete_password();
    }
}

fn list_workspace_files(root: &std::path::Path) -> Vec<String> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                    if !name.starts_with('.') && name != "norvexum.log" {
                        files.push(name.to_string());
                    }
                }
            } else if path.is_dir() {
                let dir_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if dir_name != ".git"
                    && dir_name != ".norvexum"
                    && dir_name != "target"
                    && dir_name != "node_modules"
                    && dir_name != "build"
                    && dir_name != "dist"
                    && dir_name != "deps"
                    && dir_name != ".agents"
                    && dir_name != ".gemini"
                {
                    if let Ok(sub_entries) = std::fs::read_dir(&path) {
                        for sub_entry in sub_entries.flatten() {
                            let sub_path = sub_entry.path();
                            if sub_path.is_file() {
                                if let Some(sub_name) =
                                    sub_path.file_name().and_then(|s| s.to_str())
                                {
                                    files.push(format!("{}/{}", dir_name, sub_name));
                                }
                            }
                        }
                    }
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
        // Register custom panic hook to write log to %LOCALAPPDATA%
        std::panic::set_hook(Box::new(|info| {
            let log_dir = std::env::var("LOCALAPPDATA")
                .map(|p| std::path::PathBuf::from(p).join("Norvexum"))
                .unwrap_or_else(|_| std::path::PathBuf::from("."));
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

        // Sync process CWD with project_root on startup
        let _ = std::env::set_current_dir(&config.settings.project_root);

        // Initial folder scan (loaded in background)
        let workspace_files = Vec::new();
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
            show_custom_model_input: false,
            custom_skill_name: String::new(),
            custom_skill_desc: String::new(),
            custom_skill_triggers: String::new(),
            custom_skill_instructions: String::new(),
            skills_list,
            new_model_provider: "openai".to_string(),
            new_model_id: String::new(),
            left_sidebar_open: true,
            right_sidebar_open: true,
            attached_image: None,
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
            active_written_files_backup: None,
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
            right_sidebar_width: 270.0,
            ctx: None,
            last_active_provider: initial_provider,
            last_active_model: initial_model,
            show_terminal_panel: false,
            terminal_entries: Vec::new(),
            pending_terminal_command: None,
            model_reasoning_type: None,
            probed_capabilities: std::collections::HashMap::new(),
            probing_in_progress: false,
            capabilities_rx: Some(capabilities_rx),
            capabilities_tx: Some(capabilities_tx),
        };

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
            });
            app.is_processing = true;
        }

        // Start file watcher
        app.start_file_watcher(cc.egui_ctx.clone());

        // Trigger loading the workspace files list in the background immediately
        app.trigger_load_files();

        // Scan for local Ollama models in background on startup
        {
            let ollama_url = app.settings.ollama_base_url.clone()
                .unwrap_or_else(|| crate::ai::ollama::DEFAULT_OLLAMA_URL.to_string());
            let ctx = cc.egui_ctx.clone();
            std::thread::spawn(move || {
                if let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() {
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
        let label_color = if self.dark_mode {
            egui::Color32::from_rgb(220, 225, 255)
        } else {
            egui::Color32::from_rgb(30, 41, 59)
        };

        ui.label(egui::RichText::new(label).color(label_color).strong());

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
                ui.label(
                    egui::RichText::new("••••••••")
                        .color(egui::Color32::from_rgb(16, 185, 129))
                        .strong(),
                );
                if ui.button("Clear").clicked() {
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
                let text_edit = egui::TextEdit::singleline(&mut val)
                    .password(true)
                    .text_color(if self.dark_mode {
                        egui::Color32::WHITE
                    } else {
                        egui::Color32::BLACK
                    })
                    .desired_width(120.0);

                ui.add(text_edit);
                if ui.button("Save").clicked() {
                    if !val.trim().is_empty() {
                        set_secure_key(field_name, &val);
                        let key_val = Some(val.clone());
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
        ui.add_space(6.0);
    }

    fn sync_settings(&mut self) {
        let _ = self.settings.save();
        if let Ok(json) = serde_json::to_string(&self.settings) {
            let _ = self.user_tx.send(format!("/config_update {json}"));
        }

        // Unload Ollama model from memory if we changed the active provider from Ollama to something else,
        // or if we changed the Ollama model itself.
        let last_provider = self.last_active_provider.clone();
        let last_model = self.last_active_model.clone();
        let current_provider = self.settings.active_provider.clone();
        let current_model = self.settings.active_model.clone();

        if (last_provider == "ollama" && current_provider != "ollama") || (last_provider == "ollama" && last_model != current_model) {
            let ollama_url = self.settings.ollama_base_url.clone()
                .unwrap_or_else(|| crate::ai::ollama::DEFAULT_OLLAMA_URL.to_string());
            let client = reqwest::Client::new();
            let unload_url = format!("{}/api/generate", ollama_url.trim_end_matches('/'));
            std::thread::spawn(move || {
                if let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    let _ = rt.block_on(async {
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
        let ollama_url = self.settings.ollama_base_url.clone()
            .unwrap_or_else(|| crate::ai::ollama::DEFAULT_OLLAMA_URL.to_string());
        if let Some(ref ctx) = self.ctx {
            let ctx_clone = ctx.clone();
            std::thread::spawn(move || {
                if let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    rt.block_on(async {
                        let _ = crate::ai::ollama::discover_models(&ollama_url).await;
                        ctx_clone.request_repaint();
                    });
                }
            });
        } else {
            std::thread::spawn(move || {
                if let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    rt.block_on(async {
                        let _ = crate::ai::ollama::discover_models(&ollama_url).await;
                    });
                }
            });
        }
        self.trigger_capabilities_probe();
    }

    fn save_capabilities_cache(&self) {
        let cache_dir = self.settings.project_root.join(crate::config::NORVEXUM_DIR);
        let _ = std::fs::create_dir_all(&cache_dir);
        let cache_path = cache_dir.join("capabilities_cache.json");
        if let Ok(content) = serde_json::to_string_pretty(&self.probed_capabilities) {
            let _ = std::fs::write(cache_path, content);
        }
    }

    fn load_capabilities_cache(&mut self) {
        let cache_path = self.settings.project_root.join(crate::config::NORVEXUM_DIR).join("capabilities_cache.json");
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
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(r) => r,
                Err(_) => return,
            };

            rt.block_on(async {
                let reasoning_type = crate::ai::probe_model_reasoning(&settings, &provider, &model).await;
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
                if self.settings.active_provider == update.provider && self.settings.active_model == update.model {
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
            if let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() {
                let mut temp_settings = Settings::default();
                temp_settings.project_root = path_clone;
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
        self.terminal_entries.clear();
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
        if name == "replace_file_content" || name == "multi_replace_file_content" || name == "edit_file" {
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
                streaming_content =
                    extract_streaming_content(&self.current_tool_args, "ReplacementContent");
            }
            if streaming_content.is_empty() {
                streaming_content = extract_streaming_content(&self.current_tool_args, "content");
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
                    "Preview unavailable: this file is larger than {} KB. Check 'Filter by line range' below to view sections.",
                    MAX_PREVIEW_BYTES / 1024
                ));
            }
            Ok(_) => match std::fs::read_to_string(&path) {
                Ok(content) => {
                    let lines: Vec<&str> = content.lines().collect();
                    self.line_range_total_lines = lines.len();
                    if self.line_range_enabled {
                        let start = self.line_range_start.clamp(1, lines.len().max(1)) - 1;
                        let end = self.line_range_end.clamp(1, lines.len().max(1)).max(self.line_range_start) - 1;
                        let mut filtered_content = String::new();
                        for i in start..=end.min(lines.len() - 1) {
                            filtered_content.push_str(lines[i]);
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
                let end = self.line_range_end.clamp(1, original_lines.len().max(1)).max(self.line_range_start) - 1;

                let mut new_lines = Vec::new();
                for i in 0..start {
                    if i < original_lines.len() {
                        new_lines.push(original_lines[i].to_string());
                    }
                }
                for line in self.inspected_file_content.lines() {
                    new_lines.push(line.to_string());
                }
                for i in (end + 1)..original_lines.len() {
                    new_lines.push(original_lines[i].to_string());
                }

                std::fs::write(&path, new_lines.join("\n"))?;
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
                            let msg = self.get_or_create_assistant_message();
                            msg.tool_logs.push(format!("ℹ️ {s}"));
                        }
                        AgentEvent::Thinking(t) => {
                            self.is_processing = true;
                            self.check_and_split_assistant_message();
                            let msg = self.get_or_create_assistant_message();
                            if let Some(ref mut thinking) = msg.thinking {
                                thinking.push_str(&t);
                            } else {
                                msg.thinking = Some(t.clone());
                            }
                            msg.tool_logs.push(format!("THINKING_DELTA|{}", t));
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
                            self.status_text = format!("🔧 Tool: {name}...");
                            self.current_tool_name = Some(name.clone());
                            self.current_tool_id = Some(id.clone());
                            self.current_tool_args.clear();

                            self.check_and_split_assistant_message();
                            let msg = self.get_or_create_assistant_message();
                            msg.tool_logs.push(format!(
                                "TOOL_START|{}|{}",
                                get_tool_icon(&name),
                                name
                            ));

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

                                        if self.selected_file.as_deref() == Some(&relative_path) {
                                            self.inspect_file(&relative_path);
                                        }
                                    }
                                }
                            }
                        }
                        AgentEvent::ToolResult {
                            id, name, result, success,
                        } => {
                            let args_summary = if Some(&id) == self.current_tool_id.as_ref() {
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

                            // Capture terminal entry for run_command/shell
                            if name == "run_command" || name == "shell" || name == "shell_command" || name == "execute" {
                                let cmd = if Some(&id) == self.current_tool_id.as_ref() {
                                    let mut c = extract_streaming_content(&self.current_tool_args, "command");
                                    if c.is_empty() {
                                        c = extract_streaming_content(&self.current_tool_args, "CommandLine");
                                    }
                                    if c.is_empty() {
                                        c = extract_streaming_content(&self.current_tool_args, "cmd");
                                    }
                                    c
                                } else {
                                    self.pending_terminal_command.take().unwrap_or_default()
                                };
                                if !cmd.is_empty() {
                                    self.terminal_entries.push(TerminalEntry {
                                        command: cmd,
                                        output: result.clone(),
                                        success,
                                        timestamp: std::time::Instant::now(),
                                    });
                                }
                            }

                            if Some(&id) == self.current_tool_id.as_ref() {
                                self.current_tool_name = None;
                                self.current_tool_id = None;
                                self.current_tool_args.clear();
                            }

                            let status = if success { "✅ Success" } else { "❌ Failed" };
                            self.status_text = format!("🔧 Tool {name}: {status}");
                            let msg = self.get_or_create_assistant_message();
                            msg.tool_logs.push(format!(
                                "TOOL_RESULT|{}|{}|{}|{}",
                                get_tool_icon(&name),
                                name,
                                status,
                                args_summary
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
                            let (added, deleted) = if let Some((ref live_file, live_add, live_del)) =
                                self.current_writing_file
                            {
                                if live_file == &relative_path && (live_add > 0 || live_del > 0) {
                                    (live_add as i32, live_del as i32)
                                } else {
                                    get_file_diff_stats(&self.settings.project_root, &relative_path)
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

                             let msg = self.get_or_create_assistant_message();
                             msg.tool_logs.push(format!("⚠️ Error: {display_err}"));
                             msg.content
                                 .push_str(&format!("\n\n⚠️ **Execution Error:** {display_err}"));
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
                        }
                        AgentEvent::ModelSwitched { model, provider } => {
                            self.settings.active_model = model;
                            self.settings.active_provider = provider;
                            self.status_text = "Model updated".to_string();
                        }
                        AgentEvent::ClearChat => {
                            self.messages.clear();
                            self.session_written_files.clear();
                            self.file_diff_stats.clear();
                            self.terminal_entries.clear();
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
                self.workspace_files_rx = None;
                self.is_loading_files = false;
            }
        }

        // Drain the filesystem watcher queue. We coalesce all pending events
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
                // Keep the UI responsive while we wait for the debounce
                // window to expire — without this the file list would feel
                // like it updates with a stutter.
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
        }
    }

    fn send_message(&mut self) {
        if self.input_text.trim().is_empty() {
            return;
        }

        let mut raw_prompt = self.input_text.trim().to_string();
        let display_prompt = raw_prompt.clone();

        if let Some(ref file_path) = self.attached_image.take() {
            let filename = file_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file");
            let dest_path = self.settings.project_root.join(filename);
            if std::fs::copy(file_path, &dest_path).is_ok() {
                raw_prompt = format!(
                    "{} (Uploaded/copied file {} to sandbox workspace path: {})",
                    raw_prompt,
                    filename,
                    dest_path.to_string_lossy()
                );
                self.trigger_load_files();
            } else {
                raw_prompt = format!("{} ({})", raw_prompt, file_path.to_string_lossy());
            }
        }

        self.messages.push(GuiMessage {
            role: "user".to_string(),
            content: display_prompt,
            thinking: None,
            is_thinking_done: false,
            tool_logs: Vec::new(),
        });

        // If this is the start of a conversation, clear session files
        if self.messages.len() == 1 {
            self.session_written_files.clear();
            self.file_diff_stats.clear();
            self.terminal_entries.clear();
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
        let mut open = self.show_settings_modal;
        let mut save_clicked = false;
        egui::Window::new("⚙️ Norvexum Settings & Custom Models")
            .open(&mut open)
            .default_size([460.0, 520.0])
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(420.0)
                    .show(ui, |ui| {
                        let text_color = if self.dark_mode {
                            egui::Color32::WHITE
                        } else {
                            egui::Color32::BLACK
                        };

                        let label_color = if self.dark_mode {
                            egui::Color32::from_rgb(200, 205, 235)
                        } else {
                            egui::Color32::from_rgb(60, 65, 80)
                        };

                        let mut changed = false;

                        // ── Approval Check section ──
                        ui.label(
                            egui::RichText::new("SECURITY & PERMISSIONS")
                                .size(11.0)
                                .strong()
                                .color(egui::Color32::from_rgb(167, 139, 250)),
                        );
                        ui.add_space(4.0);

                        let mut ask_perms = self.settings.ask_perms;
                        ui.horizontal(|ui| {
                            if ui.checkbox(&mut ask_perms, "").changed() {
                                self.settings.ask_perms = ask_perms;
                                changed = true;
                            }
                            ui.label(
                                egui::RichText::new("🔒 Ask permission before executing tools")
                                    .color(label_color)
                                    .strong(),
                            );
                        });
                        ui.add_space(8.0);

                        let mut changed_tools = false;
                        ui.add_enabled_ui(ask_perms, |ui| {
                            let critical_tools = [
                                (
                                    vec!["run_command", "shell"],
                                    "🐚 Execute Shell Command (run_command)",
                                ),
                                (
                                    vec!["write_file", "write_to_file"],
                                    "📝 Write/Create Files (write_to_file)",
                                ),
                                (
                                    vec![
                                        "edit_file",
                                        "replace_file_content",
                                        "multi_replace_file_content",
                                    ],
                                    "✏️ Edit Files (replace_file_content)",
                                ),
                                (vec!["remove", "delete_file"], "🗑️ Delete Files (remove)"),
                            ];

                            for (tool_names, tool_label) in critical_tools {
                                let mut requires = tool_names.iter().any(|name| {
                                    self.settings.require_approval.iter().any(|t| t == name)
                                });
                                ui.horizontal(|ui| {
                                    ui.add_space(14.0);
                                    if ui.checkbox(&mut requires, "").changed() {
                                        for name in &tool_names {
                                            if requires {
                                                if !self
                                                    .settings
                                                    .require_approval
                                                    .contains(&name.to_string())
                                                {
                                                    self.settings
                                                        .require_approval
                                                        .push(name.to_string());
                                                }
                                            } else {
                                                self.settings
                                                    .require_approval
                                                    .retain(|t| t != *name);
                                            }
                                        }
                                        changed_tools = true;
                                    }
                                    ui.label(
                                        egui::RichText::new(tool_label).color(label_color),
                                    );
                                });
                            }
                        });

                        if changed_tools {
                            changed = true;
                        }

                        ui.add_space(12.0);
                        ui.separator();
                        ui.add_space(8.0);

                        // ── Custom Upcoming Models section ──
                        ui.label(
                            egui::RichText::new("REGISTER UPCOMING CUSTOM MODELS")
                                .size(11.0)
                                .strong()
                                .color(egui::Color32::from_rgb(167, 139, 250)),
                        );
                        ui.add_space(6.0);

                        ui.horizontal(|ui| {
                            ui.label("Provider:");
                            egui::ComboBox::from_id_salt("custom_model_provider_select")
                                .selected_text(&self.new_model_provider)
                                .show_ui(ui, |ui| {
                                    for prov in &["openai", "google_direct", "anthropic", "aicredits", "ollama"] {
                                        ui.selectable_value(
                                            &mut self.new_model_provider,
                                            prov.to_string(),
                                            *prov,
                                        );
                                    }
                                });
                        });
                        ui.add_space(4.0);

                        ui.horizontal(|ui| {
                            ui.label("Model ID:  ");
                            ui.text_edit_singleline(&mut self.new_model_id);
                            if ui.button("➕ Add Model").clicked() && !self.new_model_id.trim().is_empty() {
                                let full_model_entry = format!("{}:{}", self.new_model_provider, self.new_model_id.trim());
                                if !self.settings.custom_models.contains(&full_model_entry) {
                                    self.settings.custom_models.push(full_model_entry);
                                    self.new_model_id.clear();
                                    changed = true;
                                }
                            }
                        });

                        if !self.settings.custom_models.is_empty() {
                            ui.add_space(6.0);
                            ui.label(egui::RichText::new("Registered Custom Models:").small().color(label_color));
                            let mut to_remove = None;
                            for (idx, m) in self.settings.custom_models.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    ui.add_space(14.0);
                                    ui.label(egui::RichText::new(m).monospace());
                                    if ui.button("❌").clicked() {
                                        to_remove = Some(idx);
                                    }
                                });
                            }
                            if let Some(idx) = to_remove {
                                self.settings.custom_models.remove(idx);
                                changed = true;
                            }
                        }

                        ui.add_space(12.0);
                        ui.separator();
                        ui.add_space(8.0);

                        // ── Parameters section ──
                        ui.label(
                            egui::RichText::new("CORE PARAMETERS")
                                .size(11.0)
                                .strong()
                                .color(egui::Color32::from_rgb(167, 139, 250)),
                        );
                        ui.add_space(6.0);

                        ui.label(
                            egui::RichText::new("Max thinking loops per cycle:")
                                .color(label_color),
                        );
                        if ui
                            .add(egui::Slider::new(
                                &mut self.settings.max_thinking_loops,
                                1..=100,
                            ))
                            .changed()
                        {
                            changed = true;
                        }

                        ui.label(
                            egui::RichText::new("Browser timeout (seconds):")
                                .color(label_color),
                        );
                        if ui
                            .add(egui::Slider::new(
                                &mut self.settings.browser_timeout_secs,
                                5..=300,
                            ))
                            .changed()
                        {
                            changed = true;
                        }

                        ui.label(
                            egui::RichText::new("Max content characters limit:")
                                .color(label_color),
                        );
                        if ui
                            .add(egui::Slider::new(
                                &mut self.settings.max_content_chars,
                                1000..=5000000,
                            ))
                            .changed()
                        {
                            changed = true;
                        }

                        ui.add_space(12.0);
                        ui.separator();
                        ui.add_space(8.0);

                        // ── System Prompt section ──
                        ui.label(
                            egui::RichText::new("CUSTOM SYSTEM INSTRUCTIONS")
                                .size(11.0)
                                .strong()
                                .color(egui::Color32::from_rgb(167, 139, 250)),
                        );
                        ui.add_space(4.0);

                        let mut val = self
                            .settings
                            .custom_system_prompt
                            .clone()
                            .unwrap_or_default();
                        if ui
                            .add(
                                egui::TextEdit::multiline(&mut val)
                                    .desired_rows(6)
                                    .desired_width(ui.available_width() - 20.0),
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

                        if changed {
                            self.sync_settings();
                        }
                    });

                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("💾 Save Settings").clicked() {
                        save_clicked = true;
                    }
                });
            });
        if save_clicked {
            self.sync_settings();
            open = false;
        }
        self.show_settings_modal = open;
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.ctx.is_none() {
            self.ctx = Some(ctx.clone());
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
        if is_fs != self.settings.fullscreen {
            self.settings.fullscreen = is_fs;
            changed = true;
        }
        if is_max != self.settings.maximized {
            self.settings.maximized = is_max;
            changed = true;
        }
        if changed {
            self.sync_settings();
        }

        self.poll_agent_events(ctx);
        self.poll_file_events(ctx);
        self.poll_capabilities();

        // Capture dropped files
        ctx.input(|i| {
            if !i.raw.dropped_files.is_empty() {
                for file in &i.raw.dropped_files {
                    if let Some(ref path) = file.path {
                        self.attached_image = Some(path.clone());
                        self.status_text = format!(
                            "Attached: {}",
                            path.file_name().and_then(|n| n.to_str()).unwrap_or("")
                        );
                        break;
                    }
                }
            }
        });

        // Apply Premium Visual Theme Design
        if self.dark_mode {
            let mut visuals = egui::Visuals::dark();
            visuals.panel_fill = egui::Color32::from_rgb(10, 11, 16);
            visuals.window_fill = egui::Color32::from_rgb(19, 18, 30);
            visuals.extreme_bg_color = egui::Color32::from_rgb(18, 20, 29);

            visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(22, 23, 34);
            visuals.widgets.inactive.rounding = egui::Rounding::same(10.0);
            visuals.widgets.inactive.fg_stroke =
                egui::Stroke::new(1.0, egui::Color32::from_rgb(202, 205, 220));
            visuals.widgets.inactive.bg_stroke =
                egui::Stroke::new(1.0, egui::Color32::from_rgb(47, 48, 67));

            visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(47, 38, 72);
            visuals.widgets.hovered.rounding = egui::Rounding::same(10.0);
            visuals.widgets.hovered.fg_stroke =
                egui::Stroke::new(1.0, egui::Color32::from_rgb(240, 240, 255));
            visuals.widgets.hovered.bg_stroke =
                egui::Stroke::new(1.0, egui::Color32::from_rgb(124, 58, 237));

            visuals.widgets.active.bg_fill = egui::Color32::from_rgb(124, 58, 237); // Royal Accent Purple
            visuals.widgets.active.rounding = egui::Rounding::same(8.0);
            visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, egui::Color32::WHITE);

            visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(15, 16, 23);
            visuals.widgets.noninteractive.rounding = egui::Rounding::same(8.0);
            visuals.widgets.noninteractive.fg_stroke =
                egui::Stroke::new(1.0, egui::Color32::from_rgb(218, 222, 235));
            visuals.widgets.noninteractive.bg_stroke =
                egui::Stroke::new(1.0, egui::Color32::from_rgb(43, 45, 60));
            visuals.selection.bg_fill = egui::Color32::from_rgb(124, 58, 237);
            visuals.window_stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(43, 39, 55));
            visuals.faint_bg_color = egui::Color32::from_rgb(17, 17, 25);

            ctx.set_visuals(visuals);
        } else {
            let mut visuals = egui::Visuals::light();
            visuals.panel_fill = egui::Color32::from_rgb(243, 244, 248); // Crisp light grey
            visuals.window_fill = egui::Color32::from_rgb(255, 255, 255);
            visuals.extreme_bg_color = egui::Color32::from_rgb(228, 231, 242); // Distinct grayish blue background for input fields in light mode

            visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(240, 242, 248);
            visuals.widgets.inactive.rounding = egui::Rounding::same(8.0);
            visuals.widgets.inactive.bg_stroke =
                egui::Stroke::new(1.0, egui::Color32::from_rgb(180, 185, 210)); // Clear visible borders in light mode!

            visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(212, 216, 232);
            visuals.widgets.hovered.rounding = egui::Rounding::same(8.0);
            visuals.widgets.hovered.bg_stroke =
                egui::Stroke::new(1.0, egui::Color32::from_rgb(124, 58, 237));

            visuals.widgets.active.bg_fill = egui::Color32::from_rgb(124, 58, 237);
            visuals.widgets.active.rounding = egui::Rounding::same(8.0);
            visuals.selection.bg_fill = egui::Color32::from_rgb(124, 58, 237);
            visuals.window_stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(214, 218, 232));
            visuals.faint_bg_color = egui::Color32::from_rgb(248, 249, 252);

            ctx.set_visuals(visuals);
        }

        // Custom Layout Spacing Override
        let mut style = (*ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(10.0, 9.0);
        style.spacing.button_padding = egui::vec2(13.0, 7.0);
        style.spacing.combo_width = 150.0;
        style.visuals.striped = true;
        ctx.set_style(style);

        // Top panel header
        egui::TopBottomPanel::top("top_panel")
            .frame(
                egui::Frame::default()
                    .inner_margin(egui::Margin::symmetric(14.0, 10.0))
                    .fill(if self.dark_mode {
                        egui::Color32::from_rgb(14, 14, 21)
                    } else {
                        egui::Color32::WHITE
                    })
                    .stroke(egui::Stroke::new(
                        1.0,
                        if self.dark_mode {
                            egui::Color32::from_rgb(42, 38, 54)
                        } else {
                            egui::Color32::from_rgb(220, 223, 235)
                        },
                    )),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::Image::new(&self.logo_tex).max_width(30.0));
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("NORVEXUM")
                            .font(egui::FontId::proportional(20.0))
                            .strong()
                            .color(if self.dark_mode {
                                egui::Color32::from_rgb(244, 241, 255)
                            } else {
                                egui::Color32::from_rgb(31, 25, 45)
                            }),
                    );
                    let left_sidebar_btn = if self.left_sidebar_open {
                        "◀ Settings"
                    } else {
                        "▶ Settings"
                    };
                    if ui
                        .button(left_sidebar_btn)
                        .on_hover_text("Toggle left workspace settings sidebar")
                        .clicked()
                    {
                        self.left_sidebar_open = !self.left_sidebar_open;
                    }

                    ui.add_space(16.0);

                    // Provider & Model Pickers
                    let registry = build_registry();
                    let mut current_provider_idx = registry
                        .iter()
                        .position(|p| p.name == self.settings.active_provider)
                        .unwrap_or(0);

                    let provider_label = &registry[current_provider_idx].display_name;
                    ui.label(egui::RichText::new("Provider:").strong());
                    egui::ComboBox::from_id_salt("provider_select")
                        .selected_text(provider_label)
                        .show_ui(ui, |ui| {
                            for (idx, provider) in registry.iter().enumerate() {
                                if ui
                                    .selectable_value(
                                        &mut current_provider_idx,
                                        idx,
                                        &provider.display_name,
                                    )
                                    .clicked()
                                {
                                    self.settings.active_provider = provider.name.clone();
                                    if let Some(first_model) = provider.models.first() {
                                        self.settings.active_model = first_model.id.clone();
                                    }
                                    self.sync_settings();
                                }
                            }
                        });

                    ui.add_space(12.0);

                    let active_provider = &registry[current_provider_idx];
                    ui.label(egui::RichText::new("Model:").strong());
                    egui::ComboBox::from_id_salt("model_select")
                        .selected_text(&self.settings.active_model)
                        .show_ui(ui, |ui| {
                            for model in &active_provider.models {
                                if ui
                                    .selectable_value(
                                        &mut self.settings.active_model,
                                        model.id.clone(),
                                        &model.id,
                                    )
                                    .clicked()
                                {
                                    self.sync_settings();
                                }
                            }
                        });

                    if let Some(_reasoning_type) = self.model_reasoning_type {
                        ui.add_space(12.0);
                        ui.label(egui::RichText::new("Thinking:").strong());
                        let budget = self.settings.thinking_budget.unwrap_or(2048);
                        let label = match budget {
                            0 => "Off",
                            1024 => "Minimal (1K)",
                            2048 => "Low (2K)",
                            4096 => "Medium (4K)",
                            8192 => "High (8K)",
                            16384 => "Max (16K)",
                            32768 => "UltraCode (32K)",
                            _ => "Custom",
                        };
                        egui::ComboBox::from_id_salt("thinking_select")
                            .selected_text(label)
                            .show_ui(ui, |ui| {
                                let options = [
                                    (0, "Off"),
                                    (1024, "Minimal (1K)"),
                                    (2048, "Low (2K)"),
                                    (4096, "Medium (4K)"),
                                    (8192, "High (8K)"),
                                    (16384, "Max (16K)"),
                                    (32768, "UltraCode (32K)"),
                                ];
                                for (val, text) in options {
                                    if ui.selectable_value(&mut self.settings.thinking_budget, Some(val), text).clicked() {
                                        self.sync_settings();
                                    }
                                }
                            });
                    } else if self.probing_in_progress {
                        ui.add_space(12.0);
                        ui.label(egui::RichText::new("Thinking:").strong());
                        ui.label(egui::RichText::new("Checking support...").italics().color(egui::Color32::from_rgb(120, 120, 140)));
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(if self.dark_mode { "☀" } else { "🌙" })
                            .on_hover_text("Toggle Theme")
                            .clicked()
                        {
                            self.dark_mode = !self.dark_mode;
                        }

                        ui.add_space(4.0);

                        // Terminal toggle
                        let term_label = if self.show_terminal_panel { "▼ Terminal" } else { "▲ Terminal" };
                        let term_btn = egui::Button::new(
                            egui::RichText::new(term_label)
                                .size(12.0)
                                .color(if self.show_terminal_panel {
                                    egui::Color32::from_rgb(52, 211, 153)
                                } else if self.dark_mode {
                                    egui::Color32::from_rgb(148, 163, 184)
                                } else {
                                    egui::Color32::from_rgb(100, 116, 139)
                                }),
                        )
                        .fill(if self.show_terminal_panel {
                            if self.dark_mode {
                                egui::Color32::from_rgb(20, 40, 30)
                            } else {
                                egui::Color32::from_rgb(220, 252, 231)
                            }
                        } else {
                            egui::Color32::TRANSPARENT
                        })
                        .stroke(egui::Stroke::new(
                            0.5,
                            if self.show_terminal_panel {
                                egui::Color32::from_rgb(52, 211, 153)
                            } else if self.dark_mode {
                                egui::Color32::from_rgb(55, 55, 70)
                            } else {
                                egui::Color32::from_rgb(200, 205, 218)
                            },
                        ))
                        .rounding(8.0);
                        if ui
                            .add(term_btn)
                            .on_hover_text("Toggle AI terminal panel")
                            .clicked()
                        {
                            self.show_terminal_panel = !self.show_terminal_panel;
                        }

                        ui.add_space(4.0);

                        let right_sidebar_btn = if self.right_sidebar_open {
                            "Files ▶"
                        } else {
                            "◀ Files"
                        };
                        if ui
                            .button(right_sidebar_btn)
                            .on_hover_text("Toggle right workspace files sidebar")
                            .clicked()
                        {
                            self.right_sidebar_open = !self.right_sidebar_open;
                        }
                    });
                });
            });

        // Left Panel (Sidebar Settings) — animated open/close + generously
        // wide resize range so the user can pull/push the panel freely.
        let target_left_t = if self.left_sidebar_open { 1.0 } else { 0.0 };
        let left_t_raw =
            ctx.animate_value_with_time(egui::Id::new("left_sidebar_anim"), target_left_t, 0.18);
        // ease-out-cubic for a snappy, premium feel
        let left_t = 1.0 - (1.0 - left_t_raw.clamp(0.0, 1.0)).powi(3);
        let left_animating = (left_t_raw - target_left_t).abs() > 0.005;

        if left_t > 0.005 {
            let mut left_panel = egui::SidePanel::left("left_sidebar")
                .frame(
                    egui::Frame::default()
                        .inner_margin(16.0)
                        .fill(if self.dark_mode {
                            egui::Color32::from_rgb(13, 14, 21)
                        } else {
                            egui::Color32::WHITE
                        }),
                )
                // Disable resize only while the open/close tween is running;
                // once settled we let egui remember & control the user width.
                .resizable(!left_animating && self.left_sidebar_open)
                .width_range(60.0..=1200.0);
            if left_animating {
                left_panel = left_panel.exact_width(self.left_sidebar_width * left_t);
            } else {
                left_panel = left_panel.default_width(self.left_sidebar_width);
            }
            let panel_response = left_panel.show(ctx, |ui| {
                    let pinned_bottom_h = 124.0;
                    let scroll_height = (ui.available_height() - pinned_bottom_h).max(80.0);

                    egui::ScrollArea::vertical()
                        .max_height(scroll_height)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new("WORKSPACE")
                                    .size(11.0)
                                    .strong()
                                    .color(egui::Color32::from_rgb(167, 139, 250)),
                            );
                            ui.label(
                                egui::RichText::new("Project settings")
                                    .font(egui::FontId::proportional(18.0))
                                    .strong(),
                            );
                            ui.add_space(8.0);

                // Group the workspace folder loader in a styled card frame
                egui::Frame::group(ui.style())
                    .fill(ctx.style().visuals.window_fill)
                    .inner_margin(10.0)
                    .rounding(10.0)
                    .show(ui, |ui| {
                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new("Active Workspace Folder:")
                                .small()
                                .color(if self.dark_mode { egui::Color32::from_rgb(180, 185, 210) } else { egui::Color32::from_rgb(80, 85, 110) })
                            );
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                ui.add(egui::TextEdit::singleline(&mut self.folder_input).desired_width(110.0));
                                if ui.button("📂 Select").on_hover_text("Browse workspace folder from system dialog").clicked() {
                                    if let Some(path) = rfd::FileDialog::new().pick_folder() {
                                        self.folder_input = path.to_string_lossy().to_string();
                                        self.load_project_folder(&path, ctx.clone());
                                    }
                                }
                                if ui.button("📁 Load").clicked() {
                                    let path = std::path::PathBuf::from(&self.folder_input);
                                    if path.exists() && path.is_dir() {
                                        self.load_project_folder(&path, ctx.clone());
                                    } else {
                                        self.folder_error = Some("Directory path not found!".to_string());
                                    }
                                }
                            });
                            if let Some(ref err) = self.folder_error {
                                ui.colored_label(egui::Color32::RED, err);
                            }
                        });
                    });

                ui.add_space(12.0);

                // API Key Settings inside a card frame
                egui::Frame::group(ui.style())
                    .fill(ctx.style().visuals.window_fill)
                    .inner_margin(10.0)
                    .rounding(10.0)
                    .show(ui, |ui| {
                        ui.collapsing(egui::RichText::new("🔑 API Keys Settings").strong().color(if self.dark_mode { egui::Color32::WHITE } else { egui::Color32::BLACK }), |ui| {
                            ui.add_space(6.0);
                            self.draw_key_field(ui, "google_ai_api_key", "Google AI direct key");
                            self.draw_key_field(ui, "openai_api_key", "OpenAI key");
                            self.draw_key_field(ui, "anthropic_api_key", "Anthropic key");
                            self.draw_key_field(ui, "tavily_api_key", "Tavily web search key");
                            self.draw_key_field(ui, "aicredits_api_key", "AICredits.in key");
                            self.draw_key_field(ui, "ocr_space_api_key", "OCR Space key");
                            self.draw_key_field(ui, "pollinations_api_key", "Pollinations image key");
                        });
                    });

                ui.add_space(12.0);

                // Custom Skills Section inside a card frame
                egui::Frame::group(ui.style())
                    .fill(ctx.style().visuals.window_fill)
                    .inner_margin(10.0)
                    .rounding(10.0)
                    .show(ui, |ui| {
                        ui.collapsing(egui::RichText::new("✨ Custom Skills Settings").strong().color(if self.dark_mode { egui::Color32::WHITE } else { egui::Color32::BLACK }), |ui| {
                            ui.add_space(6.0);

                            if ui.button("📂 Upload Skill File").on_hover_text("Upload a custom skill markdown file").clicked() {
                                if let Some(path) = rfd::FileDialog::new().add_filter("Markdown", &["md"]).pick_file() {
                                    let skill_dir = crate::skills::get_global_skills_dir();
                                    let _ = std::fs::create_dir_all(&skill_dir);
                                    if let Some(filename) = path.file_name() {
                                        let dest = skill_dir.join(filename);
                                        if std::fs::copy(&path, &dest).is_ok() {
                                            self.status_text = format!("Skill file {} uploaded!", filename.to_string_lossy());
                                            self.skills_list = crate::skills::load_all_skills(&self.settings.project_root);
                                        }
                                    }
                                }
                            }

                            ui.separator();

                            ui.label(egui::RichText::new("Create Custom Skill:").strong().color(if self.dark_mode { egui::Color32::LIGHT_BLUE } else { egui::Color32::DARK_BLUE }));

                            ui.label(egui::RichText::new("Skill Name:").small().color(if self.dark_mode { egui::Color32::WHITE } else { egui::Color32::BLACK }));
                            ui.add(egui::TextEdit::singleline(&mut self.custom_skill_name).desired_width(180.0));

                            ui.label(egui::RichText::new("Description:").small().color(if self.dark_mode { egui::Color32::WHITE } else { egui::Color32::BLACK }));
                            ui.add(egui::TextEdit::singleline(&mut self.custom_skill_desc).desired_width(180.0));

                            ui.label(egui::RichText::new("Trigger Patterns (comma sep):").small().color(if self.dark_mode { egui::Color32::WHITE } else { egui::Color32::BLACK }));
                            ui.add(egui::TextEdit::singleline(&mut self.custom_skill_triggers).desired_width(180.0));

                            ui.label(egui::RichText::new("System Instructions:").small().color(if self.dark_mode { egui::Color32::WHITE } else { egui::Color32::BLACK }));
                            ui.add(egui::TextEdit::multiline(&mut self.custom_skill_instructions).desired_rows(3).desired_width(180.0));

                            ui.add_space(4.0);
                            if ui.button("➕ Add Skill").clicked() {
                                let name = self.custom_skill_name.trim().to_string();
                                let inst = self.custom_skill_instructions.trim().to_string();
                                if !name.is_empty() && !inst.is_empty() {
                                    let desc = self.custom_skill_desc.trim().to_string();
                                    let triggers: Vec<String> = self.custom_skill_triggers
                                        .split(',')
                                        .map(|s| s.trim().to_string())
                                        .filter(|s| !s.is_empty())
                                        .collect();

                                    let triggers_str = if triggers.is_empty() {
                                        format!("  - {}", name.to_lowercase())
                                    } else {
                                        triggers.iter().map(|t| format!("  - {}", t)).collect::<Vec<_>>().join("\n")
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

                            ui.separator();
                                   ui.label(egui::RichText::new("Loaded Skills:").strong().color(if self.dark_mode { egui::Color32::WHITE } else { egui::Color32::BLACK }));
                            egui::ScrollArea::vertical().max_height(80.0).show(ui, |ui| {
                                for skill in &self.skills_list {
                                    ui.horizontal(|ui| {
                                        ui.label(egui::RichText::new(format!("• {}", skill.name)).strong().color(if self.dark_mode { egui::Color32::WHITE } else { egui::Color32::BLACK }));

                                        let desc_color = if self.dark_mode {
                                            egui::Color32::from_rgb(148, 163, 184) // Slate 400
                                        } else {
                                            egui::Color32::from_rgb(100, 116, 139) // Slate 500
                                        };
                                        ui.small(egui::RichText::new(format!("({})", skill.description)).color(desc_color).italics());
                                    });
                                }
                            });
                        });
                    });

                ui.add_space(16.0);
                ui.label(
                    egui::RichText::new("RECENT CHATS")
                        .size(11.0)
                        .strong()
                        .color(egui::Color32::from_rgb(167, 139, 250)),
                );
                ui.add_space(4.0);

                // Active session indicator
                let active_selected = self.current_session_id.is_none();
                if ui.selectable_label(active_selected, egui::RichText::new("● Current Active Session").strong()).clicked() {
                    if let Some(active_messages) = self.active_messages_backup.take() {
                        self.messages = active_messages;
                    }
                    self.current_session_id = None;
                    self.status_text = "Returned to current active chat session.".to_string();
                }

                // Render previous sessions
                let mut session_to_delete = None;
                let mut session_to_load = None;
                for summary in &self.history_sessions {
                    let title = format!("Chat ({})", &summary.id[..6.min(summary.id.len())]);
                    let display_text = format!("💬 {}", title);
                    let is_selected = self.current_session_id.as_deref() == Some(&summary.id);

                    ui.horizontal(|ui| {
                        if ui.selectable_label(is_selected, display_text)
                            .on_hover_text(format!("Model: {}\nMessages: {}", summary.model, summary.message_count))
                            .clicked()
                        {
                            session_to_load = Some(summary.id.clone());
                        }

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button(egui::RichText::new("🗑").color(egui::Color32::from_rgb(239, 68, 68)))
                                .on_hover_text("Delete conversation")
                                .clicked()
                            {
                                session_to_delete = Some(summary.id.clone());
                            }
                        });
                    });
                }

                if let Some(id) = session_to_load {
                    if let Some(loaded_sess) = crate::agent::session::Session::load_by_id(&self.settings.project_root, &id) {
                        if let Some(ref saved_root) = loaded_sess.project_root {
                            let saved_path = std::path::PathBuf::from(saved_root);
                            if saved_path.exists() && saved_path.is_dir() && saved_path != self.settings.project_root {
                                let ctx = ui.ctx().clone();
                                self.load_project_folder(&saved_path, ctx);
                            }
                        }
                        if self.current_session_id.is_none() {
                            self.active_messages_backup = Some(self.messages.clone());
                        }
                        self.current_session_id = Some(id.clone());
                        self.messages = loaded_sess.messages.iter().filter_map(|m| {
                            let role = match m.role {
                                crate::ai::types::Role::User => "user",
                                crate::ai::types::Role::Assistant => "assistant",
                                crate::ai::types::Role::System | crate::ai::types::Role::Tool => return None,
                            };
                            let content = m.text();
                            if content.trim().is_empty() {
                                return None;
                            }
                            Some(GuiMessage {
                                role: role.to_string(),
                                content,
                                thinking: None,
                                is_thinking_done: true,
                                tool_logs: Vec::new(),
                            })
                        }).collect();
                        self.session_written_files.clear();
                        self.file_diff_stats.clear();
                        self.terminal_entries.clear();
                        let title = format!("Chat ({})", &id[..6.min(id.len())]);
                        self.status_text = format!("Loaded conversation: {}", title);
                    }
                }

                if let Some(id) = session_to_delete {
                    let session_dir = self.settings.project_root.join(".norvexum").join("sessions");
                    let file_path = session_dir.join(format!("{}.json", id));
                    if std::fs::remove_file(&file_path).is_ok() {
                        if self.current_session_id.as_deref() == Some(&id) {
                            if let Some(active_messages) = self.active_messages_backup.take() {
                                self.messages = active_messages;
                            }
                            self.current_session_id = None;
                            self.session_written_files.clear();
                            self.file_diff_stats.clear();
                            self.terminal_entries.clear();
                        }
                        self.history_sessions = crate::agent::session::Session::list(&self.settings.project_root);
                        self.status_text = "Session deleted.".to_string();
                    }
                }
            });

                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(6.0);

                    // Pinned Logo and Settings Button at the bottom
                    ui.horizontal(|ui| {
                        ui.add(egui::Image::new(&self.logo_tex).max_width(42.0));
                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new("Norvexum").strong());
                            egui::Frame::default()
                                .fill(if self.dark_mode {
                                    egui::Color32::from_rgb(42, 31, 68)
                                } else {
                                    egui::Color32::from_rgb(237, 233, 254)
                                })
                                .inner_margin(egui::Margin::symmetric(8.0, 3.0))
                                .rounding(8.0)
                                .show(ui, |ui| {
                                    ui.label(
                                        egui::RichText::new("v0.1.0 · GUI")
                                            .size(11.0)
                                            .strong()
                                            .color(if self.dark_mode {
                                                egui::Color32::from_rgb(196, 181, 253)
                                            } else {
                                                egui::Color32::from_rgb(91, 33, 182)
                                            }),
                                    );
                                });
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button(egui::RichText::new("⚙").font(egui::FontId::proportional(16.0)))
                                .on_hover_text("Open Settings")
                                .clicked()
                            {
                                self.show_settings_modal = !self.show_settings_modal;
                            }
                        });
                    });
                });
            if !left_animating && self.left_sidebar_open {
                self.left_sidebar_width = panel_response.response.rect.width();
            }
        }

        // Right Panel (Workspace Files explorer - styled premium file tree)
        // Animated open/close + widened resize range so the user can pull it
        // out wide enough to actually read code previews without scroll-back.
        let target_right_t = if self.right_sidebar_open { 1.0 } else { 0.0 };
        let right_t_raw =
            ctx.animate_value_with_time(egui::Id::new("right_sidebar_anim"), target_right_t, 0.18);
        let right_t = 1.0 - (1.0 - right_t_raw.clamp(0.0, 1.0)).powi(3);
        let right_animating = (right_t_raw - target_right_t).abs() > 0.005;

        if right_t > 0.005 {
            let mut right_panel = egui::SidePanel::right("right_sidebar")
                .frame(
                    egui::Frame::default()
                        .inner_margin(14.0)
                        .fill(if self.dark_mode {
                            egui::Color32::from_rgb(18, 16, 25)
                        } else {
                            egui::Color32::WHITE
                        }),
                )
                .resizable(!right_animating && self.right_sidebar_open)
                .width_range(60.0..=1200.0);
            if right_animating {
                right_panel = right_panel.exact_width(self.right_sidebar_width * right_t);
            } else {
                right_panel = right_panel.default_width(self.right_sidebar_width);
            }
            let panel_response = right_panel.show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("Workspace files")
                                .font(egui::FontId::proportional(16.0))
                                .strong(),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                egui::RichText::new(format!(
                                    "{}",
                                    self.session_written_files.len()
                                ))
                                .small()
                                .color(egui::Color32::from_rgb(167, 139, 250)),
                            );
                        });
                    });
                    ui.label(
                        egui::RichText::new("Created or changed this session")
                            .size(12.0)
                            .color(egui::Color32::from_rgb(148, 163, 184)),
                    );
                    ui.add_space(8.0);

                    let mut file_to_inspect = None;
                    egui::ScrollArea::vertical()
                        .id_salt("workspace_file_list")
                        .max_height((ui.available_height() * 0.46).max(120.0))
                        .show(ui, |ui| {
                            let mut display_files = self.session_written_files.clone();
                            if let Some((ref active_file, _, _)) = self.current_writing_file {
                                if self.current_writing_status.as_deref() == Some("writing")
                                    && !active_file.is_empty()
                                    && !display_files.contains(active_file)
                                {
                                    display_files.push(active_file.clone());
                                }
                            }

                            if display_files.is_empty() {
                                egui::Frame::default()
                                    .fill(ctx.style().visuals.faint_bg_color)
                                    .inner_margin(12.0)
                                    .rounding(10.0)
                                    .show(ui, |ui| {
                                        ui.label(
                                            egui::RichText::new(
                                                "Your files will appear here.",
                                            )
                                            .size(13.0)
                                            .color(egui::Color32::from_rgb(148, 163, 184)),
                                        );
                                    });
                            } else {
                                for filename in &display_files {
                                    let icon = get_file_icon(filename);

                                    let is_writing = if let Some((ref active_file, _, _)) =
                                        self.current_writing_file
                                    {
                                        active_file == filename
                                            && self.current_writing_status.as_deref()
                                                == Some("writing")
                                    } else {
                                        false
                                    };

                                    let label_text = if is_writing {
                                        let tool_name = self.current_tool_name.as_deref().unwrap_or("");
                                        let action = if tool_name == "write_file" || tool_name == "write_to_file" {
                                            "Writing"
                                        } else {
                                            "Editing"
                                        };
                                        format!("📝 {} [{}...]", filename, action)
                                    } else {
                                        format!("{} {}", icon, filename)
                                    };

                                    let label = if is_writing {
                                        egui::RichText::new(label_text)
                                            .color(egui::Color32::from_rgb(16, 185, 129))
                                            .strong()
                                    } else {
                                        egui::RichText::new(label_text)
                                    };

                                    let is_selected =
                                        self.selected_file.as_deref() == Some(filename);
                                    if ui
                                        .selectable_label(is_selected, label)
                                        .on_hover_text("Preview file")
                                        .clicked()
                                    {
                                        file_to_inspect = Some(filename.clone());
                                    }

                                    // Show diff stats: prefer live current_writing_file, fall back to persisted file_diff_stats
                                    let (show_added, show_deleted) = if let Some((active_file, added, deleted)) =
                                        &self.current_writing_file
                                    {
                                        if active_file == filename {
                                            (*added, *deleted)
                                        } else if let Some(&(a, d)) = self.file_diff_stats.get(filename.as_str()) {
                                            (a, d)
                                        } else {
                                            (0, 0)
                                        }
                                    } else if let Some(&(a, d)) = self.file_diff_stats.get(filename.as_str()) {
                                        (a, d)
                                    } else {
                                        (0, 0)
                                    };

                                    if show_added > 0 || show_deleted > 0 {
                                        ui.horizontal(|ui| {
                                            ui.add_space(22.0);
                                            if show_added > 0 {
                                                ui.label(
                                                    egui::RichText::new(format!("+{show_added}"))
                                                        .small()
                                                        .color(egui::Color32::from_rgb(
                                                            16, 185, 129,
                                                        )),
                                                );
                                            }
                                            if show_deleted > 0 {
                                                ui.label(
                                                    egui::RichText::new(format!("-{show_deleted}"))
                                                        .small()
                                                        .color(egui::Color32::from_rgb(
                                                            239, 68, 68,
                                                        )),
                                                );
                                            }
                                        });
                                    }
                                }
                            }
                        });

                    if let Some(filename) = file_to_inspect {
                        self.inspect_file(&filename);
                    }

                    if let Some(filename) = self.selected_file.clone() {
                        ui.add_space(8.0);
                        ui.separator();
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("File preview").strong());
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .small_button("×")
                                        .on_hover_text("Close preview")
                                        .clicked()
                                    {
                                        self.selected_file = None;
                                        self.inspected_file_content.clear();
                                        self.inspector_error = None;
                                    }
                                },
                            );
                        });
                        ui.label(
                            egui::RichText::new(&filename)
                                .monospace()
                                .size(11.0)
                                .color(egui::Color32::from_rgb(167, 139, 250)),
                        );

                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            let mut filter = self.line_range_enabled;
                            if ui.checkbox(&mut filter, "Filter lines").changed() {
                                self.line_range_enabled = filter;
                                if filter && self.line_range_end == 0 {
                                    self.line_range_end = self.line_range_total_lines.min(100);
                                }
                                self.inspect_file(&filename);
                            }
                            if self.line_range_enabled {
                                ui.label("From:");
                                let mut start = self.line_range_start;
                                if ui.add(egui::DragValue::new(&mut start).range(1..=self.line_range_total_lines.max(1))).changed() {
                                    self.line_range_start = start;
                                    self.inspect_file(&filename);
                                }
                                ui.label("To:");
                                let mut end = self.line_range_end;
                                if ui.add(egui::DragValue::new(&mut end).range(1..=self.line_range_total_lines.max(1))).changed() {
                                    self.line_range_end = end;
                                    self.inspect_file(&filename);
                                }
                            }
                        });
                        ui.horizontal(|ui| {
                            let mut edits = self.manual_edits_enabled;
                            if ui.checkbox(&mut edits, "✏️ Edit").changed() {
                                self.manual_edits_enabled = edits;
                                if !edits {
                                    self.inspected_file_edited = false;
                                    self.inspect_file(&filename);
                                }
                            }
                            if self.manual_edits_enabled {
                                if ui.button("💾 Save").clicked() {
                                    if let Err(e) = self.save_inspected_file_changes() {
                                        self.inspector_error = Some(format!("Failed to save: {e}"));
                                    } else {
                                        self.status_text = "Saved!".to_string();
                                    }
                                }
                            }
                        });
                        ui.add_space(6.0);

                        if let Some(error) = &self.inspector_error {
                            ui.add_space(6.0);
                            ui.label(
                                egui::RichText::new(error)
                                    .size(12.0)
                                    .color(egui::Color32::from_rgb(248, 113, 113)),
                            );
                        } else {
                            let is_active_writing = self
                                .current_writing_file
                                .as_ref()
                                .map_or(false, |(name, _, _)| {
                                    Some(name) == self.selected_file.as_ref()
                                });

                            let active_range = self.get_active_editing_range();
                            let is_active_file = self.selected_file.as_ref().is_some() 
                                && self.selected_file.as_ref() == self.current_writing_file.as_ref().map(|(n, _, _)| n);

                            egui::ScrollArea::both()
                                .id_salt("inline_file_inspector")
                                .auto_shrink([false, false])
                                .stick_to_bottom(is_active_writing)
                                .show(ui, |ui| {
                                    if self.manual_edits_enabled {
                                        let edit_resp = ui.add(
                                            egui::TextEdit::multiline(&mut self.inspected_file_content)
                                                .font(egui::TextStyle::Monospace)
                                                .desired_width(f32::INFINITY)
                                                .interactive(true),
                                        );
                                        if edit_resp.changed() {
                                            self.inspected_file_edited = true;
                                        }
                                    } else {
                                        let lines: Vec<&str> = self.inspected_file_content.lines().collect();
                                        ui.vertical(|ui| {
                                            let start_line_num = if self.line_range_enabled { self.line_range_start } else { 1 };
                                            for (idx, line_content) in lines.iter().enumerate() {
                                                let line_num = start_line_num + idx;
                                                let is_highlighted = is_active_file && active_range.map_or(false, |(start, end)| {
                                                    line_num >= start && line_num <= end
                                                });

                                                let line_text = format!("{:>4} | {}", line_num, line_content);
                                                let mut rich_text = egui::RichText::new(line_text)
                                                    .font(egui::FontId::monospace(11.0));

                                                if is_highlighted {
                                                    rich_text = rich_text
                                                        .color(egui::Color32::from_rgb(16, 185, 129))
                                                        .strong();
                                                } else {
                                                    rich_text = rich_text.color(if self.dark_mode {
                                                        egui::Color32::from_rgb(209, 213, 219)
                                                    } else {
                                                        egui::Color32::from_rgb(55, 65, 81)
                                                    });
                                                }

                                                let response = ui.horizontal(|ui| {
                                                    if is_highlighted {
                                                        let bg = if self.dark_mode {
                                                            egui::Color32::from_rgb(20, 45, 30)
                                                        } else {
                                                            egui::Color32::from_rgb(230, 253, 240)
                                                        };
                                                        let rect = ui.max_rect();
                                                        ui.painter().rect_filled(rect, 0.0, bg);
                                                    }
                                                    ui.label(rich_text)
                                                }).response;

                                                if is_active_file {
                                                    if let Some((start, _)) = active_range {
                                                        if self.should_scroll_to_line == Some(start) && line_num == start {
                                                            response.scroll_to_me(Some(egui::Align::Center));
                                                        }
                                                    }
                                                }
                                            }
                                        });
                                    }
                                });
                        }
                    }
                });
            if !right_animating && self.right_sidebar_open {
                self.right_sidebar_width = panel_response.response.rect.width();
            }
        }

        // Bottom Chat Input panel docked at the bottom of the center area (below the main chat, between sidebars)
        egui::TopBottomPanel::bottom("bottom_input_panel")
            .frame(
                egui::Frame::default()
                    .inner_margin(egui::Margin::symmetric(18.0, 14.0))
                    .fill(if self.dark_mode {
                        egui::Color32::from_rgb(12, 13, 19)
                    } else {
                        egui::Color32::from_rgb(248, 249, 252)
                    })
                    .stroke(egui::Stroke::new(
                        1.0,
                        if self.dark_mode {
                            egui::Color32::from_rgb(37, 35, 50)
                        } else {
                            egui::Color32::from_rgb(220, 223, 235)
                        },
                    )),
            )
            .resizable(false)
            .show(ctx, |ui| {
                if self.current_session_id.is_some() {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("📁 Loaded Session (Active)")
                                .size(11.0)
                                .strong()
                                .color(egui::Color32::from_rgb(16, 185, 129)),
                        );
                        ui.label(
                            egui::RichText::new("You can continue chatting to update this session thread.")
                                .size(11.0)
                                .color(egui::Color32::from_rgb(148, 163, 184)),
                        );
                    });
                }

                // Modal/Overlay for Tool Approval requests (rendered right above prompt box inside bottom panel)
                let mut user_response = None;
                if let Some(approval) = &self.approval_pending {
                    let tool_name = approval.tool_name.clone();
                    let args_preview = approval.args_preview.clone();

                    ui.centered_and_justified(|ui| {
                        let available_width = ui.available_width();
                        let max_chat_width: f32 = 920.0;
                        let chat_width = max_chat_width.min(available_width - 36.0).max(200.0);
                        let horizontal_margin = ((available_width - chat_width) / 2.0).max(0.0);

                        ui.horizontal(|ui| {
                            ui.add_space(horizontal_margin);

                            egui::Frame::group(ui.style())
                                .fill(if self.dark_mode {
                                    egui::Color32::from_rgb(30, 32, 45)
                                } else {
                                    egui::Color32::from_rgb(254, 243, 199)
                                })
                                .stroke(egui::Stroke::new(
                                    1.0,
                                    egui::Color32::from_rgb(234, 179, 8),
                                ))
                                .inner_margin(14.0)
                                .rounding(10.0)
                                .show(ui, |ui| {
                                    ui.set_width(chat_width);
                                    ui.vertical(|ui| {
                                        ui.colored_label(
                                            egui::Color32::from_rgb(234, 179, 8),
                                            "⚠️ User Approval Required for Tool Call:",
                                        );
                                        ui.label(
                                            egui::RichText::new(format!("Tool: {}", tool_name))
                                                .strong(),
                                        );
                                        let mut target_summary = String::new();
                                        if let Ok(v) =
                                            serde_json::from_str::<serde_json::Value>(&args_preview)
                                        {
                                            if let Some(path) = v
                                                .get("path")
                                                .or_else(|| v.get("TargetFile"))
                                                .and_then(|x| x.as_str())
                                            {
                                                target_summary = format!("File: {path}");
                                            } else if let Some(cmd) =
                                                v.get("CommandLine").and_then(|x| x.as_str())
                                            {
                                                target_summary = format!("Command: {cmd}");
                                            } else if let Some(url) =
                                                v.get("Url").and_then(|x| x.as_str())
                                            {
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
                                                                "{}: {}...",
                                                                k,
                                                                &val_str[..60]
                                                            ));
                                                        } else {
                                                            parts.push(format!(
                                                                "{}: {}",
                                                                k, val_str
                                                            ));
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
                                            target_summary.truncate(120);
                                            target_summary.push_str("...");
                                        }
                                        ui.horizontal(|ui| {
                                            ui.label(egui::RichText::new("Target:").strong());
                                            ui.code(&target_summary);
                                        });
                                        ui.add_space(8.0);
                                        ui.horizontal(|ui| {
                                            if ui.button("✅ Approve").clicked() {
                                                user_response = Some(true);
                                            }
                                            ui.add_space(10.0);
                                            if ui.button("❌ Deny").clicked() {
                                                user_response = Some(false);
                                            }
                                        });
                                    });
                                });
                        });
                    });
                    ui.add_space(8.0);
                }

                if let Some(approved) = user_response {
                    if let Some(_approval) = self.approval_pending.take() {
                        let _ = self.approval_tx.send(approved);
                    }
                }
                let available_width = ui.available_width();
                let max_chat_width: f32 = 920.0;
                let chat_width = max_chat_width.min(available_width - 36.0).max(200.0);
                let horizontal_margin = ((available_width - chat_width) / 2.0).max(0.0);

                if let Some(ref path) = self.attached_image {
                    let filename = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("file");
                    let tag_text = format!("📎 {filename} ❌");
                    ui.horizontal(|ui| {
                        ui.add_space(horizontal_margin + 12.0);
                        let bg_color = if self.dark_mode {
                            egui::Color32::from_rgb(30, 24, 45)
                        } else {
                            egui::Color32::from_rgb(245, 243, 255)
                        };
                        egui::Frame::default()
                            .fill(bg_color)
                            .inner_margin(egui::Margin::symmetric(8.0, 4.0))
                            .rounding(6.0)
                            .show(ui, |ui| {
                                if ui
                                    .button(
                                        egui::RichText::new(tag_text)
                                            .color(egui::Color32::from_rgb(239, 68, 68))
                                            .size(12.0),
                                    )
                                    .on_hover_text("Click to remove file")
                                    .clicked()
                                {
                                    self.attached_image = None;
                                }
                            });
                    });
                    ui.add_space(4.0);
                }

                ui.horizontal(|ui| {
                    ui.add_space(horizontal_margin);

                    // Bottom Input bar enclosed in a premium card frame
                    egui::Frame::default()
                        .fill(if self.dark_mode {
                            egui::Color32::from_rgb(19, 20, 30)
                        } else {
                            egui::Color32::WHITE
                        })
                        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(91, 70, 145)))
                        .inner_margin(9.0)
                        .rounding(14.0)
                        .shadow(egui::epaint::Shadow {
                            offset: egui::vec2(0.0, 4.0),
                            blur: 14.0,
                            spread: 0.0,
                            color: egui::Color32::from_black_alpha(60),
                        })
                        .show(ui, |ui| {
                            ui.set_width(chat_width - 24.0);
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                if attachment_button(ui, self.dark_mode).clicked() {
                                    if let Some(path) = rfd::FileDialog::new().pick_file() {
                                        self.attached_image = Some(path);
                                        self.status_text = "File attached!".to_string();
                                    }
                                }

                                // Text prompt input area with dynamic remaining width calculation
                                let buttons_w = if self.is_processing { 170.0 } else { 85.0 };
                                let remaining_w = (ui.available_width() - buttons_w).max(80.0);
                                let mut edit_response_opt = None;

                                egui::ScrollArea::vertical()
                                    .max_height(120.0)
                                    .id_salt("prompt_input_scroll")
                                    .show(ui, |ui| {
                                        let resp = ui.add(
                                            egui::TextEdit::multiline(&mut self.input_text)
                                                .hint_text(
                                                    egui::RichText::new("Ask Norvexum anything…")
                                                        .color(egui::Color32::from_rgb(148, 163, 184))
                                                )
                                                .text_color(if self.dark_mode {
                                                    egui::Color32::from_rgb(235, 237, 245)
                                                } else {
                                                    egui::Color32::from_rgb(30, 41, 59)
                                                })
                                                .desired_rows(1)
                                                .margin(egui::vec2(10.0, 8.0))
                                                .interactive(true)
                                                .desired_width(remaining_w),
                                        );
                                        edit_response_opt = Some(resp);
                                    });

                                let edit_response = edit_response_opt.unwrap();

                                if edit_response.has_focus()
                                    && ui.input(|i| {
                                        i.key_pressed(egui::Key::Enter) && !i.modifiers.shift
                                    })
                                {
                                    self.send_message();
                                    edit_response.request_focus();
                                }

                                if self.is_processing {
                                    ui.horizontal(|ui| {
                                        if ui
                                            .add_enabled(
                                                !self.input_text.trim().is_empty(),
                                                egui::Button::new(
                                                    egui::RichText::new("Send  ↑")
                                                        .color(egui::Color32::WHITE)
                                                        .strong()
                                                        .size(14.0),
                                                )
                                                .fill(egui::Color32::from_rgb(109, 40, 217))
                                                .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(167, 139, 250)))
                                                .rounding(10.0),
                                            )
                                            .clicked()
                                        {
                                            self.send_message();
                                            edit_response.request_focus();
                                        }

                                        if ui
                                            .add(
                                                egui::Button::new(
                                                    egui::RichText::new("■  Stop")
                                                        .color(egui::Color32::WHITE)
                                                        .strong(),
                                                )
                                                .fill(egui::Color32::from_rgb(190, 52, 74))
                                                .rounding(10.0),
                                            )
                                            .on_hover_text("Stop the current response")
                                            .clicked()
                                        {
                                            let _ = self.cancel_tx.send(());
                                            self.is_processing = false;
                                            self.status_text = "Stopped".to_string();
                                        }
                                    });
                                } else if ui
                                    .add_enabled(
                                        !self.input_text.trim().is_empty(),
                                        egui::Button::new(
                                            egui::RichText::new("Send  ↑")
                                                .color(egui::Color32::WHITE)
                                                .strong()
                                                .size(14.0),
                                        )
                                        .fill(egui::Color32::from_rgb(109, 40, 217))
                                        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(167, 139, 250)))
                                        .rounding(10.0),
                                    )
                                    .clicked()
                                {
                                    self.send_message();
                                    edit_response.request_focus();
                                }
                            });
                        });

                    ui.add_space(horizontal_margin);
                });
            });

        // Terminal Panel (togglable, sits above input panel)
        if self.show_terminal_panel {
            egui::TopBottomPanel::bottom("terminal_panel")
                .frame(
                    egui::Frame::default()
                        .inner_margin(egui::Margin::symmetric(14.0, 10.0))
                        .fill(if self.dark_mode {
                            egui::Color32::from_rgb(8, 10, 14)
                        } else {
                            egui::Color32::from_rgb(24, 24, 32)
                        })
                        .stroke(egui::Stroke::new(
                            1.0,
                            if self.dark_mode {
                                egui::Color32::from_rgb(30, 40, 35)
                            } else {
                                egui::Color32::from_rgb(50, 55, 65)
                            },
                        )),
                )
                .resizable(true)
                .default_height(180.0)
                .max_height(400.0)
                .min_height(80.0)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("⬛ TERMINAL")
                                .size(10.0)
                                .strong()
                                .color(egui::Color32::from_rgb(52, 211, 153)),
                        );
                        ui.label(
                            egui::RichText::new(format!("({} commands)", self.terminal_entries.len()))
                                .size(10.0)
                                .color(egui::Color32::from_rgb(100, 116, 139)),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("Clear")
                                            .size(10.0)
                                            .color(egui::Color32::from_rgb(148, 163, 184)),
                                    )
                                    .fill(egui::Color32::TRANSPARENT)
                                    .stroke(egui::Stroke::NONE),
                                )
                                .clicked()
                            {
                                self.terminal_entries.clear();
                            }
                        });
                    });
                    ui.add_space(4.0);

                    let is_running_cmd = self.is_processing && self.current_tool_name.as_ref().map_or(false, |name| {
                        name == "run_command" || name == "shell" || name == "shell_command" || name == "execute"
                    });

                    egui::ScrollArea::vertical()
                        .stick_to_bottom(true)
                        .auto_shrink([false; 2])
                        .id_salt("terminal_scroll")
                        .show(ui, |ui| {
                            if self.terminal_entries.is_empty() && !is_running_cmd {
                                ui.label(
                                    egui::RichText::new("No commands executed yet. Agent terminal output will appear here.")
                                        .size(11.0)
                                        .italics()
                                        .color(egui::Color32::from_rgb(75, 85, 99)),
                                );
                            } else {
                                let entries = self.terminal_entries.clone();
                                for (i, entry) in entries.iter().enumerate() {
                                    // Command prompt line
                                    let cmd_color = if entry.success {
                                        egui::Color32::from_rgb(52, 211, 153)
                                    } else {
                                        egui::Color32::from_rgb(248, 113, 113)
                                    };
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(
                                            egui::RichText::new("$")
                                                .size(12.0)
                                                .strong()
                                                .color(cmd_color)
                                                .monospace(),
                                        );
                                        ui.label(
                                            egui::RichText::new(&entry.command)
                                                .size(12.0)
                                                .strong()
                                                .color(egui::Color32::from_rgb(226, 232, 240))
                                                .monospace(),
                                        );
                                    });

                                    // Output (truncated to prevent huge renders)
                                    if !entry.output.is_empty() {
                                        let output_text = if entry.output.len() > 2000 {
                                            format!("{}…\n(truncated)", &entry.output[..2000])
                                        } else {
                                            entry.output.clone()
                                        };
                                        ui.label(
                                            egui::RichText::new(&output_text)
                                                .size(11.0)
                                                .color(egui::Color32::from_rgb(148, 163, 184))
                                                .monospace(),
                                        );
                                    }

                                    if i + 1 < entries.len() || is_running_cmd {
                                        ui.add_space(6.0);
                                        let separator_color = egui::Color32::from_rgb(30, 35, 45);
                                        let rect = ui.available_rect_before_wrap();
                                        let y = rect.top();
                                        ui.painter().line_segment(
                                            [
                                                egui::pos2(rect.left(), y),
                                                egui::pos2(rect.right(), y),
                                            ],
                                            egui::Stroke::new(0.5, separator_color),
                                        );
                                        ui.add_space(6.0);
                                    }
                                }

                                // If currently executing a shell command, show it live!
                                if is_running_cmd {
                                    let cmd = extract_streaming_content(&self.current_tool_args, "command");
                                    let mut cmd_line = if cmd.is_empty() {
                                        extract_streaming_content(&self.current_tool_args, "CommandLine")
                                    } else {
                                        cmd
                                    };
                                    if cmd_line.is_empty() {
                                        cmd_line = extract_streaming_content(&self.current_tool_args, "cmd");
                                    }

                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(
                                            egui::RichText::new("$")
                                                .size(12.0)
                                                .strong()
                                                .color(egui::Color32::from_rgb(167, 139, 250)) // Purple prompt for active
                                                .monospace(),
                                        );
                                        
                                        // Make block character blink slightly (using sinus animation)
                                        let time = ui.input(|i| i.time);
                                        let cursor = if (time * 2.0) as i64 % 2 == 0 { "█" } else { " " };
                                        
                                        ui.label(
                                            egui::RichText::new(format!("{} {}", cmd_line, cursor))
                                                .size(12.0)
                                                .strong()
                                                .color(egui::Color32::from_rgb(226, 232, 240))
                                                .monospace(),
                                        );
                                    });
                                    
                                    // Request repaint for blinking cursor
                                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
                                }
                            }
                        });
                });
        }

        // Center Panel (Chat Conversation Area)
        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .inner_margin(egui::Margin::symmetric(24.0, 18.0))
                    .fill(if self.dark_mode {
                        egui::Color32::from_rgb(10, 11, 16)
                    } else {
                        egui::Color32::from_rgb(246, 247, 251)
                    }),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new("CONVERSATION")
                                .size(11.0)
                                .strong()
                                .color(egui::Color32::from_rgb(167, 139, 250)),
                        );
                        ui.label(
                            egui::RichText::new("Norvexum Agent")
                                .font(egui::FontId::proportional(20.0))
                                .strong(),
                        );
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(
                                !self.is_processing,
                                egui::Button::new(
                                    egui::RichText::new("+  New chat")
                                        .color(if self.dark_mode {
                                            egui::Color32::from_rgb(221, 214, 254)
                                        } else {
                                            egui::Color32::from_rgb(91, 33, 182)
                                        })
                                        .strong(),
                                )
                                .fill(if self.dark_mode {
                                    egui::Color32::from_rgb(37, 29, 58)
                                } else {
                                    egui::Color32::from_rgb(237, 233, 254)
                                })
                                .stroke(egui::Stroke::new(
                                    1.0,
                                    egui::Color32::from_rgb(124, 58, 237),
                                ))
                                .rounding(10.0),
                            )
                            .on_hover_text("Start a fresh conversation")
                            .clicked()
                        {
                            let _ = self.user_tx.send("/clear".to_string());
                            self.messages.clear();
                            self.session_written_files.clear();
                            self.file_diff_stats.clear();
                            self.terminal_entries.clear();
                            self.current_session_id = None;
                            self.active_messages_backup = None;
                            self.selected_file = None;
                            self.status_text = "Chat cleared".to_string();
                        }
                    });
                });
                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);

                enum PendingAction {
                    Edit(String),
                    Reroll(String),
                    Undo,
                }
                let mut pending_action = None;

                let has_user_messages = self.messages.iter().any(|m| m.role == "user");
                if !has_user_messages && !self.is_processing {
                    ui.add_space((ui.available_height() * 0.20).min(120.0));
                    ui.vertical_centered(|ui| {
                        ui.add(egui::Image::new(&self.logo_tex).max_width(54.0));
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new("What shall we build?")
                                .font(egui::FontId::proportional(24.0))
                                .strong(),
                        );
                        ui.label(
                            egui::RichText::new(
                                "Ask Norvexum to explore, explain, or change your workspace.",
                            )
                            .size(13.0)
                            .color(egui::Color32::from_rgb(148, 163, 184)),
                        );
                        for msg in &self.messages {
                            if msg.content.contains("🧹") || msg.content.contains("cleared") {
                                ui.add_space(16.0);
                                ui.colored_label(
                                    egui::Color32::from_rgb(110, 120, 140),
                                    &msg.content,
                                );
                            }
                        }
                    });
                    ui.add_space(24.0);
                } else {
                    // Chat Scroll area takes up 100% of the CentralPanel!
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            let available_chat_width = ui.available_width();
                            ui.set_min_width(available_chat_width);

                            let last_agent_idx = self
                                .messages
                                .iter()
                                .enumerate()
                                .rfind(|(_, m)| m.role == "assistant")
                                .map(|(idx, _)| idx);

                            for (msg_idx, msg) in self.messages.iter().enumerate() {
                                if msg.role == "system" {
                                    continue;
                                }
                                let is_user = msg.role == "user";
                                let is_status = msg.role == "status";
                                let is_err = msg.role == "error";

                                if is_status {
                                    ui.centered_and_justified(|ui| {
                                        let display_text = if msg.content.contains('🧹') {
                                            msg.content.clone()
                                        } else {
                                            format!("ℹ️ {}", msg.content)
                                        };
                                        ui.colored_label(
                                            egui::Color32::from_rgb(110, 120, 140),
                                            display_text,
                                        );
                                    });
                                } else if is_err {
                                    ui.centered_and_justified(|ui| {
                                        ui.colored_label(
                                            egui::Color32::from_rgb(239, 68, 68),
                                            format!("⚠️ {}", msg.content),
                                        );
                                    });
                                } else {
                                    let alignment = if is_user {
                                        egui::Align::RIGHT
                                    } else {
                                        egui::Align::LEFT
                                    };

                                    ui.allocate_ui_with_layout(
                                         egui::vec2(available_chat_width, 0.0),
                                         egui::Layout::top_down(alignment),
                                         |ui| {
                                             let msg_response = ui
                                                 .with_layout(egui::Layout::top_down(alignment), |ui| {
                                                     if is_user {
                                                         // User speech bubble: clean slate background
                                                         let max_bubble_width =
                                                             (available_chat_width * 0.72)
                                                                 .clamp(260.0, 760.0);
                                                         let inner_pad_x = 16.0;
                                                         let inner_pad_total = inner_pad_x * 2.0;
                                                         let text_max_width =
                                                             (max_bubble_width - inner_pad_total)
                                                                 .max(60.0);

                                                         let theme_color = if self.dark_mode {
                                                             egui::Color32::from_rgb(47, 37, 72)
                                                         } else {
                                                             egui::Color32::from_rgb(237, 233, 254)
                                                         };
                                                         let text_color = if self.dark_mode {
                                                             egui::Color32::WHITE
                                                         } else {
                                                             egui::Color32::from_rgb(30, 41, 59)
                                                         };
                                                         let stroke_color = if self.dark_mode {
                                                             egui::Color32::from_rgb(91, 70, 145)
                                                         } else {
                                                             egui::Color32::from_rgb(196, 181, 253)
                                                         };

                                                         // Smooth fade-in animation for newly-arriving user bubble
                                                         let entry_t = ui
                                                             .ctx()
                                                             .animate_value_with_time(
                                                                 egui::Id::new(format!("user_bubble_in_{msg_idx}")),
                                                                 1.0,
                                                                 0.16,
                                                             )
                                                             .clamp(0.0, 1.0);
                                                         let entry_alpha = 1.0 - (1.0 - entry_t).powi(3);

                                                         // Measure the exact rendered text width with the proportional font
                                                         let galley = ui.fonts(|f| {
                                                             f.layout(
                                                                 msg.content.clone(),
                                                                 egui::FontId::proportional(13.0),
                                                                 text_color,
                                                                 text_max_width,
                                                             )
                                                         });
                                                         let text_w = galley.size().x.ceil();
                                                         let bubble_w = (text_w + inner_pad_total)
                                                             .min(max_bubble_width)
                                                             .max(120.0);

                                                         ui.label(
                                                             egui::RichText::new("YOU")
                                                                 .size(9.0)
                                                                 .strong()
                                                                 .color(
                                                                     egui::Color32::from_rgb(
                                                                         167, 139, 250,
                                                                     )
                                                                     .linear_multiply(entry_alpha),
                                                                 ),
                                                         );
                                                         ui.add_space(4.0);

                                                         let tinted_fill = theme_color.linear_multiply(entry_alpha);
                                                         let tinted_stroke = egui::Stroke::new(
                                                             1.0,
                                                             stroke_color.linear_multiply(entry_alpha),
                                                         );

                                                         ui.allocate_ui_with_layout(
                                                             egui::vec2(bubble_w, 0.0),
                                                             egui::Layout::top_down(egui::Align::LEFT),
                                                             |ui| {
                                                                 ui.set_max_width(bubble_w);
                                                                 egui::Frame::default()
                                                                     .fill(tinted_fill)
                                                                     .stroke(tinted_stroke)
                                                                     .inner_margin(
                                                                         egui::Margin::symmetric(
                                                                             inner_pad_x, 12.0,
                                                                         ),
                                                                     )
                                                                     .rounding(14.0)
                                                                     .show(ui, |ui| {
                                                                         if msg.content.len() > 1000 {
                                                                             egui::ScrollArea::vertical()
                                                                                 .max_height(220.0)
                                                                                 .id_salt(format!("user_scroll_{}", msg_idx))
                                                                                 .show(ui, |ui| {
                                                                                     render_formatted_markdown(
                                                                                         ui,
                                                                                         &msg.content,
                                                                                         self.dark_mode,
                                                                                     );
                                                                                 });
                                                                         } else {
                                                                             render_formatted_markdown(
                                                                                 ui,
                                                                                 &msg.content,
                                                                                 self.dark_mode,
                                                                             );
                                                                         }
                                                                     });
                                                             },
                                                         );
                                                    } else {
                                                        // Agent response: subtle left accent bar + clean background
                                                        let agent_width = (available_chat_width - 36.0)
                                                                .clamp(260.0, 960.0);
                                                        let accent_color = if self.dark_mode {
                                                            egui::Color32::from_rgb(124, 58, 237)
                                                        } else {
                                                            egui::Color32::from_rgb(139, 92, 246)
                                                        };

                                                        // Paint a left accent bar
                                                        let before_rect = ui.cursor();
                                                        let accent_id = egui::Id::new(format!("agent_accent_{msg_idx}"));
                                                        let _ = accent_id; // used below after content

                                                        ui.set_width(agent_width);
                                                        ui.add_space(4.0);

                                                        let content_start_y = ui.cursor().top();

                                                        // Render thinking and tools chronologically interleaved
                                                        let is_latest_agent =
                                                            Some(msg_idx) == last_agent_idx;
                                                        let has_activity = !msg
                                                            .tool_logs
                                                            .is_empty()
                                                            || (msg.thinking.as_ref().is_some_and(
                                                                |t| !t.trim().is_empty(),
                                                            ))
                                                            || (is_latest_agent
                                                                && self.is_processing);

                                                        if has_activity {
                                                            let (curr_name, curr_args) =
                                                                if is_latest_agent
                                                                    && self.is_processing
                                                                {
                                                                    (
                                                                        &self.current_tool_name,
                                                                        self.current_tool_args
                                                                            .as_str(),
                                                                    )
                                                                } else {
                                                                    (&None, "")
                                                                };
                                                            render_chronological_activity(
                                                                ui,
                                                                &msg.tool_logs,
                                                                self.dark_mode,
                                                                curr_name,
                                                                curr_args,
                                                                self.is_processing
                                                                    && is_latest_agent,
                                                                &msg.thinking,
                                                                &self.settings.project_root,
                                                            );
                                                            ui.add_space(8.0);
                                                        }
                                                        ui.add_space(8.0);

                                                        // Render message body text directly
                                                        if !msg.content.is_empty() {
                                                            render_formatted_markdown(
                                                                ui,
                                                                &msg.content,
                                                                self.dark_mode,
                                                            );
                                                        }

                                                        // Paint the left accent bar
                                                        let content_end_y = ui.cursor().top();
                                                        if content_end_y > content_start_y + 2.0 {
                                                            let bar_x = before_rect.left() - 6.0;
                                                            ui.painter().line_segment(
                                                                [
                                                                    egui::pos2(bar_x, content_start_y),
                                                                    egui::pos2(bar_x, content_end_y - 4.0),
                                                                ],
                                                                egui::Stroke::new(2.5, accent_color.linear_multiply(0.6)),
                                                            );
                                                        }
                                                    }
                                                })
                                                .response;

                                            let mut hover_rect = msg_response.rect;
                                            hover_rect.min.x = ui.max_rect().min.x;
                                            hover_rect.max.x = ui.max_rect().max.x;
                                            let hover_rect = hover_rect.expand2(egui::vec2(0.0, 32.0));
                                            let is_hovered = ui
                                                .ctx()
                                                .pointer_hover_pos()
                                                .map(|pos| hover_rect.contains(pos))
                                                .unwrap_or(false)
                                                || msg_response.hovered();

                                             if is_user {
                                                 ui.allocate_ui_with_layout(
                                                     egui::vec2(ui.available_width(), 22.0),
                                                     egui::Layout::top_down(egui::Align::LEFT),
                                                     |ui| {
                                                         ui.set_height(22.0);
                                                         if is_hovered {
                                                             ui.with_layout(
                                                                 egui::Layout::right_to_left(
                                                                     egui::Align::Center,
                                                                 ),
                                                                 |ui| {
                                                                     if ui.small_button("Copy").clicked() {
                                                                         ui.ctx().copy_text(msg.content.clone());
                                                                     }
                                                                     if ui.small_button("Edit").clicked() {
                                                                         pending_action =
                                                                             Some(PendingAction::Edit(
                                                                                 msg.content.clone(),
                                                                             ));
                                                                     }
                                                                     if ui
                                                                         .small_button("Re-run")
                                                                         .on_hover_text("Send this prompt again")
                                                                         .clicked()
                                                                     {
                                                                         pending_action =
                                                                             Some(PendingAction::Reroll(
                                                                                 msg.content.clone(),
                                                                             ));
                                                                     }
                                                                     if ui
                                                                         .small_button("Undo")
                                                                         .on_hover_text(
                                                                             "Undo the last tool change",
                                                                         )
                                                                         .clicked()
                                                                     {
                                                                         pending_action =
                                                                             Some(PendingAction::Undo);
                                                                     }
                                                                 },
                                                             );
                                                         }
                                                     },
                                                 );
                                             } else {
                                                 ui.allocate_ui_with_layout(
                                                     egui::vec2(ui.available_width(), 22.0),
                                                     egui::Layout::top_down(egui::Align::LEFT),
                                                     |ui| {
                                                         ui.set_height(22.0);
                                                         ui.horizontal(|ui| {
                                                             let is_latest_agent =
                                                                 Some(msg_idx) == last_agent_idx;
                                                             if is_latest_agent && !self.is_processing {
                                                                 draw_layered_logo_loader(
                                                                     ui,
                                                                     &self.last_layer_tex,
                                                                     &self.middle_layer_tex,
                                                                     &self.top_layer_tex,
                                                                     false,
                                                                     22.0,
                                                                 );
                                                             }
                                                             if is_hovered {
                                                                 if ui.small_button("Copy").clicked() {
                                                                     ui.ctx().copy_text(msg.content.clone());
                                                                 }
                                                                 if ui
                                                                     .small_button("Undo")
                                                                     .on_hover_text(
                                                                         "Undo the last tool change",
                                                                     )
                                                                     .clicked()
                                                                 {
                                                                     pending_action =
                                                                         Some(PendingAction::Undo);
                                                                 }
                                                             }
                                                         });
                                                     },
                                                 );
                                             }
                                        },
                                    );
                                }
                                ui.add_space(10.0);
                            }

                            // Tiny rotating loader spinner under the last message
                            if self.is_processing {
                                let loader_text = if let Some(ref tool_name) =
                                    self.current_tool_name
                                {
                                    match tool_name.as_str() {
                                        "write_file"
                                        | "write_to_file"
                                        | "edit_file"
                                        | "replace_file_content"
                                        | "multi_replace_file_content" => {
                                            "Norvexum agent is writing...".to_string()
                                        }
                                        "read_file" | "view_file" | "cat" => {
                                            "Norvexum agent is reading...".to_string()
                                        }
                                        "read_skill" => {
                                            let skill_name = extract_streaming_content(
                                                &self.current_tool_args,
                                                "name",
                                            );
                                            if skill_name.is_empty() {
                                                "Norvexum agent is reading skill...".to_string()
                                            } else {
                                                format!(
                                                    "Norvexum agent is reading skill {}...",
                                                    skill_name
                                                )
                                            }
                                        }
                                        "web_search" | "search_web" => {
                                            "Norvexum agent is searching...".to_string()
                                        }
                                        "web_fetch" | "read_browser_page" | "browser_subagent" => {
                                            "Norvexum agent is browsing...".to_string()
                                        }
                                        "run_command" | "shell" => {
                                            "Norvexum agent is executing command...".to_string()
                                        }
                                        _ => {
                                            format!(
                                                "Norvexum agent is executing {}...",
                                                tool_name.replace('_', " ")
                                            )
                                        }
                                    }
                                } else {
                                    let secs = self.thinking_start_time.map_or(0, |s| s.elapsed().as_secs());
                                    if secs < 60 {
                                        format!("Norvexum agent is thinking ({secs}s)...")
                                    } else {
                                        let mins = secs / 60;
                                        let remaining = secs % 60;
                                        format!("Norvexum agent is thinking ({mins}m {remaining}s)...")
                                    }
                                };

                                ui.horizontal(|ui| {
                                    draw_layered_logo_loader(
                                        ui,
                                        &self.last_layer_tex,
                                        &self.middle_layer_tex,
                                        &self.top_layer_tex,
                                        true,
                                        24.0,
                                    );

                                    ui.label(
                                        egui::RichText::new(loader_text)
                                            .italics()
                                            .size(12.0)
                                            .color(egui::Color32::from_rgb(190, 198, 218)),
                                    );
                                });

                                if let Some(ref name) = self.current_tool_name {
                                    let is_write = name == "write_file"
                                        || name == "write_to_file"
                                        || name == "edit_file"
                                        || name == "replace_file_content"
                                        || name == "multi_replace_file_content";
                                    if is_write {
                                        if let Some((ref filename, added, deleted)) =
                                            self.current_writing_file
                                        {
                                            let (pill_fill, pill_stroke, pill_label) =
                                                if self.dark_mode {
                                                    (
                                                        egui::Color32::from_rgb(40, 30, 18),
                                                        egui::Color32::from_rgb(120, 80, 30),
                                                        egui::Color32::from_rgb(253, 224, 168),
                                                    )
                                                } else {
                                                    (
                                                        egui::Color32::from_rgb(255, 247, 237),
                                                        egui::Color32::from_rgb(214, 158, 46),
                                                        egui::Color32::from_rgb(120, 53, 15),
                                                    )
                                                };
                                            ui.horizontal(|ui| {
                                                ui.add_space(28.0);
                                                egui::Frame::default()
                                                    .fill(pill_fill)
                                                    .stroke(egui::Stroke::new(1.0, pill_stroke))
                                                    .inner_margin(egui::Margin::symmetric(8.0, 4.0))
                                                    .rounding(6.0)
                                                    .show(ui, |ui| {
                                                        ui.horizontal(|ui| {
                                                            ui.label(
                                                                egui::RichText::new(format!(
                                                                    "Writing {}",
                                                                    filename
                                                                ))
                                                                .size(11.0)
                                                                .color(pill_label)
                                                                .strong()
                                                                .monospace(),
                                                            );
                                                            ui.add_space(6.0);
                                                            if added > 0 {
                                                                ui.label(
                                                                    egui::RichText::new(format!(
                                                                        "+{}",
                                                                        added
                                                                    ))
                                                                    .size(11.0)
                                                                    .strong()
                                                                    .color(egui::Color32::from_rgb(
                                                                        34, 197, 94,
                                                                    )),
                                                                );
                                                            }
                                                            if deleted > 0 {
                                                                ui.label(
                                                                    egui::RichText::new(format!(
                                                                        "-{}",
                                                                        deleted
                                                                    ))
                                                                    .size(11.0)
                                                                    .strong()
                                                                    .color(egui::Color32::from_rgb(
                                                                        239, 68, 68,
                                                                    )),
                                                                );
                                                            }
                                                        });
                                                    });
                                            });
                                        }
                                    } else {
                                        let args_summary =
                                            format_tool_args_summary(name, &self.current_tool_args);
                                        let text = format_tool_text(name, true, &args_summary);
                                        ui.horizontal(|ui| {
                                            ui.add_space(28.0);
                                            ui.label(
                                                egui::RichText::new(format!("{}...", text))
                                                    .small()
                                                    .color(egui::Color32::from_rgb(148, 163, 184))
                                                    .italics(),
                                            );
                                        });
                                    }
                                }
                            }
                        });
                }

                if let Some(act) = pending_action {
                    match act {
                        PendingAction::Edit(txt) => {
                            self.input_text = txt;
                        }
                        PendingAction::Reroll(txt) => {
                            self.input_text = txt;
                            self.send_message();
                        }
                        PendingAction::Undo => {
                            let _ = self.user_tx.send("/undo".to_string());
                            self.status_text = "Undo command sent...".to_string();
                        }
                    }
                }
            });

        // Draw the command approvals and project settings modal window if open
        if self.show_settings_modal {
            self.draw_settings_modal(ctx);
        }

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
            .with_inner_size([1100.0, 750.0])
            .with_icon(icon_data)
            .with_fullscreen(config.settings.fullscreen)
            .with_maximized(config.settings.maximized),
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

        #[cfg(windows)]
        let output_result = spawn_silent_command("git", &args, project_root);
        #[cfg(not(windows))]
        let output_result = std::process::Command::new("git")
            .args(&args)
            .current_dir(project_root)
            .output();

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

/// Spawn an external command on Windows without flashing a console window.
/// This is the same helper the agent's `run_command` tool should use to
/// keep cmd.exe from popping up on every tool call.
#[allow(dead_code)]
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

#[allow(dead_code)]
fn _unused_git_diff_compat(project_root: &std::path::Path, file_path: &str) -> (i32, i32) {
    // Kept for reference / fallback if anyone ever wants to re-enable the
    // git-based diff. Uses spawn_silent_command so it never flashes a window.
    let output = spawn_silent_command("git", &["diff", "--numstat", "--", file_path], project_root);

    if let Ok(out) = output {
        let stdout = String::from_utf8_lossy(&out.stdout);
        if let Some(line) = stdout.lines().next() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                let added = parts[0].parse::<i32>().unwrap_or(0);
                let deleted = parts[1].parse::<i32>().unwrap_or(0);
                return (added, deleted);
            }
        }
    }

    // Fallback: if not in git or no changes, read file and count total lines as additions
    let full_path = project_root.join(file_path);
    if let Ok(content) = std::fs::read_to_string(&full_path) {
        let lines = content.lines().count() as i32;
        return (lines, 0);
    }
    (0, 0)
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

fn render_formatted_markdown(ui: &mut egui::Ui, text: &str, dark_mode: bool) {
    let mut lines = text.lines().peekable();
    let text_color = if dark_mode {
        egui::Color32::from_rgb(226, 232, 240) // Slate 200
    } else {
        egui::Color32::from_rgb(30, 41, 59) // Slate 800
    };

    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        // Check if it's a table
        if trimmed.starts_with('|') {
            // Collect all consecutive table lines
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

            render_markdown_table(ui, &table_lines, dark_mode);
            ui.add_space(4.0);
            continue;
        }

        // Check if it's a heading
        if trimmed.starts_with("### ") {
            let heading_text = trimmed.strip_prefix("### ").unwrap().trim();
            ui.horizontal_wrapped(|ui| {
                ui.style_mut().spacing.item_spacing.x = 0.0;
                render_styled_line_parts(ui, heading_text, text_color, 15.0, true);
            });
            ui.add_space(6.0);
            continue;
        }
        if trimmed.starts_with("## ") {
            let heading_text = trimmed.strip_prefix("## ").unwrap().trim();
            ui.horizontal_wrapped(|ui| {
                ui.style_mut().spacing.item_spacing.x = 0.0;
                render_styled_line_parts(ui, heading_text, text_color, 17.0, true);
            });
            ui.add_space(6.0);
            continue;
        }
        if trimmed.starts_with("# ") {
            let heading_text = trimmed.strip_prefix("# ").unwrap().trim();
            ui.horizontal_wrapped(|ui| {
                ui.style_mut().spacing.item_spacing.x = 0.0;
                render_styled_line_parts(ui, heading_text, text_color, 20.0, true);
            });
            ui.add_space(8.0);
            continue;
        }

        // Bullet point
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            let content = &trimmed[2..];
            ui.horizontal_wrapped(|ui| {
                ui.style_mut().spacing.item_spacing.x = 0.0;
                ui.label(egui::RichText::new("  •  ").color(text_color).strong());
                render_styled_line_parts(ui, content, text_color, 13.0, false);
            });
            continue;
        }

        // Ordered list
        if trimmed.chars().next().map_or(false, |c| c.is_ascii_digit()) && trimmed.contains(". ") {
            if let Some(pos) = trimmed.find(". ") {
                let num = &trimmed[..pos + 2];
                let content = &trimmed[pos + 2..];
                ui.horizontal_wrapped(|ui| {
                    ui.style_mut().spacing.item_spacing.x = 0.0;
                    ui.label(
                        egui::RichText::new(format!("  {} ", num))
                            .color(text_color)
                            .strong(),
                    );
                    render_styled_line_parts(ui, content, text_color, 13.0, false);
                });
                continue;
            }
        }

        // Plain line
        if !trimmed.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.style_mut().spacing.item_spacing.x = 0.0;
                render_styled_line_parts(ui, trimmed, text_color, 13.0, false);
            });
        } else {
            ui.add_space(6.0);
        }
    }
}

fn render_styled_line_parts(
    ui: &mut egui::Ui,
    text: &str,
    text_color: egui::Color32,
    size: f32,
    force_bold: bool,
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
        let mut text_item = egui::RichText::new(&part).size(size);
        if is_code {
            text_item = text_item
                .font(egui::FontId::monospace(size - 1.0))
                .color(if ui.visuals().dark_mode {
                    egui::Color32::from_rgb(244, 63, 94)
                } else {
                    egui::Color32::from_rgb(225, 29, 72)
                })
                .background_color(if ui.visuals().dark_mode {
                    egui::Color32::from_rgb(30, 27, 40)
                } else {
                    egui::Color32::from_rgb(240, 240, 245)
                });
        } else {
            text_item = text_item.color(text_color);
            if force_bold || is_bold {
                text_item = text_item.strong();
            }
        }
        ui.add(egui::Label::new(text_item));
    }
}

fn render_markdown_table(ui: &mut egui::Ui, lines: &[String], dark_mode: bool) {
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

    // Compute a per-column max width so columns line up cleanly and the
    // table actually looks like a table instead of a stacked list.
    let mut col_widths = vec![0.0_f32; max_cols];
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            // Approximate glyph width: 7.5 px per char at size 12.
            let w = (cell.chars().count() as f32 * 7.5).max(40.0);
            if w > col_widths[i] {
                col_widths[i] = w;
            }
        }
    }
    // Cap each column so a single giant cell doesn't blow up the layout.
    let max_col_w = 320.0_f32;
    for w in &mut col_widths {
        *w = w.min(max_col_w);
    }

    let header_bg = if dark_mode {
        egui::Color32::from_rgb(48, 38, 78)
    } else {
        egui::Color32::from_rgb(237, 233, 254)
    };
    let header_fg = if dark_mode {
        egui::Color32::from_rgb(245, 243, 255)
    } else {
        egui::Color32::from_rgb(46, 16, 101)
    };
    let body_bg_a = if dark_mode {
        egui::Color32::from_rgb(20, 22, 32)
    } else {
        egui::Color32::from_rgb(255, 255, 255)
    };
    let body_bg_b = if dark_mode {
        egui::Color32::from_rgb(26, 28, 40)
    } else {
        egui::Color32::from_rgb(248, 250, 252)
    };
    let body_fg = if dark_mode {
        egui::Color32::from_rgb(220, 226, 240)
    } else {
        egui::Color32::from_rgb(30, 41, 59)
    };
    let border_color = if dark_mode {
        egui::Color32::from_rgb(50, 52, 70)
    } else {
        egui::Color32::from_rgb(203, 213, 225)
    };

    let total_width: f32 = col_widths.iter().sum::<f32>() + (max_cols as f32 - 1.0) * 16.0 + 24.0;

    egui::Frame::default()
        .fill(if dark_mode {
            egui::Color32::from_rgb(16, 18, 26)
        } else {
            egui::Color32::from_rgb(248, 250, 252)
        })
        .stroke(egui::Stroke::new(1.0, border_color))
        .inner_margin(0.0)
        .rounding(10.0)
        .show(ui, |ui| {
            ui.set_min_width(total_width.min(ui.available_width()));
            egui::Grid::new(ui.next_auto_id())
                .num_columns(max_cols)
                .spacing([0.0, 0.0])
                .min_col_width(40.0)
                .show(ui, |ui| {
                    // Header row
                    for (i, cell) in rows[0].iter().enumerate() {
                        let w = col_widths[i];
                        egui::Frame::default()
                            .fill(header_bg)
                            .stroke(egui::Stroke::new(1.0, border_color))
                            .inner_margin(egui::Margin::symmetric(12.0, 9.0))
                            .show(ui, |ui| {
                                ui.set_min_width(w);
                                ui.set_max_width(w + 24.0);
                                ui.horizontal_wrapped(|ui| {
                                    ui.style_mut().spacing.item_spacing.x = 0.0;
                                    render_styled_line_parts(ui, cell, header_fg, 12.5, true);
                                });
                            });
                    }
                    ui.end_row();

                    // Body rows
                    for (row_idx, row) in rows[1..].iter().enumerate() {
                        let row_bg = if row_idx % 2 == 0 {
                            body_bg_a
                        } else {
                            body_bg_b
                        };
                        for (i, cell) in row.iter().enumerate() {
                            let w = col_widths[i];
                            egui::Frame::default()
                                .fill(row_bg)
                                .stroke(egui::Stroke::new(1.0, border_color))
                                .inner_margin(egui::Margin::symmetric(12.0, 7.0))
                                .show(ui, |ui| {
                                    ui.set_min_width(w);
                                    ui.set_max_width(w + 24.0);
                                    ui.horizontal_wrapped(|ui| {
                                        ui.style_mut().spacing.item_spacing.x = 0.0;
                                        render_styled_line_parts(ui, cell, body_fg, 12.0, false);
                                    });
                                });
                        }
                        ui.end_row();
                    }
                });
        });
    ui.add_space(2.0);
}

fn scan_written_files(
    messages: &[crate::ai::types::Message],
    project_root: &std::path::Path,
) -> Vec<String> {
    let mut files = Vec::new();
    for msg in messages {
        for call in &msg.tool_calls {
            if call.name == "write_file"
                || call.name == "write_to_file"
                || call.name == "edit_file"
                || call.name == "replace_file_content"
                || call.name == "multi_replace_file_content"
            {
                let mut path_str = String::new();
                if let serde_json::Value::Object(map) = &call.arguments {
                    if let Some(serde_json::Value::String(p)) = map.get("path") {
                        path_str = p.clone();
                    } else if let Some(serde_json::Value::String(p)) = map.get("TargetFile") {
                        path_str = p.clone();
                    }
                } else if let serde_json::Value::String(s) = &call.arguments {
                    if let Ok(serde_json::Value::Object(map)) =
                        serde_json::from_str::<serde_json::Value>(s)
                    {
                        if let Some(serde_json::Value::String(p)) = map.get("path") {
                            path_str = p.clone();
                        } else if let Some(serde_json::Value::String(p)) = map.get("TargetFile") {
                            path_str = p.clone();
                        }
                    }
                }
                if !path_str.is_empty() {
                    let rel = get_relative_path(&path_str, project_root);
                    if !files.contains(&rel) {
                        files.push(rel);
                    }
                }
            }
        }
    }
    files
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
        "replace_file_content" | "multi_replace_file_content" | "edit_file" => "✏️",
        "view_file" | "read_file" => "📖",
        "list_dir" | "list_permissions" => "📁",
        "remove" | "delete_file" => "🗑️",
        _ => "🛠️",
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
                } else if c.is_whitespace() {
                    if !val_str.is_empty() {
                        break;
                    }
                } else {
                    if !val_str.is_empty() {
                        break;
                    }
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
                let filename = std::path::Path::new(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or(path);
                filename
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
                let filename = std::path::Path::new(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or(path);
                filename
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
                if cmd.len() > 60 {
                    format!("{}...", &cmd[..57])
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

fn render_chronological_activity(
    ui: &mut egui::Ui,
    logs: &[String],
    dark_mode: bool,
    current_tool_name: &Option<String>,
    current_tool_args: &str,
    is_processing: bool,
    fallback_thinking: &Option<String>,
    project_root: &std::path::Path,
) {
    let has_thinking_deltas = logs.iter().any(|log| log.starts_with("THINKING_DELTA|"));

    if !has_thinking_deltas {
        if let Some(thinking) = fallback_thinking {
            if !thinking.trim().is_empty() {
                egui::CollapsingHeader::new(
                    egui::RichText::new("💭 Thinking process")
                        .size(12.0)
                        .strong()
                        .color(if dark_mode {
                            egui::Color32::from_rgb(196, 181, 253)
                        } else {
                            egui::Color32::from_rgb(91, 33, 182)
                        }),
                )
                .id_salt(ui.next_auto_id())
                .default_open(false)
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(thinking.trim())
                                .font(egui::FontId::monospace(11.0))
                                .color(if dark_mode {
                                    egui::Color32::from_rgb(190, 198, 218)
                                } else {
                                    egui::Color32::from_rgb(71, 85, 105)
                                }),
                        )
                        .wrap(),
                    );
                });
                ui.add_space(4.0);
            }
        }
    }

    let mut current_loop_title: Option<String> = None;
    let mut current_loop_thinking = String::new();

    let mut render_loop_block = |ui: &mut egui::Ui, title: &str, thinking: &str| {
        if thinking.trim().is_empty() {
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(format!("⚙️ {}", title))
                        .size(12.0)
                        .color(if dark_mode {
                            egui::Color32::from_rgb(148, 163, 184)
                        } else {
                            egui::Color32::from_rgb(100, 116, 139)
                        }),
                );
            });
        } else {
            egui::CollapsingHeader::new(
                egui::RichText::new(format!("💭 {}", title))
                    .size(12.0)
                    .strong()
                    .color(if dark_mode {
                        egui::Color32::from_rgb(196, 181, 253)
                    } else {
                        egui::Color32::from_rgb(91, 33, 182)
                    }),
            )
            .id_salt(ui.next_auto_id())
            .default_open(is_processing)
            .show(ui, |ui| {
                let thinking_bg = if dark_mode {
                    egui::Color32::from_rgb(16, 17, 26)
                } else {
                    egui::Color32::from_rgb(245, 243, 255)
                };
                let thinking_border = if dark_mode {
                    egui::Color32::from_rgb(55, 45, 80)
                } else {
                    egui::Color32::from_rgb(196, 181, 253)
                };
                egui::Frame::default()
                    .inner_margin(egui::Margin::symmetric(10.0, 8.0))
                    .fill(thinking_bg)
                    .stroke(egui::Stroke::new(0.5, thinking_border))
                    .rounding(6.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(thinking.trim())
                                    .font(egui::FontId::monospace(11.0))
                                    .color(if dark_mode {
                                        egui::Color32::from_rgb(190, 198, 218)
                                    } else {
                                        egui::Color32::from_rgb(71, 85, 105)
                                    }),
                            )
                            .wrap(),
                        );
                    });
            });
        }
        ui.add_space(4.0);
    };

    for log in logs {
        if let Some(delta) = log.strip_prefix("THINKING_DELTA|") {
            current_loop_thinking.push_str(delta);
        } else if log.starts_with("ℹ️ Thinking (loop ") {
            if let Some(ref title) = current_loop_title {
                render_loop_block(ui, title, &current_loop_thinking);
            }
            let raw_title = log.trim_start_matches("ℹ️ ").trim_end_matches("...");
            current_loop_title = Some(raw_title.to_string());
            current_loop_thinking.clear();
        } else if log.starts_with("TOOL_RESULT|") {
            if let Some(ref title) = current_loop_title {
                render_loop_block(ui, title, &current_loop_thinking);
                current_loop_title = None;
                current_loop_thinking.clear();
            }

            if let Some(payload) = log.strip_prefix("TOOL_RESULT|") {
                let parts: Vec<&str> = payload.split('|').collect();
                if parts.len() >= 3 {
                    let icon = parts[0];
                    let name = parts[1];
                    let success = parts[2].contains("Success") || parts[2].contains('✅');
                    let summary = parts.get(3).cloned().unwrap_or("");

                    let text = format_tool_text(name, success, summary);
                    ui.horizontal(|ui| {
                        ui.add_space(4.0);
                        ui.label(format!("{} {}", icon, text));
                    });
                    ui.add_space(2.0);
                }
            }
        } else if log.starts_with("FILE_CHANGE|") {
            if let Some(ref title) = current_loop_title {
                render_loop_block(ui, title, &current_loop_thinking);
                current_loop_title = None;
                current_loop_thinking.clear();
            }

            if let Some(payload) = log.strip_prefix("FILE_CHANGE|") {
                // Payload format: <icon>|<path>|+<added>|-<deleted>
                let parts: Vec<&str> = payload.split('|').collect();
                if parts.len() >= 4 {
                    let icon = parts[0];
                    let path = parts[1];
                    // Strip the leading + / - signs so usize::parse succeeds.
                    let added: usize = parts[2].trim_start_matches('+').parse().unwrap_or(0);
                    let deleted: usize = parts[3].trim_start_matches('-').parse().unwrap_or(0);

                    let filename = std::path::Path::new(path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.to_string());

                    let action = if deleted == 0 && added > 0 {
                        "Created".to_string()
                    } else if added == 0 && deleted > 0 {
                        "Deleted".to_string()
                    } else {
                        "Edited".to_string()
                    };

                    // Render every written file as a polished pill so the
                    // whole timeline of file changes is visible — not just
                    // the most recent one.
                    let (card_fill, card_stroke, label_color) = if dark_mode {
                        (
                            egui::Color32::from_rgb(22, 28, 38),
                            egui::Color32::from_rgb(48, 60, 80),
                            egui::Color32::from_rgb(220, 230, 245),
                        )
                    } else {
                        (
                            egui::Color32::from_rgb(248, 250, 252),
                            egui::Color32::from_rgb(203, 213, 225),
                            egui::Color32::from_rgb(30, 41, 59),
                        )
                    };

                    egui::Frame::default()
                        .fill(card_fill)
                        .stroke(egui::Stroke::new(1.0, card_stroke))
                        .inner_margin(egui::Margin::symmetric(10.0, 6.0))
                        .rounding(8.0)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(format!("{} {}", icon, action))
                                        .size(12.0)
                                        .color(label_color)
                                        .strong(),
                                );
                                ui.label(
                                    egui::RichText::new(filename)
                                        .size(12.0)
                                        .color(label_color)
                                        .monospace(),
                                );
                                ui.add_space(6.0);
                                if added > 0 {
                                    ui.label(
                                        egui::RichText::new(format!("+{}", added))
                                            .size(11.0)
                                            .strong()
                                            .color(egui::Color32::from_rgb(34, 197, 94)),
                                    );
                                }
                                if deleted > 0 {
                                    ui.label(
                                        egui::RichText::new(format!("-{}", deleted))
                                            .size(11.0)
                                            .strong()
                                            .color(egui::Color32::from_rgb(239, 68, 68)),
                                    );
                                }
                            });
                        });
                    ui.add_space(3.0);
                }
            }
        }
    }

    if let Some(ref title) = current_loop_title {
        render_loop_block(ui, title, &current_loop_thinking);
    }

    if is_processing {
        if let Some(name) = current_tool_name {
            let icon = get_tool_icon(name);
            let args_summary = format_tool_args_summary(name, current_tool_args);
            let text = format_tool_text(name, true, &args_summary);

            let is_write = name == "write_file"
                || name == "write_to_file"
                || name == "edit_file"
                || name == "replace_file_content"
                || name == "multi_replace_file_content";

            // Live "writing…" card — same visual language as the historical
            // FILE_CHANGE cards above, but with an amber accent + subtle
            // pulse so the user can clearly see the active write with its
            // running +x / -y counter.
            let (live_fill, live_stroke, live_label) = if dark_mode {
                (
                    egui::Color32::from_rgb(40, 30, 18),
                    egui::Color32::from_rgb(120, 80, 30),
                    egui::Color32::from_rgb(253, 224, 168),
                )
            } else {
                (
                    egui::Color32::from_rgb(255, 247, 237),
                    egui::Color32::from_rgb(214, 158, 46),
                    egui::Color32::from_rgb(120, 53, 15),
                )
            };

            let time = ui.ctx().input(|input| input.time) as f32;
            let pulse = 0.5 + 0.5 * (time * std::f32::consts::TAU / 1.6).sin();
            let pulse_alpha = (120.0 + pulse * 100.0) as u8;
            // Tint with pulse — the stroke slowly breathes in/out so the
            // user can tell the write is still in progress at a glance.
            let stroke_color = egui::Color32::from_rgba_unmultiplied(
                live_stroke.r(),
                live_stroke.g(),
                live_stroke.b(),
                pulse_alpha,
            );

            egui::Frame::default()
                .fill(live_fill)
                .stroke(egui::Stroke::new(1.4, stroke_color))
                .inner_margin(egui::Margin::symmetric(10.0, 6.0))
                .rounding(8.0)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if is_write {
                            let action_label = if name == "write_file" || name == "write_to_file" {
                                "Writing"
                            } else {
                                "Editing"
                            };
                            ui.label(
                                egui::RichText::new(format!("{} {}", icon, action_label))
                                    .size(12.0)
                                    .color(live_label)
                                    .strong(),
                            );
                            ui.label(
                                egui::RichText::new(args_summary)
                                    .size(12.0)
                                    .color(live_label)
                                    .monospace(),
                            );
                            ui.add_space(6.0);

                            let (added, deleted) =
                                estimate_live_diff_stats(name, current_tool_args, project_root);
                            if added > 0 {
                                ui.label(
                                    egui::RichText::new(format!("+{}", added))
                                        .size(11.0)
                                        .strong()
                                        .color(egui::Color32::from_rgb(34, 197, 94)),
                                );
                            }
                            if deleted > 0 {
                                ui.label(
                                    egui::RichText::new(format!("-{}", deleted))
                                        .size(11.0)
                                        .strong()
                                        .color(egui::Color32::from_rgb(239, 68, 68)),
                                );
                            }
                            ui.add_space(4.0);
                            // Animated dotted progress hint
                            let dots = match (time as usize) % 3 {
                                0 => ".",
                                1 => "..",
                                _ => "...",
                            };
                            ui.label(egui::RichText::new(dots).size(11.0).color(live_label));
                        } else {
                            ui.label(
                                egui::RichText::new(format!("{} {}…", icon, text))
                                    .size(12.0)
                                    .color(live_label)
                                    .strong(),
                            );
                        }
                    });
                });
            ui.ctx().request_repaint();
            ui.add_space(3.0);
        }
    }
}
