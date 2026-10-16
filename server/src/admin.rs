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

pub async fn put_config(State(state): State<AppState>, headers: HeaderMap, Json(mut cfg): Json<crate::config::Config>) -> Response {
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

    // Exposed models are DERIVED, not declared: the client may be an older
    // build that still posts a hand-typed list (or a stale one), so the server
    // re-derives every group's list from the members it just validated. This
    // runs AFTER the integrity checks on purpose — a dangling member id must
    // still be a 400, not a silently shortened list — and BEFORE the save, so
    // the document that lands in SQLite (and that GET /admin/api/config hands
    // back) is already the derived one.
    let exposed_synced = cfg.normalize_exposed_models();
    if exposed_synced > 0 {
        tracing::info!(groups = exposed_synced, "exposed models re-derived from group members");
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

    if let Err(e) = state.store.save_config(&cfg) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("save config: {e}")).into_response();
    }
    let cleared = state.health.reconcile_config(&old_channels, &cfg);
    if cleared > 0 {
        tracing::info!(cleared, "cooldown state reconciled with the new config");
    }
    let version = state.config.publish(cfg);
    state.health.persist();
    Json(json!({
        "ok": true,
        "config_version": version,
        "cooldowns_cleared": cleared,
        // How many groups the derivation above rewrote. 0 means the client's
        // document already agreed with its own members.
        "exposed_synced": exposed_synced,
    })).into_response()
}

#[derive(Deserialize)]
pub struct LogQuery {
    /// Page size; omitted = 100, and the store clamps it to 1000.
    pub limit: Option<u32>,
    /// Rows to skip, newest-first: `offset = (page - 1) * limit`. Omitted = 0,
    /// so every pre-paging caller keeps getting the newest `limit` rows.
    pub offset: Option<u32>,
    pub request_id: Option<String>,
    /// Scopes the log to one routing group; omitted = every group.
    pub group_id: Option<String>,
}

