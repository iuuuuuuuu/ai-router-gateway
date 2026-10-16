//! Shared test scaffolding: fake upstreams + gateway harness.
use gw_server::config::{ApiKey, Channel, Config, Group, GroupOverrides, ModelBinding, Protocol, RetryConfig, Tier, TierMember};
use gw_server::health::HealthRegistry;
use gw_server::inflight::Inflight;
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
    NormalThenDegenerate,    /// A model stuck IN ITS THINKING: the same multi-line unit repeated across
    /// reasoning frames, then an ordinary answer. The answer is fine — it is the
    /// deliberation that never ends, so the gateway must cut the thoughts
    /// instead of relaying an unbounded stream of them.
    ReasoningLoop,
    /// A thinking loop on the ONLY candidate: the request must end honestly
    /// rather than hang or loop forever.
    ReasoningLoopOnly,
    /// Control for the reasoning guard: thinking that DELIBERATES without
    /// repeating — a false positive here would discard a good candidate.
    NormalReasoning,    /// Control for the reasoning guard AT THE THRESHOLD: four copies of a
    /// 16-char unit (64 chars) sit exactly inside `max_repeats = 4` — the rule
    /// counts N-1 repeats — so they must reach the client untouched. This is
    /// the fixture a guard tuned one notch too eager fails, which the
    /// never-repeating control above cannot detect (measured).
    ReasoningAtThreshold,
    /// Same, but the loop runs for 400 frames so a truncation that failed to
    /// actually stop the stream is unmistakable.
    NormalThenDegenerateLong,
    /// Control: a committed answer that continues normally — must never be cut.
    NormalOnlyStream,
    /// NORMAL text dripped one 12-char frame at a time with a deliberate gap, so
    /// the wall-clock holdback cap fires long before `min_chars` characters have
    /// arrived. This is the fixture that makes the cap TESTABLE: every other
    /// stream fixture emits its whole body at once, so a cap measured in
    /// milliseconds would never trigger and the test would be green for the
    /// wrong reason.
    SlowDripNormalText,
    /// B1 regression: text shorter than the holdback, then a tool call. The
    /// buffered text must be committed WITH the tool call, never dropped.
    ToolAfterShortText,
    /// The end-to-end proof for loop CORRECTION: a normal committed prefix, then
    /// a repetition — but ONLY when the request does not carry the correction
    /// prompt. When the prompt is present this answers with a clean continuation,
    /// so a request that recovers proves the prompt really reached the upstream
    /// AND that the continuation was spliced onto the same client stream.
    /// The decision is read from the REQUEST BODY, so the fixture is stateless
    /// and cannot pass by accident of ordering.
    DegenerateThenContinueOnPrompt,
    /// 200 + OpenAI-shaped model list whose ids ARE the request headers this
    /// GET really arrived with (`hdr <name>=<value>`), plus the serving path
    /// as `hdr x-upstream-prefix=<prefix>`. Lets a test assert on the wire
    /// request end-to-end instead of trusting the code under test.
    RecordHeaders200Json,
    /// 404 + a JSON error body (not-open-found, and a plain-text variant).
    NotFoundJson,
    /// 200 + a model list that repeats one id, in a deliberate order.
    Duplicates200Json,
    /// 200 + an EMPTY OpenAI-shaped model list: the provider answered
    /// "no models", which is a legitimate answer and not a parse error.
    EmptyDataJson,
    /// 200 + a JSON object with no model list in it at all ({}).
    NoListJson,
    /// 200 + full OpenAI JSON, but only after a deliberate delay, so a test can
    /// observe the in-flight counter while the upstream is still thinking.
    SlowJson,
    /// 200 + SSE that COMMITS immediately (a first frame longer than the loop
    /// guard's holdback) and then keeps the connection open for a while. Lets a
    /// test prove occupancy survives the commit, not just the header arrival.
    StreamSlow,
    /// 200 + SSE whose single text delta is the REQUEST BODY this upstream
    /// received. The only fixture that observes the forwarded wire form, so a
    /// passthrough claim can be checked against real bytes.
    EchoBody,
    /// The real-world shape: a repeated run, a blank line, the run again, then
    /// a concluding remark. Emitted in 12-char frames like the live upstream.
    BlankLineRepeatShape,
    /// One OpenAI delta carrying BOTH content and reasoning_content.
    ReasoningAndTextSameFrame,
    /// OpenAI SSE: reasoning_content deltas, then answer text.
    ReasoningStream,
    /// OpenAI SSE: a frame carrying ONLY reasoning_content, then the answer.
    ReasoningOnly,
    /// Anthropic SSE: a thinking_delta content block, then a text block.
    GoodAnthropicReasoningStream,
    /// Gemini SSE: a part flagged "thought": true, then the answer part.
    GoodGeminiReasoningStream,
    /// Buffered OpenAI JSON whose message carries reasoning_content.
    GoodOpenAiReasoningJson,
    /// Buffered Anthropic JSON whose content has a thinking block then text.
    GoodAnthropicReasoningJson,
}

