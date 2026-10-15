//! Fake-upstream matrix: every failure pattern must be contained INSIDE the
//! gateway loop (user's core requirement) — the client sees either a full
//! success or one structurally-valid error, never a raw upstream leak.
//!
//! Leak checks: non-stream bodies must be gateway-rendered JSON (no upstream
//! HTML/status text); streams must terminate with [DONE] and, when repaired,
//! carry an explicit gateway error frame (never silent success).

mod support;

use support::{gateway, post_chat, post_chat_stream, spawn_fake_upstreams, test_config, test_retry, Behavior};
use tower::util::ServiceExt;

const EXPOSED: &str = "my-model";
const OK_BODY: &str = "hello from json";

fn leak_scan_json(v: &serde_json::Value) {
    // upstream raw bodies (html / plain text) must never reach the client
    let s = v.to_string();
    assert!(!s.contains("<html>"), "upstream HTML leaked: {s}");
    assert!(!s.contains("boom"), "upstream 500 text leaked: {s}");
}

// ---- 1. all tier-1 fail (500), tier-2 good JSON => success, switch logged ----
#[tokio::test]
async fn t01_tier1_500_falls_to_tier2_success() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::Fail500, Behavior::GoodJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai; 2], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    let (status, body) = post_chat(&router, EXPOSED, false).await;
    assert_eq!(status, 200, "body={body}");
    leak_scan_json(&body);
    assert_eq!(body.pointer("/choices/0/message/content").and_then(|c| c.as_str()), Some(OK_BODY));
    // model echo must be the EXPOSED name, never the upstream name
    assert_eq!(body.pointer("/model").and_then(|m| m.as_str()), Some(EXPOSED));
}

// ---- 2. single channel 500 twice => 502 exhausted, no upstream text leak ----
#[tokio::test]
async fn t02_single_500_exhausts_clean_502() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::Fail500]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    let (status, body) = post_chat(&router, EXPOSED, false).await;
    assert_eq!(status, 502);
    leak_scan_json(&body);
    // The message must say HOW MANY attempts failed and WHY. Checking for the
    // word "all" only worked while the text was English; the properties that
    // matter are language-independent and are what the operator actually needs.
    let msg = body.pointer("/error/message").and_then(|m| m.as_str()).unwrap_or_default();
    assert!(msg.contains('2'), "the attempt count must be stated: {msg}");
    assert!(
        msg.contains("上游服务端错误"),
        "the failure CAUSE must be stated, not just the count: {msg}"
    );
}

// ---- 3. garbage body (not JSON) => contained, next candidate or clean 502 ----
#[tokio::test]
async fn t03_garbage_body_contained() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::Garbage, Behavior::GoodJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai; 2], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    let (status, body) = post_chat(&router, EXPOSED, false).await;
    assert_eq!(status, 200, "body={body}");
    leak_scan_json(&body);
}

// ---- 4. empty body => contained ----
#[tokio::test]
async fn t04_empty_body_contained() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::Empty, Behavior::GoodJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai; 2], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    let (status, body) = post_chat(&router, EXPOSED, false).await;
    assert_eq!(status, 200, "body={body}");
    leak_scan_json(&body);
}

// ---- 5. hang (TTFB timeout) => attempt deadline kills it, tier-2 serves ----
#[tokio::test]
async fn t05_hang_times_out_falls_to_tier2() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::Hang, Behavior::GoodJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai; 2], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    let (status, body) = post_chat(&router, EXPOSED, false).await;
    assert_eq!(status, 200, "body={body}");
    leak_scan_json(&body);
}

