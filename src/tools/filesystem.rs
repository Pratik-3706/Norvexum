// ==========================================================================
// Filesystem Tools — Sandboxed file operations
//
// All operations are restricted to the project root directory.
// Attempting to access files outside the sandbox returns an error.
// ============================================================================

use async_trait::async_trait;
use serde_json::json;
use std::path::{Path, PathBuf};

use super::{Tool, ToolContext, ToolResult};

/// Directories that are almost never what a search is looking for and can be
/// enormous (build output, dependencies, VCS data).
const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".norvexum",
    ".venv",
    "venv",
    "__pycache__",
    "dist",
    "build",
    ".next",
    ".cache",
];

/// Files larger than this are skipped by `grep`.
const GREP_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Resolve a path relative to CWD, enforcing sandbox.
pub(super) fn resolve_path(path_str: &str, ctx: &ToolContext) -> Result<PathBuf, String> {
    let path_str = path_str.trim();
    if path_str.is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    let path = if path_str == "~" {
        dirs::home_dir().ok_or_else(|| "Could not resolve home directory".to_string())?
    } else if let Some(rest) = path_str
        .strip_prefix("~/")
        .or_else(|| path_str.strip_prefix("~\\"))
    {
        dirs::home_dir()
            .map(|h| h.join(rest))
            .ok_or_else(|| "Could not resolve home directory".to_string())?
    } else {
        // Absolute paths replace the cwd; on Windows "\x" resolves to the drive
        // root, which is outside the sandbox and gets rejected below.
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

/// Project-relative display path (shorter for the model and the user).
fn display_path(path: &Path, ctx: &ToolContext) -> String {
    let root = ctx
        .settings
        .project_root
        .canonicalize()
        .unwrap_or_else(|_| ctx.settings.project_root.clone());
    let rel = path.strip_prefix(&root).unwrap_or(path);
    let shown = rel.to_string_lossy().replace('\\', "/");
    if shown.is_empty() { ".".into() } else { shown }
}

fn is_skipped_dir(entry: &walkdir::DirEntry) -> bool {
    entry.depth() > 0
        && entry.file_type().is_dir()
        && SKIP_DIRS
            .iter()
            .any(|d| entry.file_name().to_str() == Some(*d))
}

/// First string argument found under any of `keys` (models use different names).
fn arg_str<'a>(args: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| args[*k].as_str())
}

fn line_ending(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

// ── read_file ─────────────────────────────────────────────────────────────

pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a text file with line numbers (1-indexed). Use start_line/end_line for large files; \
         without a range, long files are returned in a first chunk with a note on how to continue."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path (relative to the project root)" },
                "start_line": { "type": "integer", "description": "First line to read (1-indexed, optional)" },
                "end_line": { "type": "integer", "description": "Last line to read (inclusive, optional)" }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let path_str = arg_str(&args, &["path", "file_path", "TargetFile"]).unwrap_or("");
        let start_line = args["start_line"].as_u64().map(|n| n as usize);
        let end_line = args["end_line"].as_u64().map(|n| n as usize);

        let path = match resolve_path(path_str, ctx) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        if path.is_dir() {
            return ToolResult::err(format!(
                "{} is a directory — use ls instead",
                display_path(&path, ctx)
            ));
        }
        if !path.exists() {
            return ToolResult::err(format!("File not found: {}", display_path(&path, ctx)));
        }

        let content = match std::fs::read(&path) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    return ToolResult::err(format!(
                        "{} is a binary file and can't be shown as text (use view_image for images)",
                        display_path(&path, ctx)
                    ));
                }
            },
            Err(e) => return ToolResult::err(format!("Failed to read file: {e}")),
        };

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        if total == 0 {
            return ToolResult::ok(format!("{} is empty.", display_path(&path, ctx)));
        }

        let start = start_line.unwrap_or(1).clamp(1, total);
        let requested_end = end_line.unwrap_or(total).clamp(start, total);

        // Without an explicit end, stop at the context budget instead of
        // dumping a huge file that would just be truncated mid-line later.
        let budget = ctx.settings.max_content_chars.clamp(4_000, 40_000);
        let mut used = 0;
        let mut end = start - 1;
        for line in &lines[start - 1..requested_end] {
            used += line.len() + 8;
            if end_line.is_none() && used > budget && end >= start {
                break;
            }
            end += 1;
        }

        let numbered: Vec<String> = lines[start - 1..end]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:>5}: {}", start + i, line))
            .collect();

        let mut output = format!(
            "{} (lines {start}-{end} of {total})\n{}",
            display_path(&path, ctx),
            numbered.join("\n")
        );
        if end < requested_end {
            output.push_str(&format!(
                "\n… {} more lines. Continue with start_line={}.",
                total - end,
                end + 1
            ));
        }
        ToolResult::ok(output)
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
        "Create or overwrite a file with the given content. Creates parent directories if needed. \
         For changes to an existing file prefer edit_file / multi_edit."
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
        let path_str = arg_str(&args, &["path", "file_path", "TargetFile"]).unwrap_or("");
        let Some(content) = arg_str(&args, &["content", "CodeContent", "text"]) else {
            return ToolResult::err("Missing 'content' argument");
        };

        let path = match resolve_path(path_str, ctx) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        if path.is_dir() {
            return ToolResult::err(format!("{} is a directory", display_path(&path, ctx)));
        }

        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return ToolResult::err(format!("Failed to create directories: {}", e));
            }
        }

        let existed = path.exists();
        match std::fs::write(&path, content) {
            Ok(_) => ToolResult::ok(format!(
                "✅ {} {} ({} lines, {} bytes)",
                if existed { "Overwrote" } else { "Created" },
                display_path(&path, ctx),
                content.lines().count(),
                content.len()
            )),
            Err(e) => ToolResult::err(format!("Failed to write file: {}", e)),
        }
    }
}

