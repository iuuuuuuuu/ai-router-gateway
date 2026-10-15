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

/// Chinese label for a failure class. One place, so the console, the switch
/// log and the client-facing error message can never disagree about what a
/// class is called.
pub fn error_class_label(class: &str) -> &'static str {
    match class {
        // Exactly the strings router::classify_failure produces; a class with no
        // arm here falls back to a readable generic rather than an empty string.
        "degenerate_output" => "输出复读",
        "auth_error" => "鉴权失败",
        "model_not_found" => "模型不存在",
        "rate_limited" => "被限流",
        "http_5xx" => "上游服务端错误",
        "bad_request" => "请求被拒绝",
        "content_filter" => "内容被拦截",
        "connect_timeout" => "连接超时",
        "ttfb_timeout" => "首字节超时",
        "first_event_timeout" => "首个事件超时",
        "attempt_deadline" => "本次尝试超时",
        "connect_error" => "连接失败",
        "upstream_error_body" => "上游返回错误内容",
        "not_sse_garbage" => "上游返回的不是有效流",
        "empty_response" => "上游返回空内容",
        "client_aborted" => "客户端已断开",
        "committed" => "已提交",
        // Loop-guard classes that are not failures of the upstream at all: the
        // answer was corrected, or cut short on purpose. They must read as such
        // in the log, not as a mystery "other error".
        "loop_continue" => "复读后自动续写",
        "loop_continue_exhausted" => "复读纠正次数用尽，已收尾",
        _ => "其他错误",
    }
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
    /// 1 when the post-commit loop guard cut this answer short. Distinct from
    /// `loop_retries`: those attempts were discarded before the client saw a
    /// byte, whereas a truncated answer WAS delivered — just not in full.
    pub loop_truncated: i64,
    /// How many correction rounds this answer needed (the model repeated
    /// itself, was told why, and carried on). 0 means it never looped.
    pub loop_continues: u32,
    /// The failure cause of a terminal row, in the same vocabulary the switch
    /// log uses (degenerate_output, auth_error, ...). Empty on success.
    /// Without it the exhausted row said only "it failed", leaving the cause
    /// visible exclusively in the switch log.
    pub error_class: String,
    /// The Chinese, human-readable cause matching error_class.
    pub error_message: String,
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
/// in positional order; the caller appends `LIMIT`/`OFFSET` with the next
/// indexes so a filtered and an unfiltered query share one code path.
fn build_filters(request_id: Option<&str>, group_id: Option<&str>) -> (String, Vec<String>) {
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
    if clauses.is_empty() {
        (String::new(), binds)
    } else {
        (format!(" WHERE {}", clauses.join(" AND ")), binds)
    }
}

/// Clamps a paging request into something the SQL layer can always execute:
/// `limit` defaults to 100 and is capped at 1000 (the same ceiling the admin
/// route enforced before paging existed), `offset` is never negative.
fn page_bounds(limit: i64, offset: i64) -> (i64, i64) {
    (if limit <= 0 { 100 } else { limit.min(1000) }, offset.max(0))
}

/// Total number of rows a log filter matches, ignoring the page window — what
/// the console needs to draw "第 X / Y 页". It shares `build_filters` with the
/// page queries so a count can never disagree with the rows it labels.
fn count_rows(conn: &Connection, table: &str, request_id: Option<&str>, group_id: Option<&str>) -> anyhow::Result<i64> {
    let (where_sql, binds) = build_filters(request_id, group_id);
    let sql = format!("SELECT COUNT(*) FROM {table}{where_sql}");
    let n: i64 = conn.query_row(&sql, rusqlite::params_from_iter(binds.iter()), |r| r.get(0))?;
    Ok(n)
}

/// One page of log rows plus the total behind it. Always the same shape, so
/// the console can page through without a second request per page.
pub struct LogPage {
    rows: Vec<serde_json::Value>,
    total: i64,
    limit: i64,
    offset: i64,
}

