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
    // A group override replaces the global policy for that group alone, so it has
    // to clear the very same invariant — otherwise a user could save a group whose
    // budget cannot fit its own walk and only discover it as a runtime exhaustion.
    // The offending group is named so the console can point at the right row.
    for g in &cfg.groups {
        if let Some(ov) = g.overrides.retry.as_ref() {
            if let Err(msg) = crate::router::validate_budget(ov) {
                return (StatusCode::BAD_REQUEST, Json(json!({ "error": format!("group {}: {}", g.id, msg) }))).into_response();
            }
        }
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
    // Probe with the SAME connect timeout the data plane uses right now, so a
    // config change is reflected in the admin connectivity test too.
    let mut http = state.http.client_for(snap.config.retry.connect_timeout_ms).post(&url).json(&body);
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

/// GET /admin/api/group-members?group_id=<id> — task-27 deliverable 2.
///
/// A per-MEMBER snapshot of one routing group: the frozen console shape (field
/// names are part of the client API — renaming one silently breaks the UI).
///
/// Ordering is the SAME as router::build_plan's candidate order (tier ascending,
/// declared order inside a tier, weight-descending when cfg.auto_weight is on)
/// so the console lists members exactly like the walk tries them. Unlike the
/// plan, EVERY member is listed — disabled and cooling ones included — because
/// the panel has to be able to show "this member exists but is not usable now".
///
/// Counters come from two sources and are never null: requests/tokens/cost/avgs
/// from landed request_logs successes, failed_attempts/loop_retries from
/// switch_logs failures. Members with no rows at all still render real 0s (and
/// null averages: "no data" is not "0 ms").
#[derive(Deserialize)]
pub struct MemberQuery {
    pub group_id: String,
}

pub async fn group_members(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<MemberQuery>) -> Response {
    if let Err(r) = authorize(&state, &headers) { return r }
    let Some(snap) = state.config.get() else { return (StatusCode::SERVICE_UNAVAILABLE, "no config").into_response() };
    let Some(group) = snap.config.groups.iter().find(|g| g.id == q.group_id) else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": format!("unknown group {}", q.group_id) }))).into_response();
    };

    // The member that served this group's most recent successful request
    // ("<channel_id>::<upstream_model>"), or None when the group has never
    // had a success. NOTE: "active" is last-success-based, NOT in-flight — a
    // long-running stream does not change it until it lands.
    let active_key: Option<String> = state.store.active_member_key(&group.id).unwrap_or(None);
    let stats = state.store.group_member_stats(&group.id).unwrap_or_default();
    let cooling_rows = state.health.snapshot_summary();

    let mut tiers: Vec<&crate::config::Tier> = group.tiers.iter().collect();
    tiers.sort_by_key(|t| t.level);

    let mut members: Vec<Value> = vec![];
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for tier in tiers {
        let mut slots: Vec<&crate::config::TierMember> = tier.members.iter().collect();
        if snap.config.auto_weight {
            // Same opt-in weighting as build_plan: heaviest first, and sort_by is
            // stable so equal weights keep the declared order.
            slots.sort_by(|a, b| b.weight.cmp(&a.weight));
        }
        for (slot, mem) in slots.iter().enumerate() {
            let binding = snap.config.find_model(&mem.model_id);
            let channel = binding.and_then(|m| snap.config.find_channel(&m.channel_id));
            // A member whose model/channel vanished is still rendered (the panel
            // must show the dangling row); ids are the best identity we have.
            let channel_id = channel.map(|c| c.id.clone()).unwrap_or_else(|| binding.map(|m| m.channel_id.clone()).unwrap_or_default());
            let channel_name = channel.map(|c| c.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| channel_id.clone());
            let upstream_model = binding.map(|m| m.upstream.clone()).unwrap_or_default();
            let key = format!("{channel_id}::{upstream_model}");
            // Same (channel, upstream) dedupe as build_plan, so the key really is
            // unique per payload and the console row id stays stable.
            if !seen.insert(key.clone()) { continue }

            // A member is usable only when BOTH its binding and its channel are
            // enabled — build_plan skips it on either.
            let enabled = binding.map(|m| m.enabled).unwrap_or(false) && channel.map(|c| c.enabled).unwrap_or(false);
            let (cooldown_kind, cooling_until_ms) = cooling_for(&cooling_rows, &channel_id, &upstream_model);

            let st = stats.get(&key);
            let int = |f: &str| -> i64 { st.and_then(|v| v.get(f)).and_then(|x| x.as_i64()).unwrap_or(0) };
            let num = |f: &str| -> Value {
                match st.and_then(|v| v.get(f)) {
                    Some(v) if !v.is_null() => v.clone(),
                    _ => Value::Null,
                }
            };
            let classes = st.and_then(|v| v.get("error_classes")).cloned().unwrap_or_else(|| json!([]));
            // True for the one member whose key equals active_key (at most one per payload).
            let active = active_key.as_deref() == Some(key.as_str());

            members.push(json!({
                "key": key,
                "model_id": mem.model_id,
                "tier": tier.level,
                // Position inside this tier, counted AFTER the auto_weight sort
                // when it is on: slot N means the N-th candidate, not the N-th
                // line of the config file.
                "slot": slot,
                "weight": mem.weight,
                "channel_id": channel_id,
                "channel_name": channel_name,
                "upstream_model": upstream_model,
                "exposed_model": binding.map(|m| m.exposed.clone()).unwrap_or_default(),
                "enabled": enabled,
                "cooling": cooldown_kind.is_some(),
                "cooling_until_ms": cooling_until_ms,
                "cooldown_kind": cooldown_kind,
                "requests": int("requests"),
                "failed_attempts": int("failed_attempts"),
                "loop_retries": int("loop_retries"),
                "error_classes": classes,
                "avg_duration_ms": num("avg_duration_ms"),
                "avg_ttft_ms": num("avg_ttft_ms"),
                "input_tokens": int("input_tokens"),
                "output_tokens": int("output_tokens"),
                "cost_usd": st.and_then(|v| v.get("cost_usd")).and_then(|x| x.as_f64()).unwrap_or(0.0),
                "last_used_ts": num("last_used_ts"),
                "last_outcome": num("last_outcome"),
                "last_error_class": num("last_error_class"),
                "active": active,
            }));
        }
    }

    // The retry policy this group actually runs on, plus its provenance, so the
    // console can show "跟随全局" or the group's own numbers without re-deriving
    // the rule. `effective_retry` is the same function the data plane calls —
    // one source of truth.
    let rc = crate::config::effective_retry(&snap.config, group);
    let retry_source = if group.overrides.retry.is_some() { "group" } else { "global" };

    Json(json!({
        "group_id": group.id,
        "group_name": group.name,
        "active_key": active_key,
        "retry": rc,
        "retry_source": retry_source,
        "global_retry": snap.config.retry,
        "members": members,
    })).into_response()
}

