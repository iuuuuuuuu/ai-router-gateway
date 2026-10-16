// Admin-plane API wrapper. Admin plane listens on 127.0.0.1:9090 by default.
export interface ListenConfig { http: string; https: string; https_enabled: boolean }
export interface AdminPlane { bind: string; key: string | null }
export interface RetryConfig {
  max_attempts: number;
  max_cycles: number;
  total_deadline_ms: number;
  attempt_deadline_ms: number;
  connect_timeout_ms: number;
  ttfb_timeout_ms: number;
  first_event_timeout_ms: number;
  chunk_idle_timeout_ms: number;
  /** 预留余量：max_attempts × attempt_deadline_ms + reserve_ms ≤ total_deadline_ms */
  reserve_ms: number;
  cooldown_wait: boolean;
  /** 死循环拦截总开关。默认 true。 */
  loop_guard_enabled: boolean;
  /** 提交前至少累积多少字符才开始做退化检测；同时就是首字节推迟量（默认 64）。 */
  loop_guard_min_chars: number;
  /** 被重复的单元至少多长才算退化（默认 24）。 */
  loop_guard_repeat_min_chars: number;
  /** 重复部分占检测窗口的比例阈值，0~1（默认 0.75）。 */
  loop_guard_repeat_ratio: number;
  /** 同一单元重复达到多少次判定为退化（默认 4）。 */
  loop_guard_max_repeats: number;
  /** 检测窗口字符数上限，超出只保留尾部（默认 4000）。 */
  loop_guard_window_chars: number;
  /** 已提交之后的判定只看回答末尾这么多字符（默认 128）。提交后无法重试，
   *  只能截断收尾，所以这个值同时决定「中段复读还能泄漏多少字」。 */
  loop_guard_tail_chars: number;
  /** 命中退化时**纠正续写**而不是丢弃整段尝试（默认 true）。
   *  关闭后退回旧行为：丢弃该次尝试、换候选重试。 */
  loop_guard_continue_enabled: boolean;
  /** 一条回答最多纠正几轮（默认 2）。0 = 从不纠正。
   *  每轮纠正是一次额外的上游调用，所以服务端会把它夹到一个小范围。 */
  loop_guard_continue_max: number;
  /** 纠正续写时追加的提示词。空串表示用服务端内置的中文提示词。 */
  loop_guard_continue_prompt: string;
  /** 思考流是否也做复读判定（默认 true）。思考不是回答正文，但模型在思考里
   *  打转时永远走不到回答，而且思考会无上限地刷给客户端。判定不占首字节：
   *  提交前的思考本来就要缓存（重试不能把已发出的思考再放一遍）。 */
  loop_guard_reasoning_enabled: boolean;
  /** 提交前等待的上限（毫秒，默认 250）。`loop_guard_min_chars` 是字符数，
   *  要等多久完全取决于模型出字速度：400 字符/秒 时 64 字是 160ms，
   *  107 字符/秒 时是 600ms，2 字符/秒 时是 32 秒。这个上限把首字延迟
   *  锁在时间上，而不是锁在生成速度上。慢模型到点就先提交，后续内容走
   *  提交后判定（仍有第二层兜底）。0 表示不设上限。 */
  loop_guard_hold_max_ms: number;
}
export interface AdminConfig {
  listen: ListenConfig;
  admin: AdminPlane;
  /** 同级候选的分配方式：false（默认）= 按列表顺序依次尝试；true = 按权重降序。 */
  auto_weight: boolean;
  retry: RetryConfig;
  channels: Channel[];
  models: ModelBinding[];
  groups: Group[];
  api_keys: ApiKey[];
  [k: string]: unknown;
}
export interface Channel {
  id: string; name: string; base_url: string;
  protocol: "openai" | "anthropic" | "gemini";
  api_key: string; enabled: boolean; weight: number;
  headers: Record<string, string>;
}
export interface ModelBinding {
  id: string; channel_id: string; exposed: string; upstream: string;
  /** [input, output, cache_read, cache_creation] USD per 1M tokens */
  price_per_m: [number, number, number, number]; enabled: boolean;
}
export interface TierMember { model_id: string; weight: number }
export interface Tier { level: number; members: TierMember[] }
// exposed_models 是**服务端按成员自动派生**的只读结果（不是可填字段）：
// server/src/config.rs 的 Config::normalize_exposed_models 在每次启动与每次 PUT 时重算，
// 控制台只显示。标成可选是为了容忍「更老的服务端 / 还没有成员的组」这两种回包，
// 读取端一律用 `g.exposed_models ?? []`。
export interface Group { id: string; name: string; exposed_models?: string[]; tiers: Tier[]; overrides?: GroupOverrides }
export interface ApiKey { key: string; group_id: string; name: string; enabled: boolean }

