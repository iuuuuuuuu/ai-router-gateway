//! Buffer-commit proxy: the heart of the gateway.
//!
//! Core promise (user): model failures are handled INSIDE the loop; the
//! client never sees them. Only two terminal outcomes exist and they are
//! mutually exclusive (I2):
//!   - success: full content + full usage, in the CLIENT's protocol
//!   - failure: one structurally-valid standard error, in the CLIENT's protocol
//!
//! Commit point (mode a): first valid content delta OR complete buffered
//! response. Before commit: zero bytes to client (I1); any failure switches
//! candidate. After commit: stream to the end; a mid-stream upstream break is
//! repaired with finish_reason=error so the client never mistakes a broken
//! stream for success.

use crate::config::{ConfigSnapshot, Protocol};
use crate::health::{Attribution, CooldownKey, HealthRegistry};
use crate::protocol::{anthropic, gemini, ir, openai, ChatRequest, ChatResponse, StreamEvent};
use crate::router::{
    build_budget, build_plan, classify_failure, next_step, ProxyError, StatusClass, Step,
    Terminal, TimeoutWhat, WalkState,
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Shared reqwest client pool keyed by the configured connect timeout.
///
/// The connect timeout is a *config* value (retry.connect_timeout_ms), so it
/// cannot be baked into one client at startup: a PUT /admin/api/config that
/// tightens the timeout must take effect without a restart. The pool keeps at
/// most one client per distinct ms value and rebuilds only when the value
/// changes; every rebuild keeps the same idle-pool behaviour as before.
#[derive(Default, Clone)]
pub struct HttpPool {
    // Arc so that every clone of AppState shares one cache: axum clones the
    // state per request and each clone must see the same pooled client.
    cached: std::sync::Arc<std::sync::Mutex<Option<(u64, reqwest::Client)>>>,
}

impl HttpPool {
    /// Client whose TCP connect timeout is connect_timeout_ms.
    pub fn client_for(&self, connect_timeout_ms: u64) -> reqwest::Client {
        let ms = connect_timeout_ms.max(1);
        let mut guard = match self.cached.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some((cached_ms, client)) = guard.as_ref() {
            if *cached_ms == ms {
                return client.clone();
            }
        }
        let client = reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(90))
            .connect_timeout(Duration::from_millis(ms))
            .build()
            // A client that cannot be built is a startup-level bug, not a
            // per-request one; fall back to a default client rather than panic.
            .unwrap_or_else(|_| reqwest::Client::new());
        *guard = Some((ms, client.clone()));
        client
    }
}

pub struct AppState {
    pub http: HttpPool,
    pub config: crate::config::ConfigHolder,
    pub health: Arc<HealthRegistry>,
    pub store: Arc<crate::store::Store>,
}

