//! HTTP routes: three protocol entries + /v1/models + healthz.
//!
//! Data-plane auth: Bearer key (or x-api-key) resolves to a group; the group
//! decides exposed models and tier layout. /v1/models lists the group's
//! exposed models (per-plan: group-scoped listing, no aggregation).

use crate::config::Protocol;
use crate::protocol::{anthropic, gemini, openai, ChatRequest};
use crate::proxy::{
    feed_emit, new_client_channel, run as proxy_run, ClientFrame, EntryProtocol, Outcome, ProxyCtx,
};
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::time::Instant;
use tokio_util::sync::CancellationToken;

pub struct AuthedReq {
    pub group_id: String,
    pub entry: EntryProtocol,
    pub ir: ChatRequest,
    pub exposed_model: String,
}

/// Resolve API key -> group. Accepts Authorization: Bearer or x-api-key.
pub fn authenticate(state: &crate::proxy::AppState, headers: &HeaderMap) -> Result<String, Response> {
    let snap = state.config.get().ok_or_else(|| error_response(EntryProtocol::Openai, StatusCode::SERVICE_UNAVAILABLE, "config_not_loaded", "网关尚未加载任何配置"))?;
    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()))
        .or_else(|| headers.get("x-goog-api-key").and_then(|v| v.to_str().ok()))
        .unwrap_or("");
    if presented.is_empty() {
        return Err(error_response(EntryProtocol::Openai, StatusCode::UNAUTHORIZED, "missing_api_key", "缺少 API Key：请提供 Authorization: Bearer <key> 或 x-api-key 请求头"));
    }
    for k in &snap.config.api_keys {
        if k.enabled && k.key == presented {
            return Ok(k.group_id.clone());
        }
    }
    Err(error_response(EntryProtocol::Openai, StatusCode::UNAUTHORIZED, "invalid_api_key", "API Key 无效或已被禁用"))
}

/// Standard error body in the client's protocol.
pub fn error_response(entry: EntryProtocol, status: StatusCode, code: &str, message: &str) -> Response {
    let body = match entry {
        EntryProtocol::Openai => json!({ "error": { "message": message, "type": format!("gateway_{code}"), "code": code } }),
        EntryProtocol::Anthropic => json!({ "type": "error", "error": { "type": "api_error", "message": message } }),
        EntryProtocol::Gemini => json!({ "error": { "code": status.as_u16(), "message": message, "status": "UNAVAILABLE" } }),
    };
    (status, axum::Json(body)).into_response()
}

pub async fn healthz() -> &'static str { "ok" }

/// GET /v1/models — group-scoped exposed models.
pub async fn list_models(State(state): State<crate::proxy::AppState>, headers: HeaderMap) -> Response {
    let group_id = match authenticate(&state, &headers) {
        Ok(g) => g,
        Err(r) => return r,
    };
    let Some(snap) = state.config.get() else {
        return error_response(EntryProtocol::Openai, StatusCode::SERVICE_UNAVAILABLE, "config_not_loaded", "网关尚未加载任何配置");
    };
    let Some(group) = snap.config.groups.iter().find(|g| g.id == group_id) else {
        return error_response(EntryProtocol::Openai, StatusCode::UNAUTHORIZED, "invalid_api_key", "该 API Key 没有绑定任何分组");
    };
    let now_ms = crate::store::now_ms() as u64;
    // Task-27 deliverable 1b: the group itself is routable as a "model" (the
    // wildcard path — see router::is_wildcard), so a client must be able to
    // DISCOVER that name. The group's own exposed models come first, then the
    // group name and the group id, de-duplicated (a group whose name equals one
    // of its own models, or whose id equals its name, lists it once).
    let mut ids: Vec<&str> = group.exposed_models.iter().map(|m| m.as_str()).collect();
    for extra in [group.name.as_str(), group.id.as_str()] {
        if !ids.contains(&extra) {
            ids.push(extra);
        }
    }
    let data: Vec<Value> = ids.iter().map(|m| json!({
        "id": m, "object": "model", "created": now_ms / 1000, "owned_by": "gateway",
    })).collect();
    axum::Json(json!({ "object": "list", "data": data })).into_response()
}