impl LogPage {
    pub fn into_json(self) -> serde_json::Value {
        serde_json::json!({
            "rows": self.rows,
            "total": self.total,
            "limit": self.limit,
            "offset": self.offset,
        })
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
             -- Out-of-line secret material (channel api_key, gateway API keys).
             -- `config_json` holds a placeholder wherever one of these used to
             -- be written inline, so a plain file copy of the database no longer
             -- hands out working credentials. SPLIT, NOT SCRAMBLED: the value is
             -- stored verbatim and must round-trip byte-for-byte, because it is
             -- sent to the upstream on every request. K exists so a rotated or
             -- deleted secret can be told apart from a live one; migration is
             -- CREATE IF NOT EXISTS, so an old database gains the table empty.
             CREATE TABLE IF NOT EXISTS secrets (k TEXT PRIMARY KEY, v TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS request_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts TEXT NOT NULL, request_id TEXT NOT NULL, group_id TEXT NOT NULL,
                exposed_model TEXT NOT NULL, entry_protocol TEXT NOT NULL, status TEXT NOT NULL,
                final_channel TEXT, final_upstream_model TEXT, attempts INTEGER NOT NULL DEFAULT 0,
                input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0, cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                cost_usd REAL NOT NULL DEFAULT 0, wasted_cost_usd REAL NOT NULL DEFAULT 0,
                duration_ms INTEGER NOT NULL DEFAULT 0, ttft_ms INTEGER NOT NULL DEFAULT 0,
                loop_retries INTEGER NOT NULL DEFAULT 0,
                loop_truncated INTEGER NOT NULL DEFAULT 0,
                loop_continues INTEGER NOT NULL DEFAULT 0,
                error_class TEXT NOT NULL DEFAULT '',
                error_message TEXT NOT NULL DEFAULT '');
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
        // Same again for the post-commit truncation flag: historical rows have
        // no truncation to report, so the 0 default is the honest backfill.
        let has_loop_truncated: bool = conn
            .prepare("PRAGMA table_info(request_logs)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|c| c == "loop_truncated");
        if !has_loop_truncated {
            conn.execute_batch("ALTER TABLE request_logs ADD COLUMN loop_truncated INTEGER NOT NULL DEFAULT 0;")?;
        }
        // Correction rounds, and the cause of a terminal failure. Both backfill
        // to "nothing to report" (0 / empty), which is the honest reading of a
        // row written before the columns existed.
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(request_logs)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?;
        if !cols.iter().any(|c| c == "loop_continues") {
            conn.execute_batch("ALTER TABLE request_logs ADD COLUMN loop_continues INTEGER NOT NULL DEFAULT 0;")?;
        }
        if !cols.iter().any(|c| c == "error_class") {
            conn.execute_batch("ALTER TABLE request_logs ADD COLUMN error_class TEXT NOT NULL DEFAULT '';")?;
        }
        if !cols.iter().any(|c| c == "error_message") {
            conn.execute_batch("ALTER TABLE request_logs ADD COLUMN error_message TEXT NOT NULL DEFAULT '';")?;
        }
        Ok(())
    }

    pub fn load_config(&self) -> anyhow::Result<Option<Config>> {
        let conn = self.conn.lock().unwrap();
        let json: Option<String> = conn
            .query_row("SELECT json FROM config_json WHERE id = 1", [], |r| r.get(0))
            .optional()?;
        let Some(json) = json else { return Ok(None) };
        let mut value: serde_json::Value = serde_json::from_str(&json)?;
        restore_secrets(&conn, &mut value)?;
        Ok(Some(serde_json::from_value(value)?))
    }

    pub fn save_config(&self, cfg: &Config) -> anyhow::Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let (json, secrets) = split_secrets(cfg)?;
        // ONE transaction around BOTH halves of the same state. A crash in
        // between would otherwise leave the placeholders on disk with the rows
        // they stand for missing, and the next load would hand the gateway a
        // channel with no credential at all. Committing both or neither is the
        // only safe pairing.
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO config_json (id, json, updated_at) VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET json = ?1, updated_at = ?2",
            params![json, Utc::now().to_rfc3339()],
        )?;
        // Rewrite the whole secret set instead of upserting one by one: a
        // channel or API key the operator just deleted must not keep its
        // credential alive in the table as an orphan row.
        tx.execute("DELETE FROM secrets", [])?;
        {
            let mut stmt = tx.prepare("INSERT OR REPLACE INTO secrets (k, v) VALUES (?1, ?2)")?;
            for (k, v) in &secrets {
                stmt.execute(params![k, v])?;
            }
        }
        tx.commit()?;
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
            "INSERT INTO request_logs (ts, request_id, group_id, exposed_model, entry_protocol, status, final_channel, final_upstream_model, attempts, loop_retries, loop_truncated, loop_continues, error_class, error_message, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, cost_usd, wasted_cost_usd, duration_ms, ttft_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)",
            params![l.ts, l.request_id, l.group_id, l.exposed_model, l.entry_protocol, l.status, l.final_channel, l.final_upstream_model, l.attempts, l.loop_retries, l.loop_truncated, l.loop_continues, l.error_class, l.error_message, l.input_tokens, l.output_tokens, l.cache_read_tokens, l.cache_creation_tokens, l.cost_usd, l.wasted_cost_usd, l.duration_ms, l.ttft_ms],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// `group_id` scopes the log to one routing group, `request_id` to one request.
    /// Both are optional and combine (a request belongs to exactly one group).
    ///
    /// Unpaged entry point: the newest `limit` rows. Kept for the test-suite;
    /// the console always goes through `list_switch_logs_page` so it can page.
    pub fn list_switch_logs(&self, limit: i64, request_id: Option<&str>, group_id: Option<&str>) -> anyhow::Result<Vec<serde_json::Value>> {
        Ok(self.list_switch_logs_page(limit, 0, request_id, group_id)?.rows)
    }

