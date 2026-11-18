// =======================================================================
// Git Tools — Version control awareness
//
// Provides git status, diff, commit, and log as agent tools.
// All operations scoped to the project root.
// ===========================================================================

use async_trait::async_trait;
use serde_json::json;

use super::{Tool, ToolContext, ToolResult, push_check};

//==== git_status ======================

pub struct GitStatusTool;

#[async_trait]
impl Tool for GitStatusTool {
    fn name(&self) -> &str {
        "git_status"
    }

    fn description(&self) -> &str {
        "Show the current git status of the project. Returns modified, staged, \
         and untracked files."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        run_git_command(&["status", "--porcelain", "-b"], ctx).await
    }
}

// ============= git_diff =================================

pub struct GitDiffTool;

#[async_trait]
impl Tool for GitDiffTool {
    fn name(&self) -> &str {
        "git_diff"
    }

    fn description(&self) -> &str {
        "Show the git diff of changed files. Optionally specify a file path \
         to see changes for a specific file, or 'staged' to see staged changes."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Optional file path to diff (default: all changes)" },
                "staged": { "type": "boolean", "description": "Show staged changes only (default: false)" }
            },
            "required": []
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let mut cmd_args = vec!["diff"];
        let staged = args["staged"].as_bool().unwrap_or(false);
        if staged {
            cmd_args.push("--cached");
        }

        let path_str = args["path"].as_str().unwrap_or("");
        if !path_str.is_empty() {
            cmd_args.push("--");
            cmd_args.push(path_str);
        }

        run_git_command(&cmd_args, ctx).await
    }
}

// ── git_commit ────────────────────────────────────────────────────────────

pub struct GitCommitTool;

#[async_trait]
impl Tool for GitCommitTool {
    fn name(&self) -> &str {
        "git_commit"
    }

    fn description(&self) -> &str {
        "Stage all changes and create a git commit with the given message. \
         Equivalent to `git add -A && git commit -m '<message>'`. \
         Write only the message itself: a co-author line is added for you when the user has it turned on."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "message": { "type": "string", "description": "Commit message" },
                "add_all": { "type": "boolean", "description": "Stage all changes before committing (default: true)" }
            },
            "required": ["message"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let message = args["message"]
            .as_str()
            .unwrap_or("Auto-commit by Norvexum");
        let add_all = args["add_all"].as_bool().unwrap_or(true);

        if add_all {
            // `add -A` must never sweep Norvexum's own sessions into a commit.
            push_check::ensure_norvexum_ignored(&ctx.cwd);
            let add_result = run_git_command(&["add", "-A"], ctx).await;
            if !add_result.success {
                return add_result;
            }
        }

        let message = with_co_author(message, ctx.settings.co_author_commits);
        run_git_command(&["commit", "-m", &message], ctx).await
    }
}

/// The commit message, ending with the Norvexum co-author line when `credit`
/// is on (and the message doesn't have it already).
pub fn with_co_author(message: &str, credit: bool) -> String {
    let message = message.trim_end();
    let already = message.lines().any(|line| {
        line.trim()
            .eq_ignore_ascii_case(crate::config::CO_AUTHOR_TRAILER)
    });
    if !credit || already {
        return message.to_string();
    }
    format!("{message}\n\n{}", crate::config::CO_AUTHOR_TRAILER)
}

// ── git_log ───────────────────────────────────────────────────────────────

pub struct GitLogTool;

#[async_trait]
impl Tool for GitLogTool {
    fn name(&self) -> &str {
        "git_log"
    }

    fn description(&self) -> &str {
        "Show recent git commit history. Returns a compact log of recent commits. \
         IMPORTANT: Use the 'count' parameter to limit the output to avoid filling your context window."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "count": { "type": "integer", "description": "Number of commits to show (default: 10)" },
                "oneline": { "type": "boolean", "description": "Use one-line format (default: true)" }
            },
            "required": []
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let count = args["count"].as_u64().unwrap_or(10);
        let oneline = args["oneline"].as_bool().unwrap_or(true);

        let count_str = format!("-{}", count);
        let mut cmd_args = vec!["log", &count_str];
        if oneline {
            cmd_args.push("--oneline");
        }

        run_git_command(&cmd_args, ctx).await
    }
}

