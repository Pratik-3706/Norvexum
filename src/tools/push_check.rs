// ===========================================================================
// Push safety check — what git_push looks at before anything is uploaded
//
// Runs on exactly the commits a push would send, plus the repository root:
//   * secret files in the outgoing commits (.env, private keys, …)  → block
//   * lines that look like API keys or tokens                        → block
//   * values from the user's .env files, if they allowed reading them → block
//   * files over GitHub's 100 MB limit                               → block
//   * files over 10 MB                                               → warn
//   * secret files in the root that .gitignore doesn't cover         → warn
//   * commits whose author email isn't a GitHub noreply address      → warn
// Findings name the file, line and kind of secret, never the secret itself,
// and .env values never leave this module.
// ===========================================================================

use std::path::Path;
use std::sync::LazyLock;

/// How serious a finding is: `Block` stops the push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Block,
    Warn,
    /// For the user only (shown before the push, not reported to the agent).
    Note,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub level: Level,
    pub text: String,
}

impl Finding {
    fn block(text: impl Into<String>) -> Self {
        Self {
            level: Level::Block,
            text: text.into(),
        }
    }
    fn warn(text: impl Into<String>) -> Self {
        Self {
            level: Level::Warn,
            text: text.into(),
        }
    }
    fn note(text: impl Into<String>) -> Self {
        Self {
            level: Level::Note,
            text: text.into(),
        }
    }
}

/// GitHub rejects files over this size.
const GITHUB_FILE_LIMIT: u64 = 100 * 1024 * 1024;
/// Worth a word before it goes into the history for good.
const LARGE_FILE: u64 = 10 * 1024 * 1024;
/// Patches past this size are only checked up to here.
const MAX_PATCH_BYTES: usize = 30 * 1024 * 1024;

/// Kinds of keys and tokens, recognised by their published formats.
static KEY_PATTERNS: LazyLock<Vec<(&'static str, regex::Regex)>> = LazyLock::new(|| {
    [
        ("an Anthropic API key", r"sk-ant-[A-Za-z0-9_\-]{20,}"),
        (
            "an OpenAI-style API key",
            r"\bsk-(?:proj-)?[A-Za-z0-9_\-]{20,}",
        ),
        ("a Google API key", r"\bAIza[0-9A-Za-z_\-]{35}"),
        (
            "a GitHub token",
            r"\b(?:gh[pousr]_[A-Za-z0-9]{36}|github_pat_[A-Za-z0-9_]{22,})",
        ),
        ("an AWS access key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
        ("a Slack token", r"\bxox[abprs]-[A-Za-z0-9\-]{10,}"),
        ("a Stripe secret key", r"\b(?:sk|rk)_live_[0-9A-Za-z]{20,}"),
        ("a Hugging Face token", r"\bhf_[A-Za-z0-9]{30,}"),
        ("a GitLab token", r"\bglpat-[A-Za-z0-9_\-]{20,}"),
        ("a private key", r"-----BEGIN (?:[A-Z]+ )*PRIVATE KEY-----"),
    ]
    .into_iter()
    .map(|(kind, pattern)| (kind, regex::Regex::new(pattern).expect("valid key pattern")))
    .collect()
});

/// Whether a file name is one that holds secrets.
pub fn is_secret_file(path: &str) -> bool {
    let name = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_ascii_lowercase();
    if is_env_file(&name) {
        return true;
    }
    let exact = [
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
        "id_ed25519",
        "credentials.json",
        ".netrc",
        ".pgpass",
        "secrets.json",
        "secrets.toml",
    ];
    let suffixes = [".pem", ".key", ".p12", ".pfx", ".keystore", ".jks", ".ppk"];
    exact.contains(&name.as_str())
        || suffixes.iter().any(|s| name.ends_with(s))
        || (name.starts_with("service-account") && name.ends_with(".json"))
}

/// `.env`, `.env.local`, `.env.production`, … but not the shared templates.
fn is_env_file(name: &str) -> bool {
    let templates = [".example", ".sample", ".template", ".dist", ".defaults"];
    (name == ".env" || name.starts_with(".env.")) && !templates.iter().any(|t| name.ends_with(t))
}

/// The .env files in the project root that hold at least one value.
pub fn env_files(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|name| is_env_file(&name.to_ascii_lowercase()))
        .filter(|name| !env_values(&root.join(name)).is_empty())
        .collect();
    names.sort();
    names
}

/// (KEY, value) pairs from a .env file worth looking for: real values, not
/// placeholders, flags or short settings.
fn env_values(path: &Path) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let line = line.strip_prefix("export ").unwrap_or(line);
            if line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            let value = value.trim().trim_matches(['"', '\'']).trim();
            is_secret_value(value).then(|| (key.trim().to_string(), value.to_string()))
        })
        .collect()
}

fn is_secret_value(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let placeholder = [
        "your",
        "example",
        "changeme",
        "placeholder",
        "xxxx",
        "<",
        "todo",
    ];
    value.len() >= 8
        && !value.chars().all(|c| c.is_ascii_digit())
        && !placeholder.iter().any(|p| lower.contains(p))
        && !lower.starts_with("http://localhost")
        && !lower.starts_with("http://127.0.0.1")
}

