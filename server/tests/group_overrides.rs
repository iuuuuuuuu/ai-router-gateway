//! Task I: per-group retry overrides ("每个分组还可以跟随全局配置，也可以有自己的配置").
//!
//! The rule under test, in the user's terms: a group that says nothing runs on
//! the GLOBAL config; a group that declares \`overrides.retry\` runs on its own
//! copy instead — and only that group changes. These tests pin both halves:
//! pointer-level identity for the inherit path, and an observable behaviour
//! difference (degenerate text passes in ONE group while ANOTHER group with the
//! same global config is still intercepted) for the override path.
//!
//! Coverage map (task-28 delivery 4):
//!   g01 no override              -> effective_retry is the SAME object as cfg.retry
//!   g02 override present         -> the group's own copy is in force, field-distinguishable
//!   g03 \`retry: null\` survives   -> serialization keeps the key (never skipped)
//!   g04 legacy JSON              -> no \`overrides\` key deserializes to retry == None
//!   g05 PUT violating I4         -> 400 naming the group, stored config untouched
//!   g06 behaviour: guard off in one group, other group still intercepted
//!   g07 behaviour: no override   -> identical numbers to the global config
//!   g08 behaviour: per-group max_attempts bounds the walk
//!   g09 \`GET /admin/api/group-members\` reports retry / retry_source / global_retry

mod support;

use gw_server::config::{
    effective_retry, ApiKey, Channel, Config, Group, GroupOverrides, ModelBinding, Protocol,
    RetryConfig, Tier, TierMember,
};
use gw_server::proxy::AppState;
use support::*;
use tower::util::ServiceExt;

/// The repeated unit every degenerate fixture emits (Behavior::DegenerateShortCycle).
const UNIT: &str = "好问题。";
/// What Behavior::GoodStream answers with.
const GOOD: &str = "hello";

fn retry_with(f: impl FnOnce(&mut RetryConfig)) -> RetryConfig {
    let mut rc = test_retry();
    f(&mut rc);
    rc
}

fn tm(model_id: &str) -> TierMember {
    TierMember { model_id: model_id.to_string(), weight: 1 }
}

/// A two-group config on ONE base URL, so both groups can be driven from the
/// same fake upstreams: \`gA\` is always tier1 = ch0, tier2 = ch1.
///
/// \`over_a\` is gA's override (None = follow the global). \`gB\` exists purely as
/// the CONTROL group: it never carries an override, so whatever gA does with its
/// own copy must not touch gB's behaviour.
fn two_group_config(base: &str, prefixes: &[String], global: RetryConfig, over_a: Option<RetryConfig>) -> Config {
    let mut channels = Vec::new();
    let mut models = Vec::new();
    for (i, p) in prefixes.iter().enumerate() {
        channels.push(Channel {
            id: format!("ch{i}"),
            name: format!("channel-{i}"),
            base_url: format!("{base}{}", p),
            protocol: Protocol::Openai,
            api_key: format!("sk-fake-{i}"),
            enabled: true,
            weight: 1,
            headers: Default::default(),
        });
        models.push(ModelBinding {
            id: format!("m{i}"),
            channel_id: format!("ch{i}"),
            exposed: EXPOSED.to_string(),
            upstream: format!("up-model-{i}"),
            price_per_m: [1.0, 2.0, 0.0, 0.0],
            enabled: true,
        });
    }
    let groups = ["gA", "gB"]
        .iter()
        .map(|id| Group {
            id: (*id).to_string(),
            name: (*id).to_string(),
            exposed_models: vec![EXPOSED.to_string()],
            tiers: vec![
                Tier { level: 1, members: vec![tm("m0")] },
                Tier { level: 2, members: vec![tm("m1")] },
            ],
            overrides: GroupOverrides {
                retry: if *id == "gA" { over_a.clone() } else { None },
            },
        })
        .collect();
    Config {
        retry: global,
        channels,
        models,
        groups,
        api_keys: vec![
            ApiKey { key: "gw-test-key".into(), group_id: "gA".into(), name: "tester-a".into(), enabled: true },
            ApiKey { key: "gw-key-b".into(), group_id: "gB".into(), name: "tester-b".into(), enabled: true },
        ],
        ..Default::default()
    }
}

async fn harness(base_cfg: Config) -> (AppState, axum::Router) {
    gateway(base_cfg).await
}

