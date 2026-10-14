use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;

use crate::tools::{Tool, ToolContext, ToolResult};

const MAX_MEMORY_SIZE: usize = 20 * 1024 * 1024; // 20 MB

pub struct TempMemory {
    storage: HashMap<String, String>,
    current_size: usize,
}

impl Default for TempMemory {
    fn default() -> Self {
        Self::new()
    }
}

impl TempMemory {
    pub fn new() -> Self {
        Self {
            storage: HashMap::new(),
            current_size: 0,
        }
    }

    pub fn store(&mut self, key: String, value: String) -> Result<(), String> {
        let replaced = self.storage.get(&key).map_or(0, String::len);
        let new_size = self.current_size - replaced + value.len();
        if new_size > MAX_MEMORY_SIZE {
            // Evicting other keys silently would lose data the model still relies on.
            return Err(format!(
                "Not enough space: storing {} bytes would use {} of {} bytes. Overwrite or drop other keys first (stored keys: {}).",
                value.len(),
                new_size,
                MAX_MEMORY_SIZE,
                self.keys().join(", ")
            ));
        }
        self.storage.insert(key, value);
        self.current_size = new_size;
        Ok(())
    }

    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.storage.keys().cloned().collect();
        keys.sort();
        keys
    }

    pub fn read(&self, key: &str) -> Option<String> {
        self.storage.get(key).cloned()
    }
}

// ── Store Tool ──────────────────────────────────────────────────────────────

pub struct StoreInMemoryTool;

#[async_trait]
impl Tool for StoreInMemoryTool {
    fn name(&self) -> &str {
        "store_in_memory"
    }

    fn description(&self) -> &str {
        "Store a string in temporary in-memory storage (max 20MB total, lost when the app exits) to pass data between steps."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "key": { "type": "string", "description": "The key to store the data under." },
                "value": { "type": "string", "description": "The string data to store." }
            },
            "required": ["key", "value"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let key = match args.get("key").and_then(|v| v.as_str()) {
            Some(k) => k.to_string(),
            None => return ToolResult::err("Missing 'key' argument"),
        };
        let value = match args.get("value").and_then(|v| v.as_str()) {
            Some(v) => v.to_string(),
            None => return ToolResult::err("Missing 'value' argument"),
        };

        let size = value.len();
        let mut mem = ctx.temp_memory.write().await;
        match mem.store(key.clone(), value) {
            Ok(_) => ToolResult::ok(format!(
                "Stored {size} bytes under '{key}'. Memory in use: {:.1} KB of 20 MB.",
                mem.current_size as f64 / 1024.0
            )),
            Err(e) => ToolResult::err(e),
        }
    }
}

// ── Read Tool ───────────────────────────────────────────────────────────────

pub struct ReadFromMemoryTool;

#[async_trait]
impl Tool for ReadFromMemoryTool {
    fn name(&self) -> &str {
        "read_from_memory"
    }

    fn description(&self) -> &str {
        "Read a string value from the temporary RAM storage."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "key": { "type": "string", "description": "The key to read." }
            },
            "required": ["key"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let key = match args.get("key").and_then(|v| v.as_str()) {
            Some(k) => k,
            None => return ToolResult::err("Missing 'key' argument"),
        };

        let mem = ctx.temp_memory.read().await;
        if let Some(val) = mem.read(key) {
            ToolResult::ok(val)
        } else {
            let keys = mem.keys();
            ToolResult::err(format!(
                "Key '{key}' not found. Stored keys: {}",
                if keys.is_empty() {
                    "none".to_string()
                } else {
                    keys.join(", ")
                }
            ))
        }
    }
}