// ---- 6. 401 auth => PERMANENT channel isolation once the failure threshold
// is crossed; tier-2 keeps serving and the isolated channel is skipped ----
#[tokio::test]
async fn t06_401_isolates_channel_serves_tier2() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::Fail401, Behavior::GoodJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai; 2], 1, EXPOSED, test_retry());
    let (st, router) = gateway(cfg).await;
    let ch0 = gw_server::health::CooldownKey::channel_only("ch0");
    let auth_attempts = || {
        st.store.list_switch_logs(100, None, None).unwrap().iter()
            .filter(|l| l["error_class"] == "auth_error").count()
    };

    // Request 1: one 401 arms the counter but does NOT isolate yet
    // (FAILURES_BEFORE_COOLDOWN = 2), so tier-2 serves.
    let (status, body) = post_chat(&router, EXPOSED, false).await;
    assert_eq!(status, 200, "body={body}");
    leak_scan_json(&body);
    assert_eq!(auth_attempts(), 1, "one 401 attempt before the threshold");
    assert!(!st.health.is_cooling(&ch0), "a single 401 must not isolate the channel yet");
    assert!(st.health.snapshot_summary().is_empty(), "nothing cooling after one failure");

    // Request 2: the second consecutive 401 crosses the threshold and isolates
    // ch0 permanently (kind=auth, channel-scoped).
    let (status, body) = post_chat(&router, EXPOSED, false).await;
    assert_eq!(status, 200, "body={body}");
    leak_scan_json(&body);
    assert_eq!(auth_attempts(), 2, "second 401 attempt");
    assert!(st.health.is_cooling(&ch0), "two consecutive 401s must isolate the channel");
    let rows = st.health.snapshot_summary();
    let auth = rows.iter().find(|r| r.channel_id == "ch0").expect("auth row must be visible in health");
    assert_eq!(auth.kind, "auth");
    assert_eq!(auth.upstream_model, "*", "auth isolation is channel-level");
    assert!(auth.permanent, "auth isolation must be permanent");
    assert_eq!(auth.cooling_until_ms, 0, "a permanent isolation has no countdown");
    assert_eq!(auth.remaining_ms, 0, "a permanent isolation has no countdown");
    assert_eq!(auth.consecutive_failures, 2);

    // Request 3: the isolated channel is skipped entirely => one attempt.
    let (status, body) = post_chat(&router, EXPOSED, false).await;
    assert_eq!(status, 200, "body={body}");
    assert_eq!(auth_attempts(), 2, "an isolated channel must never be attempted again");
}

// ---- 7. 404 model-not-found => model-scoped isolation, tier-2 serves ----
#[tokio::test]
async fn t07_404_model_scoped_isolation() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::Fail404, Behavior::GoodJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai; 2], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    let (status, body) = post_chat(&router, EXPOSED, false).await;
    assert_eq!(status, 200, "body={body}");
    leak_scan_json(&body);
}

// ---- 8. streaming: tier-1 breaks mid-stream AFTER commit => repair frame, no retry after commit ----
#[tokio::test]
async fn t08_stream_break_mid_repaired() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::StreamBreakMid]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    let (status, sse) = post_chat_stream(&router, EXPOSED).await;
    assert_eq!(status, 200);
    support::assert_stream_repaired(&sse);
    // committed content IS delivered (partial), then explicit repair
    assert!(sse.contains("partial"), "committed delta must be delivered: {sse}");
}

// ---- 9. streaming: upstream error frame BEFORE commit => failover to tier-2 clean stream ----
#[tokio::test]
async fn t09_stream_error_frame_fails_over() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::StreamErrorFrame, Behavior::GoodStream]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai; 2], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    let (status, sse) = post_chat_stream(&router, EXPOSED).await;
    assert_eq!(status, 200);
    support::assert_stream_clean(&sse);
    assert!(sse.contains("he") && sse.contains("llo"), "tier-2 stream content must arrive: {sse}");
}

// ---- 10. streaming: EOF before any content => failover, clean stream ----
#[tokio::test]
async fn t10_stream_eof_early_fails_over() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::StreamEofEarly, Behavior::GoodStream]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai; 2], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    let (status, sse) = post_chat_stream(&router, EXPOSED).await;
    assert_eq!(status, 200);
    support::assert_stream_clean(&sse);
}

// ---- 11. streaming: clean success path end-to-end ----
#[tokio::test]
async fn t11_stream_clean_success() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodStream]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    let (status, sse) = post_chat_stream(&router, EXPOSED).await;
    assert_eq!(status, 200);
    support::assert_stream_clean(&sse);
    assert!(sse.contains("he") && sse.contains("llo"));
    // usage chunk must be forwarded (10 in / 5 out from the fake)
    assert!(sse.contains("\"prompt_tokens\":10"), "usage frame forwarded: {sse}");
}

// ---- 12. tier cycling: tier1 dead + tier2 dead => cycle back to tier1 until max_cycles ----
#[tokio::test]
async fn t12_cycle_back_to_tier1_then_exhaust() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::Fail500, Behavior::Fail500]).await;
    let mut cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai; 2], 1, EXPOSED, test_retry());
    cfg.retry.max_cycles = 2;
    cfg.retry.cooldown_wait = false; // keep the test fast: no waiting on cooldowns
    let (_st, router) = gateway(cfg).await;
    let (status, body) = post_chat(&router, EXPOSED, false).await;
    assert_eq!(status, 502);
    leak_scan_json(&body);
    let st = _st;
    let logs = st.store.list_switch_logs(100, None, None).unwrap();
    // tier1 members get re-tried in cycle 2 (I3: at most once per cycle)
    let tier1_tries = logs.iter().filter(|l| l["tier"] == 1).count();
    assert!(tier1_tries >= 2, "tier1 must be re-attempted in cycle 2: {logs:?}");
}

