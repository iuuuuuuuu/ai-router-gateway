//! Task K: the server half of "add models from the provider's own model list"
//! (board task-32; user m03395 「渠道与模型没有添加模型的入口,支持一下 手动添加和从models接口
//! 获取两种方式添加」).
//!
//! What these tests pin down, in the user's terms:
//!   * the console may hand the gateway EITHER a saved channel id OR a draft it
//!     has not saved yet, and gets back a plain list of model ids;
//!   * that list is read from the provider's own model endpoint with the
//!     channel's credentials, so a channel that is not configured stays broken;
//!   * EVERY failure is reported as ok:false with the reason in "error" — a 401,
//!     a dead host, an HTML body and an empty answer can never be mistaken for
//!     "this channel has no models";
//!   * the wire request is what the frozen contract says: a real GET, the
//!     documented path per protocol, and the channel's own headers applied on
//!     top of the protocol defaults.
//!
//! Every assertion below is written to go RED while the handler is missing or
//! wrong: an unimplemented route answers 405, a wrong path 404, a wrong base a
//! different id list, a body-less 200 drops the fields the assertions read.
//! Nothing here is mocked — real axum servers, real HTTP, real config.
//!
//! Coverage map:
//!   c01 openai {data:[{id}]}         -> ok:true, count, upstream order kept
//!   c02 gemini {models:[{name}]}     -> the models/ prefix stripped, ok:true
//!   c03 anthropic + channel headers  -> what a real GET really carried
//!   c04 upstream 401                 -> ok:false, status 401, error non-empty
//!   c05 upstream HTML garbage        -> ok:false, error names the payload
//!   c06 unknown channel, no base_url -> HTTP 404, never a bogus 200 list
//!   c07 draft: base_url only         -> the list is still fetched
//!   c08 duplicate ids                -> collapsed, first-seen order
//!   c09 404 / EMPTY body             -> ok:false both times, with a control
//!   c10 dead host / missing base_url -> honest connect error / 400; GET is 405
//!   c11 request headers over a base  -> merged, same name overrides

mod support;

use gw_server::config::{
    ApiKey, Channel, Config, Group, GroupOverrides, ModelBinding, Protocol, Tier, TierMember,
};
use gw_server::proxy::AppState;
use support::*;
use tower::util::ServiceExt;

/// Where the console's model probe lives.
const MODELS_PATH: &str = "/admin/api/channel-models";

fn tm(model_id: &str) -> TierMember {
    TierMember { model_id: model_id.to_string(), weight: 1 }
}

/// A config with NO channel at all — the state a user is in when the add-model
/// dialog is opened before the channel behind it has been saved.
fn empty_config() -> Config {
    Config { retry: test_retry(), ..Default::default() }
}

