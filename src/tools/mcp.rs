// ===========================================================================
// MCP — Model Context Protocol servers as extra tools
//
// Servers are configured in ~/.norvexum/mcp.json, in the `mcpServers` format
// other MCP clients use (so configs can be pasted in):
//
//   { "mcpServers": {
//       "files":  { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."] },
//       "remote": { "url": "https://example.com/mcp", "headers": { "Authorization": "Bearer …" } } } }
//
// `command` servers run as child processes speaking JSON-RPC over stdio;
// `url` servers use the streamable HTTP transport. Every tool a connected
// server offers is given to the model as `mcp__<server>__<tool>`, from the
// moment it connects. The user adds servers in Settings or with /mcp, and the
// agent can add them itself with the mcp_manage tool (after approval).
// ===========================================================================

use async_trait::async_trait;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

use super::{Tool, ToolContext, ToolResult};
use crate::ai::types::ToolDef;

/// Prefix of the tool names MCP tools get.
pub const PREFIX: &str = "mcp__";
pub const MANAGE_TOOL_NAME: &str = "mcp_manage";

const PROTOCOL_VERSION: &str = "2025-06-18";
/// Starting a server and listing its tools.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// One tool call.
const CALL_TIMEOUT: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ServerConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
}

impl ServerConfig {
    /// One line describing how the server is reached.
    pub fn summary(&self) -> String {
        match (&self.command, &self.url) {
            (Some(cmd), _) => std::iter::once(cmd.as_str())
                .chain(self.args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" "),
            (None, Some(url)) => url.clone(),
            (None, None) => "(no command or url)".into(),
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
struct ConfigFile {
    #[serde(rename = "mcpServers", default)]
    servers: BTreeMap<String, ServerConfig>,
}

pub fn config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".norvexum")
        .join("mcp.json")
}

fn load_config() -> BTreeMap<String, ServerConfig> {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|text| serde_json::from_str::<ConfigFile>(&text).ok())
        .map(|c| c.servers)
        .unwrap_or_default()
}

fn save_config(servers: &BTreeMap<String, ServerConfig>) -> Result<(), String> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(&ConfigFile {
        servers: servers.clone(),
    })
    .map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("Couldn't save {}: {e}", path.display()))
}

/// Server names become part of tool names: letters, digits, `_` and `-`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && !name.contains("__")
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Connecting,
    Connected,
    Failed(String),
    Disabled,
}

#[derive(Clone, Debug)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// What the UI shows about a server.
#[derive(Clone, Debug)]
pub struct ServerInfo {
    pub name: String,
    pub config: ServerConfig,
    pub status: Status,
    pub tools: Vec<McpTool>,
}

struct Entry {
    config: ServerConfig,
    status: Status,
    tools: Vec<McpTool>,
    conn: Option<Arc<Connection>>,
    /// Bumped on every (re)connect so a stale attempt can't overwrite a newer one.
    generation: u64,
}

static SERVERS: Lazy<Mutex<BTreeMap<String, Entry>>> = Lazy::new(|| Mutex::new(BTreeMap::new()));

fn servers() -> std::sync::MutexGuard<'static, BTreeMap<String, Entry>> {
    SERVERS.lock().unwrap_or_else(|p| p.into_inner())
}

/// Connect the configured servers (once; later calls do nothing). Needs a
/// tokio runtime; without one it waits for a call from inside one.
pub fn start() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    STARTED.call_once(|| {
        for (name, config) in load_config() {
            install(name, config);
        }
    });
}