// ── git_push ──────────────────────────────────────────────────────────────

pub struct GitPushTool;

#[async_trait]
impl Tool for GitPushTool {
    fn name(&self) -> &str {
        "git_push"
    }

    fn description(&self) -> &str {
        "Push the current branch (or `branch`) to its remote. Only call this when the user has \
         asked you to push in this conversation, and say which commits are going where first. \
         Before uploading, the commits are checked for secrets, keys and oversized files, and the \
         user is shown exactly what will be pushed. If the project has .env files, ask the user \
         with ask_user whether you may check their values, and pass the answer as `read_env`. \
         Never force-pushes."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "remote": { "type": "string", "description": "Remote to push to (default: the branch's upstream remote, else origin)" },
                "branch": { "type": "string", "description": "Local branch to push (default: the current branch)" },
                "set_upstream": { "type": "boolean", "description": "Set the remote branch as upstream (default: only when it has none yet)" },
                "read_env": { "type": "boolean", "description": "Whether the user allowed checking that no values from their .env files are being pushed. Ask them first; the values are never shown to you." }
            },
            "required": []
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let plan = match push_plan(&args, &ctx.cwd).await {
            Ok(plan) => plan,
            Err(e) => return ToolResult::err(e),
        };
        if plan.nothing_to_push() {
            return ToolResult::ok(plan.describe());
        }
        if plan.needs_env_answer() {
            return ToolResult::err(format!(
                "Not pushed yet. This project has {} with values in it. Ask the user with ask_user \
                 whether you may check that none of those values are in what's being pushed, e.g. \
                 options \"Yes, check my .env\" and \"No, push without checking it\". Then call \
                 git_push again with read_env set to their answer. The values are only compared \
                 inside the check; you never see them.",
                plan.env_files.join(", ")
            ));
        }
        if plan.blocked() {
            return ToolResult::err(format!(
                "{}\n\nNothing was pushed. Tell the user what the check found. To fix it, take the \
                 secret or file out of the commits (and out of the code: use an environment \
                 variable instead), and add secret files to .gitignore. Rewriting commits that \
                 hold them is for the user to decide. If they say it's a false alarm, they can push \
                 it themselves.",
                plan.describe_for_agent()
            ));
        }
        let mut push = vec!["push"];
        if plan.set_upstream {
            push.push("-u");
        }
        push.push(&plan.remote);
        push.push(&plan.branch);
        match git_output(&push, &ctx.cwd).await {
            Ok(out) => ToolResult::ok(format!(
                "{}\n\n{}",
                plan.describe_for_agent().replacen("Will push", "Pushed", 1),
                out.trim()
            )),
            Err(e) if is_behind_remote(&e) => ToolResult::err(format!(
                "Rejected, nothing was pushed: {}/{} has commits this branch doesn't have.\n{e}\n\n\
                 To fix it: git [\"pull\", \"--no-rebase\", \"{}\", \"{}\"]. If that reports \
                 conflicts, open each conflicted file (git_status marks them UU), resolve every \
                 <<<<<<< ======= >>>>>>> section so both sides' changes still work, then git \
                 [\"add\", <files>] and git [\"merge\", \"--continue\"]. Build or test, tell the \
                 user what conflicted and how you resolved it, then call git_push again. If the \
                 conflict isn't clear-cut, ask the user. Never force-push.",
                plan.remote, plan.branch, plan.remote, plan.branch
            )),
            Err(e) => ToolResult::err(format!(
                "The push failed, nothing was sent.\n{e}\nNever force-push."
            )),
        }
    }
}

/// Whether a failed push was rejected because the remote moved on.
fn is_behind_remote(error: &str) -> bool {
    let e = error.to_lowercase();
    e.contains("fetch first") || e.contains("non-fast-forward") || e.contains("[rejected]")
}

/// What a push would send, worked out before pushing.
#[derive(Debug)]
pub struct PushPlan {
    pub remote: String,
    pub branch: String,
    pub set_upstream: bool,
    /// The remote branch doesn't exist yet.
    pub new_branch: bool,
    /// "abc1234 Subject" lines, newest first.
    pub commits: Vec<String>,
    /// "2 files changed, 10 insertions(+), 3 deletions(-)", when known.
    pub stat: Option<String>,
    /// .env files in the project root that hold values.
    pub env_files: Vec<String>,
    /// The user's answer about checking those values, when given.
    pub read_env: Option<bool>,
    /// What the safety check found (empty until it ran).
    pub findings: Vec<push_check::Finding>,
}

