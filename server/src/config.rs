//! Immutable config snapshot + hot-reload semantics.
//!
//! All data-plane reads go through a single `Arc<Config>` fetched once per
//! request; admin writes swap the pointer atomically. In-flight requests keep
//! the old snapshot (I: config consistency per request).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Openai,
    Anthropic,
    Gemini,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    #[serde(default = "d_max_attempts")]
    pub max_attempts: u32,
    #[serde(default = "d_max_cycles")]
    pub max_cycles: u32,
    #[serde(default = "d_total_deadline_ms")]
    pub total_deadline_ms: u64,
    #[serde(default = "d_attempt_deadline_ms")]
    pub attempt_deadline_ms: u64,
    #[serde(default = "d_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    #[serde(default = "d_ttfb_timeout_ms")]
    pub ttfb_timeout_ms: u64,
    #[serde(default = "d_first_event_timeout_ms")]
    pub first_event_timeout_ms: u64,
    #[serde(default = "d_chunk_idle_timeout_ms")]
    pub chunk_idle_timeout_ms: u64,
    #[serde(default = "d_reserve_ms")]
    pub reserve_ms: u64,
    #[serde(default = "d_true")]
    pub cooldown_wait: bool,
    /// Loop guard (degenerate output detection) master switch. Disabling it also
    /// removes the holdback, so the first frame commits immediately.
    #[serde(default = "d_true")]
    pub loop_guard_enabled: bool,
    /// Characters buffered before the first frame is committed (= holdback).
    /// Larger catches longer repeats, but delays the first byte by that many
    /// generated characters.
    #[serde(default = "d_loop_guard_min_chars")]
    pub loop_guard_min_chars: usize,
    /// Minimum number of characters the repetition must cover.
    #[serde(default = "d_loop_guard_repeat_min_chars")]
    pub loop_guard_repeat_min_chars: usize,
    /// Share of the window the repetition must cover (0, 1].
    #[serde(default = "d_loop_guard_repeat_ratio")]
    pub loop_guard_repeat_ratio: f64,
    /// How many times the unit must repeat before it counts as degenerate.
    #[serde(default = "d_loop_guard_max_repeats")]
    pub loop_guard_max_repeats: u32,
    /// Rolling character window the detector keeps.
    #[serde(default = "d_loop_guard_window_chars")]
    pub loop_guard_window_chars: usize,
    /// How much of the answer's tail is judged once the answer has ALREADY been
    /// committed. A post-commit hit cannot be retried (the bytes are gone), so
    /// the only remedy is to stop; this bounds how long a mid-answer loop may
    /// run before that happens. Smaller catches loops sooner but needs the
    /// repetition to be more complete.
    ///
    /// This is the tier-1 span AND the evidence floor for both tiers: the tail
    /// must hold this many characters before ANY post-commit judgement happens,
    /// which is what keeps a three-word answer from being judged. Tier 2 widens
    /// the judged SPAN beyond this value (see `loopguard::TIER2_SPAN_MULT`),
    /// but never lowers the floor. Those two tier-2 knobs are deliberately
    /// compile-time constants rather than config fields: they are not
    /// independent, and exposing them separately would let an operator widen
    /// the span without raising the repeat bar, which silently increases the
    /// false-positive rate.
    #[serde(default = "d_loop_guard_tail_chars")]
    pub loop_guard_tail_chars: usize,
    /// Wall-clock ceiling on the PRE-COMMIT holdback, in milliseconds.
    ///
    /// `loop_guard_min_chars` is a CHARACTER count, so how long it takes to
    /// fill depends entirely on how fast the model generates: at 400 chars/s
    /// 64 characters is 160 ms, at the measured median of ~107 chars/s it is
    /// ~600 ms, and a 2 chars/s model would take 32 seconds. This cap bounds
    /// that wait so a slow model cannot turn the holdback into a multi-second
    /// first-byte delay.
    ///
    /// The trade-off is explicit and worth stating: the evidence needed to
    /// judge a repetition is CHARACTERS, so a model too slow to fill
    /// `min_chars` inside the cap commits early and is judged post-commit
    /// (tier 2 of `judge_tail` still applies). Detection quality degrades;
    /// the first byte does not wait. 0 disables the cap.
    #[serde(default = "d_loop_guard_hold_max_ms")]
    pub loop_guard_hold_max_ms: u64,
    /// On a repetition, correct the model instead of discarding the attempt:
    /// keep the text written BEFORE the loop and ask the model to carry on from
    /// where it was cut off. Disabling this restores the old behaviour (throw
    /// the whole attempt away and retry another candidate).
    #[serde(default = "d_true")]
    pub loop_guard_continue_enabled: bool,
    /// How many times one answer may be corrected. Each round is a real upstream
    /// call, so this is also the worst-case extra cost of a looping model. When
    /// the rounds are used up the answer is closed cleanly with what it has
    /// (a staged wrap-up) rather than thrown away.
    #[serde(default = "d_loop_guard_continue_max")]
    pub loop_guard_continue_max: u32,
    /// The correction prompt. It MUST state the reason: a model that is told
    /// only "continue" tends to restart the same loop, whereas one told WHY it
    /// was interrupted (it was repeating) resumes the actual task.
    /// Placeholders: {unit} = the repeated text, {repeats} = how many times.
    #[serde(default = "d_loop_guard_continue_prompt")]
    pub loop_guard_continue_prompt: String,
}