// ---- 13. switch log records the failover ----
#[tokio::test]
async fn t13_switch_logs_recorded() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::Fail500, Behavior::GoodJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai; 2], 1, EXPOSED, test_retry());
    let (st, router) = gateway(cfg).await;
    let (status, _body) = post_chat(&router, EXPOSED, false).await;
    assert_eq!(status, 200);
    let logs = st.store.list_switch_logs(100, None, None).unwrap();
    assert!(logs.len() >= 2, "one failed + one committed switch record expected: {logs:?}");
    let failed = logs.iter().find(|l| l["outcome"] == "failed").expect("failed record");
    assert_eq!(failed["tier"], 1);
    assert_eq!(failed["http_status"], 500);
    let committed = logs.iter().find(|l| l["outcome"] == "committed").expect("committed record");
    assert_eq!(committed["tier"], 2);
    let reqs = st.store.list_request_logs(100, None, None).unwrap();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0]["status"], "success");
    assert_eq!(reqs[0]["attempts"], 2);
    // cost accounting ran (input 11 + output 7 tokens at [1,2] USD/M)
    let expected = 11.0f64 / 1e6 + 7.0 * 2.0 / 1e6;
    let cost = reqs[0]["cost_usd"].as_f64().unwrap_or(0.0);
    assert!((cost - expected).abs() < 1e-9, "cost {cost} vs {expected}");
    // the UI renders 缓存读/写 and 渠道 columns from these projections — a missing
    // key silently degrades to "—", so assert the keys are actually present.
    for k in ["cache_read_tokens", "cache_creation_tokens"] {
        assert!(reqs[0].get(k).is_some(), "request_logs projection missing {k}: {}", reqs[0]);
    }
    // 成本必须能回答"落到哪个渠道的哪个模型"
    let by_model = st.store.stats_by_model(None).unwrap();
    assert!(!by_model.is_empty(), "stats_by_model empty");
    for k in ["exposed_model", "upstream_model", "channel_id", "cost_usd"] {
        assert!(by_model[0].get(k).is_some(), "stats_by_model missing {k}: {}", by_model[0]);
    }
    // 统计页顶部卡片直接读 summary 的这些键；缺键会静默退化成 "—"，
    // 所以必须断言键存在（曾经 avg_duration_ms 缺失导致「平均耗时」永远显示 —）。
    let summary = st.store.stats_summary(None).unwrap();
    for k in [
        "requests",
        "success",
        "exhausted",
        "aborted",
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_creation_tokens",
        "cost_usd",
        "wasted_cost_usd",
        "avg_duration_ms",
        "avg_ttft_ms",
    ] {
        assert!(summary.get(k).is_some(), "stats_summary missing {k}: {summary}");
    }
    assert_eq!(summary["requests"], 1);
    assert_eq!(summary["success"], 1);
    // avg_duration_ms averages the successful request and must be a real number
    assert!(
        summary["avg_duration_ms"].as_f64().is_some(),
        "avg_duration_ms must be numeric once a success exists: {summary}"
    );
}

// ---- 19. empty store: summary must be numeric, not null-blank ----
// The console renders these cards directly; NULL sums would surface as "—" on a
// fresh install, which reads as "broken" rather than "no traffic yet".
#[tokio::test]
async fn t19_empty_store_summary_is_zeroed() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    let (st, _router) = gateway(cfg).await;
    let s = st.store.stats_summary(None).unwrap();
    assert_eq!(s["requests"], 0);
    for k in ["input_tokens", "output_tokens", "cache_read_tokens", "cache_creation_tokens", "cost_usd", "wasted_cost_usd"] {
        assert_eq!(s[k], 0, "empty store {k} must be 0, got {}", s[k]);
    }
    // with no successful request there is no meaningful latency: stays null => "—"
    assert!(s["avg_duration_ms"].is_null(), "no successes => avg_duration_ms null: {s}");
}

// ---- 14. unknown model => 404 no_route, no upstream calls ----
#[tokio::test]
async fn t14_unknown_model_no_route() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    let (st, router) = gateway(cfg).await;
    use tower::util::ServiceExt;
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer gw-test-key")
        .body(axum::body::Body::from(
            serde_json::to_vec(&serde_json::json!({"model": "not-exposed", "messages": [{"role":"user","content":"hi"}]})).unwrap(),
        ))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status().as_u16(), 404);
    let logs = st.store.list_switch_logs(100, None, None).unwrap();
    assert!(logs.is_empty(), "no upstream attempts for unknown model: {logs:?}");
}