impl PushPlan {
    pub fn nothing_to_push(&self) -> bool {
        self.commits.is_empty() && !self.new_branch
    }

    /// The user still has to say whether their .env values may be checked.
    pub fn needs_env_answer(&self) -> bool {
        !self.env_files.is_empty() && self.read_env.is_none()
    }

    /// The safety check found something that must not be uploaded.
    pub fn blocked(&self) -> bool {
        self.findings
            .iter()
            .any(|f| f.level == push_check::Level::Block)
    }

    /// The plan in words, for the approval prompt and the tool result.
    pub fn describe(&self) -> String {
        self.describe_for(true)
    }

    /// The plan as reported back to the agent: without the user-only notes,
    /// which aren't something for it to act on.
    pub fn describe_for_agent(&self) -> String {
        self.describe_for(false)
    }

    fn describe_for(&self, with_notes: bool) -> String {
        let target = format!("{}/{}", self.remote, self.branch);
        if self.nothing_to_push() {
            return format!(
                "Nothing to push: {} is up to date with {target}.",
                self.branch
            );
        }
        let n = self.commits.len();
        let mut text = format!(
            "Will push {n} commit{} from {} to {target}{}{}:",
            if n == 1 { "" } else { "s" },
            self.branch,
            if self.new_branch {
                " (a new branch on the remote)"
            } else {
                ""
            },
            if self.set_upstream {
                ", and track it"
            } else {
                ""
            },
        );
        const SHOWN: usize = 20;
        for commit in self.commits.iter().take(SHOWN) {
            text.push_str(&format!("\n  {commit}"));
        }
        if n > SHOWN {
            text.push_str(&format!("\n  … and {} more", n - SHOWN));
        }
        if let Some(stat) = &self.stat {
            text.push_str(&format!("\n{}", stat.trim()));
        }
        if self.needs_env_answer() {
            return text;
        }
        use push_check::Level;
        let blocks: Vec<_> = self
            .findings
            .iter()
            .filter(|f| f.level == Level::Block)
            .collect();
        let warns: Vec<_> = self
            .findings
            .iter()
            .filter(|f| f.level == Level::Warn || (with_notes && f.level == Level::Note))
            .collect();
        if !blocks.is_empty() {
            text.push_str("\n\nStopped by the safety check:");
            for f in &blocks {
                text.push_str(&format!("\n  ✗ {}", f.text));
            }
        }
        if !warns.is_empty() {
            text.push_str("\n\nWorth a look:");
            for f in &warns {
                text.push_str(&format!("\n  ! {}", f.text));
            }
        }
        if blocks.is_empty() && !self.findings.iter().any(|f| f.level == Level::Warn) {
            text.push_str("\n\nSafety check: no secrets, keys or oversized files found.");
        }
        match (self.env_files.is_empty(), self.read_env) {
            (true, _) => {}
            (false, Some(true)) => text.push_str(&format!(
                "\nChecked against the values in {} (with your permission).",
                self.env_files.join(", ")
            )),
            (false, _) => text.push_str(&format!(
                "\nNot checked against the values in {} (you said not to).",
                self.env_files.join(", ")
            )),
        }
        text
    }
}

