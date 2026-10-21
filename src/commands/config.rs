// ===========================================================================
// Commands: config — List, get, set config settings
// ===========================================================================

use crate::cli::ConfigAction;
use crate::config::Settings;
use eyre::Result;

/// Keys that `config get` / `config set` understand, in display order.
const KEYS: &[&str] = &[
    "active_provider",
    "active_model",
    "thinking_effort",
    "max_thinking_loops",
    "max_content_chars",
    "browser_timeout_secs",
    "ask_perms",
    "require_approval",
    "ollama_base_url",
    "project_root",
];

pub async fn run(settings: &Settings, action: ConfigAction) -> Result<()> {
    match action {
        ConfigAction::List => {
            println!("Norvexum configuration (~/.norvexum/config.toml):\n");
            for key in KEYS {
                println!(
                    "  {:<22} = {}",
                    key,
                    get_value(settings, key).unwrap_or_default()
                );
            }
            println!("\nAPI keys:");
            let keys = [
                ("OPENAI_API_KEY", &settings.openai_api_key),
                ("ANTHROPIC_API_KEY", &settings.anthropic_api_key),
                ("GOOGLE_AI_API_KEY", &settings.google_ai_api_key),
                ("AICREDITS_API_KEY", &settings.aicredits_api_key),
                ("TAVILY_API_KEY", &settings.tavily_api_key),
                ("OCR_SPACE_API_KEY", &settings.ocr_space_api_key),
                ("POLLINATIONS_API_KEY", &settings.pollinations_api_key),
            ];
            for (name, value) in keys {
                println!(
                    "  {:<22} = {}",
                    name,
                    if value.is_some() { "set" } else { "not set" }
                );
            }
            println!(
                "\nKeys are read from the environment, ~/.norvexum/.env, ./.env, or the OS keyring."
            );
        }
        ConfigAction::Get { key } => match get_value(settings, &key) {
            Some(value) => println!("{value}"),
            None => {
                return Err(eyre::eyre!(
                    "Unknown configuration key '{key}'. Known keys: {}",
                    KEYS.join(", ")
                ));
            }
        },
        ConfigAction::Set { key, value } => {
            let mut current = settings.clone();
            set_value(&mut current, &key, &value)?;
            current.save()?;
            println!(
                "✅ {key} = {}",
                get_value(&current, &key).unwrap_or_default()
            );
        }
    }
    Ok(())
}

fn get_value(s: &Settings, key: &str) -> Option<String> {
    Some(match key {
        "active_provider" => s.active_provider.clone(),
        "active_model" => s.active_model.clone(),
        "thinking_effort" => s.thinking_effort.clone().unwrap_or_else(|| "none".into()),
        "max_thinking_loops" => s.max_thinking_loops.to_string(),
        "max_content_chars" => s.max_content_chars.to_string(),
        "browser_timeout_secs" => s.browser_timeout_secs.to_string(),
        "ask_perms" => s.ask_perms.to_string(),
        "require_approval" => s.require_approval.join(","),
        "ollama_base_url" => s
            .ollama_base_url
            .clone()
            .unwrap_or_else(|| crate::ai::ollama::DEFAULT_OLLAMA_URL.into()),
        "project_root" => s.project_root.display().to_string(),
        _ => return None,
    })
}

fn set_value(s: &mut Settings, key: &str, value: &str) -> Result<()> {
    let int = |v: &str| {
        v.parse::<usize>()
            .map_err(|_| eyre::eyre!("'{key}' must be a non-negative integer"))
    };
    match key {
        "active_provider" => {
            if value != "ollama" && crate::config::providers::find_provider(value).is_none() {
                let known: Vec<String> = crate::config::providers::build_registry()
                    .into_iter()
                    .map(|p| p.name)
                    .collect();
                eyre::bail!(
                    "Unknown provider '{value}'. Known: {}, ollama",
                    known.join(", ")
                );
            }
            s.active_provider = value.to_string();
        }
        "active_model" => s.active_model = value.to_string(),
        "thinking_effort" => {
            s.thinking_effort = match value {
                "none" | "" => None,
                "low" | "medium" | "high" | "xhigh" | "max" | "adaptive" => Some(value.to_string()),
                _ => {
                    return Err(eyre::eyre!(
                        "thinking_effort must be one of: none, low, medium, high, xhigh, max, adaptive"
                    ));
                }
            }
        }
        "max_thinking_loops" => s.max_thinking_loops = int(value)?,
        "max_content_chars" => s.max_content_chars = int(value)?.max(1000),
        "browser_timeout_secs" => s.browser_timeout_secs = int(value)? as u64,
        "ask_perms" => {
            s.ask_perms = match value {
                "true" | "on" | "yes" | "1" => true,
                "false" | "off" | "no" | "0" => false,
                _ => return Err(eyre::eyre!("ask_perms must be true or false")),
            }
        }
        "require_approval" => {
            s.require_approval = value
                .split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
                .collect();
        }
        "ollama_base_url" => s.ollama_base_url = Some(value.trim_end_matches('/').to_string()),
        "project_root" => {
            return Err(eyre::eyre!(
                "project_root is derived from the directory you run in"
            ));
        }
        _ => {
            return Err(eyre::eyre!(
                "Unknown configuration key '{key}'. Known keys: {}",
                KEYS.join(", ")
            ));
        }
    }
    Ok(())
}