// ── edit_file ─────────────────────────────────────────────────────────────

/// Replace `target` in `content`. Retries with the file's own line endings
/// when the model sent `\n` but the file uses `\r\n`.
fn replace_text(
    content: &str,
    target: &str,
    replacement: &str,
    all: bool,
) -> Result<(String, usize), String> {
    if target.is_empty() {
        return Err("Target string cannot be empty".into());
    }
    let eol = line_ending(content);
    let (target, replacement) =
        if !content.contains(target) && eol == "\r\n" && !target.contains("\r\n") {
            (
                target.replace('\n', "\r\n"),
                replacement.replace('\n', "\r\n"),
            )
        } else {
            (target.to_string(), replacement.to_string())
        };

    let count = content.matches(target.as_str()).count();
    match count {
        0 => Err(
            "Target text not found. It must match the file exactly, including indentation \
                  — re-read the relevant lines and try again."
                .into(),
        ),
        n if n > 1 && !all => Err(format!(
            "Target text appears {n} times. Add surrounding lines to make it unique, \
             set all=true, or restrict the search with start_line/end_line."
        )),
        n => Ok((
            if all {
                content.replace(target.as_str(), &replacement)
            } else {
                content.replacen(target.as_str(), &replacement, 1)
            },
            n,
        )),
    }
}

fn diff_summary(original: &str, updated: &str) -> String {
    let diff = similar::TextDiff::from_lines(original, updated);
    let mut out = String::new();
    let mut shown = 0;
    let mut total = 0;
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            similar::ChangeTag::Delete => "-",
            similar::ChangeTag::Insert => "+",
            similar::ChangeTag::Equal => continue,
        };
        total += 1;
        // Keep the echo short: the model already knows what it sent.
        if shown < 40 {
            out.push_str(sign);
            out.push_str(change.as_str().unwrap_or("").trim_end_matches(['\r', '\n']));
            out.push('\n');
            shown += 1;
        }
    }
    if total > shown {
        out.push_str(&format!("… {} more changed lines\n", total - shown));
    }
    out
}

pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing exact text, or by replacing a line range. \
         `target` must match the file exactly (including indentation) and be unique unless all=true. \
         With start_line/end_line and no target, the whole range is replaced. \
         For several changes to one file use multi_edit. Use this instead of sed/awk."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path" },
                "target": { "type": "string", "description": "Exact text to replace (optional when start_line/end_line are given)" },
                "replacement": { "type": "string", "description": "Replacement text" },
                "start_line": { "type": "integer", "description": "First line of the region (1-indexed, optional)" },
                "end_line": { "type": "integer", "description": "Last line of the region (inclusive, optional)" },
                "all": { "type": "boolean", "description": "Replace every occurrence in the region (default: false)" }
            },
            "required": ["path", "replacement"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let path_str = arg_str(&args, &["path", "file_path", "TargetFile"]).unwrap_or("");
        let target_opt = arg_str(
            &args,
            &["target", "old_string", "old_text", "TargetContent"],
        );
        let Some(replacement) = arg_str(
            &args,
            &[
                "replacement",
                "new_string",
                "new_text",
                "ReplacementContent",
            ],
        ) else {
            return ToolResult::err("Missing 'replacement' argument");
        };
        let start_line = args["start_line"].as_u64().map(|n| n as usize);
        let end_line = args["end_line"].as_u64().map(|n| n as usize);
        let replace_all = args["all"].as_bool().unwrap_or(false);

        let path = match resolve_path(path_str, ctx) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };

        let original = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                return ToolResult::err(format!(
                    "Failed to read {}: {e}",
                    display_path(&path, ctx)
                ));
            }
        };
        let eol = line_ending(&original);

        let new_content = match (start_line, end_line) {
            (Some(start), Some(end)) => {
                let lines: Vec<&str> = original.lines().collect();
                let total = lines.len();
                if start < 1 || start > total || end < start || end > total {
                    return ToolResult::err(format!(
                        "Invalid line range {start}-{end}: the file has {total} lines."
                    ));
                }
                let region = lines[start - 1..end].join(eol);
                let new_region = match target_opt {
                    Some(target) => match replace_text(&region, target, replacement, replace_all) {
                        Ok((text, _)) => text,
                        Err(e) => return ToolResult::err(format!("Lines {start}-{end}: {e}")),
                    },
                    None => replacement.replace("\r\n", "\n").replace('\n', eol),
                };

                let mut parts: Vec<String> = Vec::with_capacity(3);
                if start > 1 {
                    parts.push(lines[..start - 1].join(eol));
                }
                parts.push(new_region);
                if end < total {
                    parts.push(lines[end..].join(eol));
                }
                let mut joined = parts.join(eol);
                if original.ends_with('\n') && !joined.ends_with('\n') {
                    joined.push_str(eol);
                }
                joined
            }
            (None, None) => {
                let Some(target) = target_opt else {
                    return ToolResult::err("Provide `target`, or both start_line and end_line.");
                };
                match replace_text(&original, target, replacement, replace_all) {
                    Ok((text, _)) => text,
                    Err(e) => return ToolResult::err(format!("{}: {e}", display_path(&path, ctx))),
                }
            }
            _ => return ToolResult::err("Give both start_line and end_line, or neither."),
        };

        if new_content == original {
            return ToolResult::err("The edit would not change the file.");
        }

        match std::fs::write(&path, &new_content) {
            Ok(_) => ToolResult::ok(format!(
                "✅ Edited {}\n{}",
                display_path(&path, ctx),
                diff_summary(&original, &new_content)
            )),
            Err(e) => ToolResult::err(format!("Failed to write file: {}", e)),
        }
    }
}

// ── multi_edit ────────────────────────────────────────────────────────────

pub struct MultiEditTool;

#[async_trait]
impl Tool for MultiEditTool {
    fn name(&self) -> &str {
        "multi_edit"
    }

