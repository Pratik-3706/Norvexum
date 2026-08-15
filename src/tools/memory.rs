use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;

use crate::tools::{Tool, ToolContext, ToolResult};

const MAX_MEMORY_SIZE: usize = 20 * 1024 * 1024; // 20 MB

pub struct TempMemory {
    storage: HashMap<String, String>,
    current_size: usize,
}

impl TempMemory {
    pub fn new() -> Self {
        Self {
            storage: HashMap::new(),
            current_size: 0,
        }
    }

    pub fn store(&mut self, key: String, value: String) -> Result<(), String> {
        let value_size = value.len();

        // Check if value itself is too large
        if value_size > MAX_MEMORY_SIZE {
            return Err(format!("Value too large to store. Max size is 20MB."));
        }

        // If storing this exceeds max memory, flush everything
        if self.current_size + value_size > MAX_MEMORY_SIZE {
            self.storage.clear();
            self.current_size = 0;
        }

        if let Some(old_val) = self.storage.insert(key.clone(), value) {
            self.current_size -= old_val.len();
        }
        self.current_size += value_size;

        Ok(())
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
        "Store a string value in temporary RAM storage (max 20MB). Useful for stashing code, drafts, or fetched web content without creating files."
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

        let mut mem = ctx.temp_memory.write().await;
        match mem.store(key.clone(), value) {
            Ok(_) => ToolResult::ok(format!(
                "Successfully stored {} bytes under key '{}'. Current memory usage: {}/20MB.",
                mem.current_size, key, mem.current_size
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
            ToolResult::err(format!("Key '{}' not found in temporary memory.", key))
        }
    }
}
