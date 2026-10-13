//! Router state machine: candidate plan snapshot, tier walk, budget.
//!
//! Semantics (v0.4 plan):
//! - candidate plan built ONCE per request (I3: same (channel, model) never
//!   tried twice within one request, regardless of cooldown state changes)
//! - "fail-and-switch": iterate candidates in tier order; a tier is exhausted
//!   when every member in it has been tried (or excluded at snapshot time)
//! - after the last tier, wrap to tier 1 again (cycle++), up to max_cycles
//! - WAIT_FOR_COOLDOWN: if every candidate is cooling but some expiry fits in
//!   the remaining budget, sleep (with jitter) and retry the walk (I5)
//! - budgets: max_attempts, max_cycles, total_deadline - first exhausted wins

use crate::config::{Config, Group, ModelBinding, Protocol, RetryConfig};
use crate::health::{Attribution, CooldownKey, HealthRegistry};
use serde_json::Value;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Candidate {
    pub channel_id: String,
    pub channel_name: String,
    pub base_url: String,
    pub protocol: Protocol,
    pub api_key: String,
    pub extra_headers: std::collections::HashMap<String, String>,
    pub model_id: String,
    pub upstream_model: String,
    pub tier: u32,
    pub weight: u32,
}

pub struct Budget {
    pub max_attempts: u32,
    pub max_cycles: u32,
    pub deadline: Instant,
    pub attempt_deadline: Duration,
    pub connect_timeout: Duration,
    pub ttfb_timeout: Duration,
    pub first_event_timeout: Duration,
    pub chunk_idle_timeout: Duration,
    pub reserve: Duration,
    /// Degenerate-output detection for the PRE-COMMIT window (never applied
    /// after the first frame is committed).
    pub loop_guard: crate::loopguard::LoopCfg,
}

/// I4: does the configured attempt budget actually fit inside the total
/// deadline? Enforced on every config write (admin PUT). Kept separate from
/// `build_budget` so the runtime can stay fail-safe: a config nit must never
/// cost a client request (core requirement: failures never leak outwards).
pub fn validate_budget(rc: &RetryConfig) -> Result<(), String> {
    let total = Duration::from_millis(rc.total_deadline_ms);
    let attempt = Duration::from_millis(rc.attempt_deadline_ms);
    let reserve = Duration::from_millis(rc.reserve_ms);
    let worst = attempt.saturating_mul(rc.max_attempts).saturating_add(reserve);
    if worst > total {
        return Err(format!(
            "config invalid: max_attempts({}) * attempt_deadline_ms({}) + reserve_ms({}) = {}ms exceeds total_deadline_ms({})",
            rc.max_attempts, rc.attempt_deadline_ms, rc.reserve_ms, worst.as_millis(), rc.total_deadline_ms
        ));
    }
    Ok(())
}

/// Build the per-request budget. Never fails: when I4 does not hold (e.g. a
/// config written by an older build, or a hand-edited DB) the per-attempt
/// deadline is clamped so the walk still fits the total deadline. The request
/// gets fewer attempts than configured instead of an outright error.
pub fn build_budget(rc: &RetryConfig, started: Instant) -> Budget {
    let total = Duration::from_millis(rc.total_deadline_ms);
    let reserve = Duration::from_millis(rc.reserve_ms);
    let configured = Duration::from_millis(rc.attempt_deadline_ms);

    let usable = total.saturating_sub(reserve);
    let per_attempt = if rc.max_attempts == 0 { usable } else { usable / rc.max_attempts };
    let attempt = if configured > per_attempt {
        tracing::warn!(
            "retry config violates I4 ({} x {}ms + {}ms > {}ms); clamping per-attempt deadline to {}ms",
            rc.max_attempts, rc.attempt_deadline_ms, rc.reserve_ms, rc.total_deadline_ms,
            per_attempt.as_millis()
        );
        per_attempt
    } else {
        configured
    };

    Budget {
        max_attempts: rc.max_attempts,
        max_cycles: rc.max_cycles,
        deadline: started + total,
        attempt_deadline: attempt,
        connect_timeout: Duration::from_millis(rc.connect_timeout_ms),
        ttfb_timeout: Duration::from_millis(rc.ttfb_timeout_ms),
        first_event_timeout: Duration::from_millis(rc.first_event_timeout_ms),
        chunk_idle_timeout: Duration::from_millis(rc.chunk_idle_timeout_ms),
        reserve,
        loop_guard: crate::loopguard::LoopCfg::from_retry(rc),
    }
}

