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

/// Foreground commands are stopped after this long unless the call asks for more.
const DEFAULT_TIMEOUT_SECS: u64 = 300;
const MAX_TIMEOUT_SECS: u64 = 1800;

#[derive(Clone, Debug)]
pub struct ProcessInfo {
    pub pid: u32,
    pub command: String,
    pub kill_tx: tokio::sync::mpsc::Sender<()>,
    pub started: std::time::Instant,
    /// stdout + stderr of the process.
    pub log_path: std::path::PathBuf,
}

pub static BACKGROUND_PROCESSES: Lazy<Mutex<HashMap<u32, ProcessInfo>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

fn processes() -> std::sync::MutexGuard<'static, HashMap<u32, ProcessInfo>> {
    // A panic while holding the lock must not disable process management.
    BACKGROUND_PROCESSES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn get_background_processes() -> Vec<ProcessInfo> {
    let mut list: Vec<ProcessInfo> = processes().values().cloned().collect();
    list.sort_by_key(|p| p.started);
    list
}

pub fn kill_background_process(pid: u32) {
    // Remove the process and send the kill signal to the background task
    if let Some(info) = processes().remove(&pid) {
        let _ = info.kill_tx.try_send(());
    }
}

/// Kill a process and everything it started. On Windows, killing `cmd.exe`
/// alone leaves the actual server (python, node, …) running.
async fn kill_tree(child: &mut tokio::process::Child) {
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let mut taskkill = tokio::process::Command::new("taskkill");
        taskkill
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(0x08000000);
        let _ = taskkill.status().await;
    }
    let _ = child.kill().await;
}

/// Start `cmd` detached from the agent, logging its output to `.norvexum/logs/`.
fn spawn_background(
    mut cmd: tokio::process::Command,
    command_str: &str,
    ctx: &ToolContext,
) -> ToolResult {
    let log_dir = ctx
        .settings
        .project_root
        .join(crate::config::NORVEXUM_DIR)
        .join("logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join(format!(
        "bg-{}.log",
        chrono::Local::now().format("%Y%m%d-%H%M%S-%3f")
    ));
    let (stdout, stderr) =
        match std::fs::File::create(&log_path).and_then(|f| Ok((f.try_clone()?, f))) {
            Ok((out, err)) => (
                std::process::Stdio::from(out),
                std::process::Stdio::from(err),
            ),
            Err(_) => (std::process::Stdio::null(), std::process::Stdio::null()),
        };
    cmd.stdin(std::process::Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .kill_on_drop(false);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return ToolResult::err(format!("Failed to spawn background command: {e}")),
    };
    let Some(pid) = child.id() else {
        return ToolResult::ok("Command started in background (no PID available)");
    };

    let (kill_tx, mut kill_rx) = tokio::sync::mpsc::channel(1);
    processes().insert(
        pid,
        ProcessInfo {
            pid,
            command: command_str.to_string(),
            kill_tx,
            started: std::time::Instant::now(),
            log_path: log_path.clone(),
        },
    );

    tokio::spawn(async move {
        tokio::select! {
            _ = child.wait() => {}
            _ = kill_rx.recv() => kill_tree(&mut child).await,
        }
        processes().remove(&pid);
    });

    ToolResult::ok(format!(
        "Started in background with PID {pid}. Output is logged; check it with          manage_processes(action=\"logs\", pid={pid}) and stop it with manage_processes(action=\"stop\", pid={pid})."
    ))
}

/// Run `cmd` to completion with a timeout; the process tree is killed on timeout.
async fn run_foreground(mut cmd: tokio::process::Command, timeout_secs: u64) -> ToolResult {
    // A child reading stdin would hang forever (and steal the TUI's keystrokes).
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return ToolResult::err(format!("Failed to execute command: {e}")),
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let read_all = |pipe: Option<tokio::process::ChildStdout>| async move {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = tokio::io::AsyncReadExt::read_to_end(&mut p, &mut buf).await;
        }
        buf
    };
    let read_err = |pipe: Option<tokio::process::ChildStderr>| async move {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = tokio::io::AsyncReadExt::read_to_end(&mut p, &mut buf).await;
        }
        buf
    };

    let run = async {
        let (out, err, status) = tokio::join!(read_all(stdout), read_err(stderr), child.wait());
        (out, err, status)
    };
    match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), run).await {
        Ok((out, err, status)) => handle_output(status.map(|status| std::process::Output {
            status,
            stdout: out,
            stderr: err,
        })),
        Err(_) => {
            kill_tree(&mut child).await;
            ToolResult::err(format!(
                "Command timed out after {timeout_secs}s and was stopped. If it is a server or watcher,                  run it with background=true; if it waits for input, use non-interactive flags;                  otherwise pass a larger timeout_secs (max {MAX_TIMEOUT_SECS})."
            ))
        }
    }
}