/// Work out what `git_push` with `args` would send, from the local view of
/// the remote (as of the last fetch), and run the safety check on it once the
/// user has answered about their .env files.
pub async fn push_plan(
    args: &serde_json::Value,
    cwd: &std::path::Path,
) -> Result<PushPlan, String> {
    let branch = match args["branch"]
        .as_str()
        .map(str::trim)
        .filter(|b| !b.is_empty())
    {
        Some(b) => b.to_string(),
        None => git_output(&["rev-parse", "--abbrev-ref", "HEAD"], cwd)
            .await?
            .trim()
            .to_string(),
    };
    if branch == "HEAD" {
        return Err("Not on a branch (detached HEAD). Switch to a branch before pushing.".into());
    }
    let upstream_ref = format!("{branch}@{{u}}");
    let upstream = git_output(
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            &upstream_ref,
        ],
        cwd,
    )
    .await
    .ok()
    .map(|u| u.trim().to_string())
    .filter(|u| !u.is_empty());

    let remote = match args["remote"]
        .as_str()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        Some(r) => r.to_string(),
        None => upstream
            .as_deref()
            .and_then(|u| u.split_once('/'))
            .map(|(r, _)| r.to_string())
            .unwrap_or_else(|| "origin".to_string()),
    };
    let remotes = git_output(&["remote"], cwd).await?;
    if !remotes.lines().any(|r| r.trim() == remote) {
        return Err(if remotes.trim().is_empty() {
            "This repository has no remote to push to. Add one first, e.g. git with \
             [\"remote\", \"add\", \"origin\", \"<url>\"]."
                .to_string()
        } else {
            format!(
                "There's no remote called '{remote}'. Remotes: {}",
                remotes.split_whitespace().collect::<Vec<_>>().join(", ")
            )
        });
    }

    let tracking = format!("refs/remotes/{remote}/{branch}");
    let base = git_output(&["rev-parse", "--verify", "--quiet", &tracking], cwd)
        .await
        .ok()
        .map(|_| format!("{remote}/{branch}"));
    // The outgoing commits, as git log arguments.
    let range: Vec<String> = match &base {
        Some(base) => vec![format!("{base}..{branch}")],
        None => vec![
            branch.clone(),
            "--not".into(),
            format!("--remotes={remote}"),
        ],
    };
    let mut log_args = vec!["log", "--format=%h %s", "-n", "500"];
    log_args.extend(range.iter().map(String::as_str));
    let commits: Vec<String> = git_output(&log_args, cwd)
        .await?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    let stat = match &base {
        Some(base) if !commits.is_empty() => {
            let range = format!("{base}...{branch}");
            git_output(&["diff", "--shortstat", &range], cwd)
                .await
                .ok()
                .filter(|s| !s.trim().is_empty())
        }
        _ => None,
    };
    let set_upstream = args["set_upstream"].as_bool().unwrap_or(upstream.is_none());
    let mut plan = PushPlan {
        remote,
        branch,
        set_upstream,
        new_branch: base.is_none(),
        commits,
        stat,
        env_files: push_check::env_files(cwd),
        read_env: args["read_env"].as_bool(),
        findings: Vec::new(),
    };
    if !plan.nothing_to_push() && !plan.needs_env_answer() {
        plan.findings =
            push_check::scan(cwd, &range, &plan.branch, plan.read_env == Some(true)).await;
    }
    Ok(plan)
}

// ── git (everything else) ─────────────────────────────────────────────────

pub struct GitTool;

#[async_trait]
impl Tool for GitTool {
    fn name(&self) -> &str {
        "git"
    }

    fn description(&self) -> &str {
        "Run any other git command in the project: branches (branch, switch, checkout), pull, \
         fetch, merge, rebase, cherry-pick, stash, tag, remote, show, blame, reset, restore and \
         so on. Pass what comes after `git` as an array, e.g. [\"switch\", \"-c\", \"feature/login\"]. \
         Use git_status, git_diff, git_log and git_commit for those, and git_push to push. \
         Commands that can throw work away (reset --hard, clean, branch -D, stash drop, …) ask \
         the user first. Interactive modes (-i, -p) aren't available."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "The git arguments, starting with the subcommand, e.g. [\"pull\", \"--rebase\"]"
                }
            },
            "required": ["args"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let argv = match git_args(&args) {
            Ok(argv) => argv,
            Err(e) => return ToolResult::err(e),
        };
        if let Err(e) = check_git_args(&argv) {
            return ToolResult::err(e);
        }
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        match git_output(&argv, &ctx.cwd).await {
            Ok(out) if out.trim().is_empty() => ToolResult::ok("(no output)"),
            Ok(out) => ToolResult::ok(out),
            Err(e) => ToolResult::err(e),
        }
    }
}

/// The `args` array as strings.
fn git_args(args: &serde_json::Value) -> Result<Vec<String>, String> {
    let argv: Vec<String> = args["args"]
        .as_array()
        .ok_or("'args' must be an array of strings, e.g. [\"switch\", \"main\"]")?
        .iter()
        .filter_map(|a| a.as_str().map(str::to_string))
        .collect();
    if argv.is_empty() {
        return Err("Give the git subcommand, e.g. [\"branch\", \"-a\"]".into());
    }
    Ok(argv)
}

