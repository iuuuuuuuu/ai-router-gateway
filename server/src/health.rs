//! Health/cooldown registry. Attribution rules (v0.4 plan §5):
//! - 401/403  -> channel-level ("*"), permanent until manual clear/reconfig
//! - 404 model_not_found -> (channel, model) long exclusion, persisted
//! - 429      -> (channel, model), max(Retry-After, backoff)
//! - 5xx/timeout/connect -> (channel, model), exponential backoff with cap
//! Backoff advances only after consecutive failures >= threshold; one success
//! resets (avoids the "permanently fused 429 channel" bug from v0.2).

use crate::config::{Channel, Config};
use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// upstream_model == "*" means whole-channel (auth-level failures).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CooldownKey {
    pub channel_id: String,
    pub upstream_model: String,
}

impl CooldownKey {
    pub fn channel_only(channel_id: &str) -> Self {
        Self { channel_id: channel_id.into(), upstream_model: "*".into() }
    }
    pub fn scoped(channel_id: &str, upstream_model: &str) -> Self {
        Self { channel_id: channel_id.into(), upstream_model: upstream_model.into() }
    }
}

#[derive(Debug, Clone)]
pub struct CooldownState {
    pub until: Instant,
    pub consecutive_failures: u32,
    /// "auth" = needs reconfig; "model" = long exclusion; "backoff" = transient
    pub kind: &'static str,
    /// true only for kind == "auth": the isolation lasts until a manual clear
    /// or a reconfig. "until" is then a 100-year sentinel, NOT a real expiry —
    /// this flag is what makes that explicit (see earliest_expiry).
    pub permanent: bool,
}

/// Sentinel cooldown length for an auth-level (401/403) isolation: 100 years.
/// Large enough that the entry never expires on its own, small enough that
/// Instant::now() + this cannot overflow.
pub const AUTH_PERMANENT_MS: u64 = 100 * 365 * 24 * 3600 * 1000;

/// One cooling entry, shaped for the admin health API. Field names are part of
/// the frozen client contract — do not rename.
#[derive(Debug, Clone)]
pub struct CooldownRow {
    pub channel_id: String,
    pub upstream_model: String,
    pub kind: &'static str,
    pub permanent: bool,
    /// absolute epoch ms; 0 when permanent
    pub cooling_until_ms: u64,
    /// remaining ms; 0 when permanent
    pub remaining_ms: u64,
    pub consecutive_failures: u32,
}

pub struct HealthRegistry {
    map: DashMap<CooldownKey, CooldownState>,
    store: Option<Arc<crate::store::Store>>,
}

const BACKOFF_BASE_MS: u64 = 15_000;
const BACKOFF_FACTOR: u64 = 2;
const BACKOFF_MAX_MS: u64 = 300_000;
const FAILURES_BEFORE_COOLDOWN: u32 = 2;
const MODEL_EXCLUDE_MS: u64 = 6 * 3600 * 1000;

pub enum Attribution {
    Cooldown(CooldownKey, Duration, &'static str),
    /// Do not cool down at all (client-side error, content filter...).
    None,
}

impl HealthRegistry {
    pub fn new(store: Option<Arc<crate::store::Store>>) -> Self {
        let reg = Self { map: DashMap::new(), store };
        reg.load_persisted();
        reg
    }

    fn load_persisted(&self) {
        let Some(store) = &self.store else { return };
        let Ok(rows) = store.load_cooldowns() else { return };
        let now_ms = crate::store::now_ms();
        for (ch, model, until_ms, fails, kind) in rows {
            if until_ms <= now_ms { continue; }
            self.map.insert(
                CooldownKey { channel_id: ch, upstream_model: model },
                CooldownState {
                    until: Instant::now() + Duration::from_millis((until_ms - now_ms) as u64),
                    consecutive_failures: fails as u32,
                    kind: match kind.as_str() { "model" => "model", "auth" => "auth", _ => "backoff" },
                    // auth rows are never persisted (see persist()), so a row
                    // read back from the DB is never permanent.
                    permanent: false,
                },
            );
        }
    }

