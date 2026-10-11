//! Anthropic Messages wire format (/v1/messages). Entry + exit codec.

use super::ir::{Content, FinishReason, Message, Role, ToolChoice, Usage};
use super::{ChatRequest, ChatResponse, StreamEvent};
use serde_json::{json, Value};

pub fn parse_request(body: &Value) -> anyhow::Result<ChatRequest> {
    let obj = body.as_object().ok_or_else(|| anyhow::anyhow!("body must be object"))?;
    // Anthropic: max_tokens REQUIRED
    let max_tokens = obj
        .get("max_tokens")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .ok_or_else(|| anyhow::anyhow!("max_tokens is required for anthropic format"))?;

    let mut system: Vec<Content> = vec![];
    match obj.get("system") {
        Some(Value::String(s)) => system.push(Content::Text { text: s.clone() }),
        Some(Value::Array(blocks)) => {
            for b in blocks {
                if b.get("type").and_then(|v| v.as_str()) == Some("text") {
                    system.push(Content::Text { text: b.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string() });
                }
            }
        }
        _ => {}
    }

    let mut messages: Vec<Message> = vec![];
    for m in obj.get("messages").and_then(|v| v.as_array()).unwrap_or(&vec![]) {
        let role = match m.get("role").and_then(|v| v.as_str()) {
            Some("assistant") => Role::Assistant,
            _ => Role::User,
        };
        let mut blocks: Vec<Content> = vec![];
        match m.get("content") {
            Some(Value::String(s)) => blocks.push(Content::Text { text: s.clone() }),
            Some(Value::Array(arr)) => {
                for b in arr {
                    match b.get("type").and_then(|v| v.as_str()) {
                        Some("text") => blocks.push(Content::Text { text: b.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string() }),
                        Some("image") => {
                            let src = b.get("source");
                            let stype = src.and_then(|s| s.get("type")).and_then(|v| v.as_str()).unwrap_or("");
                            if stype == "base64" {
                                blocks.push(Content::ImageData {
                                    media_type: src.and_then(|s| s.get("media_type")).and_then(|v| v.as_str()).unwrap_or("image/png").to_string(),
                                    data: src.and_then(|s| s.get("data")).and_then(|v| v.as_str()).unwrap_or("").to_string(),
                                });
                            } else if stype == "url" {
                                blocks.push(Content::ImageUrl { url: src.and_then(|s| s.get("url")).and_then(|v| v.as_str()).unwrap_or("").to_string() });
                            }
                        }
                        Some("tool_use") => blocks.push(Content::ToolUse {
                            id: b.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                            name: b.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                            input: b.get("input").cloned().unwrap_or(json!({})),
                        }),
                        Some("tool_result") => blocks.push(Content::ToolResult {
                            tool_use_id: b.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                            content: match b.get("content") {
                                Some(Value::String(s)) => s.clone(),
                                Some(Value::Array(arr)) => arr.iter().filter_map(|c| c.get("text").and_then(|v| v.as_str())).collect::<Vec<_>>().join("\n"),
                                _ => String::new(),
                            },
                            is_error: b.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false),
                        }),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        messages.push(Message { role, content: blocks, tool_call_id: None });
    }

    let tools = obj
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    Some(super::ir::Tool {
                        name: t.get("name").and_then(|v| v.as_str())?.to_string(),
                        description: t.get("description").and_then(|v| v.as_str()).map(String::from),
                        input_schema: t.get("input_schema").cloned().unwrap_or(json!({"type":"object"})),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let tool_choice = match obj.get("tool_choice") {
        Some(tc) if tc.get("type").and_then(|v| v.as_str()) == Some("auto") => Some(ToolChoice::Auto),
        Some(tc) if tc.get("type").and_then(|v| v.as_str()) == Some("any") => Some(ToolChoice::Required),
        Some(tc) if tc.get("type").and_then(|v| v.as_str()) == Some("tool") => {
            tc.get("name").and_then(|v| v.as_str()).map(|n| ToolChoice::Tool(n.to_string()))
        }
        _ => None,
    };

    let stop = obj
        .get("stop_sequences")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect())
        .unwrap_or_default();

    Ok(ChatRequest {
        messages, system, max_tokens: Some(max_tokens),
        temperature: obj.get("temperature").and_then(|v| v.as_f64()),
        top_p: obj.get("top_p").and_then(|v| v.as_f64()),
        stop, stream: obj.get("stream").and_then(|v| v.as_bool()).unwrap_or(false),
        tools, tool_choice,
    })
}

pub fn render_upstream_request(req: &ChatRequest, model: &str, stream: bool) -> Value {
    let mut messages: Vec<Value> = vec![];
    for m in &req.messages {
        let role = match m.role { Role::Assistant => "assistant", _ => "user" };
        let blocks: Vec<Value> = m
            .content
            .iter()
            .map(|c| match c {
                Content::Text { text } => json!({"type": "text", "text": text}),
                Content::ImageUrl { url } => json!({"type": "image", "source": {"type": "url", "url": url}}),
                Content::ImageData { media_type, data } => {
                    json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}})
                }
                Content::ToolUse { id, name, input } => json!({"type": "tool_use", "id": id, "name": name, "input": input}),
                Content::ToolResult { tool_use_id, content, is_error } => {
                    json!({"type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": is_error})
                }
            })
            .collect();
        messages.push(json!({"role": role, "content": blocks}));
    }

    let mut body = serde_json::Map::new();
    body.insert("model".into(), json!(model));
    body.insert("max_tokens".into(), json!(req.max_tokens.unwrap_or(4096)));
    body.insert("messages".into(), json!(messages));
    if !req.system.is_empty() {
        body.insert("system".into(), json!(req.system.iter().filter_map(|c| match c {
            Content::Text { text } => Some(json!({"type": "text", "text": text})), _ => None,
        }).collect::<Vec<_>>()));
    }
    body.insert("stream".into(), json!(stream));
    if let Some(t) = req.temperature { body.insert("temperature".into(), json!(t)); }
    if let Some(t) = req.top_p { body.insert("top_p".into(), json!(t)); }
    if !req.stop.is_empty() { body.insert("stop_sequences".into(), json!(req.stop)); }
    if !req.tools.is_empty() {
        body.insert("tools".into(), json!(req.tools.iter().map(|t| json!({
            "name": t.name, "description": t.description, "input_schema": t.input_schema
        })).collect::<Vec<_>>()));
        if let Some(tc) = &req.tool_choice {
            body.insert("tool_choice".into(), match tc {
                ToolChoice::Auto => json!({"type": "auto"}),
                ToolChoice::Required => json!({"type": "any"}),
                ToolChoice::Tool(n) => json!({"type": "tool", "name": n}),
                ToolChoice::None => json!({"type": "auto"}),
            });
        }
    }
    Value::Object(body)
}

pub fn parse_response(body: &Value, model_echo: &str) -> anyhow::Result<ChatResponse> {
    if let Some(e) = body.get("error") {
        anyhow::bail!("upstream_error: {}", e.get("message").and_then(|v| v.as_str()).unwrap_or("error"));
    }
    if body.get("type").and_then(|v| v.as_str()) == Some("error") {
        anyhow::bail!("upstream_error: {}", body.get("error").cloned().unwrap_or(json!("error")).to_string());
    }
    let mut content: Vec<Content> = vec![];
    for b in body.get("content").and_then(|v| v.as_array()).unwrap_or(&vec![]) {
        match b.get("type").and_then(|v| v.as_str()) {
            Some("text") => content.push(Content::Text { text: b.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string() }),
            Some("tool_use") => content.push(Content::ToolUse {
                id: b.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                name: b.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                input: b.get("input").cloned().unwrap_or(json!({})),
            }),
            _ => {}
        }
    }
    let finish = match body.get("stop_reason").and_then(|v| v.as_str()) {
        Some("max_tokens") => FinishReason::Length,
        Some("tool_use") => FinishReason::ToolCall,
        Some("refusal") => FinishReason::ContentFilter,
        _ => FinishReason::Stop,
    };
    let u = body.get("usage");
    let usage = Usage {
        input_tokens: u.and_then(|x| x.get("input_tokens")).and_then(|v| v.as_u64()).unwrap_or(0),
        output_tokens: u.and_then(|x| x.get("output_tokens")).and_then(|v| v.as_u64()).unwrap_or(0),
        cache_read_tokens: u.and_then(|x| x.get("cache_read_input_tokens")).and_then(|v| v.as_u64()).unwrap_or(0),
        cache_creation_tokens: u.and_then(|x| x.get("cache_creation_input_tokens")).and_then(|v| v.as_u64()).unwrap_or(0),
    };
    Ok(ChatResponse { content, finish_reason: finish, usage, model: model_echo.to_string() })
}

pub fn render_response(resp: &ChatResponse) -> Value {
    let blocks: Vec<Value> = resp
        .content
        .iter()
        .map(|c| match c {
            Content::Text { text } => json!({"type": "text", "text": text}),
            Content::ToolUse { id, name, input } => json!({"type": "tool_use", "id": id, "name": name, "input": input}),
            _ => json!({"type": "text", "text": ""}),
        })
        .collect();
    json!({
        "id": format!("msg_gw{}", uuid::Uuid::new_v4().simple()),
        "type": "message",
        "role": "assistant",
        "model": resp.model,
        "content": blocks,
        "stop_reason": resp.finish_reason.anthropic_name(),
        "stop_sequence": null,
        "usage": {
            "input_tokens": resp.usage.input_tokens,
            "output_tokens": resp.usage.output_tokens,
            "cache_read_input_tokens": resp.usage.cache_read_tokens,
            "cache_creation_input_tokens": resp.usage.cache_creation_tokens
        }
    })
}

pub fn render_stream_event(model: &str, ev: &StreamEvent) -> Vec<(String, Value)> {
    // returns (event_name, data) pairs; Anthropic SSE uses "event:" lines
    match ev {
        StreamEvent::Start { model: m } => vec![("message_start".into(), json!({
            "type": "message_start",
            "message": {"id": format!("msg_gw{}", uuid::Uuid::new_v4().simple()), "type": "message", "role": "assistant", "model": m, "content": [], "stop_reason": null, "usage": {"input_tokens": 0, "output_tokens": 0}}
        }))],
        StreamEvent::TextDelta { text } => vec![("content_block_delta".into(), json!({
            "type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}
        }))],
        StreamEvent::ToolUseStart { index, id, name, args } => {
            let mut frames = vec![("content_block_start".into(), json!({
                "type": "content_block_start", "index": index,
                "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}
            }))];
            // Whole-package call (Gemini upstream): Anthropic's own wire shape
            // for a complete input is ONE input_json_delta carrying the full
            // JSON string — emit that rather than dropping the args.
            if let Some(a) = args {
                frames.push(("content_block_delta".into(), json!({
                    "type": "content_block_delta", "index": index,
                    "delta": {"type": "input_json_delta", "partial_json": a.to_string()}
                })));
            }
            frames
        }
        StreamEvent::ToolInputDelta { index, partial_json } => vec![("content_block_delta".into(), json!({
            "type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": partial_json}
        }))],
        StreamEvent::Finish { reason, usage } => vec![
            ("message_delta".into(), json!({
                "type": "message_delta",
                "delta": {"stop_reason": reason.anthropic_name(), "stop_sequence": null},
                "usage": {"output_tokens": usage.output_tokens}
            })),
            ("message_stop".into(), json!({"type": "message_stop"})),
        ],
        // model is unused here except Start; silence warning
        _ => vec![],
    }
    .into_iter()
    .map(|(name, mut data)| {
        if let Some(obj) = data.as_object_mut() {
            if name == "message_start" { /* model already inside message */ }
        }
        let _ = model;
        (name, data)
    })
    .collect()
}