async fn handle_beh(beh: Behavior, body: String, hdrs: axum::http::HeaderMap, prefix: &str) -> axum::response::Response {
    use axum::response::IntoResponse;
    let _ = body;
    let _ = prefix;
    match beh {
        Behavior::SlowJson => {
            // Long enough to observe, short enough to keep tests quick.
            tokio::time::sleep(Duration::from_millis(400)).await;
            (
                axum::http::StatusCode::OK,
                axum::Json(serde_json::json!({
                    "id": "chatcmpl-x", "object": "chat.completion",
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": "hello from json"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 11, "completion_tokens": 7}
                })),
            ).into_response()
        }
        Behavior::StreamSlow => {
            // Frame 1 is longer than the loop guard's holdback (min_chars = 64
            // by default), so the gateway COMMITS on it. The sleep after it is
            // the window where a naive in-flight implementation would already
            // have released the account even though the client is still being
            // served.
            let head = "The quick brown fox jumps over the lazy dog while a tired postman waves at a passing tram.";
            let first = format!(
                "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"{head}\"}}}}]}}\n\n"
            );
            let s = async_stream::stream! {
                yield Ok::<_, std::convert::Infallible>(bytes::Bytes::from(first));
                tokio::time::sleep(Duration::from_millis(400)).await;
                yield Ok(bytes::Bytes::from_static(
                    b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\" end\"}}]}\n\n"));
                yield Ok(bytes::Bytes::from_static(
                    b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n"));
                yield Ok(bytes::Bytes::from_static(b"data: [DONE]\n\n"));
            };
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], axum::body::Body::from_stream(s)).into_response()
        }
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
        Behavior::NotFoundJson => (axum::http::StatusCode::NOT_FOUND, "{\"error\":{\"message\":\"no such route\"}}").into_response(),
        Behavior::RecordHeaders200Json => {
            // Every header this GET really arrived with becomes one model id,
            // so a test can observe the request end-to-end instead of
            // trusting the code under test. The synthetic x-upstream-prefix
            // row names the path that served it (u0, u1, ...).
            let mut rows: Vec<(String, String)> = hdrs
                .iter()
                .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("<binary>").to_string()))
                .collect();
            rows.push(("x-upstream-prefix".to_string(), prefix.to_string()));
            rows.sort();
            let data: Vec<serde_json::Value> = rows
                .iter()
                .map(|(k, v)| serde_json::json!({ "id": format!("hdr {k}={v}") }))
                .collect();
            axum::Json(serde_json::json!({ "object": "list", "data": data })).into_response()
        }
        Behavior::Duplicates200Json => axum::Json(serde_json::json!({
            "object": "list",
            "data": [{ "id": "a" }, { "id": "b" }, { "id": "a" }, { "id": "b" }],
        })).into_response(),
        Behavior::Empty => (axum::http::StatusCode::OK, "").into_response(),
        Behavior::EmptyDataJson => axum::Json(serde_json::json!({ "object": "list", "data": [] })).into_response(),
        Behavior::NoListJson => axum::Json(serde_json::json!({})).into_response(),
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
            // answer COMMITS) followed by a degenerate repetition. The
            // repetition must be CUT OFF by the post-commit guard: the client
            // keeps the normal prefix and a clean finish, and only a bounded
            // number of the repeated frames may escape.
            let mut sse = String::new();
            for i in 0..10usize {
                let frame: String = (0..12).map(|k| (b'a' + ((i * 12 + k) % 26) as u8) as char).collect();
                sse.push_str(&openai_text_chunk(&frame));
            }
            for _ in 0..40 { sse.push_str(&openai_text_chunk("好问题。")); }
            sse.push_str(&openai_tail_sse("stop"));
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        Behavior::ReasoningLoop => {
            // A realistic shape: the model re-states the same decision over
            // and over. The unit is deliberately LONG (46 chars, measured from
            // the real capture) because that is what a circling model looks
            // like — short units are caught by tighter means.
            let unit = "我写。\n（输出）\nOK。\n现在写。\n我写。\n";
            let mut sse = String::new();
            let frame = serde_json::json!({
                "choices": [{"index": 0, "delta": {"reasoning_content": unit}}]
            });
            for _ in 0..12 { sse.push_str(&sse_frame(&frame.to_string())); }
            sse.push_str(&openai_text_chunk("ANSWER-OK"));
            sse.push_str(&openai_tail_sse("stop"));
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        Behavior::ReasoningLoopOnly => {
            let unit = "我写。\n（输出）\nOK。\n现在写。\n我写。\n";
            let mut sse = String::new();
            for _ in 0..12 { sse.push_str(&sse_frame(&serde_json::json!({"choices":[{"index":0,"delta":{"reasoning_content":unit}}]}).to_string())); }
            sse.push_str(&openai_tail_sse("stop"));
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        Behavior::ReasoningAtThreshold => {
            // L=16 distinct chars x4 copies = 64 chars. `match_span` compares
            // at shift unit_len, so four copies yield reps = 3, which is below
            // `max_repeats = 4`: the correct guard tolerates it. Five copies
            // yield reps = 4 and must fire (pinned by the unit test).
            let unit = "第一步要先确认接口返回的字段结构";
            let mut sse = String::new();
            for _ in 0..4 { sse.push_str(&sse_frame(&serde_json::json!({"choices":[{"index":0,"delta":{"reasoning_content":unit}}]}).to_string())); }
            sse.push_str(&openai_text_chunk("ANSWER-OK"));
            sse.push_str(&openai_tail_sse("stop"));
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        Behavior::NormalReasoning => {
            // Deliberation that repeats PHRASES without ever looping.
            //
            // MEASURED, not assumed: with the real rules (min_chars 64,
            // repeat_min_chars 24, ratio 0.75) this text produces NO candidate
            // at any unit length — and it still produces none when min_chars is
            // dropped to 8. R1 needs a CONTIGUOUS shift-match, and prose repeats
            // its phrases across different sentences, never back to back.
            //
            // ★ This fixture ALONE has no teeth against an over-eager guard:
            // lowering max_repeats/min_chars here left every test green. Nothing
            // repeated is present, so no threshold can fire on it. That gap is
            // what ReasoningAtThreshold closes, at the measured R2 boundary
            // (N copies give repeats = N-1, so max_repeats=4 tolerates 5 copies).
            // What this one genuinely pins is that ordinary deliberation is
            // relayed whole and never counted as a loop.
            let thoughts = [
                "先看入口：用户点了网站解析，走的是 parseWebsite。",
                "再看表单：title 是必填的，这个接口其实不吃 title，只能本地用。",
                "上传字段用原生 input，因为接口要的是原始二进制而不是 URL。",
                "导入这一步要 taskId，解析响应里没有，先如实提示。",
                "分页没有接口，用 NDataTable 的内置分页即可。",
                "让我再想想：接口的契约以后端为准，前端只做展示。",
                "好，就这样，接口不动，字段原样透出。",
            ];
            let mut sse = String::new();
            for t in thoughts { sse.push_str(&sse_frame(&serde_json::json!({"choices":[{"index":0,"delta":{"reasoning_content":t}}]}).to_string())); }
            sse.push_str(&openai_text_chunk("ANSWER-OK"));
            sse.push_str(&openai_tail_sse("stop"));
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        Behavior::NormalThenDegenerateLong => {
            // The same shape, but the upstream keeps emitting the loop for FAR
            // more frames than any tail can justify. Used to prove the cut
            // really stops the stream instead of merely bounding one frame: a
            // truncation that still relayed all 400 repetitions would leave the
            // client with the whole loop.
            let mut sse = String::new();
            for i in 0..10usize {
                let frame: String = (0..12).map(|k| (b'a' + ((i * 12 + k) % 26) as u8) as char).collect();
                sse.push_str(&openai_text_chunk(&frame));
            }
            for _ in 0..400 { sse.push_str(&openai_text_chunk("好问题。")); }
            sse.push_str(&openai_tail_sse("stop"));
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        Behavior::NormalOnlyStream => {
            // Control: normal text long enough to commit, then a NORMAL
            // continuation that must never be truncated. A false positive here
            // would cut a healthy answer short, which is worse than the loop.
            //
            // The text must be genuinely non-repeating: a repeating alphabet
            // ("abc...z" over and over) is a REAL period-26 loop and the guard
            // would rightly cut it. The generated prose below uses a per-index
            // stride so no short period can appear.
            let mut sse = String::new();
            for i in 0..40usize {
                let frame: String = (0..12)
                    .map(|k| {
                        let n = i * 12 + k;
                        // Distinct CJK block, strided so consecutive frames never
                        // line up into a period.
                        char::from_u32(0x4E00 + ((n * 37) % 2000) as u32).unwrap()
                    })
                    .collect();
                sse.push_str(&openai_text_chunk(&frame));
            }
            sse.push_str(&openai_tail_sse("stop"));
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        Behavior::SlowDripNormalText => {
            // Normal, non-repeating prose (same generator as NormalOnlyStream, so
            // the guard has no repetition to find) delivered SLOWLY: one 12-char
            // frame every `gap_ms`. The gap comes from the REQUEST BODY, not from
            // an env var: tests run concurrently in one process, and a global
            // variable would let one test's drip timing leak into another's.
            //
            // The default cap is 250 ms, so a 60 ms gap lets ~4 frames through
            // before the ceiling forces a commit — which is exactly the
            // slow-model case the cap exists for.
            // The gap is a CONSTANT of this behaviour rather than a parameter
            // read from the request or an env var. Both of those leak: the
            // gateway rebuilds the upstream request and drops unknown fields,
            // and tests share one process, so a global variable would let one
            // test's timing contaminate another's. A per-behaviour constant is
            // isolated by construction.
            //
            // 1200 ms per 12-char frame puts the two outcomes far apart: with a
            // 250 ms cap the commit lands on frame 2 (~1.2 s), while waiting for
            // the 64-character target takes frame 6 (~6 s). The gap has to exceed
            // any plausible coalescing window: when frames are batched into one
            // read the proxy sees them at the same instant, the elapsed time never
            // crosses the ceiling, and a smaller gap lets that flip the result.
            const GAP_MS: u64 = 1200;
            let s = async_stream::stream! {
                for i in 0..6usize {
                    let frame: String = (0..12)
                        .map(|k| {
                            let n = i * 12 + k;
                            char::from_u32(0x4E00 + ((n * 37) % 2000) as u32).unwrap()
                        })
                        .collect();
                    yield Ok::<_, std::convert::Infallible>(bytes::Bytes::from(openai_text_chunk(&frame)));
                    tokio::time::sleep(Duration::from_millis(GAP_MS)).await;
                }
                yield Ok(bytes::Bytes::from(openai_tail_sse("stop")));
            };
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], axum::body::Body::from_stream(s)).into_response()
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
        Behavior::DegenerateThenContinueOnPrompt => {
            // Two behaviours in one fixture, chosen by the REQUEST BODY:
            //   - no correction prompt  -> normal prefix, then a real repetition
            //   - correction prompt present -> a clean continuation
            // The gateway only adds the prompt when it decides to correct, so a
            // recovered request is proof the whole path ran: detection, cut,
            // prompt, upstream call, and splice.
            // BOTH conditions are required, and that is deliberate: keying on
            // the prompt alone would still pass if the gateway sent the reason
            // but DROPPED the answer it was continuing (the fixture would
            // recover either way). Requiring the kept prefix in the request
            // makes the test fail whenever the pre-loop text is discarded.
            if body.contains(CONTINUE_MARKER) && body.contains("abcdefghijklmnopqrstuvwxyz") {
                let mut sse = openai_text_chunk(CONTINUED);
                sse.push_str(&openai_tail_sse("stop"));
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
            } else {
                let mut sse = String::new();
                // 120 chars of normal prose: past the 64-char holdback, so the
                // answer COMMITS and the loop below is a post-commit hit.
                for i in 0..10usize {
                    let frame: String = (0..12).map(|k| (b'a' + ((i * 12 + k) % 26) as u8) as char).collect();
                    sse.push_str(&openai_text_chunk(&frame));
                }
                for _ in 0..40 { sse.push_str(&openai_text_chunk("好问题。")); }
                sse.push_str(&openai_tail_sse("stop"));
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
            }
        }
        Behavior::EchoBody => {
            // Reply with the REQUEST BODY this upstream received, as one OpenAI
            // text delta. A test can then assert on the exact bytes the gateway
            // forwarded instead of trusting that a field "should" be present:
            // this is the only fixture that observes the upstream wire form.
            let sse = openai_text_chunk(&serde_json::json!({ "echo": body }).to_string());
            let mut full = sse;
            full.push_str(&openai_tail_sse("stop"));
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], full).into_response()
        }
        Behavior::BlankLineRepeatShape => {
            // The shape from the real-world report: the same run of text, a
            // blank line, the run again, then the model's own concluding remark
            // ("（共 20 遍）"). Fed in 12-char frames because that is the stride
            // the live upstream uses, and the guard only ever sees frame
            // boundaries -- a whole-string feed can answer differently.
            let mut sse = String::new();
            let text = format!("{0}\n\n{0}\n\n（共 20 遍）", "hello ".repeat(11));
            let chars: Vec<char> = text.chars().collect();
            for chunk in chars.chunks(12) {
                sse.push_str(&openai_text_chunk(&chunk.iter().collect::<String>()));
            }
            sse.push_str(&openai_tail_sse("stop"));
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        Behavior::ReasoningAndTextSameFrame => {
            // Some providers put reasoning AND content in ONE delta. Whichever
            // the classifier reports first, the other must not be lost.
            let mut sse = String::new();
            sse.push_str(&sse_frame(&serde_json::json!({"choices":[{"index":0,"delta":{"content":"BOTH-TEXT","reasoning_content":"BOTH-THOUGHT"}}]}).to_string()));
            sse.push_str(&openai_text_chunk(" tail"));
            sse.push_str(&openai_tail_sse("stop"));
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        Behavior::ReasoningStream => {
            // thinking deltas interleaved with answer text -- reasoning arrives
            // BEFORE the prose in the real providers' shape.
            let mut sse = String::new();
            sse.push_str(&sse_frame(&serde_json::json!({"choices":[{"index":0,"delta":{"reasoning_content":"let me think"}}]}).to_string()));
            sse.push_str(&sse_frame(&serde_json::json!({"choices":[{"index":0,"delta":{"reasoning_content":" about this"}}]}).to_string()));
            sse.push_str(&openai_text_chunk("the answer"));
            sse.push_str(&openai_tail_sse("stop"));
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        Behavior::ReasoningOnly => {
            // A frame carrying ONLY reasoning must not be mistaken for the
            // answer: before the fix it was classified as ignorable.
            let mut sse = String::new();
            sse.push_str(&sse_frame(&serde_json::json!({"choices":[{"index":0,"delta":{"reasoning_content":"THOUGHT-MARKER"}}]}).to_string()));
            sse.push_str(&openai_text_chunk("final"));
            sse.push_str(&openai_tail_sse("stop"));
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        Behavior::GoodAnthropicReasoningStream => {
            // Anthropic spells streamed thinking "thinking_delta"; the
            // event framing is built with an explicit \n so the two-line
            // SSE shape is exact.
            let mut sse = String::new();
            let ev = |name: &str, payload: serde_json::Value| format!("event: {name}\ndata: {payload}\n\n");
            sse.push_str(&ev("message_start", serde_json::json!({"type":"message_start","message":{"usage":{"input_tokens":3}}})));
            sse.push_str(&ev("content_block_delta", serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"ANTHRO-THOUGHT"}})));
            sse.push_str(&ev("content_block_delta", serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"answer"}})));
            sse.push_str(&ev("message_delta", serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}})));
            sse.push_str(&ev("message_stop", serde_json::json!({"type":"message_stop"})));
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        Behavior::GoodGeminiReasoningStream => {
            let mut sse = String::new();
            sse.push_str(&sse_frame(&serde_json::json!({"candidates":[{"content":{"parts":[{"text":"GEMINI-THOUGHT","thought":true}],"role":"model"}}]}).to_string()));
            sse.push_str(&sse_frame(&serde_json::json!({"candidates":[{"content":{"parts":[{"text":"answer"}],"role":"model"}}]}).to_string()));
            sse.push_str(&sse_frame(&serde_json::json!({"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":5}}).to_string()));
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        Behavior::GoodOpenAiReasoningJson => (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({
                "id": "chatcmpl-x", "object": "chat.completion", "model": "m",
                "choices": [{"index": 0, "message": {
                    "role": "assistant",
                    "reasoning_content": "JSON-THOUGHT",
                    "content": "json answer"
                }, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 2}
            })),
        ).into_response(),
        Behavior::GoodAnthropicReasoningJson => (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({
                "id": "msg_x", "type": "message", "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "ANTHRO-JSON-THOUGHT", "signature": "sig"},
                    {"type": "text", "text": "anthropic answer"}
                ],
                "model": "claude-fake", "stop_reason": "end_turn", "stop_sequence": null,
                "usage": {"input_tokens": 12, "output_tokens": 6}
            })),
        ).into_response(),
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