pub fn iso_now() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn detect_entry(path: &str) -> EntryProtocol {
    if path.contains("/v1/messages") { EntryProtocol::Anthropic }
    else if path.contains(":generateContent") || path.contains(":streamGenerateContent") { EntryProtocol::Gemini }
    else { EntryProtocol::Openai }
}

fn parse_request_for(entry: EntryProtocol, body: &Value, exposed_model: &str, path_stream: bool) -> Result<ChatRequest, String> {
    match entry {
        EntryProtocol::Openai => openai::parse_request(body),
        EntryProtocol::Anthropic => anthropic::parse_request(body),
        EntryProtocol::Gemini => gemini::parse_request(body),
    }
    .map_err(|e| e.to_string())
    .map(|mut req| {
        // force the exposed model name into the IR (client asked for it)
        req.stream = req.stream || path_stream
            || body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
        req
    })
}

/// Shared data-plane handler for all three entries.
async fn handle_chat(state: State<crate::proxy::AppState>, headers: HeaderMap, path: String, body: Value) -> Response {
    let entry = detect_entry(&path);
    let group_id = match authenticate(&state, &headers) {
        Ok(g) => g,
        Err(r) => return r,
    };
    let Some(snap) = state.config.get() else {
        return error_response(entry, StatusCode::SERVICE_UNAVAILABLE, "config_not_loaded", "网关尚未加载任何配置");
    };
    let Some(group) = snap.config.groups.iter().find(|g| g.id == group_id) else {
        return error_response(entry, StatusCode::UNAUTHORIZED, "invalid_api_key", "该 API Key 没有绑定任何分组");
    };

    // resolve exposed model: from body (openai/anthropic) or path (gemini)
    let exposed_model = body
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            // gemini: /v1beta/models/{model}:generateContent
            path.split("/models/").nth(1).and_then(|rest| rest.split(':').next().map(|s| s.to_string()))
        })
        .unwrap_or_default();

    // Task-27 deliverable 1: the group's own name/id is routable too (wildcard).
    // EXACT WINS: an exposed model of this group always keeps the narrow
    // semantics, even when it happens to equal the group name — that decision
    // lives in ONE place, router::is_wildcard, which build_plan consults again
    // below; this guard only decides "404 or not".
    let wildcard = crate::router::is_wildcard(group, &exposed_model);
    if !wildcard && !group.exposed_models.iter().any(|m| m == &exposed_model) {
        return error_response(entry, StatusCode::NOT_FOUND, "model_not_found", &format!("模型「{exposed_model}」未对该分组开放"));
    }

    // Gemini native streaming is signaled by the URL suffix, not body.stream.
    let gemini_path_stream = entry == EntryProtocol::Gemini && path.contains(":streamGenerateContent");
    let ir_req = match parse_request_for(entry, &body, &exposed_model, gemini_path_stream) {
        Ok(r) => r,
        Err(msg) => return error_response(entry, StatusCode::BAD_REQUEST, "bad_request", &format!("请求体无效：{msg}")),
    };

    let request_id = uuid::Uuid::new_v4().to_string();
    let started = Instant::now();
    let client_gone = CancellationToken::new();

    // Per-request client frame channel — created before the walk so a
    // committed stream has a destination that belongs to THIS request only.
    //
    // The receiver MUST be created here and kept, never dropped: the
    // post-commit pump in proxy.rs starts emitting frames while the walk is
    // still running, and a channel whose receiver is gone has nowhere to put
    // them — every frame, including the terminator, would be lost and the
    // client would get a 200 with a zero-byte body.
    let (feed_tx, feed_rx) = new_client_channel();

    let ctx = ProxyCtx {
        state: &state,
        request_id: request_id.clone(),
        group_id: group_id.clone(),
        // The RAW requested name (exposed model, or the group's name/id on the
        // wildcard path). proxy.rs uses it for two things only: build_plan's
        // candidate filter and the echoed `model` field of every client frame —
        // so a wildcard request keeps echoing the group name the caller sent.
        exposed_model: exposed_model.clone(),
        entry,
        ir: ir_req,
        client_gone: client_gone.clone(),
        feed_tx: feed_tx.clone(),
    };

    // Preamble (protocol start frame) is emitted BEFORE the walk, because the
    // post-commit pump writes its frames while the walk is still running;
    // emitting it afterwards would deliver it after the terminator. These
    // frames stay inside this per-request channel until the streaming branch
    // below hands the receiver to the body, so I1 still holds: no byte reaches
    // the client socket before commit, and a failed walk discards the channel.
    feed_emit(&feed_tx, entry, &crate::protocol::StreamEvent::Start { model: exposed_model.clone() }, &exposed_model).await;

    // Client disconnect detection: token cancelled when the response body is
    // dropped (streaming path) — wired in the streaming branch below.
    let result = proxy_run(&ctx).await;
    let duration_ms = started.elapsed().as_millis() as u64;

    // persist switch + request logs
    for rec in &result.records {
        state.store.insert_switch_log(&crate::store::SwitchLog {
            ts: iso_now(),
            request_id: request_id.clone(),
            group_id: group_id.clone(),
            attempt_no: rec.attempt_no,
            tier: rec.tier,
            channel_id: rec.channel_id.clone(),
            channel_name: rec.channel_name.clone(),
            upstream_model: rec.upstream_model.clone(),
            outcome: if rec.error_class == "committed" { "committed" } else { "failed" }.to_string(),
            error_class: rec.error_class.to_string(),
            http_status: rec.http_status.map(|s| s as i64),
            latency_ms: rec.latency_ms as i64,
            message: rec.message.clone(),
        });
    }

    let status = match result.terminal {
        crate::router::Terminal::Exhausted => "exhausted",
        crate::router::Terminal::NoCandidate => "no_candidate",
        crate::router::Terminal::AllCooldown => "cooldown",
    };

    let final_channel_for_log = result.final_channel.clone();
    let final_model_for_log = result.final_upstream_model.clone();
    let attempts_for_log = result.records.len() as u32;
    // How many attempts the loop guard threw away before one produced an
    // answer. Counted here, outside the streaming `tokio::spawn` below, so the
    // detached task never has to borrow `result`. This is the same predicate
    // the /admin/api/logs/switch endpoint shows, so the two agree by construction.
    let loop_retries_for_log = result.records.iter().filter(|r| r.error_class == "degenerate_output").count() as u32;
    // How many of those discards were a THINKING loop rather than an answer
    // loop. Counted from the switch records, the same way as above, so the two
    // agree by construction — and so it survives the retry succeeding: the fact
    // that one candidate's deliberation circled is not undone by a later
    // candidate answering well, which is why this cannot be read off the pump's
    // outcome (that only ever describes the attempt that WON).
    let reasoning_truncated_for_log = result.records.iter()
        .filter(|r| r.error_class == "degenerate_output" && r.message.starts_with("思考复读"))
        .count() as i64;
    // Both figures below are priced from the SAME snapshot the walk ran against
    // (the one grabbed at :122 for routing), never a fresh `state.config.get()`:
    // a console save landing mid-request must not make one number come from one
    // price table and the other from the next. `snap.config` is an Arc clone.
    let price_snap = snap.clone();
    let wasted_cost = wasted_of(&price_snap, &result.records);

    match result.success {
        Some(Outcome::Full(resp)) => {
            let usage = &resp.usage;
            state.store.insert_request_log(&crate::store::RequestLog {
                id: None,
                ts: iso_now(),
                request_id: request_id.clone(),
                group_id: group_id.clone(),
                exposed_model: exposed_model.clone(),
                entry_protocol: format!("{entry:?}").to_lowercase(),
                status: "success".into(),
                final_channel: final_channel_for_log.clone(),
                final_upstream_model: final_model_for_log.clone(),
                attempts: attempts_for_log,
                loop_retries: loop_retries_for_log,
                // Non-stream: there is no pump, so nothing can be truncated
                // after delivery — the whole answer arrives in one piece.
                // Same for the thinking stream: nothing is relayed until the
                // answer is committed, so a reasoning loop there surfaces as a
                // discarded attempt (loop_retries), never as a truncation.
                loop_truncated: 0,
                loop_continues: 0,
                reasoning_truncated: 0,
                error_class: String::new(),
                error_message: String::new(),
                input_tokens: usage.input_tokens as i64,
                output_tokens: usage.output_tokens as i64,
                cache_read_tokens: usage.cache_read_tokens as i64,
                cache_creation_tokens: usage.cache_creation_tokens as i64,
                cost_usd: cost_of(&snap, &final_channel_for_log, &final_model_for_log, usage),
                wasted_cost_usd: wasted_cost,
                duration_ms: duration_ms as i64,
                ttft_ms: result.ttft_ms.unwrap_or(0) as i64,
            });
            // echo EXPOSED model name, never upstream
            let mut resp = resp;
            resp.model = exposed_model.clone();
            let rendered = match entry {
                EntryProtocol::Openai => openai::render_response(&resp),
                EntryProtocol::Anthropic => anthropic::render_response(&resp),
                EntryProtocol::Gemini => gemini::render_response(&resp),
            };
            (StatusCode::OK, axum::Json(rendered)).into_response()
        }
        Some(Outcome::Stream { outcome_rx }) => {
            // Hand over the receiver created before the walk (see above); it
            // already holds the preamble plus every frame the pump produced.
            let body_stream = tokio_stream_from_channel(feed_rx, client_gone);
            let mut response = Response::new(Body::from_stream(body_stream));
            response.headers_mut().insert("content-type", "text/event-stream".parse().unwrap());
            response.headers_mut().insert("cache-control", "no-cache".parse().unwrap());

            // log after pump ends (detached)
            let st = state.clone();
            let rid = request_id.clone();
            let gid = group_id.clone();
            let em = exposed_model.clone();
            let fc = result.final_channel.clone();
            let fum = result.final_upstream_model.clone();
            let recs = result.records.len() as u32;
            let lr = loop_retries_for_log;
            let dur = duration_ms as i64;
            let ttft = result.ttft_ms.map(|t| t as i64);
            // Moved into the detached logger so it can price the final usage with
            // the SAME cost_of the non-stream path uses. Without this the stream
            // row would carry a literal 0 and the console would show every
            // streaming request as free.
            let price_snap = price_snap.clone();
            let wasted = wasted_cost;
            tokio::spawn(async move {
                let outcome = outcome_rx.await.unwrap_or_default();
                let usage = outcome.usage;
                // Priced BEFORE the row below moves `fc`/`fum` into itself: a
                // struct literal evaluates its fields in written order, so a
                // borrowing call placed after them would not compile.
                let cost = cost_of(&price_snap, &fc, &fum, &usage);
                st.store.insert_request_log(&crate::store::RequestLog {
                    id: None, ts: iso_now(), request_id: rid, group_id: gid,
                    exposed_model: em, entry_protocol: format!("{entry:?}").to_lowercase(),
                    // The client received a complete, valid stream, so this is
                    // a success by I2 — but a truncated answer must not be
                    // reported as a clean one, so the cut is counted separately
                    // (a pre-commit hit is invisible to the client; a
                    // post-commit truncation is not).
                    status: "success".into(),
                    final_channel: fc, final_upstream_model: fum,
                    attempts: recs,
                    loop_retries: lr,
                    loop_truncated: outcome.loop_truncated as i64,
                    loop_continues: outcome.loop_continues,
                    reasoning_truncated: reasoning_truncated_for_log.max(outcome.reasoning_truncated as i64),
                    // A corrected answer is still a delivered answer, so the row
                    // stays "success"; the class records that it needed
                    // correcting, which is the whole point of showing a cause.
                    error_class: if outcome.loop_continues > 0 { "loop_continue" } else { "" }.to_string(),
                    // BOTH facts are reported when both are true. A corrected
                    // answer that later hit the round cap and got truncated is
                    // exactly that — saying only "纠正了 N 次" would hide the
                    // truncation, and saying only "已截断" would hide the work
                    // the correction did.
                    error_message: match (outcome.loop_continues, outcome.loop_truncated) {
                        (0, false) => String::new(),
                        (0, true) => "检测到输出复读，已截断收尾".to_string(),
                        (n, false) => format!("检测到输出复读，已自动纠正并续写 {n} 次"),
                        (n, true) => format!("检测到输出复读，已自动纠正并续写 {n} 次；仍复读，已截断收尾"),
                    },
                    input_tokens: usage.input_tokens as i64, output_tokens: usage.output_tokens as i64,
                    cache_read_tokens: usage.cache_read_tokens as i64, cache_creation_tokens: usage.cache_creation_tokens as i64,
                    cost_usd: cost, wasted_cost_usd: wasted, duration_ms: dur, ttft_ms: ttft.unwrap_or(0),
                });
            });
            response
        }
        None => {
            // WHY it failed, computed before the row so both the log row and the
            // client message can use it. `cause_counts` is the mix of classes
            // across every failed attempt; `last_cause` is the last real failure,
            // which is the one the client would have hit.
            let mut cause_counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
            for rec in result.records.iter().filter(|r| r.error_class != "committed") {
                *cause_counts.entry(rec.error_class).or_insert(0) += 1;
            }
            let cause_summary = cause_counts
                .iter()
                .map(|(class, n)| format!("{}×{}", crate::store::error_class_label(class), n))
                .collect::<Vec<_>>()
                .join("、");
            let last_cause = result
                .records
                .iter()
                .rev()
                .find(|r| r.error_class != "committed")
                .map(|r| r.message.clone())
                .unwrap_or_default();
            // The dominant class, for the row: it is the one-word answer to
            // "what went wrong" that the log could not previously give.
            let failure_class_for_log = cause_counts
                .iter()
                .max_by_key(|(_, n)| **n)
                .map(|(class, _)| (*class).to_string())
                .unwrap_or_default();
            // terminal failure: ONE structurally-valid error in the client's protocol
            state.store.insert_request_log(&crate::store::RequestLog {
                id: None, ts: iso_now(), request_id: request_id.clone(), group_id: group_id.clone(),
                exposed_model: exposed_model.clone(), entry_protocol: format!("{entry:?}").to_lowercase(),
                status: status.into(), final_channel: None, final_upstream_model: None,
                attempts: result.records.len() as u32,
                loop_retries: loop_retries_for_log,
                loop_truncated: 0,
                loop_continues: 0,
                // A THINKING loop still happened even though nothing was ever
                // committed: the discarded attempts are the record of it, and
                // "the only candidate's model circled in its own head" is
                // exactly what an operator needs to see here. Nothing was
                // relayed, so it is not a truncation of the answer — it is
                // counted from the same switch records as above.
                reasoning_truncated: reasoning_truncated_for_log,
                error_class: failure_class_for_log.clone(),
                error_message: last_cause.clone(),
                input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, cache_creation_tokens: 0,
                cost_usd: 0.0, wasted_cost_usd: wasted_cost,
                duration_ms: duration_ms as i64, ttft_ms: result.ttft_ms.unwrap_or(0) as i64,
            });
            let (code, kind) = match result.terminal {
                crate::router::Terminal::AllCooldown => (StatusCode::SERVICE_UNAVAILABLE, "all_upstreams_cooling"),
                crate::router::Terminal::NoCandidate => (StatusCode::NOT_FOUND, "no_route_for_model"),
                crate::router::Terminal::Exhausted => (StatusCode::BAD_GATEWAY, "all_upstreams_failed"),
            };
            // The client-facing text names the CAUSE, not just the count: a bare
            // "6 次尝试全部失败" forces the operator to open the switch log to
            // learn WHY, which is exactly the gap this fills.
            // When every attempt failed the same way the summary and the last
            // cause say the same thing, so only the detail is printed; the
            // summary earns its place only when the failures differ.
            let message = if cause_summary.is_empty() {
                format!("上游 {} 次尝试全部失败，请查看请求日志 {} 的失败原因", result.records.len(), request_id)
            } else if cause_counts.len() == 1 {
                format!("上游 {} 次尝试全部失败：{}。请求 ID：{}", result.records.len(), last_cause, request_id)
            } else {
                format!(
                    "上游 {} 次尝试全部失败（原因：{}）。最后一条失败原因：{}。请求 ID：{}",
                    result.records.len(),
                    cause_summary,
                    last_cause,
                    request_id,
                )
            };
            let mut r = error_response(entry, code, kind, &message);
            if matches!(result.terminal, crate::router::Terminal::AllCooldown) {
                // 头部值必须是裸的秒数 "30"。旧写法把整行 "retry-after: 30" 当成值插入，
                // 客户端解析 Retry-After 会拿到畸形字符串。
                r.headers_mut().insert("retry-after", axum::http::HeaderValue::from_static("30"));
            }
            r
        }
    }
}