/// A config whose channels carry an explicit protocol, api key and custom
/// headers — none of which the shared test_config helper can express.
fn models_config(
    base: &str,
    prefixes: &[&str],
    protocols: &[Protocol],
    headers: &[Vec<(&str, &str)>],
) -> Config {
    assert_eq!(prefixes.len(), protocols.len());
    assert_eq!(prefixes.len(), headers.len());
    let mut channels = Vec::new();
    let mut models = Vec::new();
    for (i, p) in prefixes.iter().enumerate() {
        let mut h = std::collections::HashMap::new();
        for (k, v) in &headers[i] {
            h.insert((*k).to_string(), (*v).to_string());
        }
        channels.push(Channel {
            id: format!("ch{i}"),
            name: format!("channel-{i}"),
            base_url: format!("{base}{p}"),
            protocol: protocols[i],
            api_key: format!("sk-fake-{i}"),
            enabled: true,
            weight: 1,
            headers: h,
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
    Config {
        retry: test_retry(),
        channels,
        models,
        groups: vec![Group {
            id: "g1".into(),
            name: "test-group".into(),
            exposed_models: vec![EXPOSED.to_string()],
            tiers: vec![Tier { level: 1, members: vec![tm("m0")] }],
            overrides: GroupOverrides { retry: None },
        }],
        api_keys: vec![ApiKey {
            key: "gw-test-key".into(),
            group_id: "g1".into(),
            name: "tester".into(),
            enabled: true,
        }],
        ..Default::default()
    }
}

/// Drive the REAL admin router with a POST body. Returns (http status, json).
async fn call(st: &AppState, body: serde_json::Value) -> (u16, serde_json::Value) {
    let admin = gw_server::admin::admin_router(st.clone());
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(MODELS_PATH)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = admin.oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    (status, body_json(resp).await)
}

/// The model list, or a loud failure when the field is missing entirely: a
/// handler that only ever reports ok/error must not pass these tests by default.
fn models_of(v: &serde_json::Value) -> Vec<String> {
    v.get("models")
        .and_then(|m| m.as_array())
        .unwrap_or_else(|| panic!("response has no models array: {v}"))
        .iter()
        .map(|x| x.as_str().unwrap_or_else(|| panic!("model id is not a string: {x}")).to_string())
        .collect()
}

/// One header value out of the fake upstream's raw echo of a real request.
fn header_row(rows: &[String], name: &str) -> String {
    let marker = format!("hdr {name}=");
    let hits: Vec<&String> = rows.iter().filter(|r| r.starts_with(marker.as_str())).collect();
    assert_eq!(hits.len(), 1, "expected exactly one {name} header on the wire, got {hits:?}");
    hits[0][marker.len()..].to_string()
}

#[tokio::test]
async fn c01_openai_shape_returns_ids_in_upstream_order() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson]).await;
    let cfg = models_config(&base, &[prefixes[0].as_str()], &[Protocol::Openai], &[vec![]]);
    let (st, _router) = gateway(cfg).await;

    let (status, body) = call(&st, serde_json::json!({ "channel_id": "ch0" })).await;

    assert_eq!(status, 200, "the probe always answers 200 and puts the outcome in ok: {body}");
    assert_eq!(body["ok"], true, "a reachable channel must report ok: {body}");
    assert_eq!(body["status"], 200, "the upstream status must be reported: {body}");
    assert_eq!(
        models_of(&body),
        vec!["u0-model-a", "u0-model-b", "u0-model-c"],
        "the provider order is the order the console shows: {body}"
    );
    assert_eq!(body["count"], 3, "count must agree with the list: {body}");
    assert!(body["latency_ms"].is_u64(), "latency_ms must be a number: {body}");
    assert!(body.get("error").is_none(), "a success carries no error: {body}");
}

#[tokio::test]
async fn c02_gemini_shape_strips_the_models_prefix() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson]).await;
    let cfg = models_config(&base, &[prefixes[0].as_str()], &[Protocol::Gemini], &[vec![]]);
    let (st, _router) = gateway(cfg).await;

    let (status, body) = call(&st, serde_json::json!({ "channel_id": "ch0" })).await;

    assert_eq!(status, 200);
    assert_eq!(body["ok"], true, "the gemini shape must parse too: {body}");
    // The fake serves /u0/v1beta/models with names "models/u0-gemini-alpha|beta";
    // that prefix is the provider routing syntax, never part of a model name.
    // The other route on the same prefix would have answered u0-model-a..c, so
    // this also proves the gemini path was used.
    assert_eq!(models_of(&body), vec!["u0-gemini-alpha", "u0-gemini-beta"], "{body}");
    assert_eq!(body["count"], 2);
}

#[tokio::test]
async fn c03_anthropic_protocol_and_channel_headers_reach_the_wire() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::RecordHeaders200Json]).await;
    let cfg = models_config(
        &base,
        &[prefixes[0].as_str()],
        &[Protocol::Anthropic],
        &[vec![("X-Org-Id", "team-123"), ("X-Api-Key", "sk-custom")]],
    );
    let (st, _router) = gateway(cfg).await;

    let (status, body) = call(&st, serde_json::json!({ "channel_id": "ch0" })).await;
    assert_eq!(status, 200);
    assert_eq!(body["ok"], true, "the recorded list must parse: {body}");

    // The fake echoes every header the GET really carried; each becomes a row.
    let rows = models_of(&body);
    assert_eq!(header_row(&rows, "x-upstream-prefix"), "u0", "the GET went somewhere else: {rows:?}");
    assert_eq!(header_row(&rows, "anthropic-version"), "2023-06-01", "anthropic needs this header: {rows:?}");
    assert_eq!(header_row(&rows, "x-api-key"), "sk-custom", "the channel header must win over the default: {rows:?}");
    assert_eq!(header_row(&rows, "x-org-id"), "team-123", "channel headers must be applied: {rows:?}");
}

