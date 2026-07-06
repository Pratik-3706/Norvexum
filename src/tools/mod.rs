// ═══════════════════════════════════════════════════════════════════════════
// Tools — Registry, trait, and result types
//
// All tools:
//   • Implement the Tool trait (async execute)
//   • Are registered by name in the ToolRegistry
//   • Return ToolResult with structured output
//   • Are sandboxed to the project directory
// ═══════════════════════════════════════════════════════════════════════════

pub mod batch_image_inspect;
pub mod filesystem;
pub mod git;
pub mod image_download;
pub mod image_gen;
pub mod image_inspect;
pub mod image_search;
pub mod package_safety;
pub mod shell;
pub mod skills;
pub mod web_fetch;
pub mod web_search;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

use crate::ai::types::ToolDef;
use crate::config::Settings;

/// Result of a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub success: bool,
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Structured data for the agent to use (e.g. file paths, image URLs)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl ToolResult {
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            success: true,
            output: output.into(),
            error: None,
            data: None,
        }
    }

    pub fn ok_with_data(output: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            success: true,
            output: output.into(),
            error: None,
            data: Some(data),
        }
    }

    pub fn err(error: impl Into<String>) -> Self {
        let error_str = error.into();
        Self {
            success: false,
            output: String::new(),
            error: Some(error_str),
            data: None,
        }
    }

    /// Format for inclusion in the AI conversation as a tool result.
    pub fn to_message_content(&self) -> String {
        if self.success {
            if let Some(data) = &self.data {
                format!(
                    "{}\n\n[data]: {}",
                    self.output,
                    serde_json::to_string(data).unwrap_or_default()
                )
            } else {
                self.output.clone()
            }
        } else {
            format!(
                "ERROR: {}",
                self.error.as_deref().unwrap_or("Unknown error")
            )
        }
    }
}

/// Context passed to every tool execution.
#[derive(Clone)]
pub struct ToolContext {
    pub settings: Arc<Settings>,
    /// Current working directory (within sandbox)
    pub cwd: std::path::PathBuf,
    pub client: Option<Arc<dyn crate::ai::AiClient>>,
}

/// The trait all tools must implement.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Unique name used in tool calls (e.g. "read_file", "web_search")
    fn name(&self) -> &str;

    /// Human-readable description for the AI model
    fn description(&self) -> &str;

    /// JSON Schema describing the tool's parameters
    fn parameters(&self) -> serde_json::Value;

    /// Execute the tool with the given arguments
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult;

    /// Convert to a ToolDef for the AI API
    fn to_tool_def(&self) -> ToolDef {
        ToolDef {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters(),
        }
    }
}

