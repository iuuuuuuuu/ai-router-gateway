// Shared application state: one loaded config document, shared by every page.
// Pages mutate `state.cfg` in place and call markDirty(); the shell owns saving.
import { computed, reactive } from "vue";
import { api, type AdminConfig, type Channel, type ModelBinding, type Group, type ApiKey } from "./api";
import { toast } from "./toast";

export const state = reactive({
  cfg: null as AdminConfig | null,
  loading: false,
  loaded: false,
  error: "" as string,
  dirty: false,
  saving: false,
  /** bumps after each successful save so pages can re-derive */
  version: 0,
  /** live gateway reachability, refreshed by the shell */
  online: false as boolean | null,
});

export function markDirty() { state.dirty = true }

export async function loadConfig(force = false) {
  if (state.loading) return;
  if (state.loaded && !force) return;
  state.loading = true;
  state.error = "";
  try {
    const cfg = await api.getConfig();
    // A brand-new database returns "{}" — normalize to a usable skeleton.
    state.cfg = normalize(cfg);
    state.loaded = true;
    state.dirty = false;
    state.online = true;
  } catch (e: any) {
    state.error = String(e?.message || e);
    state.online = false;
  } finally {
    state.loading = false;
  }
}

export function normalize(cfg: any): AdminConfig {
  const c: AdminConfig = {
    listen: cfg?.listen ?? { http: "0.0.0.0:8080", https: "0.0.0.0:8443", https_enabled: true },
    admin: cfg?.admin ?? { bind: "127.0.0.1:9090", key: null },
    // 默认关闭：同级按列表顺序依次尝试（与 server/src/config.rs 的 d_auto_weight 一致）。
    auto_weight: cfg?.auto_weight ?? false,
    // Mirrors server/src/config.rs d_* defaults so a fresh DB looks sane.
    retry: cfg?.retry ?? {
      max_attempts: 6, max_cycles: 2, total_deadline_ms: 180000, attempt_deadline_ms: 25000,
      connect_timeout_ms: 5000, ttfb_timeout_ms: 15000, first_event_timeout_ms: 8000,
      chunk_idle_timeout_ms: 15000, reserve_ms: 3000, cooldown_wait: true,
    },
    channels: cfg?.channels ?? [],
    models: cfg?.models ?? [],
    groups: cfg?.groups ?? [],
    api_keys: cfg?.api_keys ?? [],
  };
  return c;
}

export async function saveConfig(): Promise<boolean> {
  if (!state.cfg) return false;
  state.saving = true;
  try {
    const res = await api.putConfig(JSON.parse(JSON.stringify(state.cfg)));
    state.dirty = false;
    state.version++;
    toast.ok(`配置已保存（版本 ${res?.config_version ?? "?"}）`);
    return true;
  } catch (e: any) {
    toast.err("保存失败：" + String(e?.message || e));
    return false;
  } finally {
    state.saving = false;
  }
}

export function revert() {
  state.dirty = false;
  state.loaded = false;
  void loadConfig(true);
}

/* ------------------------------- helpers -------------------------------- */

export const channels = computed<Channel[]>(() => state.cfg?.channels ?? []);
export const models = computed<ModelBinding[]>(() => state.cfg?.models ?? []);
export const groups = computed<Group[]>(() => state.cfg?.groups ?? []);
export const apiKeys = computed<ApiKey[]>(() => state.cfg?.api_keys ?? []);

export function channelName(id: string): string {
  return channels.value.find(c => c.id === id)?.name || id || "—";
}
export function modelLabel(id: string): string {
  const m = models.value.find(x => x.id === id);
  return m ? (m.exposed || m.upstream || m.id) : id;
}

/** "渠道-模型" label: the routing identity the user actually reasons about.
 *  Falls back to the model name when the binding (or its channel) is gone. */
export function bindingLabel(modelId: string): string {
  const m = models.value.find(x => x.id === modelId);
  if (!m) return modelId;
  const ch = channels.value.find(c => c.id === m.channel_id);
  const model = m.exposed || m.upstream || m.id;
  return ch ? `${ch.name}-${model}` : model;
}

/** Model bindings attached to one channel — the "渠道下面挂着可用模型" list. */
export function modelsOfChannel(channelId: string): ModelBinding[] {
  return models.value.filter(m => m.channel_id === channelId);
}

/** Groups that reference a model binding at any tier level. */
export function groupsUsingModel(modelId: string): Group[] {
  return groups.value.filter(g => g.tiers.some(t => t.members.some(mm => mm.model_id === modelId)));
}

/** uid generator matching the id style used elsewhere in the config. */
export function uid(prefix: string): string {
  const rnd = Math.random().toString(36).slice(2, 8);
  return `${prefix}_${Date.now().toString(36)}${rnd}`;
}