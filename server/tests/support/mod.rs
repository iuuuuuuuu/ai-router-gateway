//! Shared test scaffolding: fake upstreams + gateway harness.
use gw_server::config::{ApiKey, Channel, Config, Group, GroupOverrides, ModelBinding, Protocol, RetryConfig, Tier, TierMember};
use gw_server::health::HealthRegistry;
use gw_server::proxy::AppState;
use gw_server::store::Store;
use std::sync::Arc;
use std::time::Duration;

/// What a fake upstream does when hit.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Behavior {
    /// 200 + full OpenAI JSON completion.
    GoodJson,
    /// 200 + OpenAI SSE: two text deltas + finish + usage + [DONE].
    GoodStream,
    /// HTTP 500.
    Fail500,
    /// HTTP 429 with Retry-After: 1.
    Fail429,
    /// HTTP 401.
    Fail401,
    /// HTTP 404.
    Fail404,
    /// Accept, then hang forever (TTFB timeout path).
    Hang,
    /// 200 but body is garbage (neither JSON nor SSE).
    Garbage,
    /// 200 with an empty body.
    Empty,
    /// 200 + SSE with one text delta, then EOF without finish (mid-stream break).
    StreamBreakMid,
    /// 200 + SSE carrying an OpenAI error frame, then [DONE].
    StreamErrorFrame,
    /// 200 + SSE that ends (EOF) before any content.
    StreamEofEarly,
    GoodAnthropicJson,
    GoodAnthropicStream,
    GoodGeminiJson,
    GoodGeminiStream,
    /// Gemini SSE with a WHOLE tool call (name + complete args in one frame).
    GoodGeminiToolStream,
    /// OpenAI SSE whose tool-call arguments arrive as fragments across frames.
    GoodOpenAiToolStream,
    /// OpenAI-compatible SSE that delivers name AND complete arguments in ONE
    /// frame (some providers do this).
    GoodOpenAiToolOneFrame,
    /// Anthropic SSE: content_block_start (empty input) + input_json_delta.
    GoodAnthropicToolStream,
    /// Degenerate: the same short sentence repeated across 40 text frames.
    DegenerateShortCycle,
    /// Degenerate: one single character repeated across 80 frames.
    DegenerateSingleChar,
    /// Control: ~2000 characters of normal prose that never repeats (a false
    /// positive here would be worse than the bug this feature fixes).
    NormalLongText,
    /// Control: enough normal text to COMMIT, then a degenerate repetition —
    /// detection is pre-commit only, so this repetition must reach the client
    /// unchanged (the honest boundary).
    NormalThenDegenerate,
    /// B1 regression: text shorter than the holdback, then a tool call. The
    /// buffered text must be committed WITH the tool call, never dropped.
    ToolAfterShortText,
}

