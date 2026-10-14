//! Exposed models are DERIVED, not typed in (2026-10-14).
//!
//! What this pins down, in the user terms: the "exposed model" list of a group is
//! no longer a text box somebody fills -- it is the set of model names the group
//! own members declare, recomputed by the server on every load and every save.
//!
//! Coverage map:
//!   e01 derivation  -- tier level order, declared order inside a tier, first-seen
//!                     dedupe, dangling member id contributes nothing
//!   e02 normalize   -- a hand-written / stale list is replaced; the count returned
//!                     is the number of groups that actually moved
//!   e03 PUT         -- a client that still posts a hand-typed list gets it
//!                     overwritten, and the STORED document (GET) agrees
//!   e04 request path-- a derived name routes (narrow plan), an unknown one is still
//!                     404, and the group name is STILL a wildcard (EXACT WINS
//!                     untouched: the derivation never swallows name/id)
//!   e05 wildcard    -- a group whose members are empty exposes nothing, and the
//!                     name it used to claim is honestly 404 again (this is the
//!                     shape of the live grp_pro group: an empty tier-1)
//!   e06 /v1/models  -- the derived list is what the public endpoint advertises

mod support;

use gw_server::config::{
    derive_exposed_models, ApiKey, Channel, Config, Group, GroupOverrides, ModelBinding, Protocol,
    Tier, TierMember,
};
use gw_server::proxy::AppState;
use support::*;
use tower::util::ServiceExt;

fn chan(id: &str, base: &str) -> Channel {
    Channel {
        id: id.into(),
        name: id.into(),
        base_url: base.into(),
        protocol: Protocol::Openai,
        api_key: format!("sk-{id}"),
        enabled: true,
        weight: 1,
        headers: Default::default(),
    }
}

fn bind(id: &str, channel: &str, exposed: &str) -> ModelBinding {
    ModelBinding {
        id: id.into(),
        channel_id: channel.into(),
        exposed: exposed.into(),
        upstream: format!("up-{id}"),
        price_per_m: [1.0, 2.0, 0.0, 0.0],
        enabled: true,
    }
}

fn tm(model_id: &str) -> TierMember {
    TierMember { model_id: model_id.into(), weight: 1 }
}

/// One group over real (fake) upstreams. Tiers are declared OUT of level order on
/// purpose, so anything that trusts declaration order instead of sorting by
/// level fails loudly.
fn cfg_with_tiers(base: &str, prefixes: &[String], tiers: Vec<Tier>, exposed_models: Vec<String>) -> Config {
    let mut channels = Vec::new();
    let mut models = Vec::new();
    for (i, p) in prefixes.iter().enumerate() {
        let cid = format!("ch{i}");
        channels.push(chan(&cid, &format!("{base}{p}")));
        // m0/m1 answer "alpha", m2 answers "beta" -- two names, both real.
        let name = if i < 2 { "alpha" } else { "beta" };
        models.push(bind(&format!("m{i}"), &cid, name));
    }
    Config {
        retry: test_retry(),
        channels,
        models,
        groups: vec![Group {
            id: "g1".into(),
            name: "team-one".into(),
            exposed_models,
            tiers,
            overrides: GroupOverrides::default(),
        }],
        api_keys: vec![ApiKey { key: "gw-test-key".into(), group_id: "g1".into(), name: "tester".into(), enabled: true }],
        ..Default::default()
    }
}

async fn admin_get_cfg(st: &AppState) -> serde_json::Value {
    let admin = gw_server::admin::admin_router(st.clone());
    let req = axum::http::Request::builder().method("GET").uri("/admin/api/config").body(axum::body::Body::empty()).unwrap();
    let resp = admin.oneshot(req).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    body_json(resp).await
}

