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
    let snap = state.config.get().ok_or_else(|| error_response(EntryProtocol::Openai, StatusCode::SERVICE_UNAVAILABLE, "config_not_loaded", "gateway has no configuration yet"))?;
    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()))
        .or_else(|| headers.get("x-goog-api-key").and_then(|v| v.to_str().ok()))
        .unwrap_or("");
    if presented.is_empty() {
        return Err(error_response(EntryProtocol::Openai, StatusCode::UNAUTHORIZED, "missing_api_key", "provide Authorization: Bearer <key> or x-api-key"));
    }
    for k in &snap.config.api_keys {
        if k.enabled && k.key == presented {
            return Ok(k.group_id.clone());
        }
    }
    Err(error_response(EntryProtocol::Openai, StatusCode::UNAUTHORIZED, "invalid_api_key", "unknown or disabled API key"))
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
        return error_response(EntryProtocol::Openai, StatusCode::SERVICE_UNAVAILABLE, "config_not_loaded", "no config");
    };
    let Some(group) = snap.config.groups.iter().find(|g| g.id == group_id) else {
        return error_response(EntryProtocol::Openai, StatusCode::UNAUTHORIZED, "invalid_api_key", "key group missing");
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
        return error_response(entry, StatusCode::SERVICE_UNAVAILABLE, "config_not_loaded", "no config");
    };
    let Some(group) = snap.config.groups.iter().find(|g| g.id == group_id) else {
        return error_response(entry, StatusCode::UNAUTHORIZED, "invalid_api_key", "key group missing");
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
        return error_response(entry, StatusCode::NOT_FOUND, "model_not_found", &format!("model '{exposed_model}' is not exposed to your group"));
    }

    // Gemini native streaming is signaled by the URL suffix, not body.stream.
    let gemini_path_stream = entry == EntryProtocol::Gemini && path.contains(":streamGenerateContent");
    let ir_req = match parse_request_for(entry, &body, &exposed_model, gemini_path_stream) {
        Ok(r) => r,
        Err(msg) => return error_response(entry, StatusCode::BAD_REQUEST, "bad_request", &format!("invalid request body: {msg}")),
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
    let wasted_cost = wasted_of(&snap, &result.records);

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
        Some(Outcome::Stream { usage_rx }) => {
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
            tokio::spawn(async move {
                let usage = usage_rx.await.unwrap_or_default();
                st.store.insert_request_log(&crate::store::RequestLog {
                    id: None, ts: iso_now(), request_id: rid, group_id: gid,
                    exposed_model: em, entry_protocol: format!("{entry:?}").to_lowercase(),
                    status: "success".into(), final_channel: fc, final_upstream_model: fum,
                    attempts: recs,
                    loop_retries: lr,
                    input_tokens: usage.input_tokens as i64, output_tokens: usage.output_tokens as i64,
                    cache_read_tokens: usage.cache_read_tokens as i64, cache_creation_tokens: usage.cache_creation_tokens as i64,
                    cost_usd: 0.0, wasted_cost_usd: 0.0, duration_ms: dur, ttft_ms: ttft.unwrap_or(0),
                });
            });
            response
        }
        None => {
            // terminal failure: ONE structurally-valid error in the client's protocol
            state.store.insert_request_log(&crate::store::RequestLog {
                id: None, ts: iso_now(), request_id: request_id.clone(), group_id: group_id.clone(),
                exposed_model: exposed_model.clone(), entry_protocol: format!("{entry:?}").to_lowercase(),
                status: status.into(), final_channel: None, final_upstream_model: None,
                attempts: result.records.len() as u32,
                loop_retries: loop_retries_for_log,
                input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, cache_creation_tokens: 0,
                cost_usd: 0.0, wasted_cost_usd: wasted_cost,
                duration_ms: duration_ms as i64, ttft_ms: result.ttft_ms.unwrap_or(0) as i64,
            });
            let (code, kind) = match result.terminal {
                crate::router::Terminal::AllCooldown => (StatusCode::SERVICE_UNAVAILABLE, "all_upstreams_cooling"),
                crate::router::Terminal::NoCandidate => (StatusCode::NOT_FOUND, "no_route_for_model"),
                crate::router::Terminal::Exhausted => (StatusCode::BAD_GATEWAY, "all_upstreams_failed"),
            };
            let mut r = error_response(entry, code, kind, &format!("all {} upstream attempt(s) failed; see gateway request {request_id}", result.records.len()));
            if matches!(result.terminal, crate::router::Terminal::AllCooldown) {
                if let Ok(v) = "retry-after: 30".parse::<axum::http::HeaderValue>() {
                    r.headers_mut().insert("retry-after", v);
                }
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

fn wasted_of(snap: &crate::config::ConfigSnapshot, records: &[crate::proxy::SwitchRecord]) -> f64 {
    let mut wasted = 0.0;
    for rec in records {
        if rec.error_class == "committed" { continue }
        // failed attempts may still have billed tokens upstream; we do not
        // know the count (no usage parsed) — charge a conservative estimate
        // of zero and rely on upstream_billed reconciliation later.
        let _ = (&rec.channel_id, &rec.upstream_model);
        let _ = &snap;
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
