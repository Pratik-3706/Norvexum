use async_trait::async_trait;
use serde_json::json;

use super::{Tool, ToolContext, ToolResult};

// ── ListSkillsTool ────────────────────────────────────────────────────────

pub struct ListSkillsTool;

#[async_trait]
impl Tool for ListSkillsTool {
    fn name(&self) -> &str {
        "list_skills"
    }

    fn description(&self) -> &str {
        "List all available specialized expert skills and their descriptions. Use this to discover which skills are available for the task."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let skills = crate::skills::load_all_skills(&ctx.settings.project_root);
        if skills.is_empty() {
            return ToolResult::ok("No specialized skills are currently configured.");
        }

        let mut lines = vec!["Available Specialized Skills:".to_string()];
        for skill in &skills {
            lines.push(format!(
                "- **{}**: {} (Triggers on: {})",
                skill.name,
                skill.description,
                skill.trigger_patterns.join(", ")
            ));
        }

        ToolResult::ok_with_data(
            lines.join("\n"),
            json!({ "skills": skills.iter().map(|s| s.name.clone()).collect::<Vec<_>>() })
        )
    }
}

// ── ReadSkillTool ─────────────────────────────────────────────────────────

pub struct ReadSkillTool;

#[async_trait]
impl Tool for ReadSkillTool {
    fn name(&self) -> &str {
        "read_skill"
    }

    fn description(&self) -> &str {
        "Read the full guidelines, system instructions, and templates of a specific specialized skill."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The exact name of the skill to read (e.g. 'Frontend Specialist' or 'Backend Engineer')"
                }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let name = args["name"].as_str().unwrap_or("").to_lowercase();
        if name.is_empty() {
            return ToolResult::err("Skill name parameter cannot be empty.");
        }

        let skills = crate::skills::load_all_skills(&ctx.settings.project_root);
        let found = skills.iter().find(|s| s.name.to_lowercase() == name);

        match found {
            Some(skill) => {
                ToolResult::ok_with_data(
                    format!(
                        "--- Skill: {} ---\nDescription: {}\n\nSystem Instructions:\n{}",
                        skill.name, skill.description, skill.system_instructions
                    ),
                    json!({
                        "name": skill.name,
                        "description": skill.description,
                        "instructions": skill.system_instructions
                    })
                )
            }
            None => {
                let available: Vec<_> = skills.iter().map(|s| s.name.clone()).collect();
                ToolResult::err(format!(
                    "Skill '{}' not found. Available skills: {:?}",
                    name, available
                ))
            }
        }
    }
}
