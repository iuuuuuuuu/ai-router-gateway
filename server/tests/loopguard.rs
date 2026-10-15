//! Loop-guard integration tests (Task B of the degenerate-output feature).
//!
//! What these tests pin down, in the user's terms: when a model falls into a
//! repetition loop the gateway notices INSIDE the pre-commit window, throws that
//! attempt away and answers from another candidate — the client receives one
//! ordinary answer and never a byte of the degenerate text (invariant I1), and
//! the external conversation is never interrupted.
//!
//! Coverage map (task-24 deliverable 4):
//!   l01      tier1 degenerate + tier2 good  -> success, attempts >= 2, I1 held
//!   l02..l04 the same through all three ENTRY protocols (openai/anthropic/gemini)
//!   l05/l06  a degenerate candidate that is the ONLY one -> honest terminal, no hang
//!   l07      ~2000 chars of normal prose -> never intercepted (false-positive control)
//!   l08/l09  loop guard off (switch / holdback 0) -> old behaviour, text passes through
//!   l10      two degenerate candidates -> loop_retries == 2 on every surface
//!   l11      /admin/api/stats carries a REAL number (never null, never absent)
//!   l12      B1 regression: text buffered before a tool call is not dropped
//!   l13      post-commit repetition is NOT detected (declared honest boundary)
//!   l14      a database created before loop_retries existed gains the column
//!
//! The degenerate fixtures always emit the same 4-character unit, so a leak can
//! be detected with one substring test instead of a heuristic.

mod support;

use gw_server::config::{Protocol, RetryConfig};
use gw_server::proxy::AppState;
use support::*;
use tower::util::ServiceExt;

/// The repeated unit every degenerate fixture emits (Behavior::DegenerateShortCycle).
const UNIT: &str = "好问题。";
/// What Behavior::GoodStream answers with (frames "he" + "llo").
const GOOD: &str = "hello";

fn retry_with(f: impl FnOnce(&mut RetryConfig)) -> RetryConfig {
    let mut rc = test_retry();
    f(&mut rc);
    rc
}

/// Fake upstreams + gateway. Every upstream speaks the OpenAI protocol; the
/// ENTRY protocol is the client's and is chosen by the driver the test calls.
async fn harness(behs: &[Behavior], tier1: usize, rc: RetryConfig) -> (AppState, axum::Router) {
    let (base, prefixes) = spawn_fake_upstreams(behs).await;
    let protos = vec![Protocol::Openai; behs.len()];
    let cfg = test_config(&base, &prefixes, &protos, tier1, EXPOSED, rc);
    gateway(cfg).await
}

