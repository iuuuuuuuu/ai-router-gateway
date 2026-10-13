//! Wildcard routing + group-members integration tests (Task E of the
//! degenerate-output feature, board task-27).
//!
//! What these tests pin down, in the user's terms: a client may name the GROUP
//! itself (its name or its id) instead of one model, and the gateway then routes
//! across every enabled member of every tier — the same walk, the same cooldown
//! and the same loop guard as the narrow path, plus a console endpoint that
//! shows the members of one group member by member.
//!
//! Coverage map (task-27 deliverable 3):
//!   w01  EXACT WINS: an exposed model name is never a wildcard, even when the
//!        group is NAMED like one of its own models (narrow plan byte-identical)
//!   w02  wildcard plan = every tier, ascending by level, declared order inside a
//!        tier, skipping disabled bindings / disabled channels, (channel,
//!        upstream) deduped, cooling members reported instead of dropped
//!   w03  real HTTP: a looping tier-1 member is intercepted, the client gets ONE
//!        clean answer from the next candidate, and the wildcard names are
//!        discoverable through /v1/models while an unknown name is still 404
//!   w04  /admin/api/group-members: the frozen payload shape (every member listed
//!        incl. dangling + disabled ones, real 0 counters, null averages), admin
//!        key enforcement, 404 for an unknown group
//!   w05  the SQL caliber behind it: avg_* stay SQL NULL without a success,
//!        counters are real 0s, degenerate output lands in loop_retries
//!
//! The degenerate fixture always emits the same 4-character unit, so a leak can
//! be detected with one substring test instead of a heuristic.

mod support;

use gw_server::config::{ApiKey, Channel, Config, Group, GroupOverrides, ModelBinding, Protocol, Tier, TierMember};
use gw_server::health::{Attribution, CooldownKey, HealthRegistry};
use gw_server::proxy::AppState;
use gw_server::router::{build_plan, is_wildcard};
use gw_server::store::{RequestLog, Store, SwitchLog};
use std::time::Duration;
use support::*;
use tower::util::ServiceExt;

/// The repeated unit the degenerate fixture emits (Behavior::DegenerateShortCycle).
const UNIT: &str = "好问题。";
/// What Behavior::GoodStream answers with (frames "he" + "llo").
const GOOD: &str = "hello";

const GROUP_NAME: &str = "team-a";
const GROUP_ID: &str = "gA";
const OTHER_EXPOSED: &str = "second-model";

/// Five upstreams. Channel 1 binds a DISABLED model, channel 3 is a DISABLED
/// channel, channels 0 and 2 and 4 are plain enabled members spread over three
/// tiers — so "which candidates exist" is decidable from the plan alone.
const BEHAVIORS: [Behavior; 5] = [
    Behavior::GoodJson,
    Behavior::Fail500,
    Behavior::GoodJson,
    Behavior::Fail500,
    Behavior::GoodJson,
];

/// Channel 0 loops, channel 2 answers: the wildcard walk must notice and switch.
const DEGEN_BEHAVIORS: [Behavior; 5] = [
    Behavior::DegenerateShortCycle,
    Behavior::Fail500,
    Behavior::GoodStream,
    Behavior::Fail500,
    Behavior::Fail500,
];

fn tm(i: usize) -> TierMember { TierMember { model_id: format!("m{i}"), weight: 1 } }