export interface GroupOverrides { retry?: RetryConfig | null }
export interface GroupMember {
  /** "<channel_id>::<upstream_model>" — stable row id used by nav.modelKey. */
  key: string; model_id: string; tier: number; slot: number; weight: number;
  channel_id: string; channel_name: string; upstream_model: string; exposed_model: string;
  enabled: boolean; cooling: boolean; cooling_until_ms: number | null; cooldown_kind: string | null;
  requests: number; failed_attempts: number; loop_retries: number;
  /** 该候选在自己的思考里打转、被网关掐断继续转发思考的次数。
   *  与 loop_retries 分开：那是回答复读，这是思考复读，是两种不同的问题。 */
  reasoning_truncated?: number;
  /** 该候选提交后被截断的次数。 */
  loop_truncated?: number;
  error_classes: { error_class: string; n: number }[];
  avg_duration_ms: number | null; avg_ttft_ms: number | null;
  input_tokens: number; output_tokens: number; cost_usd: number;
  last_used_ts: string | null; last_outcome: string | null; last_error_class: string | null;
  /** 最近一次成功请求的落点（last-success，不是 in-flight）。 */
  active: boolean;
  /** 此刻正在使用该 (渠道, 上游模型) 的并发请求数；0 = 空闲。 */
  inflight: number;
}
export interface GroupMembersPayload {
  group_id: string; group_name: string; active_key: string | null;
  /** 该分组此刻正在处理中的请求总数。 */
  inflight_total: number;
  /** 该分组的有效重试配置（跟随全局或分组自己的覆盖）。 */
  retry: RetryConfig;
  /** "group" = 用了分组自己的配置；"global" = 跟随全局。 */
  retry_source: "group" | "global";
  /** 全局那份，便于「跟随全局」时预览。 */
  global_retry: RetryConfig;
  members: GroupMember[];
}