    fn description(&self) -> &str {
        "Apply several exact-text replacements to one file in order, all-or-nothing \
         (if any target is missing, nothing is written). Each target must match exactly and be unique unless all=true."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path" },
                "edits": {
                    "type": "array",
                    "description": "Replacements applied in order",
                    "items": {
                        "type": "object",
                        "properties": {
                            "target": { "type": "string", "description": "Exact text to replace" },
                            "replacement": { "type": "string", "description": "Replacement text" },
                            "all": { "type": "boolean", "description": "Replace every occurrence (default: false)" }
                        },
                        "required": ["target", "replacement"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let path_str = arg_str(&args, &["path", "file_path"]).unwrap_or("");
        let Some(edits) = args["edits"].as_array().filter(|e| !e.is_empty()) else {
            return ToolResult::err("'edits' must be a non-empty array");
        };
        let path = match resolve_path(path_str, ctx) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        let original = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                return ToolResult::err(format!(
                    "Failed to read {}: {e}",
                    display_path(&path, ctx)
                ));
            }
        };

        let mut content = original.clone();
        for (i, edit) in edits.iter().enumerate() {
            let target = arg_str(edit, &["target", "old_string"]).unwrap_or("");
            let replacement = arg_str(edit, &["replacement", "new_string"]).unwrap_or("");
            let all = edit["all"].as_bool().unwrap_or(false);
            match replace_text(&content, target, replacement, all) {
                Ok((text, _)) => content = text,
                Err(e) => {
                    return ToolResult::err(format!(
                        "Edit #{} failed, no changes were written: {e}",
                        i + 1
                    ));
                }
            }
        }

        if content == original {
            return ToolResult::err("The edits would not change the file.");
        }
        match std::fs::write(&path, &content) {
            Ok(_) => ToolResult::ok(format!(
                "✅ Applied {} edits to {}\n{}",
                edits.len(),
                display_path(&path, ctx),
                diff_summary(&original, &content)
            )),
            Err(e) => ToolResult::err(format!("Failed to write file: {e}")),
        }
    }
}

// ── ls (list directory) ───────────────────────────────────────────────────

pub struct ListDirTool;

const LS_MAX_ENTRIES: usize = 500;

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "ls"
    }

    fn description(&self) -> &str {
        "List files and directories with sizes. Recursive listings skip dependency/build folders \
         (node_modules, target, .git, .venv, …)."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory path (default: project root)" },
                "recursive": { "type": "boolean", "description": "List recursively (default: false)" },
                "max_depth": { "type": "integer", "description": "Max recursion depth (default: 3)" }
            },
            "required": []
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let path_str = args["path"].as_str().unwrap_or(".");
        let recursive = args["recursive"].as_bool().unwrap_or(false);
        let max_depth = args["max_depth"].as_u64().unwrap_or(3).clamp(1, 10) as usize;

        let path = match resolve_path(path_str, ctx) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };

        if !path.is_dir() {
            return ToolResult::err(format!("Not a directory: {}", display_path(&path, ctx)));
        }

        let walker = walkdir::WalkDir::new(&path)
            .min_depth(1)
            .max_depth(if recursive { max_depth } else { 1 })
            .sort_by(|a, b| {
                b.file_type()
                    .is_dir()
                    .cmp(&a.file_type().is_dir())
                    .then_with(|| a.file_name().cmp(b.file_name()))
            });

        let mut entries = Vec::new();
        let mut truncated = false;
        for entry in walker
            .into_iter()
            .filter_entry(|e| !recursive || !is_skipped_dir(e))
            .filter_map(|e| e.ok())
        {
            if entries.len() >= LS_MAX_ENTRIES {
                truncated = true;
                break;
            }
            let rel = entry.path().strip_prefix(&path).unwrap_or(entry.path());
            let rel = rel.to_string_lossy().replace('\\', "/");
            if entry.file_type().is_dir() {
                entries.push(format!("{:>10}  {rel}/", "dir"));
            } else {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                entries.push(format!("{:>10}  {rel}", format_size(size)));
            }
        }

        if entries.is_empty() {
            return ToolResult::ok(format!("Directory is empty: {}", display_path(&path, ctx)));
        }

        let mut output = format!("{}:\n{}", display_path(&path, ctx), entries.join("\n"));
        if truncated {
            output.push_str(&format!(
                "\n… listing stopped at {LS_MAX_ENTRIES} entries; list a subdirectory or lower max_depth."
            ));
        }
        ToolResult::ok(output)
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

