pub mod ir;
pub mod openai;
pub mod anthropic;
pub mod gemini;

use crate::config::Protocol;

/// A fully-parsed chat request in protocol-neutral form.
/// Every entry codec produces this; every exit codec consumes it.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub messages: Vec<ir::Message>,
    pub system: Vec<ir::Content>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub stop: Vec<String>,
    pub stream: bool,
    pub tools: Vec<ir::Tool>,
    pub tool_choice: Option<ir::ToolChoice>,
    /// The client's thinking/reasoning request. Default when unset. Without
    /// this field the gateway silently deleted every thinking knob, because it
    /// re-renders the upstream body from this struct rather than forwarding the
    /// client's JSON.
    pub reasoning: ir::ReasoningCfg,
}

#[derive(Debug, Clone)]
pub struct ChatResponse {
    /// Assistant message content blocks.
    pub content: Vec<ir::Content>,
    pub finish_reason: ir::FinishReason,
    pub usage: ir::Usage,
    /// Model name to echo back to the client (exposed name, not upstream).
    pub model: String,
}

/// One normalized streaming event. Upstream codecs emit these; downstream
/// codecs render them into the client's protocol.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Start { model: String },
    /// Text destined for the client. `finish` is Some when this same event also
    /// completes the answer: the loop guard's holdback buffer is flushed by the
    /// frame that ended the stream, so the completion travels with the text
    /// instead of being invented later (or lost).
    TextDelta { text: String, finish: Option<ir::FinishReason> },
    /// A tool call begins. args is Some when the upstream delivered the WHOLE
    /// call (name + complete arguments) in one frame — Gemini's native shape —
    /// and None when the arguments follow as ToolInputDelta events
    /// (OpenAI/Anthropic shape). A renderer must never invent an empty object
    /// for the None case: that silently corrupts the tool call.
    ToolUseStart { index: usize, id: String, name: String, args: Option<serde_json::Value> },
    ToolInputDelta { index: usize, partial_json: String },
    /// Model thinking, streamed separately from the answer. Renderers that
    /// cannot express it must drop it rather than append it to the text.
    Reasoning { text: String },
    Finish { reason: ir::FinishReason, usage: ir::Usage },
}

pub fn protocol_name(p: Protocol) -> &'static str {
    match p { Protocol::Openai => "openai", Protocol::Anthropic => "anthropic", Protocol::Gemini => "gemini" }
}
