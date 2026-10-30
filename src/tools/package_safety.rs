// ===========================================================================
// Package Safety — Check packages before install
// ===========================================================================

use async_trait::async_trait;
use serde_json::json;

use super::{Tool, ToolContext, ToolResult};

pub struct PackageSafetyTool;

#[async_trait]
impl Tool for PackageSafetyTool {
    fn name(&self) -> &str {
        "check_package"
    }
    fn description(&self) -> &str {
        "Check a package before installing it: does it exist in its registry, does the version have \
         known vulnerabilities (OSV), and does the name look like typosquatting. Ecosystems: pypi, npm, \
         packagist (Composer), go, crates, rubygems, nuget, maven (name as group:artifact). \
         Or pass `lockfile` (composer.lock, package-lock.json, go.sum, Cargo.lock, requirements.txt, \
         Gemfile.lock) to check every pinned dependency in one call."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Package name (not needed with lockfile)" },
                "ecosystem": {
                    "type": "string",
                    "enum": ["pypi", "npm", "packagist", "go", "crates", "rubygems", "nuget", "maven"],
                    "description": "Package ecosystem (default: pypi)"
                },
                "version": { "type": "string", "description": "Specific version (optional)" },
                "lockfile": { "type": "string", "description": "Path of a lock file to audit instead of one package" }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        if let Some(lockfile) = args["lockfile"].as_str().filter(|l| !l.trim().is_empty()) {
            return audit_lockfile(lockfile, ctx).await;
        }
        let name = args["name"].as_str().unwrap_or("").trim();
        let ecosystem = args["ecosystem"].as_str().unwrap_or("pypi").to_lowercase();
        let version = args["version"].as_str().filter(|v| !v.trim().is_empty());

        if name.is_empty() {
            return ToolResult::err("Package name cannot be empty (or pass `lockfile`)");
        }

        match ecosystem.as_str() {
            "pypi" => check_pypi(name, version).await,
            "npm" => check_npm(name, version).await,
            other => match Registry::from_name(other) {
                Some(registry) => check_registry(registry, name, version).await,
                None => ToolResult::err(format!(
                    "Unknown ecosystem '{other}'. Use pypi, npm, packagist, go, crates, rubygems, nuget or maven."
                )),
            },
        }
    }
}

async fn check_pypi(name: &str, version: Option<&str>) -> ToolResult {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    // 1. Check if package exists on PyPI
    let url = format!("https://pypi.org/pypi/{}/json", name);
    let response = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => return ToolResult::err(format!("Failed to query PyPI: {}", e)),
    };

    if response.status() == 404 {
        return ToolResult::err(format!(
            "⚠️ Package '{}' NOT FOUND on PyPI. Could be typosquatting.",
            name
        ));
    }

    let data: serde_json::Value = match response.json().await {
        Ok(d) => d,
        Err(e) => return ToolResult::err(format!("Failed to parse PyPI response: {}", e)),
    };

    let info = &data["info"];
    let pkg_name = info["name"].as_str().unwrap_or(name);
    let latest_version = info["version"].as_str().unwrap_or("unknown");
    let summary = info["summary"].as_str().unwrap_or("");
    let author = info["author"].as_str().unwrap_or("unknown");

    // 2. Check OSV (Open Source Vulnerabilities) database
    let vulns = check_osv("PyPI", name, version.unwrap_or(latest_version)).await;

    // 3. Basic typosquatting heuristics
    let suspicious = check_typosquatting(name);

    let mut output = format!(
        "📦 Package: {} v{}\n   Author: {}\n   Summary: {}\n",
        pkg_name, latest_version, author, summary
    );

    if !vulns.is_empty() {
        output.push_str(&format!("\n⚠️ VULNERABILITIES FOUND ({}):\n", vulns.len()));
        for v in &vulns {
            output.push_str(&format!("   - {}\n", v));
        }
    }

    if !suspicious.is_empty() {
        output.push_str(&format!("\n⚠️ SUSPICIOUS: {}\n", suspicious));
    }

    let is_safe = vulns.is_empty() && suspicious.is_empty();
    if is_safe {
        output.push_str("\n✅ Package appears SAFE to install.");
    } else {
        output.push_str("\n❌ Package has security concerns. Review before installing.");
    }

    ToolResult::ok_with_data(
        output,
        json!({
            "name": pkg_name,
            "version": latest_version,
            "safe": is_safe,
            "vulnerabilities": vulns.len(),
            "suspicious": !suspicious.is_empty()
        }),
    )
}

