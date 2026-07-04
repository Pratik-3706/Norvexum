// ═══════════════════════════════════════════════════════════════════════════
// Filesystem Tools — Sandboxed file operations
//
// All operations are restricted to the project root directory.
// Attempting to access files outside the sandbox returns an error.
// ═══════════════════════════════════════════════════════════════════════════

use async_trait::async_trait;
use serde_json::json;
use std::path::{Path, PathBuf};

use super::{Tool, ToolContext, ToolResult};

/// Resolve a path relative to CWD, enforcing sandbox.
pub(super) fn resolve_path(path_str: &str, ctx: &ToolContext) -> Result<PathBuf, String> {
    if path_str.trim().is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    let path = if Path::new(path_str).is_absolute() {
        PathBuf::from(path_str)
    } else {
        ctx.cwd.join(path_str)
    };

    let root = ctx
        .settings
        .project_root
        .canonicalize()
        .unwrap_or_else(|_| ctx.settings.project_root.clone());

    // Resolve the nearest existing ancestor. This permits creating nested
    // directories while still catching symlinks that escape the sandbox.
    let resolved = if path.exists() {
        path.canonicalize()
            .map_err(|e| format!("Cannot resolve path: {}", e))?
    } else {
        let mut ancestor = path.as_path();
        let mut missing = Vec::new();
        while !ancestor.exists() {
            let name = ancestor
                .file_name()
                .ok_or_else(|| format!("Cannot resolve path: {}", path.display()))?;
            missing.push(name.to_os_string());
            ancestor = ancestor
                .parent()
                .ok_or_else(|| format!("Cannot resolve path: {}", path.display()))?;
        }
        let mut rebuilt = ancestor
            .canonicalize()
            .map_err(|e| format!("Cannot resolve parent: {}", e))?;
        for component in missing.into_iter().rev() {
            rebuilt.push(component);
        }
        rebuilt
    };

    if !resolved.starts_with(&root) {
        return Err(format!(
            "Access denied: path '{}' is outside the project sandbox ({})",
            resolved.display(),
            root.display()
        ));
    }

    Ok(resolved)
}

// ── read_file ─────────────────────────────────────────────────────────────

pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Returns the file content with line numbers."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path (relative to CWD or absolute within project)" },
                "start_line": { "type": "integer", "description": "Start line (1-indexed, optional)" },
                "end_line": { "type": "integer", "description": "End line (1-indexed, inclusive, optional)" }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let path_str = args["path"].as_str().unwrap_or("");
        let start_line = args["start_line"].as_u64().map(|n| n as usize);
        let end_line = args["end_line"].as_u64().map(|n| n as usize);

        let path = match resolve_path(path_str, ctx) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };

        if !path.exists() {
            return ToolResult::err(format!("File not found: {}", path.display()));
        }

        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let lines: Vec<&str> = content.lines().collect();
                let total = lines.len();
                let start = start_line.unwrap_or(1).max(1);
                let end = end_line.unwrap_or(total).min(total);

                let numbered: Vec<String> = lines[start.saturating_sub(1)..end]
                    .iter()
                    .enumerate()
                    .map(|(i, line)| format!("{:>4}: {}", start + i, line))
                    .collect();

                let output = format!(
                    "File: {} ({} lines total, showing {}-{})\n{}",
                    path.display(),
                    total,
                    start,
                    end,
                    numbered.join("\n")
                );

                ToolResult::ok_with_data(
                    output,
                    json!({
                        "path": path.to_string_lossy(),
                        "total_lines": total,
                        "start": start,
                        "end": end,
                    }),
                )
            }
            Err(e) => ToolResult::err(format!("Failed to read file: {}", e)),
        }
    }
}

// ── write_file ────────────────────────────────────────────────────────────

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Create or overwrite a file with the given content. Creates parent directories if needed."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path" },
                "content": { "type": "string", "description": "Content to write" }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let path_str = args["path"].as_str().unwrap_or("");
        let content = args["content"].as_str().unwrap_or("");

        let path = match resolve_path(path_str, ctx) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };

        // Create parent directories
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return ToolResult::err(format!("Failed to create directories: {}", e));
            }
        }

        match std::fs::write(&path, content) {
            Ok(_) => {
                let lines = content.lines().count();
                let bytes = content.len();
                ToolResult::ok_with_data(
                    format!(
                        "✅ Written {} lines ({} bytes) to {}",
                        lines,
                        bytes,
                        path.display()
                    ),
                    json!({ "path": path.to_string_lossy(), "lines": lines, "bytes": bytes }),
                )
            }
            Err(e) => ToolResult::err(format!("Failed to write file: {}", e)),
        }
    }
}

