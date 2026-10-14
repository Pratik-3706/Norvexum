// ===========================================================================
// Skills — Prebuilt and Custom extensible behavioral templates
//
// Triggers the agent with specialized instructions for specific tasks.
// Loads markdown files with YAML frontmatter from:
//   - <project_root>/skills/
//   - <project_root>/.norvexum/skills/
//   - ~/.norvexum/skills/
// ===========================================================================

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

/// Skills shipped inside the binary, so they work in every project (they used
/// to be read from `<project>/src/skills`, which only existed in this repo).
const BUILTIN_SKILL_FILES: &[&str] = &[
    include_str!("backend/SKILL.md"),
    include_str!("debugging/SKILL.md"),
    include_str!("documentation/SKILL.md"),
    include_str!("find_free_apis/SKILL.md"),
    include_str!("frontend/SKILL.md"),
    include_str!("planning/SKILL.md"),
    include_str!("presentation/SKILL.md"),
    include_str!("selfknowladge/SKILL.md"),
    include_str!("tavily_skills/SKILL.md"),
    include_str!("testing/SKILL.md"),
    include_str!("ux_copywriting/SKILL.md"),
    include_str!("visual_design_system/SKILL.md"),
];

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

/// Parse a markdown file with YAML frontmatter into a Skill
pub fn parse_skill_markdown(content: &str) -> Option<Skill> {
    let content = content.trim_start_matches('\u{feff}');
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

    for line in lines {
        if in_frontmatter {
            let trimmed = line.trim();
            if trimmed == "---" {
                in_frontmatter = false;
                continue;
            }
            if trimmed.starts_with('-') {
                let item = trimmed.trim_start_matches('-').trim();
                let unquoted = item.trim_matches('"').trim_matches('\'').to_lowercase();
                if current_key == "trigger_patterns" && !unquoted.is_empty() {
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
                            .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_lowercase())
                            .filter(|s| !s.is_empty())
                            .collect();
                        trigger_patterns.extend(parsed);
                    } else {
                        trigger_patterns.push(v.to_lowercase());
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

/// Load built-in skills, then custom ones from the project and home folders.
/// A custom skill with the same name (case-insensitive) replaces the built-in.
pub fn load_all_skills(project_root: &Path) -> Vec<Skill> {
    let mut skills = load_default_skills();
    skills.extend(
        BUILTIN_SKILL_FILES
            .iter()
            .filter_map(|c| parse_skill_markdown(c)),
    );

    let mut custom = Vec::new();
    scan_skills_dir(&project_root.join("skills"), &mut custom);
    scan_skills_dir(&project_root.join(".norvexum").join("skills"), &mut custom);
    scan_skills_dir(&get_global_skills_dir(), &mut custom);

    for skill in custom {
        let key = skill.name.to_lowercase();
        match skills.iter_mut().find(|s| s.name.to_lowercase() == key) {
            Some(existing) => *existing = skill,
            None => skills.push(skill),
        }
    }
    skills
}

/// Does `pattern` occur in `text` as whole words? ("api" matches "REST api"
/// but not "rapid"; "unit test" matches "add a unit test".)
fn matches_words(text: &str, pattern: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = text[start..].find(pattern) {
        let begin = start + pos;
        let end = begin + pattern.len();
        let before_ok = text[..begin]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let after_ok = text[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        start = begin + text[begin..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// Find the skill whose trigger best matches the query (longest matching
/// trigger wins, so "unit test" beats "test").
pub fn find_matching_skill(query: &str, project_root: &Path) -> Option<Skill> {
    if query.trim_start().starts_with('/') {
        return None;
    }
    let query_lower = query.to_lowercase();
    load_all_skills(project_root)
        .into_iter()
        .filter_map(|skill| {
            let best = skill
                .trigger_patterns
                .iter()
                .filter(|p| !p.is_empty() && matches_words(&query_lower, p))
                .map(|p| p.len())
                .max()?;
            Some((best, skill))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, skill)| skill)
}

/// Helper to get the global skills folder (~/.norvexum/skills)
pub fn get_global_skills_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".norvexum")
        .join("skills")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_skills_are_embedded() {
        let skills = load_all_skills(Path::new("definitely-not-a-real-dir"));
        assert!(skills.len() >= 14, "got {}", skills.len());
        assert!(skills.iter().any(|s| s.name == "Frontend Specialist"));
    }

    #[test]
    fn triggers_match_whole_words_only() {
        assert!(matches_words("build a rest api please", "api"));
        assert!(!matches_words("make it rapid", "api"));
        assert!(!matches_words("explain this", "plan"));
        assert!(!matches_words("get the latest version", "test"));
        assert!(matches_words("write a unit test", "unit test"));
        assert!(matches_words("café api", "api"));
    }

    #[test]
    fn longest_trigger_wins() {
        let skill =
            find_matching_skill("please add a unit test for this", Path::new("nope")).unwrap();
        assert_eq!(skill.name, "Testing Engineer");
        assert!(find_matching_skill("/help", Path::new("nope")).is_none());
    }
}