export interface SwitchLog {
  ts: string; request_id: string; group_id: string; attempt_no: number; tier: number;
  channel_id: string; channel_name: string; upstream_model: string;
  outcome: string; error_class: string; http_status: number | null;
  latency_ms: number; message: string;
}
export interface RequestLog {
  ts: string; request_id: string; group_id: string; exposed_model: string;
  status: string; final_channel: string | null; final_upstream_model: string | null;
  attempts: number; input_tokens: number; output_tokens: number;
  cache_read_tokens: number; cache_creation_tokens: number;
  cost_usd: number; wasted_cost_usd: number; duration_ms: number; ttft_ms: number;
  /** 本次请求因输出退化被内部丢弃并重试的次数（提交前拦截，客户端零字节可见）。 */
  loop_retries: number;
  /** 1 = 回答已开始发送后才判定为退化，网关就地截断并正常收尾。
   *  与 loop_retries 不同：这条回答客户端已经收到了，只是没给完。 */
  loop_truncated: number;
  /** 就地纠正续写的轮数（回答已提交后命中退化，网关请模型接着写）。
   *  与 loop_truncated 互斥：纠正成功就不截断。 */
  loop_continues: number;
  /** 失败时的错误分类（成功为空串）。与 error_message 配对展示。 */
  error_class: string;
  /** 失败原因的可读描述（成功为空串）。 */
  error_message: string;
  /** 1 = 模型在自己的思考里打转，网关停止继续转发思考。
   *  与 loop_truncated 分开记：思考打转不代表回答被截断，回答可能好得很。 */
  reasoning_truncated: number;
}
/** 实时并发快照："使用中" 的唯一数据来源。只含非零项，缺键即 0。 */
export interface InflightSnapshot {
  total: number;
  /** "<channel_id>::<upstream_model>" → 并发数。 */
  members: Record<string, number>;
  /** channel_id → 该渠道所有模型的并发数之和。 */
  channels: Record<string, number>;
  /** group_id → 该分组的并发数之和。 */
  groups: Record<string, number>;
}
/** 清空日志与统计。config 永不被触碰。 */
export interface PurgeLogsRequest { request_logs?: boolean; switch_logs?: boolean; cooldowns?: boolean }
export interface PurgeLogsResult {
  ok: boolean;
  deleted: { request_logs: number; switch_logs: number; cooldowns: number };
}
export interface CooldownRow {
  channel_id: string; upstream_model: string; kind: string; cooling_until_ms: number;
}
export interface GroupStatRow {
  group_id: string; requests: number; success: number;
  input_tokens: number; output_tokens: number;
  cost_usd: number; wasted_cost_usd: number;
  avg_duration_ms: number | null; models: number;
}
export interface StatsPayload {
  /** 汇总计数；loop_retries 为死循环拦截（输出退化）累计次数，
   *  loop_truncated 为提交后截断累计次数（客户端已收到部分内容）。 */
  summary: Record<string, number | string | null> & { loop_retries?: number; loop_truncated?: number };
  by_model: Record<string, number | string | null>[];
  /** Always the full cross-group overview, independent of group_id. */
  by_group: GroupStatRow[];
  /** Echoes the group the summary/by_model were scoped to (null = all). */
  group_id: string | null;
  health: CooldownRow[];
}
export interface TestChannelResult {
  ok: boolean; status?: number; latency_ms: number; sample?: string; error?: string;
}
/** Draft overrides used when probing a channel that has not been saved yet. */
export interface ChannelModelsDraft {
  base_url?: string;
  api_key?: string;
  protocol?: string;
  headers?: Record<string, string>;
}
/**
 * Result of POST /admin/api/channel-models. The endpoint always answers HTTP
 * 200, so success/failure has to be read from `ok` — never from the transport.
 * On failure `models` is empty and `error` carries the upstream message.
 */
export interface ChannelModelsResult {
  ok: boolean;
  status: number;
  latency_ms: number;
  models: string[];
  count: number;
  error?: string;
}

const BASE = localStorage.getItem("admin_base") || "http://127.0.0.1:9090";

export function getBase() { return localStorage.getItem("admin_base") || "http://127.0.0.1:9090" }
export function setBase(v: string) {
  if (v) localStorage.setItem("admin_base", v); else localStorage.removeItem("admin_base");
}
export function getKey() { return localStorage.getItem("admin_key") || "" }
export function setKey(v: string) {
  if (v) localStorage.setItem("admin_key", v); else localStorage.removeItem("admin_key");
}

/** Builds "&a=1&b=2" skipping empty values, so callers can pass undefined freely. */
function qs(params: Record<string, string | undefined>): string {
  return Object.entries(params)
    .filter(([, v]) => v !== undefined && v !== "")
    .map(([k, v]) => `&${k}=${encodeURIComponent(String(v))}`)
    .join("");
}