/// Newest request_logs row. The streaming success branch writes its row from a
/// detached task, so this polls instead of assuming it is already there.
async fn newest_request_row(st: &AppState) -> serde_json::Value {
    for _ in 0..300 {
        if let Some(r) = st.store.list_request_logs(10, None, None).unwrap().into_iter().next() {
            return r;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("no request_logs row appeared within 6s");
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

fn switch_rows(st: &AppState) -> Vec<serde_json::Value> {
    st.store.list_switch_logs(50, None, None).unwrap()
}

// ---- 1. the core case: a degenerate tier-1 candidate, a good tier-2 one ----

#[tokio::test]
async fn l01_tier1_degenerate_falls_to_tier2_good() {
    let (st, router) = harness(&[Behavior::DegenerateShortCycle, Behavior::GoodStream], 1, test_retry()).await;
    let (status, sse) = post_chat_stream(&router, EXPOSED).await;
    assert_eq!(status, 200, "sse={sse}");

    // I1: the discarded attempt must not have leaked a single byte.
    assert!(!sse.contains(UNIT), "degenerate text reached the client: {sse}");
    assert_eq!(client_text(&sse), GOOD, "client must see exactly the good answer: {sse}");
    assert_stream_clean(&sse);

    let rows = switch_rows(&st);
    assert_eq!(rows.len(), 2, "one degenerate attempt + one committed: {rows:?}");
    // newest first: the committed attempt
    assert_eq!(rows[0]["error_class"], "committed");
    assert_eq!(rows[0]["outcome"], "committed");
    assert_eq!(rows[0]["channel_id"], "ch1");
    assert_eq!(rows[0]["http_status"], 200);
    // and the attempt the loop guard threw away
    assert_eq!(rows[1]["error_class"], "degenerate_output");
    assert_eq!(rows[1]["outcome"], "failed");
    assert_eq!(rows[1]["channel_id"], "ch0");
    assert_eq!(rows[1]["http_status"], serde_json::Value::Null);
    assert_eq!(rows[1]["message"], "degenerate output: repeated 4-char unit 15 times");

    let req = newest_request_row(&st).await;
    assert_eq!(req["status"], "success");
    assert_eq!(req["attempts"], 2);
    assert_eq!(req["loop_retries"], 1);
    assert_eq!(req["loop_retries"].as_u64(), Some(1), "must be a real number: {req}");
    assert_eq!(req["final_channel"], "ch1");
}

// ---- 2. the same, through all three entry protocols ----

#[tokio::test]
async fn l02_degenerate_is_intercepted_for_an_openai_client() {
    let (_st, router) = harness(&[Behavior::DegenerateShortCycle, Behavior::GoodStream], 1, test_retry()).await;
    let (status, sse) = post_chat_stream(&router, EXPOSED).await;
    assert_eq!(status, 200, "sse={sse}");
    assert!(!sse.contains(UNIT), "degenerate text reached the client: {sse}");
    assert!(sse.contains(GOOD), "missing the good answer: {sse}");
    assert_stream_clean(&sse);
}

#[tokio::test]
async fn l03_degenerate_is_intercepted_for_an_anthropic_client() {
    let (st, router) = harness(&[Behavior::DegenerateShortCycle, Behavior::GoodStream], 1, test_retry()).await;
    let (status, resp) = post_messages(&router, EXPOSED, true).await;
    let sse = body_string(resp).await;
    assert_eq!(status, 200, "sse={sse}");
    assert!(!sse.contains(UNIT), "degenerate text reached the client: {sse}");
    assert!(sse.contains(GOOD), "missing the good answer: {sse}");
    assert_stream_clean_generic(&sse);
    let req = newest_request_row(&st).await;
    assert_eq!(req["status"], "success");
    assert_eq!(req["attempts"], 2);
    assert_eq!(req["loop_retries"], 1);
}

#[tokio::test]
async fn l04_degenerate_is_intercepted_for_a_gemini_client() {
    let (st, router) = harness(&[Behavior::DegenerateShortCycle, Behavior::GoodStream], 1, test_retry()).await;
    let (status, resp) = post_gemini(&router, EXPOSED, true).await;
    let sse = body_string(resp).await;
    assert_eq!(status, 200, "sse={sse}");
    assert!(!sse.contains(UNIT), "degenerate text reached the client: {sse}");
    assert!(sse.contains(GOOD), "missing the good answer: {sse}");
    assert_stream_clean_generic(&sse);
    let req = newest_request_row(&st).await;
    assert_eq!(req["status"], "success");
    assert_eq!(req["attempts"], 2);
    assert_eq!(req["loop_retries"], 1);
}

// ---- 3. a degenerate candidate that is the only one: fail honestly, never hang ----

#[tokio::test]
async fn l05_single_degenerate_candidate_ends_all_upstreams_cooling() {
    // The degenerate candidate is the ONLY one: the walk has nothing left to try,
    // so the client gets one structurally-valid error and the request never
    // hangs. cooldown_wait = false keeps that terminal immediate (503) instead of
    // a 15 s sleep; max_cycles = 3 gives the walk enough cycles to rebuild its
    // plan and actually SEE the cooldown the hit armed — with the default 2 it
    // runs out of cycles first and reports the equally honest 502.
    //
    // Two attempts, not one: FAILURES_BEFORE_COOLDOWN = 2 means the FIRST failure
    // of a key is only remembered (health.rs:164), exactly as for a 5xx or an
    // empty response. That pre-existing rule is why the guard's first hit does
    // not by itself exclude a channel.
    let rc = retry_with(|r| { r.cooldown_wait = false; r.max_cycles = 3; });
    let (st, router) = harness(&[Behavior::DegenerateShortCycle], 1, rc).await;
    let (status, body) = post_chat_stream(&router, EXPOSED).await;
    assert_eq!(status, 503, "body={body}");
    assert!(body.contains("all_upstreams_cooling"), "body={body}");
    assert!(!body.contains(UNIT), "degenerate text reached the client: {body}");

    let rows = switch_rows(&st);
    assert_eq!(rows.len(), 2, "both discarded attempts must be logged: {rows:?}");
    assert!(rows.iter().all(|r| r["error_class"] == "degenerate_output"), "{rows:?}");
    assert!(rows.iter().all(|r| r["channel_id"] == "ch0"), "{rows:?}");

    let req = newest_request_row(&st).await;
    assert_eq!(req["status"], "cooldown");
    assert_eq!(req["attempts"], 2);
    assert_eq!(req["loop_retries"], 2);
    assert_eq!(req["final_channel"], serde_json::Value::Null);
    assert_eq!(req["status"], "cooldown");
}

#[tokio::test]
async fn l06_single_degenerate_candidate_exhausts_its_attempts() {
    // max_attempts = 1: the walk is out of attempts after the discarded one, so
    // the terminal is Exhausted (502) — the normal failure path, not a hang.
    // 1 * 3000 + 500 = 3500 <= 20000, so I4 still holds.
    let rc = retry_with(|r| r.max_attempts = 1);
    let (st, router) = harness(&[Behavior::DegenerateShortCycle], 1, rc).await;
    let (status, body) = post_chat_stream(&router, EXPOSED).await;
    assert_eq!(status, 502, "body={body}");
    assert!(body.contains("all_upstreams_failed"), "body={body}");
    assert!(!body.contains(UNIT), "degenerate text reached the client: {body}");

    let req = newest_request_row(&st).await;
    assert_eq!(req["status"], "exhausted");
    assert_eq!(req["attempts"], 1);
    assert_eq!(req["loop_retries"], 1);
}

// ---- 4. false-positive control: normal long text is never touched ----

#[tokio::test]
async fn l07_normal_long_text_is_not_intercepted() {
    let (st, router) = harness(&[Behavior::NormalLongText], 1, test_retry()).await;
    let (status, sse) = post_chat_stream(&router, EXPOSED).await;
    assert_eq!(status, 200, "sse={sse}");
    assert_stream_clean(&sse);
    // Byte integrity: all 2000 characters arrive, in order, none invented.
    let text = client_text(&sse);
    assert_eq!(text.chars().count(), 2000, "text lost characters: {} chars", text.chars().count());

    let req = newest_request_row(&st).await;
    assert_eq!(req["status"], "success");
    assert_eq!(req["attempts"], 1);
    assert_eq!(req["loop_retries"], 0);
    assert_eq!(req["loop_retries"].as_u64(), Some(0), "must be a real 0, never null: {req}");
    assert!(switch_rows(&st).iter().all(|r| r["error_class"] == "committed"), "unexpected failure rows");
}

// ---- 5. switch-off regression: byte-identical to the behaviour before the feature ----

#[tokio::test]
async fn l08_switch_off_lets_degenerate_text_through() {
    let rc = retry_with(|r| r.loop_guard_enabled = false);
    let (st, router) = harness(&[Behavior::DegenerateShortCycle], 1, rc).await;
    let (status, sse) = post_chat_stream(&router, EXPOSED).await;
    assert_eq!(status, 200, "sse={sse}");
    assert!(sse.contains(UNIT), "with the guard off the text must pass through: {sse}");
    assert_stream_clean(&sse);

    let req = newest_request_row(&st).await;
    assert_eq!(req["status"], "success");
    assert_eq!(req["attempts"], 1, "no retry may happen with the guard off");
    assert_eq!(req["loop_retries"], 0);
    assert!(switch_rows(&st).iter().all(|r| r["error_class"] == "committed"), "unexpected failure rows");
}

#[tokio::test]
async fn l09_zero_holdback_lets_degenerate_text_through() {
    // The holdback IS the feature's only cost (first-token delay). Forcing it to
    // 0 must therefore restore the old behaviour exactly, even with the guard
    // still enabled.
    let rc = retry_with(|r| r.loop_guard_min_chars = 0);
    let (st, router) = harness(&[Behavior::DegenerateShortCycle], 1, rc).await;
    let (status, sse) = post_chat_stream(&router, EXPOSED).await;
    assert_eq!(status, 200, "sse={sse}");
    assert!(sse.contains(UNIT), "with a 0-char holdback the text must pass through: {sse}");
    assert_stream_clean(&sse);

    let req = newest_request_row(&st).await;
    assert_eq!(req["status"], "success");
    assert_eq!(req["attempts"], 1);
    assert_eq!(req["loop_retries"], 0);
}

// ---- 6. the counter: two discarded attempts are counted twice ----

#[tokio::test]
async fn l10_two_degenerate_candidates_count_two_loop_retries() {
    // Two independent degenerate candidates, max_attempts = 2: both attempts are
    // discarded by the guard and the walk is then out of attempts, so the request
    // ends on the normal failure path with loop_retries == 2 — the number the
    // console must show, and one it can only get by counting the discarded
    // attempts rather than the committed one.
    let rc = retry_with(|r| { r.cooldown_wait = false; r.max_attempts = 2; });
    let (st, router) = harness(&[Behavior::DegenerateShortCycle, Behavior::DegenerateSingleChar], 2, rc).await;
    let (status, body) = post_chat_stream(&router, EXPOSED).await;
    assert_eq!(status, 502, "body={body}");
    assert!(body.contains("all_upstreams_failed"), "body={body}");

    let rows = switch_rows(&st);
    assert_eq!(rows.len(), 2, "both attempts must be logged: {rows:?}");
    assert!(rows.iter().all(|r| r["error_class"] == "degenerate_output"), "{rows:?}");
    // newest first: the single-character repeat was the LAST attempt
    assert_eq!(rows[0]["channel_id"], "ch1");
    assert_eq!(rows[1]["channel_id"], "ch0");

    let req = newest_request_row(&st).await;
    assert_eq!(req["status"], "exhausted");
    assert_eq!(req["attempts"], 2);
    assert_eq!(req["loop_retries"], 2);
    assert_eq!(req["loop_retries"].as_u64(), Some(2), "must be a real number: {req}");

    // A single hit per channel is deliberately NOT enough to cool a channel
    // down: the pre-existing FAILURES_BEFORE_COOLDOWN = 2 rule (health.rs:164)
    // remembers the first failure of a key and only the second one arms the
    // cooldown, exactly as for a 5xx or an empty response. The degenerate branch
    // inherits that rule rather than inventing its own, so no row is visible yet.
    let cooling = st.health.snapshot_summary();
    assert!(cooling.is_empty(), "one failure per key must not arm a cooldown: {cooling:?}");
    assert!(cooling.iter().all(|c| c.kind != "auth"), "a degenerate hit must never look like an auth failure: {cooling:?}");

    // store-level aggregate
    let summary = st.store.stats_summary(None).unwrap();
    assert_eq!(summary["loop_retries"], 2, "{summary}");
}

#[tokio::test]
async fn l11_admin_stats_exposes_a_real_loop_retries_number() {
    // Same shape as l05: ONE degenerate candidate, so the second hit arms the
    // scoped cooldown and the console has a health row to show next to the
    // counter.
    let rc = retry_with(|r| { r.cooldown_wait = false; r.max_cycles = 3; });
    let (st, router) = harness(&[Behavior::DegenerateShortCycle], 1, rc).await;
    let (status, _body) = post_chat_stream(&router, EXPOSED).await;
    assert_eq!(status, 503);
    let _ = newest_request_row(&st).await; // ensure the row is written

    // The frozen console contract: GET /admin/api/stats -> summary.loop_retries.
    // admin.key is None in the test config, so no header is required.
    let admin = gw_server::admin::admin_router(st.clone());
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/admin/api/stats")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = admin.oneshot(req).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let v = body_json(resp).await;
    let lr = &v["summary"]["loop_retries"];
    assert!(!lr.is_null(), "loop_retries must never be null: {v}");
    assert_eq!(lr.as_i64(), Some(2), "summary.loop_retries: {v}");
    assert_eq!(v["summary"]["requests"].as_i64(), Some(1), "{v}");

    // The health array keeps the frozen client contract fields.
    let health = v["health"].as_array().cloned().unwrap_or_default();
    assert!(!health.is_empty(), "a degenerate hit must be visible in health: {v}");
    for row in &health {
        for k in ["channel_id", "upstream_model", "kind", "permanent", "cooling_until_ms", "remaining_ms", "consecutive_failures"] {
            assert!(row.get(k).is_some(), "health row lost {k}: {row}");
        }
    }
}

// ---- 7. B1 regression: buffered text must survive a tool call ----

#[tokio::test]
async fn l12_buffered_text_before_a_tool_call_is_not_dropped() {
    // The holdback buffers text; a tool call arriving before the buffer is full
    // must carry that text with it, never discard it.
    let (st, router) = harness(&[Behavior::ToolAfterShortText], 1, test_retry()).await;
    let (status, sse) = post_chat_stream(&router, EXPOSED).await;
    assert_eq!(status, 200, "sse={sse}");
    assert_eq!(client_text(&sse), "thinking", "buffered text was dropped: {sse}");
    assert!(sse.contains("get_weather"), "missing the tool name: {sse}");
    assert!(sse.contains("Paris"), "missing the assembled arguments: {sse}");
    assert!(sse.contains("\"finish_reason\":\"tool_calls\""), "wrong finish reason: {sse}");
    assert_stream_clean(&sse);

    let req = newest_request_row(&st).await;
    assert_eq!(req["status"], "success");
    assert_eq!(req["attempts"], 1);
    assert_eq!(req["loop_retries"], 0);
}

// ---- 8. post-commit repetition IS detected, and the answer is truncated ----

#[tokio::test]
async fn l13_post_commit_repetition_truncates_the_answer() {
    // The answer commits on 120 characters of normal text; the loop that
    // follows must be CUT, not relayed. Before this, a model that answered
    // normally and THEN started repeating was never caught at all.
    let (st, router) = harness(&[Behavior::NormalThenDegenerate], 1, test_retry()).await;
    let (status, sse) = post_chat_stream(&router, EXPOSED).await;
    assert_eq!(status, 200, "sse={sse}");
    assert_stream_clean(&sse);

    let text = client_text(&sse);
    // The normal prefix survives: truncation must not eat real content.
    assert!(text.starts_with("abcdefghijklmnopqrstuvwxyz"), "prefix lost: {text:?}");
    // The loop is cut short. Without the guard all 40 repetitions arrive
    // (160 chars of the unit); the tail bound must stop it far sooner.
    let leaked = text.matches(UNIT).count();
    assert!(leaked < 40, "the loop was relayed in full ({leaked} repetitions): {text:?}");

    let req = newest_request_row(&st).await;
    assert_eq!(req["status"], "success", "the client still got a valid answer: {req}");
    assert_eq!(req["loop_truncated"], 1, "the cut must be recorded: {req}");
    assert_eq!(req["attempts"], 1, "no retry may happen after commit");
    assert_eq!(req["final_channel"], "ch0");
}

#[tokio::test]
async fn l13b_the_cut_really_stops_a_long_loop() {
    // 400 repetitions upstream. A guard that only bounded one frame would still
    // relay ~1600 characters of the loop, so this pins the STRONG property:
    // the stream is abandoned early.
    let (st, router) = harness(&[Behavior::NormalThenDegenerateLong], 1, test_retry()).await;
    let (status, sse) = post_chat_stream(&router, EXPOSED).await;
    assert_eq!(status, 200);
    assert_stream_clean(&sse);

    let text = client_text(&sse);
    let leaked = text.matches(UNIT).count();
    assert!(leaked < 100, "only a bounded prefix of the loop may escape, got {leaked}");

    let req = newest_request_row(&st).await;
    assert_eq!(req["loop_truncated"], 1, "{req}");
}

#[tokio::test]
async fn l13c_a_normal_committed_answer_is_never_truncated() {
    // False-positive control for the POST-COMMIT path. A normal continuation
    // after commit must survive untouched — cutting a healthy answer short is
    // worse than the loop this feature exists to stop.
    let (st, router) = harness(&[Behavior::NormalOnlyStream], 1, test_retry()).await;
    let (status, sse) = post_chat_stream(&router, EXPOSED).await;
    assert_eq!(status, 200);
    assert_stream_clean(&sse);

    // 480 characters of ordinary, non-repeating prose: nothing may be dropped.
    assert_eq!(client_text(&sse).chars().count(), 480, "a healthy answer was cut: {sse}");
    // And it really is the whole answer, not a cut that happens to land on 480.
    assert!(sse.contains("\"finish_reason\":\"stop\""), "the answer must end normally: {sse}");

    let req = newest_request_row(&st).await;
    assert_eq!(req["loop_truncated"], 0, "no truncation on a healthy answer: {req}");
}

#[tokio::test]
async fn l13d_truncation_can_be_disabled() {
    // The switch governs BOTH windows: turning the guard off must restore the
    // old pass-through behaviour for a mid-stream loop, not just the
    // pre-commit one.
    let (st, router) = harness(&[Behavior::NormalThenDegenerate], 1,
        retry_with(|r| { r.loop_guard_enabled = false; })).await;
    let (status, sse) = post_chat_stream(&router, EXPOSED).await;
    assert_eq!(status, 200);
    assert_stream_clean(&sse);
    assert_eq!(client_text(&sse).matches(UNIT).count(), 40, "with the guard off the loop must pass through");

    let req = newest_request_row(&st).await;
    assert_eq!(req["loop_truncated"], 0, "{req}");
}

// ---- 9. migration of a database created before loop_retries existed ----

#[tokio::test]
async fn l14_legacy_database_gains_the_loop_retries_column() {
    let dir = std::env::temp_dir().join(format!("gw-loopguard-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("legacy.db");
    let _ = std::fs::remove_file(&path);
    {
        // A request_logs table shaped exactly like the pre-feature one.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE request_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts TEXT NOT NULL, request_id TEXT NOT NULL, group_id TEXT NOT NULL DEFAULT '',
                exposed_model TEXT NOT NULL, entry_protocol TEXT NOT NULL, status TEXT NOT NULL,
                final_channel TEXT, final_upstream_model TEXT, attempts INTEGER NOT NULL DEFAULT 0,
                input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0, cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                cost_usd REAL NOT NULL DEFAULT 0, wasted_cost_usd REAL NOT NULL DEFAULT 0,
                duration_ms INTEGER NOT NULL DEFAULT 0, ttft_ms INTEGER NOT NULL DEFAULT 0);
             INSERT INTO request_logs (ts, request_id, group_id, exposed_model, entry_protocol, status)
                VALUES ('2026-01-01T00:00:00Z', 'old-req', 'g1', 'my-model', 'openai', 'success');",
        ).unwrap();
    }

    let store = gw_server::store::Store::open(path.to_str().unwrap()).unwrap();
    let rows = store.list_request_logs(10, None, None).unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    // The historical row reads as "no loop retries", as a REAL number.
    assert_eq!(rows[0]["loop_retries"], 0, "{rows:?}");
    assert_eq!(rows[0]["loop_retries"].as_u64(), Some(0), "{rows:?}");
    assert_eq!(store.stats_summary(None).unwrap()["loop_retries"], 0);

    // And a freshly written row lands in the backfilled column.
    store.insert_request_log(&gw_server::store::RequestLog {
        id: None, ts: "2026-01-01T00:00:01Z".into(), request_id: "new-req".into(), group_id: "g1".into(),
        exposed_model: "my-model".into(), entry_protocol: "openai".into(), status: "success".into(),
        final_channel: Some("ch0".into()), final_upstream_model: Some("up-model-0".into()),
        attempts: 2, loop_retries: 3, loop_truncated: 1,
        input_tokens: 1, output_tokens: 2, cache_read_tokens: 0, cache_creation_tokens: 0,
        cost_usd: 0.0, wasted_cost_usd: 0.0, duration_ms: 5, ttft_ms: 5,
    }).unwrap();
    let rows = store.list_request_logs(10, None, None).unwrap();
    assert_eq!(rows[0]["loop_retries"], 3, "{rows:?}");
    assert_eq!(rows[0]["attempts"], 2, "{rows:?}");
    assert_eq!(rows[0]["loop_truncated"], 1, "the truncation flag must round-trip: {rows:?}");
    assert_eq!(store.stats_summary(None).unwrap()["loop_retries"], 3);
    assert_eq!(store.stats_summary(None).unwrap()["loop_truncated"], 1);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(dir.join("legacy.db-wal"));
    let _ = std::fs::remove_file(dir.join("legacy.db-shm"));
    let _ = std::fs::remove_dir(&dir);
}