async fn handle_beh(beh: Behavior, body: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    let _ = body;
    match beh {
        Behavior::GoodJson => (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({
                "id": "chatcmpl-x", "object": "chat.completion",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "hello from json"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 11, "completion_tokens": 7}
            })),
        ).into_response(),
        Behavior::GoodStream => {
            let sse = concat!(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"he\"}}]}\n\n",
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"llo\"}}]}\n\n",
                "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
                "data: [DONE]\n\n"
            );
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        Behavior::Fail500 => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response(),
        Behavior::Fail429 => {
            let mut resp = (axum::http::StatusCode::TOO_MANY_REQUESTS, "slow down").into_response();
            resp.headers_mut().insert(axum::http::header::RETRY_AFTER, axum::http::HeaderValue::from_static("1"));
            resp
        }
        Behavior::Fail401 => (axum::http::StatusCode::UNAUTHORIZED, "bad key").into_response(),
        Behavior::Fail404 => (axum::http::StatusCode::NOT_FOUND, "{\"error\":{\"message\":\"model not found\"}}").into_response(),
        Behavior::Hang => { tokio::time::sleep(Duration::from_secs(120)).await; (axum::http::StatusCode::OK, "late").into_response() }
        Behavior::Garbage => (axum::http::StatusCode::OK, "<html>not json</html>").into_response(),
        Behavior::Empty => (axum::http::StatusCode::OK, "").into_response(),
        Behavior::StreamBreakMid => {
            let sse = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n";
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        Behavior::StreamErrorFrame => {
            let sse = "data: {\"error\":{\"message\":\"upstream blew up\",\"code\":\"x\"}}\n\ndata: [DONE]\n\n";
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        Behavior::GoodAnthropicJson => (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({
                "id": "msg_x", "type": "message", "role": "assistant",
                "content": [{"type": "text", "text": "bonjour from anthropic"}],
                "model": "claude-fake", "stop_reason": "end_turn", "stop_sequence": null,
                "usage": {"input_tokens": 12, "output_tokens": 6}
            })),
        ).into_response(),
        Behavior::GoodAnthropicStream => {
            let sse = concat!(
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-fake\",\"content\":[],\"usage\":{\"input_tokens\":12,\"output_tokens\":0}}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"bonjour \"}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"stream\"}}\n\n",
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":6}}\n\n",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
            );
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        Behavior::GoodGeminiJson => (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({
                "candidates": [{"content": {"parts": [{"text": "hallo from gemini"}], "role": "model"}, "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 9, "candidatesTokenCount": 4}
            })),
        ).into_response(),
        Behavior::GoodGeminiStream => {
            let sse = concat!(
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"guten\"}],\"role\":\"model\"}}]}\n\n",
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" tag\"}],\"role\":\"model\"}}]}\n\n",
                "data: {\"candidates\":[{\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":9,\"candidatesTokenCount\":4}}\n\n"
            );
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        Behavior::StreamEofEarly => {
            let sse = "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n";
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        // Tool-call fixtures are built with serde_json so the nested JSON
        // inside an arguments string cannot be mis-escaped by hand.
        Behavior::GoodGeminiToolStream => {
            let frames = [
                serde_json::json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"get_weather","args":{"city":"Tokyo","unit":"c"}}}],"role":"model"}}]}).to_string(),
                serde_json::json!({"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":9,"candidatesTokenCount":4}}).to_string(),
            ];
            let sse: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        Behavior::GoodOpenAiToolStream => {
            let frames = [
                serde_json::json!({"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"get_weather","arguments":""}}]}}]}).to_string(),
                serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]}}]}).to_string(),
                serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"Paris\"}"}}]}}]}).to_string(),
                serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":8,"completion_tokens":3}}).to_string(),
                "[DONE]".to_string(),
            ];
            let sse: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        Behavior::GoodOpenAiToolOneFrame => {
            let frames = [
                serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Rome\"}"}}]}}]}).to_string(),
                serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":5,"completion_tokens":2}}).to_string(),
                "[DONE]".to_string(),
            ];
            let sse: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        Behavior::GoodAnthropicToolStream => {
            let frames = [
                ("message_start", serde_json::json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-fake","content":[],"usage":{"input_tokens":12,"output_tokens":0}}})),
                ("content_block_start", serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{}}})),
                ("content_block_delta", serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}})),
                ("content_block_delta", serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"Oslo\"}"}})),
                ("message_delta", serde_json::json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}})),
                ("message_stop", serde_json::json!({"type":"message_stop"})),
            ];
            let sse: String = frames.iter().map(|(ev, d)| format!("event: {ev}\ndata: {d}\n\n")).collect();
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        // ---- loop-guard fixtures (Task B) ----
        // Every fixture below is a VALID OpenAI SSE stream (frames separated by a
        // blank line, terminated by [DONE]) because the gateway parses it with a
        // real eventsource decoder, not by string splitting.
        Behavior::DegenerateShortCycle => {
            // 40 frames of the SAME short sentence. With min_chars = 64 the
            // detector's first possible verdict is at exactly 64 buffered
            // characters = 16 frames of 4 chars, so this is the realistic shape
            // of a model stuck in a repetition loop.
            let mut sse = String::new();
            for _ in 0..40 { sse.push_str(&openai_text_chunk("好问题。")); }
            sse.push_str(&openai_tail_sse("stop"));
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        Behavior::DegenerateSingleChar => {
            // One single character repeated 80 times: the repeated unit is one
            // char, which the R2/R3/R4 rules must still reject.
            let mut sse = String::new();
            for _ in 0..80 { sse.push_str(&openai_text_chunk("哈")); }
            sse.push_str(&openai_tail_sse("stop"));
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        Behavior::NormalLongText => {
            // ~2000 characters of prose that never repeats back-to-back: a false
            // positive here would be worse than the bug this feature fixes, so
            // this fixture is the control case for the detector.
            let alphabet: Vec<char> = "abcdefghijklmnopqrstuvwxyz".chars().collect();
            let chars: Vec<char> = (0..2000usize).map(|i| alphabet[(i * 7 + i / 26) % 26]).collect();
            let mut sse = String::new();
            for chunk in chars.chunks(100) {
                let frame: String = chunk.iter().collect();
                sse.push_str(&openai_text_chunk(&frame));
            }
            sse.push_str(&openai_tail_sse("stop"));
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        Behavior::NormalThenDegenerate => {
            // 120 characters of normal text (past the 64-char holdback, so the
            // answer COMMITS) followed by a degenerate repetition. Detection is
            // pre-commit only: this repetition must reach the client unchanged.
            let mut sse = String::new();
            for i in 0..10usize {
                let frame: String = (0..12).map(|k| (b'a' + ((i * 12 + k) % 26) as u8) as char).collect();
                sse.push_str(&openai_text_chunk(&frame));
            }
            for _ in 0..40 { sse.push_str(&openai_text_chunk("好问题。")); }
            sse.push_str(&openai_tail_sse("stop"));
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        Behavior::ToolAfterShortText => {
            // B1 regression fixture: a text prefix SHORTER than the holdback,
            // followed by a tool call. The buffered text is real content and must
            // be committed together with the tool call, never dropped.
            let mut sse = openai_text_chunk("thinking");
            sse.push_str(&sse_frame(&serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"get_weather","arguments":""}}]}}]}).to_string()));
            sse.push_str(&sse_frame(&serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments": serde_json::json!({"city":"Paris"}).to_string()}}]}}]}).to_string()));
            sse.push_str(&openai_tail_sse("tool_calls"));
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
    }
}

