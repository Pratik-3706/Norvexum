// ═══════════════════════════════════════════════════════════════════════════
// Config — Settings, .env loading, project-level config
// ═══════════════════════════════════════════════════════════════════════════

pub mod providers;
pub mod model_fetch;

use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Name of the project-local config directory
pub const NORVEXUM_DIR: &str = ".norvexum";
/// Name of the config file inside .norvexum/
pub const CONFIG_FILE: &str = "config.toml";
/// Name of the project context file
pub const CONTEXT_FILE: &str = "project_context.json";
/// History subdirectory
pub const HISTORY_DIR: &str = "history";
/// Python venvs subdirectory
pub const VENVS_DIR: &str = "venvs";

/// Global application settings, assembled from .env + config.toml + CLI args.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    // ── Active model/provider ────────────────────────────────────────────
    pub active_provider: String,
    pub active_model: String,

    // ── API Keys (loaded from env) ───────────────────────────────────────
    pub aicredits_api_key: Option<String>,
    pub google_ai_api_key: Option<String>,
    pub openai_api_key: Option<String>,
    pub anthropic_api_key: Option<String>,
    pub tavily_api_key: Option<String>,
    pub ocr_space_api_key: Option<String>,
    pub pollinations_api_key: Option<String>,

    // ── Ollama (local models) ────────────────────────────────────────────
    pub ollama_base_url: Option<String>,

    // ── Runtime behaviour ────────────────────────────────────────────────
    pub headless: bool,
    pub browser_timeout_secs: u64,
    pub max_thinking_loops: usize,
    pub max_content_chars: usize,

    // ── Safety / approval ────────────────────────────────────────────────
    /// Tools that require user approval before execution
    pub require_approval: Vec<String>,

    // ── Paths ────────────────────────────────────────────────────────────
    /// Project root (where .norvexum/ lives)
    pub project_root: PathBuf,

    // ── GUI Settings ──────────────────────────────────────────────────────
    #[serde(default)]
    pub fullscreen: bool,
    #[serde(default)]
    pub maximized: bool,
    #[serde(default)]
    pub ask_perms: bool,
    #[serde(default)]
    pub custom_system_prompt: Option<String>,
    #[serde(default)]
    pub custom_models: Vec<String>,
    #[serde(default)]
    pub starred_models: Vec<String>,
    // ── Reasoning / Thinking settings ────────────────────────────────────
    pub thinking_effort: Option<String>,
    #[serde(skip)]
    pub model_reasoning_type: Option<crate::ai::types::ReasoningType>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            active_provider: "google_direct".into(),
            active_model: "gemini-2.5-flash".into(),
            aicredits_api_key: None,
            google_ai_api_key: None,
            openai_api_key: None,
            anthropic_api_key: None,
            tavily_api_key: None,
            ocr_space_api_key: None,
            pollinations_api_key: None,
            ollama_base_url: None,
            headless: false,
            browser_timeout_secs: 30,
            max_thinking_loops: 25,
            max_content_chars: 12000,
            require_approval: vec!["run_command".to_string()],
            project_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            fullscreen: false,
            maximized: false,
            ask_perms: true,
            custom_system_prompt: None,
            custom_models: Vec::new(),
            starred_models: vec![],
            thinking_effort: None,
            model_reasoning_type: None,
        }
    }
}

impl Settings {
    /// Load settings: env vars → config.toml → defaults
    pub fn load() -> Result<Self> {
        let mut settings = Self::default();

        // ── 1. Environment variables ─────────────────────────────────────
        settings.aicredits_api_key = std::env::var("AICREDITS_API_KEY")
            .ok()
            .filter(|s| !s.is_empty());
        settings.google_ai_api_key = std::env::var("GOOGLE_AI_API_KEY")
            .ok()
            .filter(|s| !s.is_empty());
        settings.openai_api_key = std::env::var("OPENAI_API_KEY")
            .ok()
            .filter(|s| !s.is_empty());
        settings.anthropic_api_key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|s| !s.is_empty());
        settings.tavily_api_key = std::env::var("TAVILY_API_KEY")
            .ok()
            .filter(|s| !s.is_empty());
        settings.ocr_space_api_key = std::env::var("OCR_SPACE_API_KEY")
            .ok()
            .filter(|s| !s.is_empty());
        settings.pollinations_api_key = std::env::var("POLLINATIONS_API_KEY")
            .ok()
            .filter(|s| !s.is_empty());
        settings.ollama_base_url = std::env::var("OLLAMA_BASE_URL")
            .ok()
            .filter(|s| !s.is_empty());