/// Classify one upstream Anthropic SSE event (event name + data payload).
pub enum Classified {
    Ignore,
    Start { input_tokens: u64 },
    Text(String),
    /// args carries any input delivered in the SAME frame as the name; it is
    /// empty when the input follows in input_json_delta events.
    ToolUseStart { index: usize, id: String, name: String, args: String },
    ToolInput { index: usize, delta: String },
    Finish { reason: FinishReason, usage: Usage },
    Error(String),
    Done,
}

pub fn classify_upstream_event(event: &str, data: &str) -> Classified {
    let Ok(v) = serde_json::from_str::<Value>(data.trim()) else {
        return Classified::Error(format!("not_sse_garbage: {}", &data.chars().take(80).collect::<String>()));
    };
    if v.get("type").and_then(|t| t.as_str()) == Some("error") {
        let msg = v.pointer("/error/message").and_then(|m| m.as_str()).unwrap_or("anthropic error event");
        return Classified::Error(format!("upstream_error_event: {msg}"));
    }
    match event {
        "message_start" => Classified::Start {
            input_tokens: v.pointer("/message/usage/input_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
        },
        "content_block_start" => {
            let cb = v.get("content_block");
            if cb.and_then(|c| c.get("type")).and_then(|t| t.as_str()) == Some("tool_use") {
                // A non-empty input here is a whole call delivered at once;
                // an empty object means the input streams as input_json_delta.
                let input = cb.and_then(|c| c.get("input")).cloned().unwrap_or(json!({}));
                let args = if input.as_object().map(|o| o.is_empty()).unwrap_or(true) {
                    String::new()
                } else {
                    input.to_string()
                };
                Classified::ToolUseStart {
                    index: v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize,
                    id: cb.and_then(|c| c.get("id")).and_then(|i| i.as_str()).unwrap_or("").to_string(),
                    name: cb.and_then(|c| c.get("name")).and_then(|n| n.as_str()).unwrap_or("").to_string(),
                    args,
                }
            } else { Classified::Ignore }
        }
        "content_block_delta" => {
            let delta = v.get("delta");
            match delta.and_then(|d| d.get("type")).and_then(|t| t.as_str()) {
                Some("text_delta") => Classified::Text(delta.and_then(|d| d.get("text")).and_then(|t| t.as_str()).unwrap_or("").to_string()),
                Some("input_json_delta") => Classified::ToolInput {
                    index: v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize,
                    delta: delta.and_then(|d| d.get("partial_json")).and_then(|p| p.as_str()).unwrap_or("").to_string(),
                },
                _ => Classified::Ignore,
            }
        }
        "message_delta" => {
            let reason = match v.pointer("/delta/stop_reason").and_then(|s| s.as_str()) {
                Some("max_tokens") => FinishReason::Length,
                Some("tool_use") => FinishReason::ToolCall,
                Some("refusal") => FinishReason::ContentFilter,
                _ => FinishReason::Stop,
            };
            let usage = Usage {
                input_tokens: 0,
                output_tokens: v.pointer("/usage/output_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            };
            Classified::Finish { reason, usage }
        }
        "message_stop" => Classified::Done, // terminator only: Finish already came via message_delta
        "ping" => Classified::Ignore,
        _ => Classified::Ignore,
    }
}