/// Commands that never exit on their own (dev servers, watchers).
fn looks_long_running(command: &str) -> bool {
    static RE: Lazy<regex::Regex> = Lazy::new(|| {
        regex::Regex::new(
            r"(?i)(\bhttp\.server\b|\bnpm\s+(start|run\s+(dev|serve|start|watch))\b|\b(yarn|pnpm)\s+(dev|start|serve)\b|^\s*(npx\s+)?serve\b|\bfastapi\s+dev\b|\buvicorn\s|\bflask\s+run\b|\bmanage\.py\s+runserver\b|\bvite(\s|$)|\bnext\s+dev\b|\bcargo\s+watch\b|\blive-server\b)",
        )
        .expect("valid regex")
    });
    RE.is_match(command)
}

pub fn is_shell_builtin(cmd_name: &str) -> bool {
    matches!(
        cmd_name,
        "exit" | "echo" | "set" | "export" | "dir" | "cls" | "start"
    )
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
    let has_meta = command_str.contains('|')
        || command_str.contains(';')
        || command_str.contains("&&")
        || command_str.contains('>')
        || command_str.contains('<')
        || command_str.contains('$')
        || command_str.contains('`')
        || command_str.contains('*')
        || command_str.contains('?')
        || command_str.contains('[')
        || contains_single_ampersand(command_str);

    has_meta || (cfg!(windows) && command_str.contains('\\'))
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
        if cfg!(windows)
            && stripped.starts_with('/')
            && stripped.len() > 2
            && stripped.chars().nth(2) == Some(':')
        {
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
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let code = output.status.code().unwrap_or(-1);

            // Empty sections are omitted — they'd cost tokens on every later request.
            let mut summary = format!("Exit code: {code}");
            if !stdout.trim().is_empty() {
                summary.push_str(&format!("\n\nSTDOUT:\n{}", stdout.trim_end()));
            }
            if !stderr.trim().is_empty() {
                summary.push_str(&format!("\n\nSTDERR:\n{}", stderr.trim_end()));
            }
            if stdout.trim().is_empty() && stderr.trim().is_empty() {
                summary.push_str(" (no output)");
            }

            if output.status.success() {
                ToolResult::ok(summary)
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
            super::filesystem::resolve_path(&clean, ctx)?;
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
        "Run a shell command in the project directory (stdin is closed, so interactive prompts fail). \
         Use non-interactive flags (e.g. `npm init -y`). Use edit_file instead of sed/awk. \
         Servers and watchers need background=true (manage them with manage_processes). \
         Foreground commands time out after 300s by default. Paths outside the project are blocked."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The command line string to run (must be non-interactive)" },
                "background": { "type": "boolean", "description": "Run detached (servers, watchers). Returns a PID." },
                "timeout_secs": { "type": "integer", "description": "Foreground timeout in seconds (default 300, max 1800)" }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let command_str = args["command"].as_str().unwrap_or("").trim();
        // Auto-detect common long-running servers in case the model forgot the flag.
        let background =
            args["background"].as_bool().unwrap_or(false) || looks_long_running(command_str);
        let timeout_secs = args["timeout_secs"]
            .as_u64()
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS);
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
            let mut cmd = if cfg!(target_os = "windows") {
                let mut c = tokio::process::Command::new("cmd.exe");
                c.arg("/C").raw_arg(command_str);
                c
            } else {
                let mut c = tokio::process::Command::new("sh");
                c.arg("-c").arg(command_str);
                c
            };

            cmd.current_dir(&ctx.cwd);

            #[cfg(windows)]
            {
                cmd.creation_flags(0x08000000);
            }

            if background {
                return spawn_background(cmd, command_str, ctx);
            }
            return run_foreground(cmd, timeout_secs).await;
        }

        // Direct execution path
        let argv = parsed_argv.unwrap();
        if argv.is_empty() {
            return ToolResult::err("Parsed command is empty");
        }

        let mut cmd = tokio::process::Command::new(&argv[0]);
        cmd.args(&argv[1..]).current_dir(&ctx.cwd);

        #[cfg(windows)]
        {
            cmd.creation_flags(0x08000000);
        }

        if background {
            return spawn_background(cmd, command_str, ctx);
        }
        run_foreground(cmd, timeout_secs).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_running_detection_uses_word_boundaries() {
        assert!(looks_long_running("python -m http.server 8080"));
        assert!(looks_long_running("npm run dev"));
        assert!(looks_long_running("npx serve ."));
        assert!(looks_long_running("uvicorn app:app --reload"));
        assert!(!looks_long_running("echo observe this"));
        assert!(!looks_long_running("cargo test --test server"));
        assert!(!looks_long_running("npm install express"));
    }
}