/// `data: <payload>\n\n` — the SSE framing every loop-guard fixture is built from.
fn sse_frame(payload: &str) -> String {
    let mut s = String::with_capacity(payload.len() + 8);
    s.push_str("data: ");
    s.push_str(payload);
    s.push('\n');
    s.push('\n');
    s
}

/// One OpenAI-compatible SSE chunk carrying a text delta.
fn openai_text_chunk(text: &str) -> String {
    sse_frame(&serde_json::json!({"choices":[{"index":0,"delta":{"content":text}}]}).to_string())
}

/// Terminal OpenAI-compatible frames: finish_reason + usage, then [DONE].
fn openai_tail_sse(reason: &str) -> String {
    let mut s = sse_frame(&serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}],"usage":{"prompt_tokens":9,"completion_tokens":4}}).to_string());
    s.push_str(&sse_frame("[DONE]"));
    s
}

/// Mount several behaviors on ONE listener under distinct path prefixes
/// /u{i}/v1/chat/completions — channel i's base_url points at /u{i}.
/// Returns (base, Vec<prefix>).
pub async fn spawn_fake_upstreams(behs: &[Behavior]) -> (String, Vec<String>) {
    use axum::extract::{Path, State};
    use axum::routing::post;

    let behs: Vec<Behavior> = behs.to_vec();
    let idx: std::collections::HashMap<String, usize> = behs
        .iter()
        .enumerate()
        .map(|(i, _)| (format!("u{i}"), i))
        .collect();

    async fn dispatch(
        State((behs, idx)): State<(Vec<Behavior>, std::collections::HashMap<String, usize>)>,
        Path((prefix, _rest)): Path<(String, String)>,
        req: String,
    ) -> axum::response::Response {
        let beh = idx.get(&prefix).copied().and_then(|i| behs.get(i).copied()).unwrap_or(Behavior::Fail500);
        handle_beh(beh, req).await
    }

    let state = (behs, idx);
    let app: axum::Router = axum::Router::new()
        .route("/{prefix}/{*rest}", post(dispatch))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), (0..state.0.len()).map(|i| format!("/u{i}")).collect())
}

