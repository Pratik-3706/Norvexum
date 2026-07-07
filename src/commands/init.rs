// ═══════════════════════════════════════════════════════════════════════════
// Commands: init — Initializes Norvexum project directory and templates
// ═══════════════════════════════════════════════════════════════════════════

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

    // Save initial configuration
    settings
        .save()
        .wrap_err("Failed to save initial configuration")?;
    println!(
        "Created configuration: {}",
        norvexum_path.join(CONFIG_FILE).display()
    );

    // Generate initial project context
    let context = ProjectContext::scan(project_dir);
    context
        .save(project_dir)
        .wrap_err("Failed to save initial project context")?;
    println!("Created project context index.");

    // Create .env configuration template dynamically
    let env_path = project_dir.join(".env");
    if !env_path.exists() {
        let env_example_path = project_dir.join(".env.example");
        if env_example_path.exists() {
            let _ = fs::copy(&env_example_path, &env_path);
            println!("Created default .env file from .env.example (please add your API keys).");
        } else {
            let template = "# Norvexum API Keys\nGOOGLE_AI_API_KEY=\nAICREDITS_API_KEY=\nOPENAI_API_KEY=\nANTHROPIC_API_KEY=\nTAVILY_API_KEY=\nOCR_SPACE_API_KEY=\nPOLLINATIONS_API_KEY=\n";
            let _ = fs::write(&env_path, template);
            println!("Created default .env template file.");
        }
    }

    // Add to gitignore if git repo is present
    let gitignore_path = project_dir.join(".gitignore");
    if gitignore_path.exists() {
        let content = fs::read_to_string(&gitignore_path)?;
        if !content.contains(".norvexum") {
            let mut file = fs::OpenOptions::new().append(true).open(&gitignore_path)?;
            use std::io::Write;
            writeln!(file, "\n# Norvexum local data\n.norvexum/\n.env")?;
            println!("Added .norvexum/ and .env to .gitignore.");
        }
    }

    println!("✅ Norvexum initialized successfully!");
    Ok(())
}
