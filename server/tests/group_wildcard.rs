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
            loop_truncated: 0,
            loop_continues: 0,
            reasoning_truncated: 0,
            error_class: String::new(),
            error_message: String::new(),
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

/// One request row with an explicit CLIENT-FACING model name, so a test can
/// put a group name/id into `exposed_model` exactly the way the wildcard path
/// does when a client asks for the group instead of one of its models.
fn insert_request_as(
    store: &Store,
    group: &str,
    exposed: &str,
    status: &str,
    fin: Option<(&str, &str)>,
    seq: usize,
) {
    store
        .insert_request_log(&RequestLog {
            id: None,
            ts: ts_for(seq),
            request_id: format!("req-{seq}"),
            group_id: group.into(),
            exposed_model: exposed.into(),
            entry_protocol: "openai".into(),
            status: status.into(),
            final_channel: fin.map(|f| f.0.to_string()),
            final_upstream_model: fin.map(|f| f.1.to_string()),
            attempts: 1,
            loop_retries: 0,
            loop_truncated: 0,
            loop_continues: 0,
            reasoning_truncated: 0,
            error_class: String::new(),
            error_message: String::new(),
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            cost_usd: 0.001,
            wasted_cost_usd: 0.0,
            duration_ms: 100,
            ttft_ms: 10,
        })
        .unwrap();
}