/// Answers `{"rows":[...],"total":N,"limit":L,"offset":O}`. `total` is the row
/// count the SAME filter matches ignoring the page window — that is what the
/// console draws "第 X / Y 页" from, and it comes from the same `build_filters`
/// as the rows, so the two can never disagree.
pub async fn switch_logs(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<LogQuery>) -> Response {
    if let Err(r) = authorize(&state, &headers) { return r }
    let limit = q.limit.unwrap_or(100) as i64;
    let offset = q.offset.unwrap_or(0) as i64;
    match state.store.list_switch_logs_page(limit, offset, q.request_id.as_deref(), q.group_id.as_deref()) {
        Ok(page) => Json(page.into_json()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

pub async fn request_logs(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<LogQuery>) -> Response {
    if let Err(r) = authorize(&state, &headers) { return r }
    let limit = q.limit.unwrap_or(100) as i64;
    let offset = q.offset.unwrap_or(0) as i64;
    match state.store.list_request_logs_page(limit, offset, q.request_id.as_deref(), q.group_id.as_deref()) {
        Ok(page) => Json(page.into_json()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

#[derive(Deserialize)]
pub struct StatsQuery {
    /// Scopes ALL THREE blocks to one routing group when set. Omitted/empty is
    /// the global overview: the group picker needs by_group to list every group
    /// before the user has picked one, and it is exactly this empty query that
    /// draws it. A non-empty value narrows summary, by_model AND by_group to
    /// that one group.
    pub group_id: Option<String>,
}

pub async fn stats(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<StatsQuery>) -> Response {
    if let Err(r) = authorize(&state, &headers) { return r }
    let scope = q.group_id.as_deref().filter(|g| !g.is_empty());
    let summary = state.store.stats_summary(scope).unwrap_or_else(|_| json!({}));
    let by_model = state.store.stats_by_model(scope).unwrap_or_else(|_| vec![]);
    let by_group = state.store.stats_by_group(scope).unwrap_or_else(|_| vec![]);
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

/// POST /admin/api/channel-models — read the model list the provider itself
/// publishes, so the console can offer "import the provider's models" next to
/// "type one by hand" (board task-32; user m03395).
///
/// The caller names either a SAVED channel (channel_id) or hands over a draft
/// that is not saved yet (base_url / api_key / protocol / headers). Draft
/// fields win over the saved channel, and request headers MERGE over the
/// channel's own headers with the same name.
///
/// The answer is ALWAYS HTTP 200: whether the provider answered is a business
/// fact, reported in "ok", never a transport status. "status" is the upstream
/// HTTP status (0 when nothing was reached) and "models" is a de-duplicated id
/// list in the provider's own order.
#[derive(Deserialize)]
pub struct ChannelModelsReq {
    pub channel_id: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub protocol: Option<crate::config::Protocol>,
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
}

/// A provider that never answers must not hang the console dialog.
const MODELS_TIMEOUT_SECS: u64 = 20;
/// Guard against a provider that offers an endless catalogue.
const MODELS_MAX: usize = 5000;
/// How much of an upstream body is quoted back in an error.
const MODELS_SNIPPET: usize = 200;

/// Tolerant header insert, same rule as the data plane: an unusable name or
/// value is skipped instead of failing the request. Inserting over an existing
/// name REPLACES it, which is what gives the draft its override semantics.
fn set_wire_header(map: &mut reqwest::header::HeaderMap, name: &str, value: &str) {
    if let (Ok(n), Ok(v)) = (name.trim().parse::<reqwest::header::HeaderName>(), value.parse::<reqwest::header::HeaderValue>()) {
        map.insert(n, v);
    }
}

fn models_fail(status: u16, latency_ms: u64, error: String) -> Response {
    Json(json!({
        "ok": false, "status": status, "latency_ms": latency_ms,
        "models": [], "count": 0, "error": error,
    })).into_response()
}

fn push_model_id(out: &mut Vec<String>, raw: &str) {
    let id = raw.trim();
    if !id.is_empty() && !out.iter().any(|x| x == id) { out.push(id.to_string()) }
}

/// Pull the ids out of whichever shape the provider uses. The first shape that
/// yields any id wins.
///
/// The distinction this returns is about SHAPE, not about count: Some(empty)
/// means the body WAS a model list that happens to carry no models, which is a
/// legitimate answer ("this provider offers nothing right now") and must not be
/// dressed up as a parse error. None means the body was no model list at all
/// (HTML, {}, a bare string, an empty body), which the caller reports as an
/// error rather than as "this channel has no models".
fn parse_model_ids(text: &str) -> Option<Vec<String>> {
    let v: Value = serde_json::from_str(text).ok()?;
    let mut out: Vec<String> = Vec::new();
    // Whether the body was recognisable as a model list, even an empty one.
    let mut recognized = false;
    // (1) OpenAI and friends: {"data": [{"id": "gpt-4o"}, ...]}
    if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
        recognized = true;
        for row in arr {
            if let Some(id) = row.get("id").and_then(|i| i.as_str()) { push_model_id(&mut out, id) }
        }
    }
    // (2) Gemini / Anthropic style: {"models": [{"name": "models/gemini-1.5"}]}
    if out.is_empty() {
        if let Some(arr) = v.get("models").and_then(|d| d.as_array()) {
            recognized = true;
            for row in arr {
                if let Some(id) = row.get("id").and_then(|i| i.as_str()) { push_model_id(&mut out, id) }
                else if let Some(name) = row.get("name").and_then(|i| i.as_str()) {
                    // "models/" is the provider's routing syntax, never part of a model name.
                    push_model_id(&mut out, name.strip_prefix("models/").unwrap_or(name))
                }
            }
        }
    }
    // (3) a bare array: [{"id": "x"}] or ["x", "y"]
    if out.is_empty() {
        if let Some(arr) = v.as_array() {
            recognized = true;
            for row in arr {
                if let Some(id) = row.get("id").and_then(|i| i.as_str()) { push_model_id(&mut out, id) }
                else if let Some(name) = row.as_str() { push_model_id(&mut out, name) }
            }
        }
    }
    if recognized { Some(out) } else { None }
}

pub async fn channel_models(State(state): State<AppState>, headers: HeaderMap, Json(req): Json<ChannelModelsReq>) -> Response {
    if let Err(r) = authorize(&state, &headers) { return r }
    let Some(snap) = state.config.get() else { return (StatusCode::SERVICE_UNAVAILABLE, "no config").into_response() };

    // A saved channel supplies the defaults; every draft field replaces one.
    let saved = req.channel_id.as_deref().and_then(|id| snap.config.find_channel(id));
    let draft_base = req.base_url.as_deref().map(|b| b.trim()).filter(|b| !b.is_empty());
    if saved.is_none() {
        if let Some(id) = req.channel_id.as_deref() {
            if draft_base.is_none() {
                return (StatusCode::NOT_FOUND, Json(json!({ "error": format!("channel {id} not found") }))).into_response();
            }
        }
    }
    let base_url = match draft_base {
        Some(b) => b.to_string(),
        None => match saved {
            Some(c) => c.base_url.trim().to_string(),
            None => return (StatusCode::BAD_REQUEST, Json(json!({ "error": "base_url required" }))).into_response(),
        },
    };
    if base_url.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "base_url required" }))).into_response();
    }
    let protocol = match req.protocol {
        Some(p) => p,
        None => saved.map(|c| c.protocol).unwrap_or(crate::config::Protocol::Openai),
    };
    let api_key = match req.api_key.as_deref().filter(|k| !k.is_empty()) {
        Some(k) => k.to_string(),
        None => saved.map(|c| c.api_key.clone()).unwrap_or_default(),
    };

    let base = base_url.trim_end_matches('/');
    let url = match protocol {
        crate::config::Protocol::Openai => format!("{base}/v1/models"),
        crate::config::Protocol::Anthropic => format!("{base}/v1/models"),
        crate::config::Protocol::Gemini => format!("{base}/v1beta/models"),
    };

    // Protocol defaults first, then the channel's own headers, then the ones
    // typed into the dialog: last writer wins, exactly like the data plane.
    let mut wire = reqwest::header::HeaderMap::new();
    match protocol {
        crate::config::Protocol::Openai => set_wire_header(&mut wire, "authorization", &format!("Bearer {api_key}")),
        crate::config::Protocol::Anthropic => {
            set_wire_header(&mut wire, "x-api-key", &api_key);
            set_wire_header(&mut wire, "anthropic-version", "2023-06-01");
        }
        crate::config::Protocol::Gemini => set_wire_header(&mut wire, "x-goog-api-key", &api_key),
    }
    if let Some(c) = saved {
        for (k, v) in &c.headers { set_wire_header(&mut wire, k, v) }
    }
    for (k, v) in &req.headers { set_wire_header(&mut wire, k, v) }

    let started = Instant::now();
    let http = state.http.client_for(snap.config.retry.connect_timeout_ms)
        .request(reqwest::Method::GET, &url)
        .headers(wire);

    // ONE window covers BOTH halves of the exchange: the response headers AND
    // the body. An upstream that sends its header line and then stalls the body
    // would otherwise hold the console dialog open forever — the very stalled
    // -body case the data plane guards against with its chunk idle timeout.
    let exchange = async move {
        let r = http.send().await?;
        let status = r.status().as_u16();
        let text = r.text().await.unwrap_or_default();
        Ok::<(u16, String), reqwest::Error>((status, text))
    };
    match tokio::time::timeout(std::time::Duration::from_secs(MODELS_TIMEOUT_SECS), exchange).await {
        Err(_) => models_fail(0, started.elapsed().as_millis() as u64, format!("timeout after {MODELS_TIMEOUT_SECS}s")),
        Ok(Err(e)) => models_fail(0, started.elapsed().as_millis() as u64, format!("connect: {e}")),
        Ok(Ok((status, text))) => {
            let latency_ms = started.elapsed().as_millis() as u64;
            if !(200..300).contains(&status) {
                let snippet: String = text.chars().take(MODELS_SNIPPET).collect();
                return models_fail(status, latency_ms, format!("HTTP {status}: {snippet}"));
            }
            match parse_model_ids(&text) {
                Some(mut ids) => {
                    let truncated = ids.len() > MODELS_MAX;
                    ids.truncate(MODELS_MAX);
                    let count = ids.len();
                    Json(json!({
                        "ok": true, "status": status, "latency_ms": latency_ms,
                        "models": ids, "count": count, "truncated": truncated,
                    })).into_response()
                }
                None => {
                    let snippet: String = text.chars().take(MODELS_SNIPPET).collect();
                    models_fail(status, latency_ms, format!("unrecognized models payload: {snippet}"))
                }
            }
        }
    }
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
    // long-running stream does not change it until it lands. The console must
    // NOT render "active" as "使用中": the live badge comes from the
    // `inflight` counter below, which really is "is this account busy now".
    let active_key: Option<String> = state.store.active_member_key(&group.id).unwrap_or(None);
    let stats = state.store.group_member_stats(&group.id).unwrap_or_default();
    let cooling_rows = state.health.snapshot_summary();

    let mut tiers: Vec<&crate::config::Tier> = group.tiers.iter().collect();
    tiers.sort_by_key(|t| t.level);

    let mut members: Vec<Value> = vec![];
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Sum of the per-member counters BELOW, accumulated as the rows are built.
    // It must be this group's own in-flight load, not the gateway's: a slow
    // request that belongs to another group must never inflate this group's
    // "当前并发" badge (the header label says 本分组, and the rows it sums are
    // this group's members).
    let mut inflight_total: i64 = 0;
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
            // How many requests are using this account RIGHT NOW. 0 is the
            // normal state; anything above 0 is what "使用中" means.
            let inflight_n = state.inflight.member_count(&key) as i64;
            inflight_total += inflight_n;

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
                // Thinking-loop discards for THIS member. Separate from
                // loop_retries above on purpose: that one counts answer loops,
                // and a model that merely circled in its own deliberation is a
                // different problem with a different fix.
                "reasoning_truncated": int("reasoning_truncated"),
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
                "inflight": inflight_n,
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
        // Sum over the members above, so the header can say "本分组正在并发 N"
        // without re-adding the rows. Group-scoped on purpose — see the
        // accumulator's comment.
        "inflight_total": inflight_total,
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

/// Live concurrency for every upstream account, for the console's "使用中"
/// badges on the Channels and Groups pages.
///
/// This is the ONLY honest source for that badge. Two cheaper substitutes were
/// rejected on purpose:
///   * `group_members.active` / `active_key` — the last SUCCESSFUL landing
///     spot, which stays lit after the request is long gone;
///   * the tier-1 reference highlight — a config declaration with no traffic
///     meaning at all.
/// A counter that only exists while an attempt really occupies an account
/// cannot lie in either direction: it is 0 when idle and N when N requests are
/// in flight.
pub async fn inflight(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = authorize(&state, &headers) { return r }
    // Config is optional here: the members/channels/total aggregations need no
    // routing knowledge, and a gateway that has not published a config yet
    // should still answer with real zeros instead of a 503.
    let snap = state.config.get();
    let cfg = snap.as_ref().map(|s| s.config.as_ref());
    Json(state.inflight.snapshot_json(cfg)).into_response()
}

#[derive(Deserialize)]
pub struct PurgeLogsReq {
    #[serde(default)]
    pub request_logs: bool,
    #[serde(default)]
    pub switch_logs: bool,
    #[serde(default)]
    pub cooldowns: bool,
}

/// "清空日志与统计": drop the traffic history the console renders.
///
/// Deliberately NOT a config reset. The channels, models, groups and API keys
/// survive, because the point is to clear the self-test noise that made a
/// never-used account look busy — not to reconfigure the gateway.
///
/// `cooldowns` clears BOTH halves of the cooling state: the persisted table and
/// the in-memory registry. Clearing only the table would leave the console
/// showing a channel as cooling until the next restart.
pub async fn purge_logs(State(state): State<AppState>, headers: HeaderMap, Json(req): Json<PurgeLogsReq>) -> Response {
    if let Err(r) = authorize(&state, &headers) { return r }
    match state.store.purge(req.request_logs, req.switch_logs, req.cooldowns) {
        Ok((n_req, n_sw, n_cd)) => {
            if req.cooldowns {
                state.health.clear_all();
                state.health.persist();
            }
            Json(json!({
                "ok": true,
                "deleted": {
                    "request_logs": n_req,
                    "switch_logs": n_sw,
                    "cooldowns": n_cd,
                },
            })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
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
        .route("/admin/api/inflight", get(inflight))
        .route("/admin/api/purge-logs", post(purge_logs))
        .route("/admin/api/health", get(health_summary))
        .route("/admin/api/test-channel", post(test_channel))
        .route("/admin/api/channel-models", post(channel_models))
        .route("/admin/api/cooldown/clear", post(clear_cooldown))
        .with_state(state).layer(cors)
}

use std::time::Instant;