// ---- 15. bad api key => 401 invalid_api_key ----
#[tokio::test]
async fn t15_bad_client_key() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    use tower::util::ServiceExt;
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer wrong-key")
        .body(axum::body::Body::from(
            serde_json::to_vec(&serde_json::json!({"model": EXPOSED, "messages": [{"role":"user","content":"hi"}]})).unwrap(),
        ))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

// ---- 16. /v1/models lists group-exposed models AND the group's own routable name ----
// Task-27 changed the list on purpose: the group name/id became routable names
// (wildcard routing), so a client must be able to discover them. The group's own
// exposed models stay FIRST and in declared order.
#[tokio::test]
async fn t16_models_list() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    let (_st, router) = gateway(cfg).await;
    use tower::util::ServiceExt;
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/v1/models")
        .header("authorization", "Bearer gw-test-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let ids: Vec<&str> = v.pointer("/data").and_then(|d| d.as_array()).map(|a| a.iter().filter_map(|m| m.get("id").and_then(|i| i.as_str())).collect()).unwrap_or_default();
    // test_config's group is id "g1" / name "test-group".
    assert_eq!(ids, vec![EXPOSED, "test-group", "g1"]);
}
// ---- 17. REGRESSION: shipped defaults must satisfy I4 and serve requests ----
// A fresh install has an empty DB, so every request runs on RetryConfig::default().
// That default used to violate its own I4 invariant (12 x 45_000 + 5_000 > 120_000),
// which made build_budget fail and turned EVERY request into "no candidate".
#[tokio::test]
async fn t17_default_config_serves_requests() {
    // 1. the shipped defaults must pass the write-time validator
    let d = gw_server::config::RetryConfig::default();
    gw_server::router::validate_budget(&d).expect("shipped RetryConfig defaults must satisfy I4");

    // 2. and a gateway built from those defaults must really serve traffic
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson]).await;
    let mut cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, d);
    // test_config injects a hand-tuned retry; put the defaults back to model a fresh install
    cfg.retry = gw_server::config::RetryConfig::default();
    let (_st, router) = gateway(cfg).await;
    let (status, body) = post_chat(&router, EXPOSED, false).await;
    assert_eq!(status, 200, "fresh-install defaults must serve: {body}");
    assert_eq!(body.pointer("/choices/0/message/content").and_then(|c| c.as_str()), Some(OK_BODY));
}

// ---- 18. an I4-violating config is still served (clamped), never rejected at runtime ----
#[tokio::test]
async fn t18_i4_violating_config_clamps_instead_of_failing() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson]).await;
    let mut bad = test_retry();
    bad.max_attempts = 12;
    bad.attempt_deadline_ms = 45_000;
    bad.reserve_ms = 5_000;
    bad.total_deadline_ms = 120_000; // 545_000 > 120_000 -> violates I4
    assert!(gw_server::router::validate_budget(&bad).is_err(), "fixture must violate I4");

    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, bad);
    let (_st, router) = gateway(cfg).await;
    // runtime must clamp rather than refuse: the client still gets its answer
    let (status, body) = post_chat(&router, EXPOSED, false).await;
    assert_eq!(status, 200, "I4-violating config must be clamped, not fatal: {body}");
    assert_eq!(body.pointer("/choices/0/message/content").and_then(|c| c.as_str()), Some(OK_BODY));
}
// ---- 20. group scoping: switch/request logs + stats carry and filter group_id ----
// The console shows per-group logs and per-group usage, so every write must
// stamp the group and every read must be able to narrow to it. A group that
// never saw traffic must come back empty, not fall back to the global set.
#[tokio::test]
async fn t20_group_scoped_logs_and_stats() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson]).await;
    let mut cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    // second group that receives no traffic at all
    cfg.groups.push(gw_server::config::Group {
        id: "g2".into(),
        name: "quiet-group".into(),
        exposed_models: vec![EXPOSED.to_string()],
        tiers: vec![gw_server::config::Tier {
            level: 1,
            members: vec![gw_server::config::TierMember { model_id: "m0".into(), weight: 1 }],
        }],
        overrides: gw_server::config::GroupOverrides::default(),
    });
    cfg.api_keys.push(gw_server::config::ApiKey { key: "gw-quiet-key".into(), group_id: "g2".into(), name: "quiet".into(), enabled: true });
    let (st, router) = gateway(cfg).await;
    let (status, _body) = post_chat(&router, EXPOSED, false).await;
    assert_eq!(status, 200);

    let all = st.store.list_switch_logs(100, None, None).unwrap();
    assert!(!all.is_empty(), "switch log must record the attempt");
    assert!(all.iter().all(|l| l["group_id"] == "g1"), "every switch row carries the entry group: {all:?}");
    let scoped = st.store.list_switch_logs(100, None, Some("g1")).unwrap();
    assert_eq!(scoped.len(), all.len(), "g1 scope sees its own attempts");
    let other = st.store.list_switch_logs(100, None, Some("g2")).unwrap();
    assert!(other.is_empty(), "g2 saw no traffic: {other:?}");

    let reqs = st.store.list_request_logs(100, None, Some("g1")).unwrap();
    assert_eq!(reqs.len(), 1, "one request logged for g1: {reqs:?}");
    assert_eq!(reqs[0]["group_id"], "g1");
    assert!(st.store.list_request_logs(100, None, Some("g2")).unwrap().is_empty());

    // stats scoped to a group vs global
    let g1 = st.store.stats_summary(Some("g1")).unwrap();
    assert_eq!(g1["requests"], 1, "g1 summary counts only its own: {g1}");
    let g2 = st.store.stats_summary(Some("g2")).unwrap();
    assert_eq!(g2["requests"], 0, "quiet group summary is empty: {g2}");
    assert!(st.store.stats_by_model(Some("g2")).unwrap().is_empty(), "quiet group has no model rows");
    assert_eq!(st.store.stats_by_model(Some("g1")).unwrap().len(), 1);

    // the cross-group overview always lists every group that saw traffic
    // Signature change (task-43 defect 1): `None` is the cross-group overview
    // this assertion has always been about.
    let by_group = st.store.stats_by_group(None).unwrap();
    let row = by_group.iter().find(|r| r["group_id"] == "g1").expect("g1 row present");
    assert_eq!(row["requests"], 1, "by_group counts g1: {by_group:?}");
    assert!(by_group.iter().all(|r| r["group_id"] != "g2"), "a group with no traffic has no row: {by_group:?}");
}