/// Is `requested` routed through the WILDCARD path for this group?
///
/// Task-27 deliverable 1: a client may name the group itself (its `name` or its
/// `id`) instead of one of the group's exposed models, and then every enabled
/// member of every tier is a candidate.
///
/// EXACT WINS: when the requested string is one of the group's `exposed_models`
/// this is false, even if a group happens to be named like its own model. That
/// keeps the pre-existing narrow semantics byte-identical for every name that
/// used to work.
pub fn is_wildcard(group: &Group, requested: &str) -> bool {
    if group.exposed_models.iter().any(|m| m == requested) {
        return false;
    }
    requested == group.name || requested == group.id
}

/// Build the ordered candidate plan for (group, exposed_model). Snapshot at
/// request start: cooldown state consulted HERE, never re-read mid-request.
///
/// Two modes, decided here so every caller (proxy.rs has four call sites) gets
/// the right one without knowing about wildcards:
///   * `exposed_model` is one of the group's exposed models  -> narrow plan
///     (only that model's members), the pre-task-27 behaviour;
///   * otherwise, when the name is the group's `name`/`id` -> wildcard plan
///     (every enabled member of every tier);
///   * anything else -> an empty plan, i.e. Terminal::NoCandidate. routes.rs
///     already answers 404 for that case before the walk starts.
///
/// Order and cooldown semantics are shared by both modes (see `plan_with`).
pub fn build_plan(cfg: &Config, group: &Group, exposed_model: &str, health: &HealthRegistry) -> Plan {
    if is_wildcard(group, exposed_model) {
        return plan_with(cfg, group, &|_m: &ModelBinding| true, health);
    }
    plan_with(cfg, group, &|m: &ModelBinding| m.exposed == exposed_model, health)
}

/// The shared plan builder. `keep` is the ONLY difference between the narrow
/// and the wildcard plan: the tier walk, the declared/weighted member order, the
/// (channel, upstream) dedupe and the runnable/cooling split are identical.
fn plan_with(cfg: &Config, group: &Group, keep: &dyn Fn(&ModelBinding) -> bool, health: &HealthRegistry) -> Plan {
    let mut seen = std::collections::HashSet::new();
    let mut out = vec![];
    let mut tiers: Vec<&crate::config::Tier> = group.tiers.iter().collect();
    tiers.sort_by_key(|t| t.level);

    for tier in tiers {
        let mut members: Vec<&crate::config::TierMember> = tier.members.iter().collect();
        if cfg.auto_weight {
            // Opt-in auto weighting: heaviest member first; sort_by is stable,
            // so equal weights keep the declared order.
            members.sort_by(|a, b| b.weight.cmp(&a.weight));
        }
        // auto_weight = false (the default) leaves the declared order of
        // tiers[].members untouched: no weight sort, no model_id sort.
        for member in members {
            let Some(m): Option<&ModelBinding> = cfg.find_model(&member.model_id) else { continue };
            if !m.enabled || !keep(m) { continue }
            let Some(ch) = cfg.find_channel(&m.channel_id) else { continue };
            if !ch.enabled { continue }
            let key = (m.channel_id.clone(), m.upstream.clone());
            if !seen.insert(key) { continue }
            out.push(Candidate {
                channel_id: ch.id.clone(),
                channel_name: ch.name.clone(),
                base_url: ch.base_url.clone(),
                protocol: ch.protocol,
                api_key: ch.api_key.clone(),
                extra_headers: ch.headers.clone(),
                model_id: m.id.clone(),
                upstream_model: m.upstream.clone(),
                tier: tier.level,
                weight: member.weight,
            });
        }
    }
    // Cooldown split (I5). A cooling candidate is NOT thrown away: its key is
    // kept so the caller can tell "this group has no route at all" (genuinely
    // empty plan -> 404) apart from "every route is temporarily blocked"
    // (runnable empty, cooling non-empty -> wait / 503 all_upstreams_cooling).
    // Dropping the keys here was the bug that turned an all-cooling group into
    // a zero-attempt 404.
    let mut runnable = vec![];
    let mut cooling: Vec<CooldownKey> = vec![];
    for c in out {
        let channel_cool = health.is_cooling(&CooldownKey::channel_only(&c.channel_id));
        let model_cool = health.is_cooling(&CooldownKey::scoped(&c.channel_id, &c.upstream_model));
        if channel_cool || model_cool {
            if channel_cool {
                cooling.push(CooldownKey::channel_only(&c.channel_id));
            }
            if model_cool {
                cooling.push(CooldownKey::scoped(&c.channel_id, &c.upstream_model));
            }
        } else {
            runnable.push(c);
        }
    }
    Plan { runnable, cooling }
}