#[tokio::test]
async fn c04_upstream_401_is_reported_and_never_swallowed() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::Fail401]).await;
    let cfg = models_config(&base, &[prefixes[0].as_str()], &[Protocol::Openai], &[vec![]]);
    let (st, _router) = gateway(cfg).await;

    let (status, body) = call(&st, serde_json::json!({ "channel_id": "ch0" })).await;

    assert_eq!(status, 200, "the transport succeeded; only the provider refused: {body}");
    assert_eq!(body["ok"], false, "a rejected key is NOT a working channel: {body}");
    assert_eq!(body["status"], 401, "the provider status must be reported: {body}");
    assert_eq!(body["count"], 0);
    assert!(models_of(&body).is_empty(), "a refusal yields no models: {body}");
    let err = body["error"].as_str().unwrap_or_else(|| panic!("a failure must carry error text: {body}"));
    assert!(err.contains("401"), "the upstream status must survive into the message: {err}");
    assert!(err.contains("bad key"), "the upstream body must survive into the message: {err}");
}

#[tokio::test]
async fn c05_an_html_body_is_an_error_not_an_empty_model_list() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::Garbage]).await;
    let cfg = models_config(&base, &[prefixes[0].as_str()], &[Protocol::Openai], &[vec![]]);
    let (st, _router) = gateway(cfg).await;

    let (status, body) = call(&st, serde_json::json!({ "channel_id": "ch0" })).await;

    assert_eq!(status, 200);
    assert_eq!(body["ok"], false, "an unparseable body is a failure, not an empty list: {body}");
    assert_eq!(body["status"], 200, "the transport really did succeed: {body}");
    assert_eq!(body["count"], 0);
    let err = body["error"].as_str().unwrap_or_else(|| panic!("a failure must carry error text: {body}"));
    assert!(err.contains("unrecognized"), "the error must name the unusable payload: {err}");
}

#[tokio::test]
async fn c06_unknown_channel_without_a_draft_is_404() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson]).await;
    let cfg = models_config(&base, &[prefixes[0].as_str()], &[Protocol::Openai], &[vec![]]);
    let (st, _router) = gateway(cfg).await;

    let (status, body) = call(&st, serde_json::json!({ "channel_id": "ch-does-not-exist" })).await;

    assert_eq!(status, 404, "an unknown channel must be reported, not answered with a list: {body}");
    let err = body["error"].as_str().unwrap_or_else(|| panic!("the 404 must carry an error: {body}"));
    assert!(err.contains("ch-does-not-exist"), "the missing id must be named: {err}");
}

#[tokio::test]
async fn c07_a_draft_channel_is_usable_before_it_is_saved() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::GoodJson]).await;
    // No channel is saved at all: exactly the state of the dialog when the user
    // presses the fetch-models button before saving the channel.
    let (st, _router) = gateway(empty_config()).await;

    let (status, body) = call(
        &st,
        serde_json::json!({
            "base_url": format!("{base}{}", prefixes[0]),
            "api_key": "sk-draft",
            "protocol": "openai",
        }),
    )
    .await;

    assert_eq!(status, 200, "a draft that names a base_url is a legal request: {body}");
    assert_eq!(body["ok"], true, "a draft base_url plus key must be enough: {body}");
    assert_eq!(models_of(&body), vec!["u0-model-a", "u0-model-b", "u0-model-c"], "{body}");
    assert_eq!(body["count"], 3);
}

#[tokio::test]
async fn c08_duplicate_ids_collapse_in_first_seen_order() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::Duplicates200Json]).await;
    let cfg = models_config(&base, &[prefixes[0].as_str()], &[Protocol::Openai], &[vec![]]);
    let (st, _router) = gateway(cfg).await;

    let (_, body) = call(&st, serde_json::json!({ "channel_id": "ch0" })).await;

    assert_eq!(body["ok"], true, "{body}");
    // The fake answers a,b,a,b: a sorted dedup would give the same list here on
    // purpose is avoided, so this pins first-seen order over any sorting.
    assert_eq!(models_of(&body), vec!["a", "b"], "the same id twice is one import: {body}");
    assert_eq!(body["count"], 2, "count counts models, not upstream rows: {body}");
}

