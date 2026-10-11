<script setup lang="ts">
// 分组详情（整页版）：分组概览 + 该分组的用量统计 + 请求日志 + 切换日志 + 冷却状态。
// 进入页面时用 Promise.all 一次性并发拉全四份数据，页签切换只切本地视图、不再发请求；
// 「自动刷新 5s」开关控制 5s 轮询，离开页面（组件卸载）时定时器一并清掉。
// 页面由 App.vue 按 nav.groupId 挂载，group 一定非空；Esc 等价于点顶栏「返回」。
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import {
  Activity, Coins, KeyRound, RefreshCw, ScrollText, ShieldAlert, Waypoints,
} from "@lucide/vue";
import {
  api, type CooldownRow as ApiCooldownRow, type Group, type RequestLog, type StatsPayload, type SwitchLog,
} from "../api";
import { apiKeys, bindingLabel, channelName, models } from "../store";
import { closeGroupDetail } from "../nav";
import { Alert, Badge, Btn, Card, Empty, PageHeader, Spinner, TToggle } from "../components/ui";

type Tone = "gray" | "teal" | "ok" | "warn" | "err" | "brand";
type Row = Record<string, number | string | null>;
type TabKey = "overview" | "usage" | "request" | "switch" | "cooldown";
// 冷却行：cooling_until_ms 是绝对 epoch 毫秒（permanent 时为 0），remaining_ms 是服务端算好的
// 快照剩余毫秒，permanent 表示认证失败导致的永久隔离（人工解除前不会自动恢复）。
type CooldownRow = ApiCooldownRow & {
  permanent?: boolean; remaining_ms?: number; consecutive_failures?: number;
};

const props = defineProps<{ group: Group }>();

// 整页有空间，日志一次多拉一些（服务端上限由路由决定，多拿不会报错）。
const LIMIT = 200;
const TIER_NAMES = ["一级", "二级", "三级", "四级", "五级", "六级"];

/* ------------------------------ 格式化兜底 ------------------------------ */
// 与 Logs.vue / Stats.vue 保持同一份实现（两页各自复制了一份，这里照此复制）。
function num(v: unknown): number | null {
  if (v === null || v === undefined || v === "") return null;
  const n = typeof v === "number" ? v : Number(v);
  return Number.isFinite(n) ? n : null;
}
function fmtInt(v: unknown): string {
  const n = num(v);
  return n === null ? "—" : String(Math.round(n));
}
function fmtMs(v: unknown): string {
  const n = num(v);
  return n === null ? "—" : `${Math.round(n)}ms`;
}
function fmtCost(v: unknown): string {
  const n = num(v);
  return n === null ? "—" : `$${n.toFixed(6)}`;
}
function fmtText(v: unknown): string {
  if (v === null || v === undefined) return "—";
  const s = String(v).trim();
  return s === "" ? "—" : s;
}
function fmtTs(iso: unknown): string {
  if (iso === null || iso === undefined || iso === "") return "—";
  const d = new Date(String(iso));
  if (Number.isNaN(d.getTime())) return String(iso);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}
