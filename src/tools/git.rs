// ═══════════════════════════════════════════════════════════════════════════
// Git Tools — Version control awareness
//
// Provides git status, diff, commit, and log as agent tools.
// All operations scoped to the project root.
// ═══════════════════════════════════════════════════════════════════════════

use async_trait::async_trait;
use serde_json::json;

use super::{Tool, ToolContext, ToolResult};

// ── git_status ────────────────────────────────────────────────────────────

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

// ── git_diff ──────────────────────────────────────────────────────────────

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
         Equivalent to `git add -A && git commit -m '<message>'`."
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
            let add_result = run_git_command(&["add", "-A"], ctx).await;
            if !add_result.success {
                return add_result;
            }
        }

        run_git_command(&["commit", "-m", message], ctx).await
    }
}

// ── git_log ───────────────────────────────────────────────────────────────

pub struct GitLogTool;

#[async_trait]
impl Tool for GitLogTool {
    fn name(&self) -> &str {
        "git_log"
    }

    fn description(&self) -> &str {
        "Show recent git commit history. Returns a compact log of recent commits."
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

// ── Helper ────────────────────────────────────────────────────────────────

async fn run_git_command(args: &[&str], ctx: &ToolContext) -> ToolResult {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(&ctx.cwd)
        .kill_on_drop(true)
        .output()
        .await;

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