/// Put `config` in the table under `name` and connect it in the background.
fn install(name: String, config: ServerConfig) {
    let generation = {
        let mut table = servers();
        let generation = table.get(&name).map_or(1, |e| e.generation + 1);
        table.insert(
            name.clone(),
            Entry {
                status: if config.disabled {
                    Status::Disabled
                } else {
                    Status::Connecting
                },
                config: config.clone(),
                tools: Vec::new(),
                conn: None,
                generation,
            },
        );
        generation
    };
    if config.disabled {
        return;
    }
    tokio::spawn(async move {
        let outcome = tokio::time::timeout(CONNECT_TIMEOUT, connect(&config)).await;
        let mut table = servers();
        let Some(entry) = table.get_mut(&name).filter(|e| e.generation == generation) else {
            return; // replaced or removed meanwhile
        };
        match outcome {
            Ok(Ok((conn, tools))) => {
                tracing::info!("MCP server {name}: {} tools", tools.len());
                entry.status = Status::Connected;
                entry.tools = tools;
                entry.conn = Some(Arc::new(conn));
            }
            Ok(Err(e)) => {
                tracing::warn!("MCP server {name} failed: {e}");
                entry.status = Status::Failed(e);
            }
            Err(_) => {
                entry.status =
                    Status::Failed(format!("no answer within {}s", CONNECT_TIMEOUT.as_secs()));
            }
        }
    });
}

/// Every configured server, by name.
pub fn list() -> Vec<ServerInfo> {
    servers()
        .iter()
        .map(|(name, e)| ServerInfo {
            name: name.clone(),
            config: e.config.clone(),
            status: e.status.clone(),
            tools: e.tools.clone(),
        })
        .collect()
}

/// Add (or replace) a server, save it and connect it.
pub fn add(name: &str, config: ServerConfig) -> Result<(), String> {
    if !valid_name(name) {
        return Err("Server names use letters, digits, '-' and '_' (at most 32, no '__').".into());
    }
    if config.command.is_none() == config.url.is_none() {
        return Err("Give either a command (local server) or a url (remote server).".into());
    }
    let mut saved = load_config();
    saved.insert(name.to_string(), config.clone());
    save_config(&saved)?;
    install(name.to_string(), config);
    Ok(())
}

/// Remove a server: stop it, forget it, and delete what `install` put in
/// its folder.
pub fn remove(name: &str) -> Result<(), String> {
    let mut saved = load_config();
    let known = saved.remove(name).is_some() | servers().remove(name).is_some();
    if !known {
        return Err(format!("No MCP server called '{name}'."));
    }
    save_config(&saved)?;
    let dir = install_dir(name);
    if dir.is_dir() {
        // The process was just stopped; give Windows a moment to let go of its files.
        std::thread::sleep(Duration::from_millis(300));
        let _ = std::fs::remove_dir_all(&dir);
    }
    Ok(())
}

/// Where servers that need their own files (cloned repos, builds) live:
/// ~/.norvexum/mcp-servers/<name>, never inside a project.
pub fn install_dir(name: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".norvexum")
        .join("mcp-servers")
        .join(name)
}

/// Longest a clone or build step may take.
const INSTALL_STEP_TIMEOUT: Duration = Duration::from_secs(600);