/// Group `team-a`/`gA` with three tiers and two exposed models.
/// * channels 0,1,2 bind `my-model`; channel 3 binds `second-model` (it is also
///   the disabled channel);
/// * model m1 is disabled (its channel stays up), channel ch3 is disabled;
/// * tier 3 holds ch4, which binds `my-model` as well.
/// A narrow request for `my-model` therefore has exactly ONE usable candidate
/// (ch0) while the wildcard plan has three (ch0, ch2, ch4).
fn wildcard_config(base: &str, prefixes: &[String]) -> Config {
    assert!(prefixes.len() >= 5, "the fixture needs five upstreams");
    let mut channels = vec![];
    let mut models = vec![];
    for (i, p) in prefixes.iter().enumerate() {
        channels.push(Channel {
            id: format!("ch{i}"),
            name: format!("channel-{i}"),
            base_url: format!("{base}{p}"),
            protocol: Protocol::Openai,
            api_key: format!("sk-fake-{i}"),
            enabled: i != 3,
            weight: 1,
            headers: Default::default(),
        });
        models.push(ModelBinding {
            id: format!("m{i}"),
            channel_id: format!("ch{i}"),
            exposed: if i < 3 { EXPOSED.to_string() } else { OTHER_EXPOSED.to_string() },
            upstream: format!("up-model-{i}"),
            price_per_m: [1.0, 2.0, 0.0, 0.0],
            enabled: i != 1,
        });
    }
    Config {
        auto_weight: false,
        retry: test_retry(),
        channels,
        models,
        groups: vec![Group {
            id: GROUP_ID.into(),
            name: GROUP_NAME.into(),
            exposed_models: vec![EXPOSED.to_string(), OTHER_EXPOSED.to_string()],
            tiers: vec![
                Tier { level: 1, members: vec![tm(0), tm(1)] },
                Tier { level: 2, members: vec![tm(2), tm(3)] },
                Tier { level: 3, members: vec![tm(4)] },
            ],
            // No per-group overrides: this group follows the global retry config.
            overrides: GroupOverrides::default(),
        }],
        api_keys: vec![ApiKey { key: "gw-test-key".into(), group_id: GROUP_ID.into(), name: "tester".into(), enabled: true }],
        ..Default::default()
    }
}

fn fresh_health() -> HealthRegistry { HealthRegistry::new(None) }

/// Every text delta the client actually received, concatenated (OpenAI entry).
fn client_text(sse: &str) -> String {
    let mut out = String::new();
    for line in sse.lines() {
        let Some(payload) = line.strip_prefix("data: ") else { continue };
        if payload == "[DONE]" { continue }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else { continue };
        if let Some(t) = v["choices"][0]["delta"]["content"].as_str() { out.push_str(t); }
    }
    out
}

fn channel_ids(p: &gw_server::router::Plan) -> Vec<String> {
    p.runnable.iter().map(|c| c.channel_id.clone()).collect()
}

fn member(payload: &serde_json::Value, key: &str) -> serde_json::Value {
    payload["members"]
        .as_array()
        .and_then(|a| a.iter().find(|m| m["key"] == key))
        .cloned()
        .unwrap_or_else(|| panic!("no member {key} in {payload}"))
}

// ---- 1. EXACT WINS ----

#[tokio::test]
async fn w01_exact_exposed_name_wins_over_the_group_name() {
    let (base, prefixes) = spawn_fake_upstreams(&BEHAVIORS).await;
    let cfg = wildcard_config(&base, &prefixes);
    let group = &cfg.groups[0];

    assert!(!is_wildcard(group, EXPOSED), "an exposed model must never be treated as a wildcard");
    assert!(!is_wildcard(group, OTHER_EXPOSED));
    assert!(!is_wildcard(group, "no-such-model"), "an unknown name is not a wildcard either");
    assert!(is_wildcard(group, GROUP_NAME), "the group name routes as a wildcard");
    assert!(is_wildcard(group, GROUP_ID), "the group id routes as a wildcard");

    // The narrow plan follows the BINDING, not the tier: ch0 and ch2 both bind
    // this exposed name, ch1 binds it too but is disabled at the model level.
    let narrow = build_plan(&cfg, group, EXPOSED, &fresh_health());
    assert_eq!(channel_ids(&narrow), vec!["ch0", "ch2"], "the narrow plan must follow the binding, not the tier");

    // EXACT WINS: rename the group to an exposed model of its own. That name must
    // then resolve to the exposed model (the pre-task-27 semantics), NOT to the
    // wildcard — and the resulting plan is byte-for-byte the one above.
    let mut clash = cfg.clone();
    clash.groups[0].name = EXPOSED.to_string();
    let g2 = &clash.groups[0];
    assert!(!is_wildcard(g2, EXPOSED), "a name/model clash must resolve to the exact exposed model");
    let after = build_plan(&clash, g2, EXPOSED, &fresh_health());
    assert_eq!(channel_ids(&after), channel_ids(&narrow), "the clash changed the narrow plan");
    // ...and the group id is still a wildcard in that same config.
    assert!(is_wildcard(g2, GROUP_ID));
    let wide = build_plan(&clash, g2, GROUP_ID, &fresh_health());
    assert_eq!(channel_ids(&wide), vec!["ch0", "ch2", "ch4"]);
}

