// ═══════════════════════════════════════════════════════════════════════════
// Agent History — Project-aware context management
// ═══════════════════════════════════════════════════════════════════════════

use eyre::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::config;

/// Semantic project context — updated after each session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectContext {
    /// Auto-detected project name
    pub name: String,
    /// Detected tech stack
    pub tech_stack: Vec<String>,
    /// Key files and their purposes
    pub key_files: Vec<FileInfo>,
    /// Summary of project structure
    pub structure_summary: String,
    /// Recent changes made by the agent
    pub recent_changes: Vec<ChangeEntry>,
    /// Last updated timestamp
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInfo {
    pub path: String,
    pub purpose: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeEntry {
    pub timestamp: String,
    pub summary: String,
    pub files_changed: Vec<String>,
}

impl ProjectContext {
    /// Scan the project directory and build initial context.
    pub fn scan(root: &Path) -> Self {
        let name = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("project")
            .to_string();

        let mut tech_stack = Vec::new();
        let mut key_files = Vec::new();

        // Detect tech stack from files
        let markers = [
            ("Cargo.toml", "Rust"),
            ("package.json", "Node.js/JavaScript"),
            ("requirements.txt", "Python"),
            ("pyproject.toml", "Python"),
            ("go.mod", "Go"),
            ("pom.xml", "Java (Maven)"),
            ("build.gradle", "Java (Gradle)"),
            ("Gemfile", "Ruby"),
            ("Makefile", "Make"),
            ("Dockerfile", "Docker"),
            ("docker-compose.yml", "Docker Compose"),
            (".gitignore", "Git"),
        ];

        for (file, tech) in &markers {
            if root.join(file).exists() {
                tech_stack.push(tech.to_string());
                key_files.push(FileInfo {
                    path: file.to_string(),
                    purpose: format!("{} configuration", tech),
                });
            }
        }

        // Build structure summary
        let mut dirs = Vec::new();
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.filter_map(|e| e.ok()) {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    dirs.push(name);
                }
            }
        }

        let structure_summary = if dirs.is_empty() {
            "Empty project directory".to_string()
        } else {
            format!("Directories: {}", dirs.join(", "))
        };

        Self {
            name,
            tech_stack,
            key_files,
            structure_summary,
            recent_changes: Vec::new(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// Load existing context from .norvexum/project_context.json
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join(config::NORVEXUM_DIR).join(config::CONTEXT_FILE);
        let content = std::fs::read_to_string(&path)?;
        let ctx: Self = serde_json::from_str(&content)?;
        Ok(ctx)
    }

    /// Save context to .norvexum/project_context.json
    pub fn save(&self, root: &Path) -> Result<()> {
        let dir = root.join(config::NORVEXUM_DIR);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(config::CONTEXT_FILE);
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Add a change entry.
    pub fn add_change(&mut self, summary: &str, files: Vec<String>) {
        self.recent_changes.push(ChangeEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            summary: summary.to_string(),
            files_changed: files,
        });

        // Keep only last 50 changes
        if self.recent_changes.len() > 50 {
            self.recent_changes = self
                .recent_changes
                .split_off(self.recent_changes.len() - 50);
        }

        self.updated_at = chrono::Utc::now().to_rfc3339();
    }

    /// Generate a summary string for inclusion in the system prompt.
    pub fn to_prompt_summary(&self) -> String {
        let mut s = format!("PROJECT: {}\n", self.name);
        if !self.tech_stack.is_empty() {
            s.push_str(&format!("TECH STACK: {}\n", self.tech_stack.join(", ")));
        }
        s.push_str(&format!("STRUCTURE: {}\n", self.structure_summary));
        if !self.recent_changes.is_empty() {
            s.push_str("RECENT CHANGES:\n");
            for change in self.recent_changes.iter().rev().take(5) {
                s.push_str(&format!("  - {}: {}\n", change.timestamp, change.summary));
            }
        }
        s
    }
}
