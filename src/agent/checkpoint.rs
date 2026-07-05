// ═══════════════════════════════════════════════════════════════════════════
// Checkpoint — File-level undo/rollback via pre-write snapshots
//
// Before any destructive file operation (write, edit, delete), the original
// file content is snapshotted into .norvexum/checkpoints/<timestamp>/.
// The /undo command restores the most recent checkpoint.
// ═══════════════════════════════════════════════════════════════════════════

use eyre::Result;
use std::path::{Path, PathBuf};

use crate::config;

/// Snapshot a file before modification. Returns the checkpoint directory.
pub fn snapshot_file(project_root: &Path, file_path: &Path) -> Result<PathBuf> {
    if !file_path.exists() {
        // Nothing to snapshot — file is being created
        return Ok(PathBuf::new());
    }

    let checkpoint_dir = next_checkpoint_dir(project_root)?;

    // Preserve relative path structure inside the checkpoint
    let relative = file_path.strip_prefix(project_root).unwrap_or(file_path);
    let snapshot_path = checkpoint_dir.join(relative);

    if let Some(parent) = snapshot_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::copy(file_path, &snapshot_path)?;

    // Write a manifest of what was snapshotted
    let manifest_path = checkpoint_dir.join("_manifest.json");
    let manifest = serde_json::json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "files": [relative.to_string_lossy()],
        "operation": "snapshot_before_write",
    });
    std::fs::write(manifest_path, serde_json::to_string_pretty(&manifest)?)?;

    Ok(checkpoint_dir)
}

/// Undo the most recent checkpoint — restore all files from the last snapshot.
/// Returns a list of restored file paths.
pub fn undo_last(project_root: &Path) -> Result<Vec<String>> {
    let checkpoints = checkpoints_dir(project_root);
    if !checkpoints.exists() {
        eyre::bail!("No checkpoints found — nothing to undo");
    }

    // Find the most recent checkpoint directory
    let mut dirs: Vec<_> = std::fs::read_dir(&checkpoints)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .collect();

    dirs.sort_by_key(|e| std::cmp::Reverse(e.file_name()));

    let latest = dirs
        .first()
        .ok_or_else(|| eyre::eyre!("No checkpoint directories found"))?;

    let checkpoint_path = latest.path();
    let mut restored = Vec::new();

    // Walk the checkpoint and restore files
    for entry in walkdir::WalkDir::new(&checkpoint_path)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }

        let name = entry.file_name().to_string_lossy();
        if name.starts_with('_') {
            continue; // Skip manifest
        }

        let relative = entry
            .path()
            .strip_prefix(&checkpoint_path)
            .unwrap_or(entry.path());
        let target = project_root.join(relative);

        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }

        std::fs::copy(entry.path(), &target)?;
        restored.push(relative.to_string_lossy().to_string());
    }

    // Remove the used checkpoint
    std::fs::remove_dir_all(&checkpoint_path)?;

    Ok(restored)
}

/// List available checkpoints.
pub fn list_checkpoints(project_root: &Path) -> Vec<String> {
    let checkpoints = checkpoints_dir(project_root);
    if !checkpoints.exists() {
        return Vec::new();
    }

    let mut dirs: Vec<_> = std::fs::read_dir(&checkpoints)
        .ok()
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();

    dirs.sort();
    dirs.reverse();
    dirs
}

fn checkpoints_dir(root: &Path) -> PathBuf {
    root.join(config::NORVEXUM_DIR).join("checkpoints")
}

fn next_checkpoint_dir(root: &Path) -> Result<PathBuf> {
    let base = checkpoints_dir(root);
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S_%3f").to_string();
    let dir = base.join(&timestamp);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