/// Run one install step (`git clone …`, `npm install`, …) in `dir`;
/// returns the tail of its output.
async fn run_step(command: &str, dir: &std::path::Path) -> Result<String, String> {
    #[cfg(windows)]
    let mut cmd = {
        let mut c = tokio::process::Command::new("cmd.exe");
        c.arg("/C").raw_arg(command);
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(command);
        c
    };
    cmd.current_dir(dir)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(INSTALL_STEP_TIMEOUT, cmd.output())
        .await
        .map_err(|_| {
            format!(
                "`{command}` took longer than {}s",
                INSTALL_STEP_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| format!("couldn't run `{command}`: {e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let tail = crate::util::truncate_middle(text.trim(), 2_000);
    if output.status.success() {
        Ok(tail)
    } else {
        Err(format!("`{command}` failed:\n{tail}"))
    }
}

/// Clone `repo` into the server's own folder and run the build steps there.
/// Returns the folder and a log of what ran.
pub async fn install_from_source(
    name: &str,
    repo: &str,
    subdir: Option<&str>,
    build: &[String],
) -> Result<(PathBuf, String), String> {
    if !valid_name(name) {
        return Err("Server names use letters, digits, '-' and '_' (at most 32, no '__').".into());
    }
    let root = install_dir(name);
    let parent = root.parent().ok_or("no home folder")?.to_path_buf();
    std::fs::create_dir_all(&parent).map_err(|e| e.to_string())?;
    let mut log = String::new();
    if root.join(".git").is_dir() {
        log.push_str(&format!("Updating {}\n", root.display()));
        run_step("git pull --ff-only", &root).await?;
    } else {
        if root.exists() {
            std::fs::remove_dir_all(&root)
                .map_err(|e| format!("couldn't clear {}: {e}", root.display()))?;
        }
        let quoted = shlex::try_quote(repo).map_err(|_| "bad repository URL")?;
        run_step(&format!("git clone --depth 1 {quoted} {name}"), &parent).await?;
        log.push_str(&format!("Cloned {repo} into {}\n", root.display()));
    }
    let dir = match subdir.map(str::trim).filter(|s| !s.is_empty()) {
        Some(sub) => {
            let d = root.join(sub);
            if !d.starts_with(&root) || !d.is_dir() {
                return Err(format!("'{sub}' is not a folder in the repository"));
            }
            d
        }
        None => root,
    };
    for step in build.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let out = run_step(step, &dir).await?;
        log.push_str(&format!(
            "$ {step}\n{}\n",
            crate::util::truncate_chars(&out, 400)
        ));
    }
    Ok((dir, log))
}

/// Turn a server on or off (kept in the config either way).
pub fn set_enabled(name: &str, enabled: bool) -> Result<(), String> {
    let mut saved = load_config();
    let config = saved
        .get_mut(name)
        .ok_or_else(|| format!("No MCP server called '{name}'."))?;
    config.disabled = !enabled;
    let config = config.clone();
    save_config(&saved)?;
    install(name.to_string(), config);
    Ok(())
}

/// Reconnect a server (after it crashed, or to pick up new tools).
pub fn reconnect(name: &str) -> Result<(), String> {
    let config = servers()
        .get(name)
        .map(|e| e.config.clone())
        .ok_or_else(|| format!("No MCP server called '{name}'."))?;
    install(name.to_string(), config);
    Ok(())
}

/// Tool definitions of every connected server, for the model.
pub fn tool_defs() -> Vec<ToolDef> {
    let table = servers();
    let mut defs = Vec::new();
    for (server, entry) in table.iter().filter(|(_, e)| e.status == Status::Connected) {
        for tool in &entry.tools {
            defs.push(ToolDef {
                name: tool_name(server, &tool.name),
                description: format!(
                    "[MCP server '{server}'] {}",
                    crate::util::truncate_chars(&tool.description, 1000)
                ),
                parameters: tool.schema.clone(),
            });
        }
    }
    defs
}

/// The model-facing name of `tool` on `server` (APIs allow [A-Za-z0-9_-], ≤64).
fn tool_name(server: &str, tool: &str) -> String {
    let clean: String = tool
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut name = format!("{PREFIX}{server}__{clean}");
    name.truncate(64);
    name
}

/// Call an `mcp__server__tool` tool.
pub async fn call(full_name: &str, args: Value) -> ToolResult {
    let found = {
        let table = servers();
        table.iter().find_map(|(server, entry)| {
            let tool = entry
                .tools
                .iter()
                .find(|t| tool_name(server, &t.name) == full_name)?;
            Some((server.clone(), tool.name.clone(), entry.conn.clone()))
        })
    };
    let Some((server, tool, conn)) = found else {
        return ToolResult::err(format!(
            "No connected MCP server offers `{full_name}`. Use mcp_manage(action=\"list\") to see the servers."
        ));
    };
    let Some(conn) = conn else {
        return ToolResult::err(format!("MCP server '{server}' is not connected."));
    };
    let args = if args.is_object() { args } else { json!({}) };
    let request = conn.request("tools/call", json!({ "name": tool, "arguments": args }));
    let reply = match tokio::time::timeout(CALL_TIMEOUT, request).await {
        Ok(Ok(reply)) => reply,
        Ok(Err(e)) => {
            if conn.is_dead() {
                if let Some(entry) = servers().get_mut(&server) {
                    entry.status = Status::Failed(format!("stopped: {e}"));
                }
            }
            return ToolResult::err(format!("MCP server '{server}': {e}"));
        }
        Err(_) => {
            return ToolResult::err(format!(
                "MCP server '{server}' did not answer within {}s.",
                CALL_TIMEOUT.as_secs()
            ));
        }
    };
    tool_result(&reply)
}

/// Turn a tools/call result into a ToolResult.
fn tool_result(reply: &Value) -> ToolResult {
    let mut text = Vec::new();
    let mut image = None;
    for part in reply["content"].as_array().into_iter().flatten() {
        match part["type"].as_str() {
            Some("text") => text.push(part["text"].as_str().unwrap_or("").to_string()),
            Some("image") if image.is_none() => {
                image = Some((
                    part["data"].as_str().unwrap_or("").to_string(),
                    part["mimeType"].as_str().unwrap_or("image/png").to_string(),
                ));
            }
            Some("resource") => {
                let res = &part["resource"];
                text.push(
                    res["text"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("[resource {}]", res["uri"])),
                );
            }
            Some(other) => text.push(format!("[{other} content]")),
            None => {}
        }
    }
    if text.is_empty() && !reply["structuredContent"].is_null() {
        text.push(reply["structuredContent"].to_string());
    }
    let output = if text.is_empty() {
        "(no output)".to_string()
    } else {
        text.join("\n")
    };
    if reply["isError"].as_bool() == Some(true) {
        return ToolResult::err(output);
    }
    match image {
        Some((data, mime)) => ToolResult::ok_with_image(output, data, mime),
        None => ToolResult::ok(output),
    }
}

// ---------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------

enum Connection {
    Stdio(Box<StdioConn>),
    Http(HttpConn),
}

impl Connection {
    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        match self {
            Connection::Stdio(c) => c.request(method, params).await,
            Connection::Http(c) => c.request(method, params).await,
        }
    }

    async fn notify(&self, method: &str) {
        match self {
            Connection::Stdio(c) => c.send(json!({"jsonrpc": "2.0", "method": method})).await,
            Connection::Http(c) => c.notify(method).await,
        }
    }

    fn is_dead(&self) -> bool {
        match self {
            Connection::Stdio(c) => c.dead.load(Ordering::SeqCst) != 0,
            Connection::Http(_) => false,
        }
    }
}

/// Start the server, initialize the session and fetch its tools.
async fn connect(config: &ServerConfig) -> Result<(Connection, Vec<McpTool>), String> {
    let conn = match (&config.command, &config.url) {
        (Some(command), _) => Connection::Stdio(Box::new(StdioConn::spawn(command, config)?)),
        (None, Some(url)) => Connection::Http(HttpConn::new(url, &config.headers)),
        (None, None) => return Err("no command or url".into()),
    };
    conn.request(
        "initialize",
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "norvexum", "version": env!("CARGO_PKG_VERSION") }
        }),
    )
    .await
    .map_err(|e| format!("initialize failed: {e}"))?;
    conn.notify("notifications/initialized").await;

    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let params = match &cursor {
            Some(c) => json!({ "cursor": c }),
            None => json!({}),
        };
        let page = conn
            .request("tools/list", params)
            .await
            .map_err(|e| format!("tools/list failed: {e}"))?;
        for tool in page["tools"].as_array().into_iter().flatten() {
            let Some(name) = tool["name"].as_str() else {
                continue;
            };
            let mut schema = tool["inputSchema"].clone();
            if !schema.is_object() {
                schema = json!({ "type": "object", "properties": {} });
            }
            tools.push(McpTool {
                name: name.to_string(),
                description: tool["description"].as_str().unwrap_or("").to_string(),
                schema,
            });
        }
        cursor = page["nextCursor"].as_str().map(str::to_string);
        if cursor.is_none() || tools.len() > 500 {
            break;
        }
    }
    Ok((conn, tools))
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// A local server: a child process speaking newline-delimited JSON-RPC.
struct StdioConn {
    stdin: Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,
    pending: Pending,
    next_id: AtomicU64,
    /// Non-zero once the process has exited.
    dead: Arc<AtomicU64>,
    /// Last lines the server wrote to stderr, for error messages.
    stderr_tail: Arc<Mutex<String>>,
    // Kept for kill_on_drop: removing the server stops the process.
    _child: tokio::process::Child,
}

