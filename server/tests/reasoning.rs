// Reasoning ("thinking") passthrough.
//
// The gateway is NOT a transparent proxy: it parses the client body into an IR
// and RE-RENDERS the upstream body from that IR. Any field the IR does not
// model is therefore not "not forwarded" -- it is deleted. These tests exist
// because that is exactly what happened to every thinking knob: a client could
// send reasoning_effort/thinking/budget_tokens and the upstream received none
// of it, in either direction.
//
// Two things are asserted throughout:
//   1. The thinking request reaches the upstream (observed via EchoBody, which
//      replies with the bytes it really received).
//   2. Thinking in the response is kept SEPARATE from the answer text -- never
//      merged into it, never dropped when the entry protocol can carry it.

mod support;

use support::*;

const EXPOSED: &str = support::EXPOSED;

fn rcfg() -> gw_server::config::RetryConfig { test_retry() }

/// POST a raw body and return (status, sse-text). Used where the test must
/// control the exact client JSON (the shared helpers build a fixed body).
async fn post_raw_stream(router: &axum::Router, path: &str, body: serde_json::Value, key_header: (&str, &str)) -> (u16, String) {
    use tower::util::ServiceExt;
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header(key_header.0, key_header.1)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 10 * 1024 * 1024).await.unwrap().to_vec();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// Pull the JSON object the EchoBody upstream embedded in its text delta.
fn echoed_request(sse: &str) -> serde_json::Value {
    for line in sse.lines() {
        let Some(rest) = line.strip_prefix("data: ") else { continue };
        if rest.trim() == "[DONE]" { continue }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(rest) else { continue };
        // The echo rides in the text field of whichever protocol the CLIENT
        // spoke: OpenAI puts it at /choices/0/delta/content, Anthropic at
        // /delta/text.
        let t = v.pointer("/choices/0/delta/content").and_then(|c| c.as_str())
            .or_else(|| v.pointer("/delta/text").and_then(|c| c.as_str()));
        let Some(t) = t else { continue };
        if let Ok(inner) = serde_json::from_str::<serde_json::Value>(t) {
            if let Some(e) = inner.get("echo") {
                // The upstream embeds the raw body as a JSON string, so a
                // string echo is parsed once more to become an object.
                if let Some(s) = e.as_str() {
                    return serde_json::from_str::<serde_json::Value>(s).unwrap_or(serde_json::Value::Null);
                }
                return e.clone();
            }
        }
    }
    panic!("no echoed request in stream: {sse}");
}

// ---------------------------------------------------------------- request side

/// The headline defect: an OpenAI client asking for high reasoning effort had
/// that request deleted before it reached the upstream.
#[tokio::test]
async fn r01_reasoning_effort_reaches_an_openai_upstream() {
    use gw_server::config::Protocol;
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::EchoBody]).await;
    let cfg = test_config(&base, &prefixes, &[Protocol::Openai], 1, EXPOSED, rcfg());
    let (_st, router) = gateway(cfg).await;

    let body = serde_json::json!({
        "model": EXPOSED, "stream": true, "max_tokens": 50,
        "messages": [{"role": "user", "content": "hi"}],
        "reasoning_effort": "high"
    });
    let (status, sse) = post_raw_stream(&router, "/v1/chat/completions", body, ("authorization", "Bearer gw-test-key")).await;
    assert_eq!(status, 200, "sse={sse}");
    let up = echoed_request(&sse);
    assert_eq!(up["reasoning_effort"], "high", "the effort must survive the IR re-render: {up}");
}

/// A client that speaks OpenAI and also sends Anthropic-shaped thinking (some
/// clients do both) must have BOTH fields forwarded verbatim.
#[tokio::test]
async fn r02_client_fields_are_replayed_verbatim() {
    use gw_server::config::Protocol;
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::EchoBody]).await;
    let cfg = test_config(&base, &prefixes, &[Protocol::Openai], 1, EXPOSED, rcfg());
    let (_st, router) = gateway(cfg).await;

    let body = serde_json::json!({
        "model": EXPOSED, "stream": true, "max_tokens": 50,
        "messages": [{"role": "user", "content": "hi"}],
        "thinking": {"type": "enabled", "budget_tokens": 8000},
        "reasoning_effort": "medium"
    });
    let (status, sse) = post_raw_stream(&router, "/v1/chat/completions", body, ("authorization", "Bearer gw-test-key")).await;
    assert_eq!(status, 200, "sse={sse}");
    let up = echoed_request(&sse);
    assert_eq!(up["reasoning_effort"], "medium");
    assert_eq!(up.pointer("/thinking/budget_tokens").and_then(|v| v.as_u64()), Some(8000),
        "the exact client object must be replayed, not re-derived: {up}");
}