    /// One page of switch logs: the newest `limit` rows after skipping `offset`,
    /// plus the total number of rows the very same filter matches.
    pub fn list_switch_logs_page(&self, limit: i64, offset: i64, request_id: Option<&str>, group_id: Option<&str>) -> anyhow::Result<LogPage> {
        let (limit, offset) = page_bounds(limit, offset);
        let conn = self.conn.lock().unwrap();
        let cols = "json_object('id',id,'ts',ts,'request_id',request_id,'group_id',group_id,'attempt_no',attempt_no,'tier',tier,'channel_id',channel_id,'channel_name',channel_name,'upstream_model',upstream_model,'outcome',outcome,'error_class',error_class,'http_status',http_status,'latency_ms',latency_ms,'message',message)";
        let total = count_rows(&conn, "switch_logs", request_id, group_id)?;
        let (where_sql, mut binds) = build_filters(request_id, group_id);
        let n = binds.len();
        binds.push(limit.to_string());
        binds.push(offset.to_string());
        let sql = format!("SELECT {cols} FROM switch_logs{where_sql} ORDER BY id DESC LIMIT ?{} OFFSET ?{}", n + 1, n + 2);
        // bind the SQL to a local so the borrow outlives the prepared statement
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(LogPage { rows: rows.into_iter().map(|s| serde_json::from_str(&s).unwrap()).collect(), total, limit, offset })
    }

    /// Unpaged entry point for request logs (see `list_switch_logs`).
    pub fn list_request_logs(&self, limit: i64, request_id: Option<&str>, group_id: Option<&str>) -> anyhow::Result<Vec<serde_json::Value>> {
        Ok(self.list_request_logs_page(limit, 0, request_id, group_id)?.rows)
    }

