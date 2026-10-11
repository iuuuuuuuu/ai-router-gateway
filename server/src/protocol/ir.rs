//! Protocol-neutral types. Serialized differently per codec but shared here.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role { User, Assistant, Tool }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    Text { text: String },
    ImageUrl { url: String },
    ImageData { media_type: String, data: String },
    ToolUse { id: String, name: String, input: serde_json::Value },
    ToolResult { tool_use_id: String, content: String, #[serde(default)] is_error: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Content>,
    /// OpenAI tool role: the tool_call_id this result answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for parameters.
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Tool(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FinishReason {
    Stop,
    Length,
    ToolCall,
    ContentFilter,
}

impl FinishReason {
    pub fn openai_name(&self) -> &'static str {
        match self { Self::Stop => "stop", Self::Length => "length", Self::ToolCall => "tool_calls", Self::ContentFilter => "content_filter" }
    }
    pub fn anthropic_name(&self) -> &'static str {
        match self { Self::Stop => "end_turn", Self::Length => "max_tokens", Self::ToolCall => "tool_use", Self::ContentFilter => "refusal" }
    }
    /// Gemini has stopReason: STOP | MAX_TOKENS | SAFETY | RECITATION | OTHER
    pub fn gemini_name(&self) -> &'static str {
        match self { Self::Stop => "STOP", Self::Length => "MAX_TOKENS", Self::ToolCall => "STOP", Self::ContentFilter => "SAFETY" }
    }
    /// True when the finish reason means "client-side usable answer ended".
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Stop | Self::Length | Self::ToolCall)
    }
}
