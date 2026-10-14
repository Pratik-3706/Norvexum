// ===========================================================================
// Commands: chat — Starts an interactive agent session in TUI or headless mode
// ===========================================================================

use eyre::{Result, WrapErr};

use crate::config::{NORVEXUM_DIR, Settings};
use crate::runtime::{HeadlessOptions, run_headless, spawn_agent};
use crate::ui::{App, run_tui};

pub struct ChatOptions {
    pub initial_message: Option<String>,
    pub resume: bool,
    pub auto_approve: bool,
    pub verbose: bool,
}

/// Whether `.norvexum/` exists in the project root.
pub fn is_initialized(settings: &Settings) -> bool {
    settings.project_root.join(NORVEXUM_DIR).exists()
}

pub async fn run(settings: Settings, opts: ChatOptions) -> Result<()> {
    if let Some(problem) = settings.missing_api_key_message() {
        eyre::bail!("{problem}");
    }

    let model_info = format!("{} / {}", settings.active_model, settings.active_provider);
    let project_root = settings.project_root.clone();
    let headless = settings.headless;

    let mut initial = Vec::new();
    if opts.resume {
        initial.push("/session resume".to_string());
    }
    let one_shot = opts.initial_message.is_some() && headless;
    if let Some(message) = &opts.initial_message {
        initial.push(with_piped_stdin(message, headless));
    }
    let initial_count = initial.len();

    let handle = spawn_agent(settings, initial);

    if headless {
        return run_headless(
            handle,
            HeadlessOptions {
                initial_count,
                one_shot,
                auto_approve: opts.auto_approve,
                verbose: opts.verbose,
            },
        )
        .await;
    }

    let mut app = App::new(&model_info, project_root);
    if let Some(message) = opts.initial_message {
        app.add_user_message(message);
    } else if opts.resume {
        app.is_processing = true;
        app.status = "Resuming session".into();
    }
    run_tui(app, handle)
        .await
        .wrap_err("TUI runtime encountered an error")
}

/// `git diff | norvexum -p "review this"` — append piped stdin to the prompt.
fn with_piped_stdin(message: &str, headless: bool) -> String {
    use std::io::{IsTerminal, Read};
    if !headless || std::io::stdin().is_terminal() {
        return message.to_string();
    }
    let mut piped = String::new();
    if std::io::stdin().read_to_string(&mut piped).is_err() || piped.trim().is_empty() {
        return message.to_string();
    }
    format!("{message}\n\n<stdin>\n{}\n</stdin>", piped.trim_end())
}
