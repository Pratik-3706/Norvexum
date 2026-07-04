// ═══════════════════════════════════════════════════════════════════════════
// Skills — Prebuilt extensible behavioral templates
//
// Triggers the agent with specialized instructions for specific tasks.
// ═══════════════════════════════════════════════════════════════════════════

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub trigger_patterns: Vec<String>,
    pub system_instructions: String,
    pub default_tools: Vec<String>,
}

/// Load default prebuilt skills
pub fn load_default_skills() -> Vec<Skill> {
    vec![
        Skill {
            name: "code_review".into(),
            description: "Review code for performance, bugs, and best practices".into(),
            trigger_patterns: vec!["review code".into(), "check code".into(), "find bugs".into()],
            system_instructions: "You are a senior code reviewer. Analyze the code files carefully. \
                                  Point out security flaws, race conditions, memory leaks, or bad design patterns. \
                                  Propose exact refactoring options using the edit_file tool.".into(),
            default_tools: vec!["read_file".into(), "edit_file".into(), "grep".into()],
        },
        Skill {
            name: "image_finder".into(),
            description: "Find, verify, and download high-quality images".into(),
            trigger_patterns: vec!["find image".into(), "download image".into(), "search photos".into()],
            system_instructions: "You are an image curator. Search for relevant images, rank them by resolution \
                                  and query alignment, and download the best candidate to the assets directory.".into(),
            default_tools: vec!["image_search".into(), "download_image".into(), "batch_download_images".into()],
        },
    ]
}

/// Search for any custom skill matching user query.
pub fn find_matching_skill(query: &str, custom_path: Option<&Path>) -> Option<Skill> {
    let mut skills = load_default_skills();

    // Future: Load custom TOML skills from ~/.norvexum/skills/ or .norvexum/skills/
    if let Some(path) = custom_path {
        if path.is_dir() {
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.filter_map(|e| e.ok()) {
                    if entry.path().extension().and_then(|s| s.to_str()) == Some("toml") {
                        if let Ok(content) = std::fs::read_to_string(entry.path()) {
                            if let Ok(skill) = toml::from_str::<Skill>(&content) {
                                skills.push(skill);
                            }
                        }
                    }
                }
            }
        }
    }

    let query_lower = query.to_lowercase();
    for skill in skills {
        for pattern in &skill.trigger_patterns {
            if query_lower.contains(pattern) {
                return Some(skill);
            }
        }
    }
    None
}