function fmtClock(ms: number): string {
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

/* ------------------------------- 字段映射 ------------------------------- */
function tierName(level: number): string {
  return TIER_NAMES[level - 1] ?? `${level} 级`;
}
function tierInfo(tier: unknown): { label: string; tone: Tone } {
  const t = num(tier);
  if (t !== null && t >= 1 && t <= 6) return { label: tierName(Math.round(t)), tone: t === 1 ? "brand" : "gray" };
  return { label: t === null ? "—" : `${Math.round(t)} 级`, tone: "gray" };
}
function outcomeInfo(outcome: unknown): { label: string; tone: Tone } {
  // 后端 switch_logs.outcome 实际写的是 "committed"/"failed"（server/src/routes.rs），
  // 兼容 "success"，两者都算成功，避免真实数据把成功显示成失败。
  const s = String(outcome ?? "").toLowerCase();
  return s === "success" || s === "committed"
    ? { label: "成功", tone: "ok" }
    : { label: "失败", tone: "err" };
}
function errorClass(v: unknown): { text: string; tone: Tone } {
  const s = String(v ?? "").trim();
  const low = s.toLowerCase();
  if (!s || low === "none" || low === "null") return { text: "", tone: "warn" };
  // "committed" 是后端成功行的占位值（server/src/proxy.rs）→ 成功语义用 ok 色调。
  if (low === "committed") return { text: s, tone: "ok" };
  return { text: s, tone: "warn" };
}
function statusInfo(status: unknown): { label: string; tone: Tone } {
  switch (String(status ?? "").toLowerCase()) {
    case "success": return { label: "成功", tone: "ok" };
    case "exhausted": return { label: "全部失败", tone: "err" };
    // router::Terminal::NoCandidate：网关返回 404 no_route_for_model 的终态
    case "no_candidate": return { label: "无可用路由", tone: "warn" };
    case "cooldown": return { label: "冷却等待", tone: "warn" };
    case "config": return { label: "配置错误", tone: "err" };
    case "aborted": return { label: "客户端中断", tone: "gray" };
    default: return { label: fmtText(status), tone: "gray" };
  }
}
function chLabel(name: unknown, id: unknown): string {
  const n = String(name ?? "").trim();
  if (n) return n;
  const i = String(id ?? "").trim();
  return i ? channelName(i) : "—";
}
function finalChannel(v: unknown): string {
  const s = String(v ?? "").trim();
  return s ? channelName(s) : "—";
}
function wasted(v: unknown): boolean {
  const n = num(v);
  return n !== null && n > 0;
}
function rowKey(row: any, i: number): string {
  return `${row?.request_id ?? "r"}|${row?.attempt_no ?? row?.ts ?? ""}|${i}`;
}
function kindInfo(kind: unknown): { label: string; tone: Tone } {
  switch (String(kind ?? "").toLowerCase()) {
    case "auth": return { label: "认证失败", tone: "err" };
    case "model": return { label: "模型不可用", tone: "warn" };
    case "backoff": return { label: "退避中", tone: "gray" };
    default: return { label: fmtText(kind), tone: "gray" };
  }
}
// 服务端契约：cooling_until_ms 是绝对 epoch 毫秒（permanent 时恒为 0），remaining_ms 是
// 服务端算好的快照剩余毫秒（自己不会走秒）。所以优先按绝对时间戳实时倒计时，只有绝对时间
// 缺失 / 为 0 / 非数字时才退化为快照值。
function remainMs(row: CooldownRow): number | null {
  const abs = num(row?.cooling_until_ms);
  if (abs !== null && abs > 0) return Math.max(0, abs - now.value);
  const snap = num(row?.remaining_ms);
  if (snap !== null) return Math.max(0, snap);
  return null;
}
function isPermanent(row: CooldownRow): boolean {
  return row?.permanent === true;
}
function fmtCooldownRemain(row: CooldownRow): string {
  if (isPermanent(row)) return "永久";
  return fmtRemain(remainMs(row));
}
function failCount(row: CooldownRow): number | null {
  const n = num(row?.consecutive_failures);
  return n !== null && n > 0 ? n : null;
}
function fmtRemain(ms: number | null): string {
  if (ms === null) return "—";
  const s = Math.ceil(ms / 1000);
  if (s <= 0) return "已到期";
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m${s % 60}s`;
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return `${h}h${m}m`;
}
function healthKey(row: CooldownRow, i: number): string {
  return `${row?.channel_id ?? "c"}|${row?.upstream_model ?? ""}|${row?.kind ?? ""}|${i}`;
}
/** 密钥脱敏：只展示前 8 位，完整密钥永远不进 DOM。 */
function maskKey(v: unknown): string {
  const s = String(v ?? "");
  if (!s) return "—";
  return s.slice(0, 8) + "…";
}

/* --------------------------------- 状态 --------------------------------- */
const tab = ref<TabKey>("overview");
const autoRefresh = ref(true);
const loading = ref(false);
const err = ref("");
const lastAt = ref(0);
const now = ref(Date.now());
const stats = ref<StatsPayload | null>(null);
const switchRows = ref<SwitchLog[]>([]);
const requestRows = ref<RequestLog[]>([]);
const cooldowns = ref<CooldownRow[]>([]);

const tabs: { key: TabKey; label: string }[] = [
  { key: "overview", label: "概览" },
  { key: "usage", label: "用量" },
  { key: "request", label: "请求日志" },
  { key: "switch", label: "切换日志" },
  { key: "cooldown", label: "冷却状态" },
];

/* -------------------------------- 数据拉取 -------------------------------- */
// 进入页面一次性并发拉全：分组 scoped 的 stats + 该分组最近 LIMIT 条切换/请求日志 + 全局冷却列表。
let inflight = false;
async function load(silent = false) {
  const g = props.group;
  if (!g || inflight) return;
  inflight = true;
  if (!silent) loading.value = true;
  err.value = "";
  try {
    const [s, sw, rq, hl] = await Promise.all([
      api.stats(g.id),
      api.switchLogs(LIMIT, undefined, g.id),
      api.requestLogs(LIMIT, undefined, g.id),
      api.health(),
    ]);
    stats.value = (s ?? null) as StatsPayload | null;
    switchRows.value = Array.isArray(sw) ? sw : [];
    requestRows.value = Array.isArray(rq) ? rq : [];
    cooldowns.value = Array.isArray(hl?.cooldowns) ? hl.cooldowns : [];
    lastAt.value = Date.now();
  } catch (e: any) {
    err.value = String(e?.message || e);
  } finally {
    inflight = false;
    loading.value = false;
  }
}

function reset() {
  stats.value = null;
  switchRows.value = [];
  requestRows.value = [];
  cooldowns.value = [];
  err.value = "";
  lastAt.value = 0;
}

/* ------------------------------ 定时器 ------------------------------ */
let timer: number | undefined;
let tick: number | undefined;
function stopTimer() {
  if (timer) {
    window.clearInterval(timer);
    timer = undefined;
  }
}
function startTimer() {
  stopTimer();
  if (autoRefresh.value) timer = window.setInterval(() => { void load(true); }, 5000);
}
function stopTick() {
  if (tick) {
    window.clearInterval(tick);
    tick = undefined;
  }
}
function startTick() {
  stopTick();
  now.value = Date.now();
  tick = window.setInterval(() => { now.value = Date.now(); }, 1000);
}

watch(autoRefresh, () => { startTimer(); });

/** Esc = 返回分组路由（保留弹窗版按 Esc 关闭的习惯）。 */
function onKey(e: KeyboardEvent) {
  if (e.key === "Escape") closeGroupDetail();
}

onMounted(() => {
  void load();
  startTimer();
  startTick();
  window.addEventListener("keydown", onKey);
});

onUnmounted(() => {
  stopTimer();
  stopTick();
  window.removeEventListener("keydown", onKey);
});

/* ------------------------------ 概览数据 ------------------------------ */
const exposedModels = computed<string[]>(() => props.group?.exposed_models ?? []);
const sortedTiers = computed(() =>
  [...(props.group?.tiers ?? [])].sort((a, b) => a.level - b.level));
const tierCount = computed(() => sortedTiers.value.length);
const memberCount = computed(() =>
  sortedTiers.value.reduce((n, t) => n + (t.members?.length ?? 0), 0));
const groupKeys = computed(() => apiKeys.value.filter(k => k.group_id === props.group?.id));
const enabledKeys = computed(() => groupKeys.value.filter(k => k.enabled).length);

/** 本分组层级里引用到的渠道 id（冷却列表按它过滤）。 */
const groupChannelIds = computed<Set<string>>(() => {
  const ids = new Set<string>();
  for (const t of props.group?.tiers ?? []) {
    for (const m of t.members ?? []) {
      const b = models.value.find(x => x.id === m.model_id);
      if (b?.channel_id) ids.add(b.channel_id);
    }
  }
  return ids;
});
const scopedCooldowns = computed(() =>
  cooldowns.value.filter(r => groupChannelIds.value.has(String(r?.channel_id ?? ""))));
const otherCooldowns = computed(() => cooldowns.value.length - scopedCooldowns.value.length);

/* ------------------------------ 用量数据 ------------------------------ */
const summary = computed<Row>(() => (stats.value?.summary ?? {}) as Row);

function pick(keys: string[]): number | null {
  const s: Row = summary.value;
  for (const k of keys) {
    const v = s[k];
    if (v === undefined || v === null || v === "") continue;
    const n = num(v);
    if (n !== null) return n;
  }
  return null;
}

const requests = computed(() => pick(["requests", "total_requests", "request_count", "count", "total"]) ?? 0);
const success = computed(() => pick(["success", "succeeded", "success_count", "ok", "success_requests"]) ?? 0);
const failures = computed(() => {
  const explicit = pick(["failures", "failed", "failure", "failed_count", "errors", "error_count"]);
  if (explicit !== null) return explicit;
  const total = pick(["total_requests", "requests", "request_count", "total"]);
  if (total !== null) return Math.max(0, total - success.value);
  return null;
});
const inputTokens = computed(() => pick(["input_tokens", "prompt_tokens", "input"]) ?? 0);
const outputTokens = computed(() => pick(["output_tokens", "completion_tokens", "output"]) ?? 0);
const cacheRead = computed(() => pick(["cache_read_tokens", "cache_read"]) ?? 0);
const cacheWrite = computed(() => pick(["cache_creation_tokens", "cache_write_tokens", "cache_write"]) ?? 0);
const costTotal = computed(() => pick(["cost_usd", "total_cost_usd", "cost", "total_cost"]) ?? 0);
const wastedCost = computed(() => pick(["wasted_cost_usd", "wasted_cost"]) ?? 0);
const avgDuration = computed(() =>
  pick(["avg_duration_ms", "avg_latency_ms", "avg_ms", "average_duration_ms", "avg_duration"]));
const successRate = computed(() => (requests.value > 0 ? (success.value / requests.value) * 100 : null));
const rateTone = computed<Tone>(() => {
  const r = successRate.value;
  if (r === null) return "gray";
  if (r >= 99) return "ok";
  if (r >= 90) return "warn";
  return "err";
});

function firstNum(row: Row, keys: string[]): number | null {
  for (const k of keys) {
    const n = num(row[k]);
    if (n !== null) return n;
  }
  return null;
}
function firstText(row: Row, keys: string[]): string {
  for (const k of keys) {
    const v = row[k];
    if (v !== null && v !== undefined && String(v).trim() !== "") return String(v);
  }
  return "";
}
function modelExposed(row: Row): string {
  return fmtText(firstText(row, ["exposed_model", "model_name", "exposed"]));
}
function modelChannel(row: Row): string {
  const id = firstText(row, ["channel_id", "final_channel"]);
  return id && id !== "-" ? channelName(id) : "—";
}
function modelUpstream(row: Row): string {
  return fmtText(firstText(row, ["upstream_model", "model", "upstream", "upstream_name"]));
}
function modelCost(row: Row): number {
  return firstNum(row, ["cost_usd", "cost", "total_cost_usd", "total_cost"]) ?? 0;
}
// 后端 stats_by_model（server/src/store.rs）已按成本降序返回，这里再兜一次排序保证口径。
const byModel = computed<Row[]>(() => {
  const rows = Array.isArray(stats.value?.by_model) ? (stats.value!.by_model as Row[]) : [];
  return rows.slice().sort((a, b) => modelCost(b) - modelCost(a));
});
function sharePct(row: Row): number {
  const total = costTotal.value;
  const c = modelCost(row);
  if (!(total > 0) || !(c > 0)) return 0;
  return Math.min(100, Math.max(0, (c / total) * 100));
}
function shareText(row: Row): string {
  const total = costTotal.value;
  const c = modelCost(row);
  if (!(total > 0)) return "—";
  return `${((c / total) * 100).toFixed(1)}%`;
}
</script>

<template>
  <div class="px-5 py-5 space-y-3.5" data-test="group-dashboard">
    <!-- ------------------------------ 页头 ------------------------------ -->
    <PageHeader
      :title="'分组详情 · ' + (group.name || group.id)"
      :subtitle="`${group.id} · ${tierCount} 个层级 / 共 ${memberCount} 个成员 · ${exposedModels.length} 个暴露模型`"
    >
      <template #actions>
        <Btn size="sm" title="立即刷新" data-test="group-dashboard-refresh" @click="load()">
          <RefreshCw :size="13" :class="loading ? 'animate-spin' : ''" />
          刷新
        </Btn>
        <TToggle v-model="autoRefresh" label="自动刷新 5s" />
      </template>
    </PageHeader>

    <!-- ------------------------------ 吸顶页签条 ------------------------------ -->
    <div class="sticky top-0 z-10 -mx-5 mb-3.5 flex flex-wrap items-center gap-2 border-b border-line bg-app px-5">
      <button
        v-for="t in tabs" :key="t.key" type="button"
        :data-test="'group-dashboard-tab-' + t.key"
        class="-mb-px border-b-2 px-3 py-2 text-[13px] transition-colors"
        :class="tab === t.key
          ? 'border-brand-500 font-medium text-ink'
          : 'border-transparent text-ink-3 hover:text-ink-2'"
        @click="tab = t.key"
      >{{ t.label }}</button>
      <div class="ml-auto text-[12px] text-ink-3">
        请求日志 {{ requestRows.length }} 条 · 切换日志 {{ switchRows.length }} 条
        <span v-if="lastAt" class="ml-1 font-mono text-[11.5px]">· 最后更新 {{ fmtClock(lastAt) }}</span>
      </div>
    </div>

    <Alert v-if="err" tone="err">加载失败：{{ err }}</Alert>

    <div v-if="loading && !stats" class="flex items-center justify-center gap-2 py-14 text-[13px] text-ink-3">
      <Spinner :size="16" /> 正在加载…
    </div>

    <template v-else>
      <!-- ------------------------------ 概览 ------------------------------ -->
      <div v-if="tab === 'overview'" class="space-y-3.5">
        <Card :pad="false" class="overflow-hidden">
          <div class="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
            <Activity :size="15" class="text-ink-3" />
            <h2 class="text-[13.5px] font-semibold text-ink">分组概览</h2>
            <span class="ml-auto text-[12px] text-ink-3">
              {{ tierCount }} 个层级 · {{ memberCount }} 个成员 · {{ exposedModels.length }} 个暴露模型
            </span>
          </div>
          <dl class="grid grid-cols-1 gap-x-6 gap-y-2.5 px-4 py-3.5 sm:grid-cols-2">
            <div class="flex gap-2">
              <dt class="w-[68px] shrink-0 text-[11.5px] leading-5 text-ink-3">分组 ID</dt>
              <dd class="min-w-0 flex-1 truncate font-mono text-[12px] leading-5 text-ink-2">{{ group.id }}</dd>
            </div>
            <div class="flex gap-2">
              <dt class="w-[68px] shrink-0 text-[11.5px] leading-5 text-ink-3">分组名</dt>
              <dd class="min-w-0 flex-1 truncate text-[12.5px] leading-5 text-ink">{{ fmtText(group.name) }}</dd>
            </div>
            <div class="flex gap-2">
              <dt class="w-[68px] shrink-0 text-[11.5px] leading-5 text-ink-3">路由层级</dt>
              <dd class="min-w-0 flex-1 text-[12.5px] leading-5 text-ink">
                {{ tierCount }} 个层级 / 共 {{ memberCount }} 个成员
              </dd>
            </div>
            <div class="flex gap-2">
              <dt class="w-[68px] shrink-0 text-[11.5px] leading-5 text-ink-3">密钥</dt>
              <dd class="min-w-0 flex-1 text-[12.5px] leading-5 text-ink">
                {{ groupKeys.length }} 个<template v-if="groupKeys.length">（其中 {{ enabledKeys }} 个已启用）</template>
              </dd>
            </div>
            <div class="flex gap-2 sm:col-span-2">
              <dt class="w-[68px] shrink-0 text-[11.5px] leading-5 text-ink-3">暴露模型</dt>
              <dd class="min-w-0 flex-1">
                <div v-if="exposedModels.length" class="flex flex-wrap gap-1.5">
                  <Badge v-for="m in exposedModels" :key="m" tone="teal">
                    <span class="font-mono">{{ m }}</span>
                  </Badge>
                </div>
                <span v-else class="text-[12px] leading-5 text-ink-3">未设置暴露模型</span>
              </dd>
            </div>
          </dl>
        </Card>

        <!-- 路由层级速览 -->
        <Card :pad="false" class="overflow-hidden">
          <div class="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
            <Waypoints :size="15" class="text-ink-3" />
            <h2 class="text-[13.5px] font-semibold text-ink">路由层级</h2>
            <span class="ml-auto text-[12px] text-ink-3">按 一级 → 二级 → 三级 顺序尝试</span>
          </div>
          <div v-if="sortedTiers.length" class="space-y-2.5 px-4 py-3.5">
            <div v-for="t in sortedTiers" :key="t.level" class="flex items-start gap-3">
              <div class="flex w-12 shrink-0 items-center pt-1">
                <Badge :tone="tierInfo(t.level).tone">{{ tierInfo(t.level).label }}</Badge>
              </div>
              <div class="flex min-w-0 flex-1 flex-wrap items-center gap-1.5">
                <template v-if="t.members?.length">
                  <div
                    v-for="m in t.members" :key="m.model_id"
                    class="flex items-center gap-2 rounded-[10px] border border-line bg-app px-2.5 py-1"
                  >
                    <span class="font-mono text-[12px] text-ink">{{ bindingLabel(m.model_id) }}</span>
                    <Badge tone="gray">权重 {{ m.weight }}</Badge>
                  </div>
                </template>
                <span v-else class="py-1 text-[12px] text-ink-3">该级无成员</span>
              </div>
            </div>
          </div>
          <p v-else class="px-4 py-3.5 text-[12px] text-ink-3">未配置层级。</p>
        </Card>

        <!-- 密钥列表（脱敏） -->
        <Card :pad="false" class="overflow-hidden">
          <div class="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
            <KeyRound :size="15" class="text-ink-3" />
            <h2 class="text-[13.5px] font-semibold text-ink">密钥</h2>
            <span class="ml-auto text-[12px] text-ink-3">只显示前 8 位，完整密钥不会出现在看板里</span>
          </div>
          <div v-if="groupKeys.length" class="overflow-x-auto">
            <table class="w-full text-[12.5px]">
              <thead>
                <tr>
                  <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">名称</th>
                  <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">密钥（脱敏）</th>
                  <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">状态</th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="(k, i) in groupKeys" :key="k.key + i" class="border-t border-line-soft hover:bg-app">
                  <td class="whitespace-nowrap px-3 py-2 text-ink">{{ fmtText(k.name) }}</td>
                  <td class="whitespace-nowrap px-3 py-2 font-mono text-[11.5px] text-ink-2">{{ maskKey(k.key) }}</td>
                  <td class="whitespace-nowrap px-3 py-2">
                    <Badge :tone="k.enabled ? 'ok' : 'gray'">{{ k.enabled ? "已启用" : "已停用" }}</Badge>
                  </td>
                </tr>
              </tbody>
            </table>
          </div>
          <div v-else class="px-4 py-3.5 text-[12px] text-ink-3">该分组还没有密钥。</div>
        </Card>
      </div>

      <!-- ------------------------------ 用量 ------------------------------ -->
      <div v-else-if="tab === 'usage'" class="space-y-3.5">
        <div class="grid grid-cols-2 gap-3 lg:grid-cols-4">
          <Card>
            <div class="text-[11.5px] text-ink-3">请求数</div>
            <div class="mt-1.5 font-mono text-[20px] font-semibold leading-7 tabular-nums text-ink">{{ fmtInt(requests) }}</div>
          </Card>
          <Card>
            <div class="text-[11.5px] text-ink-3">成功率</div>
            <div class="mt-1.5 flex items-center gap-2">
              <span class="font-mono text-[20px] font-semibold leading-7 tabular-nums text-ink">
                {{ successRate === null ? "—" : successRate.toFixed(1) + "%" }}
              </span>
              <Badge :tone="rateTone">成功 {{ fmtInt(success) }} / 失败 {{ fmtInt(failures) }}</Badge>
            </div>
          </Card>
          <Card>
            <div class="text-[11.5px] text-ink-3">输入 tokens</div>
            <div class="mt-1.5 font-mono text-[20px] font-semibold leading-7 tabular-nums text-ink">{{ fmtInt(inputTokens) }}</div>
          </Card>
          <Card>
            <div class="text-[11.5px] text-ink-3">输出 tokens</div>
            <div class="mt-1.5 font-mono text-[20px] font-semibold leading-7 tabular-nums text-ink">{{ fmtInt(outputTokens) }}</div>
          </Card>
          <Card>
            <div class="text-[11.5px] text-ink-3">缓存读</div>
            <div class="mt-1.5 font-mono text-[20px] font-semibold leading-7 tabular-nums text-ink">{{ fmtInt(cacheRead) }}</div>
          </Card>
          <Card>
            <div class="text-[11.5px] text-ink-3">缓存写</div>
            <div class="mt-1.5 font-mono text-[20px] font-semibold leading-7 tabular-nums text-ink">{{ fmtInt(cacheWrite) }}</div>
          </Card>
          <Card>
            <div class="text-[11.5px] text-ink-3">成本</div>
            <div class="mt-1.5 font-mono text-[20px] font-semibold leading-7 tabular-nums text-ink">{{ fmtCost(costTotal) }}</div>
            <div v-if="wasted(wastedCost)" class="mt-0.5 text-[11px] text-warn-600">含浪费 {{ fmtCost(wastedCost) }}</div>
          </Card>
          <Card>
            <div class="text-[11.5px] text-ink-3">平均耗时</div>
            <div class="mt-1.5 font-mono text-[20px] font-semibold leading-7 tabular-nums text-ink">{{ fmtMs(avgDuration) }}</div>
          </Card>
        </div>

        <Card :pad="false" class="overflow-hidden">
          <div class="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
            <Coins :size="15" class="text-ink-3" />
            <h2 class="text-[13.5px] font-semibold text-ink">按模型用量</h2>
            <span class="ml-auto text-[12px] text-ink-3">{{ byModel.length }} 个模型 · 按成本降序</span>
          </div>

          <Empty v-if="!byModel.length" title="暂无用量数据" hint="本分组产生请求并成功计费后，这里会按模型列出 token 与花费">
            <template #icon><Coins :size="28" /></template>
          </Empty>

          <div v-else class="overflow-x-auto">
            <table class="w-full text-[12.5px]">
              <thead>
                <tr>
                  <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">暴露模型</th>
                  <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">渠道</th>
                  <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">上游模型</th>
                  <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">请求数</th>
                  <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">tokens 输入 / 输出</th>
                  <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">成本 / 占比</th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="(row, i) in byModel" :key="i" class="border-t border-line-soft hover:bg-app">
                  <td class="whitespace-nowrap px-3 py-2 text-ink">{{ modelExposed(row) }}</td>
                  <td class="whitespace-nowrap px-3 py-2">{{ modelChannel(row) }}</td>
                  <td class="whitespace-nowrap px-3 py-2 font-mono text-[11.5px] text-ink-2">{{ modelUpstream(row) }}</td>
                  <td class="whitespace-nowrap px-3 py-2 text-right font-mono tabular-nums">
                    {{ fmtInt(firstNum(row, ["requests", "request_count", "count", "total_requests"])) }}
                  </td>
                  <td class="whitespace-nowrap px-3 py-2 text-right font-mono tabular-nums text-ink-2">
                    {{ fmtInt(firstNum(row, ["input_tokens", "prompt_tokens", "input"])) }}
                    /
                    {{ fmtInt(firstNum(row, ["output_tokens", "completion_tokens", "output"])) }}
                  </td>
                  <td class="whitespace-nowrap px-3 py-2">
                    <div class="flex items-center justify-end gap-2">
                      <span class="w-11 text-right text-[11.5px] tabular-nums text-ink-3">{{ shareText(row) }}</span>
                      <div class="h-1 w-16 overflow-hidden rounded-full bg-line-soft">
                        <div class="h-full rounded-full bg-brand-500/70" :style="{ width: sharePct(row) + '%' }" />
                      </div>
                      <span class="font-mono tabular-nums text-ink">{{ fmtCost(modelCost(row)) }}</span>
                    </div>
                  </td>
                </tr>
              </tbody>
            </table>
          </div>
        </Card>
      </div>

      <!-- ---------------------------- 请求日志 ---------------------------- -->
      <Card v-else-if="tab === 'request'" :pad="false" class="overflow-hidden">
        <Empty v-if="!requestRows.length" title="暂无请求记录" hint="本分组产生请求后，这里会显示每一次请求的最终结果">
          <template #icon><ScrollText :size="28" /></template>
        </Empty>

        <div v-else class="overflow-x-auto">
          <table class="w-full text-[12.5px]">
            <thead>
              <tr>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">时间</th>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3 min-w-[200px]">暴露模型</th>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">状态</th>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">最终渠道</th>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3 min-w-[200px]">最终上游模型</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">尝试</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">tokens 输入 / 输出</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">成本</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">耗时</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">TTFT</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="(row, i) in requestRows" :key="rowKey(row, i)" class="border-t border-line-soft hover:bg-app">
                <td class="whitespace-nowrap px-3 py-2 font-mono text-[11.5px] text-ink-2">{{ fmtTs(row.ts) }}</td>
                <td class="whitespace-nowrap px-3 py-2 font-mono text-[11.5px] text-ink-2 min-w-[200px]">{{ fmtText(row.exposed_model) }}</td>
                <td class="whitespace-nowrap px-3 py-2">
                  <Badge :tone="statusInfo(row.status).tone">{{ statusInfo(row.status).label }}</Badge>
                </td>
                <td class="whitespace-nowrap px-3 py-2">{{ finalChannel(row.final_channel) }}</td>
                <td class="whitespace-nowrap px-3 py-2 font-mono text-[11.5px] text-ink-2 min-w-[200px]">{{ fmtText(row.final_upstream_model) }}</td>
                <td class="whitespace-nowrap px-3 py-2 text-right font-mono">{{ fmtInt(row.attempts) }}</td>
                <td class="whitespace-nowrap px-3 py-2 text-right font-mono text-ink-2">
                  {{ fmtInt(row.input_tokens) }} / {{ fmtInt(row.output_tokens) }}
                </td>
                <td class="whitespace-nowrap px-3 py-2 text-right">
                  <span class="font-mono">{{ fmtCost(row.cost_usd) }}</span>
                  <span v-if="wasted(row.wasted_cost_usd)" class="ml-1 text-[11px] text-warn-600">
                    (浪费 {{ fmtCost(row.wasted_cost_usd) }})
                  </span>
                </td>
                <td class="whitespace-nowrap px-3 py-2 text-right font-mono">{{ fmtMs(row.duration_ms) }}</td>
                <td class="whitespace-nowrap px-3 py-2 text-right font-mono">{{ fmtMs(row.ttft_ms) }}</td>
              </tr>
            </tbody>
          </table>
        </div>
      </Card>

      <!-- ---------------------------- 切换日志 ---------------------------- -->
      <Card v-else-if="tab === 'switch'" :pad="false" class="overflow-hidden">
        <Empty v-if="!switchRows.length" title="暂无切换记录" hint="本分组的请求发生故障转移时，这里会显示每一次上游尝试">
          <template #icon><ScrollText :size="28" /></template>
        </Empty>

        <div v-else class="overflow-x-auto">
          <table class="w-full text-[12.5px]">
            <thead>
              <tr>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">时间</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">尝试#</th>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">层级</th>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">渠道</th>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3 min-w-[200px]">上游模型</th>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">结果</th>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">错误类</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">HTTP</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">耗时</th>
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="(row, i) in switchRows" :key="rowKey(row, i)"
                class="border-t border-line-soft hover:bg-app"
                :title="row.message || undefined"
              >
                <td class="whitespace-nowrap px-3 py-2 font-mono text-[11.5px] text-ink-2">{{ fmtTs(row.ts) }}</td>
                <td class="whitespace-nowrap px-3 py-2 text-right font-mono">{{ fmtInt(row.attempt_no) }}</td>
                <td class="whitespace-nowrap px-3 py-2">
                  <Badge :tone="tierInfo(row.tier).tone">{{ tierInfo(row.tier).label }}</Badge>
                </td>
                <td class="whitespace-nowrap px-3 py-2">{{ chLabel(row.channel_name, row.channel_id) }}</td>
                <td class="whitespace-nowrap px-3 py-2 font-mono text-[11.5px] text-ink-2 min-w-[200px]">{{ fmtText(row.upstream_model) }}</td>
                <td class="whitespace-nowrap px-3 py-2">
                  <Badge :tone="outcomeInfo(row.outcome).tone">{{ outcomeInfo(row.outcome).label }}</Badge>
                </td>
                <td class="whitespace-nowrap px-3 py-2">
                  <Badge v-if="errorClass(row.error_class).text" :tone="errorClass(row.error_class).tone">
                    <span class="font-mono text-[10.5px]">{{ errorClass(row.error_class).text }}</span>
                  </Badge>
                  <span v-else class="text-ink-3">—</span>
                </td>
                <td class="whitespace-nowrap px-3 py-2 text-right font-mono">{{ fmtInt(row.http_status) }}</td>
                <td class="whitespace-nowrap px-3 py-2 text-right font-mono">{{ fmtMs(row.latency_ms) }}</td>
              </tr>
            </tbody>
          </table>
        </div>
      </Card>

      <!-- ---------------------------- 冷却状态 ---------------------------- -->
      <Card v-else :pad="false" class="overflow-hidden">
        <div class="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
          <ShieldAlert :size="15" class="text-ink-3" />
          <h2 class="text-[13.5px] font-semibold text-ink">冷却状态</h2>
          <span class="ml-auto text-[12px] text-ink-3">
            {{ scopedCooldowns.length ? `${scopedCooldowns.length} 个条目` : "全部正常" }}
          </span>
        </div>

        <Empty v-if="!scopedCooldowns.length" title="本分组相关渠道暂无冷却" hint="冷却中的渠道会在这里显示剩余时间；这里只列出本分组层级里引用到的渠道">
          <template #icon><ShieldAlert :size="28" /></template>
        </Empty>

        <div v-else class="overflow-x-auto">
          <table class="w-full text-[12.5px]">
            <thead>
              <tr>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">渠道</th>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">上游模型</th>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">类型</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">剩余时间</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="(row, i) in scopedCooldowns" :key="healthKey(row, i)" class="border-t border-line-soft hover:bg-app">
                <td class="whitespace-nowrap px-3 py-2">{{ channelName(row.channel_id) }}</td>
                <td class="whitespace-nowrap px-3 py-2">
                  <Badge v-if="row.upstream_model === '*'" tone="err">整渠道</Badge>
                  <span v-else class="font-mono text-[11.5px] text-ink-2">{{ fmtText(row.upstream_model) }}</span>
                </td>
                <td class="whitespace-nowrap px-3 py-2">
                  <Badge :tone="kindInfo(row.kind).tone">{{ kindInfo(row.kind).label }}</Badge>
                  <span v-if="failCount(row)" class="ml-1.5 font-mono text-[11px] text-ink-3">×{{ failCount(row) }}</span>
                </td>
                <td class="whitespace-nowrap px-3 py-2 text-right font-mono tabular-nums text-ink-2">
                  <span :class="isPermanent(row) ? 'font-sans text-[11.5px] font-medium text-err-600' : ''">{{ fmtCooldownRemain(row) }}</span>
                </td>
              </tr>
            </tbody>
          </table>
        </div>

        <p v-if="scopedCooldowns.length && otherCooldowns > 0" class="border-t border-line-soft px-4 py-2 text-[11.5px] text-ink-3">
          另有 {{ otherCooldowns }} 条其它渠道的冷却未显示（本分组没有引用这些渠道）。
        </p>
      </Card>
    </template>
  </div>
</template>
