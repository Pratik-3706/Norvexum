// ═══════════════════════════════════════════════════════════════════════════
// Shell Command execution tool — with sandbox escape detection
// ═══════════════════════════════════════════════════════════════════════════

use async_trait::async_trait;
use serde_json::json;
use std::path::Path;

use super::{Tool, ToolContext, ToolResult};

pub fn is_shell_builtin(cmd_name: &str) -> bool {
    matches!(cmd_name, "exit" | "echo" | "set" | "export" | "dir" | "cls")
}

/// Check if a command is unparseable or contains shell metacharacters/wildcards
/// requiring shell fallback. Exposes this to the agent for approval verification.
pub fn is_unparseable_or_fallback(command_str: &str) -> bool {
    if let Some(argv) = shlex::split(command_str) {
        if argv.is_empty() {
            return true;
        }
        has_shell_metacharacters(command_str)
    } else {
        true
    }
}

pub fn has_shell_metacharacters(command_str: &str) -> bool {
    command_str.contains('|')
        || command_str.contains(';')
        || command_str.contains("&&")
        || command_str.contains('>')
        || command_str.contains('`')
        || command_str.contains("$(")
        || command_str.contains('*')
        || command_str.contains('?')
        || command_str.contains('[')
}

fn is_url_with_non_file_scheme(s: &str) -> bool {
    if let Some(idx) = s.find("://") {
        let scheme = &s[..idx];
        if scheme.eq_ignore_ascii_case("file") {
            return false;
        }
        !scheme.is_empty() && scheme.chars().all(|c| c.is_ascii_alphabetic())
    } else {
        false
    }
}

fn looks_like_path(s: &str) -> bool {
    (s.contains('/') || s.contains('\\') || s.starts_with('.') || s.starts_with('~'))
        && !is_url_with_non_file_scheme(s)
}

fn clean_path_string(s: &str) -> &str {
    if let Some(stripped) = s.strip_prefix("file://") {
        stripped
    } else {
        s
    }
}

fn extract_paths(token: &str) -> Vec<&str> {
    let mut paths = Vec::new();
    if let Some((left, right)) = token.split_once('=') {
        paths.push(left);
        paths.push(right);
    } else if token.starts_with('-') {
        if let Some(idx) = token.find(['/', '\\', '.', '~']) {
            paths.push(&token[idx..]);
        }
    } else {
        paths.push(token);
    }
    paths
}

fn get_command_name(argv0: &str) -> String {
    let path = Path::new(argv0);
    let mut file_name = path
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or(argv0)
        .to_lowercase();

    if cfg!(target_os = "windows") {
        if let Some(stripped) = file_name.strip_suffix(".exe") {
            file_name = stripped.to_string();
        } else if let Some(stripped) = file_name.strip_suffix(".cmd") {
            file_name = stripped.to_string();
        } else if let Some(stripped) = file_name.strip_suffix(".bat") {
            file_name = stripped.to_string();
        }
    }
    file_name
}

fn is_denylisted(cmd_name: &str) -> bool {
    matches!(
        cmd_name,
        "sudo"
            | "su"
            | "dd"
            | "shutdown"
            | "reboot"
            | "passwd"
            | "useradd"
            | "userdel"
            | "systemctl"
    ) || cmd_name.starts_with("mkfs")
}

fn is_chmod_mode(arg: &str) -> bool {
    arg.chars().all(|c| c.is_ascii_digit())
        || arg.contains('+')
        || arg.contains('-')
        || arg.contains('=')
}

fn handle_output(output_res: std::io::Result<std::process::Output>) -> ToolResult {
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

        let parsed_argv = shlex::split(command_str);
        let fallback = is_unparseable_or_fallback(command_str)
            || parsed_argv
                .as_ref()
                .map(|argv| !argv.is_empty() && is_shell_builtin(&get_command_name(&argv[0])))
                .unwrap_or(false);

        // Pre-execution validations if parsed_argv is available
        if let Some(ref argv) = parsed_argv {
            if !argv.is_empty() {
                let cmd_name = get_command_name(&argv[0]);

                // 1. Hard denylist block
                if is_denylisted(&cmd_name) {
                    return ToolResult::err(format!(
                        "⚠️ Command blocked: execution of '{}' is restricted for security.",
                        cmd_name
                    ));
                }

                // 2. Standalone cd interception
                if cmd_name == "cd" {
                    if argv.len() > 1 {
                        let target_dir = clean_path_string(&argv[1]);
                        match super::filesystem::resolve_path(target_dir, ctx) {
                            Ok(resolved) => {
                                return ToolResult::ok(format!(
                                    "Directory changed to {}\n(Note: Directory changes do not persist across separate command runs. Use relative paths or chain commands with && instead.)",
                                    resolved.display()
                                ));
                            }
                            Err(e) => {
                                return ToolResult::err(format!(
                                    "⚠️ Command blocked: directory escape attempt detected in cd argument.\n{}",
                                    e
                                ));
                            }
                        }
                    } else {
                        let home =
                            dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("~"));
                        if ctx.settings.is_in_sandbox(&home) {
                            return ToolResult::ok(format!(
                                "Directory changed to {}",
                                home.display()
                            ));
                        } else {
                            return ToolResult::err(format!(
                                "⚠️ Command blocked: path '{}' is outside the project sandbox",
                                home.display()
                            ));
                        }
                    }
                }

                // 3. Path checking on argv[0]
                let argv0_clean = clean_path_string(&argv[0]);
                if looks_like_path(argv0_clean) {
                    if let Err(e) = super::filesystem::resolve_path(argv0_clean, ctx) {
                        return ToolResult::err(format!(
                            "⚠️ Command blocked: directory escape attempt detected in command name.\n{}",
                            e
                        ));
                    }
                }

                // 4. Path checking on other arguments
                let is_chmod = cmd_name == "chmod";
                for token in argv.iter().skip(1) {
                    if is_chmod
                        && !token.starts_with('-')
                        && !token.starts_with('+')
                        && !is_chmod_mode(token)
                    {
                        let clean_token = clean_path_string(token);
                        if let Err(e) = super::filesystem::resolve_path(clean_token, ctx) {
                            return ToolResult::err(format!(
                                "⚠️ Command blocked: directory escape attempt detected in chmod argument.\n{}",
                                e
                            ));
                        }
                    }

                    for path_str in extract_paths(token) {
                        let path_clean = clean_path_string(path_str);
                        if looks_like_path(path_clean) {
                            if let Err(e) = super::filesystem::resolve_path(path_clean, ctx) {
                                return ToolResult::err(format!(
                                    "⚠️ Command blocked: directory escape attempt detected.\n{}",
                                    e
                                ));
                            }
                        }
                    }
                }
            }
        }

        if fallback {
            // Fallback path: Run command via powershell on Windows, or sh on Unix
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

            return handle_output(output_res);
        }

        // Direct execution path
        let argv = parsed_argv.unwrap();
        if argv.is_empty() {
            return ToolResult::err("Parsed command is empty");
        }

        let output_res = tokio::process::Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&ctx.cwd)
            .kill_on_drop(true)
            .output()
            .await;

        handle_output(output_res)
    }
}
