// ═══════════════════════════════════════════════════════════════════════════
// Shell Command execution tool
// ═══════════════════════════════════════════════════════════════════════════

use async_trait::async_trait;
use serde_json::json;

use super::{Tool, ToolContext, ToolResult};

pub struct ShellCommandTool;

#[async_trait]
impl Tool for ShellCommandTool {
    fn name(&self) -> &str {
        "run_command"
    }

    fn description(&self) -> &str {
        "Execute a shell command inside the project directory. Use this to run builds, tests, compilers, or git commands."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The command line string to run" }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let command_str = args["command"].as_str().unwrap_or("");
        if command_str.is_empty() {
            return ToolResult::err("Command string cannot be empty");
        }

        // Run command via powershell on Windows, or sh on Unix
        let (shell, command_args) = if cfg!(target_os = "windows") {
            ("powershell", vec!["-NoProfile", "-Command", command_str])
        } else {
            ("sh", vec!["-c", command_str])
        };

        let output_res = tokio::process::Command::new(shell)
            .args(&command_args)
            .current_dir(&ctx.cwd)
            .kill_on_drop(true)
            .output()
            .await;

        match output_res {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let success = output.status.success();

                let summary = format!(
                    "Command finished with exit code: {}\n\nSTDOUT:\n{}\n\nSTDERR:\n{}",
                    output.status.code().unwrap_or(-1),
                    stdout,
                    stderr
                );

                if success {
                    ToolResult::ok_with_data(
                        summary,
                        json!({
                            "exit_code": output.status.code().unwrap_or(-1),
                            "success": true
                        }),
                    )
                } else {
                    ToolResult::err(summary)
                }
            }
            Err(e) => ToolResult::err(format!("Failed to execute command: {}", e)),
        }
    }
}
