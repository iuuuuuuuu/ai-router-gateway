//! Google Gemini wire format (v1beta generateContent / streamGenerateContent).
//! Entry + exit codec. Gemini streaming: SSE with "data: {candidates...}",
//! each payload is a FULL cumulative object (not deltas) — we diff it.

use super::ir::{Content, FinishReason, Message, Role, ToolChoice, Usage};
use super::{ChatRequest, ChatResponse, StreamEvent};
use serde_json::{json, Value};

pub fn parse_request(body: &Value) -> anyhow::Result<ChatRequest> {
    let obj = body.as_object().ok_or_else(|| anyhow::anyhow!("body must be object"))?;
    // Gateway accepts OpenAI-ish bodies on the Gemini path too, but native
    // Gemini entry is: {"contents":[...], "systemInstruction": {...}}
    let mut system: Vec<Content> = vec![];
    if let Some(si) = obj.get("systemInstruction").or_else(|| obj.get("system_instruction")) {
        for p in si.get("parts").and_then(|v| v.as_array()).unwrap_or(&vec![]) {
            if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                system.push(Content::Text { text: t.to_string() });
            }
        }
    }

    let mut messages: Vec<Message> = vec![];
    for c in obj.get("contents").and_then(|v| v.as_array()).unwrap_or(&vec![]) {
        let role = match c.get("role").and_then(|v| v.as_str()) {
            Some("model") => Role::Assistant,
            _ => Role::User,
        };
        let mut blocks: Vec<Content> = vec![];
        for p in c.get("parts").and_then(|v| v.as_array()).unwrap_or(&vec![]) {
            if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                blocks.push(Content::Text { text: t.to_string() });
            }
            if let Some(fc) = p.get("functionCall") {
                blocks.push(Content::ToolUse {
                    id: format!("call_{}", uuid::Uuid::new_v4().simple()),
                    name: fc.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    input: fc.get("args").cloned().unwrap_or(json!({})),
                });
            }
            if let Some(fr) = p.get("functionResponse") {
                blocks.push(Content::ToolResult {
                    tool_use_id: fr.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    content: fr.pointer("/response/result").map(|r| r.to_string()).unwrap_or_default(),
                    is_error: false,
                });
            }
            if let Some(idata) = p.get("inlineData").or_else(|| p.get("inline_data")) {
                blocks.push(Content::ImageData {
                    media_type: idata.get("mimeType").or_else(|| idata.get("mime_type")).and_then(|v| v.as_str()).unwrap_or("image/png").to_string(),
                    data: idata.get("data").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                });
            }
        }
        messages.push(Message { role, content: blocks, tool_call_id: None });
    }

    let tools = obj
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .flat_map(|t| t.get("functionDeclarations").and_then(|f| f.as_array()).cloned().unwrap_or_default())
                .filter_map(|f| {
                    Some(super::ir::Tool {
                        name: f.get("name").and_then(|v| v.as_str())?.to_string(),
                        description: f.get("description").and_then(|v| v.as_str()).map(String::from),
                        input_schema: f.get("parameters").cloned().unwrap_or(json!({"type":"object"})),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let gen = obj.get("generationConfig");
    let stop = gen
        .and_then(|g| g.get("stopSequences"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect())
        .unwrap_or_default();

    Ok(ChatRequest {
        messages, system,
        max_tokens: gen.and_then(|g| g.get("maxOutputTokens")).and_then(|v| v.as_u64()).map(|v| v as u32),
        temperature: gen.and_then(|g| g.get("temperature")).and_then(|v| v.as_f64()),
        top_p: gen.and_then(|g| g.get("topP")).and_then(|v| v.as_f64()),
        stop,
        stream: false, // caller sets based on :streamGenerateContent path
        tools,
        tool_choice: None, // Gemini uses toolConfig; rarely used — omitted v1
    })
}

pub fn render_upstream_request(req: &ChatRequest, model: &str, stream: bool) -> (String, Value) {
    // returns (path_suffix, body); caller prepends /v1beta/models/{model}
    let _ = model;
    let mut contents: Vec<Value> = vec![];
    for m in &req.messages {
        let role = match m.role { Role::Assistant => "model", _ => "user" };
        let mut parts: Vec<Value> = vec![];
        for c in &m.content {
            match c {
                Content::Text { text } => parts.push(json!({"text": text})),
                Content::ImageUrl { url } => parts.push(json!({"text": format!("[image: {url}]")})),
                Content::ImageData { media_type, data } => {
                    parts.push(json!({"inlineData": {"mimeType": media_type, "data": data}}))
                }
                Content::ToolUse { name, input, .. } => parts.push(json!({"functionCall": {"name": name, "args": input}})),
                Content::ToolResult { tool_use_id, content, .. } => {
                    parts.push(json!({"functionResponse": {"name": tool_use_id, "response": {"result": content}}}))
                }
            }
        }
        contents.push(json!({"role": role, "parts": parts}));
    }

    let mut body = serde_json::Map::new();
    body.insert("contents".into(), json!(contents));
    if !req.system.is_empty() {
        let text = req.system.iter().filter_map(|c| match c { Content::Text { text } => Some(text.clone()), _ => None }).collect::<Vec<_>>().join("\n");
        body.insert("systemInstruction".into(), json!({"parts": [{"text": text}]}));
    }
    let mut gen = serde_json::Map::new();
    if let Some(mt) = req.max_tokens { gen.insert("maxOutputTokens".into(), json!(mt)); }
    if let Some(t) = req.temperature { gen.insert("temperature".into(), json!(t)); }
    if let Some(t) = req.top_p { gen.insert("topP".into(), json!(t)); }
    if !req.stop.is_empty() { gen.insert("stopSequences".into(), json!(req.stop)); }
    if !gen.is_empty() { body.insert("generationConfig".into(), Value::Object(gen)); }
    if !req.tools.is_empty() {
        body.insert("tools".into(), json!([{"functionDeclarations": req.tools.iter().map(|t| json!({
            "name": t.name, "description": t.description, "parameters": t.input_schema
        })).collect::<Vec<_>>()}]));
    }
    let suffix = if stream { ":streamGenerateContent?alt=sse" } else { ":generateContent" };
    (suffix.to_string(), Value::Object(body))
}

pub fn parse_response(body: &Value, model_echo: &str) -> anyhow::Result<ChatResponse> {
    if let Some(e) = body.get("error") {
        anyhow::bail!("upstream_error: {}", e.get("message").and_then(|v| v.as_str()).unwrap_or("gemini error"));
    }
    let cand = body.pointer("/candidates/0").ok_or_else(|| anyhow::anyhow!("no candidates"))?;
    let mut content: Vec<Content> = vec![];
    for p in cand.pointer("/content/parts").and_then(|v| v.as_array()).unwrap_or(&vec![]) {
        if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
            if !t.is_empty() { content.push(Content::Text { text: t.to_string() }); }
        }
        if let Some(fc) = p.get("functionCall") {
            content.push(Content::ToolUse {
                id: format!("call_{}", uuid::Uuid::new_v4().simple()),
                name: fc.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                input: fc.get("args").cloned().unwrap_or(json!({})),
            });
        }
    }
    let finish = match cand.get("finishReason").and_then(|v| v.as_str()) {
        Some("MAX_TOKENS") => FinishReason::Length,
        Some("SAFETY") | Some("RECITATION") => FinishReason::ContentFilter,
        _ => FinishReason::Stop,
    };
    let u = body.get("usageMetadata");
    let usage = Usage {
        input_tokens: u.and_then(|x| x.get("promptTokenCount")).and_then(|v| v.as_u64()).unwrap_or(0),
        output_tokens: u.and_then(|x| x.get("candidatesTokenCount")).and_then(|v| v.as_u64()).unwrap_or(0),
        cache_read_tokens: u.and_then(|x| x.get("cachedContentTokenCount")).and_then(|v| v.as_u64()).unwrap_or(0),
        cache_creation_tokens: 0,
    };
    Ok(ChatResponse { content, finish_reason: finish, usage, model: model_echo.to_string() })
}

pub fn render_response(resp: &ChatResponse) -> Value {
    let parts: Vec<Value> = resp
        .content
        .iter()
        .map(|c| match c {
            Content::Text { text } => json!({"text": text}),
            Content::ToolUse { name, input, .. } => json!({"functionCall": {"name": name, "args": input}}),
            _ => json!({"text": ""}),
        })
        .collect();
    json!({
        "candidates": [{
            "content": {"parts": parts, "role": "model"},
            "finishReason": resp.finish_reason.gemini_name()
        }],
        "usageMetadata": {
            "promptTokenCount": resp.usage.input_tokens,
            "candidatesTokenCount": resp.usage.output_tokens,
            "cachedContentTokenCount": resp.usage.cache_read_tokens
        },
        "modelVersion": resp.model
    })
}

pub fn render_stream_event(_model: &str, ev: &StreamEvent) -> Vec<Value> {
    // Gemini SSE frames each carry a candidates array; we emit one frame per event
    match ev {
        StreamEvent::Start { .. } => vec![],
        StreamEvent::TextDelta { text } => vec![json!({
            "candidates": [{"content": {"parts": [{"text": text}], "role": "model"}}]
        })],
        // Gemini's functionCall is whole-package: name + COMPLETE args in ONE
        // frame. Emitting an empty args object here and dropping the input
        // deltas (the old behavior) silently corrupted every streamed tool call.
        StreamEvent::ToolUseStart { id: _, name, args, .. } => match args {
            Some(a) => vec![json!({
                "candidates": [{"content": {"parts": [{"functionCall": {"name": name, "args": a}}], "role": "model"}}]
            })],
            // Incremental form (OpenAI/Anthropic upstream): the proxy assembles
            // the pieces and re-emits ONE whole-package ToolUseStart.
            None => vec![],
        },
        StreamEvent::ToolInputDelta { .. } => vec![], // assembled by the proxy into one whole-package frame
        StreamEvent::Finish { reason, usage } => vec![json!({
            "candidates": [{"finishReason": reason.gemini_name()}],
            "usageMetadata": {
                "promptTokenCount": usage.input_tokens,
                "candidatesTokenCount": usage.output_tokens
            }
        })],
    }
}

pub enum Classified {
    Ignore,
    Text(String),
    ToolCall { name: String, args: Value },
    Finish { reason: FinishReason, usage: Usage },
    Error(String),
}

pub fn classify_upstream_frame(data: &str) -> Classified {
    let Ok(v) = serde_json::from_str::<Value>(data.trim()) else {
        return Classified::Error(format!("not_sse_garbage: {}", &data.chars().take(80).collect::<String>()));
    };
    if let Some(e) = v.get("error") {
        return Classified::Error(format!("upstream_error_event: {}", e.get("message").and_then(|m| m.as_str()).unwrap_or("gemini error")));
    }
    if let Some(cand) = v.pointer("/candidates/0") {
        if let Some(parts) = cand.pointer("/content/parts").and_then(|p| p.as_array()) {
            for p in parts {
                if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                    if !t.is_empty() { return Classified::Text(t.to_string()); }
                }
                if let Some(fc) = p.get("functionCall") {
                    return Classified::ToolCall {
                        name: fc.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(),
                        args: fc.get("args").cloned().unwrap_or(json!({})),
                    };
                }
            }
        }
        if let Some(fr) = cand.get("finishReason").and_then(|f| f.as_str()) {
            let reason = match fr {
                "MAX_TOKENS" => FinishReason::Length,
                "SAFETY" | "RECITATION" => FinishReason::ContentFilter,
                _ => FinishReason::Stop,
            };
            let usage = Usage {
                input_tokens: v.pointer("/usageMetadata/promptTokenCount").and_then(|x| x.as_u64()).unwrap_or(0),
                output_tokens: v.pointer("/usageMetadata/candidatesTokenCount").and_then(|x| x.as_u64()).unwrap_or(0),
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            };
            return Classified::Finish { reason, usage };
        }
    }
    Classified::Ignore
}