    /// One page of request logs: the newest `limit` rows after skipping `offset`,
    /// plus the total number of rows the very same filter matches.
    pub fn list_request_logs_page(&self, limit: i64, offset: i64, request_id: Option<&str>, group_id: Option<&str>) -> anyhow::Result<LogPage> {
        let (limit, offset) = page_bounds(limit, offset);
        let conn = self.conn.lock().unwrap();
        let cols = "json_object('id',id,'ts',ts,'request_id',request_id,'group_id',group_id,'exposed_model',exposed_model,'entry_protocol',entry_protocol,'status',status,'final_channel',final_channel,'final_upstream_model',final_upstream_model,'attempts',attempts,'loop_retries',loop_retries,'loop_truncated',loop_truncated,'loop_continues',loop_continues,'error_class',error_class,'error_message',error_message,'input_tokens',input_tokens,'output_tokens',output_tokens,'cache_read_tokens',cache_read_tokens,'cache_creation_tokens',cache_creation_tokens,'cost_usd',cost_usd,'wasted_cost_usd',wasted_cost_usd,'duration_ms',duration_ms,'ttft_ms',ttft_ms)";
        let total = count_rows(&conn, "request_logs", request_id, group_id)?;
        let (where_sql, mut binds) = build_filters(request_id, group_id);
        let n = binds.len();
        binds.push(limit.to_string());
        binds.push(offset.to_string());
        // bind the SQL to a local so the borrow outlives the prepared statement
        let sql = format!("SELECT {cols} FROM request_logs{where_sql} ORDER BY id DESC LIMIT ?{} OFFSET ?{}", n + 1, n + 2);
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(LogPage { rows: rows.into_iter().map(|s| serde_json::from_str(&s).unwrap()).collect(), total, limit, offset })
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
            "SELECT {key}, json_object('requests',COUNT(*),'input_tokens',COALESCE(SUM(input_tokens),0),'output_tokens',COALESCE(SUM(output_tokens),0),'cost_usd',COALESCE(ROUND(SUM(cost_usd),6),0),'avg_duration_ms',ROUND(AVG(duration_ms)),'avg_ttft_ms',ROUND(AVG(ttft_ms)),'loop_truncated',COALESCE(SUM(loop_truncated),0),'last_success_ts',MAX(ts)) \
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
            "SELECT json_object('requests',COUNT(*),'success',COALESCE(SUM(CASE WHEN status='success' THEN 1 ELSE 0 END),0),'exhausted',COALESCE(SUM(CASE WHEN status='exhausted' THEN 1 ELSE 0 END),0),'aborted',COALESCE(SUM(CASE WHEN status='aborted' THEN 1 ELSE 0 END),0),'loop_retries',COALESCE(SUM(loop_retries),0),'loop_truncated',COALESCE(SUM(loop_truncated),0),'loop_continues',COALESCE(SUM(loop_continues),0),'input_tokens',COALESCE(SUM(input_tokens),0),'output_tokens',COALESCE(SUM(output_tokens),0),'cache_read_tokens',COALESCE(SUM(cache_read_tokens),0),'cache_creation_tokens',COALESCE(SUM(cache_creation_tokens),0),'cost_usd',COALESCE(ROUND(SUM(cost_usd),6),0),'wasted_cost_usd',COALESCE(ROUND(SUM(wasted_cost_usd),6),0),'avg_duration_ms',ROUND(AVG(CASE WHEN status='success' THEN duration_ms END)),'avg_ttft_ms',ROUND(AVG(CASE WHEN status='success' THEN ttft_ms END))) FROM request_logs{scope}"
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

    /// One row per routing group: what the console renders when it needs
    /// every group's totals side by side without N round trips.
    ///
    /// `None` (or an empty string) = the full cross-group overview, which is
    /// what the group picker itself reads; `Some(g)` = ONLY that group's row.
    /// The two scopes share one SQL template on purpose: a scoped call is the
    /// same overview with a `WHERE`, so the picker and the selected group can
    /// never disagree about what a column means.
    pub fn stats_by_group(&self, group_id: Option<&str>) -> anyhow::Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock().unwrap();
        let scope = if group_id.map(|g| !g.is_empty()).unwrap_or(false) { " WHERE group_id = ?1" } else { "" };
        let sql = format!(
            "SELECT json_object('group_id',group_id,'requests',COUNT(*),'success',SUM(CASE WHEN status='success' THEN 1 ELSE 0 END),'input_tokens',COALESCE(SUM(input_tokens),0),'output_tokens',COALESCE(SUM(output_tokens),0),'cost_usd',COALESCE(ROUND(SUM(cost_usd),6),0),'wasted_cost_usd',COALESCE(ROUND(SUM(wasted_cost_usd),6),0),'avg_duration_ms',ROUND(AVG(CASE WHEN status='success' THEN duration_ms END)),'models',COUNT(DISTINCT exposed_model)) FROM request_logs{scope} GROUP BY group_id ORDER BY SUM(cost_usd) DESC LIMIT 200"
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

    /// Raw read-back of one stored credential. Used by the tests to prove the
    /// secret really left `config_json`, and available to any future caller that
    /// needs a single value without loading the whole config.
    pub fn secret(&self, k: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT v FROM secrets WHERE k = ?1")?;
        let row: Option<String> = stmt.query_row(params![k], |r| r.get(0)).optional()?;
        Ok(row)
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

/// Where a credential used to sit inline, the JSON now says this instead. The
/// literal is deliberately ugly so it can never be mistaken for a real key.
const SECRET_PLACEHOLDER: &str = "__stored_in_secrets__";

/// Split a config into (json with placeholders, credentials to store beside it).
///
/// A credential is identified by something that survives a reorder of the array
/// it lives in, never by its position:
///   - a channel, by its own id          -> `channel_key:<id>`
///   - a gateway API key, by the group it serves plus its label
///                                       -> `api_key:<group_id>:<name>`
///
/// `ApiKey` has no id field and the console is what creates those rows, so the
/// `api_key:<api_keys[i].id>` shape named in the task is not reachable without
/// a client change; group + name is the identity the console itself shows the
/// operator. Two API keys sharing BOTH group and name are the one pair this
/// cannot tell apart: they collapse onto a single secret row and both
/// placeholders restore to the first one's value. PUT /admin/api/config accepts
/// such a pair today, so the limit is written down here instead of being
/// papered over with new validation that nothing asked for.
///
/// An EMPTY credential is left exactly as it was: there is nothing to hide, and
/// a placeholder for it would turn "this channel has no key" into "this channel
/// has a key that went missing".
fn split_secrets(cfg: &Config) -> anyhow::Result<(String, Vec<(String, String)>)> {
    let mut value = serde_json::to_value(cfg)?;
    let mut secrets: Vec<(String, String)> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    if let Some(channels) = value.get_mut("channels").and_then(|v| v.as_array_mut()) {
        for ch in channels.iter_mut() {
            let Some(obj) = ch.as_object_mut() else { continue };
            let id = obj.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let key = obj.get("api_key").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if id.is_empty() || key.is_empty() || key == SECRET_PLACEHOLDER {
                continue;
            }
            let k = format!("channel_key:{id}");
            if seen.insert(k.clone()) {
                secrets.push((k, key));
            }
            obj.insert("api_key".to_string(), serde_json::Value::String(SECRET_PLACEHOLDER.to_string()));
        }
    }

    if let Some(keys) = value.get_mut("api_keys").and_then(|v| v.as_array_mut()) {
        for entry in keys.iter_mut() {
            let Some(obj) = entry.as_object_mut() else { continue };
            let group = obj.get("group_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let key = obj.get("key").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if group.is_empty() || key.is_empty() || key == SECRET_PLACEHOLDER {
                continue;
            }
            let k = format!("api_key:{group}:{name}");
            if seen.insert(k.clone()) {
                secrets.push((k, key));
            }
            obj.insert("key".to_string(), serde_json::Value::String(SECRET_PLACEHOLDER.to_string()));
        }
    }

    Ok((serde_json::to_string(&value)?, secrets))
}

/// Put the real credentials back into a config that came off disk.
///
/// Only the exact placeholder is replaced. Anything else — a hand-written
/// config, a row written before the split existed, or a key the operator
/// literally typed as the placeholder — is left untouched, so an old database
/// loads byte-for-byte as before. A placeholder whose secret row is missing is
/// also left alone: a half-written pair is impossible (both halves commit in
/// one transaction), and inventing an empty key would silently break upstream
/// auth instead of failing loudly.
fn restore_secrets(conn: &Connection, value: &mut serde_json::Value) -> anyhow::Result<()> {
    let lookup = |k: &str| -> anyhow::Result<Option<String>> {
        Ok(conn
            .prepare("SELECT v FROM secrets WHERE k = ?1")?
            .query_row(params![k], |r| r.get(0))
            .optional()?)
    };

    if let Some(channels) = value.get_mut("channels").and_then(|v| v.as_array_mut()) {
        for ch in channels.iter_mut() {
            let Some(obj) = ch.as_object_mut() else { continue };
            if obj.get("api_key").and_then(|v| v.as_str()) != Some(SECRET_PLACEHOLDER) {
                continue;
            }
            let id = obj.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if id.is_empty() {
                continue;
            }
            if let Some(v) = lookup(&format!("channel_key:{id}"))? {
                obj.insert("api_key".to_string(), serde_json::Value::String(v));
            }
        }
    }

    if let Some(keys) = value.get_mut("api_keys").and_then(|v| v.as_array_mut()) {
        for entry in keys.iter_mut() {
            let Some(obj) = entry.as_object_mut() else { continue };
            if obj.get("key").and_then(|v| v.as_str()) != Some(SECRET_PLACEHOLDER) {
                continue;
            }
            let group = obj.get("group_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if group.is_empty() {
                continue;
            }
            if let Some(v) = lookup(&format!("api_key:{group}:{name}"))? {
                obj.insert("key".to_string(), serde_json::Value::String(v));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ApiKey, Channel, Protocol};

    fn ts() -> String { "2026-01-01T00:00:00+00:00".to_string() }

    fn req_log(group: &str, channel: &str, model: &str, cost: f64, wasted: f64) -> RequestLog {
        RequestLog {
            id: None,
            ts: ts(),
            request_id: format!("r-{group}-{channel}"),
            group_id: group.to_string(),
            exposed_model: "exp".to_string(),
            entry_protocol: "openai".to_string(),
            status: "success".to_string(),
            final_channel: Some(channel.to_string()),
            final_upstream_model: Some(model.to_string()),
            attempts: 1,
            loop_retries: 0,
            loop_truncated: 0,
            loop_continues: 0,
            error_class: String::new(),
            error_message: String::new(),
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            cost_usd: cost,
            wasted_cost_usd: wasted,
            duration_ms: 12,
            ttft_ms: 3,
        }
    }

    fn channel(id: &str, key: &str) -> Channel {
        Channel {
            id: id.to_string(),
            name: id.to_string(),
            base_url: "http://127.0.0.1:1".to_string(),
            protocol: Protocol::Openai,
            api_key: key.to_string(),
            enabled: true,
            weight: 1,
            headers: Default::default(),
        }
    }

    fn raw_json(st: &Store) -> String {
        st.conn
            .lock()
            .unwrap()
            .query_row("SELECT json FROM config_json WHERE id = 1", [], |r| r.get(0))
            .unwrap()
    }

    /// Two groups in the log, and the scope decides how many rows come back.
    /// The whole point of the signature change: the group picker reads the
    /// unscoped overview while a selected group must see ONLY its own totals.
    #[test]
    fn stats_by_group_scopes_to_one_group() {
        let st = Store::open_memory().unwrap();
        st.insert_request_log(&req_log("g1", "c1", "m1", 1.0, 0.5)).unwrap();
        st.insert_request_log(&req_log("g2", "c2", "m2", 2.0, 0.25)).unwrap();

        let all = st.stats_by_group(None).unwrap();
        assert_eq!(all.len(), 2, "None must return every group: {all:?}");

        let only = st.stats_by_group(Some("g1")).unwrap();
        assert_eq!(only.len(), 1, "Some(g1) must return exactly one row: {only:?}");
        assert_eq!(only[0]["group_id"], "g1");
        assert_eq!(only[0]["requests"], 1);
        assert_eq!(only[0]["cost_usd"].as_f64(), Some(1.0));

        // An empty string is "no scope" (the picker sends it when nothing is
        // selected), NOT a group whose id happens to be empty.
        let blank = st.stats_by_group(Some("")).unwrap();
        assert_eq!(blank.len(), 2, "an empty group_id is the global overview: {blank:?}");

        // The other two blocks take the same scope; a fix that only touched
        // by_group would leave the summary and by_model unscoped.
        let sum = st.stats_summary(Some("g1")).unwrap();
        assert_eq!(sum["requests"], 1);
        assert_eq!(sum["cost_usd"].as_f64(), Some(1.0));
        let by_model = st.stats_by_model(Some("g2")).unwrap();
        assert_eq!(by_model.len(), 1);
        assert_eq!(by_model[0]["channel_id"], "c2");
    }

    /// The credential must leave config_json and live in the secrets table.
    #[test]
    fn credentials_are_split_out_of_config_json() {
        let st = Store::open_memory().unwrap();
        let cfg = Config {
            channels: vec![channel("c1", "sk-channel-secret")],
            api_keys: vec![ApiKey {
                key: "gw-gateway-secret".to_string(),
                group_id: "g1".to_string(),
                name: "main".to_string(),
                enabled: true,
            }],
            ..Config::default()
        };
        st.save_config(&cfg).unwrap();

        let raw = raw_json(&st);
        assert!(raw.contains(SECRET_PLACEHOLDER), "config_json must carry the placeholder: {raw}");
        assert!(!raw.contains("sk-channel-secret"), "the channel key is still plaintext in config_json: {raw}");
        assert!(!raw.contains("gw-gateway-secret"), "the API key is still plaintext in config_json: {raw}");

        assert_eq!(st.secret("channel_key:c1").unwrap().as_deref(), Some("sk-channel-secret"));
        assert_eq!(st.secret("api_key:g1:main").unwrap().as_deref(), Some("gw-gateway-secret"));

        // ...and the gateway still gets the REAL bytes back, or every upstream
        // call would start failing with 401 on the next restart.
        let back = st.load_config().unwrap().unwrap();
        assert_eq!(back.channels[0].api_key, "sk-channel-secret");
        assert_eq!(back.api_keys[0].key, "gw-gateway-secret");
        assert_eq!(
            serde_json::to_string(&back).unwrap(),
            serde_json::to_string(&cfg).unwrap(),
            "save then load must round-trip byte-for-byte"
        );
    }

    /// A rotated key replaces its predecessor, and a deleted channel takes its
    /// credential with it instead of leaving an orphan row behind.
    #[test]
    fn secrets_follow_their_owner_across_saves() {
        let st = Store::open_memory().unwrap();
        let one = Config { channels: vec![channel("c1", "sk-first")], ..Config::default() };
        st.save_config(&one).unwrap();
        assert_eq!(st.secret("channel_key:c1").unwrap().as_deref(), Some("sk-first"));

        let rotated = Config { channels: vec![channel("c1", "sk-second")], ..Config::default() };
        st.save_config(&rotated).unwrap();
        assert_eq!(st.secret("channel_key:c1").unwrap().as_deref(), Some("sk-second"));
        assert_eq!(st.load_config().unwrap().unwrap().channels[0].api_key, "sk-second");

        let gone = Config { channels: vec![], ..Config::default() };
        st.save_config(&gone).unwrap();
        assert_eq!(st.secret("channel_key:c1").unwrap(), None, "a deleted channel must not keep its key");

        // An empty credential means "no key yet" and is deliberately left
        // inline: turning it into a placeholder would make load look for a
        // secret that was never saved and silently blank the field.
        let blank = Config { channels: vec![channel("c2", "")], ..Config::default() };
        st.save_config(&blank).unwrap();
        assert_eq!(st.secret("channel_key:c2").unwrap(), None);
        assert_eq!(st.load_config().unwrap().unwrap().channels[0].api_key, "");
    }

    /// A database written by an older build has the keys inline, and there is
    /// no secret row for them. Load must copy those through untouched.
    #[test]
    fn a_config_that_predates_the_split_still_loads() {
        let st = Store::open_memory().unwrap();
        let legacy = r#"{"channels":[{"id":"c9","name":"legacy","base_url":"http://127.0.0.1:9","protocol":"openai","api_key":"sk-legacy-inline","enabled":true,"weight":1}],"api_keys":[]}"#;
        {
            let conn = st.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO config_json (id, json, updated_at) VALUES (1, ?1, ?2)",
                params![legacy, ts()],
            )
            .unwrap();
        }
        let got = st.load_config().unwrap().unwrap();
        assert_eq!(got.channels[0].api_key, "sk-legacy-inline", "an inline key must survive verbatim");

        // The mirror case: a placeholder whose secret row is missing (both
        // halves commit together, so this only happens in a hand-edited file).
        // Leaving it alone fails loudly later; replacing it with "" would look
        // like a working channel that authenticates as nobody.
        let orphan = format!(
            r#"{{"channels":[{{"id":"c9","name":"legacy","base_url":"http://127.0.0.1:9","protocol":"openai","api_key":"{SECRET_PLACEHOLDER}","enabled":true,"weight":1}}],"api_keys":[]}}"#
        );
        {
            let conn = st.conn.lock().unwrap();
            conn.execute("UPDATE config_json SET json = ?1 WHERE id = 1", params![orphan]).unwrap();
        }
        assert_eq!(st.load_config().unwrap().unwrap().channels[0].api_key, SECRET_PLACEHOLDER);
    }
}