/// w06 — 模型统计必须按「真实路由到的模型」聚合，绝不能把分组名当成模型。
///
/// The defect this pins down (owner-reported, seen in 分组详情·主力分组·用量):
/// a group's name and id are themselves callable model names, so clients that
/// ask for the group put "grp_main" / "主力分组" into `exposed_model`. The old
/// `stats_by_model` grouped by that column, so ONE group's traffic was split
/// across alias rows and the console charted a GROUP NAME as if it were a
/// model. Measured on the live database: 43 of grp_main's 68 requests were in
/// such rows, and `by_group` claimed 4 models for a group with 3 exposed ones.
///
/// Every assertion below FAILS on the old SQL:
///   * the alias rows are absent (they used to be the whole point)
///   * the alias traffic is MERGED into its routed model (old: separate row)
///   * `routed` is present on every row (old: the key did not exist)
///   * row sums still equal the group total, failures included
///   * `by_group.models` counts routed pairs, not request aliases
#[tokio::test]
async fn w06_stats_are_keyed_on_the_routed_model_not_the_group_alias() {
    let store = Store::open_memory().unwrap();

    // One group whose clients asked for it by NAME, by ID, and by its real
    // exposed model — all three land on the SAME routed pair.
    insert_request_as(&store, GROUP_ID, EXPOSED, "success", Some(("ch0", "up-model-0")), 1);
    insert_request_as(&store, GROUP_ID, GROUP_NAME, "success", Some(("ch0", "up-model-0")), 2);
    insert_request_as(&store, GROUP_ID, GROUP_ID, "success", Some(("ch0", "up-model-0")), 3);
    // A second routed model on another channel: it must stay its OWN row.
    insert_request_as(&store, GROUP_ID, GROUP_NAME, "success", Some(("ch2", "up-model-2")), 4);
    // And a request that never reached any upstream (all candidates failed).
    insert_request_as(&store, GROUP_ID, GROUP_NAME, "exhausted", None, 5);
    // Another group must never leak into a scoped call.
    insert_request_as(&store, "gB", EXPOSED, "success", Some(("chZ", "upZ")), 6);

    let rows = store.stats_by_model(Some(GROUP_ID), None).unwrap();

    // (1) The group alias must NOT be a model identity any more. This is the
    //     assertion the old code failed hardest: it produced 3 alias rows here.
    assert!(
        rows.iter().all(|r| r.get("exposed_model").is_none()),
        "stats_by_model must not hand the console a request alias to chart as a model: {rows:?}"
    );

    // (2) Every row states whether it was routed, so the console can label the
    //     unrouted bucket honestly instead of inventing a model name for it.
    for r in &rows {
        assert!(r.get("routed").is_some(), "every row carries `routed`: {r}");
    }

    // (3) The three alias requests MERGE into their one routed pair (3 requests,
    //     not three rows). Old SQL: three rows of 1 request each.
    let a = rows.iter().find(|r| r["channel_id"] == "ch0").expect("routed ch0 row");
    assert_eq!(a["upstream_model"].as_str(), Some("up-model-0"), "{a}");
    assert_eq!(a["routed"].as_i64(), Some(1), "{a}");
    assert_eq!(a["requests"].as_i64(), Some(3), "all three aliases collapse into the routed pair: {rows:?}");

    // (4) A different routed pair stays a different row.
    let b = rows.iter().find(|r| r["channel_id"] == "ch2").expect("routed ch2 row");
    assert_eq!(b["requests"].as_i64(), Some(1), "{b}");

    // (5) The never-routed request is ONE row flagged routed=0 with a null
    //     identity — never a model name, and never silently dropped, so the
    //     row sums still reconcile with the group total.
    let unrouted = rows.iter().find(|r| r["routed"].as_i64() == Some(0)).expect("unrouted row present");
    assert!(unrouted["channel_id"].is_null(), "an unrouted row has no channel: {unrouted}");
    assert!(unrouted["upstream_model"].is_null(), "an unrouted row has no model: {unrouted}");
    assert_eq!(unrouted["requests"].as_i64(), Some(1), "{unrouted}");

    let total: i64 = rows.iter().map(|r| r["requests"].as_i64().unwrap()).sum();
    assert_eq!(total, 5, "row sums must still equal the group's request total (4 routed + 1 not): {rows:?}");
    assert_eq!(rows.len(), 3, "exactly 2 routed pairs + 1 unrouted bucket: {rows:?}");

    // (6) by_group's 模型数 must count ROUTED pairs, not request aliases. The
    //     old COUNT(DISTINCT exposed_model) said 3 here for 2 real models.
    let by_group = store.stats_by_group(Some(GROUP_ID), None).unwrap();
    let row = &by_group[0];
    assert_eq!(row["group_id"].as_str(), Some(GROUP_ID), "{row}");
    assert_eq!(row["requests"].as_i64(), Some(5), "{row}");
    assert_eq!(
        row["models"].as_i64(),
        Some(2),
        "模型数 = distinct ROUTED pairs (ch0::up-model-0, ch2::up-model-2); the unrouted bucket is not a model: {row}"
    );

    // (7) The global scope aggregates the same way and keeps groups apart.
    let all = store.stats_by_model(None, None).unwrap();
    let chz = all.iter().find(|r| r["channel_id"] == "chZ").expect("gB row present globally");
    assert_eq!(chz["requests"].as_i64(), Some(1), "{chz}");
    assert_eq!(all.iter().filter(|r| r["channel_id"] == "ch0").count(), 1, "no duplicate ch0 rows: {all:?}");
}
/// w07 — 失败请求必须按「渠道 - 具体模型」显示出来，不能被藏进一个笼统的桶里。
///
/// Owner 的要求（原话）：「失败也要显示出来，渠道 - 具体模型」，并且裁决了多候选
/// 失败**按每次尝试展开**、0 次尝试的失败**单独一行**。真机库实测 831 次请求里
/// 498 次没路由成功，其中 385 次一次都没试（77%）—— 后者没有渠道也没有模型。
///
/// 每条断言在旧实现下都会失败：旧 SQL 只有 routed/unrouted 两态，失败请求要么
/// 全部挤进一行（丢掉「在哪个渠道×模型上失败」），要么根本没有行。
#[tokio::test]
async fn w07_failed_requests_are_shown_per_attempted_channel_and_model() {
    let store = Store::open_memory().unwrap();

    // (A) 一次失败请求，试了 2 个候选，两个都失败 —— 必须出现 2 行 attempted。
    insert_request_as(&store, GROUP_ID, EXPOSED, "exhausted", None, 1);
    insert_switch_as(&store, GROUP_ID, "req-1", "chA", "up-a", "auth_error");
    insert_switch_as(&store, GROUP_ID, "req-1", "chB", "up-b", "rate_limited");

    // (B) 同一个候选被重试两次 —— 仍只算 1 行（按 request×pair 去重），不重复计数。
    insert_request_as(&store, GROUP_ID, EXPOSED, "exhausted", None, 2);
    insert_switch_as(&store, GROUP_ID, "req-2", "chA", "up-a", "http_500");
    insert_switch_as(&store, GROUP_ID, "req-2", "chA", "up-a", "http_500");

    // (C) 一次都没试的失败（全冷却）—— 没有渠道也没有模型，单独一行 unrouted。
    insert_request_as(&store, GROUP_ID, EXPOSED, "cooldown", None, 3);

    // (D) 成功请求照旧一行 routed，且带成本。
    insert_request_as(&store, GROUP_ID, EXPOSED, "success", Some(("chC", "up-c")), 4);

    let rows = store.stats_by_model(Some(GROUP_ID), None).unwrap();
    let kind_of = |ch: &str, up: &str| -> Option<String> {
        rows.iter()
            .find(|r| r["channel_id"] == ch && r["upstream_model"] == up)
            .and_then(|r| r["kind"].as_str().map(|s| s.to_string()))
    };

    // 失败的候选各自成行，并标明是「试过但失败」。
    assert_eq!(kind_of("chA", "up-a").as_deref(), Some("attempted"), "{rows:?}");
    assert_eq!(kind_of("chB", "up-b").as_deref(), Some("attempted"), "{rows:?}");
    assert_eq!(kind_of("chC", "up-c").as_deref(), Some("routed"), "{rows:?}");

    // (A) 两个候选各 1 行；重试的同一 pair 不重复计数；加上 unrouted 与 routed。
    let a = rows.iter().find(|r| r["channel_id"] == "chA").unwrap();
    assert_eq!(a["requests"].as_i64(), Some(2), "chA 被两次请求试过（req-1 与 req-2 各一次）: {a}");
    let b = rows.iter().find(|r| r["channel_id"] == "chB").unwrap();
    assert_eq!(b["requests"].as_i64(), Some(1), "{b}");

    // unrouted 行没有渠道、没有模型，不冒充任何模型名。
    let un = rows.iter().find(|r| r["kind"] == "unrouted").expect("unrouted 行必须在");
    assert!(un["channel_id"].is_null(), "unrouted 行没有渠道: {un}");
    assert!(un["upstream_model"].is_null(), "unrouted 行没有模型: {un}");
    assert_eq!(un["routed"].as_i64(), Some(0), "{un}");

    // ★ 成本只记在 routed 行：失败尝试的 token 恒为 0，否则会与分组总额重复计价。
    for r in rows.iter().filter(|r| r["kind"] != "routed") {
        assert_eq!(r["input_tokens"].as_i64(), Some(0), "失败行不得带 token: {r}");
        assert_eq!(r["cost_usd"].as_f64(), Some(0.0), "失败行不得带成本: {r}");
    }

    // ★ 分组总额按**请求**计一次，不按**尝试**计 —— 这正是「展开只发生在模型维度，
    //    不得回流到分组维度」的判据。insert_request_as 给每次请求 10 input / 5 output，
    //    本测试共 4 次请求、5 条 switch_log 尝试（req-1 有 2 条）。
    //    按尝试计会得到 50；正确值是 40。
    let by_group = store.stats_by_group(Some(GROUP_ID), None).unwrap();
    assert_eq!(
        by_group[0]["input_tokens"].as_i64(),
        Some(40),
        "分组总额 = 4 次请求 × 10，绝不能按 5 条尝试去计（那会得到 50）: {:?}",
        by_group[0]
    );
    assert_eq!(by_group[0]["requests"].as_i64(), Some(4), "{:?}", by_group[0]);

    // 三个失败请求各自可见：2 个 attempted 行 + 1 个 unrouted 行，另加 1 个 routed 行。
    assert_eq!(rows.len(), 4, "2 attempted + 1 unrouted + 1 routed: {rows:?}");
}