async function req(method: string, path: string, body?: unknown): Promise<any> {
  const key = localStorage.getItem("admin_key") || "";
  const headers: Record<string, string> = { "Content-Type": "application/json" };
  if (key) headers["Authorization"] = "Bearer " + key;
  const r = await fetch(getBase() + path, {
    method, headers, body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await r.text();
  let parsed: any = null;
  try { parsed = text ? JSON.parse(text) : null; } catch { parsed = text }
  if (!r.ok) {
    let msg = r.status + " " + r.statusText;
    if (parsed && typeof parsed === "object") {
      if (parsed.message) msg = String(parsed.message);
      else if (parsed.error) msg = String(parsed.error);
    } else if (typeof parsed === "string" && parsed) msg = parsed;
    throw new Error(msg);
  }
  return parsed;
}

/**
 * One page of log rows plus the total row count the same filter matches.
 * `total` counts rows IGNORING the page window, so the console can render
 * "第 X / Y 页" without a second round trip. Server contract: /admin/api/logs/switch
 * and /admin/api/logs/requests both answer this exact shape.
 */
export interface LogPage<T> {
  rows: T[];
  total: number;
  limit: number;
  offset: number;
}

export const api = {
  getConfig: (): Promise<AdminConfig> => req("GET", "/admin/api/config"),
  putConfig: (cfg: AdminConfig): Promise<{ ok: boolean; config_version: number }> =>
    req("PUT", "/admin/api/config", cfg),
  // Log rows always come back paged: { rows, total, limit, offset }. Every caller
  // (Logs.vue page buttons, GroupDetail.vue and ModelDetail.vue "newest N" lists)
  // goes through these two; there is no array-only variant left to keep in sync.
  switchLogsPage: (limit = 200, offset = 0, requestId?: string, groupId?: string): Promise<LogPage<SwitchLog>> =>
    req("GET", `/admin/api/logs/switch?limit=${limit}&offset=${offset}` + qs({ request_id: requestId, group_id: groupId })),
  requestLogsPage: (limit = 200, offset = 0, requestId?: string, groupId?: string): Promise<LogPage<RequestLog>> =>
    req("GET", `/admin/api/logs/requests?limit=${limit}&offset=${offset}` + qs({ request_id: requestId, group_id: groupId })),
  // NOTE: the other endpoints already carry "?limit=", so qs()'s leading "&"
  // works there. /stats has no other parameter, so the "?" has to be explicit
  // here — otherwise the URL becomes "/admin/api/stats&group_id=..." → 404.
  stats: (groupId?: string): Promise<StatsPayload> =>
    req("GET", "/admin/api/stats" + (groupId ? "?group_id=" + encodeURIComponent(groupId) : "")),
  // Same trap as /stats above: this endpoint has no other parameter, so the
  // "?" is explicit here — qs() returns a leading "&".
  groupMembers: (groupId: string): Promise<GroupMembersPayload> =>
    req("GET", "/admin/api/group-members?group_id=" + encodeURIComponent(groupId)),
  health: (): Promise<{ cooldowns: CooldownRow[] }> => req("GET", "/admin/api/health"),
  testChannel: (channelId: string, upstreamModel?: string): Promise<TestChannelResult> =>
    req("POST", "/admin/api/test-channel", { channel_id: channelId, upstream_model: upstreamModel }),
  clearCooldown: (channelId: string): Promise<{ ok: boolean }> =>
    req("POST", "/admin/api/cooldown/clear", { channel_id: channelId }),
  // Probe a channel's upstream /v1/models. `channelId` may be empty when the
  // caller supplies a full draft (base_url + api_key); the server then uses the
  // draft alone instead of looking the channel up in the config.
  channelModels: (channelId: string, draft?: ChannelModelsDraft): Promise<ChannelModelsResult> =>
    req("POST", "/admin/api/channel-models", { channel_id: channelId || undefined, ...(draft || {}) }),
  // Live concurrency snapshot. Unlike /group-members this endpoint has no other
  // parameter, so no "?" juggling is needed — qs() is not used here at all.
  inflight: (): Promise<InflightSnapshot> => req("GET", "/admin/api/inflight"),
  // Destructive: deletes log/stat rows only. The saved config is untouched, so
  // channels/models/groups/keys survive and routing keeps working.
  purgeLogs: (what: PurgeLogsRequest): Promise<PurgeLogsResult> =>
    req("POST", "/admin/api/purge-logs", what),
};