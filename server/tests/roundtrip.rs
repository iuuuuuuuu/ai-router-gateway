// Protocol round-trip tests: entry protocol -> IR -> upstream protocol -> IR -> entry.
// Covers all 3 entry protocols in both buffered and streaming modes against a
// fake upstream that speaks the SAME protocol (format preservation end-to-end).

mod support;

use support::*;

const EXPOSED: &str = support::EXPOSED;

fn leak_scan_json(v: &serde_json::Value) {
    let s = v.to_string();
    assert!(!s.contains("<html"), "leaked upstream HTML: {s}");
    assert!(!s.contains("gateway_upstream_interrupted"), "leaked repair frame in buffered body: {s}");
}

#[tokio::test]
async fn p01_openai_buffered_roundtrip() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    let (status, body) = post_chat(&router, EXPOSED, false).await;
    assert_eq!(status, 200, "body={body}");
    leak_scan_json(&body);
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["model"], EXPOSED, "model echo must be the EXPOSED name");
    assert_eq!(body["choices"][0]["message"]["content"], "hello from json");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(body["usage"]["prompt_tokens"], 11);
    assert_eq!(body["usage"]["completion_tokens"], 7);
}

#[tokio::test]
async fn p02_openai_stream_roundtrip() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodStream]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    let (status, sse) = post_chat_stream(&router, EXPOSED).await;
    assert_eq!(status, 200);
    support::assert_stream_clean_generic(&sse);
    assert!(sse.contains("\"object\":\"chat.completion.chunk\""), "chunk framing: {sse}");
    assert!(sse.contains("\"model\":\"my-model\""), "model echo in chunks: {sse}");
    assert!(sse.contains("he") && sse.contains("llo"));
    assert!(sse.contains("\"prompt_tokens\":10"), "usage frame forwarded: {sse}");
}

#[tokio::test]
async fn p03_anthropic_buffered_roundtrip() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodAnthropicJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Anthropic], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    let (status, resp) = post_messages(&router, EXPOSED, false).await;
    let body = body_json(resp).await;
    assert_eq!(status, 200, "body={body}");
    leak_scan_json(&body);
    assert_eq!(body["type"], "message");
    assert_eq!(body["model"], EXPOSED);
    assert_eq!(body["content"][0]["type"], "text");
    assert_eq!(body["content"][0]["text"], "bonjour from anthropic");
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(body["usage"]["input_tokens"], 12);
    assert_eq!(body["usage"]["output_tokens"], 6);
}

#[tokio::test]
async fn p04_anthropic_stream_roundtrip() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodAnthropicStream]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Anthropic], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    let (status, resp) = post_messages(&router, EXPOSED, true).await;
    let sse = body_string(resp).await;
    assert_eq!(status, 200, "sse={sse}");
    support::assert_stream_clean_generic(&sse);
    assert!(sse.contains("event: message_start"), "start frame: {sse}");
    assert!(sse.contains("content_block_delta"), "delta frame: {sse}");
    assert!(sse.contains("bonjour") && sse.contains("stream"), "content: {sse}");
    assert!(sse.contains("event: message_delta"), "usage frame: {sse}");
    assert!(sse.contains("\"output_tokens\":6"), "usage values: {sse}");
}

#[tokio::test]
async fn p05_gemini_buffered_roundtrip() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodGeminiJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Gemini], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    let (status, resp) = post_gemini(&router, EXPOSED, false).await;
    let body = body_json(resp).await;
    assert_eq!(status, 200, "body={body}");
    leak_scan_json(&body);
    assert_eq!(body["candidates"][0]["content"]["parts"][0]["text"], "hallo from gemini");
    assert_eq!(body["candidates"][0]["finishReason"], "STOP");
    assert_eq!(body["usageMetadata"]["promptTokenCount"], 9);
    assert_eq!(body["usageMetadata"]["candidatesTokenCount"], 4);
}

#[tokio::test]
async fn p06_gemini_stream_roundtrip() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodGeminiStream]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Gemini], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    let (status, resp) = post_gemini(&router, EXPOSED, true).await;
    let sse = body_string(resp).await;
    assert_eq!(status, 200, "sse={sse}");
    support::assert_stream_clean_generic(&sse);
    assert!(sse.contains("guten") && sse.contains(" tag"), "content: {sse}");
    assert!(sse.contains("\"finishReason\":\"STOP\""), "finish frame: {sse}");
    assert!(sse.contains("\"promptTokenCount\":9"), "usage frame: {sse}");
}