// Cloneable for axum::with_state (all fields are Arc/Client/Clone types)
impl Clone for AppState {
    fn clone(&self) -> Self {
        Self {
            http: self.http.clone(),
            config: self.config.clone(),
            health: self.health.clone(),
            store: self.store.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryProtocol {
    Openai,
    Anthropic,
    Gemini,
}

pub enum Outcome {
    /// non-stream: full parsed response (usage included)
    Full(Box<ChatResponse>),
    /// stream committed: usage arrives via oneshot when the pump ends
    Stream {
        usage_rx: tokio::sync::oneshot::Receiver<ir::Usage>,
    },
}

pub struct SwitchRecord {
    pub attempt_no: u32,
    pub tier: u32,
    pub channel_id: String,
    pub channel_name: String,
    pub upstream_model: String,
    pub error_class: &'static str,
    pub http_status: Option<u16>,
    pub latency_ms: u64,
    pub message: String,
}

pub struct RunResult {
    pub terminal: Terminal,
    pub records: Vec<SwitchRecord>,
    pub success: Option<Outcome>,
    pub usage: ir::Usage,
    pub ttft_ms: Option<u64>,
    pub duration_ms: u64,
    pub final_channel: Option<String>,
    pub final_upstream_model: Option<String>,
}

pub struct ProxyCtx<'a> {
    pub state: &'a AppState,
    pub request_id: String,
    pub group_id: String,
    pub exposed_model: String,
    pub entry: EntryProtocol,
    pub ir: ChatRequest,
    pub client_gone: CancellationToken,
    /// Per-request client frame channel; streaming output goes here only.
    pub feed_tx: mpsc::Sender<ClientFrame>,
}

fn empty_run(terminal: Terminal, started: Instant) -> RunResult {
    RunResult {
        terminal,
        records: vec![],
        success: None,
        usage: ir::Usage::default(),
        ttft_ms: None,
        duration_ms: started.elapsed().as_millis() as u64,
        final_channel: None,
        final_upstream_model: None,
    }
}

/// Main failover runner.
pub async fn run(ctx: &ProxyCtx<'_>) -> RunResult {
    let started = Instant::now();
    let mut records: Vec<SwitchRecord> = vec![];
    let mut usage_total = ir::Usage::default();
    let mut ttft: Option<u64> = None;

    let Some(snap) = ctx.state.config.get() else {
        return empty_run(Terminal::NoCandidate, started);
    };
    let Some(group) = snap.config.groups.iter().find(|g| g.id == ctx.group_id) else {
        return empty_run(Terminal::NoCandidate, started);
    };
    let budget = build_budget(&snap.config.retry, started);

    let mut walk = WalkState { idx: 0, cycle: 0, attempts: 0 };
    let mut plan = build_plan(&snap.config, group, &ctx.exposed_model, &ctx.state.health);

    loop {
        let now = Instant::now();
        match next_step(&mut walk, &plan, &budget, now, &ctx.state.health, snap.config.retry.cooldown_wait) {
            Step::Try(idx) => {
                let candidate = plan.runnable[idx].clone();
                walk.idx += 1;
                walk.attempts += 1;
                let attempt_started = Instant::now();

                let result = attempt_once(ctx, &snap, &candidate, &budget, ctx.client_gone.clone(), ctx.feed_tx.clone()).await;
                let latency = attempt_started.elapsed().as_millis() as u64;

                match result {
                    Ok(Outcome::Full(resp)) => {
                        ctx.state.health.record_success(&CooldownKey::scoped(&candidate.channel_id, &candidate.upstream_model));
                        usage_total = resp.usage.clone();
                        if ttft.is_none() { ttft = Some(latency); }
                        records.push(SwitchRecord {
                            attempt_no: walk.attempts, tier: candidate.tier,
                            channel_id: candidate.channel_id.clone(), channel_name: candidate.channel_name.clone(),
                            upstream_model: candidate.upstream_model.clone(),
                            error_class: "committed", http_status: Some(200), latency_ms: latency,
                            message: String::new(),
                        });
                        return RunResult {
                            terminal: Terminal::Exhausted,
                            records,
                            success: Some(Outcome::Full(resp)),
                            usage: usage_total,
                            ttft_ms: ttft,
                            duration_ms: started.elapsed().as_millis() as u64,
                            final_channel: Some(candidate.channel_id),
                            final_upstream_model: Some(candidate.upstream_model),
                        };
                    }
                    Ok(Outcome::Stream { usage_rx }) => {
                        ctx.state.health.record_success(&CooldownKey::scoped(&candidate.channel_id, &candidate.upstream_model));
                        if ttft.is_none() { ttft = Some(latency); }
                        records.push(SwitchRecord {
                            attempt_no: walk.attempts, tier: candidate.tier,
                            channel_id: candidate.channel_id.clone(), channel_name: candidate.channel_name.clone(),
                            upstream_model: candidate.upstream_model.clone(),
                            error_class: "committed", http_status: Some(200), latency_ms: latency,
                            message: String::new(),
                        });
                        return RunResult {
                            terminal: Terminal::Exhausted,
                            records,
                            success: Some(Outcome::Stream { usage_rx }),
                            usage: usage_total,
                            ttft_ms: ttft,
                            duration_ms: started.elapsed().as_millis() as u64,
                            final_channel: Some(candidate.channel_id),
                            final_upstream_model: Some(candidate.upstream_model),
                        };
                    }
                    Err((perr, partial)) => {
                        usage_total = ir::Usage {
                            input_tokens: usage_total.input_tokens.max(partial.input_tokens),
                            output_tokens: usage_total.output_tokens.max(partial.output_tokens),
                            cache_read_tokens: usage_total.cache_read_tokens.max(partial.cache_read_tokens),
                            cache_creation_tokens: usage_total.cache_creation_tokens.max(partial.cache_creation_tokens),
                        };
                        let (attribution, class) = classify_failure(&perr, &candidate);
                        records.push(SwitchRecord {
                            attempt_no: walk.attempts, tier: candidate.tier,
                            channel_id: candidate.channel_id.clone(), channel_name: candidate.channel_name.clone(),
                            upstream_model: candidate.upstream_model.clone(),
                            error_class: class,
                            http_status: match &perr { ProxyError::UpstreamStatus { status, .. } => Some(*status), _ => None },
                            latency_ms: latency,
                            message: proxy_err_message(&perr),
                        });
                        ctx.state.health.apply(attribution);
                        if matches!(perr, ProxyError::ClientGone) {
                            return RunResult { terminal: Terminal::Exhausted, records, success: None, usage: usage_total, ttft_ms: ttft, duration_ms: started.elapsed().as_millis() as u64, final_channel: None, final_upstream_model: None };
                        }
                    }
                }
            }
            Step::Rebuild => {
                // Cooldowns may have expired (or been armed) since the snapshot.
                // Deliberately do NOT return here when the fresh plan has
                // nothing runnable: the next next_step() call is what decides
                // between a bounded wait (cooldown_wait + a real expiry) and the
                // terminal outcomes, so an all-cooling rebuild waits instead of
                // 404-ing with zero attempts (I5).
                plan = build_plan(&snap.config, group, &ctx.exposed_model, &ctx.state.health);
            }
            Step::WaitFor(until) => {
                // I4/I5: waiting for a cooldown is allowed, but the wait must
                // fit the request's total budget AND leave room for at least one
                // real attempt afterwards. The router's hard deadline is
                // deadline - reserve. When the wait cannot fit, the honest
                // answer is "every upstream is cooling" (503
                // all_upstreams_cooling) - never a pointless sleep followed by a
                // misleading 502, and never a zero-attempt 404.
                let hard_deadline = budget.deadline.checked_sub(budget.reserve).unwrap_or(budget.deadline);
                let now = Instant::now();
                let wanted = until.saturating_duration_since(now);
                let remaining = hard_deadline.saturating_duration_since(now);
                let attempt_room = budget.connect_timeout.min(budget.attempt_deadline);
                if wanted.saturating_add(attempt_room) > remaining {
                    tracing::info!(
                        request_id = %ctx.request_id,
                        wait_ms = wanted.as_millis() as u64,
                        remaining_ms = remaining.as_millis() as u64,
                        "cooldown wait does not fit the budget; reporting all_upstreams_cooling (I4/I5)"
                    );
                    return RunResult { terminal: Terminal::AllCooldown, records, success: None, usage: usage_total, ttft_ms: ttft, duration_ms: started.elapsed().as_millis() as u64, final_channel: None, final_upstream_model: None };
                }
                tracing::info!(request_id = %ctx.request_id, wait_ms = wanted.as_millis() as u64, "all candidates cooling; waiting (I5)");
                tokio::select! {
                    _ = tokio::time::sleep(wanted.max(Duration::from_millis(50))) => {}
                    _ = ctx.client_gone.cancelled() => {
                        return RunResult { terminal: Terminal::Exhausted, records, success: None, usage: usage_total, ttft_ms: ttft, duration_ms: started.elapsed().as_millis() as u64, final_channel: None, final_upstream_model: None };
                    }
                }
                plan = build_plan(&snap.config, group, &ctx.exposed_model, &ctx.state.health);
                if let Some(t) = plan.blocked_terminal() {
                    tracing::info!(request_id = %ctx.request_id, terminal = ?t, "still blocked after the cooldown wait");
                    return RunResult { terminal: t, records, success: None, usage: usage_total, ttft_ms: ttft, duration_ms: started.elapsed().as_millis() as u64, final_channel: None, final_upstream_model: None };
                }
            }
            Step::Exhausted(t) => {
                return RunResult { terminal: t, records, success: None, usage: usage_total, ttft_ms: ttft, duration_ms: started.elapsed().as_millis() as u64, final_channel: None, final_upstream_model: None };
            }
        }
    }
}

pub fn proxy_err_message(e: &ProxyError) -> String {
    match e {
        ProxyError::UpstreamStatus { status, class } => format!("upstream status {status} ({class:?})"),
        ProxyError::Timeout { what } => format!("timeout: {what:?}"),
        ProxyError::Connect(m) => format!("connect: {m}"),
        ProxyError::ErrorBody(m) => format!("error body: {m}"),
        ProxyError::NotSse(m) => format!("not sse: {m}"),
        ProxyError::EmptyResponse => "empty response".into(),
        ProxyError::ClientGone => "client gone".into(),
    }
}

// ===== attempt execution =====

async fn attempt_once(
    ctx: &ProxyCtx<'_>,
    _snap: &ConfigSnapshot,
    cand: &crate::router::Candidate,
    budget: &crate::router::Budget,
    client_gone: CancellationToken,
    feed_tx: mpsc::Sender<ClientFrame>,
) -> Result<Outcome, (ProxyError, ir::Usage)> {
    let attempt_deadline = Instant::now() + budget.attempt_deadline;
    let url = format!("{}{}", build_upstream_url(cand), upstream_path_suffix(cand, &ctx.ir));
    let body = render_upstream_body(ctx, cand);

    let req = ctx.state.http
        .client_for(budget.connect_timeout.as_millis() as u64)
        .post(&url)
        .headers(upstream_headers(cand))
        .json(&body);

    let resp = match tokio::time::timeout(budget.ttfb_timeout, req.send()).await {
        Err(_) => return Err((ProxyError::Timeout { what: TimeoutWhat::Ttfb }, ir::Usage::default())),
        Ok(Err(e)) if e.is_connect() => return Err((ProxyError::Connect(e.to_string()), ir::Usage::default())),
        Ok(Err(e)) => return Err((ProxyError::Connect(e.to_string()), ir::Usage::default())),
        Ok(Ok(r)) => r,
    };

    let status = resp.status();
    if !status.is_success() {
        let sc = status_class(status.as_u16());
        let _ = resp.text().await; // drain
        return Err((ProxyError::UpstreamStatus { status: status.as_u16(), class: sc }, ir::Usage::default()));
    }

    match ctx.ir.stream {
        false => attempt_non_stream(cand, resp, attempt_deadline).await,
        true  => attempt_stream(ctx, cand, resp, budget, client_gone, feed_tx).await,
    }
}

fn status_class(status: u16) -> StatusClass {
    match status {
        401 | 403 => StatusClass::Auth,
        404 => StatusClass::ModelNotFound,
        429 => StatusClass::RateLimited { retry_after_ms: None },
        400 | 422 => StatusClass::BadRequest,
        _ if status >= 500 => StatusClass::Server,
        _ => StatusClass::BadRequest,
    }
}

fn build_upstream_url(cand: &crate::router::Candidate) -> String {
    let base = cand.base_url.trim_end_matches('/');
    match cand.protocol {
        Protocol::Openai    => format!("{base}/v1/chat/completions"),
        Protocol::Anthropic => format!("{base}/v1/messages"),
        Protocol::Gemini    => format!("{base}/v1beta"),
    }
}

/// Gemini nests the model + method in the path; other protocols are fixed.
fn upstream_path_suffix(cand: &crate::router::Candidate, ir: &ChatRequest) -> String {
    match cand.protocol {
        Protocol::Openai | Protocol::Anthropic => String::new(),
        Protocol::Gemini => {
            let (suffix, _body) = gemini::render_upstream_request(ir, &cand.upstream_model, ir.stream);
            format!("/models/{}{}", cand.upstream_model, suffix)
        }
    }
}

fn upstream_headers(cand: &crate::router::Candidate) -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let mut h = HeaderMap::new();
    match cand.protocol {
        Protocol::Openai => {
            h.insert(reqwest::header::AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {}", cand.api_key)).unwrap_or(HeaderValue::from_static("")));
        }
        Protocol::Anthropic => {
            h.insert("x-api-key", HeaderValue::from_str(&cand.api_key).unwrap_or(HeaderValue::from_static("")));
            h.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        }
        Protocol::Gemini => {
            h.insert("x-goog-api-key", HeaderValue::from_str(&cand.api_key).unwrap_or(HeaderValue::from_static("")));
        }
    }
    for (k, v) in &cand.extra_headers {
        if let (Ok(name), Ok(val)) = (k.parse::<HeaderName>(), v.parse::<HeaderValue>()) {
            h.insert(name, val);
        }
    }
    h
}

fn render_upstream_body(ctx: &ProxyCtx<'_>, cand: &crate::router::Candidate) -> serde_json::Value {
    match cand.protocol {
        Protocol::Openai => openai::render_upstream_request(&ctx.ir, &cand.upstream_model, ctx.ir.stream),
        Protocol::Anthropic => anthropic::render_upstream_request(&ctx.ir, &cand.upstream_model, ctx.ir.stream),
        Protocol::Gemini => {
            let (_path, body) = gemini::render_upstream_request(&ctx.ir, &cand.upstream_model, ctx.ir.stream);
            body
        }
    }
}

// ---- non-stream attempt: buffer whole body, validate, commit ----
async fn attempt_non_stream(
    cand: &crate::router::Candidate,
    resp: reqwest::Response,
    attempt_deadline: Instant,
) -> Result<Outcome, (ProxyError, ir::Usage)> {
    let text = match tokio::time::timeout_at(tokio::time::Instant::from_std(attempt_deadline), resp.text()).await {
        Err(_) => return Err((ProxyError::Timeout { what: TimeoutWhat::Attempt }, ir::Usage::default())),
        Ok(Err(e)) => return Err((ProxyError::Connect(e.to_string()), ir::Usage::default())),
        Ok(Ok(t)) => t,
    };
    if text.trim().is_empty() {
        return Err((ProxyError::EmptyResponse, ir::Usage::default()));
    }
    let v: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            let head: String = text.chars().take(120).collect();
            return Err((ProxyError::NotSse(format!("non-JSON body: {head}")), ir::Usage::default()));
        }
    };
    let parsed = match cand.protocol {
        Protocol::Openai    => openai::parse_response(&v, &cand.upstream_model),
        Protocol::Anthropic => anthropic::parse_response(&v, &cand.upstream_model),
        Protocol::Gemini    => gemini::parse_response(&v, &cand.upstream_model),
    };
    match parsed {
        Ok(r) => Ok(Outcome::Full(Box::new(r))),
        Err(e) => Err((ProxyError::ErrorBody(e.to_string()), ir::Usage::default())),
    }
}

