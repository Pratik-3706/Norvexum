use async_trait::async_trait;
use serde_json::json;

use crate::tools::{Tool, ToolContext, ToolResult};

pub struct ManageTasksTool;

#[async_trait]
impl Tool for ManageTasksTool {
    fn name(&self) -> &str {
        "manage_tasks"
    }

    fn description(&self) -> &str {
        "Manage the project's task.md file. Allows adding, removing, or updating the status of tasks. Tasks must be an artifact named task.md."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["view", "add", "remove", "update_status"],
                    "description": "The action to perform."
                },
                "task_text": {
                    "type": "string",
                    "description": "The exact text of the task (without the `[ ]` prefix) for add, remove, or update_status."
                },
                "new_status": {
                    "type": "string",
                    "enum": ["todo", "in_progress", "done", "skipped"],
                    "description": "The new status for update_status. Maps to [ ], [/], [x], or [-]."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let action = match args.get("action").and_then(|v| v.as_str()) {
            Some(a) => a,
            None => return ToolResult::err("Missing 'action' argument"),
        };

        // Find task.md in the workspace
        let mut task_path = None;
        for entry in walkdir::WalkDir::new(&ctx.cwd)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.file_name() == "task.md" {
                task_path = Some(entry.path().to_path_buf());
                break;
            }
        }

        let path = match task_path {
            Some(p) => p,
            None => {
                if action == "add" {
                    // Create it in the root if it doesn't exist
                    ctx.cwd.join("task.md")
                } else {
                    return ToolResult::err(
                        "No task.md found in the workspace. Create one first or use 'add' to create it.",
                    );
                }
            }
        };

        let mut content =
            std::fs::read_to_string(&path).unwrap_or_else(|_| "# Tasks\n\n".to_string());

        if action == "view" {
            return ToolResult::ok(content);
        }

        let task_text = match args.get("task_text").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => {
                return ToolResult::err("Missing 'task_text' argument required for this action.");
            }
        };

        match action {
            "add" => {
                let new_line = format!("- `[ ]` {}\n", task_text);
                content.push_str(&new_line);
            }
            "remove" => {
                let mut new_content = String::new();
                let mut found = false;
                for line in content.lines() {
                    if line.contains(task_text) && line.trim().starts_with("- `[") {
                        found = true;
                        continue;
                    }
                    new_content.push_str(line);
                    new_content.push('\n');
                }
                if !found {
                    return ToolResult::err(format!("Task '{}' not found.", task_text));
                }
                content = new_content;
            }
            "update_status" => {
                let new_status = match args.get("new_status").and_then(|v| v.as_str()) {
                    Some("todo") => "[ ]",
                    Some("in_progress") => "[/]",
                    Some("done") => "[x]",
                    Some("skipped") => "[-]",
                    _ => return ToolResult::err("Invalid or missing 'new_status' argument."),
                };

                let mut new_content = String::new();
                let mut found = false;
                for line in content.lines() {
                    if line.contains(task_text) && line.trim().starts_with("- `[") {
                        // Replace the status box
                        if let Some(start) = line.find("- `[") {
                            if let Some(end) = line[start..].find("]`") {
                                let prefix = &line[..start];
                                let suffix = &line[start + end + 2..];
                                let new_line = format!("{}- `{}`{}", prefix, new_status, suffix);
                                new_content.push_str(&new_line);
                                new_content.push('\n');
                                found = true;
                                continue;
                            }
                        }
                    }
                    new_content.push_str(line);
                    new_content.push('\n');
                }
                if !found {
                    return ToolResult::err(format!("Task '{}' not found.", task_text));
                }
                content = new_content;
            }
            _ => return ToolResult::err("Unknown action"),
        }

        if let Err(e) = std::fs::write(&path, content) {
            return ToolResult::err(format!("Failed to write task.md: {}", e));
        }

        ToolResult::ok(format!("Successfully performed '{}' on task.md", action))
    }
}