/// Registry of all available tools.
/// Wrapped in Arc-compatible containers for safe sharing across spawned tasks.
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Create a new registry and register all built-in tools.
    pub fn new(_settings: &Settings) -> Self {
        let mut registry = Self {
            tools: HashMap::new(),
        };

        // ── Filesystem tools ─────────────────────────────────────────────
        registry.register(Arc::new(filesystem::ReadFileTool));
        registry.register(Arc::new(filesystem::WriteFileTool));
        registry.register(Arc::new(filesystem::EditFileTool));
        registry.register(Arc::new(filesystem::ListDirTool));
        registry.register(Arc::new(filesystem::GrepTool));
        registry.register(Arc::new(filesystem::FindTool));
        registry.register(Arc::new(filesystem::TouchTool));
        registry.register(Arc::new(filesystem::RemoveTool));
        registry.register(Arc::new(filesystem::MoveTool));
        registry.register(Arc::new(filesystem::CopyTool));
        registry.register(Arc::new(filesystem::PwdTool));
        registry.register(Arc::new(filesystem::CatTool));

        // ── Web tools ────────────────────────────────────────────────────
        registry.register(Arc::new(web_search::WebSearchTool));
        registry.register(Arc::new(web_fetch::WebFetchTool));
        registry.register(Arc::new(image_search::ImageSearchTool));
        registry.register(Arc::new(image_search::ZerochanSearchTool));
        registry.register(Arc::new(image_download::DownloadImageTool));
        registry.register(Arc::new(image_download::BatchDownloadImageTool));
        registry.register(Arc::new(image_inspect::InspectImageTool));
        registry.register(Arc::new(image_gen::GenerateImageTool));
        registry.register(Arc::new(skills::ListSkillsTool));
        registry.register(Arc::new(skills::ReadSkillTool));

        // ── Batch image analysis ─────────────────────────────────────────
        registry.register(Arc::new(batch_image_inspect::BatchViewImageTool));

        // ── Package safety ───────────────────────────────────────────────
        registry.register(Arc::new(package_safety::PackageSafetyTool));

        // ── Shell command execution ──────────────────────────────────────
        registry.register(Arc::new(shell::ShellCommandTool));

        // ── Git tools ────────────────────────────────────────────────────
        registry.register(Arc::new(git::GitStatusTool));
        registry.register(Arc::new(git::GitDiffTool));
        registry.register(Arc::new(git::GitCommitTool));
        registry.register(Arc::new(git::GitLogTool));

        registry
    }

    fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// Get a tool by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Execute a tool by name with the given arguments.
    pub async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> ToolResult {
        match self.get(name) {
            Some(tool) => tool.execute(args, ctx).await,
            None => ToolResult::err(format!("Unknown tool: {}", name)),
        }
    }

    /// Get all tool definitions for the AI API.
    pub fn tool_defs(&self) -> Vec<ToolDef> {
        self.tools.values().map(|t| t.to_tool_def()).collect()
    }

    /// List all registered tool names.
    pub fn tool_names(&self) -> Vec<&str> {
        self.tools.keys().map(|s| s.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_context() -> (std::path::PathBuf, ToolContext) {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("tool-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let settings = Settings {
            project_root: root.clone(),
            ..Settings::default()
        };
        let context = ToolContext {
            settings: Arc::new(settings),
            cwd: root.clone(),
            client: None,
        };
        (root, context)
    }

    #[tokio::test]
    async fn write_file_creates_nested_directories() {
        let (root, context) = test_context();
        let registry = ToolRegistry::new(&context.settings);
        let result = registry
            .execute(
                "write_file",
                json!({ "path": "nested/deeper/file.txt", "content": "works" }),
                &context,
            )
            .await;

        assert!(result.success, "{:?}", result.error);
        assert_eq!(
            std::fs::read_to_string(root.join("nested/deeper/file.txt")).unwrap(),
            "works"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn nonzero_shell_exit_is_a_failed_tool_result() {
        let (root, context) = test_context();
        let registry = ToolRegistry::new(&context.settings);
        let command = "exit 7";
        let result = registry
            .execute("run_command", json!({ "command": command }), &context)
            .await;

        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or_default().contains('7'));
        std::fs::remove_dir_all(root).unwrap();
    }

    fn create_symlink(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link)
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(target, link)
        }
    }

    #[tokio::test]
    async fn resolve_path_blocks_symlink_escape() {
        let (root, context) = test_context();
        let outside_file = root.parent().unwrap().join("outside_symlink.txt");
        std::fs::write(&outside_file, "hello").unwrap();

        let link_path = root.join("escaped_link.txt");
        if create_symlink(&outside_file, &link_path).is_ok() {
            let res = super::filesystem::resolve_path("escaped_link.txt", &context);
            assert!(res.is_err(), "Symlink escape should be blocked");
        }

        let _ = std::fs::remove_file(outside_file);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn resolve_path_allows_internal_symlink() {
        let (root, context) = test_context();
        let inside_file = root.join("inside.txt");
        std::fs::write(&inside_file, "hello").unwrap();

        let link_path = root.join("internal_link.txt");
        if create_symlink(&inside_file, &link_path).is_ok() {
            let res = super::filesystem::resolve_path("internal_link.txt", &context);
            assert!(res.is_ok(), "Internal symlink should be allowed");
        }

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn run_command_blocks_absolute_and_traversal_argv_paths() {
        let (root, context) = test_context();
        let registry = ToolRegistry::new(&context.settings);

        // Test absolute path in argument
        let result = registry
            .execute(
                "run_command",
                json!({ "command": "cat /etc/passwd" }),
                &context,
            )
            .await;
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("blocked")
        );

        // Test traversal path in argument
        let result = registry
            .execute(
                "run_command",
                json!({ "command": "cat ../outside.txt" }),
                &context,
            )
            .await;
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("blocked")
        );

        // Test absolute path command name (argv[0])
        let result = registry
            .execute("run_command", json!({ "command": "/bin/ls" }), &context)
            .await;
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("blocked")
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn run_command_intercepts_cd() {
        let (root, context) = test_context();
        let registry = ToolRegistry::new(&context.settings);

        // Test valid cd
        let result = registry
            .execute("run_command", json!({ "command": "cd src" }), &context)
            .await;
        assert!(result.success);
        assert!(result.output.contains("Directory changed to"));

        // Test invalid cd (outside sandbox)
        let result = registry
            .execute("run_command", json!({ "command": "cd .." }), &context)
            .await;
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("blocked")
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn run_command_blocks_denylisted_basename() {
        let (root, context) = test_context();
        let registry = ToolRegistry::new(&context.settings);

        // Test exact match
        let result = registry
            .execute(
                "run_command",
                json!({ "command": "sudo rm -rf ." }),
                &context,
            )
            .await;
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("restricted")
        );

        // Test path prefix match
        let result = registry
            .execute(
                "run_command",
                json!({ "command": "/usr/bin/sudo rm -rf ." }),
                &context,
            )
            .await;
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("restricted")
        );

        // Test mkfs prefix match
        let result = registry
            .execute(
                "run_command",
                json!({ "command": "mkfs.ext4 /dev/sdb1" }),
                &context,
            )
            .await;
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("restricted")
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn image_download_cannot_escape_project() {
        let (root, context) = test_context();
        let registry = ToolRegistry::new(&context.settings);
        let result = registry
            .execute(
                "download_image",
                json!({
                    "url": "https://example.invalid/image.png",
                    "filename": "image",
                    "output_dir": "../outside"
                }),
                &context,
            )
            .await;

        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("outside the project sandbox")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn registry_exposes_only_implemented_tools() {
        let settings = Settings::default();
        let registry = ToolRegistry::new(&settings);
        let names = registry.tool_names();
        assert!(!names.contains(&"browser_click"));
        assert!(!names.contains(&"browser_screenshot"));
        assert!(!names.contains(&"cd"));
        // Verify new tools are registered
        assert!(names.contains(&"git_status"));
        assert!(names.contains(&"git_diff"));
        assert!(names.contains(&"git_commit"));
        assert!(names.contains(&"git_log"));
        assert!(names.contains(&"batch_view_images"));
    }
}