// =====================================================================
// I5 (task-17 defect A): candidates that EXIST but are ALL cooling must
// never produce a zero-attempt 404. The walk waits for the earliest real
// expiry and retries; when the wait cannot fit the budget, or every entry
// is permanently isolated, the client gets 503 all_upstreams_cooling.
// =====================================================================

/// Arm a real (non-permanent) cooldown: two failures cross
/// FAILURES_BEFORE_COOLDOWN, so the second one sets until = now + dur.
/// Same threshold crossing, on a bare registry (unit-level reconcile tests).
fn arm_key(health: &HealthRegistry, key: CooldownKey, dur: std::time::Duration) {
    for _ in 0..2 {
        health.apply(Attribution::Cooldown(key.clone(), dur, "backoff"));
    }
}

fn arm(st: &gw_server::proxy::AppState, channel: &str, upstream: &str, dur: std::time::Duration) {
    for _ in 0..2 {
        st.health.apply(gw_server::health::Attribution::Cooldown(
            gw_server::health::CooldownKey::scoped(channel, upstream),
            dur,
            "backoff",
        ));
    }
}

// ---- 21. every candidate cooling + cooldown_wait => wait, then succeed (I5) ----
#[tokio::test]
async fn t21_all_cooling_waits_then_succeeds() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    let (st, router) = gateway(cfg).await;
    // deterministic and fast: 600ms real cooldown on the only candidate
    arm(&st, "ch0", "up-model-0", std::time::Duration::from_millis(600));

    let started = std::time::Instant::now();
    let (status, body) = post_chat(&router, EXPOSED, false).await;
    let elapsed = started.elapsed();

    assert_eq!(status, 200, "all-cooling must wait and retry, not fail: {body}");
    assert_eq!(body.pointer("/choices/0/message/content").and_then(|c| c.as_str()), Some(OK_BODY));
    assert!(elapsed >= std::time::Duration::from_millis(400), "request must have actually waited for the cooldown: {elapsed:?}");
    let logs = st.store.list_switch_logs(100, None, None).unwrap();
    assert!(!logs.is_empty(), "a real attempt happened after the wait (attempts must not be 0): {logs:?}");
    assert_eq!(logs.last().unwrap()["outcome"], "committed", "the post-wait attempt succeeded: {logs:?}");
}

// ---- 22. cooldown_wait = false + all cooling => 503 with ZERO attempts ----
#[tokio::test]
async fn t22_all_cooling_without_wait_is_503_not_404() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson]).await;
    let mut cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    cfg.retry.cooldown_wait = false;
    let (st, router) = gateway(cfg).await;
    arm(&st, "ch0", "up-model-0", std::time::Duration::from_secs(60));

    let (status, body) = post_chat(&router, EXPOSED, false).await;
    assert_eq!(status, 503, "all-cooling with wait disabled is a 503, never a 404: {body}");
    assert_eq!(body.pointer("/error/code").and_then(|c| c.as_str()), Some("all_upstreams_cooling"));
    let logs = st.store.list_switch_logs(100, None, None).unwrap();
    assert!(logs.is_empty(), "no upstream attempt may be made while cooling: {logs:?}");
}

