// ===========================================================================
// Checkpoint — File-level undo/rollback via pre-write snapshots
//
// Before any destructive file operation (write, edit), the original file
// content is snapshotted into .norvexum/checkpoints/<timestamp>/files/.
// Files that did not exist yet are recorded too, so /undo can delete them.
// The /undo command restores the most recent checkpoint.
// ===========================================================================

use eyre::Result;
use std::path::{Path, PathBuf};

use crate::config;

const MANIFEST: &str = "_manifest.json";
const FILES_DIR: &str = "files";
/// Older checkpoints beyond this count are deleted.
const MAX_CHECKPOINTS: usize = 50;

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Manifest {
    timestamp: String,
    /// Project-relative paths that were snapshotted.
    #[serde(default)]
    files: Vec<String>,
    /// Project-relative paths that did not exist before the write.
    #[serde(default)]
    created: Vec<String>,
}

/// Snapshot a file before modification. Returns the checkpoint directory.
pub fn snapshot_file(project_root: &Path, file_path: &Path) -> Result<PathBuf> {
    let root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let full = if file_path.exists() {
        file_path.canonicalize()?
    } else {
        file_path.to_path_buf()
    };

    // Only project files are snapshotted. Joining an absolute path onto the
    // checkpoint dir would yield the original path and copy it onto itself.
    let Ok(relative) = full
        .strip_prefix(&root)
        .or_else(|_| file_path.strip_prefix(project_root))
        .map(Path::to_path_buf)
    else {
        eyre::bail!(
            "{} is outside the project; not snapshotted",
            file_path.display()
        );
    };

    let checkpoint_dir = next_checkpoint_dir(project_root)?;
    let rel_str = relative.to_string_lossy().replace('\\', "/");
    let mut manifest = Manifest {
        timestamp: chrono::Utc::now().to_rfc3339(),
        ..Manifest::default()
    };

    if full.is_file() {
        let snapshot_path = checkpoint_dir.join(FILES_DIR).join(&relative);
        if let Some(parent) = snapshot_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&full, &snapshot_path)?;
        manifest.files.push(rel_str);
    } else {
        manifest.created.push(rel_str);
    }

    std::fs::write(
        checkpoint_dir.join(MANIFEST),
        serde_json::to_string_pretty(&manifest)?,
    )?;
    prune_old_checkpoints(project_root);
    Ok(checkpoint_dir)
}

/// Undo the most recent checkpoint — restore all files from the last snapshot.
/// Returns a list of restored (or removed) file paths.
pub fn undo_last(project_root: &Path) -> Result<Vec<String>> {
    let dirs = checkpoint_dirs(project_root);
    let latest = dirs
        .last()
        .ok_or_else(|| eyre::eyre!("No checkpoints found — nothing to undo"))?;

    let manifest: Manifest = std::fs::read_to_string(latest.join(MANIFEST))
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default();

    let mut restored = Vec::new();
    for rel in &manifest.files {
        let source = latest.join(FILES_DIR).join(rel);
        let target = project_root.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&source, &target)?;
        restored.push(rel.clone());
    }
    for rel in &manifest.created {
        let target = project_root.join(rel);
        if target.is_file() {
            std::fs::remove_file(&target)?;
            restored.push(format!("{rel} (removed — it was newly created)"));
        }
    }

    // Checkpoints written by older versions stored files at the top level.
    if manifest.files.is_empty() && manifest.created.is_empty() {
        for entry in walkdir::WalkDir::new(latest)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() || entry.file_name() == MANIFEST {
                continue;
            }
            let relative = entry.path().strip_prefix(latest).unwrap_or(entry.path());
            let target = project_root.join(relative);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &target)?;
            restored.push(relative.to_string_lossy().to_string());
        }
    }

    std::fs::remove_dir_all(latest)?;
    Ok(restored)
}

/// Whether any checkpoint exists (used by the GUI to enable "Undo").
pub fn has_checkpoints(project_root: &Path) -> bool {
    !checkpoint_dirs(project_root).is_empty()
}

fn checkpoints_dir(root: &Path) -> PathBuf {
    root.join(config::NORVEXUM_DIR).join("checkpoints")
}

/// Checkpoint directories, oldest first (names are sortable timestamps).
fn checkpoint_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(checkpoints_dir(root))
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs
}

fn next_checkpoint_dir(root: &Path) -> Result<PathBuf> {
    let base = checkpoints_dir(root);
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S_%6f").to_string();
    // Parallel writes can land in the same microsecond.
    let mut dir = base.join(&timestamp);
    let mut n = 1;
    while dir.exists() {
        dir = base.join(format!("{timestamp}_{n}"));
        n += 1;
    }
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn prune_old_checkpoints(root: &Path) {
    let dirs = checkpoint_dirs(root);
    if dirs.len() > MAX_CHECKPOINTS {
        for dir in &dirs[..dirs.len() - MAX_CHECKPOINTS] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_restores_underscore_files_and_removes_new_ones() {
        let root = std::env::temp_dir().join(format!("nvx-ckpt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        let init = root.join("pkg").join("__init__.py");
        std::fs::write(&init, "original").unwrap();

        snapshot_file(&root, &init).unwrap();
        std::fs::write(&init, "changed").unwrap();
        let restored = undo_last(&root).unwrap();
        assert_eq!(std::fs::read_to_string(&init).unwrap(), "original");
        assert_eq!(restored.len(), 1);

        let new_file = root.join("new.txt");
        snapshot_file(&root, &new_file).unwrap();
        std::fs::write(&new_file, "fresh").unwrap();
        undo_last(&root).unwrap();
        assert!(!new_file.exists());
        assert!(!has_checkpoints(&root));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refuses_paths_outside_project() {
        let root = std::env::temp_dir().join(format!("nvx-ckpt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let outside =
            std::env::temp_dir().join(format!("nvx-outside-{}.txt", uuid::Uuid::new_v4()));
        std::fs::write(&outside, "keep me").unwrap();
        assert!(snapshot_file(&root, &outside).is_err());
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep me");
        let _ = std::fs::remove_file(outside);
        std::fs::remove_dir_all(root).unwrap();
    }
}