// ── grep ──────────────────────────────────────────────────────────────────

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search file contents with a regex. Returns path:line: text (max 100 matches). \
         Skips binary files and dependency/build folders."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Search pattern (regex)" },
                "path": { "type": "string", "description": "File or directory to search (default: project root)" },
                "case_insensitive": { "type": "boolean", "description": "Case-insensitive search (default: false)" },
                "glob": { "type": "string", "description": "Only search files whose name matches this glob, e.g. '*.rs'" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let pattern_str = args["pattern"].as_str().unwrap_or("");
        let path_str = args["path"].as_str().unwrap_or(".");
        let case_insensitive = args["case_insensitive"].as_bool().unwrap_or(false);

        if pattern_str.is_empty() {
            return ToolResult::err("Pattern cannot be empty");
        }
        let path = match resolve_path(path_str, ctx) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };

        let regex = match regex::RegexBuilder::new(pattern_str)
            .case_insensitive(case_insensitive)
            .size_limit(10 * 1024 * 1024)
            .build()
        {
            Ok(r) => r,
            Err(e) => return ToolResult::err(format!("Invalid regex: {}", e)),
        };
        let name_filter = match args["glob"].as_str().filter(|g| !g.is_empty()) {
            Some(g) => match globset::Glob::new(g) {
                Ok(glob) => Some(glob.compile_matcher()),
                Err(e) => return ToolResult::err(format!("Invalid glob: {e}")),
            },
            None => None,
        };

        let max_results = 100;
        let mut results = Vec::new();
        let mut truncated = false;

        let files: Vec<PathBuf> = if path.is_file() {
            vec![path.clone()]
        } else {
            walkdir::WalkDir::new(&path)
                .max_depth(12)
                .into_iter()
                .filter_entry(|e| !is_skipped_dir(e))
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
                .filter(|e| e.metadata().is_ok_and(|m| m.len() <= GREP_MAX_FILE_BYTES))
                .filter(|e| {
                    name_filter
                        .as_ref()
                        .is_none_or(|g| g.is_match(e.file_name()))
                })
                .map(|e| e.into_path())
                .collect()
        };

        'outer: for file in &files {
            // read_to_string fails on non-UTF-8 (binary) files, which skips them.
            let Ok(content) = std::fs::read_to_string(file) else {
                continue;
            };
            let rel = display_path(file, ctx);
            for (i, line) in content.lines().enumerate() {
                if regex.is_match(line) {
                    // Minified files have enormous lines.
                    let shown = crate::util::truncate_chars(line.trim(), 240);
                    results.push(format!("{rel}:{}: {shown}", i + 1));
                    if results.len() >= max_results {
                        truncated = true;
                        break 'outer;
                    }
                }
            }
        }

        if results.is_empty() {
            return ToolResult::ok(format!(
                "No matches for /{pattern_str}/ in {}",
                display_path(&path, ctx)
            ));
        }
        let mut output = results.join("\n");
        if truncated {
            output.push_str(&format!(
                "\n… stopped at {max_results} matches; narrow the pattern, path or glob."
            ));
        }
        ToolResult::ok(output)
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
        "Find files by name (glob, e.g. '*.rs' or 'test_*'). Matches the file name, or the relative \
         path when the pattern contains '/'. Skips dependency/build folders."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern (e.g. '*.rs', 'src/**/mod.rs')" },
                "path": { "type": "string", "description": "Directory to search (default: project root)" }
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
            .literal_separator(false)
            .build()
        {
            Ok(g) => g.compile_matcher(),
            Err(e) => return ToolResult::err(format!("Invalid glob pattern: {}", e)),
        };
        let match_path = pattern.contains('/');

        let mut results = Vec::new();
        for entry in walkdir::WalkDir::new(&path)
            .max_depth(12)
            .min_depth(1)
            .into_iter()
            .filter_entry(|e| !is_skipped_dir(e))
            .filter_map(|e| e.ok())
        {
            let rel = entry.path().strip_prefix(&path).unwrap_or(entry.path());
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            let hit = if match_path {
                glob.is_match(&rel_str)
            } else {
                glob.is_match(entry.file_name())
            };
            if hit {
                results.push(display_path(entry.path(), ctx));
                if results.len() >= 200 {
                    results.push("… stopped at 200 results".into());
                    break;
                }
            }
        }

        if results.is_empty() {
            ToolResult::ok(format!("No files found matching: {pattern}"))
        } else {
            ToolResult::ok(results.join("\n"))
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
            if let Err(e) = std::fs::create_dir_all(parent) {
                return ToolResult::err(format!("Failed to create directories: {e}"));
            }
        }

        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(_) => ToolResult::ok(format!("✅ Created: {}", display_path(&path, ctx))),
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
        "Remove a file, or a directory (recursive=true for non-empty directories)."
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

        let root = ctx
            .settings
            .project_root
            .canonicalize()
            .unwrap_or_else(|_| ctx.settings.project_root.clone());
        if path == root {
            return ToolResult::err("Refusing to delete the project root.");
        }
        // Never allow deleting .norvexum config/checkpoints/sessions
        if path
            .components()
            .any(|c| c.as_os_str() == crate::config::NORVEXUM_DIR)
        {
            return ToolResult::err("Cannot delete the .norvexum configuration directory");
        }
        if !path.exists() {
            return ToolResult::err(format!("Not found: {}", display_path(&path, ctx)));
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
            Ok(_) => ToolResult::ok(format!("✅ Removed: {}", display_path(&path, ctx))),
            Err(e) if path.is_dir() && !recursive => ToolResult::err(format!(
                "Failed to remove directory ({e}). Use recursive=true for non-empty directories."
            )),
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
        "Move or rename a file or directory. Moving into an existing directory keeps the name."
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
        let mut dst = match resolve_path(dst_str, ctx) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        if !src.exists() {
            return ToolResult::err(format!("Source not found: {}", display_path(&src, ctx)));
        }
        if dst.is_dir() {
            if let Some(name) = src.file_name() {
                dst = dst.join(name);
            }
        }
        if let Some(parent) = dst.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        match std::fs::rename(&src, &dst) {
            Ok(_) => ToolResult::ok(format!(
                "✅ Moved: {} → {}",
                display_path(&src, ctx),
                display_path(&dst, ctx)
            )),
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
        "Copy a file, or a directory with recursive=true. Copying into an existing directory keeps the name."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "source": { "type": "string", "description": "Source path" },
                "destination": { "type": "string", "description": "Destination path" },
                "recursive": { "type": "boolean", "description": "Copy a directory and its contents (default: false)" }
            },
            "required": ["source", "destination"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let src_str = args["source"].as_str().unwrap_or("");
        let dst_str = args["destination"].as_str().unwrap_or("");
        let recursive = args["recursive"].as_bool().unwrap_or(false);

        let src = match resolve_path(src_str, ctx) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        let mut dst = match resolve_path(dst_str, ctx) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        if !src.exists() {
            return ToolResult::err(format!("Source not found: {}", display_path(&src, ctx)));
        }
        if dst.is_dir() {
            if let Some(name) = src.file_name() {
                dst = dst.join(name);
            }
        }

        if src.is_dir() {
            if !recursive {
                return ToolResult::err("Source is a directory; set recursive=true to copy it.");
            }
            if dst.starts_with(&src) {
                return ToolResult::err("Cannot copy a directory into itself.");
            }
            let mut copied = 0usize;
            for entry in walkdir::WalkDir::new(&src)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                let rel = entry.path().strip_prefix(&src).unwrap_or(entry.path());
                let target = dst.join(rel);
                let res = if entry.file_type().is_dir() {
                    std::fs::create_dir_all(&target)
                } else {
                    std::fs::copy(entry.path(), &target).map(|_| {
                        copied += 1;
                    })
                };
                if let Err(e) = res {
                    return ToolResult::err(format!(
                        "Copy failed at {}: {e}",
                        display_path(&target, ctx)
                    ));
                }
            }
            return ToolResult::ok(format!(
                "✅ Copied {copied} files: {} → {}",
                display_path(&src, ctx),
                display_path(&dst, ctx)
            ));
        }

        if let Some(parent) = dst.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::copy(&src, &dst) {
            Ok(bytes) => ToolResult::ok(format!(
                "✅ Copied: {} → {} ({} bytes)",
                display_path(&src, ctx),
                display_path(&dst, ctx),
                bytes
            )),
            Err(e) => ToolResult::err(format!("Failed to copy: {}", e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_text_handles_crlf_and_uniqueness() {
        let file = "fn a() {\r\n    1\r\n}\r\nfn b() {\r\n    1\r\n}\r\n";
        let err = replace_text(file, "    1", "    2", false).unwrap_err();
        assert!(err.contains("2 times"));

        let (out, n) =
            replace_text(file, "fn a() {\n    1\n}", "fn a() {\n    9\n}", false).unwrap();
        assert_eq!(n, 1);
        assert!(out.contains("fn a() {\r\n    9\r\n}"));
        assert!(!out.contains("\n    9\n"));
    }
}