#[derive(Default)]
struct ToolAssembler;

impl ToolAssembler {
    fn begin(&mut self, _index: usize, _id: &str, _name: &str) {}
    fn begin_if_new(&mut self, _index: usize) {}
}

async fn sleep_until_std(t: Instant) {
    tokio::time::sleep_until(tokio::time::Instant::from_std(t)).await;
}

// ---- streaming attempt: classify frames, commit at first content ----

async fn attempt_stream(
    ctx: &ProxyCtx<'_>,
    cand: &crate::router::Candidate,
    resp: reqwest::Response,
    budget: &crate::router::Budget,
    client_gone: CancellationToken,
    feed_tx: mpsc::Sender<ClientFrame>,
) -> Result<Outcome, (ProxyError, ir::Usage)> {
    use crate::protocol::openai::Classified as CO;
    use crate::protocol::anthropic::Classified as CA;
    use crate::protocol::gemini::Classified as CG;
    use eventsource_stream::Eventsource;
    use futures::StreamExt;

    let mut es = resp.bytes_stream().eventsource();
    let mut usage_acc = ir::Usage::default();


    let mut frame_deadline = Instant::now() + budget.first_event_timeout;

    /// One classified upstream frame. Declared once here because the enum is
    /// not allowed to be the subject of a match while declared inside the loop.
    enum FrameClass {
        Ignore,
        Text(String),
        /// args = arguments that arrived in the SAME frame as the name (a whole
        /// call delivered at once); empty when they stream in as ToolInput.
        ToolUseStart { index: usize, id: String, name: String, args: String },
        ToolInput { index: usize, delta: String },
        ToolCallFull { name: String, args: serde_json::Value },
        Start { input_tokens: u64 },
        Finish { reason: ir::FinishReason, usage: Option<ir::Usage> },
        Error(String),
        Done,
    }

    // PRE-COMMIT loop: zero client bytes written in this loop.
    let committed = loop {
        let next = tokio::select! {
            ev = es.next() => ev,
            _ = sleep_until_std(frame_deadline) => {
                return Err((ProxyError::Timeout { what: TimeoutWhat::FirstEvent }, usage_acc));
            }
            _ = client_gone.cancelled() => {
                return Err((ProxyError::ClientGone, usage_acc));
            }
        };
        frame_deadline = Instant::now() + budget.chunk_idle_timeout;

        let Some(ev) = next else {
            return Err((ProxyError::NotSse("stream ended before any content".into()), usage_acc));
        };
        let Ok(ev) = ev else { continue }; // malformed SSE chunk: skip
        let data = ev.data.trim().to_string();
        if data.is_empty() { continue }

        let fc = match cand.protocol {
            Protocol::Openai => match openai::classify_upstream_frame(&data) {
                CO::Ignore => FrameClass::Ignore,
                CO::Text(t) => FrameClass::Text(t),
                CO::ToolUseStart { index, id, name, args } => FrameClass::ToolUseStart { index, id, name, args },
                CO::ToolInput { index, delta } => FrameClass::ToolInput { index, delta },
                CO::Finish { reason, usage } => FrameClass::Finish { reason, usage: Some(usage) },
                CO::Error(m) => FrameClass::Error(m),
                CO::Done => FrameClass::Done,
            },
            Protocol::Anthropic => match anthropic::classify_upstream_event(&ev.event, &data) {
                CA::Ignore => FrameClass::Ignore,
                CA::Start { input_tokens } => FrameClass::Start { input_tokens },
                CA::Text(t) => FrameClass::Text(t),
                CA::ToolUseStart { index, id, name, args } => FrameClass::ToolUseStart { index, id, name, args },
                CA::ToolInput { index, delta } => FrameClass::ToolInput { index, delta },
                CA::Finish { reason, usage } => FrameClass::Finish { reason, usage: Some(usage) },
                CA::Error(m) => FrameClass::Error(m),
                CA::Done => FrameClass::Done,
            },
            Protocol::Gemini => match gemini::classify_upstream_frame(&data) {
                CG::Ignore => FrameClass::Ignore,
                CG::Text(t) => FrameClass::Text(t),
                CG::ToolCall { name, args } => FrameClass::ToolCallFull { name, args },
                CG::Finish { reason, usage } => FrameClass::Finish { reason, usage: Some(usage) },
                CG::Error(m) => FrameClass::Error(m),
            },
        };

        match fc {
            FrameClass::Ignore => continue,
            FrameClass::Start { input_tokens } => {
                // anthropic message_start: usage only, not commit-worthy alone
                usage_acc.input_tokens = usage_acc.input_tokens.max(input_tokens);
                continue;
            }
            FrameClass::Error(m) => return Err((ProxyError::ErrorBody(m), usage_acc)),
            FrameClass::Done => {
                return Err((ProxyError::EmptyResponse, usage_acc));
            }
            FrameClass::Text(t) => break Some(StreamEvent::TextDelta { text: t }),
            FrameClass::ToolUseStart { index, id, name, args } => {
                // A whole call delivered in one frame (some OpenAI-compatible
                // providers do this) commits with its args already in hand;
                // otherwise the arguments stream in afterwards.
                if args.is_empty() {
                    break Some(StreamEvent::ToolUseStart { index, id, name, args: None });
                }
                let parsed = serde_json::from_str::<serde_json::Value>(&args)
                    .unwrap_or(serde_json::Value::String(args));
                break Some(StreamEvent::ToolUseStart { index, id, name, args: Some(parsed) });
            }
            FrameClass::ToolInput { index, delta } => {
                break Some(StreamEvent::ToolInputDelta { index, partial_json: delta })
            }
            FrameClass::ToolCallFull { name, args } => {
                // Gemini sends the whole call — name AND complete args — in one
                // frame. Carry the args through instead of discarding them.
                let index = 0usize;
                let id = format!("call_{name}");
                break Some(StreamEvent::ToolUseStart { index, id, name, args: Some(args) });
            }
            FrameClass::Finish { reason, usage } => {
                if let Some(u) = usage {
                    usage_acc = merge_usage_max(usage_acc, u);
                }
                // A Finish without content is still a valid (empty) completion.
                break None;
            }
        }
    };

    // COMMITTED (or empty-but-valid). Client still received zero bytes.
    let (usage_tx, usage_rx) = tokio::sync::oneshot::channel::<ir::Usage>();
    let entry = ctx.entry;
    let exposed_name = ctx.exposed_model.clone();
    let idle = budget.chunk_idle_timeout;
    let deadline_total = budget.deadline;
    let proto = cand.protocol;

    tokio::spawn(async move {
        let mut tx = Some(usage_tx);
        let mut final_usage = usage_acc;
        let feed = feed_tx.clone();

        // POST-COMMIT pump: convert frames to IR events, render in client protocol.
        let mut finished = committed.is_none();
        if finished {
            feed_close_clean(&feed, entry).await;
            if let Some(tx) = tx.take() { let _ = tx.send(final_usage); }
            return;
        }

        // Gemini's functionCall is whole-package, but an OpenAI/Anthropic
        // upstream delivers a call as a start frame plus argument fragments.
        // Gemini's downstream wire form cannot express a partial call, so we
        // assemble the fragments here and emit exactly ONE whole-package
        // ToolUseStart — dropping them would hand the client a call with no
        // arguments at all.
        let mut pending_tool: Option<(usize, String, String, String)> = None;

        // Emit the commit event(s) that triggered the commit. It goes through
        // the SAME assembly as pump frames: when the commit event IS the tool
        // call (a whole-package Gemini frame, or an incremental start), emitting
        // it directly would render a call with no arguments at all.
        match committed {
            Some(StreamEvent::ToolUseStart { index, id, name, args }) => match args {
                // Whole call already (Gemini upstream): pass straight through.
                Some(a) => {
                    let ev = StreamEvent::ToolUseStart { index, id, name, args: Some(a) };
                    feed_emit(&feed, entry, &ev, &exposed_name).await;
                }
                // Incremental start: hold it back until its args arrive.
                None => pending_tool = Some((index, id, name, String::new())),
            },
            Some(StreamEvent::ToolInputDelta { index, partial_json }) => {
                // Upstream opened with arguments directly (no start frame seen
                // pre-commit): buffer them so the call is still emitted whole.
                pending_tool = Some((index, String::new(), String::new(), partial_json));
            }
            Some(ev) => feed_emit(&feed, entry, &ev, &exposed_name).await,
            None => {}
        }

        loop {
            let next = tokio::select! {
                ev = es.next() => ev,
                _ = sleep_until_std(Instant::now() + idle) => {
                    feed_repair(&feed, entry, "upstream chunk idle timeout").await;
                    if let Some(tx) = tx.take() { let _ = tx.send(final_usage); }
                    return;
                }
                _ = sleep_until_std(deadline_total) => {
                    feed_repair(&feed, entry, "total deadline exceeded").await;
                    if let Some(tx) = tx.take() { let _ = tx.send(final_usage); }
                    return;
                }
                _ = client_gone.cancelled() => {
                    // client vanished: stop pumping; drop closes upstream conn
                    if let Some(tx) = tx.take() { let _ = tx.send(final_usage); }
                    return;
                }
            };
            let Some(ev) = next else {
                if !finished {
                    feed_repair(&feed, entry, "upstream stream ended prematurely").await;
                } else {
                    feed_close_clean(&feed, entry).await;
                }
                if let Some(tx) = tx.take() { let _ = tx.send(final_usage); }
                return;
            };
            let Ok(ev) = ev else { continue };
            let data = ev.data.trim().to_string();
            if data.is_empty() { continue }

            let fc = match proto {
                Protocol::Openai => match openai::classify_upstream_frame(&data) {
                    CO::Ignore => FrameClass::Ignore,
                    CO::Text(t) => FrameClass::Text(t),
                    CO::ToolUseStart { index, id, name, args } => FrameClass::ToolUseStart { index, id, name, args },
                    CO::ToolInput { index, delta } => FrameClass::ToolInput { index, delta },
                    CO::Finish { reason, usage } => FrameClass::Finish { reason, usage: Some(usage) },
                    CO::Error(m) => FrameClass::Error(m),
                    CO::Done => FrameClass::Done,
                },
                Protocol::Anthropic => match anthropic::classify_upstream_event(&ev.event, &data) {
                    CA::Ignore => FrameClass::Ignore,
                    CA::Start { input_tokens } => FrameClass::Start { input_tokens },
                    CA::Text(t) => FrameClass::Text(t),
                    CA::ToolUseStart { index, id, name, args } => FrameClass::ToolUseStart { index, id, name, args },
                    CA::ToolInput { index, delta } => FrameClass::ToolInput { index, delta },
                    CA::Finish { reason, usage } => FrameClass::Finish { reason, usage: Some(usage) },
                    CA::Error(m) => FrameClass::Error(m),
                    CA::Done => FrameClass::Done,
                },
                Protocol::Gemini => match gemini::classify_upstream_frame(&data) {
                    CG::Ignore => FrameClass::Ignore,
                    CG::Text(t) => FrameClass::Text(t),
                    CG::ToolCall { name, args } => FrameClass::ToolCallFull { name, args },
                    CG::Finish { reason, usage } => FrameClass::Finish { reason, usage: Some(usage) },
                    CG::Error(m) => FrameClass::Error(m),
                },
            };

            match fc {
                FrameClass::Ignore => continue,
                FrameClass::Start { input_tokens } => {
                    final_usage.input_tokens = final_usage.input_tokens.max(input_tokens);
                }
                FrameClass::Error(m) => {
                    // mid-stream upstream error AFTER commit: repair, never leak
                    feed_repair(&feed, entry, &m).await;
                    if let Some(tx) = tx.take() { let _ = tx.send(final_usage); }
                    return;
                }
                FrameClass::Text(t) => feed_emit(&feed, entry, &StreamEvent::TextDelta { text: t }, &exposed_name).await,
                FrameClass::ToolUseStart { index, id, name, args } => {
                    // Incremental upstream call: hold it back until the args
                    // are complete so the client sees ONE well-formed call.
                    flush_tool(&feed, entry, &exposed_name, &mut pending_tool).await;
                    pending_tool = Some((index, id, name, args));
                }
                FrameClass::ToolInput { index: _, delta } => {
                    if let Some(p) = pending_tool.as_mut() { p.3.push_str(&delta); }
                }
                FrameClass::ToolCallFull { name, args } => {
                    // Whole call in one frame (Gemini): pass it straight through.
                    flush_tool(&feed, entry, &exposed_name, &mut pending_tool).await;
                    let ev = StreamEvent::ToolUseStart {
                        index: 0, id: format!("call_{name}"), name, args: Some(args),
                    };
                    feed_emit(&feed, entry, &ev, &exposed_name).await;
                }
                FrameClass::Finish { reason, usage } => {
                    if let Some(u) = usage { final_usage = merge_usage_max(final_usage, u); }
                    // The call is complete once the upstream stops: release it
                    // before the finish frame so it cannot be stranded.
                    flush_tool(&feed, entry, &exposed_name, &mut pending_tool).await;
                    finished = true;
                    feed_emit(&feed, entry, &StreamEvent::Finish { reason, usage: final_usage.clone() }, &exposed_name).await;
                }
                FrameClass::Done => {
                    if !finished {
                        feed_repair(&feed, entry, "upstream closed without finish").await;
                    } else {
                        feed_close_clean(&feed, entry).await;
                    }
                    if let Some(tx) = tx.take() { let _ = tx.send(final_usage); }
                    return;
                }
            }
        }
    });

    Ok(Outcome::Stream { usage_rx })
}