// ---- 22b. the 503 all-cooling reply must carry a well-formed retry-after: 30 ----
#[tokio::test]
async fn t22b_all_cooling_carries_retry_after_header() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson]).await;
    let mut cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    cfg.retry.cooldown_wait = false;
    let (st, router) = gateway(cfg).await;
    arm(&st, "ch0", "up-model-0", std::time::Duration::from_secs(60));

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer gw-test-key")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            serde_json::to_vec(&serde_json::json!({
                "model": EXPOSED, "stream": false,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status().as_u16(), 503, "all cooling must be a 503");
    // The value must be exactly "30" (bare seconds): a client library parses it, so a
    // malformed value ("retry-after: 30") silently breaks retry pacing.
    assert_eq!(
        resp.headers().get("retry-after").and_then(|v| v.to_str().ok()),
        Some("30"),
        "headers={:?}",
        resp.headers()
    );
}

// ---- 23. every candidate PERMANENTLY isolated (401) => 503, not 404 ----
#[tokio::test]
async fn t23_all_permanent_isolation_is_503() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::Fail401, Behavior::Fail401]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai; 2], 2, EXPOSED, test_retry());
    let (st, router) = gateway(cfg).await;
    // two real 401 rounds: each request arms both channels permanently
    for _ in 0..2 {
        let (_s, _b) = post_chat(&router, EXPOSED, false).await;
    }
    let rows = st.health.snapshot_summary();
    assert_eq!(rows.len(), 2, "both channels permanently isolated: {rows:?}");
    assert!(rows.iter().all(|r| r.permanent), "401 isolation is permanent: {rows:?}");

    let started = std::time::Instant::now();
    let (status, body) = post_chat(&router, EXPOSED, false).await;
    let elapsed = started.elapsed();
    assert_eq!(status, 503, "permanently isolated candidates are 'all cooling', not 'no route': {body}");
    assert_eq!(body.pointer("/error/code").and_then(|c| c.as_str()), Some("all_upstreams_cooling"));
    assert!(elapsed < std::time::Duration::from_secs(2), "a permanent isolation has no expiry to wait for: {elapsed:?}");
}

// ---- 24. genuinely no candidate (model not exposed by the group) => 404 ----
#[tokio::test]
async fn t24_no_candidate_at_all_is_still_404() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson]).await;
    let mut cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    // the model is bound to a DISABLED channel: a candidate is never constructed
    cfg.channels[0].enabled = false;
    let (st, router) = gateway(cfg).await;
    arm(&st, "ch0", "up-model-0", std::time::Duration::from_secs(60));

    let (status, body) = post_chat(&router, EXPOSED, false).await;
    assert_eq!(status, 404, "no candidate was built, so this is a genuine no-route: {body}");
    assert_eq!(body.pointer("/error/code").and_then(|c| c.as_str()), Some("no_route_for_model"));
    let logs = st.store.list_switch_logs(100, None, None).unwrap();
    assert!(logs.is_empty(), "no attempt for a group without candidates: {logs:?}");
}

// =====================================================================
// Defect B: PUT /admin/api/config must reconcile the health registry.
// =====================================================================
use gw_server::health::{Attribution, CooldownKey, HealthRegistry};
use gw_server::config::{Channel, Config, Group, GroupOverrides, ModelBinding, Protocol, Tier, TierMember};

fn chan(id: &str, key: &str, base: &str) -> Channel {
    Channel {
        id: id.into(),
        name: id.into(),
        base_url: base.into(),
        protocol: Protocol::Openai,
        api_key: key.into(),
        enabled: true,
        weight: 1,
        headers: Default::default(),
    }
}

fn mdl(id: &str, channel: &str, upstream: &str) -> ModelBinding {
    ModelBinding {
        id: id.into(),
        channel_id: channel.into(),
        exposed: EXPOSED.into(),
        upstream: upstream.into(),
        price_per_m: [1.0, 2.0, 0.0, 0.0],
        enabled: true,
    }
}

fn group1(model_ids: &[&str]) -> Group {
    Group {
        id: "g1".into(),
        name: "test-group".into(),
        exposed_models: vec![EXPOSED.into()],
        tiers: vec![Tier { level: 1, members: model_ids.iter().map(|m| TierMember { model_id: (*m).into(), weight: 1 }).collect() }],
        overrides: GroupOverrides::default(),
    }
}