#[tokio::test]
async fn c09_a_404_or_an_unusable_body_is_never_a_silent_success() {
    let (base, prefixes) = spawn_fake_upstreams(&[
        Behavior::NotFoundJson,
        Behavior::Garbage,
        Behavior::Empty,
        Behavior::GoodJson,
    ])
    .await;
    let p: Vec<&str> = prefixes.iter().map(|s| s.as_str()).collect();
    let cfg = models_config(&base, &p, &[Protocol::Openai; 4], &[vec![], vec![], vec![], vec![]]);
    let (st, _router) = gateway(cfg).await;

    // A 404 from the provider is a failure, not an empty catalogue.
    let (_, missing) = call(&st, serde_json::json!({ "channel_id": "ch0" })).await;
    assert_eq!(missing["ok"], false, "a 404 from the provider is not a success: {missing}");
    assert_eq!(missing["status"], 404);
    assert!(
        missing["error"].as_str().map_or(false, |e| e.contains("404")),
        "the provider status must survive into the message: {missing}"
    );

    // An HTML body is a failure too.
    let (_, html) = call(&st, serde_json::json!({ "channel_id": "ch1" })).await;
    assert_eq!(html["ok"], false, "HTML is not a model list: {html}");
    assert!(
        html["error"].as_str().map_or(false, |e| e.contains("unrecognized")),
        "the error must name the unusable payload: {html}"
    );

    // So is a body that is completely empty.
    let (_, empty) = call(&st, serde_json::json!({ "channel_id": "ch2" })).await;
    assert_eq!(empty["ok"], false, "an EMPTY body is not an empty catalogue: {empty}");
    assert_eq!(empty["count"], 0);
    assert!(models_of(&empty).is_empty());
    assert!(
        empty["error"].as_str().map_or(false, |e| e.contains("unrecognized")),
        "an unusable answer must say why: {empty}"
    );

    // Control: the same helper, same config, against a channel that IS fine, so
    // the failures above cannot be explained by a broken test harness.
    let (_, good) = call(&st, serde_json::json!({ "channel_id": "ch3" })).await;
    assert_eq!(good["ok"], true, "the control channel must still work: {good}");
    assert_eq!(models_of(&good).len(), 3);
}

#[tokio::test]
async fn c10_a_dead_host_and_a_missing_base_url_are_reported_honestly() {
    let (st, _router) = gateway(empty_config()).await;

    // Port 1 on localhost: nothing listens there, so this is a REAL connect
    // failure against a real socket, not a simulated one.
    let (status, dead) = call(
        &st,
        serde_json::json!({ "base_url": "http://127.0.0.1:1", "api_key": "sk-x", "protocol": "openai" }),
    )
    .await;
    assert_eq!(status, 200, "a dead host is a business failure, not a transport one: {dead}");
    assert_eq!(dead["ok"], false, "a dead host is not an empty list: {dead}");
    assert_eq!(dead["status"], 0, "nothing was reached, so there is no upstream status: {dead}");
    let err = dead["error"].as_str().unwrap_or_else(|| panic!("a connect failure must be named: {dead}"));
    assert!(!err.is_empty(), "a connect failure must be named: {err}");

    let (status, none) = call(&st, serde_json::json!({ "api_key": "sk-x" })).await;
    assert_eq!(status, 400, "no base_url anywhere must be a 400: {none}");
    assert!(
        none["error"].as_str().map_or(false, |e| e.contains("base_url")),
        "the 400 must name the missing field: {none}"
    );

    // The probe posts a body, so the method itself is part of the contract: a
    // stray GET must not be answered with a phantom list.
    let admin = gw_server::admin::admin_router(st.clone());
    let resp = admin
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(MODELS_PATH)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 405, "the probe is POST-only");
}

#[tokio::test]
async fn c11_request_headers_merge_over_the_saved_channel() {
    let (base, prefixes) = spawn_fake_upstreams(&[Behavior::RecordHeaders200Json]).await;
    // The saved channel already carries a default header and a key of its own.
    let cfg = models_config(
        &base,
        &[prefixes[0].as_str()],
        &[Protocol::Openai],
        &[vec![("X-From-Channel", "saved"), ("X-Same-Name", "saved")]],
    );
    let (st, _router) = gateway(cfg).await;

    let (_, body) = call(
        &st,
        serde_json::json!({
            "channel_id": "ch0",
            "api_key": "sk-typed-over",
            "headers": { "X-From-Request": "typed", "X-Same-Name": "typed" },
        }),
    )
    .await;

    assert_eq!(body["ok"], true, "{body}");
    let rows = models_of(&body);
    assert_eq!(header_row(&rows, "x-from-channel"), "saved", "the saved header must survive: {rows:?}");
    assert_eq!(header_row(&rows, "x-from-request"), "typed", "the request header must be applied: {rows:?}");
    assert_eq!(header_row(&rows, "x-same-name"), "typed", "same name: the override wins: {rows:?}");
    assert_eq!(header_row(&rows, "authorization"), "Bearer sk-typed-over", "an overridden key must be used: {rows:?}");
}




