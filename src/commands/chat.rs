// ═══════════════════════════════════════════════════════════════════════════
// Commands: chat — Starts interactive agent session in TUI or Headless mode
//
// Supports:
//   • Full TUI mode with streaming, tool calls, and text selection
//   • Headless mode for CI/scripting with --headless flag
//   • Session persistence with auto-save/resume
//   • Tool approval flow for sandboxed execution
// ═══════════════════════════════════════════════════════════════════════════

use crate::agent::{Agent, AgentEvent};
use crate::config::{NORVEXUM_DIR, Settings};
use crate::ui::{App, run_tui};
use eyre::{Result, WrapErr};
use tokio::sync::mpsc;

/// Verify if the project is initialized
pub fn is_initialized() -> bool {
    let cwd = std::env::current_dir().unwrap_or_default();
    cwd.join(NORVEXUM_DIR).exists()
}

/// Ensure the project is initialized, otherwise return error
pub fn ensure_initialized(settings: &Settings) -> Result<()> {
    if !settings.project_root.join(NORVEXUM_DIR).exists() {
        eyre::bail!(
            "Norvexum is not initialized in this directory ({}).\n\
             Run `norvexum init` to initialize first.",
            settings.project_root.display()
        );
    }
    Ok(())
}

pub async fn run(settings: Settings, initial_msg: Option<String>) -> Result<()> {
    let model_info = format!("{} ({})", settings.active_model, settings.active_provider);
    let initial_for_ui = initial_msg.clone();

    let (agent_tx, agent_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let (user_tx, mut user_rx) = mpsc::unbounded_channel::<String>();

    // Instantiate the agent
    let mut agent = Agent::new(settings.clone(), agent_tx.clone())?;
    let approval_tx = agent.approval_sender();

    let (cancel_tx, mut cancel_rx) = mpsc::unbounded_channel::<()>();

    // Spawn agent processing task
    let agent_tx_err = agent_tx.clone();
    let process_task = tokio::spawn(async move {
        // Send initial message if provided
        if let Some(msg) = initial_msg {
            tokio::select! {
                res = agent.process_message(&msg) => {
                    if let Err(e) = res {
                        let _ = agent_tx_err.send(AgentEvent::Error(format!("Agent error: {}", e)));
                    }
                }
                _ = cancel_rx.recv() => {
                    let _ = agent_tx_err.send(AgentEvent::Status("⏹️ Run stopped".into()));
                    let _ = agent_tx_err.send(AgentEvent::Done { usage: None });
                }
            }
        }

        while let Some(msg) = user_rx.recv().await {
            // Discard any leftover/buffered cancel signals before starting the next processing run
            while cancel_rx.try_recv().is_ok() {}

            tokio::select! {
                res = agent.process_message(&msg) => {
                    if let Err(e) = res {
                        let _ = agent_tx_err.send(AgentEvent::Error(format!("Agent error: {}", e)));
                    }
                }
                _ = cancel_rx.recv() => {
                    let _ = agent_tx_err.send(AgentEvent::Status("⏹️ Run stopped".into()));
                    let _ = agent_tx_err.send(AgentEvent::Done { usage: None });
                }
            }
        }
    });

    if settings.headless {
        // Print message and run in plain stdout/stdin mode
        println!(
            "Norvexum interactive chat (Headless mode) - model: {}",
            model_info
        );
        println!("Press Ctrl+C to exit.\n");

        let agent_rx_print = agent_rx;
        let approval_tx_print = approval_tx.clone();
        let print_task = tokio::spawn(async move {
            let mut rx = agent_rx_print;
            while let Some(event) = rx.recv().await {
                match event {
                    AgentEvent::Thinking(text) => {
                        print!("{}", text);
                        use std::io::Write;
                        let _ = std::io::stdout().flush();
                    }
                    AgentEvent::ThinkingDone => {
                        println!("\n--- Thinking Done ---");
                    }
                    AgentEvent::Content(text) => {
                        print!("{}", text);
                        use std::io::Write;
                        let _ = std::io::stdout().flush();
                    }
                    AgentEvent::ToolStart { name, id: _ } => {
                        println!("\n🔧 Calling tool: {}...", name);
                    }
                    AgentEvent::ToolResult {
                        id: _,
                        name,
                        result: _,
                        success,
                    } => {
                        println!(
                            "🔧 Tool {} finished: {}",
                            name,
                            if success { "✅ Success" } else { "❌ Failed" }
                        );
                    }
                    AgentEvent::FileWrite {
                        path,
                        content_preview: _,
                    } => {
                        println!("📝 File written: {}", path);
                    }
                    AgentEvent::Done { usage } => {
                        if let Some(u) = usage {
                            println!("\n--- Done ({} tokens) ---", u.total_tokens);
                        } else {
                            println!("\n--- Done ---");
                        }
                    }
                    AgentEvent::Error(e) => {
                        eprintln!("\n❌ Error: {}", e);
                    }
                    AgentEvent::Status(s) => {
                        println!("Status: {}", s);
                    }
                    AgentEvent::ApprovalRequest {
                        id: _,
                        tool_name,
                        args_preview,
                    } => {
                        let mut approved = true;
                        let mut reason = "[Auto-approved in headless mode]".to_string();

                        if tool_name == "run_command" {
                            if let Ok(args_val) =
                                serde_json::from_str::<serde_json::Value>(&args_preview)
                            {
                                if let Some(cmd_str) =
                                    args_val.get("command").and_then(|v| v.as_str())
                                {
                                    if crate::tools::shell::is_unparseable_or_fallback(cmd_str) {
                                        approved = false;
                                        reason = "[Auto-denied in headless mode: forced shell fallback containing wildcards/metacharacters is blocked]".to_string();
                                    }
                                }
                            }
                        }

                        println!(
                            "\n⚠️  Tool '{}' wants to execute:\n  {}\n  {}",
                            tool_name, args_preview, reason
                        );
                        let _ = approval_tx_print.send(approved);
                    }
                    _ => {}
                }
            }
        });

        // Read lines from stdin
        use std::io::BufRead;
        let stdin = std::io::stdin();
        let mut handle = stdin.lock();
        loop {
            print!("\n> ");
            use std::io::Write;
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            if handle.read_line(&mut line)? == 0 {
                break; // EOF
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed == "exit" || trimmed == "quit" {
                break;
            }
            let _ = user_tx.send(trimmed.to_string());
        }

        print_task.abort();
    } else {
        // Run standard interactive TUI mode
        let mut app = App::new(&model_info);
        if let Some(message) = initial_for_ui {
            app.add_user_message(message);
        }
        run_tui(app, agent_rx, user_tx, cancel_tx, approval_tx)
            .await
            .wrap_err("TUI runtime encountered an error")?;
    }

    process_task.abort();
    Ok(())
}