async fn check_npm(name: &str, version: Option<&str>) -> ToolResult {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let url = format!("https://registry.npmjs.org/{}", name);

    let response = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => return ToolResult::err(format!("Failed to query npm: {}", e)),
    };

    if response.status() == 404 {
        return ToolResult::err(format!(
            "⚠️ Package '{}' NOT FOUND on npm. Could be typosquatting.",
            name
        ));
    }

    let data: serde_json::Value = match response.json().await {
        Ok(d) => d,
        Err(e) => return ToolResult::err(format!("Failed to parse npm response: {}", e)),
    };

    let latest = data["dist-tags"]["latest"].as_str().unwrap_or("unknown");
    let description = data["description"].as_str().unwrap_or("");

    let vulns = check_osv("npm", name, version.unwrap_or(latest)).await;
    let suspicious = check_typosquatting(name);

    let is_safe = vulns.is_empty() && suspicious.is_empty();
    let status = if is_safe {
        "✅ SAFE"
    } else {
        "❌ CONCERNS FOUND"
    };

    ToolResult::ok_with_data(
        format!(
            "📦 {} v{}\n   {}\n   Vulnerabilities: {}\n   Status: {}",
            name,
            latest,
            description,
            vulns.len(),
            status
        ),
        json!({ "name": name, "version": latest, "safe": is_safe }),
    )
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        // crates.io rejects requests without a user agent.
        .user_agent(concat!("norvexum/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Ecosystems checked through their registry's JSON API plus OSV.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Registry {
    Packagist,
    Go,
    Crates,
    RubyGems,
    NuGet,
    Maven,
}

impl Registry {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "packagist" | "composer" | "php" => Self::Packagist,
            "go" | "golang" => Self::Go,
            "crates" | "crates.io" | "cargo" | "rust" => Self::Crates,
            "rubygems" | "gem" | "ruby" => Self::RubyGems,
            "nuget" | ".net" | "dotnet" => Self::NuGet,
            "maven" | "java" => Self::Maven,
            _ => return None,
        })
    }

    /// The ecosystem name OSV uses.
    fn osv_name(self) -> &'static str {
        match self {
            Self::Packagist => "Packagist",
            Self::Go => "Go",
            Self::Crates => "crates.io",
            Self::RubyGems => "RubyGems",
            Self::NuGet => "NuGet",
            Self::Maven => "Maven",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Packagist => "Packagist",
            Self::Go => "the Go module proxy",
            Self::Crates => "crates.io",
            Self::RubyGems => "RubyGems",
            Self::NuGet => "NuGet",
            Self::Maven => "Maven Central",
        }
    }
}

/// Go module paths are case-encoded in proxy URLs: "A" becomes "!a".
fn go_escape(module: &str) -> String {
    module
        .chars()
        .flat_map(|c| {
            if c.is_ascii_uppercase() {
                vec!['!', c.to_ascii_lowercase()]
            } else {
                vec![c]
            }
        })
        .collect()
}

/// Latest version and a description from the registry; `Ok(None)` when the
/// package does not exist there.
async fn registry_lookup(
    registry: Registry,
    name: &str,
) -> Result<Option<(String, String)>, String> {
    let client = http_client();
    let url = match registry {
        Registry::Packagist => format!("https://repo.packagist.org/p2/{name}.json"),
        Registry::Go => format!("https://proxy.golang.org/{}/@latest", go_escape(name)),
        Registry::Crates => format!("https://crates.io/api/v1/crates/{name}"),
        Registry::RubyGems => format!("https://rubygems.org/api/v1/gems/{name}.json"),
        Registry::NuGet => format!(
            "https://api.nuget.org/v3-flatcontainer/{}/index.json",
            name.to_lowercase()
        ),
        Registry::Maven => {
            let (group, artifact) = name
                .split_once(':')
                .ok_or("Maven packages are named group:artifact")?;
            format!(
                "https://search.maven.org/solrsearch/select?q=g:%22{group}%22+AND+a:%22{artifact}%22&rows=1&wt=json"
            )
        }
    };
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Failed to query {}: {e}", registry.label()))?;
    if matches!(response.status().as_u16(), 404 | 410) {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(format!(
            "{} answered HTTP {}",
            registry.label(),
            response.status()
        ));
    }
    let data: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("Unexpected answer from {}: {e}", registry.label()))?;
    let text = |v: &serde_json::Value| v.as_str().unwrap_or("").to_string();
    Ok(match registry {
        Registry::Packagist => {
            let latest = &data["packages"][name][0];
            Some((text(&latest["version"]), text(&latest["description"])))
        }
        Registry::Go => Some((text(&data["Version"]), String::new())),
        Registry::Crates => Some((
            text(&data["crate"]["max_stable_version"]),
            text(&data["crate"]["description"]),
        )),
        Registry::RubyGems => Some((text(&data["version"]), text(&data["info"]))),
        Registry::NuGet => data["versions"]
            .as_array()
            .and_then(|v| v.last())
            .map(|v| (text(v), String::new())),
        Registry::Maven => {
            let doc = &data["response"]["docs"][0];
            if doc.is_null() {
                None
            } else {
                Some((text(&doc["latestVersion"]), String::new()))
            }
        }
    })
}

