// ═══════════════════════════════════════════════════════════════════════════
// Skills — Prebuilt and Custom extensible behavioral templates
//
// Triggers the agent with specialized instructions for specific tasks.
// Loads markdown files with YAML frontmatter from:
//   - <project_root>/skills/
//   - <project_root>/.norvexum/skills/
//   - ~/.norvexum/skills/
// ═══════════════════════════════════════════════════════════════════════════

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub trigger_patterns: Vec<String>,
    pub system_instructions: String,
    pub default_tools: Vec<String>,
}

/// Load default prebuilt fallback skills
pub fn load_default_skills() -> Vec<Skill> {
    vec![
        Skill {
            name: "code_review".into(),
            description: "Review code for performance, bugs, and best practices".into(),
            trigger_patterns: vec!["review code".into(), "check code".into(), "find bugs".into()],
            system_instructions: "You are a senior code reviewer. Analyze the code files carefully. \
                                  Point out security flaws, race conditions, memory leaks, or bad design patterns. \
                                  Propose exact refactoring options using the edit_file tool.".into(),
            default_tools: vec![],
        },
        Skill {
            name: "image_finder".into(),
            description: "Find, verify, and download high-quality images".into(),
            trigger_patterns: vec!["find image".into(), "download image".into(), "search photos".into()],
            system_instructions: "You are an image curator. Search for relevant images, rank them by resolution \
                                  and query alignment, and download the best candidate to the user-specified output folder \
                                  or the current working directory.".into(),
            default_tools: vec![],
        },
    ]
}

/// Helper to get user's home directory cross-platform
fn get_home_dir() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("USERPROFILE") {
        return Some(PathBuf::from(home));
    }
    if let Ok(home) = std::env::var("HOME") {
        return Some(PathBuf::from(home));
    }
    None
}

/// Parse a markdown file with YAML frontmatter into a Skill
pub fn parse_skill_markdown(content: &str) -> Option<Skill> {
    if !content.starts_with("---") {
        return None;
    }

    let mut lines = content.lines();
    // Skip first "---"
    lines.next()?;

    let mut name = String::new();
    let mut description = String::new();
    let mut trigger_patterns = Vec::new();
    let mut in_frontmatter = true;
    let mut remaining_lines = Vec::new();
    let mut current_key = String::new();

    while let Some(line) = lines.next() {
        if in_frontmatter {
            let trimmed = line.trim();
            if trimmed == "---" {
                in_frontmatter = false;
                continue;
            }
            if trimmed.starts_with('-') {
                let item = trimmed.trim_start_matches('-').trim();
                let unquoted = item.trim_matches('"').trim_matches('\'').to_string();
                if current_key == "trigger_patterns" {
                    trigger_patterns.push(unquoted);
                }
            } else if let Some((key, val)) = trimmed.split_once(':') {
                let k = key.trim().to_lowercase();
                let v = val.trim().trim_matches('"').trim_matches('\'').to_string();
                current_key = k.clone();
                if k == "name" {
                    name = v;
                } else if k == "description" {
                    description = v;
                } else if k == "trigger_patterns" && !v.is_empty() {
                    if v.starts_with('[') && v.ends_with(']') {
                        let parsed: Vec<String> = v[1..v.len() - 1]
                            .split(',')
                            .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                            .collect();
                        trigger_patterns.extend(parsed);
                    } else {
                        trigger_patterns.push(v);
                    }
                }
            }
        } else {
            remaining_lines.push(line);
        }
    }

    if name.is_empty() {
        return None;
    }

    if trigger_patterns.is_empty() {
        trigger_patterns.push(name.to_lowercase());
    }

    Some(Skill {
        name,
        description,
        trigger_patterns,
        system_instructions: remaining_lines.join("\n").trim().to_string(),
        default_tools: vec![],
    })
}

/// Recursively scan directory for .md skill files
fn scan_skills_dir(dir: &Path, skills: &mut Vec<Skill>) {
    if !dir.exists() || !dir.is_dir() {
        return;
    }

    for entry in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("md") {
                if let Ok(content) = std::fs::read_to_string(path) {
                    if let Some(skill) = parse_skill_markdown(&content) {
                        skills.push(skill);
                    }
                }
            }
        }
    }
}

/// Load default and all custom skills from project and home paths
pub fn load_all_skills(project_root: &Path) -> Vec<Skill> {
    let mut skills = load_default_skills();

    // 1. Scan <project_root>/src/skills/
    scan_skills_dir(&project_root.join("src").join("skills"), &mut skills);

    // 2. Scan <project_root>/skills/
    scan_skills_dir(&project_root.join("skills"), &mut skills);

    // 3. Scan <project_root>/.norvexum/skills/
    scan_skills_dir(&project_root.join(".norvexum").join("skills"), &mut skills);

    // 4. Scan ~/.norvexum/skills/
    if let Some(home) = get_home_dir() {
        scan_skills_dir(&home.join(".norvexum").join("skills"), &mut skills);
    }

    skills
}

/// Load default and all custom skills, checking trigger patterns against query.
pub fn find_matching_skill(query: &str, project_root: &Path) -> Option<Skill> {
    let skills = load_all_skills(project_root);
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
