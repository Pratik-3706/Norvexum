// ===========================================================================
// norvexum — terminal entry point (TUI, headless, and one-shot `-p` mode)
// ===========================================================================

use clap::Parser;
use color_eyre::eyre::Result;
use norvexum::cli::{Cli, Commands};
use norvexum::commands::{self, chat::ChatOptions};
use norvexum::config::Settings;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    norvexum::cli::init_logging(cli.is_headless());

    let mut settings = Settings::load()?;
    cli.apply_overrides(&mut settings);

    let result = match cli.command {
        Some(Commands::Init) => commands::init::run(&settings).await,
        Some(Commands::Config { action }) => commands::config::run(&settings, action).await,
        _ => {
            if !commands::chat::is_initialized(&settings) {
                commands::init::run(&settings).await?;
            }
            norvexum::config::model_fetch::spawn_background_fetch(&settings);
            let opts = ChatOptions {
                initial_message: cli.initial_message(),
                resume: cli.resume,
                auto_approve: cli.yes,
                verbose: cli.verbose,
            };
            commands::chat::run(settings, opts).await
        }
    };

    if let Err(e) = result {
        eprintln!("\n\x1b[31merror:\x1b[0m {e}");
        std::process::exit(1);
    }
    // Background tasks (model fetch, child processes) must not keep the process alive.
    std::process::exit(0);
}