impl StdioConn {
    fn spawn(command: &str, config: &ServerConfig) -> Result<Self, String> {
        let mut cmd = command_for(command, &config.args);
        cmd.envs(&config.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("couldn't start `{command}`: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let stderr = child.stderr.take().ok_or("no stderr")?;

        let stdin = Arc::new(tokio::sync::Mutex::new(stdin));
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let dead = Arc::new(AtomicU64::new(0));
        let stderr_tail = Arc::new(Mutex::new(String::new()));

        {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut tail = tail.lock().unwrap_or_else(|p| p.into_inner());
                    tail.push_str(&line);
                    tail.push('\n');
                    let mut cut = tail.len().saturating_sub(2000);
                    while !tail.is_char_boundary(cut) {
                        cut += 1;
                    }
                    tail.drain(..cut);
                }
            });
        }
        {
            let pending = pending.clone();
            let dead = dead.clone();
            let stdin = stdin.clone();
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                        continue; // servers sometimes log to stdout
                    };
                    if msg.get("method").is_some() {
                        // A request from the server: answer pings, decline the rest.
                        if let Some(id) = msg.get("id").cloned() {
                            let reply = if msg["method"] == "ping" {
                                json!({"jsonrpc": "2.0", "id": id, "result": {}})
                            } else {
                                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "not supported"}})
                            };
                            let mut out = stdin.lock().await;
                            let _ = out.write_all(format!("{reply}\n").as_bytes()).await;
                            let _ = out.flush().await;
                        }
                        continue;
                    }
                    let Some(id) = msg["id"].as_u64() else {
                        continue;
                    };
                    let waiter = pending
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(&id);
                    if let Some(waiter) = waiter {
                        let _ = waiter.send(reply_of(&msg));
                    }
                }
                dead.store(1, Ordering::SeqCst);
                let why = tail
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .trim()
                    .to_string();
                let why = if why.is_empty() {
                    "the server exited".to_string()
                } else {
                    format!(
                        "the server exited: {}",
                        crate::util::truncate_chars(why.lines().last().unwrap_or(""), 300)
                    )
                };
                for (_, waiter) in pending.lock().unwrap_or_else(|p| p.into_inner()).drain() {
                    let _ = waiter.send(Err(why.clone()));
                }
            });
        }
        Ok(Self {
            stdin,
            pending,
            next_id: AtomicU64::new(1),
            dead,
            stderr_tail,
            _child: child,
        })
    }

    async fn send(&self, msg: Value) {
        let mut out = self.stdin.lock().await;
        let _ = out.write_all(format!("{msg}\n").as_bytes()).await;
        let _ = out.flush().await;
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        if self.dead.load(Ordering::SeqCst) != 0 {
            let tail = self
                .stderr_tail
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            return Err(format!(
                "the server is not running{}",
                tail.lines()
                    .last()
                    .map(|l| format!(" ({l})"))
                    .unwrap_or_default()
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, tx);
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        rx.await.map_err(|_| "the server went away".to_string())?
    }
}

/// The command to start a stdio server. On Windows, `npx`, `uvx` and other
/// .cmd/.bat shims need cmd.exe, and the console window stays hidden.
fn command_for(command: &str, args: &[String]) -> tokio::process::Command {
    #[cfg(windows)]
    {
        let resolved = which::which(command).ok();
        let is_script = resolved.as_ref().is_some_and(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
        });
        let mut cmd = if is_script {
            let mut c = tokio::process::Command::new("cmd.exe");
            c.arg("/C").arg(resolved.unwrap_or_else(|| command.into()));
            c
        } else {
            tokio::process::Command::new(resolved.unwrap_or_else(|| command.into()))
        };
        cmd.args(args);
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args);
        cmd
    }
}

