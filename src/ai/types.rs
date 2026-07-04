// ═══════════════════════════════════════════════════════════════════════════
// AI Types — Shared data structures for messages, tool calls, streaming
// ═══════════════════════════════════════════════════════════════════════════

use serde::{Deserialize, Serialize};

// ── Messages ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentPart>,
    /// Tool calls requested by an assistant message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Tool call ID (only for Role::Tool responses).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Tool name (only for Role::Tool responses).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image {
        /// Base64-encoded image data
        data: String,
        /// MIME type (e.g. "image/png")
        mime_type: String,
    },
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: vec![ContentPart::Text { text: text.into() }],
            tool_calls: vec![],
            tool_call_id: None,
            tool_name: None,
        }
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentPart::Text { text: text.into() }],
            tool_calls: vec![],
            tool_call_id: None,
            tool_name: None,
        }
    }

    pub fn user_with_image(text: impl Into<String>, image_b64: String, mime: String) -> Self {
        Self {
            role: Role::User,
            content: vec![
                ContentPart::Text { text: text.into() },
                ContentPart::Image {
                    data: image_b64,
                    mime_type: mime,
                },
            ],
            tool_calls: vec![],
            tool_call_id: None,
            tool_name: None,
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentPart::Text { text: text.into() }],
            tool_calls: vec![],
            tool_call_id: None,
            tool_name: None,
        }
    }

    pub fn assistant_with_tool_calls(text: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentPart::Text { text: text.into() }],
            tool_calls,
            tool_call_id: None,
            tool_name: None,
        }
    }

    pub fn tool_result(
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        result: impl Into<String>,
    ) -> Self {
        Self {
            role: Role::Tool,
            content: vec![ContentPart::Text {
                text: result.into(),
            }],
            tool_calls: vec![],
            tool_call_id: Some(call_id.into()),
            tool_name: Some(tool_name.into()),
        }
    }

    /// Extract all text content, concatenated.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

// ── Tool Definitions ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema for tool parameters
    pub parameters: serde_json::Value,
}

// ── Tool Calls ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

// ── AI Request ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AiRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub temperature: f32,
    pub max_tokens: Option<u32>,
}

impl AiRequest {
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            messages,
            tools: vec![],
            temperature: 0.7,
            max_tokens: None,
        }
    }

    pub fn with_tools(mut self, tools: Vec<ToolDef>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_temperature(mut self, temp: f32) -> Self {
        self.temperature = temp;
        self
    }
}

// ── Streaming Events ──────────────────────────────────────────────────────

/// Events emitted during streaming response.
/// The TUI listens on these to render tokens live.
#[derive(Debug, Clone)]
pub enum AiStreamEvent {
    /// A chunk of thinking/reasoning text (displayed in thinking panel)
    ThinkingDelta(String),
    /// End of thinking block
    ThinkingDone,
    /// A chunk of response text (displayed in chat)
    ContentDelta(String),
    /// The model wants to call tool(s)
    ToolCallStart(ToolCall),
    /// Additional argument data for a tool call being streamed
    ToolCallDelta { id: String, arguments_delta: String },
    /// Tool call definition is complete, ready to execute
    ToolCallComplete(ToolCall),
    /// The full response is done
    Done {
        finish_reason: String,
        usage: Option<UsageStats>,
    },
    /// An error occurred
    Error(String),
}

// ── Usage Stats ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageStats {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

// ── Image Generation ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ImageGenRequest {
    pub prompt: String,
    pub width: u32,
    pub height: u32,
    pub num_images: u32,
}

impl ImageGenRequest {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            width: 1024,
            height: 1024,
            num_images: 1,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ImageGenResult {
    /// Generated images as (base64_data, mime_type) pairs
    pub images: Vec<(String, String)>,
    /// Provider that generated the image
    pub provider: String,
    /// Model used
    pub model: String,
}
