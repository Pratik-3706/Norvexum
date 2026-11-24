// ===========================================================================
// manage_processes — list / inspect / stop background commands started by
// run_command(background=true)
// ===========================================================================

use async_trait::async_trait;
use serde_json::json;

use super::shell::{StoppedBy, background_snapshot, get_background_processes, stop_and_wait};
use super::{Tool, ToolContext, ToolResult};

/// How long stopping may take (killing a process tree on Windows is not instant).
const STOP_WAIT_SECS: u64 = 15;

pub struct ManageProcessesTool;

#[async_trait]
impl Tool for ManageProcessesTool {
    fn name(&self) -> &str {
        "manage_processes"
    }

    fn description(&self) -> &str {
        "Manage commands running in the background (started with background=true, or still running \
         when run_command stopped waiting): 'list' shows running and recently finished ones, 'wait' \
         waits up to `secs` for one to finish and returns its status and latest output, 'logs' shows \
         recent output (also after it exited), 'stop' terminates it and its child processes and returns once it has ended."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["list", "wait", "logs", "stop"], "description": "What to do" },
                "pid": { "type": "integer", "description": "Process id (required for wait/logs/stop)" },
                "secs": { "type": "integer", "description": "For wait: how long to wait at most (default 30, max 600)" },
                "lines": { "type": "integer", "description": "For logs/wait: how many trailing lines to show (default 60)" }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let action = args["action"].as_str().unwrap_or("list");
        let pid = args["pid"].as_u64().map(|p| p as u32);
        if action == "wait" {
            let Some(pid) = pid else {
                return ToolResult::err("'pid' is required for wait");
            };
            let secs = args["secs"].as_u64().unwrap_or(30).clamp(1, 600);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
            while get_background_processes().iter().any(|p| p.pid == pid)
                && std::time::Instant::now() < deadline
            {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            let still = get_background_processes().iter().any(|p| p.pid == pid);
            let mut logs = self
                .execute(
                    json!({"action": "logs", "pid": pid, "lines": args["lines"]}),
                    _ctx,
                )
                .await;
            if still {
                logs.output = format!(
                    "pid {pid} is still running after waiting {secs}s.\n{}",
                    logs.output
                );
            }
            return logs;
        }
        let (processes, finished) = background_snapshot();

        match action {
            "list" => {
                if processes.is_empty() && finished.is_empty() {
                    return ToolResult::ok("No background processes are running.");
                }
                let running = processes.iter().map(|p| {
                    format!(
                        "pid {} — running {}s — {}",
                        p.pid,
                        p.started.elapsed().as_secs(),
                        crate::util::truncate_chars(&p.command, 120)
                    )
                });
                let done = finished.iter().rev().map(|f| {
                    format!(
                        "pid {} — {} after {}s — {}",
                        f.info.pid,
                        f.exit_label(),
                        f.ran_for.as_secs(),
                        crate::util::truncate_chars(&f.info.command, 120)
                    )
                });
                ToolResult::ok(running.chain(done).collect::<Vec<_>>().join("\n"))
            }
            "logs" | "stop" => {
                let Some(pid) = pid else {
                    return ToolResult::err("'pid' is required for this action");
                };
                let exited = finished.iter().rev().find(|f| f.info.pid == pid);
                if action == "stop" {
                    if let Some(f) = exited.filter(|_| !processes.iter().any(|p| p.pid == pid)) {
                        return ToolResult::ok(format!(
                            "pid {pid} had already ended: it {} after {}s.",
                            f.exit_label(),
                            f.ran_for.as_secs()
                        ));
                    }
                }
                let info = processes
                    .iter()
                    .find(|p| p.pid == pid)
                    .or(exited.map(|f| &f.info));
                let Some(info) = info else {
                    return ToolResult::err(format!(
                        "No background process with pid {pid}. Running: {}",
                        if processes.is_empty() {
                            "none".to_string()
                        } else {
                            processes
                                .iter()
                                .map(|p| p.pid.to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        }
                    ));
                };

                if action == "stop" {
                    let limit = std::time::Duration::from_secs(STOP_WAIT_SECS);
                    return match stop_and_wait(pid, StoppedBy::Agent, limit).await {
                        Some(f) if f.stopped_by == Some(StoppedBy::Agent) => {
                            ToolResult::ok(format!("Stopped pid {pid} ({})", info.command))
                        }
                        // It ended by itself, or someone else stopped it first.
                        Some(f) => ToolResult::ok(format!(
                            "pid {pid} {} before it could be stopped.",
                            f.exit_label()
                        )),
                        None => ToolResult::err(format!(
                            "pid {pid} is still running {STOP_WAIT_SECS}s after being told to \
                             stop. Check it with action=\"list\"."
                        )),
                    };
                }

                let wanted = args["lines"].as_u64().unwrap_or(60).clamp(1, 500) as usize;
                match std::fs::read(&info.log_path) {
                    Ok(bytes) => {
                        let text = String::from_utf8_lossy(&bytes);
                        let lines: Vec<&str> = text.lines().collect();
                        let tail = &lines[lines.len().saturating_sub(wanted)..];
                        let state = match exited {
                            Some(f) if !processes.iter().any(|p| p.pid == pid) => {
                                format!(" ({})", f.exit_label())
                            }
                            _ => String::new(),
                        };
                        if tail.is_empty() {
                            ToolResult::ok(format!(
                                "pid {pid}{state} has not printed anything yet. If the command \
                                 redirects its output (> file), read that file instead."
                            ))
                        } else {
                            ToolResult::ok(format!(
                                "Last {} lines of pid {pid} output{state}:\n{}",
                                tail.len(),
                                tail.join("\n")
                            ))
                        }
                    }
                    Err(e) => ToolResult::err(format!("Could not read the log for pid {pid}: {e}")),
                }
            }
            other => ToolResult::err(format!("Unknown action '{other}'. Use list, logs or stop.")),
        }
    }
}