/// A JSON-RPC reply's result, or its error as text.
fn reply_of(msg: &Value) -> Result<Value, String> {
    match msg.get("error") {
        Some(err) if !err.is_null() => Err(err["message"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| err.to_string())),
        _ => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
    }
}

/// A remote server over the streamable HTTP transport.
struct HttpConn {
    client: reqwest::Client,
    url: String,
    headers: BTreeMap<String, String>,
    session: Mutex<Option<String>>,
    next_id: AtomicU64,
}

impl HttpConn {
    fn new(url: &str, headers: &BTreeMap<String, String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(20))
                .build()
                .unwrap_or_default(),
            url: url.to_string(),
            headers: headers.clone(),
            session: Mutex::new(None),
            next_id: AtomicU64::new(1),
        }
    }

    fn post(&self, body: &Value) -> reqwest::RequestBuilder {
        let mut req = self
            .client
            .post(&self.url)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PROTOCOL_VERSION)
            .json(body);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        if let Some(session) = self
            .session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        {
            req = req.header("Mcp-Session-Id", session);
        }
        req
    }

    async fn notify(&self, method: &str) {
        let _ = self
            .post(&json!({"jsonrpc": "2.0", "method": method}))
            .send()
            .await;
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        use futures::StreamExt;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let resp = self
            .post(&body)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        if let Some(session) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *self.session.lock().unwrap_or_else(|p| p.into_inner()) = Some(session.to_string());
        }
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!(
                "HTTP {status}: {}",
                crate::util::truncate_chars(text.trim(), 300)
            ));
        }
        let is_sse = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.contains("text/event-stream"));
        if !is_sse {
            let msg: Value = resp.json().await.map_err(|e| format!("bad reply: {e}"))?;
            return reply_of(&msg);
        }
        // Server-sent events: read until the reply to our request arrives.
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut data = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("stream failed: {e}"))?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(end) = buf.find('\n') {
                let line = buf[..end].trim_end_matches('\r').to_string();
                buf.drain(..=end);
                if let Some(rest) = line.strip_prefix("data:") {
                    data.push_str(rest.trim_start());
                } else if line.is_empty() && !data.is_empty() {
                    let event = std::mem::take(&mut data);
                    if let Ok(msg) = serde_json::from_str::<Value>(&event) {
                        if msg["id"].as_u64() == Some(id) {
                            return reply_of(&msg);
                        }
                    }
                }
            }
        }
        Err("the stream ended without a reply".into())
    }
}