// ── edit_file ─────────────────────────────────────────────────────────────

pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing specific text. Finds the exact target string and replaces it. \
         Shows a diff preview of changes made."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path" },
                "target": { "type": "string", "description": "Exact text to find and replace" },
                "replacement": { "type": "string", "description": "Replacement text" },
                "all": { "type": "boolean", "description": "Replace all occurrences (default: false, replace first only)" }
            },
            "required": ["path", "target", "replacement"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let path_str = args["path"].as_str().unwrap_or("");
        let target = args["target"].as_str().unwrap_or("");
        let replacement = args["replacement"].as_str().unwrap_or("");
        let replace_all = args["all"].as_bool().unwrap_or(false);

        if target.is_empty() {
            return ToolResult::err("Target string cannot be empty");
        }

        let path = match resolve_path(path_str, ctx) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };

        let original = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return ToolResult::err(format!("Failed to read file: {}", e)),
        };

        if !original.contains(target) {
            return ToolResult::err(format!(
                "Target text not found in {}.\nSearched for:\n{}",
                path.display(),
                target
            ));
        }

        let new_content = if replace_all {
            original.replace(target, replacement)
        } else {
            original.replacen(target, replacement, 1)
        };

        let count = if replace_all {
            original.matches(target).count()
        } else {
            1
        };

        // Generate diff
        let diff = similar::TextDiff::from_lines(&original, &new_content);
        let mut diff_output = String::new();
        for change in diff.iter_all_changes() {
            let sign = match change.tag() {
                similar::ChangeTag::Delete => "-",
                similar::ChangeTag::Insert => "+",
                similar::ChangeTag::Equal => " ",
            };
            // Only show changed lines and a bit of context
            if change.tag() != similar::ChangeTag::Equal {
                diff_output.push_str(&format!("{}{}", sign, change));
            }
        }

        match std::fs::write(&path, &new_content) {
            Ok(_) => ToolResult::ok_with_data(
                format!(
                    "✅ Edited {} ({} replacement{})\n\nDiff:\n```diff\n{}\n```",
                    path.display(),
                    count,
                    if count > 1 { "s" } else { "" },
                    diff_output.trim()
                ),
                json!({ "path": path.to_string_lossy(), "replacements": count }),
            ),
            Err(e) => ToolResult::err(format!("Failed to write file: {}", e)),
        }
    }
}

// ── ls (list directory) ───────────────────────────────────────────────────

pub struct ListDirTool;

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "ls"
    }

    fn description(&self) -> &str {
        "List files and directories. Shows name, type, size, and modification time."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory path (default: current directory)" },
                "recursive": { "type": "boolean", "description": "List recursively (default: false)" },
                "max_depth": { "type": "integer", "description": "Max recursion depth (default: 3)" }
            },
            "required": []
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let path_str = args["path"].as_str().unwrap_or(".");
        let recursive = args["recursive"].as_bool().unwrap_or(false);
        let max_depth = args["max_depth"].as_u64().unwrap_or(3) as usize;

        let path = match resolve_path(path_str, ctx) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };

        if !path.is_dir() {
            return ToolResult::err(format!("Not a directory: {}", path.display()));
        }

        let mut entries = Vec::new();

        if recursive {
            let walker = walkdir::WalkDir::new(&path)
                .max_depth(max_depth)
                .sort_by_file_name();
            for entry in walker.into_iter().filter_map(|e| e.ok()) {
                if entry.path() == path {
                    continue;
                }
                let rel = entry.path().strip_prefix(&path).unwrap_or(entry.path());
                let meta = entry.metadata().ok();
                let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                let kind = if entry.file_type().is_dir() {
                    "dir"
                } else {
                    "file"
                };
                entries.push(format!(
                    "{:<6} {:>10}  {}",
                    kind,
                    format_size(size),
                    rel.display()
                ));
            }
        } else {
            let mut dir_entries = match std::fs::read_dir(&path) {
                Ok(read_dir) => read_dir.filter_map(|e| e.ok()).collect::<Vec<_>>(),
                Err(e) => return ToolResult::err(format!("Cannot read directory: {}", e)),
            };
            dir_entries.sort_by_key(|e| e.file_name());

            for entry in &dir_entries {
                let meta = entry.metadata().ok();
                let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                let kind = if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    "dir"
                } else {
                    "file"
                };
                entries.push(format!(
                    "{:<6} {:>10}  {}",
                    kind,
                    format_size(size),
                    entry.file_name().to_string_lossy()
                ));
            }
        }

        if entries.is_empty() {
            return ToolResult::ok(format!("Directory is empty: {}", path.display()));
        }

        ToolResult::ok_with_data(
            format!("{}\n\n{} entries", entries.join("\n"), entries.len()),
            json!({ "path": path.to_string_lossy(), "count": entries.len() }),
        )
    }
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{} B", bytes);
    }
    if bytes < 1024 * 1024 {
        return format!("{:.1} KB", bytes as f64 / 1024.0);
    }
    if bytes < 1024 * 1024 * 1024 {
        return format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0));
    }
    format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

