// ===========================================================================
// Runtime — Spawns the agent task and wires its channels for any front-end
//
// The TUI, the GUI and headless mode all talk to the agent the same way:
//   • `user_tx`     — a new message (queued while a run is active)
//   • `inst_tx`     — an instruction injected into the *current* run
//   • `cancel_tx`   — stop the current run
//   • `approval_tx` — answer a tool approval request
//   • `events`      — everything the agent reports back
// ===========================================================================

use std::collections::VecDeque;
use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use eyre::Result;
use tokio::sync::mpsc;

use crate::agent::{Agent, AgentEvent};
use crate::config::Settings;

/// Prefix of the internal message the GUI uses to push new settings to the agent.
pub const CONFIG_UPDATE_PREFIX: &str = "/config_update ";

pub struct AgentHandle {
    pub events: mpsc::UnboundedReceiver<AgentEvent>,
    pub user_tx: mpsc::UnboundedSender<String>,
    pub inst_tx: mpsc::UnboundedSender<String>,
    pub cancel_tx: mpsc::UnboundedSender<()>,
    pub approval_tx: mpsc::UnboundedSender<bool>,
    pub is_processing: Arc<AtomicBool>,
}

/// Spawn the agent on the current Tokio runtime.
///
/// `initial` messages are processed in order before anything sent later.
pub fn spawn_agent(settings: Settings, initial: Vec<String>) -> AgentHandle {
    let (event_tx, events) = mpsc::unbounded_channel::<AgentEvent>();
    let (user_tx, mut user_rx) = mpsc::unbounded_channel::<String>();
    let (cancel_tx, mut cancel_rx) = mpsc::unbounded_channel::<()>();
    let (inst_tx, inst_rx) = mpsc::unbounded_channel::<String>();
    let (queue_tx, mut queue_rx) = mpsc::unbounded_channel::<String>();

    let mut agent = Agent::new(settings, event_tx.clone());
    agent.set_instruction_receiver(inst_rx);
    let approval_tx = agent.approval_sender();
    let is_processing = Arc::new(AtomicBool::new(false));

    // Router: while a run is active, plain messages become mid-run instructions.
    {
        let is_processing = is_processing.clone();
        let inst_tx = inst_tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = user_rx.recv().await {
                let busy = is_processing.load(Ordering::SeqCst);
                if busy && !msg.starts_with(CONFIG_UPDATE_PREFIX) && !msg.starts_with('/') {
                    let _ = inst_tx.send(msg);
                } else {
                    let _ = queue_tx.send(msg);
                }
            }
        });
    }

    {
        let is_processing = is_processing.clone();
        tokio::spawn(async move {
            let mut pending: VecDeque<String> = initial.into();
            loop {
                let msg = match pending.pop_front() {
                    Some(msg) => msg,
                    None => {
                        // Stale cancel presses from a finished run must not cancel the next one.
                        while cancel_rx.try_recv().is_ok() {}
                        match queue_rx.recv().await {
                            Some(msg) => msg,
                            None => break,
                        }
                    }
                };

                if let Some(json) = msg.strip_prefix(CONFIG_UPDATE_PREFIX) {
                    apply_config_update(&mut agent, json, &event_tx);
                    continue;
                }

                is_processing.store(true, Ordering::SeqCst);
                match agent.process_message(&msg, &mut cancel_rx).await {
                    Ok(unhandled) => pending.extend(unhandled),
                    Err(e) => {
                        let _ = event_tx.send(AgentEvent::Error(format!("Agent error: {e}")));
                        let _ = event_tx.send(AgentEvent::Done { usage: None });
                    }
                }
                is_processing.store(false, Ordering::SeqCst);
                // Instructions that raced with the end of the run become the next message.
                pending.extend(agent.drain_instructions());
            }
        });
    }

    AgentHandle {
        events,
        user_tx,
        inst_tx,
        cancel_tx,
        approval_tx,
        is_processing,
    }
}

fn apply_config_update(
    agent: &mut Agent,
    json: &str,
    event_tx: &mpsc::UnboundedSender<AgentEvent>,
) {
    let status = match serde_json::from_str::<Settings>(json) {
        Ok(new_settings) => {
            let root = new_settings.project_root.display().to_string();
            match agent.update_settings(new_settings) {
                Ok(()) => format!("⚙️ Workspace root set to: {root}"),
                Err(e) => format!("⚠️ Config update failed: {e}"),
            }
        }
        Err(e) => format!("⚠️ Config parse error: {e}"),
    };
    let _ = event_tx.send(AgentEvent::Status(status));
}

// ── Headless front-end ────────────────────────────────────────────────────

pub struct HeadlessOptions {
    /// How many messages were passed to `spawn_agent` as `initial`.
    pub initial_count: usize,
    /// Exit after the initial messages have been answered.
    pub one_shot: bool,
    /// Approve every tool call without asking.
    pub auto_approve: bool,
    /// Print status lines and reasoning to stderr.
    pub verbose: bool,
}