/// Run the whole check. `range` selects the outgoing commits as git log
/// arguments (e.g. ["origin/main..main"]), `tip` is the branch being pushed,
/// and `read_env` says whether the user allowed reading their .env values.
pub async fn scan(root: &Path, range: &[String], tip: &str, read_env: bool) -> Vec<Finding> {
    let mut findings = Vec::new();
    let range: Vec<&str> = range.iter().map(String::as_str).collect();

    // Files the commits touch.
    let mut args = vec!["log", "--name-only", "--format="];
    args.extend(&range);
    let files: Vec<String> = git(root, &args)
        .await
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    for file in files.iter().filter(|f| is_secret_file(f)) {
        findings.push(Finding::block(format!(
            "`{file}` is in the commits being pushed, and files like it hold secrets."
        )));
    }

    // Lines the commits add: key formats, and the user's own .env values.
    let env: Vec<(String, String, String)> = if read_env {
        env_files(root)
            .into_iter()
            .flat_map(|name| {
                env_values(&root.join(&name))
                    .into_iter()
                    .map(move |(k, v)| (name.clone(), k, v))
            })
            .collect()
    } else {
        Vec::new()
    };
    let mut args = vec!["log", "-p", "--no-ext-diff", "--format=", "--unified=0"];
    args.extend(&range);
    let patch = git(root, &args).await.unwrap_or_default();
    let patch = &patch[..floor_char_boundary(&patch, MAX_PATCH_BYTES)];
    findings.extend(scan_patch(patch, &env));

    // Sizes of the touched files at the branch tip.
    let listing = git(root, &["ls-tree", "-r", "-l", tip])
        .await
        .unwrap_or_default();
    for line in listing.lines() {
        let Some((meta, path)) = line.split_once('\t') else {
            continue;
        };
        if !files.iter().any(|f| f == path) {
            continue;
        }
        let Some(size) = meta
            .split_whitespace()
            .nth(3)
            .and_then(|s| s.parse::<u64>().ok())
        else {
            continue;
        };
        let mb = size as f64 / (1024.0 * 1024.0);
        if size > GITHUB_FILE_LIMIT {
            findings.push(Finding::block(format!(
                "`{path}` is {mb:.0} MB; GitHub rejects files over 100 MB (Git LFS handles big files)."
            )));
        } else if size > LARGE_FILE {
            findings.push(Finding::warn(format!(
                "`{path}` is {mb:.0} MB and will stay in the history for good."
            )));
        }
    }

    // Secret files in the root that .gitignore doesn't cover.
    let root_files = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|name| is_secret_file(name));
    for name in root_files {
        let ignored = git(root, &["check-ignore", "-q", &name]).await.is_ok();
        if !ignored {
            findings.push(Finding::warn(format!(
                "`{name}` isn't in .gitignore, so it could be committed by accident. Add it to .gitignore."
            )));
        }
    }

    // Author emails.
    let mut args = vec!["log", "--format=%ae"];
    args.extend(&range);
    let personal = git(root, &args)
        .await
        .unwrap_or_default()
        .lines()
        .filter(|e| !e.trim().is_empty() && !e.trim().ends_with("@users.noreply.github.com"))
        .count();
    if personal > 0 {
        findings.push(Finding::note(format!(
            "{} list your personal email (your `git config user.email`) as the author, and \
             anyone can see it on GitHub once pushed. That's normal if you're fine with it. To \
             hide it in future commits, set user.email to your own GitHub noreply address \
             (GitHub → Settings → Emails).",
            if personal == 1 {
                "This commit will".to_string()
            } else {
                format!("These {personal} commits will")
            }
        )));
    }
    findings
}

/// Look through a `git log -p --unified=0` patch for keys and .env values on
/// added lines. `env` holds (file, KEY, value).
fn scan_patch(patch: &str, env: &[(String, String, String)]) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut file = String::new();
    let mut line_no = 0usize;
    for line in patch.lines() {
        if let Some(path) = line.strip_prefix("+++ ") {
            file = path.strip_prefix("b/").unwrap_or(path).to_string();
            continue;
        }
        if let Some(hunk) = line.strip_prefix("@@ ") {
            // "@@ -a,b +c,d @@": new lines start at c.
            line_no = hunk
                .split_whitespace()
                .find_map(|part| part.strip_prefix('+'))
                .and_then(|n| n.split(',').next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            continue;
        }
        let Some(added) = line.strip_prefix('+') else {
            continue;
        };
        if file == "/dev/null" {
            continue;
        }
        let at = format!("{file}:{line_no}");
        for (kind, pattern) in KEY_PATTERNS.iter() {
            if pattern.is_match(added) && seen.insert((at.clone(), *kind)) {
                findings.push(Finding::block(format!(
                    "`{at}` has what looks like {kind}."
                )));
            }
        }
        for (env_file, key, value) in env {
            if added.contains(value.as_str()) && seen.insert((at.clone(), "env")) {
                findings.push(Finding::block(format!(
                    "`{at}` contains the value of {key} from {env_file}."
                )));
            }
        }
        line_no += 1;
    }
    findings
}