/// Which behaviors answer the model-list probe with their OWN response
/// instead of the default canned list. Listed explicitly so a normal chat
/// behavior (GoodJson and friends) keeps serving the static model list the
/// model-list tests rely on.
fn models_override(b: Behavior) -> bool {
    matches!(
        b,
        Behavior::Fail401
            | Behavior::Fail404
            | Behavior::Fail500
            | Behavior::Fail429
            | Behavior::Garbage
            | Behavior::Empty
            | Behavior::NotFoundJson
            | Behavior::Duplicates200Json
            | Behavior::RecordHeaders200Json
            | Behavior::EmptyDataJson
            | Behavior::NoListJson
    )
}

/// Mount several behaviors on ONE listener under distinct path prefixes
/// /u{i}/v1/chat/completions — channel i's base_url points at /u{i}.
///
/// The same prefix also answers the console model-list probe in BOTH provider
/// shapes: GET /u{i}/v1/models (OpenAI) and GET /u{i}/v1beta/models (Gemini).
/// Every id carries the prefix, so a handler that resolved the wrong base_url
/// gets visibly wrong ids; an unknown prefix is 404, never somebody else's list.
/// The POST behaviour above is untouched — every other test depends on it.
/// Returns (base, Vec<prefix>).
pub async fn spawn_fake_upstreams(behs: &[Behavior]) -> (String, Vec<String>) {
    use axum::extract::{Path, State};
    use axum::routing::{get, post};

    let behs: Vec<Behavior> = behs.to_vec();
    let idx: std::collections::HashMap<String, usize> = behs
        .iter()
        .enumerate()
        .map(|(i, _)| (format!("u{i}"), i))
        .collect();

    async fn dispatch(
        State((behs, idx)): State<(Vec<Behavior>, std::collections::HashMap<String, usize>)>,
        Path((prefix, _rest)): Path<(String, String)>,
        hdrs: axum::http::HeaderMap,
        req: String,
    ) -> axum::response::Response {
        let beh = idx.get(&prefix).copied().and_then(|i| behs.get(i).copied()).unwrap_or(Behavior::Fail500);
        handle_beh(beh, req, hdrs, &prefix).await
    }

    /// GET /{prefix}/v1/models — the OpenAI shape the real providers send.
    async fn models_openai(
        State((behs, idx)): State<(Vec<Behavior>, std::collections::HashMap<String, usize>)>,
        Path(prefix): Path<String>,
        hdrs: axum::http::HeaderMap,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        let Some(i) = idx.get(&prefix).copied() else {
            return (axum::http::StatusCode::NOT_FOUND, "unknown prefix").into_response();
        };
        let beh = behs.get(i).copied().unwrap_or(Behavior::Fail500);
        if models_override(beh) {
            return handle_beh(beh, String::new(), hdrs, &prefix).await;
        }
        let data: Vec<serde_json::Value> = ["a", "b", "c"]
            .iter()
            .map(|s| serde_json::json!({ "id": format!("{prefix}-model-{s}"), "object": "model" }))
            .collect();
        axum::Json(serde_json::json!({ "object": "list", "data": data })).into_response()
    }

    /// GET /{prefix}/v1beta/models — the Gemini shape: names carry the "models/" prefix.
    async fn models_gemini(
        State((behs, idx)): State<(Vec<Behavior>, std::collections::HashMap<String, usize>)>,
        Path(prefix): Path<String>,
        hdrs: axum::http::HeaderMap,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        let Some(i) = idx.get(&prefix).copied() else {
            return (axum::http::StatusCode::NOT_FOUND, "unknown prefix").into_response();
        };
        let beh = behs.get(i).copied().unwrap_or(Behavior::Fail500);
        if models_override(beh) {
            return handle_beh(beh, String::new(), hdrs, &prefix).await;
        }
        let models: Vec<serde_json::Value> = ["alpha", "beta"]
            .iter()
            .map(|s| serde_json::json!({ "name": format!("models/{prefix}-gemini-{s}") }))
            .collect();
        axum::Json(serde_json::json!({ "models": models })).into_response()
    }

    let state = (behs, idx);
    let app: axum::Router = axum::Router::new()
        .route("/{prefix}/v1/models", get(models_openai))
        .route("/{prefix}/v1beta/models", get(models_gemini))
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

/// A phrase that appears ONLY in the gateway's correction prompt. The fixture
/// above keys off it, so it must be a literal from the prompt text.
pub const CONTINUE_MARKER: &str = "接着上面被打断的地方继续";
/// The text the upstream returns once it receives the correction prompt.
pub const CONTINUED: &str = "CONTINUED-OK";

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
    let state = AppState { http: Default::default(), config: holder, health, store, inflight: Arc::new(Inflight::new()) };
    // The data plane lives at the root; the admin endpoints the in-flight tests
    // need are MERGED in with the real admin router (same paths, same handlers,
    // same auth), so a test cannot pass against a reimplementation.
    let router = gw_server::routes::data_plane_router(state.clone())
        .merge(gw_server::admin::admin_router(state.clone()));
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