/// Plain-text front-end. The assistant's answer goes to **stdout**; tool
/// activity, status and errors go to **stderr**, so `norvexum -p ... > out.md`
/// captures just the answer.
pub async fn run_headless(mut handle: AgentHandle, opts: HeadlessOptions) -> Result<()> {
    let interactive = std::io::stdin().is_terminal();
    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();

    // In one-shot mode a piped stdin is extra context, not a stream of prompts;
    // it has already been folded into the initial message by the caller.
    let read_stdin = !opts.one_shot;
    if read_stdin {
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let mut line = String::new();
            loop {
                line.clear();
                match stdin.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if line_tx.send(line.trim_end().to_string()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    } else {
        drop(line_tx);
    }

    let mut initial_left = opts.initial_count;
    let mut busy = initial_left > 0;
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut stdin_open = read_stdin;
    let mut ctrl_c_armed = false;
    let mut run_failed = false;
    let mut at_line_start = true;

    if interactive && !opts.one_shot {
        eprintln!("Norvexum headless chat — type a message, `exit` to quit, Ctrl+C to stop a run.");
        if !busy {
            prompt();
        }
    }

    loop {
        tokio::select! {
            event = handle.events.recv() => {
                let Some(event) = event else { break };
                match event {
                    AgentEvent::Content(text) => {
                        print!("{text}");
                        let _ = std::io::stdout().flush();
                        at_line_start = text.ends_with('\n');
                    }
                    AgentEvent::Thinking(text) if opts.verbose => {
                        eprint!("\x1b[2m{text}\x1b[0m");
                    }
                    AgentEvent::ThinkingDone if opts.verbose => eprintln!(),
                    AgentEvent::ToolStart { name, .. } => {
                        if !at_line_start {
                            println!();
                            at_line_start = true;
                        }
                        eprintln!("\x1b[33m⚙ {name}\x1b[0m");
                    }
                    AgentEvent::ToolResult { name, success, result, .. } => {
                        if success {
                            if opts.verbose {
                                eprintln!("\x1b[32m✓ {name}\x1b[0m");
                            }
                        } else {
                            let first_line = result.lines().next().unwrap_or_default();
                            eprintln!("\x1b[31m✗ {name}: {}\x1b[0m", crate::util::truncate_chars(first_line, 160));
                        }
                    }
                    AgentEvent::FileWrite { path, .. } => eprintln!("\x1b[36m✎ {path}\x1b[0m"),
                    AgentEvent::Status(s) if opts.verbose => eprintln!("\x1b[2m{s}\x1b[0m"),
                    AgentEvent::Error(e) => {
                        run_failed = true;
                        eprintln!("\x1b[31merror: {e}\x1b[0m");
                    }
                    AgentEvent::ApprovalRequest { tool_name, args_preview, .. } => {
                        let approved = opts.auto_approve
                            || !headless_should_deny(&tool_name, &args_preview);
                        eprintln!(
                            "\x1b[33m{} {tool_name}: {}\x1b[0m",
                            if approved { "auto-approved" } else { "denied (use --yes to allow)" },
                            crate::util::truncate_chars(&args_preview, 200)
                        );
                        let _ = handle.approval_tx.send(approved);
                    }
                    AgentEvent::Done { usage } => {
                        if !at_line_start {
                            println!();
                            at_line_start = true;
                        }
                        if opts.verbose {
                            if let Some(u) = usage {
                                eprintln!("\x1b[2m— {} tokens —\x1b[0m", u.total_tokens);
                            }
                        }
                        if initial_left > 0 {
                            initial_left -= 1;
                            if initial_left > 0 {
                                continue;
                            }
                        }
                        busy = false;
                        ctrl_c_armed = false;
                        if let Some(next) = queue.pop_front() {
                            let _ = handle.user_tx.send(next);
                            busy = true;
                        } else if opts.one_shot || !stdin_open {
                            break;
                        } else if interactive {
                            prompt();
                        }
                    }
                    AgentEvent::Quit => break,
                    _ => {}
                }
            }
            line = line_rx.recv(), if stdin_open => {
                match line {
                    Some(line) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            if interactive && !busy { prompt(); }
                            continue;
                        }
                        if trimmed == "exit" || trimmed == "quit" {
                            break;
                        }
                        if busy {
                            queue.push_back(trimmed.to_string());
                        } else {
                            let _ = handle.user_tx.send(trimmed.to_string());
                            busy = true;
                        }
                    }
                    None => {
                        stdin_open = false;
                        if !busy && queue.is_empty() {
                            break;
                        }
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                if busy && !ctrl_c_armed {
                    eprintln!("\n\x1b[33mStopping… press Ctrl+C again to quit.\x1b[0m");
                    queue.clear();
                    let _ = handle.cancel_tx.send(());
                    ctrl_c_armed = true;
                } else {
                    break;
                }
            }
        }
    }
    if opts.one_shot && run_failed {
        eyre::bail!("the request failed (see the error above)");
    }
    Ok(())
}

fn prompt() {
    eprint!("\n\x1b[36m›\x1b[0m ");
    let _ = std::io::stderr().flush();
}

/// Without `--yes`, headless runs refuse shell commands that need a real shell
/// (pipes, redirects, globs) because nobody is there to review them.
fn headless_should_deny(tool_name: &str, args_preview: &str) -> bool {
    if tool_name != "run_command" {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(args_preview)
        .ok()
        .and_then(|v| {
            v.get("command")
                .and_then(|c| c.as_str())
                .map(str::to_string)
        })
        .is_none_or(|cmd| crate::tools::shell::is_unparseable_or_fallback(&cmd))
}