fn floor_char_boundary(text: &str, max: usize) -> usize {
    if text.len() <= max {
        return text.len();
    }
    (0..=max)
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0)
}

async fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("--no-pager")
        .args(["-c", "core.quotepath=false"])
        .args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        cmd.creation_flags(0x08000000);
    }
    let out = cmd.output().await.map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).to_string())
    }
}

/// Make sure the project's .gitignore keeps Norvexum's own folder
/// (`.norvexum/`: sessions, context) out of commits. Only in the root of a git
/// repository, and only once: nothing changes when it is already ignored.
pub fn ensure_norvexum_ignored(root: &Path) {
    if !root.join(".git").exists() {
        return;
    }
    let probe = format!("{}/config.toml", crate::config::NORVEXUM_DIR);
    let mut check = std::process::Command::new("git");
    check
        .args(["check-ignore", "-q", &probe])
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        check.creation_flags(0x08000000);
    }
    match check.status() {
        // Already ignored, or git couldn't tell: leave the file alone.
        Ok(status) if status.code() == Some(1) => {}
        _ => return,
    }
    let path = root.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut text = existing.clone();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(&format!(
        "# Norvexum's local data (sessions, context)\n{}/\n",
        crate::config::NORVEXUM_DIR
    ));
    if let Err(e) = std::fs::write(&path, text) {
        tracing::warn!(
            "Couldn't add {} to .gitignore: {e}",
            crate::config::NORVEXUM_DIR
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test keys are built at run time, so this file never contains a line
    /// that looks like a real key (it would trip the check it tests).
    fn fake(prefix: &str, len: usize) -> String {
        format!("{prefix}{}", &"a1B2".repeat(len / 4 + 1)[..len])
    }

    #[test]
    fn secret_files_are_recognised_and_templates_are_not() {
        for secret in [
            ".env",
            "config/.env.local",
            "id_rsa",
            "certs/server.pem",
            "service-account-prod.json",
        ] {
            assert!(is_secret_file(secret), "{secret}");
        }
        for fine in [
            ".env.example",
            ".env.sample",
            "README.md",
            "src/keys.rs",
            "environment.ts",
        ] {
            assert!(!is_secret_file(fine), "{fine}");
        }
    }

    #[test]
    fn added_keys_and_env_values_are_found_with_their_place_but_not_their_value() {
        let openai = fake("sk-proj-", 40);
        let github = fake("ghp_", 36);
        let secret = "s3cr3t-db-pass".to_string();
        let patch = format!(
            "+++ b/src/config.rs\n@@ -1,0 +10,3 @@\n+let a = \"{openai}\";\n+let b = 1;\n+let c = \"{github}\";\n\
             +++ b/app.py\n@@ -0,0 +1 @@\n+DB = \"{secret}\"\n\
             +++ b/old.rs\n@@ -5 +4,0 @@\n-let gone = \"{openai}\";\n"
        );
        let env = vec![(".env".to_string(), "DB_PASS".to_string(), secret.clone())];
        let found = scan_patch(&patch, &env);
        let texts: Vec<&str> = found.iter().map(|f| f.text.as_str()).collect();
        assert!(
            texts
                .iter()
                .any(|t| t.contains("src/config.rs:10") && t.contains("OpenAI")),
            "{texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|t| t.contains("src/config.rs:12") && t.contains("GitHub")),
            "{texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|t| t.contains("app.py:1") && t.contains("DB_PASS")),
            "{texts:?}"
        );
        // Removed lines aren't uploaded as new content; nothing reveals a value.
        assert!(!texts.iter().any(|t| t.contains("old.rs")), "{texts:?}");
        assert!(
            found
                .iter()
                .all(|f| !f.text.contains(&openai) && !f.text.contains(&secret))
        );
        assert!(found.iter().all(|f| f.level == Level::Block));
    }

    #[test]
    fn env_placeholders_and_flags_are_not_treated_as_secrets() {
        for skip in [
            "",
            "true",
            "12345678",
            "your_key_here",
            "changeme123",
            "http://localhost:3000",
        ] {
            assert!(!is_secret_value(skip), "{skip}");
        }
        assert!(is_secret_value("kq83jd02la"));
    }

    #[test]
    fn norvexum_folder_is_added_to_gitignore_once() {
        let root = std::env::temp_dir().join(format!("nvx-ignore-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            return;
        }
        std::fs::write(root.join(".gitignore"), "target/").unwrap();
        ensure_norvexum_ignored(&root);
        ensure_norvexum_ignored(&root);
        let text = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(text.starts_with("target/\n"), "{text}");
        assert_eq!(text.matches(".norvexum/").count(), 1, "{text}");
        std::fs::remove_dir_all(&root).ok();
    }
}