pub fn merge_usage_max(a: ir::Usage, b: ir::Usage) -> ir::Usage {
    ir::Usage {
        input_tokens: a.input_tokens.max(b.input_tokens),
        output_tokens: a.output_tokens.max(b.output_tokens),
        cache_read_tokens: a.cache_read_tokens.max(b.cache_read_tokens),
        cache_creation_tokens: a.cache_creation_tokens.max(b.cache_creation_tokens),
    }
}

// ===== client feed =====
//
// The pump writes rendered SSE frames into an mpsc channel; the HTTP handler
// (routes.rs) drains it into the response body. Before commit no frames are
// produced => I1 holds by construction.
//
// mpsc, NOT broadcast: a broadcast ring buffer silently drops the OLDEST
// frames once it is full, which would truncate a long stream behind a slow
// client with no error anywhere. mpsc::send().await applies backpressure
// instead, so the client's socket speed throttles the upstream read loop and
// no frame can ever be lost. It also has exactly one consumer, which is what
// this per-request channel has.

use tokio::sync::mpsc;

#[derive(Clone)]
pub enum ClientFrame {
    Sse(String),
    Close,
}

pub fn new_client_channel() -> (mpsc::Sender<ClientFrame>, mpsc::Receiver<ClientFrame>) {
    mpsc::channel(256)
}