/// Drive one chat request with an explicit api key (the two groups have two keys).
async fn post_stream_key(router: &axum::Router, key: &str, exposed: &str) -> (u16, String) {
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            serde_json::to_vec(&serde_json::json!({
                "model": exposed, "stream": true,
                "messages": [{"role": "user", "content": "hi"}], "max_tokens": 50
            }))
            .unwrap(),
        ))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 10 * 1024 * 1024).await.unwrap().to_vec();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

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

/// Newest request_logs row for one group. The streaming success branch writes
/// its row from a detached task, so this polls instead of assuming presence.
async fn newest_request_row(st: &AppState, group_id: &str) -> serde_json::Value {
    for _ in 0..300 {
        if let Some(r) = st.store.list_request_logs(10, None, Some(group_id)).unwrap().into_iter().next() {
            return r;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("no request_logs row for {group_id} appeared within 6s");
}

fn switch_rows(st: &AppState) -> Vec<serde_json::Value> {
    st.store.list_switch_logs(50, None, None).unwrap()
}

async fn admin_get(st: &AppState, uri: &str) -> (u16, serde_json::Value) {
    let admin = gw_server::admin::admin_router(st.clone());
    let req = axum::http::Request::builder().method("GET").uri(uri).body(axum::body::Body::empty()).unwrap();
    let resp = admin.oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    (status, body_json(resp).await)
}

async fn admin_put_config(st: &AppState, cfg: &Config) -> (u16, serde_json::Value) {
    let admin = gw_server::admin::admin_router(st.clone());
    let req = axum::http::Request::builder()
        .method("PUT")
        .uri("/admin/api/config")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(serde_json::to_vec(cfg).unwrap()))
        .unwrap();
    let resp = admin.oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    (status, body_json(resp).await)
}

// ---- 1. no override: the group borrows the global object itself ----

#[test]
fn g01_group_without_overrides_borrows_the_global_config() {
    let cfg = Config {
        retry: retry_with(|r| { r.max_attempts = 4; r.loop_guard_min_chars = 32; }),
        groups: vec![
            Group { id: "gA".into(), name: "A".into(), exposed_models: vec![], tiers: vec![], overrides: GroupOverrides::default() },
            // Same, but with an overrides object that is present and empty.
            Group { id: "gC".into(), name: "C".into(), exposed_models: vec![], tiers: vec![], overrides: GroupOverrides { retry: None } },
        ],
        ..Default::default()
    };
    for g in &cfg.groups {
        let rc = effective_retry(&cfg, g);
        assert!(
            std::ptr::eq(rc, &cfg.retry),
            "group {} without an override must borrow the global config, not a copy",
            g.id
        );
        assert_eq!(rc.max_attempts, 4);
        assert_eq!(rc.loop_guard_min_chars, 32);
    }
}

// ---- 2. with an override: the group's own copy wins, field by field ----

#[test]
fn g02_group_with_overrides_uses_its_own_copy() {
    let own = retry_with(|r| { r.max_attempts = 1; r.loop_guard_enabled = false; r.cooldown_wait = false; });
    let cfg = Config {
        retry: test_retry(),
        groups: vec![Group {
            id: "gA".into(),
            name: "A".into(),
            exposed_models: vec![],
            tiers: vec![],
            overrides: GroupOverrides { retry: Some(own.clone()) },
        }],
        ..Default::default()
    };
    let rc = effective_retry(&cfg, &cfg.groups[0]);
    assert!(!std::ptr::eq(rc, &cfg.retry), "an override must not alias the global config");
    assert_eq!(rc.max_attempts, 1, "the group's own number must win over global 6");
    assert_eq!(cfg.retry.max_attempts, 6, "the global copy must be untouched");
    assert!(!rc.loop_guard_enabled, "the override carries the whole loop-guard block");
    assert_eq!(rc.total_deadline_ms, own.total_deadline_ms);
    assert!(cfg.retry.loop_guard_enabled, "global stays on");
}

// ---- 3./4. wire format: \`"retry": null\` is kept, and legacy JSON still loads ----

#[test]
fn g03_null_retry_is_serialized_as_null_never_skipped() {
    let g = Group {
        id: "gA".into(),
        name: "A".into(),
        exposed_models: vec![],
        tiers: vec![],
        overrides: GroupOverrides::default(),
    };
    let v = serde_json::to_value(&g).unwrap();
    assert_eq!(v["overrides"]["retry"], serde_json::Value::Null, "overrides.retry must be present as null: {v}");
    assert!(v["overrides"].get("retry").is_some(), "the key itself must exist: {v}");
    // And it round-trips back to None rather than to a default RetryConfig.
    let back: Group = serde_json::from_value(v).unwrap();
    assert!(back.overrides.retry.is_none());
}

#[test]
fn g04_legacy_config_without_overrides_still_deserializes() {
    // The exact shape written by every build before task-28: no \`overrides\` key
    // anywhere. It must keep loading, with retry == None (i.e. follow global).
    let legacy = serde_json::json!({
        "retry": { "max_attempts": 3 },
        "groups": [{
            "id": "grp_main", "name": "主力分组",
            "exposed_models": ["m"],
            "tiers": [{ "level": 1, "members": [{"model_id": "m0", "weight": 7}] }]
        }]
    });
    let cfg: Config = serde_json::from_value(legacy).unwrap();
    assert_eq!(cfg.groups.len(), 1);
    assert!(cfg.groups[0].overrides.retry.is_none(), "an absent key must mean 'follow global'");
    assert_eq!(cfg.groups[0].tiers[0].members[0].weight, 7);
    assert_eq!(cfg.retry.max_attempts, 3);
    assert!(std::ptr::eq(effective_retry(&cfg, &cfg.groups[0]), &cfg.retry));
}

// ---- 5. a group budget that violates I4 is rejected, and nothing is saved ----

#[tokio::test]
async fn g05_put_rejects_an_i4_violating_group_override() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson, Behavior::GoodJson]).await;
    let cfg = two_group_config(&base, &prefixes, test_retry(), None);
    let (st, _router) = harness(cfg.clone()).await;
    // Seed the store the way production does at boot, so "config untouched" below
    // has something real to compare against (GET /admin/api/config reads the store,
    // not the live snapshot).
    st.store.save_config(&cfg).unwrap();

    // 6 * 3000 + 500 = 18500 <= 20000 holds globally, so ONLY the override can
    // be blamed: max_attempts 6 * attempt_deadline 30000 + 500 > 20000.
    let bad = test_retry_with_i4_violation();
    let mut broken = cfg.clone();
    broken.groups[0].overrides.retry = Some(bad);
    // Sanity: the global config on its own is accepted.
    assert!(gw_server::router::validate_budget(&cfg.retry).is_ok());
    assert!(gw_server::router::validate_budget(&broken.groups[0].overrides.retry.clone().unwrap()).is_err());

    let (status, body) = admin_put_config(&st, &broken).await;
    assert_eq!(status, 400, "body={body}");
    let msg = body["error"].as_str().unwrap_or_default().to_string();
    assert!(msg.starts_with("group gA: "), "the message must name the offending group: {msg}");
    assert!(msg.contains("exceeds total_deadline_ms"), "the global wording must be reused: {msg}");

    // Nothing may have been written: the stored config is still the old one and
    // gA still follows the global.
    let (code, stored) = admin_get(&st, "/admin/api/config").await;
    assert_eq!(code, 200);
    assert_eq!(stored["groups"].as_array().map(|a| a.len()), Some(2), "config must be untouched: {stored}");
    assert_eq!(stored["groups"][0]["id"], "gA");
    assert_eq!(stored["groups"][1]["id"], "gB");
    assert!(stored["groups"][0]["overrides"]["retry"].is_null(), "the rejected override must NOT be stored: {stored}");
    assert!(stored["groups"][1]["overrides"]["retry"].is_null(), "{stored}");

    // ... and the running snapshot too.
    let snap = st.config.get().unwrap();
    assert!(snap.config.groups[0].overrides.retry.is_none());

    // Same PUT with a LEGAL override is accepted and now reported as "group".
    let mut good = cfg.clone();
    good.groups[0].overrides.retry = Some(retry_with(|r| r.max_attempts = 2));
    let (status, body) = admin_put_config(&st, &good).await;
    assert_eq!(status, 200, "body={body}");
    let (code, members) = admin_get(&st, "/admin/api/group-members?group_id=gA").await;
    assert_eq!(code, 200);
    assert_eq!(members["retry_source"], "group", "{members}");
    assert_eq!(members["retry"]["max_attempts"], 2, "{members}");
}