// ── cd (change directory) ─────────────────────────────────────────────────

// ── cat (view file) ───────────────────────────────────────────────────────

pub struct CatTool;

#[async_trait]
impl Tool for CatTool {
    fn name(&self) -> &str {
        "cat"
    }
    fn description(&self) -> &str {
        "View full file content (alias for read_file)."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path" }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        ReadFileTool.execute(args, ctx).await
    }
}

// ── grep ──────────────────────────────────────────────────────────────────

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search for a pattern in files. Supports regex. Returns matching lines with file and line number."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Search pattern (regex)" },
                "path": { "type": "string", "description": "File or directory to search (default: current dir)" },
                "case_insensitive": { "type": "boolean", "description": "Case-insensitive search (default: false)" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let pattern_str = args["pattern"].as_str().unwrap_or("");
        let path_str = args["path"].as_str().unwrap_or(".");
        let case_insensitive = args["case_insensitive"].as_bool().unwrap_or(false);

        let path = match resolve_path(path_str, ctx) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };

        let regex = match regex::RegexBuilder::new(pattern_str)
            .case_insensitive(case_insensitive)
            .build()
        {
            Ok(r) => r,
            Err(e) => return ToolResult::err(format!("Invalid regex: {}", e)),
        };

        let mut results = Vec::new();
        let max_results = 100;

        let files: Vec<PathBuf> = if path.is_file() {
            vec![path.clone()]
        } else {
            walkdir::WalkDir::new(&path)
                .max_depth(5)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
                .filter(|e| {
                    // Skip binary/large files
                    let ext = e.path().extension().and_then(|s| s.to_str()).unwrap_or("");
                    !matches!(
                        ext,
                        "png"
                            | "jpg"
                            | "jpeg"
                            | "gif"
                            | "webp"
                            | "ico"
                            | "exe"
                            | "dll"
                            | "so"
                            | "bin"
                            | "wasm"
                            | "lock"
                    )
                })
                .map(|e| e.into_path())
                .collect()
        };

        'outer: for file in &files {
            if let Ok(content) = std::fs::read_to_string(file) {
                let rel = file
                    .strip_prefix(&ctx.settings.project_root)
                    .unwrap_or(file);
                for (i, line) in content.lines().enumerate() {
                    if regex.is_match(line) {
                        results.push(format!("{}:{}: {}", rel.display(), i + 1, line.trim()));
                        if results.len() >= max_results {
                            results.push(format!("... (truncated at {} results)", max_results));
                            break 'outer;
                        }
                    }
                }
            }
        }

        if results.is_empty() {
            ToolResult::ok(format!("No matches found for pattern: {}", pattern_str))
        } else {
            ToolResult::ok_with_data(
                results.join("\n"),
                json!({ "matches": results.len(), "pattern": pattern_str }),
            )
        }
    }
}

// ── find ──────────────────────────────────────────────────────────────────

pub struct FindTool;

#[async_trait]
impl Tool for FindTool {
    fn name(&self) -> &str {
        "find"
    }
    fn description(&self) -> &str {
        "Find files by name pattern (glob)."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern (e.g. '*.rs', 'test_*')" },
                "path": { "type": "string", "description": "Directory to search (default: current dir)" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let pattern = args["pattern"].as_str().unwrap_or("*");
        let path_str = args["path"].as_str().unwrap_or(".");

        let path = match resolve_path(path_str, ctx) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };

        let glob = match globset::GlobBuilder::new(pattern)
            .case_insensitive(true)
            .build()
        {
            Ok(g) => g.compile_matcher(),
            Err(e) => return ToolResult::err(format!("Invalid glob pattern: {}", e)),
        };

        let mut results = Vec::new();
        for entry in walkdir::WalkDir::new(&path)
            .max_depth(10)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let name = entry.file_name().to_string_lossy();
            if glob.is_match(name.as_ref()) {
                let rel = entry
                    .path()
                    .strip_prefix(&ctx.settings.project_root)
                    .unwrap_or(entry.path());
                results.push(rel.display().to_string());
            }
            if results.len() >= 200 {
                break;
            }
        }