async fn check_registry(registry: Registry, name: &str, version: Option<&str>) -> ToolResult {
    let (latest, description) = match registry_lookup(registry, name).await {
        Ok(Some(found)) => found,
        Ok(None) => {
            return ToolResult::err(format!(
                "⚠️ Package '{name}' NOT FOUND on {}. Check the name; it could be typosquatting.",
                registry.label()
            ));
        }
        Err(e) => return ToolResult::err(e),
    };
    let checked = version.unwrap_or(&latest).to_string();
    let vulns = check_osv(registry.osv_name(), name, &checked).await;
    let suspicious = check_typosquatting(name);
    let is_safe = vulns.is_empty() && suspicious.is_empty();

    let mut output = format!("📦 {name} (latest {latest}) on {}\n", registry.label());
    if !description.is_empty() {
        output.push_str(&format!("   {description}\n"));
    }
    output.push_str(&format!("   Checked version: {checked}\n"));
    if !vulns.is_empty() {
        output.push_str(&format!("\n⚠️ VULNERABILITIES FOUND ({}):\n", vulns.len()));
        for v in &vulns {
            output.push_str(&format!("   - {v}\n"));
        }
    }
    if !suspicious.is_empty() {
        output.push_str(&format!("\n⚠️ SUSPICIOUS: {suspicious}\n"));
    }
    output.push_str(if is_safe {
        "\n✅ No known vulnerabilities for this version."
    } else {
        "\n❌ Security concerns found. Review before installing."
    });
    ToolResult::ok_with_data(
        output,
        json!({ "name": name, "version": checked, "latest": latest, "safe": is_safe, "vulnerabilities": vulns.len() }),
    )
}

/// One pinned dependency from a lock file.
#[derive(Debug, PartialEq)]
struct Pinned {
    ecosystem: &'static str,
    name: String,
    version: String,
}