/// Refuse what the generic tool must not do: options before the subcommand
/// (`-C`, `-c`, `--git-dir` could point git outside the project), pushes and
/// commits (they have their own tools), and interactive modes.
fn check_git_args(argv: &[String]) -> Result<(), String> {
    let sub = argv[0].as_str();
    if sub.starts_with('-') {
        return Err(
            "Start with the git subcommand (options like -C or -c before it aren't allowed)."
                .into(),
        );
    }
    match sub {
        "push" => {
            return Err(
                "Use git_push to push: it shows the user what will be pushed first.".into(),
            );
        }
        "commit" => {
            return Err(
                "Use git_commit to commit. Amending or rewriting commits is for the user to do."
                    .into(),
            );
        }
        _ => {}
    }
    let interactive = argv[1..].iter().any(|a| {
        matches!(
            a.as_str(),
            "-i" | "--interactive" | "-p" | "--patch" | "--edit-todo"
        )
    });
    if interactive {
        return Err("Interactive modes (-i, -p) need a terminal and aren't available here.".into());
    }
    Ok(())
}

/// Whether a git tool call must be approved by the user even when that tool
/// isn't on the approval list: every push, and git commands that can throw
/// work away.
pub fn always_ask(tool: &str, args: &serde_json::Value) -> bool {
    match tool {
        "git_push" => true,
        "git" => git_args(args).is_ok_and(|argv| is_destructive(&argv)),
        _ => false,
    }
}

/// Git commands that can lose work or rewrite history.
fn is_destructive(argv: &[String]) -> bool {
    let rest: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
    let has = |flags: &[&str]| rest.iter().any(|a| flags.contains(a));
    match argv[0].as_str() {
        "reset" => has(&["--hard", "--merge", "--keep"]),
        "clean" => !has(&["-n", "--dry-run"]),
        "checkout" => has(&["--", ".", "-f", "--force", "-B", "--ours", "--theirs"]),
        "restore" => !has(&["--staged", "-S"]) || has(&["--worktree", "-W"]),
        "switch" => has(&["--discard-changes", "-f", "--force", "-C", "--force-create"]),
        "branch" => has(&["-D", "-d", "--delete", "-f", "--force", "-C", "-M"]),
        "stash" => has(&["drop", "clear"]),
        "tag" => has(&["-d", "--delete", "-f", "--force"]),
        "remote" => has(&["remove", "rm", "set-url", "prune"]),
        "worktree" => has(&["remove", "prune"]),
        "reflog" => has(&["expire", "delete"]),
        "config" => !has(&["--get", "--get-all", "--list", "-l", "--get-regexp"]),
        // Finishing or backing out of a rebase that's already underway is safe.
        "rebase" => !has(&["--continue", "--abort", "--quit"]),
        "filter-branch" | "filter-repo" | "update-ref" | "gc" | "prune" | "rm" | "replace"
        | "submodule" => true,
        _ => false,
    }
}

