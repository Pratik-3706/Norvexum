// ═══════════════════════════════════════════════════════════════════════════
// Session Persistence — Save/load conversation state across restarts
//
// Sessions are stored as JSON in .norvexum/sessions/<hash>.json
// Auto-saved after each agent turn for crash safety.
// ═══════════════════════════════════════════════════════════════════════════

use eyre::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use crate::ai::types::Message;
use crate::config;

/// A serializable session snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub messages: Vec<Message>,
    pub created_at: String,
    pub updated_at: String,
    pub model: String,
    pub provider: String,
}

/// Lightweight summary for listing sessions.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub message_count: usize,
    pub model: String,
}

impl Session {
    /// Create a new session.
    pub fn new(messages: Vec<Message>, model: &str, provider: &str) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        let id = generate_session_id(&now);
        Self {
            id,
            messages,
            created_at: now.clone(),
            updated_at: now,
            model: model.to_string(),
            provider: provider.to_string(),
        }
    }

    /// Save the session to disk.
    pub fn save(&mut self, root: &Path) -> Result<()> {
        self.updated_at = chrono::Utc::now().to_rfc3339();
        let dir = sessions_dir(root);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.json", self.id));
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Load the most recent session for a project.
    pub fn load_latest(root: &Path) -> Option<Self> {
        let dir = sessions_dir(root);
        if !dir.exists() {
            return None;
        }

        let mut sessions: Vec<(String, PathBuf)> = std::fs::read_dir(&dir)
            .ok()?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .map(|ext| ext == "json")
                    .unwrap_or(false)
            })
            .filter_map(|e| {
                let metadata = e.metadata().ok()?;
                let modified = metadata
                    .modified()
                    .ok()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_secs()
                    .to_string();
                Some((modified, e.path()))
            })
            .collect();

        sessions.sort_by(|a, b| b.0.cmp(&a.0)); // Most recent first

        if let Some((_, path)) = sessions.first() {
            let content = std::fs::read_to_string(path).ok()?;
            serde_json::from_str(&content).ok()
        } else {
            None
        }
    }

    /// Load a specific session by ID.
    pub fn load_by_id(root: &Path, id: &str) -> Option<Self> {
        let path = sessions_dir(root).join(format!("{}.json", id));
        if !path.exists() {
            return None;
        }
        let content = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// List all sessions for a project.
    pub fn list(root: &Path) -> Vec<SessionSummary> {
        let dir = sessions_dir(root);
        if !dir.exists() {
            return Vec::new();
        }

        let mut summaries = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                if entry
                    .path()
                    .extension()
                    .map(|e| e == "json")
                    .unwrap_or(false)
                {
                    if let Ok(content) = std::fs::read_to_string(entry.path()) {
                        if let Ok(session) = serde_json::from_str::<Session>(&content) {
                            summaries.push(SessionSummary {
                                id: session.id,
                                created_at: session.created_at,
                                updated_at: session.updated_at,
                                message_count: session.messages.len(),
                                model: session.model,
                            });
                        }
                    }
                }
            }
        }

        summaries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        summaries
    }

    /// Delete a session.
    pub fn delete(root: &Path, id: &str) -> Result<()> {
        let path = sessions_dir(root).join(format!("{}.json", id));
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    /// Clear all sessions.
    pub fn clear_all(root: &Path) -> Result<()> {
        let dir = sessions_dir(root);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
            std::fs::create_dir_all(&dir)?;
        }
        Ok(())
    }
}

fn sessions_dir(root: &Path) -> PathBuf {
    root.join(config::NORVEXUM_DIR).join("sessions")
}

fn generate_session_id(timestamp: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(timestamp.as_bytes());
    hasher.update(uuid::Uuid::new_v4().to_string().as_bytes());
    let result = hasher.finalize();
    format!("{:x}", result)[..12].to_string()
}