/// max_attempts 6 * attempt_deadline_ms 30000 + reserve 500 > total 20000.
fn test_retry_with_i4_violation() -> RetryConfig {
    retry_with(|r| { r.attempt_deadline_ms = 30_000; r.max_attempts = 6; })
}

// ---- 6. the point of the feature: it is PER GROUP ----

#[tokio::test]
async fn g06_guard_off_in_one_group_does_not_disable_it_for_another() {
    // One fake upstream layout, two channels: ch0 is the degenerate one, ch1 is
    // good. Both groups are tier1 = ch0 then tier2 = ch1.
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::DegenerateShortCycle, Behavior::GoodStream]).await;
    let global = test_retry();
    let group_a = retry_with(|r| r.loop_guard_enabled = false);
    let cfg = two_group_config(&base, &prefixes, global.clone(), Some(group_a.clone()));
    let (st, router) = harness(cfg).await;

    // gA (override: guard OFF): the degenerate text must reach the client and the
    // walk must NOT switch to ch1 — one attempt, one committed row.
    let (status, sse) = post_stream_key(&router, "gw-test-key", EXPOSED).await;
    assert_eq!(status, 200, "sse={sse}");
    assert!(sse.contains(UNIT), "with the override the degenerate text must pass through: {sse}");
    assert_eq!(client_text(&sse).chars().filter(|c| *c == '好').count(), 40, "all 40 frames, no substitute answer");
    let row_a = newest_request_row(&st, "gA").await;
    assert_eq!(row_a["status"], "success", "{row_a}");
    assert_eq!(row_a["attempts"], 1, "guard off => no retry: {row_a}");
    assert_eq!(row_a["loop_retries"], 0, "{row_a}");
    assert!(switch_rows(&st).iter().all(|r| r["error_class"] == "committed"), "{:?}", switch_rows(&st));

    // gB (no override, same global): the guard is still armed, so the same
    // degenerate upstream is thrown away and ch1 answers.
    let (status, sse) = post_stream_key(&router, "gw-key-b", EXPOSED).await;
    assert_eq!(status, 200, "sse={sse}");
    assert!(!sse.contains(UNIT), "group B must still be protected: {sse}");
    assert_eq!(client_text(&sse), GOOD, "group B gets the good answer: {sse}");
    let row_b = newest_request_row(&st, "gB").await;
    assert_eq!(row_b["status"], "success", "{row_b}");
    assert_eq!(row_b["attempts"], 2, "one discarded + one committed: {row_b}");
    assert_eq!(row_b["loop_retries"], 1, "{row_b}");
    assert_eq!(row_b["final_channel"], "ch1", "{row_b}");

    // The two groups really ran on different policies, and each row recorded the
    // group it belonged to.
    assert_ne!(row_a["attempts"], row_b["attempts"]);
    assert_eq!(row_a["group_id"], "gA");
    assert_eq!(row_b["group_id"], "gB");
}