// Cross-protocol: OpenAI-format client talking to an Anthropic upstream via
// the IR translation layer (the compatibility-layer requirement).
#[tokio::test]
async fn p07_openai_entry_anthropic_upstream() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodAnthropicJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Anthropic], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    let (status, body) = post_chat(&router, EXPOSED, false).await;
    assert_eq!(status, 200, "body={body}");
    leak_scan_json(&body);
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["choices"][0]["message"]["content"], "bonjour from anthropic");
    assert_eq!(body["usage"]["prompt_tokens"], 12);
}

#[tokio::test]
async fn p08_anthropic_entry_openai_upstream() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    let (status, resp) = post_messages(&router, EXPOSED, false).await;
    let body = body_json(resp).await;
    assert_eq!(status, 200, "body={body}");
    leak_scan_json(&body);
    assert_eq!(body["type"], "message");
    assert_eq!(body["content"][0]["text"], "hello from json");
    assert_eq!(body["usage"]["output_tokens"], 7);
}

#[tokio::test]
async fn p09_gemini_entry_openai_upstream() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    let (status, resp) = post_gemini(&router, EXPOSED, false).await;
    let body = body_json(resp).await;
    assert_eq!(status, 200, "body={body}");
    leak_scan_json(&body);
    assert_eq!(body["candidates"][0]["content"]["parts"][0]["text"], "hello from json");
}

#[tokio::test]
async fn p10_openai_entry_gemini_upstream() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodGeminiJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Gemini], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    let (status, body) = post_chat(&router, EXPOSED, false).await;
    assert_eq!(status, 200, "body={body}");
    leak_scan_json(&body);
    assert_eq!(body["choices"][0]["message"]["content"], "hallo from gemini");
    assert_eq!(body["usage"]["prompt_tokens"], 9);
}
// --- Tool calls: arguments must survive the IR round trip. ---
// Regression: the Gemini renderer used to hard-code {"args": {}} and drop
// ToolInputDelta, so a streaming tool call reached the client with no
// arguments at all — silent corruption, not an error.
#[tokio::test]
async fn p11_gemini_stream_tool_call_keeps_args() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodGeminiToolStream]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Gemini], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    let (status, resp) = post_gemini(&router, EXPOSED, true).await;
    let sse = body_string(resp).await;
    assert_eq!(status, 200, "sse={sse}");
    support::assert_stream_clean_generic(&sse);
    assert!(sse.contains("\"functionCall\""), "functionCall frame: {sse}");
    assert!(sse.contains("get_weather"), "tool name: {sse}");
    assert!(sse.contains("Tokyo"), "tool args must be the real ones: {sse}");
    assert!(sse.contains("\"unit\":\"c\""), "nested args preserved: {sse}");
    assert!(!sse.contains("\"args\":{}"), "args must never be an empty object: {sse}");
}

#[tokio::test]
async fn p12_gemini_stream_assembles_fragmented_openai_tool_args() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodOpenAiToolStream]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    let (status, resp) = post_gemini(&router, EXPOSED, true).await;
    let sse = body_string(resp).await;
    assert_eq!(status, 200, "sse={sse}");
    support::assert_stream_clean_generic(&sse);
    assert!(sse.contains("get_weather"), "tool name: {sse}");
    assert!(sse.contains("Paris"), "fragmented args must be assembled: {sse}");
    // Gemini's wire form is whole-package: the fragments must collapse into
    // exactly ONE functionCall frame.
    assert_eq!(sse.matches("functionCall").count(), 1, "exactly one call frame: {sse}");
    assert!(!sse.contains("\"args\":{}"), "args must never be an empty object: {sse}");
}

#[tokio::test]
async fn p13_openai_stream_one_frame_tool_call_keeps_arguments() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodOpenAiToolOneFrame]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    let (status, sse) = post_chat_stream(&router, EXPOSED).await;
    assert_eq!(status, 200, "sse={sse}");
    support::assert_stream_clean(&sse);
    assert!(sse.contains("get_weather"), "tool name: {sse}");
    assert!(sse.contains("Rome"), "same-frame arguments must survive: {sse}");
    assert!(sse.contains("\"finish_reason\":\"tool_calls\""), "tool_calls finish: {sse}");
}

#[tokio::test]
async fn p14_anthropic_stream_assembles_fragmented_tool_input() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodAnthropicToolStream]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Anthropic], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    let (status, resp) = post_messages(&router, EXPOSED, true).await;
    let sse = body_string(resp).await;
    assert_eq!(status, 200, "sse={sse}");
    support::assert_stream_clean_generic(&sse);
    assert!(sse.contains("\"type\":\"tool_use\""), "tool_use block: {sse}");
    assert!(sse.contains("get_weather"), "tool name: {sse}");
    assert!(sse.contains("input_json_delta"), "input delta: {sse}");
    assert!(sse.contains("Oslo"), "assembled tool input: {sse}");
    assert!(!sse.contains("input_json_delta\",\"partial_json\":\"{}"), "input must not collapse to {{}}: {sse}");
}