async fn admin_put_cfg(st: &AppState, cfg: &Config) -> (u16, serde_json::Value) {
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

async fn models_list(router: &axum::Router) -> Vec<String> {
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/v1/models")
        .header("authorization", "Bearer gw-test-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let v = body_json(resp).await;
    v["data"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap().to_string()).collect()
}

fn strings(v: &serde_json::Value) -> Vec<String> {
    v.as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect()
}

// ---- e01: the derivation itself ----

#[test]
fn e01_derivation_follows_levels_declared_order_and_dedupes() {
    let base = Config {
        channels: vec![chan("ch0", "http://x")],
        // m1 and m0 BOTH expose "alpha"; m2 exposes "beta".
        models: vec![
            bind("m0", "ch0", "alpha"),
            bind("m1", "ch0", "alpha"),
            bind("m2", "ch0", "beta"),
        ],
        groups: vec![],
        ..Default::default()
    };
    // Tiers declared 3,1,2 -- the derivation must walk 1,2,3.
    let g = Group {
        id: "g".into(),
        name: "G".into(),
        exposed_models: vec!["nonsense".into()],
        tiers: vec![
            Tier { level: 3, members: vec![tm("m2")] },
            Tier { level: 1, members: vec![tm("m1"), tm("ghost")] },
            Tier { level: 2, members: vec![tm("m0")] },
        ],
        overrides: GroupOverrides::default(),
    };
    assert_eq!(
        derive_exposed_models(&base, &g),
        vec!["alpha".to_string(), "beta".to_string()],
        "level 1 introduces alpha first, level 3 then adds beta; the dangling id adds nothing"
    );
}

#[test]
fn e01_empty_group_derives_to_an_empty_list() {
    let base = Config { channels: vec![chan("ch0", "http://x")], models: vec![bind("m0", "ch0", "alpha")], ..Default::default() };
    let g = Group {
        id: "g".into(),
        name: "G".into(),
        exposed_models: vec!["alpha".into()],
        tiers: vec![Tier { level: 1, members: vec![] }],
        overrides: GroupOverrides::default(),
    };
    assert!(derive_exposed_models(&base, &g).is_empty(), "no members means nothing is exposed");
}

#[test]
fn e02_normalize_reports_only_the_groups_it_moved() {
    let mut cfg = Config {
        channels: vec![chan("ch0", "http://x")],
        models: vec![bind("m0", "ch0", "alpha")],
        groups: vec![
            // already correct: must NOT count as changed
            Group { id: "ok".into(), name: "OK".into(), exposed_models: vec!["alpha".into()],
                    tiers: vec![Tier { level: 1, members: vec![tm("m0")] }], overrides: GroupOverrides::default() },
            // stale ghost name: must be rewritten
            Group { id: "ghost".into(), name: "GHOST".into(), exposed_models: vec!["alpha".into(), "retired".into()],
                    tiers: vec![Tier { level: 1, members: vec![tm("m0")] }], overrides: GroupOverrides::default() },
        ],
        ..Default::default()
    };
    assert_eq!(cfg.normalize_exposed_models(), 1, "only the second group disagreed with its members");
    assert_eq!(cfg.groups[0].exposed_models, vec!["alpha".to_string()]);
    assert_eq!(cfg.groups[1].exposed_models, vec!["alpha".to_string()], "the ghost name is gone");
    assert_eq!(cfg.normalize_exposed_models(), 0, "normalization is idempotent");
}

// ---- e03/e04/e05/e06: through the real admin + data plane ----

#[tokio::test]
async fn e03_put_rewrites_a_hand_typed_list_and_stores_the_derived_one() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson, Behavior::GoodJson, Behavior::GoodJson]).await;
    let cfg = cfg_with_tiers(
        &base, &prefixes,
        vec![
            Tier { level: 1, members: vec![tm("m0"), tm("m1")] },
            Tier { level: 2, members: vec![tm("m2")] },
        ],
        // Deliberately wrong: a ghost name nobody serves, and the derived one missing.
        vec!["long-retired-model".into()],
    );
    let (st, router) = gateway(cfg.clone()).await;

    let (status, body) = admin_put_cfg(&st, &cfg).await;
    assert_eq!(status, 200, "a stale list is not an error, it is normalized: {body}");
    assert_eq!(body["exposed_synced"], 1, "one group was rewritten: {body}");

    // The document that LANDED is the derived one, not what the client sent.
    let stored = admin_get_cfg(&st).await;
    assert_eq!(
        stored["groups"][0]["exposed_models"],
        serde_json::json!(["alpha", "beta"]),
        "GET hands back the derived list: {stored}"
    );
    assert!(!strings(&stored["groups"][0]["exposed_models"]).iter().any(|m| m == "long-retired-model"));

    // And the running snapshot is driven by real requests, not just by JSON.
    let (status, _) = post_chat(&router, "alpha", false).await;
    assert_eq!(status, 200, "a derived name routes on the narrow plan");
    let (status, _) = post_chat(&router, "beta", false).await;
    assert_eq!(status, 200, "the second derived name routes too");
    let (status, body) = post_chat(&router, "long-retired-model", false).await;
    assert_eq!(status, 404, "a name no member serves is not exposed any more: {body}");
}

