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
const MAX_TIMEOUT_SECS: u64 = 3600;

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

/// A background command that has exited; kept so its log can still be read.
#[derive(Clone, Debug)]
pub struct FinishedProcess {
    pub info: ProcessInfo,
    /// Exit code, or `None` when it was stopped or killed by a signal.
    pub exit_code: Option<i32>,
    pub ran_for: std::time::Duration,
}

/// How many exited background commands are remembered.
const KEEP_FINISHED: usize = 20;

static FINISHED_PROCESSES: Lazy<Mutex<Vec<FinishedProcess>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// Background commands that have exited, oldest first.
pub fn get_finished_processes() -> Vec<FinishedProcess> {
    FINISHED_PROCESSES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn remember_finished(info: ProcessInfo, exit_code: Option<i32>) {
    let mut finished = FINISHED_PROCESSES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    finished.retain(|f| f.info.pid != info.pid);
    let ran_for = info.started.elapsed();
    finished.push(FinishedProcess {
        info,
        exit_code,
        ran_for,
    });
    let excess = finished.len().saturating_sub(KEEP_FINISHED);
    finished.drain(..excess);
}

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
        remember_finished(info, None);
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
        let exit_code = tokio::select! {
            status = child.wait() => status.ok().and_then(|s| s.code()),
            _ = kill_rx.recv() => {
                kill_tree(&mut child).await;
                None
            }
        };
        // Stopped processes were already removed by `kill_background_process`.
        let info = processes().remove(&pid);
        if let Some(info) = info.or_else(|| {
            get_finished_processes()
                .into_iter()
                .find(|f| f.info.pid == pid)
                .map(|f| f.info)
        }) {
            remember_finished(info, exit_code);
        }
    });

    let log = log_path
        .strip_prefix(&ctx.settings.project_root)
        .unwrap_or(&log_path)
        .display()
        .to_string()
        .replace('\\', "/");
    ToolResult::ok(format!(
        "Started in background with PID {pid}; stdout and stderr go to {log}. Read them with \
         manage_processes(action=\"logs\", pid={pid}) (works after it exits too) and stop it with \
         manage_processes(action=\"stop\", pid={pid})."
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

/// Split a command line into the commands chained by `&&`, `||`, `&`, `;`,
/// `|` or newlines, ignoring separators inside quotes.
fn split_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in command.chars() {
        match (quote, c) {
            (Some(q), _) if c == q => {
                quote = None;
                current.push(c);
            }
            (Some(_), _) => current.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                current.push(c);
            }
            (None, '&' | '|' | ';' | '\n') => {
                if !current.trim().is_empty() {
                    segments.push(std::mem::take(&mut current));
                }
                current.clear();
            }
            (None, _) => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        segments.push(current);
    }
    segments
}

/// Whitespace-separated words with quotes removed. Backslashes are kept as
/// they are: on Windows they separate path components, not escape characters.
fn split_words(segment: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut in_word = false;
    for c in segment.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                in_word = true;
            }
            None if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            None => {
                current.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(current);
    }
    words
}

/// Words the shell expands at run time (`$VAR`, `%VAR%`, `%%f`), which can't
/// be checked as written.
fn has_variable(word: &str) -> bool {
    word.contains('$') || (cfg!(windows) && word.contains('%'))
}

/// A Windows command switch such as `/C:"text"`, `/s` or `/fo:csv`, not a path.
fn is_windows_switch(word: &str) -> bool {
    static RE: Lazy<regex::Regex> =
        Lazy::new(|| regex::Regex::new(r"^/[A-Za-z?][A-Za-z0-9-]*(:.*)?$").expect("valid regex"));
    cfg!(windows) && RE.is_match(word)
}