// ---- 2. the wildcard plan: every tier, declared order, skips, dedupe ----

#[tokio::test]
async fn w02_wildcard_plan_covers_every_tier_and_skips_disabled_members() {
    let (base, prefixes) = spawn_fake_upstreams(&BEHAVIORS).await;
    let cfg = wildcard_config(&base, &prefixes);
    let health = fresh_health();

    let plan = build_plan(&cfg, &cfg.groups[0], GROUP_NAME, &health);
    let got: Vec<String> = plan.runnable.iter().map(|c| format!("{}@{}", c.channel_id, c.tier)).collect();
    assert_eq!(got, vec!["ch0@1", "ch2@2", "ch4@3"], "tier ascending, declared order inside a tier, disabled members skipped");
    assert!(plan.cooling.is_empty(), "{plan:?}");

    // m1 is disabled although its CHANNEL is enabled; ch3 is a disabled CHANNEL
    // although its binding is enabled — neither may appear above.
    assert!(!got.iter().any(|c| c.starts_with("ch1")), "a disabled binding must be skipped");
    assert!(!got.iter().any(|c| c.starts_with("ch3")), "a disabled channel must be skipped");

    // Tier levels are visited ascending even when declared out of order.
    let mut shuffled = cfg.clone();
    shuffled.groups[0].tiers.reverse();
    let p2 = build_plan(&shuffled, &shuffled.groups[0], GROUP_NAME, &health);
    assert_eq!(p2.runnable.iter().map(|c| c.tier).collect::<Vec<u32>>(), vec![1, 2, 3]);

    // I3: the same (channel, upstream) pair is never tried twice in one request,
    // even when it is declared in two tiers.
    let mut dup = cfg.clone();
    dup.groups[0].tiers[2].members.push(tm(0));
    let p3 = build_plan(&dup, &dup.groups[0], GROUP_NAME, &health);
    assert_eq!(channel_ids(&p3), vec!["ch0", "ch2", "ch4"], "a duplicated pair must be deduped, keeping the first slot");

    // I5: a cooling member leaves the runnable set but is REPORTED, so the walk
    // can tell "no route at all" (404) from "temporarily blocked" (wait/503).
    // Twice on purpose: the FIRST failure of a key arms no cooldown at all
    // (health.rs: FAILURES_BEFORE_COOLDOWN = 2), it only counts the failure.
    for _ in 0..2 {
        health.apply(Attribution::Cooldown(
            CooldownKey::scoped("ch2", "up-model-2"),
            Duration::from_secs(15),
            "backoff",
        ));
    }
    let p4 = build_plan(&cfg, &cfg.groups[0], GROUP_NAME, &health);
    assert_eq!(channel_ids(&p4), vec!["ch0", "ch4"], "a cooling member must not be attempted");
    assert!(p4.cooling.contains(&CooldownKey::scoped("ch2", "up-model-2")), "{:?}", p4.cooling);
}

// ---- 3. end to end over real HTTP: wildcard + loop guard + /v1/models ----