/// Run git in `cwd` without prompts, pagers or editors. Ok(stdout + stderr)
/// on success, Err(the same, with the command) on failure.
async fn git_output(args: &[&str], cwd: &std::path::Path) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("--no-pager")
        .args(["-c", "color.ui=false"])
        .args(args)
        .current_dir(cwd)
        // A credential or passphrase prompt would wait forever: fail instead.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_SEQUENCE_EDITOR", "true")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        cmd.creation_flags(0x08000000);
    }
    let output = cmd
        .output()
        .await
        .map_err(|e| format!("Failed to run git: {e}. Is git installed and in PATH?"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = match (stdout.trim().is_empty(), stderr.trim().is_empty()) {
        (_, true) => stdout.to_string(),
        (true, false) => stderr.to_string(),
        (false, false) => format!("{stdout}\n{stderr}"),
    };
    if output.status.success() {
        Ok(text)
    } else {
        Err(format!(
            "git {} failed (exit {}):\n{}",
            args.join(" "),
            output.status.code().unwrap_or(-1),
            text.trim()
        ))
    }
}

// ── Helper ────────────────────────────────────────────────────────────────

async fn run_git_command(args: &[&str], ctx: &ToolContext) -> ToolResult {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args).current_dir(&ctx.cwd).kill_on_drop(true);

    #[cfg(windows)]
    {
        cmd.creation_flags(0x08000000);
    }

    let output = cmd.output().await;

    match output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();

            if output.status.success() {
                if stdout.trim().is_empty() && stderr.trim().is_empty() {
                    ToolResult::ok("(no output)")
                } else {
                    let combined = if stderr.is_empty() {
                        stdout
                    } else {
                        format!("{}\n{}", stdout, stderr)
                    };
                    ToolResult::ok(combined)
                }
            } else {
                ToolResult::err(format!(
                    "git {} failed (exit {}):\n{}{}",
                    args.join(" "),
                    output.status.code().unwrap_or(-1),
                    stdout,
                    stderr
                ))
            }
        }
        Err(e) => ToolResult::err(format!(
            "Failed to run git: {}. Is git installed and in PATH?",
            e
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CO_AUTHOR_TRAILER;

    #[test]
    fn co_author_line_is_added_once_and_only_when_on() {
        let credited = with_co_author("Fix the login form\n", true);
        assert_eq!(
            credited,
            format!("Fix the login form\n\n{CO_AUTHOR_TRAILER}")
        );
        // A message that already has it is left alone.
        assert_eq!(with_co_author(&credited, true), credited);
        // Turned off: the message is untouched.
        assert_eq!(
            with_co_author("Fix the login form", false),
            "Fix the login form"
        );
    }

    #[test]
    fn the_co_author_address_is_a_github_noreply_address() {
        let email = CO_AUTHOR_TRAILER
            .split('<')
            .nth(1)
            .and_then(|rest| rest.strip_suffix('>'))
            .expect("trailer has an <email>");
        assert!(email.ends_with("@users.noreply.github.com"), "{email}");
    }

    /// Runs `git_commit` in a throwaway repo and returns the commit message.
    async fn commit_with(credit: bool) -> Option<String> {
        let root = std::env::temp_dir().join(format!("nvx-git-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .ok()
                .filter(|o| o.status.success())
        };
        // No git on this machine: nothing to check.
        git(&["init", "-q"])?;
        // The throwaway repo needs an identity of its own to commit.
        git(&["config", "user.name", "Test"])?;
        git(&["config", "user.email", "test@example.invalid"])?;
        git(&["config", "commit.gpgsign", "false"])?;
        std::fs::write(root.join("a.txt"), "a").unwrap();

        let settings = crate::config::Settings {
            project_root: root.clone(),
            co_author_commits: credit,
            ..Default::default()
        };
        let ctx = ToolContext {
            settings: std::sync::Arc::new(settings),
            cwd: root.clone(),
            client: None,
            temp_memory: std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::tools::memory::TempMemory::new(),
            )),
            events: None,
            approvals_off: false,
            agent_name: None,
            inbox: None,
        };
        let result = GitCommitTool
            .execute(json!({ "message": "Add a" }), &ctx)
            .await;
        assert!(result.success, "{}", result.to_message_content());
        let log = git(&["log", "-1", "--format=%B"])?;
        std::fs::remove_dir_all(&root).ok();
        Some(String::from_utf8_lossy(&log.stdout).trim().to_string())
    }

    #[tokio::test]
    async fn git_commit_credits_norvexum_only_when_turned_on() {
        let Some(on) = commit_with(true).await else {
            return;
        };
        assert!(on.starts_with("Add a"), "{on}");
        assert!(on.ends_with(CO_AUTHOR_TRAILER), "{on}");
        let off = commit_with(false).await.unwrap();
        assert_eq!(off, "Add a");
    }
}