/// Options after which interpreters take a program as text, not a file.
fn inline_code_flags(cmd_name: &str) -> &'static [&'static str] {
    match cmd_name {
        "python" | "python3" | "py" | "pythonw" => &["-c"],
        "php" => &["-r"],
        "node" | "deno" | "bun" => &["-e", "--eval", "-p", "--print"],
        "ruby" | "perl" => &["-e", "-E"],
        _ => &[],
    }
}

/// Resolve `path` relative to `cwd` inside the sandbox.
fn resolve_from(cwd: &Path, path: &str, ctx: &ToolContext) -> Result<std::path::PathBuf, String> {
    let joined = if Path::new(path).is_absolute() || path.starts_with('~') {
        path.to_string()
    } else {
        cwd.join(path).to_string_lossy().into_owned()
    };
    super::filesystem::resolve_path(&joined, ctx)
}

/// Every path a command line mentions must stay inside the project. Each
/// chained command is checked from the directory the earlier `cd`s left it
/// in, so `cd sub && … && cd ..` is fine. Inline interpreter code, shell
/// variables and Windows switches can't be paths and are left alone.
pub(super) fn check_command_paths(command: &str, ctx: &ToolContext) -> Result<(), String> {
    let mut cwd = ctx.cwd.clone();
    for segment in split_segments(command) {
        let words = split_words(&segment);
        let Some(first) = words.first() else {
            continue;
        };
        // Leading `@` only silences echo in batch syntax.
        let first = first.trim_start_matches('@');
        let cmd_name = get_command_name(first);

        if matches!(cmd_name.as_str(), "cd" | "chdir" | "pushd") {
            let target = words.iter().skip(1).find(|w| !w.eq_ignore_ascii_case("/d"));
            match target {
                Some(t) if has_variable(t) => {}
                Some(t) => cwd = resolve_from(&cwd, &clean_path_string(t), ctx)?,
                // Plain `cd` prints (Windows) or goes home (Unix); neither moves us here.
                None => {}
            }
            continue;
        }
        if cmd_name == "popd" {
            cwd = ctx.cwd.clone();
            continue;
        }
        // PowerShell's arguments are a script; a path check can't parse it.
        if matches!(cmd_name.as_str(), "powershell" | "pwsh") {
            continue;
        }
        // `cmd /c <command>` runs a command line of its own.
        if cmd_name == "cmd" {
            if let Some(i) = words
                .iter()
                .position(|w| w.eq_ignore_ascii_case("/c") || w.eq_ignore_ascii_case("/k"))
            {
                check_command_paths(&words[i + 1..].join(" "), ctx)?;
            }
            continue;
        }

        let first_clean = clean_path_string(first);
        if !has_variable(&first_clean) && looks_like_path(&first_clean) {
            resolve_from(&cwd, &first_clean, ctx)?;
        }
        let code_flags = inline_code_flags(&cmd_name);
        let mut skip_next = false;
        for word in words.iter().skip(1) {
            if std::mem::take(&mut skip_next) {
                continue;
            }
            if code_flags.contains(&word.as_str()) {
                skip_next = true;
                continue;
            }
            if has_variable(word) || is_windows_switch(word) {
                continue;
            }
            for path in extract_paths(word) {
                let path = clean_path_string(&path);
                if !path.is_empty() && looks_like_path(&path) {
                    resolve_from(&cwd, &path, ctx)?;
                }
            }
        }
    }
    Ok(())
}

/// Folder with the Unix tools that ship with Git for Windows (grep, tail, …).
#[cfg(windows)]
fn unix_tools_dir() -> Option<&'static std::path::PathBuf> {
    static DIR: Lazy<Option<std::path::PathBuf>> = Lazy::new(|| {
        // …\Git\cmd\git.exe → …\Git\usr\bin
        let from_git = which::which("git")
            .ok()
            .and_then(|git| Some(git.parent()?.parent()?.join("usr").join("bin")));
        let standard = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
            .iter()
            .filter_map(std::env::var_os)
            .map(|root| {
                let root = std::path::PathBuf::from(root);
                if root.ends_with("Local") {
                    root.join("Programs").join("Git").join("usr").join("bin")
                } else {
                    root.join("Git").join("usr").join("bin")
                }
            });
        from_git
            .into_iter()
            .chain(standard)
            .find(|dir| dir.join("tail.exe").is_file())
    });
    DIR.as_ref()
}