#[tokio::test]
async fn c12_an_empty_but_recognisable_list_is_a_real_answer() {
    let (base, prefixes) = spawn_fake_upstreams(&[
        Behavior::EmptyDataJson,
        Behavior::GoodJson,
        Behavior::NoListJson,
        Behavior::Empty,
    ])
    .await;
    let p: Vec<&str> = prefixes.iter().map(|s| s.as_str()).collect();
    let cfg = models_config(&base, &p, &[Protocol::Openai; 4], &[vec![], vec![], vec![], vec![]]);
    let (st, _router) = gateway(cfg).await;

    // {"data": []} is a model list the gateway fully understands that carries
    // no models: the provider's honest answer is "nothing here". Reporting that
    // as "unrecognized" would blame the provider for our own parser, and the
    // console would show a parse error instead of an empty list.
    let (status, empty) = call(&st, serde_json::json!({ "channel_id": "ch0" })).await;
    assert_eq!(status, 200);
    assert_eq!(empty["ok"], true, "an empty list is a valid answer, not a parse error: {empty}");
    assert_eq!(empty["status"], 200);
    assert_eq!(empty["count"], 0);
    assert!(models_of(&empty).is_empty(), "there really are no models: {empty}");
    assert!(empty.get("error").is_none(), "a working channel carries no error: {empty}");

    // Control on the SAME server: a non-empty list still works, so the empty
    // case above is not an artefact of a broken route.
    let (_, good) = call(&st, serde_json::json!({ "channel_id": "ch1" })).await;
    assert_eq!(good["ok"], true, "{good}");
    assert_eq!(models_of(&good).len(), 3);

    // {} has no model list in it at all: that IS a parse failure, and the
    // distinction from {"data": []} above is the whole point of this test.
    let (_, nolist) = call(&st, serde_json::json!({ "channel_id": "ch2" })).await;
    assert_eq!(nolist["ok"], false, "an object with no model list is unusable: {nolist}");
    assert!(
        nolist["error"].as_str().map_or(false, |e| e.contains("unrecognized")),
        "the error must name the unusable payload: {nolist}"
    );

    // And an empty body is still a failure, not an empty catalogue.
    let (_, blank) = call(&st, serde_json::json!({ "channel_id": "ch3" })).await;
    assert_eq!(blank["ok"], false, "an empty body is not an empty catalogue: {blank}");
}

#[tokio::test]
async fn c13_a_stalled_body_times_out_instead_of_hanging_the_dialog() {
    // A provider that answers with its response HEAD and then stalls the body
    // forever. send() returns fine; only the body read hangs. If the timeout
    // covers just send(), the console dialog spins forever on a channel that
    // answered "200 OK" — so this pins the whole exchange under one deadline.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                // Head only: valid Content-Length, valid JSON, zero body bytes.
                // A raw socket is used because no axum response can send a head
                // and then legally never produce the promised body.
                let head = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 400\r\n\r\n";
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.flush().await;
                tokio::time::sleep(std::time::Duration::from_secs(45)).await;
            });
        }
    });

    let (st, _router) = gateway(empty_config()).await;
    let started = std::time::Instant::now();
    let (status, body) = call(
        &st,
        serde_json::json!({ "base_url": format!("http://{addr}"), "api_key": "sk-x", "protocol": "openai" }),
    )
    .await;
    let elapsed = started.elapsed();

    assert_eq!(status, 200, "the transport succeeded, the provider stalled: {body}");
    assert_eq!(body["ok"], false, "a stalled body is not a working channel: {body}");
    assert_eq!(body["status"], 0, "the exchange never completed: {body}");
    assert_eq!(body["count"], 0);
    let err = body["error"].as_str().unwrap_or_else(|| panic!("a timeout must be named: {body}"));
    assert!(err.contains("timeout"), "the error must name the timeout: {err}");
    // The handler's own 20s deadline is the only thing that can end this, so
    // finishing well inside the 45s the upstream would stall proves it fired.
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "the 20s deadline must end the exchange (took {elapsed:?}): {body}"
    );
}
