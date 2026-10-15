//! Protocol-neutral types. Serialized differently per codec but shared here.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role { User, Assistant, Tool }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    Text { text: String },
    /// Model "thinking" output. Kept as its own variant, never merged into
    /// Text: a client that wants only the answer must still be able to tell
    /// them apart, and a codec that cannot express thoughts must be able to
    /// drop them deliberately instead of leaking them as prose.
    Reasoning { text: String },
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

/// The client's request for model "thinking", in protocol-neutral terms.
///
/// Every entry protocol spells this differently (OpenAI `reasoning_effort`,
/// Anthropic `thinking.budget_tokens`, Gemini `thinkingConfig`), so the IR
/// keeps the *intent* plus the original wire fields. Two consequences matter:
/// it can be translated when the upstream speaks a different protocol, and a
/// same-protocol forward can replay the client's own fields faithfully instead
/// of re-deriving them.
///
/// This exists because the gateway re-renders every upstream body from the IR:
/// a field the IR does not model is not "passed through", it is deleted. The
/// absence of this struct was exactly why a client's thinking budget never
/// reached the model.
#[derive(Debug, Clone, Default)]
pub struct ReasoningCfg {
    /// Effort hint as sent ("low"/"medium"/"high", or a provider extension).
    /// A free string so an unknown value survives instead of being dropped.
    pub effort: Option<String>,
    /// Explicit thinking budget in tokens.
    pub budget_tokens: Option<u32>,
    /// Some(true) = thinking explicitly asked for, Some(false) = explicitly
    /// disabled, None = the client said nothing.
    pub enabled: Option<bool>,
    /// The client's own fields, tagged with the entry protocol:
    /// `{"protocol":"anthropic","fields":{"thinking":{...}}}`. Used only to make
    /// a same-protocol forward faithful.
    pub raw: Option<serde_json::Value>,
}

impl ReasoningCfg {
    pub fn is_empty(&self) -> bool {
        self.effort.is_none() && self.budget_tokens.is_none() && self.enabled.is_none() && self.raw.is_none()
    }

    /// The client's original fields, but only when they came from `protocol`:
    /// replaying an OpenAI-shaped blob into an Anthropic body would be wrong.
    pub fn raw_for(&self, protocol: &str) -> Option<&serde_json::Map<String, serde_json::Value>> {
        let raw = self.raw.as_ref()?;
        if raw.get("protocol").and_then(|p| p.as_str()) != Some(protocol) {
            return None;
        }
        raw.get("fields").and_then(|f| f.as_object())
    }

    /// Budget in tokens, falling back to a translation of the effort hint.
    ///
    /// The fallback is a deliberate, documented invention: Anthropic and Gemini
    /// have no effort knob, so an OpenAI client asking for `reasoning_effort:
    /// high` would otherwise get *no* thinking at all when routed to them —
    /// which is the bug this whole struct exists to fix. The numbers are
    /// conventional thinking-budget sizes, not measurements.
    pub fn budget_or_effort(&self) -> Option<u32> {
        self.budget_tokens
            .or_else(|| self.effort.as_deref().map(effort_to_budget))
    }

    /// Thinking was asked for (or a budget/effort implies it) and not disabled.
    pub fn wants_thinking(&self) -> bool {
        if self.enabled == Some(false) {
            return false;
        }
        self.enabled == Some(true) || self.budget_tokens.is_some() || self.effort.is_some()
    }
}

/// Effort hint -> thinking budget. See `ReasoningCfg::budget_or_effort`.
pub fn effort_to_budget(effort: &str) -> u32 {
    match effort.trim().to_ascii_lowercase().as_str() {
        "minimal" | "low" => 4096,
        "high" | "xhigh" | "max" => 16384,
        // Unknown and "medium" both land here: a value we do not recognise must
        // still produce thinking rather than silently none.
        _ => 8192,
    }
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