        // ── 2. Project-local config ──────────────────────────────────────
        let mut target_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        // If the current directory ends with target\release or target\debug, default to two levels up (project root)
        if target_dir.ends_with("target/release") || target_dir.ends_with("target/debug") || target_dir.ends_with("target\\release") || target_dir.ends_with("target\\debug") {
            if let Some(parent) = target_dir.parent().and_then(|p| p.parent()) {
                target_dir = parent.to_path_buf();
            }
        }

        settings.project_root = target_dir.clone();

        let config_path = target_dir.join(NORVEXUM_DIR).join(CONFIG_FILE);
        if config_path.exists() {
            let content = std::fs::read_to_string(&config_path)
                .wrap_err_with(|| format!("Failed to read config: {}", config_path.display()))?;
            let file_settings: FileSettings =
                toml::from_str(&content).wrap_err("Failed to parse config.toml")?;

            if let Some(provider) = file_settings.active_provider {
                settings.active_provider = provider;
            }
            if let Some(model) = file_settings.active_model {
                settings.active_model = model;
            }
            if let Some(timeout) = file_settings.browser_timeout_secs {
                settings.browser_timeout_secs = timeout;
            }
            if let Some(loops) = file_settings.max_thinking_loops {
                settings.max_thinking_loops = loops;
            }
            if let Some(chars) = file_settings.max_content_chars {
                settings.max_content_chars = chars;
            }
            if let Some(approval) = file_settings.require_approval {
                settings.require_approval = approval;
            }
            if let Some(url) = file_settings.ollama_base_url {
                settings.ollama_base_url = Some(url);
            }
            if let Some(custom) = file_settings.custom_models {
                settings.custom_models = custom;
            }
            if let Some(starred) = file_settings.starred_models {
                settings.starred_models = starred;
            }
            if let Some(effort) = file_settings.thinking_effort {
                settings.thinking_effort = Some(effort);
            }
        }

        // Auto-select provider based on available keys if using defaults
        if settings.active_provider == "google_direct" && settings.google_ai_api_key.is_none() {
            if settings.aicredits_api_key.is_some() {
                settings.active_provider = "aicredits".into();
                settings.active_model = "google/gemini-2.5-flash".into();
            } else if settings.openai_api_key.is_some() {
                settings.active_provider = "openai".into();
                settings.active_model = "gpt-4o-mini".into();
            } else if settings.anthropic_api_key.is_some() {
                settings.active_provider = "anthropic".into();
                settings.active_model = "claude-3-5-haiku-latest".into();
            }
        }

