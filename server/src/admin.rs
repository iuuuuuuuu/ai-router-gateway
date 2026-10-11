//! Admin plane: config CRUD, logs, stats, health, real-request channel test.

use crate::proxy::AppState;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

fn authorize(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    let Some(snap) = state.config.get() else { return Err((StatusCode::SERVICE_UNAVAILABLE, "no config").into_response()) };
    let Some(expected) = &snap.config.admin.key else {
        return Ok(()); // no admin key configured: localhost-only by bind
    };
    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-admin-key").and_then(|v| v.to_str().ok()))
        .unwrap_or("");
    if presented == expected {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, "invalid admin key").into_response())
    }
}

pub async fn get_config(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = authorize(&state, &headers) { return r }
    match state.store.load_config() {
        Ok(Some(cfg)) => Json(cfg).into_response(),
        Ok(None) => Json(json!({})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("load config: {e}")).into_response(),
    }
}

pub async fn put_config(State(state): State<AppState>, headers: HeaderMap, Json(cfg): Json<crate::config::Config>) -> Response {
    if let Err(r) = authorize(&state, &headers) { return r }

    // I4 validation BEFORE publish
    if let Err(msg) = crate::router::validate_budget(&cfg.retry) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response();
    }
    // model binding integrity
    for m in &cfg.models {
        if cfg.find_channel(&m.channel_id).is_none() {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": format!("model {} references missing channel {}", m.id, m.channel_id) }))).into_response();
        }
    }
    for g in &cfg.groups {
        for t in &g.tiers {
            for mem in &t.members {
                if cfg.find_model(&mem.model_id).is_none() {
                    return (StatusCode::BAD_REQUEST, Json(json!({ "error": format!("group {} tier {} references missing model {}", g.id, t.level, mem.model_id) }))).into_response();
                }
            }
        }
    }

    // Defect B: reconcile cooldowns with the config we are about to save.
    // Snapshotted BEFORE publish so we can tell which channel identities changed
    // (a deleted channel, or one whose api_key/base_url was rewritten, must not
    // keep its 100-year auth isolation / stale backoff rows).
    let old_channels: Vec<crate::config::Channel> = state
        .config
        .get()
        .map(|s| s.config.channels.clone())
        .unwrap_or_default();

    let json = serde_json::to_value(&cfg).unwrap_or(Value::Null);
    if let Err(e) = state.store.save_config(&cfg) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("save config: {e}")).into_response();
    }
    let _ = json;
    let cleared = state.health.reconcile_config(&old_channels, &cfg);
    if cleared > 0 {
        tracing::info!(cleared, "cooldown state reconciled with the new config");
    }
    let version = state.config.publish(cfg);
    state.health.persist();
    Json(json!({ "ok": true, "config_version": version, "cooldowns_cleared": cleared })).into_response()
}

#[derive(Deserialize)]
pub struct LogQuery {
    pub limit: Option<u32>,
    pub request_id: Option<String>,
    /// Scopes the log to one routing group; omitted = every group.
    pub group_id: Option<String>,
}

