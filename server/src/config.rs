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
    /// Exposed model names this group answers for.
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
