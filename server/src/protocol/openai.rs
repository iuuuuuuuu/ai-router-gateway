//! OpenAI wire format codec (/v1/chat/completions).
//! Entry: client JSON -> ChatRequest. Exit: ChatResponse / StreamEvent -> wire.

use super::ir::{Content, FinishReason, Message, Role, ToolChoice, Usage};
use super::{ChatRequest, ChatResponse, StreamEvent};
use serde_json::{json, Value};

pub fn parse_request(body: &Value) -> anyhow::Result<ChatRequest> {
    let obj = body.as_object().ok_or_else(|| anyhow::anyhow!("request body must be a JSON object"))?;
    let msgs = obj
        .get("messages")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("messages array is required"))?;

    let mut system: Vec<Content> = vec![];
    let mut messages: Vec<Message> = vec![];

    for m in msgs {
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
        match role {
            "system" | "developer" => {
                if let Some(t) = extract_text(m.get("content")) {
                    system.push(Content::Text { text: t });
                }
            }
            "assistant" => {
                let mut blocks: Vec<Content> = vec![];
                if let Some(t) = extract_text(m.get("content")) {
                    if !t.is_empty() { blocks.push(Content::Text { text: t }); }
                }
                if let Some(tcs) = m.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tcs {
                        let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let name = tc.pointer("/function/name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let args = tc.pointer("/function/arguments").cloned().unwrap_or(json!({}));
                        let input = if args.is_string() {
                            serde_json::from_str(args.as_str().unwrap()).unwrap_or(json!({}))
                        } else { args };
                        blocks.push(Content::ToolUse { id, name, input });
                    }
                }
                messages.push(Message { role: Role::Assistant, content: blocks, tool_call_id: None });
            }
            "tool" => {
                let content_text = extract_text(m.get("content")).unwrap_or_default();
                messages.push(Message {
                    role: Role::Tool,
                    content: vec![Content::ToolResult {
                        tool_use_id: m.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        content: content_text,
                        is_error: false,
                    }],
                    tool_call_id: m.get("tool_call_id").and_then(|v| v.as_str()).map(String::from),
                });
            }
            _ => {
                // user; content may be string or multimodal array
                let content_val = m.get("content").cloned().unwrap_or(Value::Null);
                let mut blocks: Vec<Content> = vec![];
                match content_val {
                    Value::String(s) => blocks.push(Content::Text { text: s }),
                    Value::Array(parts) => {
                        for p in parts {
                            let ptype = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            match ptype {
                                "text" => blocks.push(Content::Text {
                                    text: p.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                                }),
                                "image_url" => {
                                    let url = p.pointer("/image_url/url").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                    if let Some(data) = url.strip_prefix("data:") {
                                        // data:image/png;base64,xxxx
                                        if let Some((meta, b64)) = data.split_once(',') {
                                            let media_type = meta.split(';').next().unwrap_or("image/png").to_string();
                                            blocks.push(Content::ImageData { media_type, data: b64.to_string() });
                                        }
                                    } else {
                                        blocks.push(Content::ImageUrl { url });
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                messages.push(Message { role: Role::User, content: blocks, tool_call_id: None });
            }
        }
    }

    let tools = obj
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    let f = t.get("function")?;
                    Some(super::ir::Tool {
                        name: f.get("name").and_then(|v| v.as_str())?.to_string(),
                        description: f.get("description").and_then(|v| v.as_str()).map(String::from),
                        input_schema: f.get("parameters").cloned().unwrap_or(json!({"type":"object","properties":{}})),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let tool_choice = match obj.get("tool_choice") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => match s.as_str() {
            "auto" => Some(ToolChoice::Auto),
            "none" => Some(ToolChoice::None),
            "required" => Some(ToolChoice::Required),
            _ => None,
        },
        Some(other) => other.pointer("/function/name").and_then(|v| v.as_str()).map(|n| ToolChoice::Tool(n.to_string())),
    };

    let stop = match obj.get("stop") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
        _ => vec![],
    };

    Ok(ChatRequest {
        messages,
        system,
        max_tokens: obj.get("max_tokens").or_else(|| obj.get("max_completion_tokens")).and_then(|v| v.as_u64()).map(|v| v as u32),
        temperature: obj.get("temperature").and_then(|v| v.as_f64()),
        top_p: obj.get("top_p").and_then(|v| v.as_f64()),
        stop,
        stream: obj.get("stream").and_then(|v| v.as_bool()).unwrap_or(false),
        tools,
        tool_choice,
    })
}

fn extract_text(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let mut out = String::new();
            for p in parts {
                if p.get("type").and_then(|v| v.as_str()) == Some("text") {
                    out.push_str(p.get("text").and_then(|v| v.as_str()).unwrap_or(""));
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// Render ChatRequest back into an OpenAI upstream request body.
pub fn render_upstream_request(req: &ChatRequest, model: &str, stream: bool) -> Value {
    let mut messages: Vec<Value> = vec![];
    if !req.system.is_empty() {
        let text = req
            .system
            .iter()
            .map(|c| match c { Content::Text { text } => text.clone(), _ => String::new() })
            .collect::<Vec<_>>()
            .join("\n");
        messages.push(json!({"role": "system", "content": text}));
    }
    for m in &req.messages {
        match m.role {
            Role::User => {
                let all_text = m.content.iter().all(|c| matches!(c, Content::Text { .. }));
                if all_text {
                    let text = m
                        .content
                        .iter()
                        .map(|c| match c { Content::Text { text } => text.clone(), _ => String::new() })
                        .collect::<Vec<_>>()
                        .join("");
                    messages.push(json!({"role": "user", "content": text}));
                } else {
                    let parts: Vec<Value> = m
                        .content
                        .iter()
                        .map(|c| match c {
                            Content::Text { text } => json!({"type": "text", "text": text}),
                            Content::ImageUrl { url } => json!({"type": "image_url", "image_url": {"url": url}}),
                            Content::ImageData { media_type, data } => {
                                json!({"type": "image_url", "image_url": {"url": format!("data:{};base64,{}", media_type, data)}})
                            }
                            _ => json!({"type": "text", "text": ""}),
                        })
                        .collect();
                    messages.push(json!({"role": "user", "content": parts}));
                }
            }
            Role::Assistant => {
                let mut obj = serde_json::Map::new();
                obj.insert("role".into(), json!("assistant"));
                let text = m
                    .content
                    .iter()
                    .filter_map(|c| match c { Content::Text { text } => Some(text.clone()), _ => None })
                    .collect::<Vec<_>>()
                    .join("");
                obj.insert("content".into(), if text.is_empty() { Value::Null } else { json!(text) });
                let tool_uses: Vec<Value> = m
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        Content::ToolUse { id, name, input } => Some(json!({
                            "id": id, "type": "function",
                            "function": {"name": name, "arguments": serde_json::to_string(input).unwrap_or_default()}
                        })),
                        _ => None,
                    })
                    .collect();
                if !tool_uses.is_empty() {
                    obj.insert("tool_calls".into(), json!(tool_uses));
                }
                messages.push(Value::Object(obj));
            }
            Role::Tool => {
                for c in &m.content {
                    if let Content::ToolResult { tool_use_id, content, .. } = c {
                        messages.push(json!({"role": "tool", "tool_call_id": tool_use_id, "content": content}));
                    }
                }
            }
        }
    }

    let mut body = serde_json::Map::new();
    body.insert("model".into(), json!(model));
    body.insert("messages".into(), json!(messages));
    body.insert("stream".into(), json!(stream));
    if let Some(mt) = req.max_tokens { body.insert("max_tokens".into(), json!(mt)); }
    if let Some(t) = req.temperature { body.insert("temperature".into(), json!(t)); }
    if let Some(t) = req.top_p { body.insert("top_p".into(), json!(t)); }
    if !req.stop.is_empty() { body.insert("stop".into(), json!(req.stop)); }
    if stream {
        // ask for usage in the final chunk (OpenAI spec)
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.input_schema}}))
            .collect();
        body.insert("tools".into(), json!(tools));
        if let Some(tc) = &req.tool_choice {
            body.insert("tool_choice".into(), match tc {
                ToolChoice::Auto => json!("auto"),
                ToolChoice::None => json!("none"),
                ToolChoice::Required => json!("required"),
                ToolChoice::Tool(n) => json!({"type": "function", "function": {"name": n}}),
            });
        }
    }
    Value::Object(body)
}

pub fn parse_response(body: &Value, model_echo: &str) -> anyhow::Result<ChatResponse> {
    // 200-with-error detection happens before this in the proxy; here we
    // additionally catch nested error shapes.
    if let Some(err) = body.get("error") {
        let msg = err.get("message").and_then(|v| v.as_str()).unwrap_or("upstream error");
        anyhow::bail!("upstream_error: {}", msg);
    }
    let choice = body
        .pointer("/choices/0")
        .ok_or_else(|| anyhow::anyhow!("empty choices"))?;
    let message = choice.get("message").cloned().unwrap_or(json!({}));

    let mut content: Vec<Content> = vec![];
    if let Some(t) = message.get("content").and_then(|v| v.as_str()) {
        if !t.is_empty() { content.push(Content::Text { text: t.to_string() }); }
    }
    if let Some(tcs) = message.get("tool_calls").and_then(|v| v.as_array()) {
        for tc in tcs {
            let args = tc.pointer("/function/arguments").cloned().unwrap_or(json!({}));
            let input = if args.is_string() {
                serde_json::from_str(args.as_str().unwrap()).unwrap_or(json!({}))
            } else { args };
            content.push(Content::ToolUse {
                id: tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                name: tc.pointer("/function/name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                input,
            });
        }
    }

    let finish = match choice.get("finish_reason").and_then(|v| v.as_str()) {
        Some("length") => FinishReason::Length,
        Some("tool_calls") | Some("function_call") => FinishReason::ToolCall,
        Some("content_filter") => FinishReason::ContentFilter,
        _ => FinishReason::Stop,
    };

    let usage = parse_usage(body.get("usage"));
    Ok(ChatResponse { content, finish_reason: finish, usage, model: model_echo.to_string() })
}

pub fn parse_usage(u: Option<&Value>) -> Usage {
    let Some(u) = u else { return Usage::default() };
    Usage {
        input_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        output_tokens: u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        cache_read_tokens: u.pointer("/prompt_tokens_details/cached_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        cache_creation_tokens: 0,
    }
}

/// Render a full (non-stream) response for an OpenAI client.
pub fn render_response(resp: &ChatResponse) -> Value {
    let mut message = serde_json::Map::new();
    let text: String = resp
        .content
        .iter()
        .filter_map(|c| match c { Content::Text { text } => Some(text.clone()), _ => None })
        .collect::<Vec<_>>()
        .join("");
    message.insert("role".into(), json!("assistant"));
    message.insert("content".into(), if text.is_empty() { Value::Null } else { json!(text) });
    let tcs: Vec<Value> = resp
        .content
        .iter()
        .filter_map(|c| match c {
            Content::ToolUse { id, name, input } => Some(json!({
                "id": id, "type": "function",
                "function": {"name": name, "arguments": serde_json::to_string(input).unwrap_or_default()}
            })),
            _ => None,
        })
        .collect();
    if !tcs.is_empty() { message.insert("tool_calls".into(), json!(tcs)); }

    json!({
        "id": format!("chatcmpl-gw{}", uuid::Uuid::new_v4().simple()),
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": resp.model,
        "choices": [{"index": 0, "message": Value::Object(message), "finish_reason": resp.finish_reason.openai_name()}],
        "usage": {
            "prompt_tokens": resp.usage.input_tokens,
            "completion_tokens": resp.usage.output_tokens,
            "total_tokens": resp.usage.input_tokens + resp.usage.output_tokens,
            "prompt_tokens_details": {"cached_tokens": resp.usage.cache_read_tokens}
        }
    })
}

/// Render one IR event as OpenAI SSE frame(s). Returns Vec of "data: {...}" JSON
/// strings; Finish emits the usage chunk and [DONE] is appended by the caller.
pub fn render_stream_event(model: &str, ev: &StreamEvent) -> Vec<Value> {
    match ev {
        StreamEvent::Start { .. } => vec![], // OpenAI has no start frame; first delta carries id
        StreamEvent::TextDelta { text } => vec![json!({
            "id": format!("chatcmpl-gw{}", uuid::Uuid::new_v4().simple()),
            "object": "chat.completion.chunk",
            "created": chrono::Utc::now().timestamp(),
            "model": model,
            "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
        })],
        StreamEvent::ToolUseStart { index, id, name, args } => {
            // A Gemini upstream delivers the whole call at once; OpenAI's wire
            // form carries that same object as a JSON *string* in arguments.
            // One frame holding the complete arguments is valid.
            let arguments = match args { Some(a) => a.to_string(), None => String::new() };
            vec![json!({
                "id": format!("chatcmpl-gw{}", uuid::Uuid::new_v4().simple()),
                "object": "chat.completion.chunk",
                "created": chrono::Utc::now().timestamp(),
                "model": model,
                "choices": [{"index": 0, "delta": {"tool_calls": [{"index": index, "id": id, "type": "function", "function": {"name": name, "arguments": arguments}}]}, "finish_reason": null}]
            })]
        }
        StreamEvent::ToolInputDelta { index, partial_json } => vec![json!({
            "id": format!("chatcmpl-gw{}", uuid::Uuid::new_v4().simple()),
            "object": "chat.completion.chunk",
            "created": chrono::Utc::now().timestamp(),
            "model": model,
            "choices": [{"index": 0, "delta": {"tool_calls": [{"index": index, "function": {"arguments": partial_json}}]}, "finish_reason": null}]
        })],
        StreamEvent::Finish { reason, usage } => {
            let mut frames = vec![json!({
                "id": format!("chatcmpl-gw{}", uuid::Uuid::new_v4().simple()),
                "object": "chat.completion.chunk",
                "created": chrono::Utc::now().timestamp(),
                "model": model,
                "choices": [{"index": 0, "delta": {}, "finish_reason": reason.openai_name()}]
            })];
            if usage.input_tokens > 0 || usage.output_tokens > 0 {
                frames.push(json!({
                    "id": format!("chatcmpl-gw{}", uuid::Uuid::new_v4().simple()),
                    "object": "chat.completion.chunk",
                    "created": chrono::Utc::now().timestamp(),
                    "model": model,
                    "choices": [],
                    "usage": {
                        "prompt_tokens": usage.input_tokens,
                        "completion_tokens": usage.output_tokens,
                        "total_tokens": usage.input_tokens + usage.output_tokens
                    }
                }));
            }
            frames
        }
    }
}

/// Classify one upstream SSE data payload: None = content/ignorable,
/// Some(Err) = error frame, Some(Ok(evt)) = normal event.
pub enum Classified {
    Ignore,
    Text(String),
    /// args carries any arguments that arrived in the SAME frame as the name
    /// (providers that emit a whole call at once); it is empty when the
    /// arguments follow in later ToolInput frames.
    ToolUseStart { index: usize, id: String, name: String, args: String },
    ToolInput { index: usize, delta: String },
    Finish { reason: FinishReason, usage: Usage },
    Error(String),
    Done,
}

/// Classify a single upstream OpenAI SSE "data:" payload.
pub fn classify_upstream_frame(data: &str) -> Classified {
    let trimmed = data.trim();
    if trimmed == "[DONE]" { return Classified::Done; }
    let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
        return Classified::Error(format!("not_sse_garbage: {}",
            &trimmed.chars().take(80).collect::<String>()));
    };
    if let Some(err) = v.get("error") {
        let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("upstream error event");
        let code = err.get("code").and_then(|c| c.as_str()).unwrap_or("");
        return Classified::Error(format!("upstream_error_event({code}): {msg}"));
    }
    // content deltas
    if let Some(choice) = v.pointer("/choices/0") {
        if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            let reason = match fr {
                "length" => FinishReason::Length,
                "tool_calls" | "function_call" => FinishReason::ToolCall,
                "content_filter" => FinishReason::ContentFilter,
                _ => FinishReason::Stop,
            };
            // usage chunk may ride along in the same frame
            let usage = parse_usage(v.get("usage"));
            return Classified::Finish { reason, usage };
        }
        if let Some(delta) = choice.get("delta") {
            if let Some(t) = delta.get("content").and_then(|c| c.as_str()) {
                if !t.is_empty() { return Classified::Text(t.to_string()); }
            }
            if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tcs {
                    let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                    if let Some(f) = tc.get("function") {
                        let name = f.get("name").and_then(|n| n.as_str()).unwrap_or("");
                        let args = f.get("arguments").and_then(|a| a.as_str()).unwrap_or("");
                        if !name.is_empty() {
                            let id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("");
                            // Keep same-frame arguments: dropping them hands the
                            // client a call whose arguments never existed.
                            return Classified::ToolUseStart {
                                index: idx, id: id.to_string(), name: name.to_string(), args: args.to_string(),
                            };
                        }
                        if !args.is_empty() {
                            return Classified::ToolInput { index: idx, delta: args.to_string() };
                        }
                    }
                }
            }
        }
        return Classified::Ignore;
    }
    // usage-only frame (choices empty)
    if v.get("usage").is_some() && v.pointer("/choices").map(|c| c.as_array().map(|a| a.is_empty()).unwrap_or(true)).unwrap_or(false) {
        return Classified::Ignore; // usage merged at finish
    }
    Classified::Ignore
}
