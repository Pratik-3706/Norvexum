// ===========================================================================
// Shell Command execution tool — with sandbox escape detection
// ===========================================================================

use async_trait::async_trait;
use serde_json::json;
use std::path::Path;

use super::{Tool, ToolContext, ToolResult};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Clone, Debug)]
pub struct ProcessInfo {
    pub pid: u32,
    pub command: String,
    pub kill_tx: tokio::sync::mpsc::Sender<()>,
}

pub static BACKGROUND_PROCESSES: Lazy<Mutex<HashMap<u32, ProcessInfo>>> = Lazy::new(|| Mutex::new(HashMap::new()));

pub fn get_background_processes() -> Vec<ProcessInfo> {
    let map = BACKGROUND_PROCESSES.lock().unwrap();
    map.values().cloned().collect()
}

pub fn kill_background_process(pid: u32) {
    // Remove the process and send the kill signal to the background task
    if let Some(info) = BACKGROUND_PROCESSES.lock().unwrap().remove(&pid) {
        let _ = info.kill_tx.try_send(());
    }
}

pub fn is_shell_builtin(cmd_name: &str) -> bool {
    matches!(cmd_name, "exit" | "echo" | "set" | "export" | "dir" | "cls" | "start")
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

fn contains_single_ampersand(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    for i in 0..chars.len() {
        if chars[i] == '&' {
            let prev_is_amp = i > 0 && chars[i - 1] == '&';
            let next_is_amp = i + 1 < chars.len() && chars[i + 1] == '&';
            if !prev_is_amp && !next_is_amp {
                return true;
            }
        }
    }
    false
}

pub fn has_shell_metacharacters(command_str: &str) -> bool {
    command_str.contains('|')
        || command_str.contains(';')
        || command_str.contains("&&")
        || command_str.contains('>')
        || command_str.contains('<')
        || command_str.contains('$')
        || command_str.contains('`')
        || command_str.contains('*')
        || command_str.contains('?')
        || command_str.contains('[')
        || contains_single_ampersand(command_str)
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
    if (s.contains('/') || s.contains('\\') || s.starts_with('.') || s.starts_with('~'))
        && !is_url_with_non_file_scheme(s)
    {
        // On Windows, ignore forward-slash command line switches (e.g. /c, /all, /fo)
        if cfg!(windows)
            && s.starts_with('/')
            && s.len() > 1
            && !s[1..].contains('/')
            && !s[1..].contains('\\')
            && !s[1..].contains('.')
        {
            return false;
        }
        true
    } else {
        false
    }
}

fn clean_path_string(s: &str) -> String {
    let stripped = if let Some(stripped) = s.strip_prefix("file://") {
        if cfg!(windows) && stripped.starts_with('/') && stripped.len() > 2 && stripped.chars().nth(2) == Some(':') {
            &stripped[1..]
        } else {
            stripped
        }
    } else {
        s
    };
    stripped.replace("%20", " ")
}

fn extract_paths(token: &str) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some((left, right)) = token.split_once('=') {
        paths.push(left.to_string());
        paths.push(right.to_string());
    } else if token.starts_with('-') {
        if let Some(idx) = token.find(['/', '\\', '.', '~']) {
            paths.push(token[idx..].to_string());
        }
    } else {
        paths.push(token.to_string());
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

fn check_raw_command_paths(command_str: &str, ctx: &ToolContext) -> Result<(), String> {
    let words = command_str.split(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '=');
    for word in words {
        let clean = clean_path_string(word.trim());
        if !clean.is_empty() && looks_like_path(&clean) {
            if let Err(e) = super::filesystem::resolve_path(&clean, ctx) {
                return Err(e);
            }
        }
    }
    Ok(())
}

pub struct ShellCommandTool;

#[async_trait]
impl Tool for ShellCommandTool {
    fn name(&self) -> &str {
        "run_command"
    }

    fn description(&self) -> &str {
        "Execute a shell command inside the project directory. \
         IMPORTANT RULES: 1) NEVER run interactive commands (like `nano`, `vim`, or blocking scripts) that wait for user input. \
         2) Always use non-interactive flags (e.g. `apt-get -y`, `npm install --no-fund`). \
         3) DO NOT use this tool for text replacement (like `sed` or `awk`); use the `edit_file` tool instead. \
         4) Use background=true to run long-running servers without deadlocking. \
         Commands are sandboxed — attempts to escape the project directory will be blocked."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The command line string to run (must be non-interactive)" },
                "background": { "type": "boolean", "description": "Set to true to run the command in the background (detached). Useful for long-running servers." }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let command_str = args["command"].as_str().unwrap_or("");
        let mut background = args["background"].as_bool().unwrap_or(false);
        
        // Auto-detect common long-running servers and force background if the agent forgot
        if !background {
            let cmd_lower = command_str.to_lowercase();
            if cmd_lower.contains("http.server") 
                || cmd_lower.contains("npm start") 
                || cmd_lower.contains("npm run dev")
                || cmd_lower.contains("serve ")
                || cmd_lower.contains("fastapi dev")
                || cmd_lower.contains("uvicorn ")
                || cmd_lower.contains("flask run")
            {
                background = true;
            }
        }
        if command_str.is_empty() {
            return ToolResult::err("Command string cannot be empty");
        }

        let parsed_argv = shlex::split(command_str);

        // 1. Hard denylist block first
        if let Some(ref argv) = parsed_argv {
            if !argv.is_empty() {
                let cmd_name = get_command_name(&argv[0]);
                if is_denylisted(&cmd_name) {
                    return ToolResult::err(format!(
                        "⚠️ Command blocked: execution of '{}' is restricted for security.",
                        cmd_name
                    ));
                }
            }
        }

        // 2. Check raw command strings for Windows path escapes (catches shlex stripping backslashes)
        if let Err(e) = check_raw_command_paths(command_str, ctx) {
            return ToolResult::err(format!(
                "⚠️ Command blocked: directory escape attempt detected.\n{}",
                e
            ));
        }

        let fallback = is_unparseable_or_fallback(command_str)
            || parsed_argv
                .as_ref()
                .map(|argv| !argv.is_empty() && is_shell_builtin(&get_command_name(&argv[0])))
                .unwrap_or(false);

        // Pre-execution validations if parsed_argv is available
        if let Some(ref argv) = parsed_argv {
            if !argv.is_empty() {
                let cmd_name = get_command_name(&argv[0]);

                // 2. Standalone cd interception
                if cmd_name == "cd" {
                    if argv.len() > 1 {
                        let target_dir = clean_path_string(&argv[1]);
                        match super::filesystem::resolve_path(&target_dir, ctx) {
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
                if looks_like_path(&argv0_clean) {
                    if let Err(e) = super::filesystem::resolve_path(&argv0_clean, ctx) {
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
                        if let Err(e) = super::filesystem::resolve_path(&clean_token, ctx) {
                            return ToolResult::err(format!(
                                "⚠️ Command blocked: directory escape attempt detected in chmod argument.\n{}",
                                e
                            ));
                        }
                    }

                    for path_str in extract_paths(token) {
                        let path_clean = clean_path_string(&path_str);
                        if looks_like_path(&path_clean) {
                            if let Err(e) = super::filesystem::resolve_path(&path_clean, ctx) {
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
            // Fallback path: Run command via cmd.exe on Windows, or sh on Unix
            let (shell, command_args) = if cfg!(target_os = "windows") {
                ("cmd.exe", vec!["/C", command_str])
            } else {
                ("sh", vec!["-c", command_str])
            };

            let mut cmd = tokio::process::Command::new(shell);
            cmd.args(&command_args)
               .current_dir(&ctx.cwd)
               .kill_on_drop(!background);

            #[cfg(windows)]
            {
                cmd.creation_flags(0x08000000);
            }

            if background {
                cmd.stdin(std::process::Stdio::null());
                cmd.stdout(std::process::Stdio::null());
                cmd.stderr(std::process::Stdio::null());
                match cmd.spawn() {
                    Ok(mut child) => {
                        if let Some(pid) = child.id() {
                            let (kill_tx, mut kill_rx) = tokio::sync::mpsc::channel(1);
                            BACKGROUND_PROCESSES.lock().unwrap().insert(pid, ProcessInfo {
                                pid,
                                command: command_str.to_string(),
                                kill_tx,
                            });
                            
                            tokio::spawn(async move {
                                tokio::select! {
                                    _ = child.wait() => {}
                                    _ = kill_rx.recv() => {
                                        let _ = child.kill().await;
                                    }
                                }
                                BACKGROUND_PROCESSES.lock().unwrap().remove(&pid);
                            });
                            
                            return ToolResult::ok(format!("Command started in background with PID: {}", pid));
                        }
                        return ToolResult::ok("Command started in background (no PID available)".to_string());
                    }
                    Err(e) => return ToolResult::err(format!("Failed to spawn background command: {}", e)),
                }
            }

            let output_res = cmd.output().await;
            return handle_output(output_res);
        }

        // Direct execution path
        let argv = parsed_argv.unwrap();
        if argv.is_empty() {
            return ToolResult::err("Parsed command is empty");
        }

        let mut cmd = tokio::process::Command::new(&argv[0]);
        cmd.args(&argv[1..])
           .current_dir(&ctx.cwd)
           .kill_on_drop(!background);

        #[cfg(windows)]
        {
            cmd.creation_flags(0x08000000);
        }

        if background {
            cmd.stdin(std::process::Stdio::null());
            cmd.stdout(std::process::Stdio::null());
            cmd.stderr(std::process::Stdio::null());
            match cmd.spawn() {
                Ok(mut child) => {
                    if let Some(pid) = child.id() {
                        let (kill_tx, mut kill_rx) = tokio::sync::mpsc::channel(1);
                        BACKGROUND_PROCESSES.lock().unwrap().insert(pid, ProcessInfo {
                            pid,
                            command: command_str.to_string(),
                            kill_tx,
                        });
                        
                        tokio::spawn(async move {
                            tokio::select! {
                                _ = child.wait() => {}
                                _ = kill_rx.recv() => {
                                    let _ = child.kill().await;
                                }
                            }
                            BACKGROUND_PROCESSES.lock().unwrap().remove(&pid);
                        });
                        
                        return ToolResult::ok(format!("Command started in background with PID: {}", pid));
                    }
                    return ToolResult::ok("Command started in background (no PID available)".to_string());
                }
                Err(e) => return ToolResult::err(format!("Failed to spawn background command: {}", e)),
            }
        }

        let output_res = cmd.output().await;
        handle_output(output_res)
    }
}