fn d_max_attempts() -> u32 { 6 }
fn d_max_cycles() -> u32 { 2 }
// Must satisfy I4 against the other defaults: 6 x 25_000 + 3_000 = 153_000 <= 180_000.
fn d_total_deadline_ms() -> u64 { 180_000 }
fn d_attempt_deadline_ms() -> u64 { 25_000 }
fn d_connect_timeout_ms() -> u64 { 5_000 }
fn d_ttfb_timeout_ms() -> u64 { 15_000 }
fn d_first_event_timeout_ms() -> u64 { 8_000 }
fn d_chunk_idle_timeout_ms() -> u64 { 15_000 }
fn d_reserve_ms() -> u64 { 3_000 }
fn d_true() -> bool { true }
fn d_loop_guard_min_chars() -> usize { 64 }
fn d_loop_guard_repeat_min_chars() -> usize { 24 }
fn d_loop_guard_repeat_ratio() -> f64 { 0.75 }
fn d_loop_guard_max_repeats() -> u32 { 4 }
fn d_loop_guard_window_chars() -> usize { 4000 }
fn d_loop_guard_tail_chars() -> usize { 128 }
fn d_loop_guard_hold_max_ms() -> u64 { 250 }
fn d_loop_guard_continue_max() -> u32 { 2 }
fn d_loop_guard_continue_prompt() -> String {
    // Chinese on purpose: the model that loops is usually a Chinese-speaking
    // assistant, and the reason ("你复读了") is what makes the correction work.
    "你刚才的输出出现了复读：同一段内容「{unit}」连续重复了 {repeats} 次，已被系统截断。请不要重复已经说过的内容，也不要道歉或解释，直接接着上面被打断的地方继续输出剩余内容。".to_string()
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: d_max_attempts(),
            max_cycles: d_max_cycles(),
            total_deadline_ms: d_total_deadline_ms(),
            attempt_deadline_ms: d_attempt_deadline_ms(),
            connect_timeout_ms: d_connect_timeout_ms(),
            ttfb_timeout_ms: d_ttfb_timeout_ms(),
            first_event_timeout_ms: d_first_event_timeout_ms(),
            chunk_idle_timeout_ms: d_chunk_idle_timeout_ms(),
            reserve_ms: d_reserve_ms(),
            cooldown_wait: true,
            loop_guard_enabled: true,
            loop_guard_min_chars: d_loop_guard_min_chars(),
            loop_guard_repeat_min_chars: d_loop_guard_repeat_min_chars(),
            loop_guard_repeat_ratio: d_loop_guard_repeat_ratio(),
            loop_guard_max_repeats: d_loop_guard_max_repeats(),
            loop_guard_window_chars: d_loop_guard_window_chars(),
            loop_guard_tail_chars: d_loop_guard_tail_chars(),
            loop_guard_hold_max_ms: d_loop_guard_hold_max_ms(),
            loop_guard_continue_enabled: true,
            loop_guard_continue_max: d_loop_guard_continue_max(),
            loop_guard_continue_prompt: d_loop_guard_continue_prompt(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListenConfig {
    #[serde(default = "d_http_addr")]
    pub http: String,
    #[serde(default = "d_https_addr")]
    pub https: String,
    #[serde(default)]
    pub https_enabled: bool,
}

fn d_http_addr() -> String { "0.0.0.0:8080".into() }
fn d_https_addr() -> String { "0.0.0.0:8443".into() }

impl Default for ListenConfig {
    fn default() -> Self {
        Self { http: d_http_addr(), https: d_https_addr(), https_enabled: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminConfig {
    #[serde(default = "d_admin_bind")]
    pub bind: String,
    /// Plaintext admin key on first run; stored hashed afterwards.
    #[serde(default)]
    pub key: Option<String>,
}

fn d_admin_bind() -> String { "127.0.0.1:9090".into() }

impl Default for AdminConfig {
    fn default() -> Self { Self { bind: d_admin_bind(), key: None } }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub protocol: Protocol,
    pub api_key: String,
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default = "d_weight")]
    pub weight: u32,
    /// Extra headers appended to every upstream request from this channel.
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

fn d_weight() -> u32 { 1 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelBinding {
    pub id: String,
    pub channel_id: String,
    /// Name clients ask for, e.g. "my-gpt-4o".
    pub exposed: String,
    /// Actual upstream model name, e.g. "gpt-4o-2024-08-06".
    pub upstream: String,
    /// Price per 1M tokens: [input, output, cache_read, cache_creation], USD.
    #[serde(default)]
    pub price_per_m: [f64; 4],
    #[serde(default = "d_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierMember {
    pub model_id: String,
    #[serde(default = "d_weight")]
    pub weight: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tier {
    /// 1-based priority level.
    pub level: u32,
    pub members: Vec<TierMember>,
}

/// Per-group overrides of the global settings.
///
/// A field left at its default (here: `None`) means "follow the global
/// config"; only a group that actually sets one changes behaviour, so every
/// other group keeps behaving exactly like before this field existed.
///
/// NOTE: `retry` is deliberately NOT skipped when serializing — `None` must
/// come back out as `"retry": null`, because the console tells "has its own
/// config" from "follows the global one" by looking at this very field.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GroupOverrides {
    #[serde(default)]
    pub retry: Option<RetryConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub id: String,
    pub name: String,
    /// The exposed model names this group answers for.
    ///
    /// NOT hand-written input any more: the server RE-DERIVES this list from
    /// the group's own members on every load and every save
    /// (`Config::normalize_exposed_models`), so the console only displays it.
    /// The field stays in the struct — and in the JSON — because the request
    /// path reads exactly this vector: `router::is_wildcard` decides narrow
    /// vs wildcard from it, `routes::list_models` lists it, and the 404 guard
    /// matches against it. Right after normalization it IS the derived list.
    ///
    /// `#[serde(default)]` keeps hand-written config files (and any older
    /// document) loadable without spelling the field out.
    #[serde(default)]
    pub exposed_models: Vec<String>,
    pub tiers: Vec<Tier>,
    /// Optional per-group settings; everything absent follows `Config::retry`.
    #[serde(default)]
    pub overrides: GroupOverrides,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub key: String,
    pub group_id: String,
    pub name: String,
    #[serde(default = "d_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub listen: ListenConfig,
    #[serde(default)]
    pub admin: AdminConfig,
    #[serde(default)]
    pub retry: RetryConfig,
    /// Routing-order switch. false (default): each tier is tried in the order
    /// the members are declared in the config, with no weight/id sorting.
    /// true: opt-in auto weighting — each tier is sorted by member weight
    /// descending, ties keeping the declared order.
    #[serde(default)]
    pub auto_weight: bool,
    #[serde(default)]
    pub channels: Vec<Channel>,
    #[serde(default)]
    pub models: Vec<ModelBinding>,
    #[serde(default)]
    pub groups: Vec<Group>,
    #[serde(default)]
    pub api_keys: Vec<ApiKey>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: ListenConfig::default(),
            admin: AdminConfig::default(),
            retry: RetryConfig::default(),
            auto_weight: false,
            channels: vec![],
            models: vec![],
            groups: vec![],
            api_keys: vec![],
        }
    }
}

impl Config {
    pub fn find_channel(&self, id: &str) -> Option<&Channel> {
        self.channels.iter().find(|c| c.id == id)
    }
    pub fn find_model(&self, id: &str) -> Option<&ModelBinding> {
        self.models.iter().find(|m| m.id == id)
    }

    /// Re-derive `exposed_models` for EVERY group from its own members.
    /// Returns how many groups the derivation actually changed, so callers can
    /// log/persist only when something moved.
    ///
    /// This is the single source of truth for "what does this group answer
    /// for": a name is exposed because some member declares it, never because
    /// somebody typed it into a text box. Both write paths call it —
    /// `PUT /admin/api/config` (before saving, so a hand-edited or stale
    /// client document cannot smuggle a list back in) and process start
    /// (before publishing, so an old database converges on the way up).
    ///
    /// WILD-CARD NAMES ARE UNAFFECTED: `router::is_wildcard` answers `false`
    /// for a name found here (EXACT WINS, narrow plan) and falls back to
    /// `name`/`id` (wildcard plan) for everything else. Both of those keep
    /// working whether or not the derivation runs, so no request that resolved
    /// before this existed can start failing after it.
    pub fn normalize_exposed_models(&mut self) -> usize {
        let want: Vec<Vec<String>> = self
            .groups
            .iter()
            .map(|g| derive_exposed_models(self, g))
            .collect();
        let mut changed = 0;
        for (group, want) in self.groups.iter_mut().zip(want) {
            if group.exposed_models != want {
                group.exposed_models = want;
                changed += 1;
            }
        }
        changed
    }
}

/// The exposed-model names ONE group answers for, derived from its own members.
///
/// Walk: tiers in ascending `level`, declared member order inside a tier,
/// taking each member binding's `exposed` the first time that exact string is
/// seen (so the list is ordered by "which tier introduces this name first" and
/// free of duplicates). A member id with no binding contributes nothing — a
/// dangling reference is a validation error, not a name.
///
/// Deliberately independent of `enabled`/weights on both the member and its
/// channel: a name that is exposed stays exposed, and `router::plan_with`
/// skips disabled members when it builds candidates. Deriving from "enabled
/// right now" would make a toggle silently un-expose a name a client may
/// still be calling, which is a routing change no text box ever implied.
pub fn derive_exposed_models(cfg: &Config, group: &Group) -> Vec<String> {
    let mut tiers: Vec<&Tier> = group.tiers.iter().collect();
    tiers.sort_by_key(|t| t.level);
    let mut out: Vec<String> = Vec::new();
    for tier in tiers {
        for member in &tier.members {
            let Some(m) = cfg.find_model(&member.model_id) else { continue };
            if out.iter().any(|x| x == &m.exposed) {
                continue;
            }
            out.push(m.exposed.clone());
        }
    }
    out
}

/// The retry settings that are in force for ONE group.
///
/// A group that carries its own `overrides.retry` uses it verbatim; every other
/// group borrows the global `cfg.retry` — the same reference, so nothing can
/// drift between the two paths. Callers must pass overrides through
/// `crate::router::validate_budget` before saving (a group budget that violates
/// the deadline invariants would otherwise be accepted here and only fail later).
pub fn effective_retry<'a>(cfg: &'a Config, group: &'a Group) -> &'a RetryConfig {
    group.overrides.retry.as_ref().unwrap_or(&cfg.retry)
}

/// Monotonic config version, bumped on every published swap.
static CONFIG_VERSION: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub struct ConfigSnapshot {
    pub config: std::sync::Arc<Config>,
    pub version: u64,
}

/// Atomic config holder: readers grab one snapshot per request and keep it.
#[derive(Clone, Default)]
pub struct ConfigHolder {
    inner: std::sync::Arc<std::sync::RwLock<Option<ConfigSnapshot>>>,
}

impl ConfigHolder {
    pub fn publish(&self, config: Config) -> u64 {
        let v = CONFIG_VERSION.fetch_add(1, Ordering::SeqCst) + 1;
        let snap = ConfigSnapshot { config: std::sync::Arc::new(config), version: v };
        *self.inner.write().unwrap() = Some(snap);
        v
    }
    pub fn get(&self) -> Option<ConfigSnapshot> {
        self.inner.read().unwrap().clone()
    }
}
