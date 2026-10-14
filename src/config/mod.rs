// ===========================================================================
// Config — Settings, .env loading, project-level config
// ===========================================================================

pub mod model_fetch;
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
    /// Detected reasoning API style for the active model. Not stored in
    /// config.toml, but it must survive the JSON hand-off from the GUI to the
    /// agent (it used to be `serde(skip)`, so the agent never saw it).
    #[serde(default)]
    pub model_reasoning_type: Option<crate::ai::types::ReasoningType>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            active_provider: "openai".into(),
            active_model: "gpt-4o".into(),
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
    /// Load settings: env vars (+ .env files, OS keyring) → config.toml → defaults
    pub fn load() -> Result<Self> {
        load_env_files();
        // ── 1. Environment variables, then the OS keyring ────────────────
        let mut settings = Self {
            aicredits_api_key: secret("AICREDITS_API_KEY"),
            google_ai_api_key: secret("GOOGLE_AI_API_KEY"),
            openai_api_key: secret("OPENAI_API_KEY"),
            anthropic_api_key: secret("ANTHROPIC_API_KEY"),
            tavily_api_key: secret("TAVILY_API_KEY"),
            ocr_space_api_key: secret("OCR_SPACE_API_KEY"),
            pollinations_api_key: secret("POLLINATIONS_API_KEY"),
            ollama_base_url: std::env::var("OLLAMA_BASE_URL")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            ..Self::default()
        };

        // ── 2. Project-local config ──────────────────────────────────────
        let mut target_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        // If the current directory ends with target\release or target\debug, default to two levels up (project root)
        if target_dir.ends_with("target/release")
            || target_dir.ends_with("target/debug")
            || target_dir.ends_with("target\\release")
            || target_dir.ends_with("target\\debug")
        {
            if let Some(parent) = target_dir.parent().and_then(|p| p.parent()) {
                target_dir = parent.to_path_buf();
            }
        }

        settings.project_root = target_dir.clone();

        let config_dir = dirs::home_dir()
            .map(|h| h.join(".norvexum"))
            .unwrap_or_else(|| target_dir.join(NORVEXUM_DIR));
        let config_path = config_dir.join(CONFIG_FILE);
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
            if let Some(ask) = file_settings.ask_perms {
                settings.ask_perms = ask;
            }
            if let Some(fs) = file_settings.fullscreen {
                settings.fullscreen = fs;
            }
            if let Some(max) = file_settings.maximized {
                settings.maximized = max;
            }
            if let Some(csp) = file_settings.custom_system_prompt {
                settings.custom_system_prompt = Some(csp);
            }
        }

        // Auto-select provider based on available keys if using defaults
        if settings.active_provider == "openai" && settings.openai_api_key.is_none() {
            if settings.anthropic_api_key.is_some() {
                settings.active_provider = "anthropic".into();
                settings.active_model = "claude-sonnet-5-5".into();
            } else if settings.google_ai_api_key.is_some() {
                settings.active_provider = "google_direct".into();
                settings.active_model = "gemini-2.5-flash".into();
            } else if settings.aicredits_api_key.is_some() {
                settings.active_provider = "aicredits".into();
                settings.active_model = "google/gemini-2.5-flash".into();
            }
        }

        Ok(settings)
    }

    /// Load settings, restoring the last-used project root from global config.
    /// Use this ONLY at GUI startup to remember the last workspace.
    /// CLI and explicit folder changes should use `load()` which is CWD-based.
    pub fn load_remembered() -> Result<Self> {
        let mut settings = Self::load()?;

        let saved_root = dirs::home_dir()
            .map(|home| home.join(".norvexum_global.json"))
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
            .and_then(|val| val["last_project_root"].as_str().map(PathBuf::from))
            .filter(|path| path.is_dir());

        if let Some(saved_path) = saved_root {
            let _ = std::env::set_current_dir(&saved_path);
            // A project-local .env in the remembered workspace still applies.
            let _ = dotenvy::from_path(saved_path.join(".env"));
            settings.project_root = saved_path;
        }

        Ok(settings)
    }

    /// Save current config to global ~/.norvexum/config.toml
    pub fn save(&self) -> Result<()> {
        let config_dir = dirs::home_dir()
            .map(|h| h.join(".norvexum"))
            .unwrap_or_else(|| self.project_root.join(NORVEXUM_DIR));
        std::fs::create_dir_all(&config_dir)?;

        let file_settings = FileSettings {
            active_provider: Some(self.active_provider.clone()),
            active_model: Some(self.active_model.clone()),
            browser_timeout_secs: Some(self.browser_timeout_secs),
            max_thinking_loops: Some(self.max_thinking_loops),
            max_content_chars: Some(self.max_content_chars),
            // Always written: an empty list means "approve nothing" and must not
            // fall back to the default on the next start.
            require_approval: Some(self.require_approval.clone()),
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
            ask_perms: Some(self.ask_perms),
            fullscreen: Some(self.fullscreen),
            maximized: Some(self.maximized),
            custom_system_prompt: self.custom_system_prompt.clone(),
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

    /// Check if a path is within the project sandbox.
    ///
    /// Paths that don't exist yet are resolved through their nearest existing
    /// ancestor, and `..` components are applied before comparing, so
    /// `root/../../Windows/x` is rejected even though it starts with `root`.
    pub fn is_in_sandbox(&self, path: &Path) -> bool {
        let root = self
            .project_root
            .canonicalize()
            .unwrap_or_else(|_| normalize_lexically(&self.project_root));
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.project_root.join(path)
        };
        resolve_existing_prefix(&absolute).is_some_and(|resolved| resolved.starts_with(&root))
    }

    /// A user-facing explanation when the active provider has no API key.
    pub fn missing_api_key_message(&self) -> Option<String> {
        if self.active_provider == "ollama" {
            return None;
        }
        if self.active_api_key().is_some_and(|k| !k.trim().is_empty()) {
            return None;
        }
        let env_name = providers::find_provider(&self.active_provider)
            .map(|p| p.env_key)
            .unwrap_or_else(|| "the provider's API key".into());
        Some(format!(
            "No API key found for provider '{provider}'.\n\n\
             Set {env_name} in your environment or in ~/.norvexum/.env, e.g.\n  \
             {env_name}=sk-...\n\n\
             Or switch provider: norvexum config set active_provider <openai|anthropic|google_direct|aicredits|ollama>",
            provider = self.active_provider
        ))
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
    ask_perms: Option<bool>,
    fullscreen: Option<bool>,
    maximized: Option<bool>,
    custom_system_prompt: Option<String>,
}

/// Load `~/.norvexum/.env` (where the GUI stores keys) and the project `.env`.
/// Variables already present in the environment always win.
fn load_env_files() {
    if let Some(home) = dirs::home_dir() {
        let _ = dotenvy::from_path(home.join(NORVEXUM_DIR).join(".env"));
    }
    let _ = dotenvy::dotenv();
}

fn secret(env_name: &str) -> Option<String> {
    std::env::var(env_name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .or_else(|| get_keyring_secret(&env_name.to_lowercase()))
}

/// Read a key saved by the GUI in the OS keyring (service "norvexum").
pub fn get_keyring_secret(name: &str) -> Option<String> {
    keyring::Entry::new("norvexum", name)
        .ok()?
        .get_password()
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Apply `.` and `..` components without touching the filesystem.
fn normalize_lexically(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Canonicalize the longest existing prefix of `path` (resolving symlinks),
/// then re-append the missing tail.
fn resolve_existing_prefix(path: &Path) -> Option<PathBuf> {
    let normalized = normalize_lexically(path);
    let mut existing = normalized.as_path();
    let mut tail = Vec::new();
    while !existing.exists() {
        tail.push(existing.file_name()?.to_os_string());
        existing = existing.parent()?;
    }
    let mut resolved = existing.canonicalize().ok()?;
    for part in tail.into_iter().rev() {
        resolved.push(part);
    }
    Some(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_rejects_traversal_through_missing_paths() {
        let root = std::env::temp_dir().join(format!("nvx-sandbox-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let settings = Settings {
            project_root: root.clone(),
            ..Settings::default()
        };
        assert!(settings.is_in_sandbox(&root.join("new/dir/file.txt")));
        assert!(settings.is_in_sandbox(Path::new("relative.txt")));
        assert!(
            !settings.is_in_sandbox(&root.join("missing").join("..").join("..").join("evil.txt"))
        );
        let sibling = PathBuf::from(format!("{}-evil", root.display())).join("x");
        assert!(!settings.is_in_sandbox(&sibling));
        std::fs::remove_dir_all(root).unwrap();
    }
}