        if results.is_empty() {
            ToolResult::ok(format!("No files found matching: {}", pattern))
        } else {
            ToolResult::ok_with_data(results.join("\n"), json!({ "count": results.len() }))
        }
    }
}

// ── touch ─────────────────────────────────────────────────────────────────

pub struct TouchTool;

#[async_trait]
impl Tool for TouchTool {
    fn name(&self) -> &str {
        "touch"
    }
    fn description(&self) -> &str {
        "Create an empty file (and parent directories if needed)."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string", "description": "File path" } },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let path_str = args["path"].as_str().unwrap_or("");
        let path = match resolve_path(path_str, ctx) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };

        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(_) => ToolResult::ok(format!("✅ Created: {}", path.display())),
            Err(e) => ToolResult::err(format!("Failed to create file: {}", e)),
        }
    }
}

// ── rm ────────────────────────────────────────────────────────────────────

pub struct RemoveTool;

#[async_trait]
impl Tool for RemoveTool {
    fn name(&self) -> &str {
        "rm"
    }
    fn description(&self) -> &str {
        "Remove a file or empty directory."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File or directory to remove" },
                "recursive": { "type": "boolean", "description": "Remove directories recursively (default: false)" }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let path_str = args["path"].as_str().unwrap_or("");
        let recursive = args["recursive"].as_bool().unwrap_or(false);

        let path = match resolve_path(path_str, ctx) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };

        // Never allow deleting .norvexum config
        if path.ends_with(".norvexum") || path.to_string_lossy().contains(".norvexum") {
            return ToolResult::err("Cannot delete .norvexum configuration directory");
        }

        let result = if path.is_dir() {
            if recursive {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_dir(&path)
            }
        } else {
            std::fs::remove_file(&path)
        };

        match result {
            Ok(_) => ToolResult::ok(format!("✅ Removed: {}", path.display())),
            Err(e) => ToolResult::err(format!("Failed to remove: {}", e)),
        }
    }
}

// ── mv ────────────────────────────────────────────────────────────────────

pub struct MoveTool;

#[async_trait]
impl Tool for MoveTool {
    fn name(&self) -> &str {
        "mv"
    }
    fn description(&self) -> &str {
        "Move or rename a file or directory."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "source": { "type": "string", "description": "Source path" },
                "destination": { "type": "string", "description": "Destination path" }
            },
            "required": ["source", "destination"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let src_str = args["source"].as_str().unwrap_or("");
        let dst_str = args["destination"].as_str().unwrap_or("");

        let src = match resolve_path(src_str, ctx) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        let dst = match resolve_path(dst_str, ctx) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };

        match std::fs::rename(&src, &dst) {
            Ok(_) => ToolResult::ok(format!("✅ Moved: {} → {}", src.display(), dst.display())),
            Err(e) => ToolResult::err(format!("Failed to move: {}", e)),
        }
    }
}

// ── cp ────────────────────────────────────────────────────────────────────

pub struct CopyTool;

#[async_trait]
impl Tool for CopyTool {
    fn name(&self) -> &str {
        "cp"
    }
    fn description(&self) -> &str {
        "Copy a file."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "source": { "type": "string", "description": "Source file path" },
                "destination": { "type": "string", "description": "Destination file path" }
            },
            "required": ["source", "destination"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let src_str = args["source"].as_str().unwrap_or("");
        let dst_str = args["destination"].as_str().unwrap_or("");

        let src = match resolve_path(src_str, ctx) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        let dst = match resolve_path(dst_str, ctx) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };

        match std::fs::copy(&src, &dst) {
            Ok(bytes) => ToolResult::ok(format!(
                "✅ Copied: {} → {} ({} bytes)",
                src.display(),
                dst.display(),
                bytes
            )),
            Err(e) => ToolResult::err(format!("Failed to copy: {}", e)),
        }
    }
}

// ── pwd ───────────────────────────────────────────────────────────────────

pub struct PwdTool;

#[async_trait]
impl Tool for PwdTool {
    fn name(&self) -> &str {
        "pwd"
    }
    fn description(&self) -> &str {
        "Print the current working directory."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({ "type": "object", "properties": {}, "required": [] })
    }

    async fn execute(&self, _args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        ToolResult::ok(format!("{}", ctx.cwd.display()))
    }
}
