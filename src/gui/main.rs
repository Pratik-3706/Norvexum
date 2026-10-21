#![windows_subsystem = "windows"]

// ===========================================================================
// norvexum-gui — desktop entry point
//
// The GUI runs on the main thread (required by eframe/winit); the agent runs
// on a Tokio runtime in a background thread and talks to the GUI through the
// channels in `runtime::AgentHandle`.
// ===========================================================================

mod app;
mod style;

// Re-export the shared library modules so `crate::agent`, `crate::config`, …
// keep resolving inside `app.rs`.
pub(crate) use norvexum::{agent, ai, commands, config, skills, tools, ui, util};

use clap::Parser;
use color_eyre::eyre::Result;
use norvexum::cli::{Cli, Commands};
use norvexum::commands::chat::ChatOptions;

fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    norvexum::cli::init_logging(cli.is_headless());

    let wants_gui = !cli.is_headless() && matches!(cli.command, None | Some(Commands::Chat { .. }));
    if wants_gui {
        return run_gui_mode(cli);
    }

    // Subcommands and headless mode behave exactly like the `norvexum` binary.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let mut settings = config::Settings::load_remembered()?;
        cli.apply_overrides(&mut settings);
        let initial_message = cli.initial_message();
        match cli.command {
            Some(Commands::Init) => commands::init::run(&settings).await,
            Some(Commands::Config { action }) => commands::config::run(&settings, action).await,
            _ => {
                if !commands::chat::is_initialized(&settings) {
                    commands::init::run(&settings).await?;
                }
                let opts = ChatOptions {
                    initial_message,
                    resume: cli.resume,
                    auto_approve: cli.yes,
                    verbose: cli.verbose,
                };
                commands::chat::run(settings, opts).await
            }
        }
    })?;
    std::process::exit(0);
}

fn run_gui_mode(cli: Cli) -> Result<()> {
    let (config_tx, config_rx) = std::sync::mpsc::channel::<app::GuiConfig>();

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!("Failed to start async runtime: {e}");
                return;
            }
        };
        rt.block_on(async move {
            let mut settings = config::Settings::load_remembered().unwrap_or_default();
            cli.apply_overrides(&mut settings);
            if !commands::chat::is_initialized(&settings) {
                let _ = commands::init::run(&settings).await;
            }
            config::model_fetch::spawn_background_fetch(&settings);

            let initial_message = cli.initial_message();
            let initial: Vec<String> = initial_message.iter().cloned().collect();
            let handle = norvexum::runtime::spawn_agent(settings.clone(), initial);
            let gui_config = app::GuiConfig {
                settings,
                initial_message,
                agent_rx: handle.events,
                user_tx: handle.user_tx,
                inst_tx: handle.inst_tx,
                cancel_tx: handle.cancel_tx,
                approval_tx: handle.approval_tx,
            };
            if config_tx.send(gui_config).is_err() {
                return;
            }
            // Keep the runtime (and the agent task on it) alive for the GUI's lifetime;
            // the GUI thread ends the process with `std::process::exit`.
            std::future::pending::<()>().await;
        });
    });

    match config_rx.recv() {
        Ok(config) => {
            if let Err(e) = app::run_gui(config) {
                eprintln!("GUI launch error: {e}");
                std::process::exit(1);
            }
            Ok(())
        }
        Err(_) => {
            eprintln!("Norvexum failed to start its agent runtime. See ~/.norvexum/norvexum.log.");
            std::process::exit(1);
        }
    }
}
