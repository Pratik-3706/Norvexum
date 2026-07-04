// ═══════════════════════════════════════════════════════════════════════════
// Package Safety — Check packages before install
// ═══════════════════════════════════════════════════════════════════════════

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
        "Check if a package is safe before installing. Queries vulnerability databases \
         for known security issues and typosquatting detection."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Package name" },
                "ecosystem": {
                    "type": "string",
                    "enum": ["pypi", "npm"],
                    "description": "Package ecosystem (default: pypi)"
                },
                "version": { "type": "string", "description": "Specific version (optional)" }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let name = args["name"].as_str().unwrap_or("");
        let ecosystem = args["ecosystem"].as_str().unwrap_or("pypi");
        let version = args["version"].as_str();

        if name.is_empty() {
            return ToolResult::err("Package name cannot be empty");
        }

        match ecosystem {
            "pypi" => check_pypi(name, version).await,
            "npm" => check_npm(name, version).await,
            _ => ToolResult::err(format!("Unknown ecosystem: {}", ecosystem)),
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

    match response {
        Ok(r) => {
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
        Err(_) => {}
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

fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_len = a.len();
    let b_len = b.len();
    let mut matrix = vec![vec![0usize; b_len + 1]; a_len + 1];

    for i in 0..=a_len {
        matrix[i][0] = i;
    }
    for j in 0..=b_len {
        matrix[0][j] = j;
    }

    for (i, ca) in a.chars().enumerate() {
        for (j, cb) in b.chars().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            matrix[i + 1][j + 1] = (matrix[i][j + 1] + 1)
                .min(matrix[i + 1][j] + 1)
                .min(matrix[i][j] + cost);
        }
    }

    matrix[a_len][b_len]
}