/// w07 的反向对照：**按每次尝试展开**是刻意的 —— 行请求数之和**允许**大于请求总数。
/// 这条断言把该语义钉住，防止将来有人为了「和等于总数」把失败行去重掉。
#[tokio::test]
async fn w07b_per_attempt_expansion_may_exceed_the_request_total_by_design() {
    let store = Store::open_memory().unwrap();

    // 一次请求试 3 个候选全失败 → 3 行 attempted。
    insert_request_as(&store, GROUP_ID, EXPOSED, "exhausted", None, 1);
    insert_switch_as(&store, GROUP_ID, "req-1", "chA", "up-a", "auth_error");
    insert_switch_as(&store, GROUP_ID, "req-1", "chB", "up-b", "auth_error");
    insert_switch_as(&store, GROUP_ID, "req-1", "chC", "up-c", "auth_error");

    let rows = store.stats_by_model(Some(GROUP_ID), None).unwrap();
    let sum: i64 = rows.iter().map(|r| r["requests"].as_i64().unwrap()).sum();
    let total = store.stats_by_group(Some(GROUP_ID), None).unwrap()[0]["requests"].as_i64().unwrap();

    assert_eq!(total, 1, "真机只有 1 次请求");
    assert_eq!(sum, 3, "但展开成 3 行尝试 —— 这是 Owner 选定的语义，不是 bug: {rows:?}");
    assert!(sum > total, "「按每次尝试展开」必然让行和大于总数");
}

fn insert_switch_as(store: &Store, group: &str, request_id: &str, channel: &str, upstream: &str, class: &str) {
    store
        .insert_switch_log(&SwitchLog {
            ts: ts_for(1),
            request_id: request_id.into(),
            group_id: group.into(),
            attempt_no: 1,
            tier: 1,
            channel_id: channel.into(),
            channel_name: format!("{channel}-name"),
            upstream_model: upstream.into(),
            outcome: "failed".into(),
            error_class: class.into(),
            http_status: None,
            latency_ms: 12,
            message: String::new(),
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