// ---------------------------------------------------------------------------
// mcp_manage — the agent adds and manages servers itself
// ---------------------------------------------------------------------------

pub struct McpManageTool;

/// Whether this call changes the setup (and so needs the user's approval).
pub fn changes_setup(args: &Value) -> bool {
    !matches!(args["action"].as_str(), Some("list") | None)
}

/// A readable list of the servers.
pub fn describe_servers() -> String {
    let all = list();
    if all.is_empty() {
        return format!(
            "No MCP servers are configured ({}).",
            config_path().display()
        );
    }
    all.iter()
        .map(|s| {
            let state = match &s.status {
                Status::Connecting => "connecting".to_string(),
                Status::Connected => format!("connected, {} tools", s.tools.len()),
                Status::Failed(e) => format!("failed: {e}"),
                Status::Disabled => "disabled".to_string(),
            };
            let tools = if s.tools.is_empty() {
                String::new()
            } else {
                format!(
                    "\n    tools: {}",
                    s.tools
                        .iter()
                        .map(|t| tool_name(&s.name, &t.name))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            format!("- {} ({state}) — {}{tools}", s.name, s.config.summary())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[async_trait]
impl Tool for McpManageTool {
    fn name(&self) -> &str {
        MANAGE_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Manage MCP (Model Context Protocol) servers, which add tools named mcp__<server>__<tool>. \
         'list' shows servers, their status and tools. 'add' registers one: a local server with \
         `command` + `args` (+ `env`), e.g. command \"npx\", args [\"-y\", \"@modelcontextprotocol/server-memory\"] \
         (prefer npx/uvx packages or a remote `url` + `headers`: nothing to download). 'install' is \
         for servers that must be built from source: give `repo` (git URL), optional `subdir` and \
         `build` steps (e.g. [\"npm install\", \"npm run build\"]) plus `command`/`args` to start it, \
         where {dir} stands for the built folder (e.g. args [\"{dir}/dist/index.js\"]); it clones and \
         builds in Norvexum's own server folder. Never clone or build MCP servers inside the user's \
         project with run_command. A new server connects right away and its tools are usable from \
         your next step. 'remove' (also deletes an installed server's folder), 'enable', 'disable' \
         and 'reconnect' take `name`. Every change asks the user first."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["list", "add", "install", "remove", "enable", "disable", "reconnect"] },
                "repo": { "type": "string", "description": "install: git URL of the server's repository" },
                "subdir": { "type": "string", "description": "install: folder inside the repository that holds the server (monorepos)" },
                "build": { "type": "array", "items": { "type": "string" }, "description": "install: commands to build it, run in that folder" },
                "name": { "type": "string", "description": "Server name (letters, digits, - and _)" },
                "command": { "type": "string", "description": "add, local server: program to run (npx, uvx, python, node, a path…)" },
                "args": { "type": "array", "items": { "type": "string" }, "description": "add, local server: its arguments" },
                "env": { "type": "object", "additionalProperties": { "type": "string" }, "description": "add, local server: environment variables (API keys…)" },
                "url": { "type": "string", "description": "add, remote server: its MCP endpoint URL" },
                "headers": { "type": "object", "additionalProperties": { "type": "string" }, "description": "add, remote server: HTTP headers (e.g. Authorization)" }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> ToolResult {
        start();
        let name = args["name"].as_str().unwrap_or("").trim().to_string();
        let action = args["action"].as_str().unwrap_or("list");
        if action == "list" {
            return ToolResult::ok(describe_servers());
        }
        if name.is_empty() {
            return ToolResult::err("'name' is required for this action");
        }
        let strings = |v: &Value| -> BTreeMap<String, String> {
            v.as_object()
                .into_iter()
                .flatten()
                .map(|(k, v)| (k.clone(), v.as_str().map_or(v.to_string(), str::to_string)))
                .collect()
        };
        let list = |v: &Value| -> Vec<String> {
            v.as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| a.as_str().map(str::to_string))
                .collect()
        };
        let mut install_log = String::new();
        let done = match action {
            "add" => {
                let config = ServerConfig {
                    command: args["command"].as_str().map(str::to_string),
                    args: list(&args["args"]),
                    env: strings(&args["env"]),
                    url: args["url"].as_str().map(str::to_string),
                    headers: strings(&args["headers"]),
                    disabled: false,
                };
                add(&name, config)
            }
            "install" => {
                let Some(repo) = args["repo"].as_str().filter(|r| !r.trim().is_empty()) else {
                    return ToolResult::err(
                        "'repo' is required for install. For npm/PyPI packages use 'add' with npx/uvx instead.",
                    );
                };
                let Some(command) = args["command"].as_str().filter(|c| !c.trim().is_empty())
                else {
                    return ToolResult::err(
                        "'command' (and 'args') to start the built server are required.",
                    );
                };
                match install_from_source(
                    &name,
                    repo.trim(),
                    args["subdir"].as_str(),
                    &list(&args["build"]),
                )
                .await
                {
                    Ok((dir, log)) => {
                        install_log = log;
                        let dir_text = dir.display().to_string().replace('\\', "/");
                        let fill = |s: &str| s.replace("{dir}", &dir_text);
                        let config = ServerConfig {
                            command: Some(fill(command)),
                            args: list(&args["args"]).iter().map(|a| fill(a)).collect(),
                            env: strings(&args["env"]),
                            ..Default::default()
                        };
                        add(&name, config)
                    }
                    Err(e) => Err(format!("Install failed: {e}")),
                }
            }
            "remove" => remove(&name),
            "enable" => set_enabled(&name, true),
            "disable" => set_enabled(&name, false),
            "reconnect" => reconnect(&name),
            other => return ToolResult::err(format!("Unknown action '{other}'.")),
        };
        if let Err(e) = done {
            return ToolResult::err(e);
        }
        // Give a new connection a moment, so the answer says whether it worked.
        let deadline = std::time::Instant::now() + Duration::from_secs(45);
        while std::time::Instant::now() < deadline
            && servers()
                .get(&name)
                .is_some_and(|e| e.status == Status::Connecting)
        {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let log = if install_log.is_empty() {
            String::new()
        } else {
            format!("{install_log}\n")
        };
        ToolResult::ok(format!("Done.\n{log}{}", describe_servers()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_fit_api_rules() {
        assert_eq!(tool_name("files", "read_file"), "mcp__files__read_file");
        assert_eq!(tool_name("gh", "search.repos"), "mcp__gh__search_repos");
        assert!(tool_name("x", &"a".repeat(100)).len() <= 64);
    }

    #[test]
    fn installed_servers_live_outside_projects() {
        let dir = install_dir("github");
        assert!(
            dir.ends_with(
                std::path::Path::new(".norvexum")
                    .join("mcp-servers")
                    .join("github")
            )
        );
        assert!(dir.starts_with(dirs::home_dir().unwrap()));
    }

    #[tokio::test]
    async fn install_needs_a_valid_name() {
        assert!(
            install_from_source("bad name", "https://x", None, &[])
                .await
                .is_err()
        );
    }

    #[test]
    fn server_names_are_checked() {
        assert!(valid_name("github"));
        assert!(valid_name("my-server_2"));
        assert!(!valid_name(""));
        assert!(!valid_name("a b"));
        assert!(!valid_name("a__b"));
    }

    #[test]
    fn config_reads_the_common_format() {
        let text = r#"{"mcpServers": {
            "files": {"command": "npx", "args": ["-y", "pkg"], "env": {"K": "v"}},
            "remote": {"url": "https://x/mcp", "headers": {"Authorization": "Bearer t"}}
        }}"#;
        let parsed: ConfigFile = serde_json::from_str(text).unwrap();
        assert_eq!(parsed.servers["files"].summary(), "npx -y pkg");
        assert_eq!(
            parsed.servers["remote"].url.as_deref(),
            Some("https://x/mcp")
        );
    }

    #[test]
    fn results_keep_text_and_errors() {
        let ok = tool_result(&json!({"content": [{"type": "text", "text": "hi"}]}));
        assert!(ok.success && ok.output == "hi");
        let err =
            tool_result(&json!({"content": [{"type": "text", "text": "bad"}], "isError": true}));
        assert!(!err.success);
    }

    /// A tiny MCP server in Python, run over stdio. Needs python on PATH.
    #[tokio::test]
    #[ignore]
    async fn talks_to_a_stdio_server() {
        let script = r#"
import sys, json
for line in sys.stdin:
    m = json.loads(line)
    if "id" not in m: continue
    if m["method"] == "initialize": r = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "t", "version": "1"}}
    elif m["method"] == "tools/list": r = {"tools": [{"name": "echo", "description": "Echo", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}]}
    elif m["method"] == "tools/call": r = {"content": [{"type": "text", "text": "echo: " + m["params"]["arguments"]["text"]}]}
    else: r = {}
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
"#;
        let config = ServerConfig {
            command: Some("python".into()),
            args: vec!["-c".into(), script.into()],
            ..Default::default()
        };
        let (conn, tools) = connect(&config).await.expect("connects");
        assert_eq!(tools.len(), 1);
        servers().insert(
            "pytest".into(),
            Entry {
                config,
                status: Status::Connected,
                tools,
                conn: Some(Arc::new(conn)),
                generation: 1,
            },
        );
        assert!(tool_defs().iter().any(|d| d.name == "mcp__pytest__echo"));
        let result = call("mcp__pytest__echo", json!({"text": "hi"})).await;
        assert_eq!(result.output, "echo: hi");
        servers().remove("pytest");
    }
}