#[tokio::test]
async fn w03_wildcard_intercepts_a_looping_member_and_answers_cleanly() {
    let (base, prefixes) = spawn_fake_upstreams(&DEGEN_BEHAVIORS).await;
    let cfg = wildcard_config(&base, &prefixes);
    let (state, router) = gateway(cfg).await;

    // The request names the GROUP, not a model.
    let (status, sse) = post_chat_stream(&router, GROUP_NAME).await;
    assert_eq!(status, 200, "a wildcard request must be answered normally: {sse}");
    // I1: not one byte of the discarded attempt reached the client.
    assert!(!sse.contains(UNIT), "degenerate text reached the client: {sse}");
    assert_eq!(client_text(&sse), GOOD, "the client must see exactly the good answer: {sse}");
    assert_stream_clean(&sse);

    // The walk really switched: ch0 produced the degenerate output, ch2 committed.
    let rows = wait_switch_rows(&state, 2).await;
    let deg = rows.iter().find(|r| r["error_class"] == "degenerate_output").expect("the degenerate attempt must be logged");
    assert_eq!(deg["channel_id"], "ch0", "{deg}");
    assert_eq!(deg["outcome"], "failed", "{deg}");
    let commit = rows.iter().find(|r| r["outcome"] == "committed").expect("a committed attempt must be logged");
    assert_eq!(commit["channel_id"], "ch2", "{commit}");

    // The console agrees: counters per member, and exactly ONE active member.
    let payload = wait_group_members(&state).await;
    assert_eq!(payload["group_id"], GROUP_ID);
    assert_eq!(payload["group_name"], GROUP_NAME);
    assert_eq!(payload["active_key"].as_str(), Some("ch2::up-model-2"), "{payload}");
    let answered = member(&payload, "ch2::up-model-2");
    assert_eq!(answered["requests"].as_i64(), Some(1), "{answered}");
    assert_eq!(answered["active"].as_bool(), Some(true), "{answered}");
    assert_eq!(answered["last_outcome"].as_str(), Some("success"), "{answered}");
    let looped = member(&payload, "ch0::up-model-0");
    assert_eq!(looped["loop_retries"].as_i64(), Some(1), "{looped}");
    assert_eq!(looped["failed_attempts"].as_i64(), Some(1), "{looped}");
    assert_eq!(looped["active"].as_bool(), Some(false), "{looped}");
    let active_count = payload["members"].as_array().unwrap().iter().filter(|m| m["active"] == true).count();
    assert_eq!(active_count, 1, "at most one member may be active: {payload}");

    // The wildcard names are discoverable ("what can I call?"), de-duplicated.
    let list = models_list(&router).await;
    let ids: Vec<&str> = list["data"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec![EXPOSED, OTHER_EXPOSED, GROUP_NAME, GROUP_ID], "{list}");

    // Non-negotiable legacy behaviour: an unknown name is still a 404.
    let (bad, body) = post_chat(&router, "no-such-model", false).await;
    assert_eq!(bad, 404, "{body}");
    assert_eq!(body["error"]["code"], "model_not_found", "{body}");
}

// ---- 4. the console endpoint: shape, auth, unknown group ----

#[tokio::test]
async fn w04_group_members_payload_shape_and_access_control() {
    let (base, prefixes) = spawn_fake_upstreams(&BEHAVIORS).await;
    let mut cfg = wildcard_config(&base, &prefixes);
    // A member whose model binding vanished: the panel must still render the row
    // (otherwise a broken config would look like a smaller group).
    cfg.groups[0].tiers[0].members.push(TierMember { model_id: "ghost".into(), weight: 1 });
    // A channel with an EMPTY name: channel_name falls back to the id.
    cfg.channels[2].name = String::new();
    let (state, _router) = gateway(cfg).await;

    let payload = admin_group_members(&state).await;
    assert_eq!(payload["group_id"], "gA");
    assert_eq!(payload["group_name"], GROUP_NAME);
    assert!(payload["active_key"].is_null(), "a group that never had a success has no active member: {payload}");
    assert_eq!(payload["members"].as_array().unwrap().len(), 6, "every declared member is listed, the dangling one included: {payload}");

    // The dangling member: no ids, not usable, but still shaped like a member.
    let ghost = member(&payload, "::");
    assert_eq!(ghost["model_id"].as_str(), Some("ghost"), "{ghost}");
    assert_eq!(ghost["enabled"].as_bool(), Some(false), "{ghost}");
    assert_eq!(ghost["cooling"].as_bool(), Some(false), "{ghost}");
    assert!(ghost["cooling_until_ms"].is_null(), "no cooldown -> null, never 0: {ghost}");
    assert_eq!(ghost["requests"].as_i64(), Some(0), "counters are real 0s, never null: {ghost}");
    assert_eq!(ghost["loop_retries"].as_i64(), Some(0), "{ghost}");
    assert_eq!(ghost["cost_usd"].as_f64(), Some(0.0), "{ghost}");
    assert!(ghost["avg_duration_ms"].is_null(), "no data is null, not 0ms: {ghost}");
    assert!(ghost["avg_ttft_ms"].is_null(), "{ghost}");
    assert_eq!(ghost["error_classes"].as_array().map(|a| a.len()), Some(0), "{ghost}");

    // enabled = binding.enabled AND channel.enabled
    assert_eq!(member(&payload, "ch0::up-model-0")["enabled"].as_bool(), Some(true));
    assert_eq!(member(&payload, "ch1::up-model-1")["enabled"].as_bool(), Some(false), "a disabled binding is not usable");
    assert_eq!(member(&payload, "ch3::up-model-3")["enabled"].as_bool(), Some(false), "a disabled channel is not usable");
    let m2 = member(&payload, "ch2::up-model-2");
    assert_eq!(m2["channel_name"].as_str(), Some("ch2"), "an empty channel name falls back to the id: {m2}");
    assert_eq!(m2["exposed_model"].as_str(), Some(EXPOSED), "{m2}");
    // slot = position inside its own tier, tier = the level
    assert_eq!(m2["tier"].as_i64(), Some(2), "{m2}");
    assert_eq!(m2["slot"].as_i64(), Some(0), "{m2}");
    assert_eq!(m2["weight"].as_i64(), Some(1), "{m2}");

    // Unknown group: 404, never an empty payload that reads like "no members".
    assert_eq!(raw_admin_get(&state, "/admin/api/group-members?group_id=nope", None).await, 404);

    // With admin.key set the endpoint is closed without the key and open with it.
    let mut locked = state.config.get().unwrap().config.as_ref().clone();
    locked.admin.key = Some("admin-secret".into());
    state.config.publish(locked);
    assert_eq!(raw_admin_get(&state, "/admin/api/group-members?group_id=gA", None).await, 401);
    assert_eq!(raw_admin_get(&state, "/admin/api/group-members?group_id=gA", Some("admin-secret")).await, 200);
}

