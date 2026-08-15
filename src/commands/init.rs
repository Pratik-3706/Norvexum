// ===========================================================================
// Commands: init — Initializes Norvexum project directory and templates
// ===========================================================================

use crate::agent::history::ProjectContext;
use crate::config::{CONFIG_FILE, HISTORY_DIR, NORVEXUM_DIR, Settings, VENVS_DIR};
use eyre::{Result, WrapErr};
use std::fs;

pub async fn run(settings: &Settings) -> Result<()> {
    let project_dir = &settings.project_root;
    let norvexum_path = project_dir.join(NORVEXUM_DIR);

    if norvexum_path.exists() {
        println!(
            "Norvexum is already initialized in {}",
            project_dir.display()
        );
        return Ok(());
    }

    println!("Initializing Norvexum in {}...", project_dir.display());

    // Create .norvexum/ directory structure
    fs::create_dir_all(&norvexum_path).wrap_err("Failed to create .norvexum directory")?;
    fs::create_dir_all(norvexum_path.join(HISTORY_DIR))
        .wrap_err("Failed to create history directory")?;
    fs::create_dir_all(norvexum_path.join(VENVS_DIR))
        .wrap_err("Failed to create venvs directory")?;

    // I no longer save settings.json here because it is global.
    // Instead I just ensure the global config directory exists.
    let global_config_dir = dirs::home_dir()
        .map(|h| h.join(".norvexum"))
        .unwrap_or_else(|| project_dir.join(NORVEXUM_DIR));
    let _ = fs::create_dir_all(&global_config_dir);

    // Generate initial project context
    let context = ProjectContext::scan(project_dir);
    context
        .save(project_dir)
        .wrap_err("Failed to save initial project context")?;
    println!("Created project context index.");

    // I no longer create a local .env file because API keys are stored globally.

    // Add to gitignore if git repo is present
    let gitignore_path = project_dir.join(".gitignore");
    if gitignore_path.exists() {
        let content = fs::read_to_string(&gitignore_path)?;
        if !content.contains(".norvexum") {
            let mut file = fs::OpenOptions::new().append(true).open(&gitignore_path)?;
            use std::io::Write;
            writeln!(file, "\n# Norvexum local data\n.norvexum/")?;
            println!("Added .norvexum/ to .gitignore.");
        }
    }

    println!("✅ Norvexum initialized successfully!");
    Ok(())
}