fn cost_of(snap: &crate::config::ConfigSnapshot, final_channel: &Option<String>, final_upstream_model: &Option<String>, usage: &crate::protocol::ir::Usage) -> f64 {
    let Some(chan) = final_channel.as_ref() else { return 0.0 };
    let Some(model) = final_upstream_model.as_ref() else { return 0.0 };
    let Some(b) = snap.config.models.iter().find(|m| &m.channel_id == chan && &m.upstream == model) else { return 0.0 };
    let p = b.price_per_m;
    let in_m = usage.input_tokens as f64 / 1_000_000.0;
    let out_m = usage.output_tokens as f64 / 1_000_000.0;
    let cr_m = usage.cache_read_tokens as f64 / 1_000_000.0;
    let cc_m = usage.cache_creation_tokens as f64 / 1_000_000.0;
    p[0] * in_m + p[1] * out_m + p[2] * cr_m + p[3] * cc_m
}

/// What the attempts that were THROWN AWAY actually cost.
///
/// A committed attempt is skipped: its tokens are the answer the client got, so
/// they are already counted in that request's `cost_usd`, and adding them here
/// would bill the same tokens twice. Everything else is priced through the very
/// same `cost_of` as the normal path — one pricing function, so the "what we
/// paid" and the "what we wasted" columns can never drift apart.
///
/// A record with no observed usage contributes 0. That is not a fudge: nothing
/// was ever parsed from that attempt, and inventing an estimate here would put
/// a number in the console that no upstream ever confirmed. A record whose
/// channel/model binding cannot be found (deleted between the attempt and the
/// log write), or whose price is all zeroes, also contributes exactly 0.
fn wasted_of(snap: &crate::config::ConfigSnapshot, records: &[crate::proxy::SwitchRecord]) -> f64 {
    let mut wasted = 0.0;
    for rec in records {
        if rec.error_class == "committed" { continue }
        let ch = Some(rec.channel_id.clone());
        let up = Some(rec.upstream_model.clone());
        wasted += cost_of(snap, &ch, &up, &rec.usage);
    }
    wasted
}