#[cfg(test)]
mod push_tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn work_losing_git_commands_ask_first() {
        for risky in [
            &["reset", "--hard", "HEAD~1"][..],
            &["clean", "-fd"],
            &["checkout", "--", "src/main.rs"],
            &["restore", "src/main.rs"],
            &["branch", "-D", "old"],
            &["stash", "drop"],
            &["rebase", "main"],
            &["config", "user.name", "x"],
        ] {
            assert!(is_destructive(&argv(risky)), "{risky:?}");
        }
        for safe in [
            &["status"][..],
            &["switch", "-c", "feature"],
            &["branch", "-a"],
            &["pull", "--ff-only"],
            &["fetch", "origin"],
            &["stash"],
            &["restore", "--staged", "a.rs"],
            &["clean", "-n"],
            &["config", "--get", "user.name"],
        ] {
            assert!(!is_destructive(&argv(safe)), "{safe:?}");
        }
        assert!(always_ask("git_push", &json!({})));
        assert!(always_ask("git", &json!({ "args": ["reset", "--hard"] })));
        assert!(!always_ask("git", &json!({ "args": ["log"] })));
    }

    #[test]
    fn the_generic_git_tool_refuses_push_commit_and_global_options() {
        assert!(check_git_args(&argv(&["push", "--force"])).is_err());
        assert!(check_git_args(&argv(&["commit", "--amend"])).is_err());
        assert!(check_git_args(&argv(&["-C", "..", "status"])).is_err());
        assert!(check_git_args(&argv(&["rebase", "-i", "main"])).is_err());
        assert!(check_git_args(&argv(&["switch", "main"])).is_ok());
    }

    fn run(dir: &std::path::Path, args: &[&str]) -> Option<String> {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
    }

    #[tokio::test]
    async fn git_push_shows_what_it_sends_and_then_sends_it() {
        let root = std::env::temp_dir().join(format!("nvx-push-{}", uuid::Uuid::new_v4()));
        let (remote, work) = (root.join("remote.git"), root.join("work"));
        std::fs::create_dir_all(&remote).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        // No git on this machine: nothing to check.
        if run(&remote, &["init", "-q", "--bare"]).is_none() {
            return;
        }
        run(&work, &["init", "-q"]).unwrap();
        // The throwaway repo needs an identity of its own to commit.
        run(&work, &["config", "user.name", "Test"]).unwrap();
        run(&work, &["config", "user.email", "test@example.invalid"]).unwrap();
        run(&work, &["config", "commit.gpgsign", "false"]).unwrap();
        run(
            &work,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        )
        .unwrap();
        std::fs::write(work.join("a.txt"), "a").unwrap();
        run(&work, &["add", "-A"]).unwrap();
        run(&work, &["commit", "-q", "-m", "Add a"]).unwrap();

        let plan = push_plan(&json!({}), &work).await.unwrap();
        assert!(plan.new_branch && plan.set_upstream);
        assert_eq!(plan.commits.len(), 1);
        assert!(plan.describe().contains("Add a"), "{}", plan.describe());

        let settings = crate::config::Settings {
            project_root: work.clone(),
            ..Default::default()
        };
        let ctx = ToolContext {
            settings: std::sync::Arc::new(settings),
            cwd: work.clone(),
            client: None,
            temp_memory: std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::tools::memory::TempMemory::new(),
            )),
            events: None,
            approvals_off: false,
            agent_name: None,
            inbox: None,
        };
        let pushed = GitPushTool.execute(json!({}), &ctx).await;
        assert!(pushed.success, "{}", pushed.to_message_content());
        assert!(
            pushed.output.starts_with("Pushed 1 commit"),
            "{}",
            pushed.output
        );
        assert!(pushed.output.contains("Add a"), "{}", pushed.output);
        // The email note is for the user (shown before the push), not the agent.
        assert!(
            plan.describe().contains("personal email"),
            "{}",
            plan.describe()
        );
        assert!(
            !pushed.output.contains("personal email"),
            "{}",
            pushed.output
        );

        // Pushed and tracked: nothing left to send.
        let again = GitPushTool.execute(json!({}), &ctx).await;
        assert!(again.success);
        assert!(
            again.output.starts_with("Nothing to push"),
            "{}",
            again.output
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// A work repo with a bare "origin" next to it. None when git is missing.
    fn repo_pair() -> Option<(std::path::PathBuf, std::path::PathBuf, ToolContext)> {
        let root = std::env::temp_dir().join(format!("nvx-pair-{}", uuid::Uuid::new_v4()));
        let (remote, work) = (root.join("remote.git"), root.join("work"));
        std::fs::create_dir_all(&remote).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        run(&remote, &["init", "-q", "--bare"])?;
        run(&work, &["init", "-q"])?;
        // The throwaway repo needs an identity of its own to commit.
        run(&work, &["config", "user.name", "Test"])?;
        run(&work, &["config", "user.email", "test@example.invalid"])?;
        run(&work, &["config", "commit.gpgsign", "false"])?;
        run(
            &work,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        )?;
        let settings = crate::config::Settings {
            project_root: work.clone(),
            ..Default::default()
        };
        let ctx = ToolContext {
            settings: std::sync::Arc::new(settings),
            cwd: work.clone(),
            client: None,
            temp_memory: std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::tools::memory::TempMemory::new(),
            )),
            events: None,
            approvals_off: false,
            agent_name: None,
            inbox: None,
        };
        Some((root, work, ctx))
    }

    fn commit_file(work: &std::path::Path, name: &str, text: &str) {
        std::fs::write(work.join(name), text).unwrap();
        run(work, &["add", "-A"]).unwrap();
        run(work, &["commit", "-q", "-m", &format!("Add {name}")]).unwrap();
    }

    #[tokio::test]
    async fn a_push_with_env_files_asks_first_and_never_uploads_their_values() {
        let Some((root, work, ctx)) = repo_pair() else {
            return;
        };
        std::fs::write(work.join(".gitignore"), ".env\n").unwrap();
        // Built at run time so this file holds nothing that looks like a key.
        let value = format!("{}{}", "q8Lw", "x2Pz9vK4");
        std::fs::write(work.join(".env"), format!("DB_PASS={value}\n")).unwrap();
        commit_file(&work, "app.py", &format!("DB = \"{value}\"\n"));

        // No answer about .env yet: nothing is pushed, the agent is told to ask.
        let first = GitPushTool.execute(json!({}), &ctx).await;
        assert!(!first.success);
        let said = first.to_message_content();
        assert!(said.contains("ask_user") && said.contains(".env"), "{said}");

        // Allowed to check: the value is found and the push is stopped.
        let checked = GitPushTool.execute(json!({ "read_env": true }), &ctx).await;
        assert!(!checked.success);
        let said = checked.to_message_content();
        assert!(
            said.contains("app.py:1") && said.contains("DB_PASS"),
            "{said}"
        );
        assert!(
            !said.contains(&value),
            "the value itself must never be shown"
        );
        let pushed = run(&work, &["ls-remote", "origin"]).unwrap_or_default();
        assert!(
            pushed.trim().is_empty(),
            "nothing may reach the remote: {pushed}"
        );

        // Told not to read it: the push goes ahead, saying it wasn't checked.
        let unchecked = GitPushTool
            .execute(json!({ "read_env": false }), &ctx)
            .await;
        assert!(unchecked.success, "{}", unchecked.to_message_content());
        assert!(
            unchecked.output.contains("Not checked against"),
            "{}",
            unchecked.output
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn committed_secret_files_stop_the_push() {
        let Some((root, work, ctx)) = repo_pair() else {
            return;
        };
        commit_file(&work, "server.pem", "not really a key\n");
        let result = GitPushTool.execute(json!({}), &ctx).await;
        assert!(!result.success);
        let said = result.to_message_content();
        assert!(
            said.contains("server.pem") && said.contains("Nothing was pushed"),
            "{said}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_rejected_push_explains_how_to_pull_and_resolve() {
        let Some((root, work, ctx)) = repo_pair() else {
            return;
        };
        commit_file(&work, "a.txt", "one\n");
        assert!(GitPushTool.execute(json!({}), &ctx).await.success);
        // Someone else pushes first, from another clone.
        let other = root.join("other");
        let remote = root.join("remote.git");
        run(
            &root,
            &[
                "clone",
                "-q",
                remote.to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        )
        .unwrap();
        run(&other, &["config", "user.name", "Other"]).unwrap();
        run(&other, &["config", "user.email", "other@example.invalid"]).unwrap();
        run(&other, &["config", "commit.gpgsign", "false"]).unwrap();
        commit_file(&other, "b.txt", "theirs\n");
        run(&other, &["push", "-q"]).unwrap();

        commit_file(&work, "c.txt", "mine\n");
        // Our view of the remote is stale, so the check passes and git refuses.
        let result = GitPushTool.execute(json!({}), &ctx).await;
        assert!(!result.success);
        let said = result.to_message_content();
        assert!(
            said.contains("--no-rebase") && said.contains("merge"),
            "{said}"
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