/// Emit an assembled incremental tool call as ONE whole-package IR event.
/// The buffer is the concatenation of the upstream's argument fragments.
async fn flush_tool(
    feed: &mpsc::Sender<ClientFrame>,
    entry: EntryProtocol,
    exposed_model: &str,
    pending: &mut Option<(usize, String, String, String)>,
) {
    let Some((index, id, name, buf)) = pending.take() else { return };
    // Keep a non-JSON buffer verbatim rather than collapsing it to an empty
    // object: inventing {} is exactly the corruption this assembly prevents.
    let args = match serde_json::from_str::<serde_json::Value>(&buf) {
        Ok(v) => v,
        Err(_) if buf.trim().is_empty() => serde_json::json!({}),
        Err(_) => serde_json::Value::String(buf),
    };
    let ev = StreamEvent::ToolUseStart { index, id, name, args: Some(args) };
    feed_emit(feed, entry, &ev, exposed_model).await;
}

/// Render one IR event in the client's protocol and push it into `tx`.
pub async fn feed_emit(tx: &mpsc::Sender<ClientFrame>, entry: EntryProtocol, ev: &StreamEvent, exposed_model: &str) {
    let frames: Vec<String> = match entry {
        EntryProtocol::Openai => openai::render_stream_event(exposed_model, ev).into_iter().map(|v| format!("data: {v}\n\n")).collect(),
        EntryProtocol::Anthropic => anthropic::render_stream_event(exposed_model, ev).into_iter().map(|(name, v)| format!("event: {name}\ndata: {v}\n\n")).collect(),
        EntryProtocol::Gemini => gemini::render_stream_event(exposed_model, ev).into_iter().map(|v| format!("data: {v}\n\n")).collect(),
    };
    // last-frame delivery handled by feed_close_*; emit only data here.
    // A send error means the handler dropped the receiver (client gone), so
    // the pump should stop — it is not a frame we may silently skip.
    let _ = tx.send(ClientFrame::Sse(frames.concat())).await;
}

