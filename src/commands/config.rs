// ═══════════════════════════════════════════════════════════════════════════
// Commands: config — List, get, set config settings
// ═══════════════════════════════════════════════════════════════════════════

use crate::ConfigAction;
use crate::config::Settings;
use eyre::Result;

pub async fn run(settings: &Settings, action: ConfigAction) -> Result<()> {
    match action {
        ConfigAction::List => {
            println!("Norvexum Configuration:\n");
            println!("  active_provider      = {}", settings.active_provider);
            println!("  active_model         = {}", settings.active_model);
            println!("  browser_timeout_secs = {}", settings.browser_timeout_secs);
            println!("  max_thinking_loops   = {}", settings.max_thinking_loops);
            println!("  max_content_chars    = {}", settings.max_content_chars);
            println!(
                "  project_root         = {}",
                settings.project_root.display()
            );
            println!("\nAPI Keys Status:");
            println!(
                "  GOOGLE_AI_API_KEY    = {}",
                check_key(&settings.google_ai_api_key)
            );
            println!(
                "  AICREDITS_API_KEY    = {}",
                check_key(&settings.aicredits_api_key)
            );
            println!(
                "  OPENAI_API_KEY       = {}",
                check_key(&settings.openai_api_key)
            );
            println!(
                "  ANTHROPIC_API_KEY    = {}",
                check_key(&settings.anthropic_api_key)
            );
            println!(
                "  TAVILY_API_KEY       = {}",
                check_key(&settings.tavily_api_key)
            );
            println!(
                "  OCR_SPACE_API_KEY    = {}",
                check_key(&settings.ocr_space_api_key)
            );
        }
        ConfigAction::Get { key } => match key.as_str() {
            "active_provider" => println!("{}", settings.active_provider),
            "active_model" => println!("{}", settings.active_model),
            "browser_timeout_secs" => println!("{}", settings.browser_timeout_secs),
            "max_thinking_loops" => println!("{}", settings.max_thinking_loops),
            "max_content_chars" => println!("{}", settings.max_content_chars),
            _ => eyre::bail!("Unknown configuration key: {}", key),
        },
        ConfigAction::Set { key, value } => {
            let mut current = settings.clone();
            match key.as_str() {
                "active_provider" => current.active_provider = value.clone(),
                "active_model" => current.active_model = value.clone(),
                "browser_timeout_secs" => {
                    let val = value
                        .parse::<u64>()
                        .map_err(|_| eyre::eyre!("Must be an integer"))?;
                    current.browser_timeout_secs = val;
                }
                "max_thinking_loops" => {
                    let val = value
                        .parse::<usize>()
                        .map_err(|_| eyre::eyre!("Must be an integer"))?;
                    current.max_thinking_loops = val;
                }
                "max_content_chars" => {
                    let val = value
                        .parse::<usize>()
                        .map_err(|_| eyre::eyre!("Must be an integer"))?;
                    current.max_content_chars = val;
                }
                _ => eyre::bail!("Configuration key '{}' cannot be set or is read-only", key),
            }
            current.save()?;
            println!("✅ Configuration updated: {} = {}", key, value);
        }
    }
    Ok(())
}

fn check_key(key: &Option<String>) -> &'static str {
    if key.is_some() { "SET" } else { "NOT SET" }
}