// ---- 5. the SQL caliber under the endpoint ----

fn ts_for(seq: usize) -> String { format!("2026-10-13T10:00:{seq:02}.000+08:00") }

fn insert_request(store: &Store, group: &str, status: &str, fin: Option<(&str, &str)>, seq: usize, duration_ms: i64, ttft_ms: i64) {
    store
        .insert_request_log(&RequestLog {
            id: None,
            ts: ts_for(seq),
            request_id: format!("req-{seq}"),
            group_id: group.into(),
            exposed_model: EXPOSED.into(),
            entry_protocol: "openai".into(),
            status: status.into(),
            final_channel: fin.map(|f| f.0.to_string()),
            final_upstream_model: fin.map(|f| f.1.to_string()),
            attempts: 1,
            loop_retries: 0,
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            cost_usd: 0.001,
            wasted_cost_usd: 0.0,
            duration_ms,
            ttft_ms,
        })
        .unwrap();
}

fn insert_switch(store: &Store, group: &str, channel: &str, upstream: &str, outcome: &str, class: &str, seq: usize) {
    store
        .insert_switch_log(&SwitchLog {
            ts: ts_for(seq),
            request_id: format!("req-{seq}"),
            group_id: group.into(),
            attempt_no: 1,
            tier: 1,
            channel_id: channel.into(),
            channel_name: format!("{channel}-name"),
            upstream_model: upstream.into(),
            outcome: outcome.into(),
            error_class: class.into(),
            http_status: None,
            latency_ms: 12,
            message: String::new(),
        })
        .unwrap();
}