/// Clean end: protocol terminator ([DONE] / message_stop) + Close.
pub async fn feed_close_clean(tx: &mpsc::Sender<ClientFrame>, entry: EntryProtocol) {
    let tail = match entry {
        EntryProtocol::Openai => "data: [DONE]\n\n".to_string(),
        EntryProtocol::Anthropic => String::new(), // message_stop already emitted by Finish
        EntryProtocol::Gemini => String::new(),
    };
    if !tail.is_empty() { let _ = tx.send(ClientFrame::Sse(tail)).await; }
    let _ = tx.send(ClientFrame::Close).await;
}

/// Broken-stream repair: a final error frame in the client's protocol, then
/// terminator + Close. The client SDK must never mistake this for success.
pub async fn feed_repair(tx: &mpsc::Sender<ClientFrame>, entry: EntryProtocol, msg: &str) {
    match entry {
        EntryProtocol::Openai => {
            let err = serde_json::json!({
                "error": { "message": format!("upstream interrupted: {msg}"), "type": "gateway_upstream_interrupted", "code": "upstream_interrupted" }
            });
            let _ = tx.send(ClientFrame::Sse(format!("data: {err}\n\n"))).await;
            let ev = StreamEvent::Finish { reason: ir::FinishReason::ContentFilter, usage: ir::Usage::default() };
            let finish = openai::render_stream_event("unknown", &ev);
            for v in finish { let _ = tx.send(ClientFrame::Sse(format!("data: {v}\n\n"))).await; }
            let _ = tx.send(ClientFrame::Sse("data: [DONE]\n\n".to_string())).await;
        }
        EntryProtocol::Anthropic => {
            let err = serde_json::json!({
                "type": "error", "error": { "type": "api_error", "message": format!("upstream interrupted: {msg}") }
            });
            let _ = tx.send(ClientFrame::Sse(format!("event: error\ndata: {err}\n\n"))).await;
        }
        EntryProtocol::Gemini => {
            let err = serde_json::json!({ "error": { "code": 502, "message": format!("upstream interrupted: {msg}"), "status": "UNAVAILABLE" } });
            let _ = tx.send(ClientFrame::Sse(format!("data: {err}\n\n"))).await;
        }
    }
    let _ = tx.send(ClientFrame::Close).await;
}

// NOTE: feed channels are per-request (created in routes.rs), never global —
// a process-global slot would cross-talk frames between concurrent streams.