/// The pinned dependencies in a lock file, by file name.
fn parse_lockfile(file_name: &str, content: &str) -> Result<Vec<Pinned>, String> {
    let pin = |ecosystem, name: &str, version: &str| Pinned {
        ecosystem,
        name: name.to_string(),
        version: version.trim_start_matches(['v', '=']).to_string(),
    };
    let mut out = Vec::new();
    match file_name {
        "composer.lock" => {
            let data: serde_json::Value =
                serde_json::from_str(content).map_err(|e| format!("Invalid composer.lock: {e}"))?;
            for section in ["packages", "packages-dev"] {
                for p in data[section].as_array().into_iter().flatten() {
                    if let (Some(n), Some(v)) = (p["name"].as_str(), p["version"].as_str()) {
                        out.push(pin("Packagist", n, v));
                    }
                }
            }
        }
        "package-lock.json" | "npm-shrinkwrap.json" => {
            let data: serde_json::Value =
                serde_json::from_str(content).map_err(|e| format!("Invalid {file_name}: {e}"))?;
            if let Some(packages) = data["packages"].as_object() {
                for (path, p) in packages {
                    // "node_modules/a/node_modules/@scope/b" → "@scope/b"
                    let Some(name) = path
                        .rsplit("node_modules/")
                        .next()
                        .filter(|_| !path.is_empty())
                    else {
                        continue;
                    };
                    if let Some(v) = p["version"].as_str() {
                        out.push(pin("npm", name, v));
                    }
                }
            } else if let Some(deps) = data["dependencies"].as_object() {
                for (name, p) in deps {
                    if let Some(v) = p["version"].as_str() {
                        out.push(pin("npm", name, v));
                    }
                }
            }
        }
        "go.sum" => {
            for line in content.lines() {
                let mut parts = line.split_whitespace();
                if let (Some(module), Some(version)) = (parts.next(), parts.next()) {
                    out.push(pin("Go", module, version.trim_end_matches("/go.mod")));
                }
            }
        }
        "Cargo.lock" => {
            let data: toml::Value =
                toml::from_str(content).map_err(|e| format!("Invalid Cargo.lock: {e}"))?;
            for p in data["package"].as_array().into_iter().flatten() {
                // Path and git dependencies have no registry source.
                let from_registry = p
                    .get("source")
                    .and_then(|s| s.as_str())
                    .is_some_and(|s| s.starts_with("registry+"));
                if let (true, Some(n), Some(v)) = (
                    from_registry,
                    p.get("name").and_then(|v| v.as_str()),
                    p.get("version").and_then(|v| v.as_str()),
                ) {
                    out.push(pin("crates.io", n, v));
                }
            }
        }
        "Gemfile.lock" => {
            // "    name (1.2.3)" under the "specs:" sections.
            for line in content.lines() {
                let trimmed = line.trim_start();
                if line.len() - trimmed.len() != 4 {
                    continue;
                }
                if let Some((name, rest)) = trimmed.split_once(" (") {
                    out.push(pin("RubyGems", name, rest.trim_end_matches(')')));
                }
            }
        }
        name if name.ends_with(".txt") => {
            for line in content.lines() {
                let line = line.split('#').next().unwrap_or("").trim();
                if let Some((name, version)) = line.split_once("==") {
                    let name = name.split('[').next().unwrap_or(name).trim();
                    let version = version.split([';', ' ']).next().unwrap_or(version);
                    out.push(pin("PyPI", name, version));
                }
            }
        }
        other => {
            return Err(format!(
                "Don't know how to read '{other}'. Supported: composer.lock, package-lock.json, \
                 go.sum, Cargo.lock, Gemfile.lock, requirements*.txt"
            ));
        }
    }
    out.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
    out.dedup();
    Ok(out)
}

async fn audit_lockfile(lockfile: &str, ctx: &ToolContext) -> ToolResult {
    let path = match super::filesystem::resolve_path(lockfile, ctx) {
        Ok(p) => p,
        Err(e) => return ToolResult::err(e),
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return ToolResult::err(format!("Could not read {lockfile}: {e}")),
    };
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let pinned = match parse_lockfile(file_name, &content) {
        Ok(p) => p,
        Err(e) => return ToolResult::err(e),
    };
    if pinned.is_empty() {
        return ToolResult::ok(format!("{lockfile} pins no registry packages."));
    }

    // OSV takes up to 1000 queries per batch request.
    let client = http_client();
    let mut findings: Vec<(String, String, Vec<String>)> = Vec::new();
    for chunk in pinned.chunks(1000) {
        let queries: Vec<serde_json::Value> = chunk
            .iter()
            .map(|p| {
                json!({
                    "package": { "name": p.name, "ecosystem": p.ecosystem },
                    "version": p.version
                })
            })
            .collect();
        let response = client
            .post("https://api.osv.dev/v1/querybatch")
            .json(&json!({ "queries": queries }))
            .send()
            .await;
        let data: serde_json::Value = match response {
            Ok(r) => match r.json().await {
                Ok(d) => d,
                Err(e) => return ToolResult::err(format!("Unexpected answer from OSV: {e}")),
            },
            Err(e) => return ToolResult::err(format!("Failed to query OSV: {e}")),
        };
        for (p, result) in chunk
            .iter()
            .zip(data["results"].as_array().into_iter().flatten())
        {
            let ids: Vec<String> = result["vulns"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v["id"].as_str().map(str::to_string))
                .collect();
            if !ids.is_empty() {
                findings.push((p.name.clone(), p.version.clone(), ids));
            }
        }
    }

    let mut output = format!(
        "Checked {} pinned packages from {lockfile} against OSV.\n",
        pinned.len()
    );
    if findings.is_empty() {
        output.push_str("✅ No known vulnerabilities.");
    } else {
        output.push_str(&format!(
            "\n⚠️ {} packages have known vulnerabilities:\n",
            findings.len()
        ));
        for (name, version, ids) in &findings {
            output.push_str(&format!("   - {name} {version}: {}\n", ids.join(", ")));
        }
        output.push_str("\nDetails: https://osv.dev/vulnerability/<id>");
    }
    ToolResult::ok_with_data(
        output,
        json!({ "packages": pinned.len(), "vulnerable": findings.len() }),
    )
}