pub async fn switch_logs(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<LogQuery>) -> Response {
    if let Err(r) = authorize(&state, &headers) { return r }
    match state.store.list_switch_logs((q.limit.unwrap_or(100).min(1000)) as i64, q.request_id.as_deref(), q.group_id.as_deref()) {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

pub async fn request_logs(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<LogQuery>) -> Response {
    if let Err(r) = authorize(&state, &headers) { return r }
    match state.store.list_request_logs((q.limit.unwrap_or(100).min(1000)) as i64, q.request_id.as_deref(), q.group_id.as_deref()) {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

#[derive(Deserialize)]
pub struct StatsQuery {
    /// Scopes summary + by_model to one routing group; by_group is always the
    /// full cross-group overview so the console can show the picker.
    pub group_id: Option<String>,
}

pub async fn stats(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<StatsQuery>) -> Response {
    if let Err(r) = authorize(&state, &headers) { return r }
    let scope = q.group_id.as_deref().filter(|g| !g.is_empty());
    let summary = state.store.stats_summary(scope).unwrap_or_else(|_| json!({}));
    let by_model = state.store.stats_by_model(scope).unwrap_or_else(|_| vec![]);
    let by_group = state.store.stats_by_group().unwrap_or_else(|_| vec![]);
    let health: Vec<Value> = state.health.snapshot_summary().into_iter().map(cooldown_json).collect();
    Json(json!({ "summary": summary, "by_model": by_model, "by_group": by_group, "group_id": scope, "health": health })).into_response()
}

/// POST /admin/api/test-channel — REAL minimal request (never a fake probe).
/// Uses a genuine question so providers cannot flag it as probing.
#[derive(Deserialize)]
pub struct TestChannelReq {
    pub channel_id: String,
    /// optional upstream model override; defaults to first binding
    pub upstream_model: Option<String>,
}

pub async fn test_channel(State(state): State<AppState>, headers: HeaderMap, Json(req): Json<TestChannelReq>) -> Response {
    if let Err(r) = authorize(&state, &headers) { return r }
    let Some(snap) = state.config.get() else { return (StatusCode::SERVICE_UNAVAILABLE, "no config").into_response() };
    let Some(ch) = snap.config.find_channel(&req.channel_id) else {
        return (StatusCode::NOT_FOUND, format!("channel {} not found", req.channel_id)).into_response();
    };

    let upstream_model = req.upstream_model.unwrap_or_else(|| {
        snap.config.models.iter().find(|m| m.channel_id == ch.id).map(|m| m.upstream.clone()).unwrap_or_else(|| "gpt-4o-mini".into())
    });

    // A REAL question with sane parameters; max_tokens small but not probe-y.
    let (url, body): (String, Value) = match ch.protocol {
        crate::config::Protocol::Openai => (
            format!("{}/v1/chat/completions", ch.base_url.trim_end_matches('/')),
            json!({ "model": upstream_model, "messages": [{ "role": "user", "content": "请用一句话解释什么是负载均衡" }], "max_tokens": 200, "temperature": 0.7 }),
        ),
        crate::config::Protocol::Anthropic => (
            format!("{}/v1/messages", ch.base_url.trim_end_matches('/')),
            json!({ "model": upstream_model, "messages": [{ "role": "user", "content": "请用一句话解释什么是负载均衡" }], "max_tokens": 300 }),
        ),
        crate::config::Protocol::Gemini => (
            format!("{}/v1beta/models/{upstream_model}:generateContent", ch.base_url.trim_end_matches('/')),
            json!({ "contents": [{ "parts": [{ "text": "请用一句话解释什么是负载均衡" }] }] }),
        ),
    };

    let started = Instant::now();
    let mut http = state.http.post(&url).json(&body);
    http = match ch.protocol {
        crate::config::Protocol::Openai => http.bearer_auth(&ch.api_key),
        crate::config::Protocol::Anthropic => http.header("x-api-key", &ch.api_key).header("anthropic-version", "2023-06-01"),
        crate::config::Protocol::Gemini => http.header("x-goog-api-key", &ch.api_key),
    };

    let resp = match tokio::time::timeout(std::time::Duration::from_secs(30), http.send()).await {
        Err(_) => Json(json!({ "ok": false, "error": "timeout after 30s", "latency_ms": started.elapsed().as_millis() as u64 })).into_response(),
        Ok(Err(e)) => Json(json!({ "ok": false, "error": format!("connect: {e}"), "latency_ms": started.elapsed().as_millis() as u64 })).into_response(),
        Ok(Ok(r)) => {
            let status = r.status().as_u16();
            let text = r.text().await.unwrap_or_default();
            let ok = (200..300).contains(&status);
            Json(json!({
                "ok": ok, "status": status,
                "latency_ms": started.elapsed().as_millis() as u64,
                "sample": text.chars().take(400).collect::<String>(),
            })).into_response()
        }
    };
    resp
}

/// One cooling row, in the FROZEN console contract. Field names are part of
/// the client API — renaming one silently breaks the UI.
fn cooldown_json(r: crate::health::CooldownRow) -> Value {
    json!({
        "channel_id": r.channel_id,
        "upstream_model": r.upstream_model,
        "kind": r.kind,
        "permanent": r.permanent,
        "cooling_until_ms": r.cooling_until_ms,
        "remaining_ms": r.remaining_ms,
        "consecutive_failures": r.consecutive_failures,
    })
}

pub async fn health_summary(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = authorize(&state, &headers) { return r }
    let rows: Vec<Value> = state.health.snapshot_summary().into_iter().map(cooldown_json).collect();
    Json(json!({ "cooldowns": rows })).into_response()
}

/// POST /admin/api/cooldown/clear { channel_id }
#[derive(Deserialize)]
pub struct ClearCooldown { pub channel_id: String }

pub async fn clear_cooldown(State(state): State<AppState>, headers: HeaderMap, Json(req): Json<ClearCooldown>) -> Response {
    if let Err(r) = authorize(&state, &headers) { return r }
    state.health.clear_channel(&req.channel_id);
    state.health.persist();
    Json(json!({ "ok": true })).into_response()
}

pub fn admin_router(state: AppState) -> axum::Router {
    use axum::routing::{get, post, put};
    use tower_http::cors::{Any, CorsLayer};
    let cors = CorsLayer::new()
        .allow_origin([
            "http://tauri.localhost".parse::<HeaderValue>().unwrap(),
            "https://tauri.localhost".parse::<HeaderValue>().unwrap(),
            "http://localhost:5173".parse::<HeaderValue>().unwrap(),
        ])
        .allow_methods(Any)
        .allow_headers(Any);
    axum::Router::new()
        .route("/admin/api/config", get(get_config).put(put_config))
        .route("/admin/api/logs/switch", get(switch_logs))
        .route("/admin/api/logs/requests", get(request_logs))
        .route("/admin/api/stats", get(stats))
        .route("/admin/api/health", get(health_summary))
        .route("/admin/api/test-channel", post(test_channel))
        .route("/admin/api/cooldown/clear", post(clear_cooldown))
        .with_state(state).layer(cors)
}

use std::time::Instant;
