//! Live in-flight concurrency registry — the ONLY data source for "使用中".
//!
//! The console's "使用中" badge means exactly one thing: how many requests are
//! using this upstream account RIGHT NOW. Two earlier readings were fake and
//! are what this module replaces:
//!   * last-success landing spot (see `store::active_member_key`) — it keeps
//!     saying "in use" long after the request is gone;
//!   * the static "referenced by a tier-1 group" highlight in Channels.vue —
//!     it is a config declaration and says nothing about traffic.
//!
//! A counter is entered when an attempt is about to occupy a
//! (channel, upstream model) pair and released when that attempt stops
//! occupying it. For a streaming request the release point is the END of the
//! POST-COMMIT pump, not the arrival of the response headers: the account is
//! still busy while the body is being relayed to the client.

use dashmap::DashMap;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

/// The one key format shared by the whole product: the same string
/// `store::active_member_key` produces and `GroupMember.key` carries, so the
/// console can look a member up in `members` without any translation.
pub fn member_key(channel_id: &str, upstream_model: &str) -> String {
    format!("{channel_id}::{upstream_model}")
}

#[derive(Default)]
pub struct Inflight {
    /// member key -> number of requests currently occupying that pair.
    /// A key is REMOVED when its count reaches zero, so an idle registry is
    /// empty: the console never has to tell "absent" from "present but 0".
    map: DashMap<String, u64>,
}

impl Inflight {
    pub fn new() -> Self {
        Self { map: DashMap::new() }
    }

    /// Take one unit of occupancy on (channel, upstream model). The returned
    /// guard releases it on Drop — RAII is the point: an early return, a `?`,
    /// or a panic anywhere downstream still decrements exactly once.
    pub fn enter(self: &Arc<Self>, channel_id: &str, upstream_model: &str) -> InflightGuard {
        let key = member_key(channel_id, upstream_model);
        *self.map.entry(key.clone()).or_insert(0) += 1;
        InflightGuard { reg: Arc::clone(self), key }
    }

    /// Current concurrency for one member key; 0 when idle.
    pub fn member_count(&self, key: &str) -> u64 {
        self.map.get(key).map(|v| *v).unwrap_or(0)
    }

    /// Every request currently occupying any upstream account.
    pub fn total(&self) -> u64 {
        self.map.iter().map(|e| *e.value()).sum()
    }

    /// (total, members, channels) — the two aggregations that need no config.
    /// The channel id is everything before the first "::", matching how the
    /// key was built.
    fn raw(&self) -> (u64, BTreeMap<String, u64>, BTreeMap<String, u64>) {
        let mut members: BTreeMap<String, u64> = BTreeMap::new();
        let mut channels: BTreeMap<String, u64> = BTreeMap::new();
        let mut total = 0u64;
        for e in self.map.iter() {
            let n = *e.value();
            if n == 0 {
                continue;
            }
            let k = e.key().clone();
            total += n;
            let ch = match k.split_once("::") {
                Some((c, _)) => c.to_string(),
                None => k.clone(),
            };
            *channels.entry(ch).or_insert(0) += n;
            members.insert(k, n);
        }
        (total, members, channels)
    }

    /// The wire shape of `GET /admin/api/inflight`.
    ///
    /// `groups` needs the config because a group is a list of model ids while
    /// the registry is keyed by (channel, upstream). A member whose binding or
    /// channel vanished contributes nothing; the same (channel, upstream) pair
    /// reached through two different model ids is counted ONCE per group, the
    /// same dedupe `router::plan_with` and `group_members` apply.
    pub fn snapshot_json(&self, cfg: Option<&crate::config::Config>) -> Value {
        let (total, members, channels) = self.raw();
        let mut groups: BTreeMap<String, u64> = BTreeMap::new();
        if let Some(cfg) = cfg {
            for g in &cfg.groups {
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                let mut n = 0u64;
                for tier in &g.tiers {
                    for mem in &tier.members {
                        let Some(m) = cfg.find_model(&mem.model_id) else { continue };
                        let key = member_key(&m.channel_id, &m.upstream);
                        if !seen.insert(key.clone()) {
                            continue;
                        }
                        n += self.member_count(&key);
                    }
                }
                groups.insert(g.id.clone(), n);
            }
        }
        json!({
            "total": total,
            "members": members,
            "channels": channels,
            "groups": groups,
        })
    }
}

/// One unit of occupancy. Dropping it decrements the counter and deletes the
/// key at zero, under the same shard lock, so concurrent drops cannot leave a
/// stale zero row behind or race each other into negative counts.
pub struct InflightGuard {
    reg: Arc<Inflight>,
    key: String,
}

impl InflightGuard {
    pub fn key(&self) -> &str {
        &self.key
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        use dashmap::mapref::entry::Entry;
        if let Entry::Occupied(mut o) = self.reg.map.entry(self.key.clone()) {
            if *o.get() <= 1 {
                o.remove();
            } else {
                *o.get_mut() -= 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_and_drop_returns_to_empty() {
        let reg = Arc::new(Inflight::new());
        assert_eq!(reg.total(), 0);
        {
            let _g = reg.enter("ch_a", "m1");
            assert_eq!(reg.total(), 1);
            assert_eq!(reg.member_count("ch_a::m1"), 1);
        }
        assert_eq!(reg.total(), 0);
        assert_eq!(reg.member_count("ch_a::m1"), 0);
        // The key is GONE, not a lingering zero: "absent" is the idle state.
        assert!(reg.map.is_empty());
    }

    #[test]
    fn concurrent_guards_stack_on_one_key() {
        let reg = Arc::new(Inflight::new());
        let a = reg.enter("ch_a", "m1");
        let b = reg.enter("ch_a", "m1");
        let c = reg.enter("ch_a", "m1");
        assert_eq!(reg.member_count("ch_a::m1"), 3);
        assert_eq!(reg.total(), 3);
        drop(b);
        assert_eq!(reg.member_count("ch_a::m1"), 2);
        drop(a);
        assert_eq!(reg.member_count("ch_a::m1"), 1);
        drop(c);
        assert_eq!(reg.total(), 0);
        assert!(reg.map.is_empty());
    }

    #[test]
    fn distinct_members_and_channels_aggregate() {
        let reg = Arc::new(Inflight::new());
        let _a = reg.enter("ch_a", "m1");
        let _b = reg.enter("ch_a", "m2");
        let _c = reg.enter("ch_b", "m1");
        let v = reg.snapshot_json(None);
        assert_eq!(v["total"], 3);
        assert_eq!(v["members"]["ch_a::m1"], 1);
        assert_eq!(v["members"]["ch_a::m2"], 1);
        assert_eq!(v["members"]["ch_b::m1"], 1);
        assert_eq!(v["channels"]["ch_a"], 2);
        assert_eq!(v["channels"]["ch_b"], 1);
        // No config supplied -> groups is an empty object, never null.
        assert_eq!(v["groups"], json!({}));
    }

    #[test]
    fn zero_rows_are_not_reported() {
        let reg = Arc::new(Inflight::new());
        {
            let _g = reg.enter("ch_a", "m1");
        }
        let v = reg.snapshot_json(None);
        assert_eq!(v["total"], 0);
        assert_eq!(v["members"], json!({}));
        assert_eq!(v["channels"], json!({}));
    }

    #[test]
    fn key_format_matches_the_rest_of_the_product() {
        assert_eq!(member_key("ch_real", "deepseek-v4.1-flash"), "ch_real::deepseek-v4.1-flash");
    }
}
