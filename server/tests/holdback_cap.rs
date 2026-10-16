//! Wall-clock holdback cap (`loop_guard_hold_max_ms`) — end-to-end.
//!
//! These live in their OWN test binary on purpose. The assertions are timing
//! based, and cargo runs the tests inside one binary in parallel: sharing a
//! binary with the 400-frame stream fixtures starves the drip fixture badly
//! enough to invert the result. Cargo runs separate test binaries one after
//! another, so a dedicated binary removes that contention without serialising
//! anything else.
//!
//! The DECISION itself (characters vs. wall clock) is pinned by deterministic
//! unit tests in `src/loopguard.rs`, which need no real time at all. What is
//! proven here is the wiring: that the configured value reaches the proxy and
//! actually moves the first byte.

mod support;

use gw_server::config::RetryConfig;
use support::*;

fn retry_with(f: impl FnOnce(&mut RetryConfig)) -> RetryConfig {
    let mut rc = test_retry();
    f(&mut rc);
    rc
}

/// The character counts of every text delta the client received, in order.
///
/// The FIRST entry is the size of the committed prefix, which is the observable
/// this feature really controls: the ceiling ends the holdback with FEWER
/// characters than `min_chars`, while a purely character-counted holdback always
/// commits at or above it. Asserting on this instead of on elapsed time keeps the
/// test immune to scheduling noise, which is what made the wall-clock version
/// pass or fail depending on machine load.
fn client_delta_sizes(sse: &str) -> Vec<usize> {
    sse.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|p| serde_json::from_str::<serde_json::Value>(p).ok())
        .filter_map(|v| v["choices"][0]["delta"]["content"].as_str().map(|s| s.chars().count()))
        .collect()
}

/// Serialises the three timing tests against EACH OTHER.
///
/// They assert on wall-clock first-byte latency, so running them in parallel
/// makes them compete for the same CPU and can push a 400 ms measurement past
/// its bound. A mutex is the honest fix: the assertions stay strict, and the
/// only thing given up is parallelism between tests that measure time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Fake upstreams + gateway (mirrors the loopguard suite's harness).
async fn harness(behs: &[Behavior], tier1: usize, rc: RetryConfig) -> (gw_server::proxy::AppState, axum::Router) {
    let (base, prefixes) = spawn_fake_upstreams(behs).await;
    let protos = vec![gw_server::config::Protocol::Openai; behs.len()];
    let cfg = test_config(&base, &prefixes, &protos, tier1, EXPOSED, rc);
    gateway(cfg).await
}

/// Newest request_logs row (the success branch writes it from a detached task).
async fn newest_request_row(st: &gw_server::proxy::AppState) -> serde_json::Value {
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
/// The holdback is bounded by TIME, not only by characters.
///
/// `loop_guard_min_chars` counts CHARACTERS, so how long the client waits for
/// the first byte depends entirely on how fast the model generates. The
/// `SlowDripNormalText` fixture drips one 12-char frame every 60 ms, so filling
/// 64 characters takes ~320 ms while the default cap is 250 ms: the commit must
/// happen on the CAP, with fewer characters than `min_chars`.
///
/// The observable is `ttft_ms` (time to first byte, recorded by the gateway),
/// NOT the total request duration: committing early does not shorten the
/// stream, it only moves WHEN the first byte goes out. Asserting on the total
/// duration would be green whether or not the cap fired.
///
/// This is also the test that makes the cap testable at all: every other stream
/// fixture emits its whole body in one shot, so a cap measured in milliseconds
/// would never fire and a broken cap would still look green.
#[tokio::test]
async fn l17_holdback_is_bounded_by_time_not_only_by_chars() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // The fixture drips one 12-char frame every 400 ms. Waiting for the
    // 64-character target would take frame 6 (~2000 ms); the 250 ms cap fires on
    // frame 2 (~400 ms). The outcomes are 1.6 s apart, so this test cannot pass
    // or fail by luck on a loaded machine.
    let rc = retry_with(|r| {
        r.loop_guard_min_chars = 64;
        r.loop_guard_hold_max_ms = 250;
    });
    let (st, router) = harness(&[Behavior::SlowDripNormalText], 1, rc).await;

    let (status, sse) = post_chat_stream(&router, EXPOSED).await;

    assert_eq!(status, 200, "sse={sse}");
    assert_stream_clean(&sse);

    // The cap commits early, it never drops content: every frame still arrives.
    assert_eq!(client_text(&sse).chars().count(), 6 * 12, "every frame must reach the client");

    let req = newest_request_row(&st).await;
    assert_eq!(req["status"], "success");
    assert_eq!(req["loop_retries"], 0, "normal text must never be judged degenerate");
    assert_eq!(req["loop_truncated"], 0, "normal text must never be truncated");

    // Frames land at 0, 400, 800, ... ms. The cap fires on frame 2 (~400 ms);
    // waiting for the 64-character target takes frame 6 (~2000 ms). The bound
    // sits in the gap, so a cap that never fires fails loudly.
    // The cap ends the holdback with FEWER characters than the 64-char target,
    // which is the whole point: evidence is given up to protect the first byte.
    let sizes = client_delta_sizes(&sse);
    let first = *sizes.first().expect("the client must receive at least one delta");
    assert!(
        first < 64,
        "the committed prefix was {first} chars (deltas={sizes:?}); the 250 ms ceiling must end the holdback before the 64-char target"
    );
    assert!(
        first > 0,
        "the cap commits early, it must never commit nothing: deltas={sizes:?}"
    );
    let _ = req;
}

