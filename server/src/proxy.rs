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

use crate::config::{effective_retry, ConfigSnapshot, Protocol};
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
    /// Requests occupying each upstream account RIGHT NOW. This is the only
    /// source of truth for "使用中" — not the last-success row, not the
    /// static tier-1 reference highlight.
    pub inflight: Arc<crate::inflight::Inflight>,
}

// Cloneable for axum::with_state (all fields are Arc/Client/Clone types)
impl Clone for AppState {
    fn clone(&self) -> Self {
        Self {
            http: self.http.clone(),
            config: self.config.clone(),
            health: self.health.clone(),
            store: self.store.clone(),
            inflight: self.inflight.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryProtocol {
    Openai,
    Anthropic,
    Gemini,
}

/// What the post-commit pump finally delivered. Sent over the oneshot because
/// the truncation decision is made AFTER the request row's other fields are
/// already fixed by the caller: a mid-stream loop hit can only be known once
/// the answer is being relayed.
#[derive(Debug, Clone, Copy, Default)]
pub struct StreamOutcome {
    pub usage: ir::Usage,
    /// The answer was cut short because the model repeated itself (either the
    /// wrap-up that closes an answer whose correction rounds ran out, or a
    /// post-commit truncation). The client received a valid, complete-looking
    /// stream — that is the point of the clean wrap-up — so this flag is the
    /// ONLY record that the answer is not what the model meant to write. It
    /// must reach the request log.
    pub loop_truncated: bool,
    /// How many correction rounds this answer needed: the model repeated
    /// itself, the gateway told it why and asked it to carry on, and it did.
    /// 0 means the answer never looped. Unlike `loop_truncated` this is not a
    /// defect marker — a corrected answer is a delivered answer.
    pub loop_continues: u32,
}

pub enum Outcome {
    /// non-stream: full parsed response (usage included)
    Full(Box<ChatResponse>),
    /// stream committed: outcome arrives via oneshot when the pump ends
    Stream {
        outcome_rx: tokio::sync::oneshot::Receiver<StreamOutcome>,
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
    /// What THIS attempt had already billed upstream when it ended. A failed or
    /// timed-out attempt can still have consumed real tokens (the upstream
    /// streamed a usage frame, or our partial accumulator saw one), and those
    /// tokens are money the user paid for an answer they never got — that is
    /// exactly what `wasted_cost_usd` prices. Stays 0 when nothing was
    /// observed: an unobserved count is not evidence of zero cost, it is
    /// absence of evidence, and the normal path has no usage to price either.
    pub usage: ir::Usage,
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
    // The running maximum *before* the attempt in flight. attempt_once reports a
    // monotonic running max, so the per-attempt usage is that max minus this.
    let mut usage_baseline = ir::Usage::default();
    let mut ttft: Option<u64> = None;

    let Some(snap) = ctx.state.config.get() else {
        return empty_run(Terminal::NoCandidate, started);
    };
    let Some(group) = snap.config.groups.iter().find(|g| g.id == ctx.group_id) else {
        return empty_run(Terminal::NoCandidate, started);
    };
    // Per-group settings: a group with its own overrides.retry drives the whole
    // walk (budget + cooldown_wait) from that copy; every other group borrows the
    // global config, so its behaviour is bit-identical to before this existed.
    let rc = effective_retry(&snap.config, group);
    let budget = build_budget(rc, started);

    let mut walk = WalkState { idx: 0, cycle: 0, attempts: 0 };
    let mut plan = build_plan(&snap.config, group, &ctx.exposed_model, &ctx.state.health);

    loop {
        let now = Instant::now();
        match next_step(&mut walk, &plan, &budget, now, &ctx.state.health, rc.cooldown_wait) {
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
                            usage: resp.usage.clone(),
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
                    Ok(Outcome::Stream { outcome_rx }) => {
                        ctx.state.health.record_success(&CooldownKey::scoped(&candidate.channel_id, &candidate.upstream_model));
                        if ttft.is_none() { ttft = Some(latency); }
                        records.push(SwitchRecord {
                            attempt_no: walk.attempts, tier: candidate.tier,
                            channel_id: candidate.channel_id.clone(), channel_name: candidate.channel_name.clone(),
                            upstream_model: candidate.upstream_model.clone(),
                            error_class: "committed", http_status: Some(200), latency_ms: latency,
                            message: String::new(),
                            // A stream that commits here has not finished yet, so the
                            // final usage does not exist at this point. It arrives later
                            // via usage_rx and is priced into cost_usd by the caller.
                            usage: ir::Usage::default(),
                        });
                        return RunResult {
                            terminal: Terminal::Exhausted,
                            records,
                            success: Some(Outcome::Stream { outcome_rx }),
                            usage: usage_total,
                            ttft_ms: ttft,
                            duration_ms: started.elapsed().as_millis() as u64,
                            final_channel: Some(candidate.channel_id),
                            final_upstream_model: Some(candidate.upstream_model),
                        };
                    }
                    Err((perr, partial)) => {
                        let baseline = usage_baseline;
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
                            // What THIS attempt burned before it failed. Deliberately
                            // the per-attempt remainder, not the running maximum: a
                            // failed attempt can only ever be priced when it observed
                            // usage itself, and a monotonic running max would re-charge
                            // an earlier attempt's tokens. `partial` holds the largest
                            // accumulator THIS attempt reached, so subtracting the
                            // pre-attempt high-water mark is safe (`saturating_sub`
                            // clamps any stale baseline to 0 instead of wrapping).
                            usage: ir::Usage {
                                input_tokens: partial.input_tokens.saturating_sub(baseline.input_tokens),
                                output_tokens: partial.output_tokens.saturating_sub(baseline.output_tokens),
                                cache_read_tokens: partial.cache_read_tokens.saturating_sub(baseline.cache_read_tokens),
                                cache_creation_tokens: partial.cache_creation_tokens.saturating_sub(baseline.cache_creation_tokens),
                            },
                        });
                        usage_baseline = usage_total;
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

/// Human-readable Chinese cause for one failed attempt. This is what the
/// console shows in the request log and the switch log, so it must name the
/// CAUSE, not merely the error's Rust variant: "上游返回 401（鉴权失败）" tells
/// the operator what to fix, "UpstreamStatus" does not.
pub fn proxy_err_message(e: &ProxyError) -> String {
    match e {
        ProxyError::UpstreamStatus { status, class } => match class {
            crate::proxy::StatusClass::Auth => format!("上游返回 {status}：鉴权失败（API Key 无效或已过期）"),
            crate::proxy::StatusClass::ModelNotFound => format!("上游返回 {status}：该渠道没有这个模型"),
            crate::proxy::StatusClass::RateLimited { .. } => format!("上游返回 {status}：被限流"),
            crate::proxy::StatusClass::BadRequest => format!("上游返回 {status}：请求被拒绝"),
            crate::proxy::StatusClass::Server => format!("上游返回 {status}：上游服务端错误"),
            crate::proxy::StatusClass::ContentFilter => format!("上游返回 {status}：内容被安全策略拦截"),
        },
        ProxyError::Timeout { what } => format!("超时：{}", match what {
            TimeoutWhat::Connect => "连接上游超时",
            TimeoutWhat::Ttfb => "上游首字节超时",
            TimeoutWhat::FirstEvent => "上游第一个事件超时",
            TimeoutWhat::Attempt => "本次尝试超时",
        }),
        ProxyError::Connect(m) => format!("连接上游失败：{m}"),
        ProxyError::ErrorBody(m) => format!("上游返回错误内容：{m}"),
        ProxyError::NotSse(m) => format!("上游返回的不是有效流：{m}"),
        ProxyError::EmptyResponse => "上游返回空内容".into(),
        ProxyError::DegenerateOutput { unit, repeats } => format!(
            "输出复读：同一段内容连续重复了 {repeats} 次（重复单元 {} 个字符：{}）",
            unit.chars().count(),
            unit.chars().take(40).collect::<String>(),
        ),
        ProxyError::ClientGone => "客户端已断开".into(),
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
    // In-flight occupancy starts HERE, before the socket is opened, and ends
    // when this attempt stops using the account. For a non-stream attempt that
    // is this function's return; for a committed stream it is the end of the
    // POST-COMMIT pump, so the guard is handed to attempt_stream and moved into
    // the pump task. Every early return below (timeout, connect error, non-2xx)
    // drops it on the way out, which is exactly the accounting we want.
    let inflight = ctx.state.inflight.enter(&cand.channel_id, &cand.upstream_model);
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
        true  => attempt_stream(ctx, cand, resp, budget, client_gone, feed_tx, inflight).await,
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
    render_upstream_body_for(&ctx.ir, cand)
}

fn render_upstream_body_for(ir: &ChatRequest, cand: &crate::router::Candidate) -> serde_json::Value {
    match cand.protocol {
        Protocol::Openai => openai::render_upstream_request(ir, &cand.upstream_model, ir.stream),
        Protocol::Anthropic => anthropic::render_upstream_request(ir, &cand.upstream_model, ir.stream),
        Protocol::Gemini => {
            let (_path, body) = gemini::render_upstream_request(ir, &cand.upstream_model, ir.stream);
            body
        }
    }
}

// ===== loop correction: "you are repeating, carry on from where you were cut" =====

/// How much of the model's own already-written text is handed back to it in a
/// correction round. Bounded: this is context for "where was I", not a second
/// copy of the answer, and the request may already be long.
const CONTINUE_CONTEXT_CHARS: usize = 4000;

/// The IR for one correction round: the original conversation, plus what the
/// model had already written (when any of it survived the cut), plus the reason
/// it was interrupted.
///
/// The reason is not optional. A model told only "continue" tends to restart
/// the same loop; a model told WHY it was cut — it was repeating — resumes the
/// actual task. `kept` is omitted when empty rather than sent as an empty
/// assistant turn, which some providers reject.
fn continuation_ir(base: &ChatRequest, kept: &str, prompt: &str) -> ChatRequest {
    let mut ir = base.clone();
    if !kept.trim().is_empty() {
        let tail: String = {
            let n = kept.chars().count();
            let skip = n.saturating_sub(CONTINUE_CONTEXT_CHARS);
            kept.chars().skip(skip).collect()
        };
        ir.messages.push(ir::Message {
            role: ir::Role::Assistant,
            content: vec![ir::Content::Text { text: tail }],
            tool_call_id: None,
        });
    }
    ir.messages.push(ir::Message {
        role: ir::Role::User,
        content: vec![ir::Content::Text { text: prompt.to_string() }],
        tool_call_id: None,
    });
    ir
}

/// May one more correction round start?
///
/// Three things must hold. The round cap and the request's total deadline, because
/// a correction is a real upstream call and must fit in the budget the walk was
/// given rather than extend it. And `stuck`: a model that repeats the SAME unit
/// again right after being told it is repeating is not going to recover — each
/// further round would only relay more of the same loop before the guard could
/// cut it again, so the loop stops here instead of burning the remaining cap.
fn can_continue_now(
    cfg: &crate::loopguard::LoopCfg,
    done: u32,
    deadline: Instant,
    ttfb: Duration,
    stuck: bool,
) -> bool {
    if !cfg.continue_enabled || done >= cfg.continue_max || stuck {
        return false;
    }
    let hard = deadline.checked_sub(ttfb).unwrap_or(deadline);
    Instant::now() < hard
}

/// Open one correction round against the SAME candidate. Same channel and same
/// upstream model on purpose: this is the answer being continued, not a new
/// answer, so switching candidates mid-answer would splice two different models
/// together.
async fn open_continuation(
    http: &HttpPool,
    cand: &crate::router::Candidate,
    connect_timeout: Duration,
    ttfb_timeout: Duration,
    ir: &ChatRequest,
) -> Result<reqwest::Response, ProxyError> {
    let url = format!("{}{}", build_upstream_url(cand), upstream_path_suffix(cand, ir));
    let body = render_upstream_body_for(ir, cand);
    let req = http
        .client_for(connect_timeout.as_millis() as u64)
        .post(&url)
        .headers(upstream_headers(cand))
        .json(&body);
    let resp = match tokio::time::timeout(ttfb_timeout, req.send()).await {
        Err(_) => return Err(ProxyError::Timeout { what: TimeoutWhat::Ttfb }),
        Ok(Err(e)) => return Err(ProxyError::Connect(e.to_string())),
        Ok(Ok(r)) => r,
    };
    let status = resp.status();
    if !status.is_success() {
        let sc = status_class(status.as_u16());
        let _ = resp.text().await; // drain
        return Err(ProxyError::UpstreamStatus { status: status.as_u16(), class: sc });
    }
    Ok(resp)
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

/// Start one POST-COMMIT correction round: same candidate, same channel, and a
/// prompt that states the reason. Returns the replacement stream plus the
/// re-seeded window state, or `None` when no round can start (cap reached, no
/// budget left, or the continuation could not be opened) — in which case the
/// caller must finish the answer at the cut instead of continuing to relay a
/// stream that is still repeating.
#[allow(clippy::too_many_arguments)]
async fn start_post_commit_round(
    http: &HttpPool,
    cand: &crate::router::Candidate,
    base_ir: &ChatRequest,
    cfg: &crate::loopguard::LoopCfg,
    hit: &crate::loopguard::LoopHit,
    guard: &mut crate::loopguard::LoopGuard,
    continues: u32,
    deadline: Instant,
    ttfb: Duration,
    connect: Duration,
    last_unit: &Option<String>,
) -> Option<reqwest::Response> {
    // Same unit as the round that just failed => the model is stuck on one
    // phrase and another round cannot help (see `can_continue_now`).
    let stuck = last_unit.as_deref() == Some(hit.unit.as_str());
    if !can_continue_now(cfg, continues, deadline, ttfb, stuck) {
        return None;
    }
    // Everything before the repetition is answer text the client ALREADY has,
    // so only the tail is re-sent as context — the whole answer would waste the
    // continuation's context window.
    let kept = guard.text_before(hit.real_span);
    let prompt = crate::loopguard::render_continue_prompt(&cfg.continue_prompt, hit);
    let ir = continuation_ir(base_ir, &kept, &prompt);
    match open_continuation(http, cand, connect, ttfb, &ir).await {
        Ok(resp) => {
            // Re-seed only on success: a failed round must leave the window
            // intact so the cut still points at the real repetition.
            guard.reseed(&kept);
            Some(resp)
        }
        Err(_) => None,
    }
}

async fn attempt_stream(
    ctx: &ProxyCtx<'_>,
    cand: &crate::router::Candidate,
    resp: reqwest::Response,
    budget: &crate::router::Budget,
    client_gone: CancellationToken,
    feed_tx: mpsc::Sender<ClientFrame>,
    // Occupancy for this attempt, owned here so it can be moved into the pump
    // task that outlives this function. A PRE-COMMIT failure returns early and
    // drops it with the attempt, which is what "stopped using the account"
    // means.
    inflight: crate::inflight::InflightGuard,
) -> Result<Outcome, (ProxyError, ir::Usage)> {
    use crate::protocol::openai::Classified as CO;
    use crate::protocol::anthropic::Classified as CA;
    use crate::protocol::gemini::Classified as CG;
    use eventsource_stream::Eventsource;
    use futures::StreamExt;

    let mut es = resp.bytes_stream().eventsource();
    let mut usage_acc = ir::Usage::default();

    // Degenerate-output detection plus the holdback buffer it needs. Both live
    // entirely inside the PRE-COMMIT window, so a hit costs the client zero
    // bytes. `hold == 0` (guard disabled, or a config that clamped it away)
    // commits the very first text frame, i.e. exactly the old behaviour.
    // One guard for the whole attempt: it keeps the rolling window across the
    // commit point and is moved into the pump task below, so a loop that
    // STRADDLES the commit is judged on the text that actually surrounds it.
    // The pre-commit window is the evidence that the tail is a loop rather than
    // an ordinary short answer, and every text frame reaches the guard before
    // it reaches `pending`, so no committed character is missing from it.
    let mut guard = crate::loopguard::LoopGuard::new(&budget.loop_guard);
    let mut pending = String::new();
    let hold = budget.loop_guard.min_chars;
    // Wall-clock ceiling on the holdback. `hold` counts CHARACTERS, which says
    // nothing about how long filling them takes: a fast model fills 64 in 160 ms
    // while a 2 chars/s model would take 32 s. The cap is what keeps a slow model
    // from turning the holdback into a multi-second first-byte delay, and it is
    // why the first byte is bounded by TIME rather than by generation speed.
    //
    // The clock starts at the FIRST TEXT FRAME, not at the attempt: time spent
    // waiting for the upstream to think is not the holdback's doing, and a model
    // that thinks for five seconds should not then also be charged the full
    // character holdback before the client sees anything.
    let hold_max = Duration::from_millis(budget.loop_guard.hold_max_ms);
    let mut hold_started: Option<Instant> = None;
    // The finish reason the commit itself carried, when the frame that flushed
    // the holdback buffer was also the frame that ended the answer.
    let mut commit_finish: Option<ir::FinishReason> = None;

    let mut frame_deadline = Instant::now() + budget.first_event_timeout;

    // Set when the answer was wrapped up at a loop cut rather than finished by
    // the model. It is the sole record that the answer the client received is
    // a shortened one, and it survives into the pump's outcome.
    let mut loop_truncated = false;

    // Thinking seen before the commit point. It cannot be sent as it arrives:
    // the pre-commit region may still end in a retry, and a retry that had
    // already streamed the reasoning would make the client show it twice.
    // Collecting it here keeps the order and replays it as part of the commit.
    let mut pre_reasoning: Vec<StreamEvent> = Vec::new();

    /// One classified upstream frame. Declared once here because the enum is
    /// not allowed to be the subject of a match while declared inside the loop.
    enum FrameClass {
        Ignore,
        Text(String),
        /// Model thinking. Never enters the loop guard and never joins
        /// `pending`: reasoning is not the answer, so neither a repetition in
        /// it nor delaying it should affect the client's prose.
        Reasoning(String),
        /// One upstream delta that carried a thought AND answer text.
        ReasoningAndText { reasoning: String, text: String },
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
    // The commit value is a LIST: flushing the holdback buffer and the frame
    // that ended the holdback are one and the same commit, so a tool call
    // arriving after buffered text must carry that text with it instead of
    // discarding it.
    //
    // ROUNDS: a loop hit no longer throws this attempt away. Everything the
    // model wrote BEFORE it started repeating is real answer text, so the
    // repetition is cut out, that text is kept, and the model is asked - with
    // the reason stated in the prompt - to carry on from where it was cut off.
    // Only when no round can start (cap reached, or no budget left) is the
    // answer wrapped up where it stands; it is never discarded whole.
    let mut continues: u32 = 0;
    // Handed out of the inner frame loop by the loop-guard arms below.
    let mut looped: Option<crate::loopguard::LoopHit> = None;
    // Length of the frame that triggered the hit. It reached the guard but not
    // `pending`, which is what makes the pending cut exact (see below).
    let mut trigger_len: usize = 0;
    // The repeated unit of the last correction round, for the stuck check.
    let mut last_unit: Option<String> = None;
    // True once the upstream stream has been deliberately abandoned. The pump
    // must then finish the answer instead of reading a stream we walked away
    // from, which would relay the very repetition that was just cut.
    let mut upstream_done = false;
    let committed_tail: Vec<StreamEvent> = loop {
        looped = None;
        let tail: Vec<StreamEvent> = loop {
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
            // The stream ended: buffered text is real content and must not be
            // dropped, so it becomes the committed event.
            if !pending.is_empty() {
                break vec![StreamEvent::TextDelta {
                    text: std::mem::take(&mut pending),
                    finish: commit_finish,
                }];
            }
            return Err((ProxyError::NotSse("stream ended before any content".into()), usage_acc));
        };
        let Ok(ev) = ev else { continue }; // malformed SSE chunk: skip
        let data = ev.data.trim().to_string();
        if data.is_empty() { continue }

        let fc = match cand.protocol {
            Protocol::Openai => match openai::classify_upstream_frame(&data) {
                CO::Ignore => FrameClass::Ignore,
                CO::Text(t) => FrameClass::Text(t),
                CO::Reasoning(t) => FrameClass::Reasoning(t),
                CO::ReasoningAndText { reasoning, text } => FrameClass::ReasoningAndText { reasoning, text },
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
                CA::Reasoning(t) => FrameClass::Reasoning(t),
                CA::ToolUseStart { index, id, name, args } => FrameClass::ToolUseStart { index, id, name, args },
                CA::ToolInput { index, delta } => FrameClass::ToolInput { index, delta },
                CA::Finish { reason, usage } => FrameClass::Finish { reason, usage: Some(usage) },
                CA::Error(m) => FrameClass::Error(m),
                CA::Done => FrameClass::Done,
            },
            Protocol::Gemini => match gemini::classify_upstream_frame(&data) {
                CG::Ignore => FrameClass::Ignore,
                CG::Text(t) => FrameClass::Text(t),
                CG::Reasoning(t) => FrameClass::Reasoning(t),
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
                // The upstream declared the stream finished: buffered text is
                // real content and must not be dropped.
                if !pending.is_empty() {
                    // No Finish frame was seen (a Finish always breaks this
                    // loop), so [DONE] itself is the end of a complete answer.
                    commit_finish = Some(ir::FinishReason::Stop);
                    break vec![StreamEvent::TextDelta {
                        text: std::mem::take(&mut pending),
                        finish: commit_finish,
                    }];
                }
                return Err((ProxyError::EmptyResponse, usage_acc));
            }
            FrameClass::Reasoning(t) => {
                // Thinking is held separately and never enters `pending`. It is
                // not part of the answer, so a repetition inside a scratchpad is
                // not the degenerate output the guard exists to stop (see
                // docs/loopguard-spec.md §2). It is buffered rather than sent
                // because this region may still end in a retry.
                pre_reasoning.push(StreamEvent::Reasoning { text: t });
                continue;
            }
            FrameClass::ReasoningAndText { reasoning, text } => {
                // One delta, two destinations: the thought is buffered for the
                // commit list, the text goes through the normal holdback and
                // loop-guard path. Dropping either half loses real content.
                pre_reasoning.push(StreamEvent::Reasoning { text: reasoning });
                if let Some(hit) = guard.push(&text) {
                    // Not a failure: hand the hit to the round logic, which keeps
                    // the text written before the repetition and asks the model to
                    // carry on from the cut.
                    trigger_len = text.chars().count();
                    looped = Some(hit);
                    break Vec::new();
                }
                if hold_started.is_none() { hold_started = Some(Instant::now()); }
                pending.push_str(&text);
                if crate::loopguard::holdback_done(
                    pending.chars().count(), hold, hold_max, hold_started.map(|t| t.elapsed()),
                ) {
                    // The post-loop merge prepends `pre_reasoning`, so this
                    // commit list carries the thinking in front of the text.
                    break vec![StreamEvent::TextDelta {
                        text: std::mem::take(&mut pending),
                        finish: None,
                    }];
                }
                continue;
            }
            FrameClass::Text(t) => {
                // Only text is judged. A repetition is a property of prose, not
                // of tool arguments, and delaying the latter would break the
                // tool stream semantics (see docs/loopguard-spec.md §2).
                if let Some(hit) = guard.push(&t) {
                    trigger_len = t.chars().count();
                    looped = Some(hit);
                    break Vec::new();
                }
                if hold_started.is_none() { hold_started = Some(Instant::now()); }
                pending.push_str(&t);
                if crate::loopguard::holdback_done(
                    pending.chars().count(), hold, hold_max, hold_started.map(|t| t.elapsed()),
                ) {
                    break vec![StreamEvent::TextDelta {
                        text: std::mem::take(&mut pending),
                        finish: None,
                    }];
                }
                continue;
            }
            FrameClass::ToolUseStart { index, id, name, args } => {
                // A whole call delivered in one frame (some OpenAI-compatible
                // providers do this) commits with its args already in hand;
                // otherwise the arguments stream in afterwards.
                let mut out = flush_pending(&mut pending);
                if args.is_empty() {
                    out.push(StreamEvent::ToolUseStart { index, id, name, args: None });
                } else {
                    let parsed = serde_json::from_str::<serde_json::Value>(&args)
                        .unwrap_or(serde_json::Value::String(args));
                    out.push(StreamEvent::ToolUseStart { index, id, name, args: Some(parsed) });
                }
                break out;
            }
            FrameClass::ToolInput { index, delta } => {
                let mut out = flush_pending(&mut pending);
                out.push(StreamEvent::ToolInputDelta { index, partial_json: delta });
                break out;
            }
            FrameClass::ToolCallFull { name, args } => {
                // Gemini sends the whole call — name AND complete args — in one
                // frame. Carry the args through instead of discarding them.
                let index = 0usize;
                let id = format!("call_{name}");
                let mut out = flush_pending(&mut pending);
                out.push(StreamEvent::ToolUseStart { index, id, name, args: Some(args) });
                break out;
            }
            FrameClass::Finish { reason, usage } => {
                if let Some(u) = usage {
                    usage_acc = merge_usage_max(usage_acc, u);
                }
                // A Finish carrying buffered text commits that text together
                // with its own finish reason; a Finish without any is still a
                // valid (empty) completion.
                if !pending.is_empty() {
                    commit_finish = Some(reason);
                    break vec![StreamEvent::TextDelta {
                        text: std::mem::take(&mut pending),
                        finish: commit_finish,
                    }];
                }
                break Vec::new();
            }
        }
        };

        // ---- no loop: this round produced the answer ----
        let Some(hit) = looped.take() else { break tail };

        // ---- a loop was detected: cut it and try to carry on ----
        // Everything before the repetition is real answer text. `text_before`
        // walks back `real_span` characters from the window tail, which is
        // exactly the boundary the detector found, and drops the repetition
        // itself along with it.
        let kept = guard.text_before(hit.real_span);
        // Is there an ANSWER before the loop, or only the loop itself?
        //
        // `hold` is the same threshold the commit point uses: the number of
        // buffered characters at which an answer counts as started. Below it
        // nothing had committed, so the "prefix" is not an answer the client
        // is owed — it is a model that repeated itself from its first
        // character. Wrapping up on that would hand the client a fragment of
        // the repetition (or nothing at all) as a successful answer, which is
        // strictly worse than letting the walk try the next candidate. Above
        // it there IS real answer text, and the whole point of this path is
        // that such text is continued rather than thrown away.
        if kept.chars().count() < hold {
            return Err((
                ProxyError::DegenerateOutput { unit: hit.unit, repeats: hit.repeats },
                usage_acc,
            ));
        }
        // The holdback buffer holds the SAME tail as the window (every text
        // char enters the guard before it enters `pending`), so the repetition
        // has to come out of it too: otherwise a wrap-up at this cut would
        // relay the very repetition that was just detected.
        // `pending` is the window MINUS the frame that triggered this hit: that
        // frame reached the guard but never reached `pending`. So the repetition
        // occupies the window's last `real_span` chars, of which only the part
        // BEFORE the trigger frame can be sitting in `pending`. Dropping the
        // whole `real_span` here would eat into text that is still good — for a
        // short trigger frame it would wipe `pending` out entirely.
        {
            let plen = pending.chars().count();
            let drop_from_pending = hit.real_span.saturating_sub(trigger_len).min(plen);
            let keep_n = plen - drop_from_pending;
            if keep_n < plen {
                pending = pending.chars().take(keep_n).collect();
            }
        }
        // The repeated unit must not be re-detected: the window now holds only
        // the text that survived, so the continuation is judged on its own
        // output plus a bounded tail of the answer it is continuing.
        guard.reseed(&kept);
        let prompt = crate::loopguard::render_continue_prompt(&budget.loop_guard.continue_prompt, &hit);
        // The unit of the round that just ran. Repeating the SAME unit after being
        // told about it means the model is stuck, so the next round is refused.
        let stuck = last_unit.as_deref() == Some(hit.unit.as_str());
        if can_continue_now(&budget.loop_guard, continues, budget.deadline, budget.ttfb_timeout, stuck) {
            let next_ir = continuation_ir(&ctx.ir, &kept, &prompt);
            match open_continuation(&ctx.state.http, cand, budget.connect_timeout, budget.ttfb_timeout, &next_ir).await {
                Ok(resp) => {
                    continues += 1;
                    last_unit = Some(hit.unit.clone());
                    es = resp.bytes_stream().eventsource();
                    // A fresh stream deserves a fresh first-event window; the
                    // request's total deadline still bounds it.
                    frame_deadline = Instant::now() + budget.first_event_timeout;
                    // `pending` and `pre_reasoning` deliberately SURVIVE: the
                    // text held back before the cut is the beginning of this
                    // answer, and the continuation appends to it. Dropping
                    // either would delete real content the client never saw.
                    continue;
                }
                Err(e) => {
                    // The continuation could not start. The answer so far is
                    // still real text, so wrap up with what we have instead of
                    // failing a request that already holds a partial answer.
                    let _ = e;
                }
            }
        }
        // No round available (cap reached, no budget, or the continuation could
        // not be opened): close the answer cleanly at the cut.
        loop_truncated = true;
        upstream_done = true;
        commit_finish = Some(ir::FinishReason::Length);
        break vec![StreamEvent::TextDelta {
            text: std::mem::take(&mut pending),
            finish: commit_finish,
        }];
    };

    // COMMITTED (or empty-but-valid). Client still received zero bytes.
    // Thinking precedes the answer in the commit list because that is the order
    // the upstream produced it in; a client rendering both must not see the
    // reasoning after the text it explains.
    let mut committed = pre_reasoning;
    committed.extend(committed_tail);
    let (outcome_tx, outcome_rx) = tokio::sync::oneshot::channel::<StreamOutcome>();
    let entry = ctx.entry;
    let exposed_name = ctx.exposed_model.clone();
    let idle = budget.chunk_idle_timeout;
    let deadline_total = budget.deadline;
    let proto = cand.protocol;
    // Moved into the pump so a POST-COMMIT loop can be corrected there too:
    // the correction is another upstream call against the SAME candidate, with
    // the same budget rules the pre-commit rounds obey.
    let cont_http = ctx.state.http.clone();
    let cont_cand = cand.clone();
    let cont_base_ir = ctx.ir.clone();
    let cont_cfg = budget.loop_guard.clone();
    let cont_ttfb = budget.ttfb_timeout;
    let cont_connect = budget.connect_timeout;

    tokio::spawn(async move {
        // The account stays "in use" until the pump stops relaying, not until
        // the response headers arrived: `inflight` is moved in here and only
        // released when this task ends, on every exit path below.
        let _inflight = inflight;
        let mut tx = Some(outcome_tx);
        let mut final_usage = usage_acc;
        // Both flags are seeded from the PRE-COMMIT rounds, which already know
        // whether this answer was cut at a loop (`loop_truncated`) and how many
        // corrections it took (`continues`). The post-commit guard below can
        // only ever raise them further.
        let mut loop_truncated = loop_truncated;
        let mut loop_continues: u32 = continues;
        // The repeated unit of the last correction round, for the stuck check.
        let mut last_unit: Option<String> = last_unit;
        let feed = feed_tx.clone();

        // POST-COMMIT pump: convert frames to IR events, render in client protocol.
        let mut finished = committed.is_empty();
        if finished {
            feed_close_clean(&feed, entry).await;
            if let Some(tx) = tx.take() { let _ = tx.send(StreamOutcome { usage: final_usage, loop_truncated, loop_continues }); }
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
        for ev in committed {
            match ev {
                StreamEvent::ToolUseStart { index, id, name, args } => match args {
                    // Whole call already (Gemini upstream): pass straight through.
                    Some(a) => {
                        let ev = StreamEvent::ToolUseStart { index, id, name, args: Some(a) };
                        feed_emit(&feed, entry, &ev, &exposed_name).await;
                    }
                    // Incremental start: hold it back until its args arrive.
                    None => pending_tool = Some((index, id, name, String::new())),
                },
                StreamEvent::ToolInputDelta { index, partial_json } => {
                    // Upstream opened with arguments directly (no start frame seen
                    // pre-commit): buffer them so the call is still emitted whole.
                    pending_tool = Some((index, String::new(), String::new(), partial_json));
                }
                other => feed_emit(&feed, entry, &other, &exposed_name).await,
            }
        }

        // The pre-commit rounds may have walked away from the upstream stream
        // (a cut that no correction could resume). Reading it now would relay
        // the very repetition that was just removed, so the answer is finished
        // at the cut instead.
        let mut upstream_done = upstream_done;
        // The upstream stream is swappable: a correction round replaces it with
        // the continuation's stream, spliced onto the same client feed so the
        // client sees ONE continuous answer.
        let mut es = es;

        loop {
            if upstream_done {
                if !finished {
                    finished = true;
                    feed_emit(&feed, entry, &StreamEvent::Finish {
                        reason: commit_finish.unwrap_or(ir::FinishReason::Length),
                        usage: final_usage.clone(),
                    }, &exposed_name).await;
                }
                feed_close_clean(&feed, entry).await;
                if let Some(tx) = tx.take() { let _ = tx.send(StreamOutcome { usage: final_usage, loop_truncated, loop_continues }); }
                return;
            }
            let next = tokio::select! {
                ev = es.next() => ev,
                _ = sleep_until_std(Instant::now() + idle) => {
                    feed_repair(&feed, entry, "上游分片超时，已中断").await;
                    if let Some(tx) = tx.take() { let _ = tx.send(StreamOutcome { usage: final_usage, loop_truncated, loop_continues }); }
                    return;
                }
                _ = sleep_until_std(deadline_total) => {
                    feed_repair(&feed, entry, "总超时时间已用尽，已中断").await;
                    if let Some(tx) = tx.take() { let _ = tx.send(StreamOutcome { usage: final_usage, loop_truncated, loop_continues }); }
                    return;
                }
                _ = client_gone.cancelled() => {
                    // client vanished: stop pumping; drop closes upstream conn
                    if let Some(tx) = tx.take() { let _ = tx.send(StreamOutcome { usage: final_usage, loop_truncated, loop_continues }); }
                    return;
                }
            };
            let Some(ev) = next else {
                // Same rule as the [DONE] branch below: a commit that already
                // carried its own finish reason is a clean completion, even when
                // the upstream closes without a separate Finish frame.
                if !finished {
                    if let Some(reason) = commit_finish {
                        finished = true;
                        feed_emit(&feed, entry, &StreamEvent::Finish { reason, usage: final_usage.clone() }, &exposed_name).await;
                    }
                }
                if !finished {
                    feed_repair(&feed, entry, "上游连接提前结束").await;
                } else {
                    feed_close_clean(&feed, entry).await;
                }
                if let Some(tx) = tx.take() { let _ = tx.send(StreamOutcome { usage: final_usage, loop_truncated, loop_continues }); }
                return;
            };
            let Ok(ev) = ev else { continue };
            let data = ev.data.trim().to_string();
            if data.is_empty() { continue }

            let fc = match proto {
                Protocol::Openai => match openai::classify_upstream_frame(&data) {
                    CO::Ignore => FrameClass::Ignore,
                    CO::Text(t) => FrameClass::Text(t),
                    CO::Reasoning(t) => FrameClass::Reasoning(t),
                    CO::ReasoningAndText { reasoning, text } => FrameClass::ReasoningAndText { reasoning, text },
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
                    CA::Reasoning(t) => FrameClass::Reasoning(t),
                    CA::ToolUseStart { index, id, name, args } => FrameClass::ToolUseStart { index, id, name, args },
                    CA::ToolInput { index, delta } => FrameClass::ToolInput { index, delta },
                    CA::Finish { reason, usage } => FrameClass::Finish { reason, usage: Some(usage) },
                    CA::Error(m) => FrameClass::Error(m),
                    CA::Done => FrameClass::Done,
                },
                Protocol::Gemini => match gemini::classify_upstream_frame(&data) {
                    CG::Ignore => FrameClass::Ignore,
                    CG::Text(t) => FrameClass::Text(t),
                    CG::Reasoning(t) => FrameClass::Reasoning(t),
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
                    if let Some(tx) = tx.take() { let _ = tx.send(StreamOutcome { usage: final_usage, loop_truncated, loop_continues }); }
                    return;
                }
                FrameClass::Reasoning(t) => {
                    // Forward immediately: thinking is not judged and not held
                    // back, for the same reasons as the pre-commit path.
                    feed_emit(&feed, entry, &StreamEvent::Reasoning { text: t }, &exposed_name).await;
                }
                FrameClass::ReasoningAndText { reasoning, text } => {
                    // Emit the thought, then run the text through the SAME
                    // post-commit detection as a plain Text frame: a mixed
                    // delta must not become a hole in the guard's coverage.
                    feed_emit(&feed, entry, &StreamEvent::Reasoning { text: reasoning }, &exposed_name).await;
                    if let Some(hit) = guard.push_tail(&text) {
                        // Same correction path as a plain Text hit: the thought
                        // has already been relayed, and the answer must carry on
                        // rather than stop here.
                        flush_tool(&feed, entry, &exposed_name, &mut pending_tool).await;
                        if let Some(resp) = start_post_commit_round(
                            &cont_http, &cont_cand, &cont_base_ir, &cont_cfg, &hit, &mut guard,
                            loop_continues, deadline_total, cont_ttfb, cont_connect, &last_unit,
                        ).await {
                            loop_continues += 1;
                            last_unit = Some(hit.unit.clone());
                            es = resp.bytes_stream().eventsource();
                            continue;
                        }
                        loop_truncated = true;
                        finished = true;
                        feed_emit(
                            &feed, entry,
                            &StreamEvent::Finish { reason: ir::FinishReason::Length, usage: final_usage.clone() },
                            &exposed_name,
                        ).await;
                        feed_close_clean(&feed, entry).await;
                        if let Some(tx) = tx.take() { let _ = tx.send(StreamOutcome { usage: final_usage, loop_truncated, loop_continues }); }
                        return;
                    }
                    feed_emit(&feed, entry, &StreamEvent::TextDelta { text, finish: None }, &exposed_name).await;
                }
                FrameClass::Text(t) => {
                    // POST-COMMIT detection. The bytes already sent cannot be
                    // recalled, so the repetition itself is not relayed: the
                    // model is instead asked, with the reason stated, to carry
                    // on from the cut, and the continuation is spliced onto the
                    // SAME client stream. Only when no round can start is the
                    // answer finished where it stands — it is never abandoned
                    // whole, because the prefix the client already holds is real
                    // answer text.
                    if let Some(hit) = guard.push_tail(&t) {
                        tracing::warn!(
                            unit_chars = hit.unit.chars().count(),
                            repeats = hit.repeats,
                            covered = hit.chars,
                            "degenerate output detected AFTER commit; correcting"
                        );
                        // A tool call already assembled but not yet emitted is
                        // released first: cutting mid-call would hand the
                        // client an argument list cut in half.
                        flush_tool(&feed, entry, &exposed_name, &mut pending_tool).await;
                        if let Some(resp) = start_post_commit_round(
                            &cont_http, &cont_cand, &cont_base_ir, &cont_cfg, &hit, &mut guard,
                            loop_continues, deadline_total, cont_ttfb, cont_connect, &last_unit,
                        ).await {
                            loop_continues += 1;
                            last_unit = Some(hit.unit.clone());
                            es = resp.bytes_stream().eventsource();
                            continue;
                        }
                        loop_truncated = true;
                        finished = true;
                        feed_emit(
                            &feed, entry,
                            &StreamEvent::Finish { reason: ir::FinishReason::Length, usage: final_usage.clone() },
                            &exposed_name,
                        ).await;
                        feed_close_clean(&feed, entry).await;
                        if let Some(tx) = tx.take() { let _ = tx.send(StreamOutcome { usage: final_usage, loop_truncated, loop_continues }); }
                        return;
                    }
                    feed_emit(&feed, entry, &StreamEvent::TextDelta { text: t, finish: None }, &exposed_name).await
                }
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
                    // A commit that already reported this completion (the
                    // holdback buffer was flushed by a Finish frame) must not
                    // complete the same answer twice.
                    if finished { continue; }
                    // The call is complete once the upstream stops: release it
                    // before the finish frame so it cannot be stranded.
                    flush_tool(&feed, entry, &exposed_name, &mut pending_tool).await;
                    finished = true;
                    feed_emit(&feed, entry, &StreamEvent::Finish { reason, usage: final_usage.clone() }, &exposed_name).await;
                }
                FrameClass::Done => {
                    // A commit that already carried its finish reason is a clean
                    // completion even when the upstream never sent a Finish
                    // frame of its own (the holdback buffer was flushed by
                    // [DONE]). Repairing here would tell the client of a
                    // perfectly normal answer that the upstream had broken.
                    if !finished {
                        if let Some(reason) = commit_finish {
                            finished = true;
                            feed_emit(&feed, entry, &StreamEvent::Finish { reason, usage: final_usage.clone() }, &exposed_name).await;
                        }
                    }
                    if finished {
                        feed_close_clean(&feed, entry).await;
                    } else {
                        feed_repair(&feed, entry, "upstream closed without finish").await;
                    }
                    if let Some(tx) = tx.take() { let _ = tx.send(StreamOutcome { usage: final_usage, loop_truncated, loop_continues }); }
                    return;
                }
            }
        }
    });

    Ok(Outcome::Stream { outcome_rx })
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

/// Drain the holdback buffer into a commit list. The text is NOT a completion
/// here: the frame that flushed it (a tool call) continues the answer, so the
/// text carries `finish: None` and the answer ends the way it otherwise would.
fn flush_pending(pending: &mut String) -> Vec<StreamEvent> {
    if pending.is_empty() {
        return Vec::new();
    }
    vec![StreamEvent::TextDelta { text: std::mem::take(pending), finish: None }]
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
                "error": { "message": format!("上游流中断：{msg}"), "type": "gateway_upstream_interrupted", "code": "upstream_interrupted" }
            });
            let _ = tx.send(ClientFrame::Sse(format!("data: {err}\n\n"))).await;
            let ev = StreamEvent::Finish { reason: ir::FinishReason::ContentFilter, usage: ir::Usage::default() };
            let finish = openai::render_stream_event("unknown", &ev);
            for v in finish { let _ = tx.send(ClientFrame::Sse(format!("data: {v}\n\n"))).await; }
            let _ = tx.send(ClientFrame::Sse("data: [DONE]\n\n".to_string())).await;
        }
        EntryProtocol::Anthropic => {
            let err = serde_json::json!({
                "type": "error", "error": { "type": "api_error", "message": format!("上游流中断：{msg}") }
            });
            let _ = tx.send(ClientFrame::Sse(format!("event: error\ndata: {err}\n\n"))).await;
        }
        EntryProtocol::Gemini => {
            let err = serde_json::json!({ "error": { "code": 502, "message": format!("上游流中断：{msg}"), "status": "UNAVAILABLE" } });
            let _ = tx.send(ClientFrame::Sse(format!("data: {err}\n\n"))).await;
        }
    }
    let _ = tx.send(ClientFrame::Close).await;
}

// NOTE: feed channels are per-request (created in routes.rs), never global —
// a process-global slot would cross-talk frames between concurrent streams.

#[cfg(test)]
mod loop_guard_commit_tests {
    use super::*;

    /// B1 regression: text buffered by the loop guard's holdback must not be
    /// dropped when a tool call arrives before the buffer is full.
    #[test]
    fn flush_pending_returns_the_buffer_and_empties_it() {
        let mut pending = String::from("好的，我来调用工具");
        let evs = flush_pending(&mut pending);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            StreamEvent::TextDelta { text, finish } => {
                assert_eq!(text, "好的，我来调用工具");
                assert!(finish.is_none(), "a tool frame does not end the answer");
            }
            other => panic!("expected TextDelta, got {other:?}"),
        }
        assert!(pending.is_empty(), "the buffer must be drained, not copied");

        // Empty buffer contributes nothing at all.
        assert!(flush_pending(&mut pending).is_empty());
    }
}