/// Fast retry config for tests: short deadlines so failure paths finish quickly.
pub fn test_retry() -> RetryConfig {
    RetryConfig {
        max_attempts: 6,
        max_cycles: 2,
        total_deadline_ms: 20_000,
        attempt_deadline_ms: 3_000,
        connect_timeout_ms: 1_000,
        ttfb_timeout_ms: 2_000,
        first_event_timeout_ms: 2_000,
        chunk_idle_timeout_ms: 2_000,
        reserve_ms: 500,
        cooldown_wait: true,
        ..Default::default()
    }
}

/// Build a gateway Config: one group ("g1"), key "gw-test-key", channels
/// pointing at fake prefixes; tier1 = channels[..tier1], rest into tier 2.
pub const EXPOSED: &str = "my-model";

pub fn test_config(
    base: &str,
    prefixes: &[String],
    protocols: &[Protocol],
    tier1: usize,
    exposed: &str,
    retry: RetryConfig,
) -> Config {
    assert_eq!(prefixes.len(), protocols.len());
    let mut channels = Vec::new();
    let mut models = Vec::new();
    for (i, p) in prefixes.iter().enumerate() {
        let cid = format!("ch{i}");
        channels.push(Channel {
            id: cid.clone(),
            name: format!("channel-{i}"),
            base_url: format!("{base}{}", p),
            protocol: protocols[i],
            api_key: format!("sk-fake-{i}"),
            enabled: true,
            weight: 1,
            headers: Default::default(),
        });
        models.push(ModelBinding {
            id: format!("m{i}"),
            channel_id: cid,
            exposed: exposed.to_string(),
            upstream: format!("up-model-{i}"),
            price_per_m: [1.0, 2.0, 0.0, 0.0],
            enabled: true,
        });
    }
    let mut tiers = vec![Tier {
        level: 1,
        members: (0..tier1).map(|i| TierMember { model_id: format!("m{i}"), weight: 1 }).collect(),
    }];
    if tier1 < prefixes.len() {
        tiers.push(Tier {
            level: 2,
            members: (tier1..prefixes.len())
                .map(|i| TierMember { model_id: format!("m{i}"), weight: 1 })
                .collect(),
        });
    }
    Config {
        retry,
        channels,
        models,
        groups: vec![Group {
            id: "g1".into(),
            name: "test-group".into(),
            exposed_models: vec![exposed.to_string()],
            tiers,
            // Task-28: no overrides here — this group follows the global retry.
            overrides: GroupOverrides::default(),
        }],
        api_keys: vec![ApiKey { key: "gw-test-key".into(), group_id: "g1".into(), name: "tester".into(), enabled: true }],
        ..Default::default()
    }
}

/// Full gateway harness: AppState + router with published config.
pub async fn gateway(cfg: Config) -> (AppState, axum::Router) {
    let store = Arc::new(Store::open_memory().unwrap());
    let holder = gw_server::config::ConfigHolder::default();
    holder.publish(cfg);
    let health = Arc::new(HealthRegistry::new(Some(store.clone())));
    let state = AppState { http: Default::default(), config: holder, health, store };
    let router = gw_server::routes::data_plane_router(state.clone());
    (state, router)
}