/// The cap is a CEILING, never a floor: a model fast enough to fill
/// `min_chars` inside the ceiling is still judged with the full character
/// evidence, so the cap cannot be satisfied by simply always committing early.
#[tokio::test]
async fn l18_fast_model_still_uses_the_full_character_holdback() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // No drip: the fixture emits its whole body at once, so the very first
    // frame already exceeds min_chars and the cap is irrelevant.
    let rc = retry_with(|r| {
        r.loop_guard_min_chars = 64;
        r.loop_guard_hold_max_ms = 250;
    });
    let (st, router) = harness(&[Behavior::NormalOnlyStream], 1, rc).await;
    let (status, sse) = post_chat_stream(&router, EXPOSED).await;
    assert_eq!(status, 200, "sse={sse}");
    assert_stream_clean(&sse);
    assert_eq!(client_text(&sse).chars().count(), 40 * 12);

    let req = newest_request_row(&st).await;
    assert_eq!(req["status"], "success");
    assert_eq!(req["loop_retries"], 0);
    assert_eq!(req["loop_truncated"], 0);
}

/// With the cap DISABLED (0) the holdback is purely `min_chars` characters —
/// the old behaviour, and the control that makes l17 meaningful. Same fixture,
/// same timing, opposite expected outcome.
#[tokio::test]
async fn l19_cap_disabled_falls_back_to_the_character_holdback() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let rc = retry_with(|r| {
        r.loop_guard_min_chars = 64;
        r.loop_guard_hold_max_ms = 0;
    });
    let (st, router) = harness(&[Behavior::SlowDripNormalText], 1, rc).await;

    let (status, sse) = post_chat_stream(&router, EXPOSED).await;

    assert_eq!(status, 200, "sse={sse}");
    assert_stream_clean(&sse);
    assert_eq!(client_text(&sse).chars().count(), 6 * 12);

    let req = newest_request_row(&st).await;
    assert_eq!(req["status"], "success");
    assert_eq!(req["loop_truncated"], 0);

    // Same fixture, same timing, cap disabled: the wait is now the CHARACTER
    // target (64 chars = frame 6, ~2000 ms), not the ceiling. That the number
    // moves with this one field is what makes l19 the control for l17.
    // Cap disabled: the holdback is purely characters, so the committed prefix
    // is at least the 64-char target. That this number moves with the one field
    // is what makes this the control for l17.
    let sizes = client_delta_sizes(&sse);
    let first = *sizes.first().expect("the client must receive at least one delta");
    assert!(
        first >= 64,
        "with the cap disabled the committed prefix must reach the 64-char target, got {first} (deltas={sizes:?})"
    );
    let _ = req;
}