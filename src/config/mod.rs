// ═══════════════════════════════════════════════════════════════════════════
// Config — Settings, .env loading, project-level config
// ═══════════════════════════════════════════════════════════════════════════

pub mod providers;

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
    #[serde(skip)]
    pub aicredits_api_key: Option<String>,
    #[serde(skip)]
    pub google_ai_api_key: Option<String>,
    #[serde(skip)]
    pub openai_api_key: Option<String>,
    #[serde(skip)]
    pub anthropic_api_key: Option<String>,
    #[serde(skip)]
    pub tavily_api_key: Option<String>,
    #[serde(skip)]
    pub ocr_space_api_key: Option<String>,
    #[serde(skip)]
    pub pollinations_api_key: Option<String>,

    // ── Runtime behaviour ────────────────────────────────────────────────
    pub headless: bool,
    pub browser_timeout_secs: u64,
    pub max_thinking_loops: usize,
    pub max_content_chars: usize,

    // ── Paths ────────────────────────────────────────────────────────────
    /// Project root (where .norvexum/ lives)
    pub project_root: PathBuf,
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
            headless: false,
            browser_timeout_secs: 30,
            max_thinking_loops: 25,
            max_content_chars: 12000,
            project_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
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

        // ── 2. Project-local config ──────────────────────────────────────
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        settings.project_root = cwd.clone();

        let config_path = cwd.join(NORVEXUM_DIR).join(CONFIG_FILE);
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
        };

        let content = toml::to_string_pretty(&file_settings)?;
        let config_path = config_dir.join(CONFIG_FILE);
        std::fs::write(&config_path, content)?;
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
            _ => None,
        }
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
}