fn base_cfg(channels: Vec<Channel>, models: Vec<ModelBinding>, group: Group) -> Config {
    Config { retry: test_retry(), channels, models, groups: vec![group], ..Default::default() }
}

// ---- 25. an api_key fix clears that channel's permanent 401 isolation ----
#[tokio::test]
async fn t25_reconcile_clears_channel_on_key_change() {
    let health = HealthRegistry::new(None);
    let old = base_cfg(
        vec![chan("chA", "sk-old", "http://127.0.0.1:1"), chan("chB", "sk-b", "http://127.0.0.1:2")],
        vec![mdl("m0", "chA", "up-0"), mdl("m1", "chB", "up-1")],
        group1(&["m0", "m1"]),
    );
    // chA permanently isolated (401), plus a transient scoped row on chB
    health.apply(Attribution::Cooldown(CooldownKey::channel_only("chA"), HealthRegistry::auth_permanent(), "auth"));
    arm_key(&health, CooldownKey::scoped("chB", "up-1"), std::time::Duration::from_secs(60));

    let mut new = old.clone();
    new.channels[0].api_key = "sk-fixed".into(); // only chA's credential changed
    let cleared = health.reconcile_config(&old.channels, &new);

    assert_eq!(cleared, 1, "only the changed channel's row is cleared");
    assert!(!health.is_cooling(&CooldownKey::channel_only("chA")), "api_key change must drop the permanent isolation");
    assert!(health.is_cooling(&CooldownKey::scoped("chB", "up-1")), "untouched channel keeps its cooldown state (rule d)");
}

// ---- 26. a deleted channel loses its rows; unrelated state survives ----
#[tokio::test]
async fn t26_reconcile_clears_deleted_channel() {
    let health = HealthRegistry::new(None);
    let old = base_cfg(
        vec![chan("chA", "sk-a", "http://127.0.0.1:1"), chan("chB", "sk-b", "http://127.0.0.1:2")],
        vec![mdl("m0", "chA", "up-0"), mdl("m1", "chB", "up-1")],
        group1(&["m0", "m1"]),
    );
    health.apply(Attribution::Cooldown(CooldownKey::channel_only("chA"), HealthRegistry::auth_permanent(), "auth"));
    arm_key(&health, CooldownKey::scoped("chA", "up-0"), std::time::Duration::from_secs(60));
    arm_key(&health, CooldownKey::scoped("chB", "up-1"), std::time::Duration::from_secs(60));

    let new = base_cfg(
        vec![chan("chB", "sk-b", "http://127.0.0.1:2")],
        vec![mdl("m1", "chB", "up-1")],
        group1(&["m1"]),
    );
    let cleared = health.reconcile_config(&old.channels, &new);

    assert_eq!(cleared, 2, "channel-level AND scoped rows of the deleted channel are cleared");
    assert!(health.snapshot_summary().iter().all(|r| r.channel_id != "chA"), "no chA row may survive: {:?}", health.snapshot_summary());
    assert!(health.is_cooling(&CooldownKey::scoped("chB", "up-1")), "the surviving channel is untouched");
}

// ---- 27. a scoped row whose (channel, model) pair is gone is dropped ----
#[tokio::test]
async fn t27_reconcile_clears_orphan_scoped_row() {
    let health = HealthRegistry::new(None);
    let old = base_cfg(
        vec![chan("chA", "sk-a", "http://127.0.0.1:1")],
        vec![mdl("m0", "chA", "up-0"), mdl("m9", "chA", "up-9")],
        group1(&["m0", "m9"]),
    );
    arm_key(&health, CooldownKey::scoped("chA", "up-9"), std::time::Duration::from_secs(60));

    // the binding for up-9 is removed; chA itself is unchanged
    let new = base_cfg(vec![chan("chA", "sk-a", "http://127.0.0.1:1")], vec![mdl("m0", "chA", "up-0")], group1(&["m0"]));
    let cleared = health.reconcile_config(&old.channels, &new);

    assert_eq!(cleared, 1, "orphan scoped row is cleared");
    assert!(!health.is_cooling(&CooldownKey::scoped("chA", "up-9")), "no model points at up-9 any more");
}