fn chat_body(exposed: &str, stream: bool) -> serde_json::Value {
    serde_json::json!({
        "model": exposed,
        "stream": stream,
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 50
    })
}

/// Drive the router: chat request. Returns (status, body bytes).
pub async fn post_chat(router: &axum::Router, exposed: &str, stream: bool) -> (u16, serde_json::Value) {
    use tower::util::ServiceExt;
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer gw-test-key")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(serde_json::to_vec(&chat_body(exposed, stream)).unwrap()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 10 * 1024 * 1024).await.unwrap().to_vec();
    let v = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, v)
}

/// Drive the router: streaming chat. Returns (status, raw SSE text).
pub async fn post_chat_stream(router: &axum::Router, exposed: &str) -> (u16, String) {
    use tower::util::ServiceExt;
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer gw-test-key")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(serde_json::to_vec(&chat_body(exposed, true)).unwrap()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 10 * 1024 * 1024).await.unwrap().to_vec();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// POST /v1/messages (Anthropic entry). stream=true appends \"stream\":true.
pub async fn post_messages(router: &axum::Router, exposed: &str, stream: bool) -> (u16, axum::response::Response) {
    use tower::util::ServiceExt;
    let mut v = serde_json::json!({
        "model": exposed, "max_tokens": 100,
        "messages": [{"role": "user", "content": "hi"}]
    });
    if stream { v["stream"] = serde_json::json!(true); }
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("x-api-key", "gw-test-key")
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(serde_json::to_vec(&v).unwrap()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    (status, resp)
}

/// POST /v1beta/models/{m}:generateContent or :streamGenerateContent (Gemini entry).
pub async fn post_gemini(router: &axum::Router, exposed: &str, stream: bool) -> (u16, axum::response::Response) {
    use tower::util::ServiceExt;
    let suffix = if stream { ":streamGenerateContent" } else { ":generateContent" };
    let uri = format!("/v1beta/models/{exposed}{suffix}");
    let v = serde_json::json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}]
    });
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(uri)
        .header("x-goog-api-key", "gw-test-key")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(serde_json::to_vec(&v).unwrap()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    (status, resp)
}

/// Drain a response body to a string (for the (u16, Response) helpers above).
pub async fn body_string(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 10 * 1024 * 1024).await.unwrap().to_vec();
    String::from_utf8_lossy(&bytes).to_string()
}

/// Parse a buffered JSON response body.
pub async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let s = body_string(resp).await;
    serde_json::from_str(&s).unwrap_or(serde_json::Value::Null)
}

/// Leak assertions for a SUCCESSFUL stream: has terminator + finish, no repair frame.
pub fn assert_stream_clean(sse: &str) {
    assert!(!sse.contains("gateway_upstream_interrupted"), "unexpected repair frame: {sse}");
    assert!(sse.contains("data: [DONE]"), "missing [DONE] terminator: {sse}");
    assert!(sse.contains("finish_reason"), "missing finish_reason: {sse}");
}

/// Leak assertions for a non-OpenAI stream: has a protocol terminator
/// (anthropic message_stop / gemini finishReason frame), no repair frame.
pub fn assert_stream_clean_generic(sse: &str) {
    assert!(!sse.contains("gateway_upstream_interrupted"), "unexpected repair frame: {sse}");
    assert!(
        sse.contains("message_stop") || sse.contains("finishReason") || sse.contains("finish_reason"),
        "missing terminator: {sse}"
    );
}

/// Leak assertions for a REPAIRED stream: explicit error frame + [DONE] —
/// a client SDK must never mistake this for a clean success.
pub fn assert_stream_repaired(sse: &str) {
    assert!(sse.contains("gateway_upstream_interrupted"), "expected explicit repair frame: {sse}");
    assert!(sse.contains("data: [DONE]"), "missing [DONE] after repair: {sse}");
}