    pub fn persist(&self) {
        let Some(store) = &self.store else { return };
        let now_ms = crate::store::now_ms();
        let rows: Vec<(String, String, i64, i64, String)> = self
            .map
            .iter()
            .filter(|e| e.value().until > Instant::now() && e.value().kind != "auth")
            .map(|e| {
                let until = now_ms + e.value().until.duration_since(Instant::now()).as_millis() as i64;
                (e.key().channel_id.clone(), e.key().upstream_model.clone(), until, e.value().consecutive_failures as i64, e.value().kind.to_string())
            })
            .collect();
        let _ = store.save_cooldowns(&rows);
    }

    pub fn is_cooling(&self, key: &CooldownKey) -> bool {
        match self.map.get(key) {
            Some(st) => st.until > Instant::now(),
            None => false,
        }
    }

    /// Earliest expiry among the given keys, for WAIT_FOR_COOLDOWN.
    ///
    /// Permanent (auth) entries are SKIPPED: they never expire on their own, so
    /// treating one as a wait target would park the request on a 100-year sleep
    /// until the total budget ran out. Skipping them means "every candidate is
    /// permanently isolated" naturally degrades to an empty plan (Terminal
    /// NoCandidate / AllCooldown) instead of a bogus wait.
    pub fn earliest_expiry(&self, keys: &[CooldownKey]) -> Option<Instant> {
        let mut earliest: Option<Instant> = None;
        for k in keys {
            if let Some(st) = self.map.get(k) {
                if st.permanent { continue; }
                if st.until > Instant::now() {
                    earliest = Some(match earliest { Some(e) if e <= st.until => e, _ => st.until });
                }
            }
        }
        earliest
    }

    pub fn apply(&self, attr: Attribution) {
        match attr {
            Attribution::None => {}
            Attribution::Cooldown(key, dur, kind) => {
                let mut entry = self.map.entry(key).or_insert(CooldownState {
                    until: Instant::now(),
                    consecutive_failures: 0,
                    kind,
                    permanent: kind == "auth",
                });
                entry.consecutive_failures += 1;
                entry.kind = kind;
                // Written unconditionally: a key can move between kinds (e.g. a
                // backoff that later turns out to be an auth failure), so a
                // stale permanent=true must never survive.
                entry.permanent = kind == "auth";
                entry.until = if entry.consecutive_failures < FAILURES_BEFORE_COOLDOWN {
                    // first failure: no cooldown yet (fast retry allowed), but
                    // remember the failure so the second one triggers cooling.
                    // Applies to auth too: the threshold semantics are unchanged.
                    Instant::now()
                } else {
                    Instant::now() + dur
                };
            }
        }
    }

    pub fn record_success(&self, key: &CooldownKey) {
        self.map.remove(key);
    }

    pub fn backoff_for(&self, key: &CooldownKey) -> Duration {
        let fails = self.map.get(key).map(|s| s.consecutive_failures).unwrap_or(0);
        let exp = fails.saturating_sub(FAILURES_BEFORE_COOLDOWN) as u32;
        let ms = BACKOFF_BASE_MS.saturating_mul(BACKOFF_FACTOR.saturating_pow(exp)).min(BACKOFF_MAX_MS);
        Duration::from_millis(ms)
    }

    pub fn model_exclusion() -> Duration { Duration::from_millis(MODEL_EXCLUDE_MS) }

    /// Cooldown length for a 401/403: effectively permanent (100 years).
    /// Paired with CooldownState::permanent so it is never mistaken for a real
    /// expiry by earliest_expiry / the health API.
    pub fn auth_permanent() -> Duration { Duration::from_millis(AUTH_PERMANENT_MS) }

