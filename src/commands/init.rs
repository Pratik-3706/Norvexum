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

    // Copy .env configuration dynamically from home folder fallback, executable folder, or original build workspace
    let env_path = project_dir.join(".env");
    if !env_path.exists() {
        let mut copied = false;

        // 1. Try to copy from original build workspace directory (compile-time dynamic, no hardcoding in source file)
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let build_env_path = std::path::Path::new(manifest_dir).join(".env");
        if build_env_path.exists() {
            if fs::copy(&build_env_path, &env_path).is_ok() {
                println!(
                    "✅ Automatically copied .env configuration from build workspace: {}",
                    build_env_path.display()
                );
                copied = true;
            }
        }

        // 2. Try to copy from global home directory fallback (~/.norvexum/.env) - completely dynamic for any user
        if !copied {
            if let Some(home) = dirs::home_dir() {
                let global_env = home.join(".norvexum").join(".env");
                if global_env.exists() {
                    if fs::copy(&global_env, &env_path).is_ok() {
                        println!(
                            "✅ Automatically copied global .env fallback from your user home profile."
                        );
                        copied = true;
                    }
                }
            }
        }

        // 3. Try to copy from directory of the executable itself
        if !copied {
            if let Ok(exe_path) = std::env::current_exe() {
                if let Some(exe_dir) = exe_path.parent() {
                    let exe_env = exe_dir.join(".env");
                    if exe_env.exists() {
                        if fs::copy(&exe_env, &env_path).is_ok() {
                            println!("✅ Automatically copied .env from executable folder.");
                            copied = true;
                        }
                    }
                }
            }
        }

        // 3. Fallback: write a blank template
        if !copied {
            let env_example_path = project_dir.join(".env.example");
            if env_example_path.exists() {
                let _ = fs::copy(&env_example_path, &env_path);
                println!("Created default .env file from .env.example (please add your API keys).");
            } else {
                let template = "# Norvexum API Keys\nGOOGLE_AI_API_KEY=\nAICREDITS_API_KEY=\nTAVILY_API_KEY=\n";
                let _ = fs::write(&env_path, template);
                println!("Created default .env template file.");
            }
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