// ---- 28. base_url change counts as a new identity; a no-op PUT keeps state ----
#[tokio::test]
async fn t28_reconcile_base_url_change_and_noop() {
    let health = HealthRegistry::new(None);
    let old = base_cfg(
        vec![chan("chA", "sk-a", "http://127.0.0.1:1"), chan("chB", "sk-b", "http://127.0.0.1:2")],
        vec![mdl("m0", "chA", "up-0"), mdl("m1", "chB", "up-1")],
        group1(&["m0", "m1"]),
    );
    arm_key(&health, CooldownKey::scoped("chA", "up-0"), std::time::Duration::from_secs(60));
    arm_key(&health, CooldownKey::scoped("chB", "up-1"), std::time::Duration::from_secs(60));

    // (c) base_url rewritten -> that channel's rows go
    let mut new = old.clone();
    new.channels[0].base_url = "http://127.0.0.1:3".into();
    let cleared = health.reconcile_config(&old.channels, &new);
    assert_eq!(cleared, 1, "base_url change clears the channel");
    assert!(!health.is_cooling(&CooldownKey::scoped("chA", "up-0")));
    assert!(health.is_cooling(&CooldownKey::scoped("chB", "up-1")));

    // (d) a byte-identical channel list keeps every cooldown row
    let cleared = health.reconcile_config(&new.channels, &new);
    assert_eq!(cleared, 0, "a no-op PUT must not clear cooldowns");
    assert!(health.is_cooling(&CooldownKey::scoped("chB", "up-1")), "unchanged channel keeps cooling");
}
// =====================================================================
// auto_weight: the routing-order switch (user requirement #1).
// Default OFF => each tier follows the DECLARED order of tiers[].members.
// =====================================================================

// ---- 29. auto_weight=false: the declared order wins, weight/id are ignored ----
#[tokio::test]
async fn t29_auto_weight_off_uses_declared_order() {
    // u0 = GoodJson, u1 = Fail500. Members are DECLARED as [m1, m0], so the
    // declared-first candidate (m1 -> ch1 -> /u1) must be attempted FIRST and
    // fail; only then m0 is tried. The old weight-then-id sort would have put
    // m0 first and committed on attempt 1 with no failure at all.
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson, Behavior::Fail500]).await;
    let mut cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai; 2], 1, EXPOSED, test_retry());
    cfg.retry.cooldown_wait = false; // keep the test fast
    assert!(!cfg.auto_weight, "auto_weight must default to false");
    cfg.groups[0].tiers = vec![Tier {
        level: 1,
        members: vec![
            TierMember { model_id: "m1".into(), weight: 1 },
            TierMember { model_id: "m0".into(), weight: 1 },
        ],
    }];
    let (st, router) = gateway(cfg).await;
    let (status, body) = post_chat(&router, EXPOSED, false).await;
    assert_eq!(status, 200, "failover in declared order must still succeed: {body}");
    leak_scan_json(&body);
    let mut rows = st.store.list_switch_logs(100, None, None).unwrap();
    rows.reverse(); // store returns newest-first; read oldest-first
    assert_eq!(rows.len(), 2, "declared-first fails, declared-second commits: {rows:?}");
    assert_eq!(rows[0]["channel_id"], "ch1", "declared first member is attempted first: {rows:?}");
    assert_eq!(rows[0]["attempt_no"], 1);
    assert_eq!(rows[0]["tier"], 1);
    assert_eq!(rows[0]["outcome"], "failed");
    assert_eq!(rows[1]["channel_id"], "ch0", "then the declared second member: {rows:?}");
    assert_eq!(rows[1]["attempt_no"], 2);
    assert_eq!(rows[1]["outcome"], "committed");
}

// ---- 30. auto_weight=true: the same config is reordered by weight ----
#[tokio::test]
async fn t30_auto_weight_on_sorts_by_weight() {
    // Same upstreams and same DECLARED order [m0, m1], but m1 now weighs 5x.
    // With the switch on, m1 (ch1 -> /u1 = 500) is attempted first, then m0.
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson, Behavior::Fail500]).await;
    let mut cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai; 2], 1, EXPOSED, test_retry());
    cfg.retry.cooldown_wait = false;
    cfg.auto_weight = true;
    cfg.groups[0].tiers = vec![Tier {
        level: 1,
        members: vec![
            TierMember { model_id: "m0".into(), weight: 1 },
            TierMember { model_id: "m1".into(), weight: 5 },
        ],
    }];
    let (st, router) = gateway(cfg).await;
    let (status, body) = post_chat(&router, EXPOSED, false).await;
    assert_eq!(status, 200, "weighted order must still fail over correctly: {body}");
    leak_scan_json(&body);
    let mut rows = st.store.list_switch_logs(100, None, None).unwrap();
    rows.reverse();
    assert_eq!(rows.len(), 2, "heaviest first fails, the other commits: {rows:?}");
    assert_eq!(rows[0]["channel_id"], "ch1", "weight 5 outranks weight 1: {rows:?}");
    assert_eq!(rows[0]["attempt_no"], 1);
    assert_eq!(rows[1]["channel_id"], "ch0");
    assert_eq!(rows[1]["attempt_no"], 2);
}