// ---- 7. inherit path: same numbers as the global config ----

#[tokio::test]
async fn g07_group_without_override_behaves_exactly_like_the_global_config() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::DegenerateShortCycle, Behavior::GoodStream]).await;
    let global = test_retry();

    // (a) no overrides at all on gA
    let cfg = two_group_config(&base, &prefixes, global.clone(), None);
    let (st, router) = harness(cfg).await;
    let (status, sse) = post_stream_key(&router, "gw-test-key", EXPOSED).await;
    assert_eq!(status, 200, "sse={sse}");
    assert!(!sse.contains(UNIT), "sse={sse}");
    let inherited = newest_request_row(&st, "gA").await;

    // (b) the SAME policy spelled out as an explicit override with identical
    // numbers: it must produce the same accounting, one group apart.
    let cfg = two_group_config(&base, &prefixes, global, Some(test_retry()));
    let (st, router) = harness(cfg).await;
    let (status, sse) = post_stream_key(&router, "gw-key-b", EXPOSED).await;
    assert_eq!(status, 200, "sse={sse}");
    assert!(!sse.contains(UNIT), "sse={sse}");
    let explicit = newest_request_row(&st, "gB").await;

    for k in ["status", "attempts", "loop_retries", "final_channel", "final_upstream_model"] {
        assert_eq!(inherited[k], explicit[k], "field {k} differs between inherited and explicit: {inherited} vs {explicit}");
    }
    assert_eq!(inherited["attempts"], 2);
    assert_eq!(inherited["loop_retries"], 1);
}

// ---- 8. a per-group max_attempts really bounds that group's walk ----