/// Bridge broadcast frames into an axum body stream.
/// Bridge pump frames into an axum body stream.
///
/// mpsc (not broadcast) so no frame can be dropped: the pump awaits on a full
/// channel, which backpressures the upstream read loop instead of discarding
/// frames. `recv()` returning None means the pump finished and dropped its
/// sender, which is the normal end of stream.
fn tokio_stream_from_channel(
    mut rx: tokio::sync::mpsc::Receiver<ClientFrame>,
    _cancel: CancellationToken,
) -> impl futures::Stream<Item = Result<bytes::Bytes, std::io::Error>> {
    async_stream::stream! {
        while let Some(frame) = rx.recv().await {
            match frame {
                ClientFrame::Sse(s) => yield Ok(bytes::Bytes::from(s)),
                ClientFrame::Close => break,
            }
        }
    }
}

pub fn data_plane_router(state: crate::proxy::AppState) -> axum::Router {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(handle_openai))
        .route("/v1/messages", post(handle_anthropic))
        .route("/v1beta/models/{model_path}", post(handle_gemini_generic))
        .with_state(state)
}

async fn handle_openai(state: State<crate::proxy::AppState>, headers: HeaderMap, body: String) -> Response {
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    handle_chat(state, headers, "/v1/chat/completions".into(), v).await
}