/// Cooling facts for one (channel, upstream) pair, or (None, None) when the
/// member is usable.
///
/// Either key counts, exactly like build_plan's split: the whole-channel key
/// (an auth failure isolates every model of the channel) or the model-scoped one.
/// A PERMANENT isolation has no real expiry — the snapshot's 0 is a sentinel, not
/// an epoch millisecond — so it is reported as cooling_until_ms: null and the
/// kind is what tells the other side why (the console then prints a "剩余时间未知"
/// placeholder instead of a bogus countdown to 1970).
fn cooling_for(rows: &[crate::health::CooldownRow], channel_id: &str, upstream_model: &str) -> (Option<&'static str>, Option<u64>) {
    for r in rows {
        let scoped = r.channel_id == channel_id && r.upstream_model == upstream_model;
        let whole = r.channel_id == channel_id && r.upstream_model == "*";
        if scoped || whole {
            return (Some(r.kind), if r.permanent { None } else { Some(r.cooling_until_ms) });
        }
    }
    (None, None)
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
        .route("/admin/api/group-members", get(group_members))
        .route("/admin/api/health", get(health_summary))
        .route("/admin/api/test-channel", post(test_channel))
        .route("/admin/api/cooldown/clear", post(clear_cooldown))
        .with_state(state).layer(cors)
}

use std::time::Instant;
