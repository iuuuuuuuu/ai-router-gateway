//! In-flight occupancy: the "使用中" badge's only honest source.
//!
//! The badge answers one question — "is this upstream account being used right
//! now, and by how many requests?" — so these tests pin the two ways a naive
//! implementation gets it wrong:
//!   * it must be 0 when nothing is running, even if a success was just logged
//!     (that is the last-success reading we replaced);
//!   * it must stay N for the WHOLE life of a committed stream, not just until
//!     the response headers arrive, and must come back to 0 when the pump ends.

mod support;

use gw_server::router::build_plan;
use support::{gateway, spawn_fake_upstreams, test_config, test_retry, Behavior};
use tower::util::ServiceExt;

const EXPOSED: &str = "my-model";

/// The key format every surface shares: "<channel_id>::<upstream_model>".
fn key(channel: &str, upstream: &str) -> String {
    format!("{channel}::{upstream}")
}

async fn get_json(router: &axum::Router, uri: &str) -> (u16, serde_json::Value) {
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", "Bearer gw-test-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap().to_vec();
    (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

async fn post_json(router: &axum::Router, uri: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", "Bearer gw-test-key")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap().to_vec();
    (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

/// Wait until the registry reports `want` total, or give up after ~2s.
async fn wait_total(state: &gw_server::proxy::AppState, want: u64) -> u64 {
    for _ in 0..200 {
        let n = state.inflight.total();
        if n == want { return n }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    state.inflight.total()
}

#[tokio::test]
async fn idle_registry_is_zero_and_reports_no_keys() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    let (state, router) = gateway(cfg).await;

    let (status, v) = get_json(&router, "/admin/api/inflight").await;
    assert_eq!(status, 200);
    assert_eq!(v["total"], 0);
    // Absent means 0: an idle registry must not advertise keys at all, or the
    // console would have to tell "missing" from "present but zero".
    assert_eq!(v["members"], serde_json::json!({}));
    assert_eq!(v["channels"], serde_json::json!({}));
    // The configured group is still listed, with a real 0.
    assert_eq!(v["groups"]["g1"], 0);
    assert_eq!(state.inflight.total(), 0);
}

#[tokio::test]
async fn a_finished_request_leaves_the_registry_empty() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    let (state, router) = gateway(cfg).await;

    let (status, body) = support::post_chat(&router, EXPOSED, false).await;
    assert_eq!(status, 200);
    assert_eq!(body["choices"][0]["message"]["content"], "hello from json");

    // The request landed, but nothing is running any more. A last-success
    // reading would still be lit here; in-flight must not be.
    assert_eq!(state.inflight.total(), 0);
    let (_, v) = get_json(&router, "/admin/api/inflight").await;
    assert_eq!(v["total"], 0);
    assert_eq!(v["members"], serde_json::json!({}));
}

#[tokio::test]
async fn a_slow_request_is_visible_while_it_is_running() {
    // 400ms of upstream think time: long enough to observe the counter, short
    // enough that the test stays fast.
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::SlowJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    let (state, router) = gateway(cfg).await;

    let runner = {
        let router = router.clone();
        tokio::spawn(async move { support::post_chat(&router, EXPOSED, false).await })
    };

    let n = wait_total(&state, 1).await;
    assert_eq!(n, 1, "the in-flight request must be visible while the upstream is thinking");
    assert_eq!(state.inflight.member_count(&key("ch0", "up-model-0")), 1);

    let (_, v) = get_json(&router, "/admin/api/inflight").await;
    assert_eq!(v["total"], 1);
    assert_eq!(v["members"][key("ch0", "up-model-0")], 1);
    assert_eq!(v["channels"]["ch0"], 1);
    assert_eq!(v["groups"]["g1"], 1);

    let (status, _) = runner.await.unwrap();
    assert_eq!(status, 200);
    assert_eq!(wait_total(&state, 0).await, 0, "occupancy must be released when the request ends");
}

#[tokio::test]
async fn concurrent_requests_stack_on_one_account() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::SlowJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    let (state, router) = gateway(cfg).await;

    let mut runners = vec![];
    for _ in 0..3 {
        let router = router.clone();
        runners.push(tokio::spawn(async move { support::post_chat(&router, EXPOSED, false).await }));
    }

    // This is the number the badge shows: THREE requests are using the same
    // account at once.
    assert_eq!(wait_total(&state, 3).await, 3);
    let (_, v) = get_json(&router, "/admin/api/inflight").await;
    assert_eq!(v["total"], 3);
    assert_eq!(v["members"][key("ch0", "up-model-0")], 3);
    assert_eq!(v["channels"]["ch0"], 3);
    assert_eq!(v["groups"]["g1"], 3);

    for r in runners {
        assert_eq!(r.await.unwrap().0, 200);
    }
    assert_eq!(wait_total(&state, 0).await, 0);
}

#[tokio::test]
async fn a_committed_stream_occupies_until_the_pump_ends() {
    // StreamSlow commits early, then keeps writing for a while: the interesting
    // window is AFTER the headers, where a naive implementation would already
    // have released the account.
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::StreamSlow]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    let (state, router) = gateway(cfg).await;

    let runner = {
        let router = router.clone();
        tokio::spawn(async move { support::post_chat_stream(&router, EXPOSED).await })
    };

    assert_eq!(wait_total(&state, 1).await, 1, "a streaming request occupies the account too");
    assert_eq!(state.inflight.member_count(&key("ch0", "up-model-0")), 1);

    let (status, sse) = runner.await.unwrap();
    assert_eq!(status, 200);
    assert!(sse.contains("[DONE]"), "the stream must finish cleanly: {sse}");
    assert_eq!(wait_total(&state, 0).await, 0, "the pump ending must release the account");
}

#[tokio::test]
async fn a_failed_attempt_releases_before_the_retry_takes_the_next_account() {
    // tier1 = ch0 (500), tier2 = ch1 (good). The failure path must release ch0,
    // otherwise a long outage would slowly leak occupancy on a dead account.
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::Fail500, Behavior::GoodJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai, gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    let (state, router) = gateway(cfg).await;

    let (status, body) = support::post_chat(&router, EXPOSED, false).await;
    assert_eq!(status, 200);
    assert_eq!(body["choices"][0]["message"]["content"], "hello from json");

    assert_eq!(state.inflight.total(), 0);
    assert_eq!(state.inflight.member_count(&key("ch0", "up-model-0")), 0, "the failed account must not stay occupied");
    assert_eq!(state.inflight.member_count(&key("ch1", "up-model-1")), 0);
}

#[tokio::test]
async fn group_members_reports_live_inflight_next_to_last_success() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::SlowJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    let (state, router) = gateway(cfg).await;

    let runner = {
        let router = router.clone();
        tokio::spawn(async move { support::post_chat(&router, EXPOSED, false).await })
    };
    assert_eq!(wait_total(&state, 1).await, 1);

    let (status, v) = get_json(&router, "/admin/api/group-members?group_id=g1").await;
    assert_eq!(status, 200);
    // The two readings coexist and mean different things: `inflight` is now,
    // `active` is where the last success landed (nothing has landed yet here,
    // so it is false — proving they are not the same field).
    assert_eq!(v["inflight_total"], 1);
    assert_eq!(v["members"][0]["inflight"], 1);
    assert_eq!(v["members"][0]["active"], false);
    assert_eq!(v["active_key"], serde_json::Value::Null);

    runner.await.unwrap();
    assert_eq!(wait_total(&state, 0).await, 0);
    let (_, v) = get_json(&router, "/admin/api/group-members?group_id=g1").await;
    assert_eq!(v["inflight_total"], 0);
    assert_eq!(v["members"][0]["inflight"], 0);
    // And after it lands, the last-success reading flips on while in-flight
    // goes back to 0 — the exact pair the old badge conflated.
    assert_eq!(v["members"][0]["active"], true);
    assert_eq!(v["active_key"], key("ch0", "up-model-0"));
}

