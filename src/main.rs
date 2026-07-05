#![allow(unused)]
#![allow(clippy::all)]

mod agent;
mod ai;
mod commands;
mod config;
mod skills;
mod tools;
mod ui;

use clap::{Parser, Subcommand};
use color_eyre::eyre::Result;

/// Norvexum — Advanced multi-threaded agentic CLI
#[derive(Parser)]
#[command(
    name = "norvexum",
    version,
    about = "An advanced agentic CLI with multi-provider AI, rich TUI, and parallel tool calling",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Override the AI model to use (e.g. "gemini-3-flash", "claude-haiku-4.5")
    #[arg(long, global = true)]
    model: Option<String>,

    /// Override the AI provider (e.g. "google_direct", "aicredits", "openai")
    #[arg(long, global = true)]
    provider: Option<String>,

    /// Run in headless mode (no TUI, plain text output)
    #[arg(long, global = true)]
    headless: bool,
}

#[derive(Subcommand)]
enum Commands {
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

#[derive(Subcommand)]
enum ConfigAction {
    /// List all configuration values
    List,
    /// Set a configuration value
    Set { key: String, value: String },
    /// Get a configuration value
    Get { key: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    // Check if running in headless mode
    let is_headless = std::env::args().any(|arg| arg == "--headless");

    if is_headless {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "norvexum=info,warn".into()),
            )
            .with_writer(std::io::stderr)
            .init();
    } else {
        // TUI mode: write logs to file to prevent console pollution/corruption
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("norvexum.log")
        {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "norvexum=info,warn".into()),
                )
                .with_writer(file)
                .init();
        } else {
            // Discard logs if we cannot open the log file
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "norvexum=info,warn".into()),
                )
                .with_writer(std::io::sink)
                .init();
        }
    }

    // Load local .env file (silently ignore if missing)
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

    // Build settings from env + config file + CLI overrides
    let mut settings = config::Settings::load()?;
    if let Some(model) = &cli.model {
        settings.active_model = model.clone();
    }
    if let Some(provider) = &cli.provider {
        settings.active_provider = provider.clone();
    }
    settings.headless = cli.headless;

    let result = match cli.command {
        Some(Commands::Init) => commands::init::run(&settings).await,
        Some(Commands::Config { action }) => commands::config::run(&settings, action).await,
        Some(Commands::Chat { message }) => {
            if !commands::chat::is_initialized() {
                commands::init::run(&settings).await?;
                // Force reload env variables from the newly created .env
                let _ = dotenvy::dotenv();
            }
            let initial_msg = if message.is_empty() {
                None
            } else {
                Some(message.join(" "))
            };
            commands::chat::run(settings, initial_msg).await
        }
        None => {
            if !commands::chat::is_initialized() {
                commands::init::run(&settings).await?;
                // Force reload env variables from the newly created .env
                let _ = dotenvy::dotenv();
            }
            // Reload settings so newly created config/env is loaded
            let mut settings = config::Settings::load()?;
            if let Some(model) = &cli.model {
                settings.active_model = model.clone();
            }
            if let Some(provider) = &cli.provider {
                settings.active_provider = provider.clone();
            }
            settings.headless = cli.headless;

            commands::chat::run(settings, None).await
        }
    };

    if let Err(e) = result {
        let err_str = e.to_string();
        if err_str.contains("No API key found") {
            eprintln!(
                "\n⚠️  Norvexum Setup Required:\n{}\n\n\
                 Please open the '.env' file generated in this folder and add your keys.\n",
                err_str
            );
            std::process::exit(1);
        } else {
            return Err(e);
        }
    }

    Ok(())
}
