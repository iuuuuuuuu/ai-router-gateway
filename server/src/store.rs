//! SQLite storage. Three logical stores kept in one file for v1 simplicity
//! (config row / logs / cooldown persistence), single writer connection +
//! mutex: WAL still allows concurrent readers from other connections, and we
//! funnel every write through one queue (see P1-26/P1-27 review findings).

use crate::config::Config;
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use std::sync::Mutex;

pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SwitchLog {
    pub ts: String,
    pub request_id: String,
    /// Which routing group the request entered through. Stored so the console
    /// can scope the switch log to one group instead of only "everything".
    pub group_id: String,
    pub attempt_no: u32,
    pub tier: u32,
    pub channel_id: String,
    pub channel_name: String,
    pub upstream_model: String,
    pub outcome: String,        // committed | failed (written at routes.rs:174)
    pub error_class: String,    // see classify::ErrorClass
    pub http_status: Option<i64>,
    pub latency_ms: i64,
    pub message: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RequestLog {
    pub id: Option<i64>,
    pub ts: String,
    pub request_id: String,
    pub group_id: String,
    pub exposed_model: String,
    pub entry_protocol: String,
    pub status: String,           // success | exhausted | cooldown | config | aborted
    pub final_channel: Option<String>,
    pub final_upstream_model: Option<String>,
    pub attempts: u32,
    /// How many attempts were discarded by the loop guard before one produced
    /// a committed answer (i.e. attempts whose `error_class` was
    /// `degenerate_output`). 0 means no degenerate output was ever detected.
    pub loop_retries: u32,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub cost_usd: f64,
    pub wasted_cost_usd: f64,
    pub duration_ms: i64,
    pub ttft_ms: i64,
}

/// Builds the shared `WHERE` clause for the two log queries. Returns the
/// clause (with a leading space, or "" when unfiltered) plus the bind values
/// in positional order; the caller appends `LIMIT` with the next index so a
/// filtered and an unfiltered query share one code path.
fn build_filters(request_id: Option<&str>, group_id: Option<&str>, limit: i64) -> (String, Vec<String>) {
    let mut clauses: Vec<&str> = Vec::new();
    let mut binds: Vec<String> = Vec::new();
    if let Some(rid) = request_id {
        if !rid.is_empty() {
            clauses.push("request_id = ?");
            binds.push(rid.to_string());
        }
    }
    if let Some(gid) = group_id {
        if !gid.is_empty() {
            clauses.push("group_id = ?");
            binds.push(gid.to_string());
        }
    }
    binds.push(limit.to_string());
    if clauses.is_empty() {
        (String::new(), binds)
    } else {
        (format!(" WHERE {}", clauses.join(" AND ")), binds)
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Store {
    pub fn open(path: &str) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "wal_autocheckpoint", 4000)?;
        Self::migrate(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn open_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::migrate(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn migrate(conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS config_json (id INTEGER PRIMARY KEY CHECK (id = 1), json TEXT NOT NULL, updated_at TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS request_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts TEXT NOT NULL, request_id TEXT NOT NULL, group_id TEXT NOT NULL,
                exposed_model TEXT NOT NULL, entry_protocol TEXT NOT NULL, status TEXT NOT NULL,
                final_channel TEXT, final_upstream_model TEXT, attempts INTEGER NOT NULL DEFAULT 0,
                input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0, cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                cost_usd REAL NOT NULL DEFAULT 0, wasted_cost_usd REAL NOT NULL DEFAULT 0,
                duration_ms INTEGER NOT NULL DEFAULT 0, ttft_ms INTEGER NOT NULL DEFAULT 0,
                loop_retries INTEGER NOT NULL DEFAULT 0);
             CREATE INDEX IF NOT EXISTS idx_rl_ts ON request_logs(ts);
             CREATE INDEX IF NOT EXISTS idx_rl_model ON request_logs(exposed_model);
             CREATE INDEX IF NOT EXISTS idx_rl_status ON request_logs(status);
             CREATE TABLE IF NOT EXISTS switch_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts TEXT NOT NULL, request_id TEXT NOT NULL, group_id TEXT NOT NULL DEFAULT '',
                attempt_no INTEGER NOT NULL, tier INTEGER NOT NULL,
                channel_id TEXT NOT NULL, channel_name TEXT NOT NULL, upstream_model TEXT NOT NULL,
                outcome TEXT NOT NULL, error_class TEXT NOT NULL, http_status INTEGER,
                latency_ms INTEGER NOT NULL, message TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS idx_sl_req ON switch_logs(request_id);
             CREATE INDEX IF NOT EXISTS idx_sl_ts ON switch_logs(ts);
             CREATE TABLE IF NOT EXISTS cooldown_persist (
                channel_id TEXT NOT NULL, upstream_model TEXT NOT NULL,
                until_ms INTEGER NOT NULL, consecutive_failures INTEGER NOT NULL, kind TEXT NOT NULL,
                PRIMARY KEY (channel_id, upstream_model));",
        )?;
        // Column-level migration: a database created before group_id existed
        // keeps its old switch_logs table, and CREATE TABLE IF NOT EXISTS is a
        // no-op there. Add the column explicitly when it is missing — and only
        // then build the group indexes, because an index over a column that the
        // legacy table does not have yet fails the whole batch ("no such column:
        // group_id"). Ordering matters: table -> column -> index.
        for table in ["switch_logs", "request_logs"] {
            let has_group: bool = conn
                .prepare(&format!("PRAGMA table_info({table})"))?
                .query_map([], |r| r.get::<_, String>(1))?
                .collect::<Result<Vec<_>, _>>()?
                .iter()
                .any(|c| c == "group_id");
            if !has_group {
                conn.execute_batch(&format!(
                    "ALTER TABLE {table} ADD COLUMN group_id TEXT NOT NULL DEFAULT '';"
                ))?;
            }
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_sl_group ON switch_logs(group_id);
             CREATE INDEX IF NOT EXISTS idx_rl_group ON request_logs(group_id);",
        )?;
        // Same reasoning as the group_id backfill above: a database created
        // before loop_retries existed keeps its old request_logs table, so the
        // column is added explicitly when it is missing. The default of 0 is
        // what makes every historical row read as "no loop retries".
        let has_loop_retries: bool = conn
            .prepare("PRAGMA table_info(request_logs)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|c| c == "loop_retries");
        if !has_loop_retries {
            conn.execute_batch("ALTER TABLE request_logs ADD COLUMN loop_retries INTEGER NOT NULL DEFAULT 0;")?;
        }
        Ok(())
    }

    pub fn load_config(&self) -> anyhow::Result<Option<Config>> {
        let conn = self.conn.lock().unwrap();
        let json: Option<String> = conn
            .query_row("SELECT json FROM config_json WHERE id = 1", [], |r| r.get(0))
            .optional()?;
        Ok(json.map(|j| serde_json::from_str(&j)).transpose()?)
    }

    pub fn save_config(&self, cfg: &Config) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let json = serde_json::to_string(cfg)?;
        conn.execute(
            "INSERT INTO config_json (id, json, updated_at) VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET json = ?1, updated_at = ?2",
            params![json, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn insert_switch_log(&self, l: &SwitchLog) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        // fail-open: logging must never break the request path
        let _ = conn.execute(
            "INSERT INTO switch_logs (ts, request_id, group_id, attempt_no, tier, channel_id, channel_name, upstream_model, outcome, error_class, http_status, latency_ms, message)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![l.ts, l.request_id, l.group_id, l.attempt_no, l.tier, l.channel_id, l.channel_name, l.upstream_model, l.outcome, l.error_class, l.http_status, l.latency_ms, l.message],
        );
        Ok(())
    }

    pub fn insert_request_log(&self, l: &RequestLog) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO request_logs (ts, request_id, group_id, exposed_model, entry_protocol, status, final_channel, final_upstream_model, attempts, loop_retries, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, cost_usd, wasted_cost_usd, duration_ms, ttft_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
            params![l.ts, l.request_id, l.group_id, l.exposed_model, l.entry_protocol, l.status, l.final_channel, l.final_upstream_model, l.attempts, l.loop_retries, l.input_tokens, l.output_tokens, l.cache_read_tokens, l.cache_creation_tokens, l.cost_usd, l.wasted_cost_usd, l.duration_ms, l.ttft_ms],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// `group_id` scopes the log to one routing group, `request_id` to one request.
    /// Both are optional and combine (a request belongs to exactly one group).
    pub fn list_switch_logs(&self, limit: i64, request_id: Option<&str>, group_id: Option<&str>) -> anyhow::Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock().unwrap();
        let cols = "json_object('id',id,'ts',ts,'request_id',request_id,'group_id',group_id,'attempt_no',attempt_no,'tier',tier,'channel_id',channel_id,'channel_name',channel_name,'upstream_model',upstream_model,'outcome',outcome,'error_class',error_class,'http_status',http_status,'latency_ms',latency_ms,'message',message)";
        let (where_sql, binds) = build_filters(request_id, group_id, limit);
        let sql = format!("SELECT {cols} FROM switch_logs{where_sql} ORDER BY id DESC LIMIT ?{}", binds.len());
        // bind the SQL to a local so the borrow outlives the prepared statement
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().map(|s| serde_json::from_str(&s).unwrap()).collect())
    }

    pub fn list_request_logs(&self, limit: i64, request_id: Option<&str>, group_id: Option<&str>) -> anyhow::Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock().unwrap();
        let cols = "json_object('id',id,'ts',ts,'request_id',request_id,'group_id',group_id,'exposed_model',exposed_model,'entry_protocol',entry_protocol,'status',status,'final_channel',final_channel,'final_upstream_model',final_upstream_model,'attempts',attempts,'loop_retries',loop_retries,'input_tokens',input_tokens,'output_tokens',output_tokens,'cache_read_tokens',cache_read_tokens,'cache_creation_tokens',cache_creation_tokens,'cost_usd',cost_usd,'wasted_cost_usd',wasted_cost_usd,'duration_ms',duration_ms,'ttft_ms',ttft_ms)";
        let (where_sql, binds) = build_filters(request_id, group_id, limit);
        // bind the SQL to a local so the borrow outlives the prepared statement
        let sql = format!("SELECT {cols} FROM request_logs{where_sql} ORDER BY id DESC LIMIT ?{}", binds.len());
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().map(|s| serde_json::from_str(&s).unwrap()).collect())
    }

    /// Per-member aggregates for ONE group, keyed by
    /// `"<channel_id>::<upstream_model>"` — the same key the console uses as a
    /// member row id (task-27 deliverable 2).
    ///
    /// Two independent passes, both filtered by `group_id` BEFORE aggregating:
    ///   * successes come from `request_logs` (status='success' with a landed
    ///     final_channel + final_upstream_model): requests / tokens / cost /
    ///     avg_duration_ms / avg_ttft_ms / last_success_ts;
    ///   * failures come from `switch_logs` (outcome='failed'): failed_attempts /
    ///     loop_retries / per-class error_classes / last_fail_ts /
    ///     last_error_class.
    /// A member with no rows at all simply does not appear in the map; the caller
    /// renders its missing counts as a real 0 and its missing averages as null.
    ///
    /// The two averages are deliberately NOT coalesced to 0: with no successful
    /// request they stay SQL NULL, because "no data" and "0 ms" are different
    /// facts and the console prints null as "—".
    ///
    /// `last_outcome` / `last_error_class` are derived from the two timestamps
    /// (both written by routes.rs::iso_now — local +08:00 with fixed millisecond
    /// precision — so a plain string compare is chronological): the newer side
    /// wins, and the error class is reported only when the newer side is a
    /// failure, so a member whose last landed request succeeded reports
    /// `last_outcome: "success", last_error_class: null`.
    pub fn group_member_stats(&self, group_id: &str) -> anyhow::Result<std::collections::HashMap<String, serde_json::Value>> {
        let conn = self.conn.lock().unwrap();
        let key = "final_channel || '::' || final_upstream_model";
        let mut out: std::collections::HashMap<String, serde_json::Value> = std::collections::HashMap::new();

        let mut stmt = conn.prepare(&format!(
            "SELECT {key}, json_object('requests',COUNT(*),'input_tokens',COALESCE(SUM(input_tokens),0),'output_tokens',COALESCE(SUM(output_tokens),0),'cost_usd',COALESCE(ROUND(SUM(cost_usd),6),0),'avg_duration_ms',ROUND(AVG(duration_ms)),'avg_ttft_ms',ROUND(AVG(ttft_ms)),'last_success_ts',MAX(ts)) \
             FROM request_logs WHERE group_id = ?1 AND status = 'success' AND final_channel IS NOT NULL AND final_upstream_model IS NOT NULL \
             GROUP BY {key}"
        ))?;
        let rows = stmt
            .query_map(params![group_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        for (k, js) in rows {
            out.insert(k, serde_json::from_str(&js).unwrap_or(serde_json::Value::Null));
        }

        let mut stmt = conn.prepare(
            "SELECT s.channel_id || '::' || s.upstream_model, json_object(\
                'failed_attempts',SUM(s.n),\
                'loop_retries',SUM(s.lr),\
                'error_classes',json_group_array(json_object('error_class',s.error_class,'n',s.n)),\
                'last_fail_ts',MAX(s.ts),\
                'last_error_class',(SELECT s2.error_class FROM switch_logs s2 WHERE s2.group_id = ?1 AND s2.outcome = 'failed' AND s2.channel_id = s.channel_id AND s2.upstream_model = s.upstream_model ORDER BY s2.ts DESC, s2.id DESC LIMIT 1)) \
             FROM (SELECT channel_id, upstream_model, error_class, COUNT(*) AS n, MAX(ts) AS ts, SUM(CASE WHEN error_class = 'degenerate_output' THEN 1 ELSE 0 END) AS lr \
                   FROM switch_logs WHERE group_id = ?1 AND outcome = 'failed' GROUP BY channel_id, upstream_model, error_class) AS s \
             GROUP BY s.channel_id, s.upstream_model"
        )?;
        let rows = stmt
            .query_map(params![group_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        for (k, js) in rows {
            let v: serde_json::Value = serde_json::from_str(&js).unwrap_or(serde_json::Value::Null);
            match out.entry(k) {
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    if let (Some(dst), Some(src)) = (e.get_mut().as_object_mut(), v.as_object()) {
                        for (f, val) in src {
                            dst.insert(f.clone(), val.clone());
                        }
                    }
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(v);
                }
            }
        }

        for v in out.values_mut() {
            let Some(obj) = v.as_object_mut() else { continue };
            let succ = obj.get("last_success_ts").and_then(|x| x.as_str()).map(|s| s.to_string());
            let fail = obj.get("last_fail_ts").and_then(|x| x.as_str()).map(|s| s.to_string());
            let last_failed = match (&succ, &fail) {
                (Some(s), Some(f)) => f > s,
                (None, Some(_)) => true,
                _ => false,
            };
            let last_used = match (&succ, &fail) {
                (Some(s), Some(f)) => Some(if s > f { s.clone() } else { f.clone() }),
                (Some(s), None) => Some(s.clone()),
                (None, Some(f)) => Some(f.clone()),
                (None, None) => None,
            };
            let outcome = if last_failed {
                Some("failed")
            } else if succ.is_some() {
                Some("success")
            } else {
                None
            };
            let last_err = if last_failed {
                obj.get("last_error_class").cloned().unwrap_or(serde_json::Value::Null)
            } else {
                serde_json::Value::Null
            };
            obj.insert("last_used_ts".into(), last_used.map(|s| serde_json::json!(s)).unwrap_or(serde_json::Value::Null));
            obj.insert("last_outcome".into(), outcome.map(|s| serde_json::json!(s)).unwrap_or(serde_json::Value::Null));
            obj.insert("last_error_class".into(), last_err);
            obj.remove("last_success_ts");
            obj.remove("last_fail_ts");
            for f in ["requests", "failed_attempts", "loop_retries", "input_tokens", "output_tokens", "cost_usd"] {
                obj.entry(f.to_string()).or_insert(serde_json::json!(0));
            }
            for f in ["avg_duration_ms", "avg_ttft_ms", "last_used_ts", "last_outcome", "last_error_class"] {
                obj.entry(f.to_string()).or_insert(serde_json::Value::Null);
            }
            obj.entry("error_classes".to_string()).or_insert(serde_json::json!([]));
        }

        Ok(out)
    }

    /// The member that served this group's most recent successful request
    /// (`"<channel_id>::<upstream_model>"`), or None when the group has never had
    /// a success. NOTE: "active" is last-success-based, NOT in-flight — a
    /// long-running stream does not change it until it lands.
    pub fn active_member_key(&self, group_id: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT final_channel || '::' || final_upstream_model FROM request_logs \
             WHERE group_id = ?1 AND status = 'success' AND final_channel IS NOT NULL AND final_upstream_model IS NOT NULL \
             ORDER BY id DESC LIMIT 1",
        )?;
        Ok(stmt.query_row(params![group_id], |r| r.get::<_, String>(0)).optional()?)
    }

    /// `group_id` narrows every figure to one routing group; `None` = global.
    pub fn stats_summary(&self, group_id: Option<&str>) -> anyhow::Result<serde_json::Value> {
        let conn = self.conn.lock().unwrap();
        let scope = if group_id.map(|g| !g.is_empty()).unwrap_or(false) { " WHERE group_id = ?1" } else { "" };
        let mut stmt = conn.prepare(&format!(
            // Sums are COALESCEd to 0 so an empty table renders as 0, not blank.
            // avg_* are deliberately NOT COALESCEd: they average only SUCCESSFUL
            // requests (a failed attempt's duration is dominated by timeouts and
            // would misrepresent latency), and with no successes at all they stay
            // NULL so the console shows "—" instead of a bogus 0ms.
            "SELECT json_object('requests',COUNT(*),'success',COALESCE(SUM(CASE WHEN status='success' THEN 1 ELSE 0 END),0),'exhausted',COALESCE(SUM(CASE WHEN status='exhausted' THEN 1 ELSE 0 END),0),'aborted',COALESCE(SUM(CASE WHEN status='aborted' THEN 1 ELSE 0 END),0),'loop_retries',COALESCE(SUM(loop_retries),0),'input_tokens',COALESCE(SUM(input_tokens),0),'output_tokens',COALESCE(SUM(output_tokens),0),'cache_read_tokens',COALESCE(SUM(cache_read_tokens),0),'cache_creation_tokens',COALESCE(SUM(cache_creation_tokens),0),'cost_usd',COALESCE(ROUND(SUM(cost_usd),6),0),'wasted_cost_usd',COALESCE(ROUND(SUM(wasted_cost_usd),6),0),'avg_duration_ms',ROUND(AVG(CASE WHEN status='success' THEN duration_ms END)),'avg_ttft_ms',ROUND(AVG(CASE WHEN status='success' THEN ttft_ms END))) FROM request_logs{scope}"
        ))?;
        let v: String = match group_id {
            Some(g) if !g.is_empty() => stmt.query_row(params![g], |r| r.get(0))?,
            _ => stmt.query_row([], |r| r.get(0))?,
        };
        Ok(serde_json::from_str(&v).unwrap())
    }

    /// Per-model usage. `group_id` narrows it to one group, which is what the
    /// console shows when a group is selected ("这个分组里具体模型用了多少").
    pub fn stats_by_model(&self, group_id: Option<&str>) -> anyhow::Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock().unwrap();
        let scope = if group_id.map(|g| !g.is_empty()).unwrap_or(false) { " WHERE group_id = ?1" } else { "" };
        let sql = format!(
            "SELECT json_object('exposed_model',exposed_model,'upstream_model',COALESCE(final_upstream_model,'-'),'channel_id',COALESCE(final_channel,'-'),'requests',COUNT(*),'input_tokens',SUM(input_tokens),'output_tokens',SUM(output_tokens),'cache_read_tokens',SUM(cache_read_tokens),'cache_creation_tokens',SUM(cache_creation_tokens),'cost_usd',ROUND(SUM(cost_usd),6),'wasted_cost_usd',ROUND(SUM(wasted_cost_usd),6)) FROM request_logs{scope} GROUP BY exposed_model, final_upstream_model, final_channel ORDER BY SUM(cost_usd) DESC LIMIT 200"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = match group_id {
            Some(g) if !g.is_empty() => stmt
                .query_map(params![g], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?,
            _ => stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?,
        };
        Ok(rows.into_iter().map(|s| serde_json::from_str(&s).unwrap()).collect())
    }

    /// One row per routing group: the "整体用了多少" overview that lets the
    /// console show every group's totals side by side without N round trips.
    pub fn stats_by_group(&self) -> anyhow::Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT json_object('group_id',group_id,'requests',COUNT(*),'success',SUM(CASE WHEN status='success' THEN 1 ELSE 0 END),'input_tokens',COALESCE(SUM(input_tokens),0),'output_tokens',COALESCE(SUM(output_tokens),0),'cost_usd',COALESCE(ROUND(SUM(cost_usd),6),0),'wasted_cost_usd',COALESCE(ROUND(SUM(wasted_cost_usd),6),0),'avg_duration_ms',ROUND(AVG(CASE WHEN status='success' THEN duration_ms END)),'models',COUNT(DISTINCT exposed_model)) FROM request_logs GROUP BY group_id ORDER BY SUM(cost_usd) DESC LIMIT 200",
        )?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().map(|s| serde_json::from_str(&s).unwrap()).collect())
    }

    pub fn save_cooldowns(&self, rows: &[(String, String, i64, i64, String)]) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM cooldown_persist", [])?;
        for (ch, m, until, fails, kind) in rows {
            let _ = conn.execute(
                "INSERT OR REPLACE INTO cooldown_persist (channel_id, upstream_model, until_ms, consecutive_failures, kind) VALUES (?1,?2,?3,?4,?5)",
                params![ch, m, until, fails, kind],
            );
        }
        Ok(())
    }

    pub fn load_cooldowns(&self) -> anyhow::Result<Vec<(String, String, i64, i64, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT channel_id, upstream_model, until_ms, consecutive_failures, kind FROM cooldown_persist")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Delete the traffic history the console displays: request logs, switch
    /// logs, and persisted cooldowns.
    ///
    /// The CONFIG IS NEVER TOUCHED. `config_json` and `meta` are deliberately
    /// not in this list: "清空日志与统计" means "forget the traffic", not "reset
    /// the gateway". A caller that wants a blank console keeps every channel,
    /// model, group and API key it had.
    ///
    /// Each flag is independent, and the returned count is the number of rows
    /// actually deleted (not merely requested), so the console can report the
    /// real effect.
    pub fn purge(&self, request_logs: bool, switch_logs: bool, cooldowns: bool) -> anyhow::Result<(usize, usize, usize)> {
        let conn = self.conn.lock().unwrap();
        let mut n_req = 0usize;
        let mut n_sw = 0usize;
        let mut n_cd = 0usize;
        if request_logs {
            n_req = conn.execute("DELETE FROM request_logs", [])?;
        }
        if switch_logs {
            n_sw = conn.execute("DELETE FROM switch_logs", [])?;
        }
        if cooldowns {
            n_cd = conn.execute("DELETE FROM cooldown_persist", [])?;
        }
        Ok((n_req, n_sw, n_cd))
    }
}