#[tokio::test]
async fn purge_clears_logs_and_cooling_but_never_the_config() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::Fail500, Behavior::Fail500]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai, gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    let (state, router) = gateway(cfg).await;

    // Produce real rows: a request that exhausts both candidates logs a
    // request_log plus switch_log rows, and cools both accounts down.
    let (status, _) = support::post_chat(&router, EXPOSED, false).await;
    assert_eq!(status, 502);
    assert!(!state.store.list_request_logs(50, None, None).unwrap().is_empty(), "the failed request must have been logged");
    assert!(!state.store.list_switch_logs(50, None, None).unwrap().is_empty());
    // Cooling is kept in memory and written to the table by an explicit
    // persist (config save / admin action), exactly like production.
    state.health.persist();
    assert!(!state.store.load_cooldowns().unwrap().is_empty(), "an explicit persist must have written the cooldown rows");
    assert!(!state.health.snapshot_summary().is_empty(), "the accounts must actually be cooling in memory");

    // Config is on disk (gateway() publishes to the holder; save it explicitly
    // so we can prove the purge does not touch the row).
    let before = state.config.get().unwrap().config.as_ref().clone();
    state.store.save_config(&before).unwrap();

    let (status, v) = post_json(&router, "/admin/api/purge-logs", serde_json::json!({
        "request_logs": true, "switch_logs": true, "cooldowns": true
    })).await;
    assert_eq!(status, 200);
    assert_eq!(v["ok"], true);
    assert!(v["deleted"]["request_logs"].as_i64().unwrap() > 0);
    assert!(v["deleted"]["switch_logs"].as_i64().unwrap() > 0);
    assert!(v["deleted"]["cooldowns"].as_i64().unwrap() > 0);

    assert!(state.store.list_request_logs(50, None, None).unwrap().is_empty());
    assert!(state.store.list_switch_logs(50, None, None).unwrap().is_empty());
    assert!(state.store.load_cooldowns().unwrap().is_empty());
    // The in-memory cooling map is cleared too, not just the table.
    assert!(state.health.snapshot_summary().is_empty());

    // ...and the configuration survived, byte for byte.
    let after = state.store.load_config().unwrap().expect("config row must survive a purge");
    assert_eq!(serde_json::to_value(&after).unwrap(), serde_json::to_value(&before).unwrap());
    assert_eq!(after.channels.len(), 2);
    assert_eq!(after.groups.len(), 1);
    assert_eq!(after.api_keys.len(), 1);

    // Purge is selective: asking for nothing deletes nothing.
    let (_, v) = post_json(&router, "/admin/api/purge-logs", serde_json::json!({})).await;
    assert_eq!(v["deleted"]["request_logs"], 0);
    assert_eq!(v["deleted"]["switch_logs"], 0);
    assert_eq!(v["deleted"]["cooldowns"], 0);
}

#[tokio::test]
async fn plan_keys_match_the_inflight_key_format() {
    // The console joins the two by string. If build_plan's candidate identity
    // ever drifts from inflight::member_key, the badge silently stops lighting
    // up, so pin the agreement here.
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson]).await;
    let cfg = test_config(&base, &prefixes, &[gw_server::config::Protocol::Openai], 1, EXPOSED, test_retry());
    let (state, _router) = gateway(cfg).await;
    let snap = state.config.get().unwrap();
    let group = snap.config.groups.iter().find(|g| g.id == "g1").unwrap();
    let plan = build_plan(&snap.config, group, EXPOSED, &state.health);
    assert_eq!(plan.runnable.len(), 1);
    let c = &plan.runnable[0];
    assert_eq!(gw_server::inflight::member_key(&c.channel_id, &c.upstream_model), key("ch0", "up-model-0"));
}