/// A request with NO thinking knob must not gain one: the gateway must not
/// invent a reasoning_effort for every ordinary call.
#[tokio::test]
async fn r03_no_thinking_request_stays_absent() {
    use gw_server::config::Protocol;
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::EchoBody]).await;
    let cfg = test_config(&base, &prefixes, &[Protocol::Openai], 1, EXPOSED, rcfg());
    let (_st, router) = gateway(cfg).await;

    let body = serde_json::json!({
        "model": EXPOSED, "stream": true, "max_tokens": 50,
        "messages": [{"role": "user", "content": "hi"}]
    });
    let (status, sse) = post_raw_stream(&router, "/v1/chat/completions", body, ("authorization", "Bearer gw-test-key")).await;
    assert_eq!(status, 200, "sse={sse}");
    let up = echoed_request(&sse);
    assert!(up.get("reasoning_effort").is_none(), "no effort was asked for: {up}");
    assert!(up.get("thinking").is_none(), "no thinking was asked for: {up}");
    assert!(up.get("reasoning_content").is_none());
}

/// An Anthropic client's thinking budget must be translated for an OpenAI
/// upstream rather than silently dropped.
#[tokio::test]
async fn r04_anthropic_budget_translates_to_openai_effort() {
    use gw_server::config::Protocol;
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::EchoBody]).await;
    let cfg = test_config(&base, &prefixes, &[Protocol::Openai], 1, EXPOSED, rcfg());
    let (_st, router) = gateway(cfg).await;

    let body = serde_json::json!({
        "model": EXPOSED, "stream": true, "max_tokens": 50,
        "messages": [{"role": "user", "content": "hi"}],
        "thinking": {"type": "enabled", "budget_tokens": 20000}
    });
    let (status, sse) = post_raw_stream(&router, "/v1/messages", body, ("x-api-key", "gw-test-key")).await;
    assert_eq!(status, 200, "sse={sse}");
    let up = echoed_request(&sse);
    assert_eq!(up["reasoning_effort"], "high",
        "a 20000-token budget is a high effort: {up}");
}


// --------------------------------------------------------------- response side

/// Upstream reasoning_content must reach the client as reasoning, and must NOT
/// be appended to the answer text.
#[tokio::test]
async fn r05_openai_stream_reasoning_is_forwarded_separately() {
    use gw_server::config::Protocol;
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::ReasoningStream]).await;
    let cfg = test_config(&base, &prefixes, &[Protocol::Openai], 1, EXPOSED, rcfg());
    let (_st, router) = gateway(cfg).await;

    let (status, sse) = post_chat_stream(&router, EXPOSED).await;
    assert_eq!(status, 200);
    assert!(sse.contains("let me think"), "reasoning must reach the client: {sse}");
    assert!(sse.contains("about this"));
    assert!(sse.contains("\"reasoning_content\""), "sent under its own field name: {sse}");
    // The answer text must be exactly the answer -- reasoning must not be
    // concatenated into content, or the client's reply becomes the scratchpad.
    let content: String = sse.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v.pointer("/choices/0/delta/content").and_then(|c| c.as_str()).map(String::from))
        .collect();
    assert_eq!(content, "the answer", "content must carry only the answer");
}

/// A frame carrying ONLY reasoning used to be classified as ignorable and
/// dropped entirely. It must arrive.
#[tokio::test]
async fn r06_reasoning_only_frame_is_not_dropped() {
    use gw_server::config::Protocol;
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::ReasoningOnly]).await;
    let cfg = test_config(&base, &prefixes, &[Protocol::Openai], 1, EXPOSED, rcfg());
    let (_st, router) = gateway(cfg).await;

    let (status, sse) = post_chat_stream(&router, EXPOSED).await;
    assert_eq!(status, 200);
    assert!(sse.contains("THOUGHT-MARKER"), "a reasoning-only frame must not vanish: {sse}");
    assert!(sse.contains("final"));
}

/// Anthropic's thinking_delta is a different type name from OpenAI's; both must
/// be recognized.
#[tokio::test]
async fn r07_anthropic_thinking_delta_is_forwarded() {
    use gw_server::config::Protocol;
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodAnthropicReasoningStream]).await;
    let cfg = test_config(&base, &prefixes, &[Protocol::Anthropic], 1, EXPOSED, rcfg());
    let (_st, router) = gateway(cfg).await;

    let (status, resp) = post_messages(&router, EXPOSED, true).await;
    assert_eq!(status, 200);
    let sse = body_string(resp).await;
    assert!(sse.contains("ANTHRO-THOUGHT"), "thinking_delta must reach the client: {sse}");
    assert!(sse.contains("thinking_delta"), "kept under its own delta type: {sse}");
    assert!(sse.contains("answer"));
}

/// Gemini flags thinking with "thought": true inside the ordinary parts array.
/// Miss it and the model's scratchpad is served as its answer.
#[tokio::test]
async fn r08_gemini_thought_part_is_not_the_answer() {
    use gw_server::config::Protocol;
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodGeminiReasoningStream]).await;
    let cfg = test_config(&base, &prefixes, &[Protocol::Gemini], 1, EXPOSED, rcfg());
    let (_st, router) = gateway(cfg).await;

    let body = serde_json::json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
    let (status, sse) = post_raw_stream(&router, &format!("/v1beta/models/{EXPOSED}:streamGenerateContent"), body, ("x-goog-api-key", "gw-test-key")).await;
    assert_eq!(status, 200, "sse={sse}");
    assert!(sse.contains("GEMINI-THOUGHT"), "thought part must reach the client: {sse}");
    assert!(sse.contains("\"thought\":true"), "and stay flagged as a thought: {sse}");
    // The answer part must not contain the thought text.
    let answers: String = sse.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v.pointer("/candidates/0/content/parts/0/text").and_then(|t| t.as_str()).map(String::from))
        .filter(|t| t != "GEMINI-THOUGHT")
        .collect();
    assert_eq!(answers, "answer", "the answer must be only the answer");
}