#[tokio::test]
async fn g08_per_group_max_attempts_bounds_the_walk() {
    // gA: max_attempts = 1 override, single degenerate candidate -> Exhausted
    // after ONE discarded attempt. gB: global 6 attempts over the same single
    // candidate -> it gets to the second one (the 2-hit cooldown rule), so the
    // two groups provably ran different budgets.
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::DegenerateShortCycle]).await;
    let cfg = two_group_config(&base, &prefixes, test_retry(), Some(retry_with(|r| r.max_attempts = 1)));
    // Only tier1/ch0 exists on this upstream list: drop the dangling tier2 member.
    let mut cfg = cfg;
    cfg.groups[0].tiers = vec![Tier { level: 1, members: vec![tm("m0")] }];
    cfg.groups[1].tiers = vec![Tier { level: 1, members: vec![tm("m0")] }];
    let (st, router) = harness(cfg).await;

    let (status, body) = post_stream_key(&router, "gw-test-key", EXPOSED).await;
    assert_eq!(status, 502, "override max_attempts=1 => exhausted: {body}");
    assert!(body.contains("all_upstreams_failed"), "{body}");
    // The repeated unit appears in the failure MESSAGE by design (it names the
    // cause); what must never appear is a degenerate ANSWER, so assert on the
    // absence of an answer rather than on a substring of the diagnosis.
    assert!(!body.contains("\"choices\""), "no answer may be delivered: {body}");
    let row_a = newest_request_row(&st, "gA").await;
    assert_eq!(row_a["status"], "exhausted", "{row_a}");
    assert_eq!(row_a["attempts"], 1, "the override bound the walk: {row_a}");

    let (status, body) = post_stream_key(&router, "gw-key-b", EXPOSED).await;
    assert_eq!(status, 502, "{body}");
    let row_b = newest_request_row(&st, "gB").await;
    assert_eq!(row_b["attempts"], 2, "the global 6 lets the walk try twice: {row_b}");
    assert_eq!(row_b["loop_retries"], 2, "{row_b}");
}

// ---- 9. the console payload carries the effective config and its provenance ----

#[tokio::test]
async fn g09_group_members_reports_retry_source_and_both_configs() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson, Behavior::GoodJson]).await;
    let cfg = two_group_config(&base, &prefixes, test_retry(), Some(retry_with(|r| {
        r.max_attempts = 3;
        r.loop_guard_min_chars = 128;
    })));
    let (st, _router) = harness(cfg).await;

    let (code, a) = admin_get(&st, "/admin/api/group-members?group_id=gA").await;
    assert_eq!(code, 200, "{a}");
    assert_eq!(a["retry_source"], "group", "{a}");
    assert_eq!(a["retry"]["max_attempts"], 3, "{a}");
    assert_eq!(a["retry"]["loop_guard_min_chars"], 128, "{a}");
    // The full shape, including all seven loop-guard knobs.
    for k in [
        "max_attempts", "max_cycles", "total_deadline_ms", "attempt_deadline_ms",
        "connect_timeout_ms", "ttfb_timeout_ms", "first_event_timeout_ms",
        "chunk_idle_timeout_ms", "reserve_ms", "cooldown_wait",
        "loop_guard_enabled", "loop_guard_min_chars", "loop_guard_repeat_min_chars",
        "loop_guard_repeat_ratio", "loop_guard_max_repeats", "loop_guard_window_chars",
        "loop_guard_tail_chars", "loop_guard_hold_max_ms", "loop_guard_reasoning_enabled",
    ] {
        assert!(a["retry"].get(k).is_some(), "effective retry lost {k}: {a}");
    }
    // global_retry is the untouched global copy, so the console can preview
    // "跟随全局" without another request.
    assert_eq!(a["global_retry"]["max_attempts"], 6, "{a}");
    assert_eq!(a["global_retry"]["loop_guard_min_chars"], 64, "{a}");
    assert_eq!(a["global_retry"]["loop_guard_hold_max_ms"], 250, "the holdback ceiling must reach the console: {a}");

    let (code, b) = admin_get(&st, "/admin/api/group-members?group_id=gB").await;
    assert_eq!(code, 200, "{b}");
    assert_eq!(b["retry_source"], "global", "{b}");
    assert_eq!(b["retry"]["max_attempts"], 6, "{b}");
    assert_eq!(b["retry"]["loop_guard_min_chars"], 64, "{b}");
    assert_eq!(b["retry"]["loop_guard_hold_max_ms"], 250, "{b}");
    assert_eq!(b["global_retry"], a["global_retry"], "both groups must report the same global copy");

    // The task-27 keys must survive the addition.
    for k in ["group_id", "group_name", "active_key", "members"] {
        assert!(a.get(k).is_some(), "group-members lost {k}: {a}");
    }
}