/// The candidate plan for one (group, exposed_model) snapshot.
///
/// Split in two on purpose (I5): `runnable` are the candidates this request may
/// try now, `cooling` are the cooldown keys of every candidate that was skipped
/// because it is cooling. The split is what lets the walk tell a genuinely
/// unroutable group (both empty -> 404 no_route_for_model) apart from a group
/// whose every route is temporarily blocked (runnable empty, cooling non-empty
/// -> wait for the earliest expiry, or 503 all_upstreams_cooling).
#[derive(Debug, Clone)]
pub struct Plan {
    pub runnable: Vec<Candidate>,
    pub cooling: Vec<CooldownKey>,
}

impl Plan {
    /// No candidate was even CONSTRUCTED: exposed/model mismatch, missing or
    /// disabled channel/model, or no tier member at all. This is the only case
    /// that deserves a 404 — the group has no route for this model.
    pub fn is_unroutable(&self) -> bool {
        self.runnable.is_empty() && self.cooling.is_empty()
    }

    /// Candidates exist but every one of them is cooling right now.
    pub fn is_all_cooling(&self) -> bool {
        self.runnable.is_empty() && !self.cooling.is_empty()
    }

    /// Terminal outcome for a plan with nothing left to try, or None when at
    /// least one candidate is runnable. Used after a rebuild.
    pub fn blocked_terminal(&self) -> Option<Terminal> {
        if self.is_unroutable() {
            Some(Terminal::NoCandidate)
        } else if self.is_all_cooling() {
            Some(Terminal::AllCooldown)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Try(usize),
    /// rebuild the plan (cooldowns may have expired) and continue walking
    Rebuild,
    WaitFor(Instant),
    Exhausted(Terminal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminal {
    Exhausted,
    NoCandidate,
    AllCooldown,
}

pub struct WalkState {
    pub idx: usize,
    pub cycle: u32,
    pub attempts: u32,
}

pub fn next_step(
    state: &mut WalkState,
    plan: &Plan,
    budget: &Budget,
    now: Instant,
    health: &HealthRegistry,
    cooldown_wait: bool,
) -> Step {
    if state.attempts >= budget.max_attempts {
        return Step::Exhausted(Terminal::Exhausted);
    }
    let hard_deadline = budget.deadline.checked_sub(budget.reserve).unwrap_or(budget.deadline);
    if now >= hard_deadline {
        return Step::Exhausted(Terminal::Exhausted);
    }
    if state.cycle >= budget.max_cycles {
        return Step::Exhausted(Terminal::Exhausted);
    }
    // No candidate was constructed at all: the group really has no route for
    // this exposed model (404, unchanged behaviour).
    if plan.is_unroutable() {
        return Step::Exhausted(Terminal::NoCandidate);
    }
    // I5: candidates EXIST but every one of them is cooling. Never fail with
    // zero attempts: wait for the earliest real expiry, or — when every entry
    // is permanent (auth isolation, no real expiry) or the wait does not fit
    // the budget — report AllCooldown so the client gets a 503 that says
    // "try later" instead of a bogus 404.
    if plan.is_all_cooling() {
        if cooldown_wait {
            if let Some(earliest) = health.earliest_expiry(&plan.cooling) {
                if earliest > now {
                    return Step::WaitFor(earliest);
                }
            }
        }
        return Step::Exhausted(Terminal::AllCooldown);
    }
    if state.idx >= plan.runnable.len() {
        state.idx = 0;
        state.cycle += 1;
        if state.cycle >= budget.max_cycles {
            return Step::Exhausted(Terminal::Exhausted);
        }
        return Step::Rebuild;
    }
    // Late cooldown (armed by an earlier attempt of THIS request): same wait
    // rule, still bounded by the hard deadline checked above.
    if cooldown_wait && state.attempts == 0 {
        if let Some(earliest) = health.earliest_expiry(&plan.cooling) {
            if earliest > now {
                return Step::WaitFor(earliest);
            }
        }
    }
    Step::Try(state.idx)
}

#[derive(Debug, Clone)]
pub enum ProxyError {
    UpstreamStatus { status: u16, class: StatusClass },
    Timeout { what: TimeoutWhat },
    Connect(String),
    ErrorBody(String),
    NotSse(String),
    EmptyResponse,
    /// The stream repeated the same short unit so many times that the output is
    /// useless. Raised only before the first frame is committed, so the client
    /// has seen nothing and another candidate can answer instead.
    DegenerateOutput { unit: String, repeats: u32 },
    ClientGone,
}

#[derive(Debug, Clone)]
pub enum StatusClass {
    Auth,
    ModelNotFound,
    RateLimited { retry_after_ms: Option<u64> },
    Server,
    BadRequest,
    ContentFilter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutWhat { Connect, Ttfb, FirstEvent, Attempt }

pub fn status_class(status: u16) -> StatusClass {
    match status {
        401 | 403 => StatusClass::Auth,
        404 => StatusClass::ModelNotFound,
        429 => StatusClass::RateLimited { retry_after_ms: None },
        400 | 422 => StatusClass::BadRequest,
        _ if status >= 500 => StatusClass::Server,
        _ => StatusClass::BadRequest,
    }
}

/// Error classification -> (attribution, switch-log error_class).
pub fn classify_failure(err: &ProxyError, candidate: &Candidate) -> (Attribution, &'static str) {
    let transient = |key: CooldownKey, dur: Duration, class: &'static str| {
        (Attribution::Cooldown(key, dur, "backoff"), class)
    };
    match err {
        ProxyError::UpstreamStatus { status, class } => match class {
            // 401/403 = channel-level PERMANENT isolation (see
            // HealthRegistry::auth_permanent): the channel stays out of every
            // plan until a manual clear or a reconfig. The old Duration::ZERO
            // made "until = now", so is_cooling() was instantly false and the
            // isolation silently never happened.
            StatusClass::Auth => (
                Attribution::Cooldown(
                    CooldownKey::channel_only(&candidate.channel_id),
                    HealthRegistry::auth_permanent(),
                    "auth",
                ),
                "auth_error",
            ),
            StatusClass::ModelNotFound => (
                Attribution::Cooldown(
                    CooldownKey::scoped(&candidate.channel_id, &candidate.upstream_model),
                    HealthRegistry::model_exclusion(),
                    "model",
                ),
                "model_not_found",
            ),
            StatusClass::RateLimited { retry_after_ms } => {
                let dur = retry_after_ms.map(Duration::from_millis).unwrap_or_else(|| Duration::from_secs(30));
                transient(CooldownKey::scoped(&candidate.channel_id, &candidate.upstream_model), dur, "rate_limited")
            }
            StatusClass::Server => transient(CooldownKey::scoped(&candidate.channel_id, &candidate.upstream_model), Duration::from_secs(15), "http_5xx"),
            StatusClass::BadRequest => (Attribution::None, "bad_request"),
            StatusClass::ContentFilter => (Attribution::None, "content_filter"),
        },
        ProxyError::Timeout { what } => {
            let class = match what {
                TimeoutWhat::Connect => "connect_timeout",
                TimeoutWhat::Ttfb => "ttfb_timeout",
                TimeoutWhat::FirstEvent => "first_event_timeout",
                TimeoutWhat::Attempt => "attempt_deadline",
            };
            transient(CooldownKey::scoped(&candidate.channel_id, &candidate.upstream_model), Duration::from_secs(15), class)
        }
        ProxyError::Connect(_) => transient(CooldownKey::scoped(&candidate.channel_id, &candidate.upstream_model), Duration::from_secs(15), "connect_error"),
        ProxyError::ErrorBody(_) => transient(CooldownKey::scoped(&candidate.channel_id, &candidate.upstream_model), Duration::from_secs(15), "upstream_error_body"),
        ProxyError::NotSse(_) => transient(CooldownKey::scoped(&candidate.channel_id, &candidate.upstream_model), Duration::from_secs(15), "not_sse_garbage"),
        ProxyError::EmptyResponse => transient(CooldownKey::scoped(&candidate.channel_id, &candidate.upstream_model), Duration::from_secs(15), "empty_response"),
        // Same treatment as an empty response: the attempt produced nothing
        // usable, so the candidate is put on a short scoped cooldown and the
        // walk moves on to another one.
        ProxyError::DegenerateOutput { .. } => transient(CooldownKey::scoped(&candidate.channel_id, &candidate.upstream_model), Duration::from_secs(15), "degenerate_output"),
        ProxyError::ClientGone => (Attribution::None, "client_aborted"),
    }
}
