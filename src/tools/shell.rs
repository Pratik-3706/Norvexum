// ═══════════════════════════════════════════════════════════════════════════
// Shell Command execution tool — with sandbox escape detection
// ═══════════════════════════════════════════════════════════════════════════

use async_trait::async_trait;
use serde_json::json;

use super::{Tool, ToolContext, ToolResult};

/// Patterns that indicate a potential sandbox escape attempt.
const DANGEROUS_PATTERNS: &[&str] = &[
    "../../",
    "../..",
    "~/.ssh",
    "~/.aws",
    "~/.gnupg",
    "~/.config",
    "/etc/passwd",
    "/etc/shadow",
    "/etc/hosts",
    "C:\\Users\\",
    "C:\\Windows\\",
    "%USERPROFILE%",
    "%APPDATA%",
    "curl | sh",
    "curl | bash",
    "wget | sh",
    "wget | bash",
    "curl|sh",
    "curl|bash",
    "| bash",
    "| sh",
    "| powershell",
    "| cmd",
    "eval $(",
    "eval \"$(",
    "exec ",
    "rm -rf /",
    "rm -rf ~",
    "del /f /s /q C:\\",
    "format C:",
    "mkfs.",
    ":(){:|:&};:",  // Fork bomb
];

/// Patterns that indicate attempts to change directory out of sandbox.
const CD_ESCAPE_PATTERNS: &[&str] = &[
    "cd /",
    "cd ~",
    "cd ..",
    "cd C:\\",
    "pushd /",
    "pushd ~",
    "pushd ..",
    "Set-Location /",
    "Set-Location ~",
    "Set-Location ..",
    "sl /",
    "sl ~",
    "sl ..",
];

pub struct ShellCommandTool;

#[async_trait]
impl Tool for ShellCommandTool {
    fn name(&self) -> &str {
        "run_command"
    }

    fn description(&self) -> &str {
        "Execute a shell command inside the project directory. Use this to run builds, tests, compilers, or git commands. \
         Commands are sandboxed — attempts to escape the project directory will be blocked."
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

        // ── Sandbox escape detection ─────────────────────────────────────
        let lower_cmd = command_str.to_lowercase();

        for pattern in DANGEROUS_PATTERNS {
            if lower_cmd.contains(&pattern.to_lowercase()) {
                tracing::warn!("Blocked dangerous command pattern '{}' in: {}", pattern, command_str);
                return ToolResult::err(format!(
                    "⚠️ Command blocked: potential security risk detected.\n\
                     Pattern matched: `{}`\n\
                     Command: `{}`\n\n\
                     If you need this functionality, please ask the user to run it manually.",
                    pattern, command_str
                ));
            }
        }

        for pattern in CD_ESCAPE_PATTERNS {
            if lower_cmd.contains(&pattern.to_lowercase()) {
                // Check if this is actually trying to escape the sandbox
                // Allow `cd subdir` within the project
                let is_escape = !lower_cmd.contains("cd src")
                    && !lower_cmd.contains("cd ./")
                    && !lower_cmd.contains("cd .\\");

                if is_escape {
                    tracing::warn!("Blocked cd escape attempt: {}", command_str);
                    return ToolResult::err(format!(
                        "⚠️ Command blocked: directory escape attempt detected.\n\
                         Pattern: `{}`\n\
                         All commands must run within the project directory: {}\n\n\
                         Use relative paths within the project instead.",
                        pattern,
                        ctx.cwd.display()
                    ));
                }
            }
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