    /// Every entry that is currently cooling, in the frozen shape of the
    /// admin health contract. Sorted by (channel_id, upstream_model) so the UI
    /// rows never reshuffle between polls.
    pub fn snapshot_summary(&self) -> Vec<CooldownRow> {
        let now = Instant::now();
        let now_ms = crate::store::now_ms().max(0) as u64;
        let mut rows: Vec<CooldownRow> = self
            .map
            .iter()
            .filter(|e| e.value().until > now)
            .map(|e| {
                let remaining_ms = e.value().until.duration_since(now).as_millis() as u64;
                let permanent = e.value().permanent;
                CooldownRow {
                    channel_id: e.key().channel_id.clone(),
                    upstream_model: e.key().upstream_model.clone(),
                    kind: e.value().kind,
                    permanent,
                    // A permanent isolation has no real expiry: the 100-year
                    // sentinel must never be shown as a countdown.
                    cooling_until_ms: if permanent { 0 } else { now_ms.saturating_add(remaining_ms) },
                    remaining_ms: if permanent { 0 } else { remaining_ms },
                    consecutive_failures: e.value().consecutive_failures,
                }
            })
            .collect();
        rows.sort_by(|a, b| {
            a.channel_id.cmp(&b.channel_id).then_with(|| a.upstream_model.cmp(&b.upstream_model))
        });
        rows
    }

    pub fn clear_all(&self) { self.map.clear(); }

    pub fn clear_channel(&self, channel_id: &str) {
        self.map.retain(|k, _| k.channel_id != channel_id);
    }

    /// Defect B: re-align the cooldown map with a config that was just saved.
    ///
    /// Without this, a cooldown row outlives the thing it describes: an api_key
    /// fix keeps the channel isolated for 100 years (kind="auth"), and a
    /// deleted channel keeps showing up as cooling in the admin health view.
    /// Rules (frozen contract):
    ///   a) a channel that is gone from the new config loses ALL of its rows
    ///      (channel-level AND scoped);
    ///   c) a channel whose api_key or base_url changed loses all of its rows —
    ///      the endpoint identity changed, so the old verdicts are meaningless;
    ///   d) a channel whose api_key AND base_url are both unchanged keeps its
    ///      cooldown state untouched (no blanket clear);
    ///   b) a scoped row whose (channel_id, upstream_model) pair no longer
    ///      exists in the new config is dropped too.
    ///
    /// `old_channels` is the channel list of the previously published config
    /// (empty when there was none). Returns the number of cleared rows so the
    /// caller can log/assert it.
    pub fn reconcile_config(&self, old_channels: &[Channel], new: &Config) -> usize {
        let mut doomed: Vec<CooldownKey> = vec![];

        // (a) + (c): channel vanished, or its endpoint identity changed.
        for old in old_channels {
            let still_there = new.channels.iter().find(|c| c.id == old.id);
            let identity_changed = match still_there {
                None => true,
                Some(c) => c.api_key != old.api_key || c.base_url != old.base_url,
            };
            if identity_changed {
                for e in self.map.iter() {
                    if e.key().channel_id == old.id {
                        doomed.push(e.key().clone());
                    }
                }
            }
        }

        // (b): scoped row whose (channel, upstream model) pair is gone.
        for e in self.map.iter() {
            let key = e.key();
            if key.upstream_model == "*" {
                continue; // channel-level rows are handled by (a)/(c)
            }
            let pair_exists = new
                .models
                .iter()
                .any(|m| m.channel_id == key.channel_id && m.upstream == key.upstream_model);
            if !pair_exists {
                doomed.push(key.clone());
            }
        }

        doomed.sort_by(|a, b| a.channel_id.cmp(&b.channel_id).then_with(|| a.upstream_model.cmp(&b.upstream_model)));
        doomed.dedup();
        for key in &doomed {
            self.map.remove(key);
        }
        doomed.len()
    }
}