#[tokio::test]
async fn e04_the_group_name_is_still_a_wildcard_but_an_unknown_name_is_not() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson, Behavior::GoodJson, Behavior::GoodJson]).await;
    let cfg = cfg_with_tiers(
        &base, &prefixes,
        vec![
            Tier { level: 1, members: vec![tm("m0")] },
            Tier { level: 2, members: vec![tm("m1")] },
            Tier { level: 3, members: vec![tm("m2")] },
        ],
        vec!["alpha".into()],
    );
    let (st, router) = gateway(cfg.clone()).await;
    let (status, body) = admin_put_cfg(&st, &cfg).await;
    assert_eq!(status, 200, "{body}");

    // The derivation must NOT swallow the wildcard identity: name and id keep
    // working exactly as before, and they are still NOT part of exposed_models.
    let stored = admin_get_cfg(&st).await;
    let stored_list = strings(&stored["groups"][0]["exposed_models"]);
    assert_eq!(stored_list, vec!["alpha".to_string(), "beta".to_string()]);
    assert!(!stored_list.iter().any(|m| m == "team-one" || m == "g1"), "name/id stay OUT of the list: {stored_list:?}");

    for name in ["alpha", "team-one", "g1"] {
        let (status, body) = post_chat(&router, name, false).await;
        assert_eq!(status, 200, "request for {name} must still resolve: {body}");
    }
    let (status, body) = post_chat(&router, "nobody-serves-this", false).await;
    assert_eq!(status, 404, "{body}");
}

#[tokio::test]
async fn e06_v1_models_advertises_the_derived_list_plus_the_group_identity() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson, Behavior::GoodJson, Behavior::GoodJson]).await;
    let cfg = cfg_with_tiers(
        &base, &prefixes,
        vec![
            Tier { level: 1, members: vec![tm("m0")] },
            Tier { level: 2, members: vec![tm("m1"), tm("m2")] },
        ],
        vec!["alpha".into()],
    );
    let (st, router) = gateway(cfg.clone()).await;
    let (status, body) = admin_put_cfg(&st, &cfg).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        models_list(&router).await,
        vec!["alpha".to_string(), "beta".to_string(), "team-one".to_string(), "g1".to_string()],
        "exposed first, then name and id -- the order list_models always had"
    );
}

#[tokio::test]
async fn e05_a_group_whose_members_are_empty_exposes_nothing() {
    // The grp_pro shape: an empty tier and a list claiming a model nobody serves.
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson]).await;
    let cfg = cfg_with_tiers(
        &base, &prefixes,
        vec![Tier { level: 1, members: vec![] }],
        vec!["deepseek-v4-pro".into()],
    );
    let (st, router) = gateway(cfg.clone()).await;
    let (status, body) = admin_put_cfg(&st, &cfg).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["exposed_synced"], 1, "{body}");

    let stored = admin_get_cfg(&st).await;
    assert_eq!(stored["groups"][0]["exposed_models"], serde_json::json!([]), "the ghost claim is gone: {stored}");
    // Callers now get the honest, documented answer instead of a route that
    // could never be built.
    let (status, body) = post_chat(&router, "deepseek-v4-pro", false).await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body.pointer("/error/code").and_then(|c| c.as_str()), Some("model_not_found"), "{body}");
    // The group identity is a wildcard, but a wildcard over zero members has no
    // candidate either -- unchanged from before this feature.
    let (status, body) = post_chat(&router, "team-one", false).await;
    assert_eq!(status, 404, "an empty group has no candidates, wildcard or not: {body}");
}