async fn handle_anthropic(state: State<crate::proxy::AppState>, headers: HeaderMap, body: String) -> Response {
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    handle_chat(state, headers, "/v1/messages".into(), v).await
}

/// Gemini entry: /v1beta/models/{model}:generateContent (or streamGenerateContent)
async fn handle_gemini_generic(state: State<crate::proxy::AppState>, headers: HeaderMap, axum::extract::Path(model_path): axum::extract::Path<String>, body: String) -> Response {
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    handle_chat(state, headers, format!("/v1beta/models/{model_path}"), v).await
}

/// D2: the wasted-cost helpers. They are private to this module, so their tests
/// sit next to them and call them exactly the way the request path does.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Channel, Config, ConfigSnapshot, ModelBinding, Protocol};

    fn binding(channel_id: &str, upstream: &str, price: [f64; 4]) -> ModelBinding {
        ModelBinding {
            id: format!("{channel_id}-{upstream}"),
            channel_id: channel_id.to_string(),
            exposed: "exp".to_string(),
            upstream: upstream.to_string(),
            price_per_m: price,
            enabled: true,
        }
    }

    fn snapshot(models: Vec<ModelBinding>) -> ConfigSnapshot {
        let channels = models
            .iter()
            .map(|m| Channel {
                id: m.channel_id.clone(),
                name: m.channel_id.clone(),
                base_url: "http://127.0.0.1:1".to_string(),
                protocol: Protocol::Openai,
                api_key: "sk-x".to_string(),
                enabled: true,
                weight: 1,
                headers: Default::default(),
            })
            .collect();
        ConfigSnapshot {
            config: std::sync::Arc::new(Config { channels, models, ..Config::default() }),
            version: 1,
        }
    }

    fn usage(input: u64, output: u64, cache_read: u64, cache_creation: u64) -> crate::protocol::ir::Usage {
        crate::protocol::ir::Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_creation_tokens: cache_creation,
        }
    }

    fn record(channel: &str, upstream: &str, error_class: &'static str, u: crate::protocol::ir::Usage) -> crate::proxy::SwitchRecord {
        crate::proxy::SwitchRecord {
            attempt_no: 1,
            tier: 1,
            channel_id: channel.to_string(),
            channel_name: channel.to_string(),
            upstream_model: upstream.to_string(),
            error_class,
            http_status: Some(502),
            latency_ms: 5,
            message: String::new(),
            usage: u,
        }
    }

    fn approx(got: f64, want: f64) {
        assert!((got - want).abs() < 1e-9, "expected {want}, got {got}");
    }

    /// The regression this whole defect is about: a failed attempt that really
    /// did consume tokens used to be reported as 0, because the helper was an
    /// empty body returning the literal 0.0.
    #[test]
    fn a_failed_attempt_with_usage_is_priced() {
        let snap = snapshot(vec![binding("c1", "up1", [1.0, 2.0, 0.0, 0.0])]);
        // 1M input at 1 USD per M, plus 1M output at 2 USD per M.
        let recs = vec![record("c1", "up1", "http_5xx", usage(1_000_000, 1_000_000, 0, 0))];
        let w = wasted_of(&snap, &recs);
        assert!(w > 0.0, "a discarded attempt that observed usage must cost money, got {w}");
        approx(w, 3.0);
    }

    /// Every price slot is read from its own index, so a cache-heavy attempt
    /// cannot be silently billed at the input rate.
    #[test]
    fn cache_token_prices_use_their_own_slots() {
        let snap = snapshot(vec![binding("c1", "up1", [0.0, 0.0, 3.0, 4.0])]);
        let recs = vec![record("c1", "up1", "aborted", usage(0, 0, 1_000_000, 1_000_000))];
        approx(wasted_of(&snap, &recs), 7.0);
    }

    /// The committed attempt is the answer the client actually received, and
    /// its tokens are already counted in that request cost_usd. Pricing it here
    /// again would bill the same tokens twice.
    #[test]
    fn a_committed_attempt_is_not_wasted() {
        let snap = snapshot(vec![binding("c1", "up1", [1.0, 0.0, 0.0, 0.0])]);
        let committed = vec![record("c1", "up1", "committed", usage(1_000_000, 0, 0, 0))];
        assert_eq!(wasted_of(&snap, &committed), 0.0, "a committed attempt must not be counted as waste");

        // The SAME usage under a non-committed class does cost 1.0, which proves
        // the 0 above came from the skip and not from a failed binding lookup.
        let failed = vec![record("c1", "up1", "http_5xx", usage(1_000_000, 0, 0, 0))];
        approx(wasted_of(&snap, &failed), 1.0);
    }

    /// 0 is the honest answer for a free channel and for a record whose binding
    /// disappeared, and it must be produced by pricing the record, not by
    /// refusing to look at it.
    #[test]
    fn zero_price_and_unknown_bindings_contribute_zero() {
        let free = snapshot(vec![binding("c1", "up1", [0.0; 4])]);
        let recs = vec![record("c1", "up1", "http_5xx", usage(5_000_000, 5_000_000, 0, 0))];
        assert_eq!(wasted_of(&free, &recs), 0.0, "an all-zero price table must stay 0");

        let other = snapshot(vec![binding("c9", "up9", [1.0, 1.0, 1.0, 1.0])]);
        assert_eq!(wasted_of(&other, &recs), 0.0, "a record with no matching binding must stay 0");
        assert_eq!(cost_of(&other, &Some("c1".into()), &Some("up1".into()), &usage(1_000_000, 0, 0, 0)), 0.0);
    }

    /// Several discarded attempts add up, each priced on its own channel: the
    /// retry that moved to a pricier channel is the expensive one.
    #[test]
    fn discarded_attempts_sum_across_channels() {
        let snap = snapshot(vec![
            binding("c1", "up1", [1.0, 0.0, 0.0, 0.0]),
            binding("c2", "up2", [2.0, 0.0, 0.0, 0.0]),
        ]);
        let recs = vec![
            record("c1", "up1", "http_5xx", usage(1_000_000, 0, 0, 0)),
            record("c2", "up2", "timeout", usage(1_000_000, 0, 0, 0)),
            record("c2", "up2", "committed", usage(1_000_000, 0, 0, 0)),
        ];
        approx(wasted_of(&snap, &recs), 3.0);
        // The normal path keeps pricing the committed answer on its own channel.
        approx(cost_of(&snap, &Some("c2".into()), &Some("up2".into()), &usage(1_000_000, 0, 0, 0)), 2.0);
        // Nothing observed means nothing charged, rather than an invented estimate.
        assert_eq!(cost_of(&snap, &Some("c1".into()), &Some("up1".into()), &usage(0, 0, 0, 0)), 0.0);
    }
}