// -------------------------------------------------------------- buffered path

/// Buffered OpenAI: reasoning_content must not be merged into the answer.
#[tokio::test]
async fn r09_buffered_openai_reasoning_is_not_the_answer() {
    use gw_server::config::Protocol;
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodOpenAiReasoningJson]).await;
    let cfg = test_config(&base, &prefixes, &[Protocol::Openai], 1, EXPOSED, rcfg());
    let (_st, router) = gateway(cfg).await;

    let (status, body) = post_chat(&router, EXPOSED, false).await;
    assert_eq!(status, 200, "body={body}");
    assert_eq!(body["choices"][0]["message"]["content"], "json answer",
        "the answer must be only the answer: {body}");
    assert!(!body.to_string().contains("JSON-THOUGHT"),
        "the scratchpad must not be served as the reply: {body}");
}

/// Buffered Anthropic: a thinking block must be dropped, and must NOT become a
/// spurious empty text block (which would corrupt the content array).
#[tokio::test]
async fn r10_buffered_anthropic_thinking_is_dropped_cleanly() {
    use gw_server::config::Protocol;
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodAnthropicReasoningJson]).await;
    let cfg = test_config(&base, &prefixes, &[Protocol::Anthropic], 1, EXPOSED, rcfg());
    let (_st, router) = gateway(cfg).await;

    let (status, resp) = post_messages(&router, EXPOSED, false).await;
    assert_eq!(status, 200);
    let body = body_json(resp).await;
    let blocks = body["content"].as_array().expect("content must be an array");
    assert_eq!(blocks.len(), 1, "no placeholder block may appear: {body}");
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[0]["text"], "anthropic answer");
    assert!(!body.to_string().contains("ANTHRO-JSON-THOUGHT"),
        "unsigned thinking must not be replayed: {body}");
}

/// Gemini CAN carry a thought in a buffered body, so it is forwarded with the
/// flag rather than dropped -- and never as an empty placeholder part.
#[tokio::test]
async fn r11_buffered_gemini_thought_keeps_its_flag() {
    use gw_server::config::Protocol;
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodGeminiJson]).await;
    let cfg = test_config(&base, &prefixes, &[Protocol::Gemini], 1, EXPOSED, rcfg());
    let (_st, router) = gateway(cfg).await;

    let body = serde_json::json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
    let (status, resp) = post_raw_buffered(&router, &format!("/v1beta/models/{EXPOSED}:generateContent"), body, ("x-goog-api-key", "gw-test-key")).await;
    assert_eq!(status, 200, "body={resp}");
    // The control fixture has no thought, so nothing may be invented.
    assert!(!resp.to_string().contains("thought"), "no thought was sent: {resp}");
    assert!(resp["candidates"][0]["content"]["parts"][0]["text"].is_string());
}

/// Translate a raw buffered POST and parse the JSON reply.
async fn post_raw_buffered(router: &axum::Router, path: &str, body: serde_json::Value, key: (&str, &str)) -> (u16, serde_json::Value) {
    use tower::util::ServiceExt;
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header(key.0, key.1)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 10 * 1024 * 1024).await.unwrap().to_vec();
    (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}


/// A delta carrying BOTH content and reasoning_content in the same frame: the
/// classifier can only report one variant per frame, and returning the
/// reasoning first silently deleted the answer text.
#[tokio::test]
async fn r12_same_frame_reasoning_and_text_keeps_both() {
    use gw_server::config::Protocol;
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::ReasoningAndTextSameFrame]).await;
    let cfg = test_config(&base, &prefixes, &[Protocol::Openai], 1, EXPOSED, rcfg());
    let (_st, router) = gateway(cfg).await;

    let (status, sse) = post_chat_stream(&router, EXPOSED).await;
    assert_eq!(status, 200);
    let content: String = sse.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v.pointer("/choices/0/delta/content").and_then(|c| c.as_str()).map(String::from))
        .collect();
    assert_eq!(content, "BOTH-TEXT tail", "the text in a mixed delta must survive");
    let reasoning: String = sse.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v.pointer("/choices/0/delta/reasoning_content").and_then(|c| c.as_str()).map(String::from))
        .collect();
    assert_eq!(reasoning, "BOTH-THOUGHT", "and so must the reasoning");
    // And the answer must end cleanly, not as a repaired stream.
    assert!(!sse.contains("gateway_upstream_interrupted"), "spurious repair frame: {sse}");
    assert!(sse.contains("\"finish_reason\":\"stop\""), "clean finish expected: {sse}");
}

