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
                duration_ms INTEGER NOT NULL DEFAULT 0, ttft_ms INTEGER NOT NULL DEFAULT 0);
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
            "INSERT INTO request_logs (ts, request_id, group_id, exposed_model, entry_protocol, status, final_channel, final_upstream_model, attempts, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, cost_usd, wasted_cost_usd, duration_ms, ttft_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
            params![l.ts, l.request_id, l.group_id, l.exposed_model, l.entry_protocol, l.status, l.final_channel, l.final_upstream_model, l.attempts, l.input_tokens, l.output_tokens, l.cache_read_tokens, l.cache_creation_tokens, l.cost_usd, l.wasted_cost_usd, l.duration_ms, l.ttft_ms],
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
        let cols = "json_object('id',id,'ts',ts,'request_id',request_id,'group_id',group_id,'exposed_model',exposed_model,'entry_protocol',entry_protocol,'status',status,'final_channel',final_channel,'final_upstream_model',final_upstream_model,'attempts',attempts,'input_tokens',input_tokens,'output_tokens',output_tokens,'cache_read_tokens',cache_read_tokens,'cache_creation_tokens',cache_creation_tokens,'cost_usd',cost_usd,'wasted_cost_usd',wasted_cost_usd,'duration_ms',duration_ms,'ttft_ms',ttft_ms)";
        let (where_sql, binds) = build_filters(request_id, group_id, limit);
        // bind the SQL to a local so the borrow outlives the prepared statement
        let sql = format!("SELECT {cols} FROM request_logs{where_sql} ORDER BY id DESC LIMIT ?{}", binds.len());
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().map(|s| serde_json::from_str(&s).unwrap()).collect())
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
            "SELECT json_object('requests',COUNT(*),'success',SUM(CASE WHEN status='success' THEN 1 ELSE 0 END),'exhausted',SUM(CASE WHEN status='exhausted' THEN 1 ELSE 0 END),'aborted',SUM(CASE WHEN status='aborted' THEN 1 ELSE 0 END),'input_tokens',COALESCE(SUM(input_tokens),0),'output_tokens',COALESCE(SUM(output_tokens),0),'cache_read_tokens',COALESCE(SUM(cache_read_tokens),0),'cache_creation_tokens',COALESCE(SUM(cache_creation_tokens),0),'cost_usd',COALESCE(ROUND(SUM(cost_usd),6),0),'wasted_cost_usd',COALESCE(ROUND(SUM(wasted_cost_usd),6),0),'avg_duration_ms',ROUND(AVG(CASE WHEN status='success' THEN duration_ms END)),'avg_ttft_ms',ROUND(AVG(CASE WHEN status='success' THEN ttft_ms END))) FROM request_logs{scope}"
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
}