        Ok(settings)
    }

    /// Load settings, restoring the last-used project root from global config.
    /// Use this ONLY at GUI startup to remember the last workspace.
    /// CLI and explicit folder changes should use `load()` which is CWD-based.
    pub fn load_remembered() -> Result<Self> {
        let mut settings = Self::load()?;

        // Check global config for last-used project root
        if let Some(home) = dirs::home_dir() {
            let global_path = home.join(".norvexum_global.json");
            if global_path.exists() {
                if let Ok(content) = std::fs::read_to_string(&global_path) {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                        if let Some(saved_root) = val["last_project_root"].as_str() {
                            let saved_path = PathBuf::from(saved_root);
                            if saved_path.exists() && saved_path.is_dir() {
                                // Override project root and set CWD
                                let _ = std::env::set_current_dir(&saved_path);
                                settings.project_root = saved_path.clone();

                                // Also load directory-specific config.toml from the remembered dir
                                let config_path = saved_path.join(NORVEXUM_DIR).join(CONFIG_FILE);
                                if config_path.exists() {
                                    if let Ok(cfg_content) = std::fs::read_to_string(&config_path) {
                                        if let Ok(file_settings) = toml::from_str::<FileSettings>(&cfg_content) {
                                            if let Some(provider) = file_settings.active_provider {
                                                settings.active_provider = provider;
                                            }
                                            if let Some(model) = file_settings.active_model {
                                                settings.active_model = model;
                                            }
                                            if let Some(url) = file_settings.ollama_base_url {
                                                settings.ollama_base_url = Some(url);
                                            }
                                            if let Some(starred) = file_settings.starred_models {
                                                settings.starred_models = starred;
                                            }
                                            if let Some(effort) = file_settings.thinking_effort {
                                                settings.thinking_effort = Some(effort);
                                            }
                                        }
                                    }
                                }

                                // Load .env from remembered dir
                                let env_path = saved_path.join(".env");
                                if env_path.exists() {
                                    let _ = dotenvy::from_path(&env_path);
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(settings)
    }

    /// Save current config to .norvexum/config.toml
    pub fn save(&self) -> Result<()> {
        let config_dir = self.project_root.join(NORVEXUM_DIR);
        std::fs::create_dir_all(&config_dir)?;

        let file_settings = FileSettings {
            active_provider: Some(self.active_provider.clone()),
            active_model: Some(self.active_model.clone()),
            browser_timeout_secs: Some(self.browser_timeout_secs),
            max_thinking_loops: Some(self.max_thinking_loops),
            max_content_chars: Some(self.max_content_chars),
            require_approval: if self.require_approval.is_empty() {
                None
            } else {
                Some(self.require_approval.clone())
            },
            ollama_base_url: self.ollama_base_url.clone(),
            custom_models: if self.custom_models.is_empty() {
                None
            } else {
                Some(self.custom_models.clone())
            },
            starred_models: if self.starred_models.is_empty() {
                None
            } else {
                Some(self.starred_models.clone())
            },
            thinking_effort: self.thinking_effort.clone(),
        };

        let content = toml::to_string_pretty(&file_settings)?;
        let config_path = config_dir.join(CONFIG_FILE);
        std::fs::write(&config_path, content)?;

        // Save last_project_root to global config file
        let global_config_path = dirs::home_dir().map(|h| h.join(".norvexum_global.json"));
        if let Some(ref path) = global_config_path {
            let global_val = serde_json::json!({
                "last_project_root": self.project_root.to_string_lossy().to_string()
            });
            if let Ok(content) = serde_json::to_string_pretty(&global_val) {
                let _ = std::fs::write(path, content);
            }
        }

        Ok(())
    }

    /// Get the .norvexum/ directory path
    pub fn norvexum_dir(&self) -> PathBuf {
        self.project_root.join(NORVEXUM_DIR)
    }

    /// Get the history directory path
    pub fn history_dir(&self) -> PathBuf {
        self.norvexum_dir().join(HISTORY_DIR)
    }

    /// Check if a path is within the project sandbox
    pub fn is_in_sandbox(&self, path: &Path) -> bool {
        match (path.canonicalize(), self.project_root.canonicalize()) {
            (Ok(abs_path), Ok(abs_root)) => abs_path.starts_with(&abs_root),
            _ => {
                // Fallback: string prefix check
                let path_str = path.to_string_lossy();
                let root_str = self.project_root.to_string_lossy();
                path_str.starts_with(root_str.as_ref())
            }
        }
    }

    /// Get the API key for the active provider
    pub fn active_api_key(&self) -> Option<&str> {
        match self.active_provider.as_str() {
            "aicredits" => self.aicredits_api_key.as_deref(),
            "google_direct" => self.google_ai_api_key.as_deref(),
            "openai" => self.openai_api_key.as_deref(),
            "anthropic" => self.anthropic_api_key.as_deref(),
            "ollama" => Some("ollama"), // Ollama doesn't need a real key
            _ => None,
        }
    }

    /// Check if a tool requires user approval.
    pub fn needs_approval(&self, tool_name: &str) -> bool {
        self.require_approval.iter().any(|t| t == tool_name)
    }
}

/// Subset of settings that gets serialized to config.toml
#[derive(Debug, Serialize, Deserialize)]
struct FileSettings {
    active_provider: Option<String>,
    active_model: Option<String>,
    browser_timeout_secs: Option<u64>,
    max_thinking_loops: Option<usize>,
    max_content_chars: Option<usize>,
    require_approval: Option<Vec<String>>,
    ollama_base_url: Option<String>,
    custom_models: Option<Vec<String>>,
    starred_models: Option<Vec<String>>,
    thinking_effort: Option<String>,
}