/// Query the OSV (Open Source Vulnerabilities) API.
async fn check_osv(ecosystem: &str, name: &str, version: &str) -> Vec<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let body = json!({
        "package": {
            "name": name,
            "ecosystem": ecosystem
        },
        "version": version
    });

    let response = client
        .post("https://api.osv.dev/v1/query")
        .json(&body)
        .send()
        .await;

    if let Ok(r) = response {
        if let Ok(data) = r.json::<serde_json::Value>().await {
            if let Some(vulns) = data["vulns"].as_array() {
                return vulns
                    .iter()
                    .filter_map(|v| {
                        let id = v["id"].as_str()?;
                        let summary = v["summary"].as_str().unwrap_or("No summary");
                        Some(format!("{}: {}", id, summary))
                    })
                    .collect();
            }
        }
    }

    Vec::new()
}

/// Basic typosquatting detection heuristics.
fn check_typosquatting(name: &str) -> String {
    let popular_packages = [
        "requests",
        "flask",
        "django",
        "numpy",
        "pandas",
        "tensorflow",
        "pytorch",
        "scikit-learn",
        "react",
        "express",
        "lodash",
        "axios",
    ];

    let name_lower = name.to_lowercase();

    for popular in &popular_packages {
        if name_lower != *popular && levenshtein_distance(&name_lower, popular) <= 2 {
            return format!(
                "Name '{}' is very similar to popular package '{}' — possible typosquatting",
                name, popular
            );
        }
    }

    String::new()
}

/// Edit distance in characters (the old version sized its table in bytes,
/// which gave wrong distances for non-ASCII names).
fn levenshtein_distance(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b_chars.len()).collect();
    let mut curr = vec![0; b_chars.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b_chars.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b_chars.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lockfiles_yield_their_pinned_packages() {
        let composer = r#"{"packages":[{"name":"guzzlehttp/guzzle","version":"v7.8.1"}],
                           "packages-dev":[{"name":"phpunit/phpunit","version":"10.5.0"}]}"#;
        let pins = parse_lockfile("composer.lock", composer).unwrap();
        assert_eq!(pins.len(), 2);
        assert_eq!(pins[0].ecosystem, "Packagist");
        assert_eq!(pins[0].version, "7.8.1");

        let go_sum = "github.com/pkg/errors v0.9.1 h1:abc=\n\
                      github.com/pkg/errors v0.9.1/go.mod h1:def=\n";
        let pins = parse_lockfile("go.sum", go_sum).unwrap();
        assert_eq!(pins.len(), 1, "go.mod lines are the same module version");
        assert_eq!(pins[0].version, "0.9.1");

        let npm = r#"{"packages":{"":{"name":"app"},
                      "node_modules/lodash":{"version":"4.17.20"},
                      "node_modules/a/node_modules/@scope/b":{"version":"1.0.0"}}}"#;
        let names: Vec<String> = parse_lockfile("package-lock.json", npm)
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, vec!["@scope/b", "lodash"]);

        let cargo = "[[package]]\nname = \"serde\"\nversion = \"1.0.0\"\n\
                     source = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n\
                     [[package]]\nname = \"mine\"\nversion = \"0.1.0\"\n";
        let pins = parse_lockfile("Cargo.lock", cargo).unwrap();
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].name, "serde");

        let reqs = "requests==2.31.0  # http\nflask>=2\nuvicorn[standard]==0.23.0\n";
        let pins = parse_lockfile("requirements.txt", reqs).unwrap();
        assert_eq!(pins.len(), 2);
        assert_eq!(pins[1].name, "uvicorn");

        assert!(parse_lockfile("yarn.lock", "").is_err());
    }

    #[test]
    fn go_module_paths_are_case_encoded() {
        assert_eq!(
            go_escape("github.com/Azure/azure-sdk"),
            "github.com/!azure/azure-sdk"
        );
    }
}
