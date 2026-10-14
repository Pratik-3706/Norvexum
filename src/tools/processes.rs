// ===========================================================================
// manage_processes — list / inspect / stop background commands started by
// run_command(background=true)
// ===========================================================================

use async_trait::async_trait;
use serde_json::json;

use super::shell::{get_background_processes, kill_background_process};
use super::{Tool, ToolContext, ToolResult};

pub struct ManageProcessesTool;

#[async_trait]
impl Tool for ManageProcessesTool {
    fn name(&self) -> &str {
        "manage_processes"
    }

    fn description(&self) -> &str {
        "Manage background commands started with run_command(background=true): \
         'list' shows them, 'logs' shows recent output of one (by pid), 'stop' terminates it and its child processes."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["list", "logs", "stop"], "description": "What to do" },
                "pid": { "type": "integer", "description": "Process id (required for logs/stop)" },
                "lines": { "type": "integer", "description": "For logs: how many trailing lines to show (default 60)" }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let action = args["action"].as_str().unwrap_or("list");
        let pid = args["pid"].as_u64().map(|p| p as u32);
        let processes = get_background_processes();

        match action {
            "list" => {
                if processes.is_empty() {
                    return ToolResult::ok("No background processes are running.");
                }
                let lines: Vec<String> = processes
                    .iter()
                    .map(|p| {
                        format!(
                            "pid {} — running {}s — {}",
                            p.pid,
                            p.started.elapsed().as_secs(),
                            crate::util::truncate_chars(&p.command, 120)
                        )
                    })
                    .collect();
                ToolResult::ok(lines.join("\n"))
            }
            "logs" | "stop" => {
                let Some(pid) = pid else {
                    return ToolResult::err("'pid' is required for this action");
                };
                let Some(info) = processes.iter().find(|p| p.pid == pid) else {
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
                    kill_background_process(pid);
                    return ToolResult::ok(format!("Stopped pid {pid} ({})", info.command));
                }

                let wanted = args["lines"].as_u64().unwrap_or(60).clamp(1, 500) as usize;
                match std::fs::read(&info.log_path) {
                    Ok(bytes) => {
                        let text = String::from_utf8_lossy(&bytes);
                        let lines: Vec<&str> = text.lines().collect();
                        let tail = &lines[lines.len().saturating_sub(wanted)..];
                        if tail.is_empty() {
                            ToolResult::ok(format!("pid {pid} has not printed anything yet."))
                        } else {
                            ToolResult::ok(format!(
                                "Last {} lines of pid {pid} output:\n{}",
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
