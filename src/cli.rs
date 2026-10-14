// ===========================================================================
// CLI — Argument definitions and startup helpers shared by both binaries
// ===========================================================================

use clap::{Parser, Subcommand};

use crate::config::Settings;

/// Norvexum — Advanced multi-threaded agentic CLI
#[derive(Parser, Debug)]
#[command(
    name = "norvexum",
    version,
    about = "An agentic coding assistant with multi-provider AI, a rich TUI, and parallel tool calling",
    long_about = None,
    after_help = "Examples:\n  \
        norvexum                          Start the interactive TUI\n  \
        norvexum -p \"explain src/main.rs\"  Ask once, print the answer, exit\n  \
        git diff | norvexum -p \"review\"    Pipe input into a one-shot prompt\n  \
        norvexum --resume                 Continue the last saved session\n  \
        norvexum config set active_model gpt-4o"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Run a single prompt non-interactively, print the answer, and exit
    #[arg(short = 'p', long = "prompt", value_name = "TEXT")]
    pub prompt: Option<String>,

    /// Override the AI model to use (e.g. "gpt-4o", "gemini-2.5-flash")
    #[arg(long, global = true)]
    pub model: Option<String>,

    /// Override the AI provider (e.g. "openai", "anthropic", "google_direct", "ollama")
    #[arg(long, global = true)]
    pub provider: Option<String>,

    /// Run in headless mode (no TUI, plain text output)
    #[arg(long, global = true)]
    pub headless: bool,

    /// Resume the most recent saved session for this project
    #[arg(long, global = true)]
    pub resume: bool,

    /// Headless only: approve every tool call without asking
    #[arg(long, short = 'y', global = true)]
    pub yes: bool,

    /// Headless only: also print status messages and reasoning to stderr
    #[arg(long, short = 'v', global = true)]
    pub verbose: bool,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Initialize Norvexum in the current directory
    Init,

    /// Start an interactive chat session (default if no subcommand)
    Chat {
        /// Optional initial message to send
        #[arg(trailing_var_arg = true)]
        message: Vec<String>,
    },

    /// Manage configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// List all configuration values
    List,
    /// Set a configuration value
    Set { key: String, value: String },
    /// Get a configuration value
    Get { key: String },
}

impl Cli {
    /// True when the run should not open a TUI/GUI window.
    pub fn is_headless(&self) -> bool {
        self.headless || self.prompt.is_some()
    }

    /// The first message to send to the agent, if any.
    pub fn initial_message(&self) -> Option<String> {
        if let Some(prompt) = &self.prompt {
            return Some(prompt.clone());
        }
        match &self.command {
            Some(Commands::Chat { message }) if !message.is_empty() => Some(message.join(" ")),
            _ => None,
        }
    }

    /// Apply `--model` / `--provider` / `--headless` overrides on top of loaded settings.
    pub fn apply_overrides(&self, settings: &mut Settings) {
        if let Some(provider) = &self.provider {
            settings.active_provider = provider.clone();
        }
        if let Some(model) = &self.model {
            settings.active_model = model.clone();
        }
        settings.headless = self.is_headless();
    }
}

/// Route tracing output: stderr for headless runs, a log file otherwise
/// (writing to the terminal would corrupt the TUI).
pub fn init_logging(headless: bool) {
    let filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "norvexum=info,warn".into())
    };

    if headless {
        tracing_subscriber::fmt()
            .with_env_filter(filter())
            .with_writer(std::io::stderr)
            .init();
        return;
    }

    let log_dir = dirs::home_dir()
        .map(|h| h.join(crate::config::NORVEXUM_DIR))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let _ = std::fs::create_dir_all(&log_dir);
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("norvexum.log"));

    match log_file {
        Ok(file) => tracing_subscriber::fmt()
            .with_env_filter(filter())
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .init(),
        Err(_) => tracing_subscriber::fmt()
            .with_env_filter(filter())
            .with_writer(std::io::sink)
            .init(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_flag_implies_headless_one_shot() {
        let cli = Cli::parse_from(["norvexum", "-p", "hello there"]);
        assert!(cli.is_headless());
        assert_eq!(cli.initial_message().as_deref(), Some("hello there"));
    }

    #[test]
    fn chat_subcommand_joins_message_words() {
        let cli = Cli::parse_from(["norvexum", "chat", "fix", "the", "bug"]);
        assert!(!cli.is_headless());
        assert_eq!(cli.initial_message().as_deref(), Some("fix the bug"));
    }
}