#[tokio::test]
async fn w05_group_member_stats_sql_caliber() {
    let store = Store::open_memory().unwrap();

    // No rows at all: the member is simply absent from the map (the caller
    // renders a real 0 / null), and there is no active member yet.
    assert!(store.group_member_stats("gA").unwrap().is_empty());
    assert_eq!(store.active_member_key("gA").unwrap(), None);

    // Two landed successes on one member, a terminal failure with no final member,
    // then two failed attempts on another member (one of them degenerate).
    insert_request(&store, "gA", "success", Some(("chA", "upA")), 1, 100, 30);
    insert_request(&store, "gA", "success", Some(("chA", "upA")), 2, 300, 50);
    insert_request(&store, "gA", "exhausted", None, 3, 5_000, 0);
    insert_switch(&store, "gA", "chB", "upB", "failed", "http_5xx", 4);
    insert_switch(&store, "gA", "chB", "upB", "failed", "degenerate_output", 5);
    // Another group must never leak into this payload: its member key is
    // deliberately one this group has never used.
    insert_request(&store, "gB", "success", Some(("chZ", "upZ")), 6, 9_999, 9_999);

    let m = store.group_member_stats("gA").unwrap();
    let a = &m["chA::upA"];
    assert_eq!(a["requests"].as_i64(), Some(2), "{a}");
    // SQLite ROUND() yields a REAL, so these arrive as JSON floats (the console
    // contract only promises `number | null`).
    assert_eq!(a["avg_duration_ms"].as_f64(), Some(200.0), "{a}");
    assert_eq!(a["avg_ttft_ms"].as_f64(), Some(40.0), "{a}");
    assert_eq!(a["input_tokens"].as_i64(), Some(20), "{a}");
    assert_eq!(a["last_outcome"].as_str(), Some("success"), "{a}");
    assert!(a["last_error_class"].is_null(), "a member whose last request succeeded has no error class: {a}");

    let b = &m["chB::upB"];
    assert_eq!(b["failed_attempts"].as_i64(), Some(2), "{b}");
    assert_eq!(b["loop_retries"].as_i64(), Some(1), "only degenerate_output counts as a loop retry: {b}");
    assert_eq!(b["requests"].as_i64(), Some(0), "{b}");
    assert!(b["avg_duration_ms"].is_null(), "no success -> SQL NULL, never a bogus 0ms: {b}");
    assert!(b["avg_ttft_ms"].is_null(), "{b}");
    assert_eq!(b["last_outcome"].as_str(), Some("failed"), "{b}");
    assert_eq!(b["last_error_class"].as_str(), Some("degenerate_output"), "the NEWEST failure owns the class: {b}");
    let classes = b["error_classes"].as_array().cloned().unwrap_or_default();
    let names: Vec<&str> = classes.iter().map(|c| c["error_class"].as_str().unwrap()).collect();
    assert!(names.contains(&"degenerate_output") && names.contains(&"http_5xx"), "{b}");

    assert!(!m.contains_key("chZ::upZ"), "another group leaked into the payload: {m:?}");
    assert_eq!(store.active_member_key("gA").unwrap().as_deref(), Some("chA::upA"));
    assert_eq!(store.active_member_key("gZ").unwrap(), None);
}

// ---- drivers ----

/// Newest-first switch_logs rows, waiting until at least `n` exist.
async fn wait_switch_rows(st: &AppState, n: usize) -> Vec<serde_json::Value> {
    for _ in 0..300 {
        let rows = st.store.list_switch_logs(50, None, None).unwrap();
        if rows.len() >= n { return rows }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("only {} switch_logs rows appeared within 6s (wanted {n})", st.store.list_switch_logs(50, None, None).unwrap().len());
}

/// The admin endpoint, waiting for the detached stream-log task to land (the
/// active member is written by it, so the first poll can legitimately be empty).
async fn wait_group_members(st: &AppState) -> serde_json::Value {
    let mut last = serde_json::Value::Null;
    for _ in 0..300 {
        last = admin_group_members(st).await;
        if !last["active_key"].is_null() { return last }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no active_key appeared within 6s: {last}");
}

async fn admin_group_members(st: &AppState) -> serde_json::Value {
    let admin = gw_server::admin::admin_router(st.clone());
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/admin/api/group-members?group_id=gA")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = admin.oneshot(req).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200, "group-members must answer 200");
    body_json(resp).await
}

async fn raw_admin_get(st: &AppState, uri: &str, key: Option<&str>) -> u16 {
    let admin = gw_server::admin::admin_router(st.clone());
    let mut b = axum::http::Request::builder().method("GET").uri(uri);
    if let Some(k) = key { b = b.header("x-admin-key", k); }
    let req = b.body(axum::body::Body::empty()).unwrap();
    admin.oneshot(req).await.unwrap().status().as_u16()
}

async fn models_list(router: &axum::Router) -> serde_json::Value {
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/v1/models")
        .header("authorization", "Bearer gw-test-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    body_json(resp).await
}

