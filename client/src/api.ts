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
}
export interface AdminConfig {
  listen: ListenConfig;
  admin: AdminPlane;
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
export interface Group { id: string; name: string; exposed_models: string[]; tiers: Tier[] }
export interface ApiKey { key: string; group_id: string; name: string; enabled: boolean }

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
  summary: Record<string, number | string | null>;
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

export const api = {
  getConfig: (): Promise<AdminConfig> => req("GET", "/admin/api/config"),
  putConfig: (cfg: AdminConfig): Promise<{ ok: boolean; config_version: number }> =>
    req("PUT", "/admin/api/config", cfg),
  switchLogs: (limit = 200, requestId?: string, groupId?: string): Promise<SwitchLog[]> =>
    req("GET", `/admin/api/logs/switch?limit=${limit}` + qs({ request_id: requestId, group_id: groupId })),
  requestLogs: (limit = 200, requestId?: string, groupId?: string): Promise<RequestLog[]> =>
    req("GET", `/admin/api/logs/requests?limit=${limit}` + qs({ request_id: requestId, group_id: groupId })),
  // NOTE: the other endpoints already carry "?limit=", so qs()'s leading "&"
  // works there. /stats has no other parameter, so the "?" has to be explicit
  // here — otherwise the URL becomes "/admin/api/stats&group_id=..." → 404.
  stats: (groupId?: string): Promise<StatsPayload> =>
    req("GET", "/admin/api/stats" + (groupId ? "?group_id=" + encodeURIComponent(groupId) : "")),
  health: (): Promise<{ cooldowns: CooldownRow[] }> => req("GET", "/admin/api/health"),
  testChannel: (channelId: string, upstreamModel?: string): Promise<TestChannelResult> =>
    req("POST", "/admin/api/test-channel", { channel_id: channelId, upstream_model: upstreamModel }),
  clearCooldown: (channelId: string): Promise<{ ok: boolean }> =>
    req("POST", "/admin/api/cooldown/clear", { channel_id: channelId }),
};