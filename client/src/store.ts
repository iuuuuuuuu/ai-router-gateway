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

/**
 * 保存前的本地自检：把 server/src/admin.rs:56-70 那两条引用完整性规则在本地先跑一遍。
 *
 * ★ 为什么值得单独做（2026-10-14 修「我保存失败了」）：
 *   服务端 PUT /admin/api/config 是「先校验、后整体替换」——只要有一处悬空引用，
 *   整份配置 400 拒收、一个字节都不落库；用户看到的就只有一行机器味的
 *   `group grp_pro tier 1 references missing model mdl_pro_real`，既不知道是哪张卡弄的，
 *   也不知道该去哪儿修。这里就地翻成人话，并直接指出该去哪里处理。
 *   （删除路径已经在 Channels.vue 的 dropGroupRefs() 里清理引用；这里是第二道兜底。）
 */
function configProblems(cfg: AdminConfig): string[] {
  const out: string[] = [];
  const chIds = new Set((cfg.channels ?? []).map(c => c.id));
  const mIds = new Set((cfg.models ?? []).map(m => m.id));
  for (const m of cfg.models ?? []) {
    if (!chIds.has(m.channel_id)) {
      out.push(`模型绑定「${m.exposed || m.id}」挂在已不存在的渠道 ${m.channel_id} 上`);
    }
  }
  for (const g of cfg.groups ?? []) {
    for (const t of g.tiers ?? []) {
      for (const mem of t.members ?? []) {
        if (!mIds.has(mem.model_id)) {
          out.push(`分组「${g.name || g.id}」第 ${t.level} 级引用了已删除的模型绑定 ${mem.model_id}`);
        }
      }
    }
  }
  return out;
}

export async function saveConfig(): Promise<boolean> {
  if (!state.cfg) return false;
  // ★ 本地先拦一道：悬空引用不是「保存失败」而是「配置本身不成立」，
  //   说清数量与前几条，并告诉用户去哪儿处理，而不是把服务端 raw 400 甩出去。
  const problems = configProblems(state.cfg);
  if (problems.length) {
    toast.err(
      `保存失败：配置里还有 ${problems.length} 处失效引用 —— ${problems.slice(0, 3).join("；")}` +
      `${problems.length > 3 ? " 等" : ""}。请到「渠道与模型」页删除对应绑定，或到分组详情页把该成员移除后重试。`,
    );
    return false;
  }
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