#[cfg(not(windows))]
fn unix_tools_dir() -> Option<&'static std::path::PathBuf> {
    None
}

/// Settings every command gets: no console window on Windows, the Unix tools
/// on PATH (after the system's own, so `find`/`sort` keep their Windows
/// meaning), and unbuffered Python output so background logs fill as it runs.
fn prepare_command(cmd: &mut tokio::process::Command) {
    cmd.env("PYTHONUNBUFFERED", "1");
    #[cfg(windows)]
    {
        cmd.creation_flags(0x08000000);
        if let Some(dir) = unix_tools_dir() {
            let mut paths: Vec<std::path::PathBuf> = std::env::var_os("PATH")
                .map(|p| std::env::split_paths(&p).collect())
                .unwrap_or_default();
            if !paths.contains(dir) {
                paths.push(dir.clone());
            }
            if let Ok(joined) = std::env::join_paths(paths) {
                cmd.env("PATH", joined);
            }
        }
    }
}

pub struct ShellCommandTool;

#[async_trait]
impl Tool for ShellCommandTool {
    fn name(&self) -> &str {
        "run_command"
    }

    fn description(&self) -> &str {
        static DESCRIPTION: Lazy<String> = Lazy::new(|| {
            let shell = if cfg!(windows) {
                match unix_tools_dir() {
                    Some(_) => "Commands run in cmd.exe; pipes (|), && and redirects work, and the Unix \
                                tools from Git for Windows (grep, head, tail, sed, awk, ls, wc, …) are on PATH. "
                        .to_string(),
                    None => "Commands run in cmd.exe; pipes (|), && and redirects work. There are no Unix \
                             tools (use findstr, dir, type, more). "
                        .to_string(),
                }
            } else {
                "Commands run in sh; pipes, && and redirects work. ".to_string()
            };
            format!(
                "Run a shell command in the project directory (stdin is closed, so interactive prompts fail). \
                 {shell}Inline code (python -c, php -r, node -e, powershell) is fine. \
                 Use non-interactive flags (e.g. `npm init -y`). Use edit_file instead of sed/awk for edits. \
                 Servers, watchers and long jobs need background=true (manage them with manage_processes). \
                 Foreground commands time out after {DEFAULT_TIMEOUT_SECS}s by default (max {MAX_TIMEOUT_SECS}s). \
                 Paths outside the project are blocked."
            )
        });
        &DESCRIPTION
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The command line string to run (must be non-interactive)" },
                "background": { "type": "boolean", "description": "Run detached (servers, watchers). Returns a PID." },
                "timeout_secs": { "type": "integer", "description": "Foreground timeout in seconds (default 300, max 3600)" }
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

        // 2. Paths in every part of the command line must stay inside the project.
        if let Err(e) = check_command_paths(command_str, ctx) {
            return ToolResult::err(format!(
                "⚠️ Command blocked: it refers to a path outside the project.\n{e}"
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
                // A lone `cd` changes nothing for later calls; say so instead of running it.
                if cmd_name == "cd" && !fallback {
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
            }
        }

        if fallback {
            // Fallback path: Run command via cmd.exe on Windows, or sh on Unix
            #[cfg(windows)]
            let mut cmd = {
                let mut c = tokio::process::Command::new("cmd.exe");
                c.arg("/C").raw_arg(command_str);
                c
            };
            #[cfg(not(windows))]
            let mut cmd = {
                let mut c = tokio::process::Command::new("sh");
                c.arg("-c").arg(command_str);
                c
            };

            cmd.current_dir(&ctx.cwd);
            prepare_command(&mut cmd);

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
        prepare_command(&mut cmd);

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
