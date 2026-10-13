<script setup lang="ts">
// 分组详情（整页版）：分组设置（内联编辑）+ 分组概览 + 该分组的用量统计 + 请求日志 + 切换日志 + 冷却状态。
// 路由层级 / 成员 / 权重 不在这里 —— 那是「模型配置」子页（nav.groupView === "models"），
// 因为改的是路由表，而且带拖动排序，塞进概览会把看板撑长；这里只留一份只读速览 + 跳转按钮。
// 进入页面时用 Promise.all 一次性并发拉全四份数据，页签切换只切本地视图、不再发请求；
// 「自动刷新 5s」开关控制 5s 轮询，离开页面（组件卸载）时定时器一并清掉。
// 页面由 App.vue 按 nav.groupId 挂载，group 一定非空；Esc 等价于点顶栏「返回」。
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import {
  Activity, ArrowRight, Coins, Copy, Info, KeyRound, Pencil, RefreshCw, RotateCcw,
  Save, ScrollText, ShieldAlert, Waypoints,
} from "@lucide/vue";
import {
  api, type CooldownRow as ApiCooldownRow, type Group, type GroupMember, type GroupMembersPayload,
  type RequestLog, type RetryConfig, type StatsPayload, type SwitchLog,
} from "../api";
import { apiKeys, bindingLabel, channelName, markDirty, models, state } from "../store";
import { closeGroupDetail, openModelDetail, setGroupView } from "../nav";
import { toast } from "../toast";
import {
  Alert, Badge, Btn, Card, Empty, Field, PageHeader, Spinner, TInput, TTextarea, TToggle,
} from "../components/ui";

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

/**
 * 自动权重开关（AdminConfig.auto_weight，默认关）：
 * 关闭时同级按列表顺序依次尝试，层级里显示「顺序 N · 权重 N」（权重已填但未参与调度）；
 * 开启时按权重降序，显示「权重 N」。权重两种模式下都能在「分组路由」页手改。
 */
const autoWeight = computed(() => state.cfg?.auto_weight ?? false);

/* --------------------- 成员级真实数据（并发 / 请求数 / 失败） --------------------- */
// 来自 GET /admin/api/group-members：member.inflight = 该成员此刻正在处理的并发请求数，
// requests / failed_attempts 是服务端按该成员聚合的真实计数。端点缺失或请求失败时
// memberPayload 保持 null，chip 退化成「0 次」且不显示「使用中」，整页其余部分照常。
//
// 「使用中」的口径（task-40）：**真实 in-flight 并发 > 0** —— 只有此刻真的在打这个
// (渠道, 上游模型) 才算在用。旧口径 member.active / active_key 是「最近一次成功请求的落点」
// （last-success），与此刻是否在用无关，属于假状态，已不再用于徽章。
// last-success 没有删掉：它仍在页头以「最近成功 …」单独呈现（见 activeMember）。
const memberPayload = ref<GroupMembersPayload | null>(null);
// 同一 model_id 可跨层出现，成员一律用 (tier, model_id) 复合定位。
function memberOf(tierLevel: number, modelId: string): GroupMember | null {
  return memberPayload.value?.members.find(m => m.model_id === modelId && m.tier === tierLevel) ?? null;
}
/** 该成员此刻的并发请求数；成员数据不可用 / 不在用即 0。 */
function memberInflight(tierLevel: number, modelId: string): number {
  return memberOf(tierLevel, modelId)?.inflight ?? 0;
}
/** 是否正在被使用：**真实 in-flight 并发 > 0**（此刻真的在打这个账号）。 */
function isMemberInUse(tierLevel: number, modelId: string): boolean {
  return memberInflight(tierLevel, modelId) > 0;
}
function memberRequests(tierLevel: number, modelId: string): number {
  return memberOf(tierLevel, modelId)?.requests ?? 0;
}
function memberFailed(tierLevel: number, modelId: string): number {
  return memberOf(tierLevel, modelId)?.failed_attempts ?? 0;
}
/** 数字徽章的 title：把「成员数据不可用」和「真的是 0 次」分开说。 */
function memberRequestsTitle(tierLevel: number, modelId: string): string {
  if (!memberPayload.value) return "成员数据不可用";
  const n = memberRequests(tierLevel, modelId);
  return n > 0 ? `本分组内该成员成功请求 ${n} 次` : "本分组内该成员成功请求 0 次";
}
/**
 * 最近一次成功请求落在哪个成员（active_key 指向的那个）——**last-success，不是「使用中」**。
 * 只用于页头那句「最近成功 …」：让「上一个成功落点」与「此刻的并发」两个口径都看得见。
 */
const activeMember = computed<GroupMember | null>(() => {
  const p = memberPayload.value;
  if (!p?.active_key) return null;
  return p.members.find(m => m.key === p.active_key) ?? null;
});
/** 该分组此刻正在处理中的请求总数；服务端没给 inflight_total 时按成员求和兜底。 */
const inflightTotal = computed(() => {
  const p = memberPayload.value;
  if (!p) return 0;
  if (typeof p.inflight_total === "number") return p.inflight_total;
  return p.members.reduce((n, m) => n + (m.inflight ?? 0), 0);
});
/** 「最多 N 轮」：优先用该分组实际生效的重试配置（task-28 起 group-members 会带），否则回落全局。 */
const maxCycles = computed(() => memberPayload.value?.retry?.max_cycles ?? state.cfg?.retry.max_cycles ?? 2);
/** chip 点击进模型详情：优先用服务端 key；端点缺失时用本地绑定推一个等价 key。 */
function openMember(tierLevel: number, modelId: string) {
  const fromServer = memberOf(tierLevel, modelId)?.key;
  const b = models.value.find(x => x.id === modelId);
  const key = fromServer || (b ? `${b.channel_id}::${b.upstream}` : "");
  if (!key) return;
  openModelDetail({ groupId: props.group.id, key, modelId });
}

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
  // 死循环拦截（server/src/proxy.rs 写 error_class: "degenerate_output"）：网关在提交前
  // 判定这次尝试是退化输出，直接丢弃并换下一个候选 —— 是「这次尝试失败」而非整体失败，
  // 所以用 warn 色调 + 中文标签，和上面的 committed 一样走显式映射。
  if (low === "degenerate_output") return { text: "输出退化", tone: "warn" };
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
let loadInflight = false;
async function load(silent = false) {
  const g = props.group;
  if (!g || loadInflight) return;
  loadInflight = true;
  if (!silent) loading.value = true;
  err.value = "";
  try {
    const [s, sw, rq, hl, gm] = await Promise.all([
      api.stats(g.id),
      api.switchLogs(LIMIT, undefined, g.id),
      api.requestLogs(LIMIT, undefined, g.id),
      api.health(),
      // 成员数据失败不拖垮整页：单独吞掉错误，退化成「0 次 + 无使用中（并发未知）」。
      api.groupMembers(g.id).catch(() => null),
    ]);
    stats.value = (s ?? null) as StatsPayload | null;
    switchRows.value = Array.isArray(sw) ? sw : [];
    requestRows.value = Array.isArray(rq) ? rq : [];
    cooldowns.value = Array.isArray(hl?.cooldowns) ? hl.cooldowns : [];
    memberPayload.value = (gm ?? null) as GroupMembersPayload | null;
  } catch (e: any) {
    err.value = String(e?.message || e);
  } finally {
    loadInflight = false;
    loading.value = false;
  }
}

function reset() {
  stats.value = null;
  memberPayload.value = null;
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

/* ---------------------------- 分组设置（内联编辑） ---------------------------- */
// 2026-10-13 起，分组自己的设置不再走「编辑分组」弹窗，而是在这一页直接改：
//  · 名称 / 暴露模型 —— 分组对客户端暴露的身份；
//  · 重试与超时      —— 这个分组自己的策略配置（跟随全局 ↔ 自有配置）。
// 改动先进草稿（sName / sExposed / sRetry），点本卡右上「保存设置」才写回 state.cfg
// 并 markDirty()；真正的落库仍然由顶栏「保存」统一负责 —— 与其它页面同一套语义。
// 路由层级 / 成员 / 权重 不在这里：那是「模型配置」子页（nav.groupView === "models"），
// 因为它是「另一件事」（改的是路由表，而且带拖动排序，塞进概览会把看板撑长）。

const sName = ref("");
const sExposed = ref("");
/** null = 跟随全局；有对象 = 使用这个分组自己的配置（判据只有 overrides.retry 是否存在）。 */
const sRetry = ref<RetryConfig | null>(null);

/** 从当前分组把三份草稿重新灌一遍（进入页面、外部换配置、放弃修改时都走它）。 */
function syncSettings() {
  const g = props.group;
  sName.value = g?.name ?? "";
  sExposed.value = (g?.exposed_models ?? []).join("\n");
  sRetry.value = g?.overrides?.retry ? normRetry(g.overrides.retry) : null;
}
syncSettings();
// 分组对象被换掉（顶栏「重新加载配置」、保存成功）时重新灌草稿，别拿着上一份的残留。
watch(() => props.group, syncSettings);

const sExposedList = computed(() =>
  sExposed.value.split(/[\s,，;；、]+/).map(x => x.trim()).filter(Boolean),
);
const sNameError = computed(() => (sName.value.trim() ? "" : "请填写分组名。"));
const sExposedError = computed(() =>
  sExposedList.value.length ? "" : "至少填写一个暴露模型名，客户端用这些名字发起请求。");

/**
 * 服务端不保证每个字段都回来（老服务端 / 以后会加新字段），缺的用全局值补齐，
 * 避免出现空数字框；已有的一律保留，不做类型转换。
 */
function normRetry(src: RetryConfig): RetryConfig {
  const base = state.cfg!.retry as unknown as Record<string, unknown>;
  const out: Record<string, unknown> = {};
  for (const k of Object.keys(src)) out[k] = (src as unknown as Record<string, unknown>)[k];
  for (const k of Object.keys(base)) {
    if (out[k] === undefined || out[k] === null) out[k] = base[k];
  }
  return out as unknown as RetryConfig;
}

/** 全局配置：服务端回报的 global_retry 优先，否则本地 state.cfg。 */
function globalRetry(): RetryConfig {
  return memberPayload.value?.global_retry ?? state.cfg!.retry;
}
/** 该分组当前实际生效的配置：优先服务端回报，其次它自己的，最后退回全局。 */
function effRetry(): RetryConfig {
  return memberPayload.value?.retry ?? props.group?.overrides?.retry ?? state.cfg!.retry;
}
/** 该分组的配置来源：优先服务端回报，否则按本地是否自有判断。 */
function retrySrc(): "global" | "group" {
  const srv = memberPayload.value?.retry_source;
  if (srv === "group" || srv === "global") return srv;
  return sRetry.value ? "group" : "global";
}

/** 切到「使用自己的配置」：从当前生效值克隆一份，之后改的就是这份独立副本。 */
function useOwn() {
  if (sRetry.value) return;
  sRetry.value = normRetry(effRetry());
}
/** 切回「跟随全局配置」：写 null，绝不去复制全局值。 */
function followGlobal() {
  if (!sRetry.value) return;
  sRetry.value = null;
}
/** 把当前全局配置整份复制到这个分组，之后它就和全局解耦、可以单独改了。 */
function copyGlobal() {
  sRetry.value = normRetry(globalRetry());
}

/** 该分组自己的配置里的预算校验 —— 与设置页「重试与超时」同一套公式。 */
const sBudget = computed(() => {
  const r = sRetry.value;
  if (!r) return { ok: true, line: "" };
  const worst = Number(r.max_attempts) * Number(r.attempt_deadline_ms) + Number(r.reserve_ms);
  return {
    ok: worst <= Number(r.total_deadline_ms),
    line: `${r.max_attempts} × ${r.attempt_deadline_ms} ms + ${r.reserve_ms} ms = ${worst} ms`,
  };
});

/** 跟随全局时的一句话摘要：用实际生效值，和生效的模型名一样能直接看。 */
const followLine = computed(() => {
  const r = effRetry();
  const sec = Math.max(1, Math.round(Number(r.total_deadline_ms) / 1000));
  return `跟随全局（当前：max_attempts=${r.max_attempts} / 总超时 ${sec}s / 死循环拦截 ${r.loop_guard_enabled ? "开" : "关"}）`;
});

/** 有没有未保存的设置改动（顶栏之外的第二处脏标记，只描述本卡）。 */
const sDirty = computed(() => {
  const g = props.group;
  if (!g) return false;
  const nextRetry = sRetry.value ? normRetry(sRetry.value) : null;
  const curRetry = g.overrides?.retry ?? null;
  return sName.value.trim() !== (g.name ?? "")
    || JSON.stringify(sExposedList.value) !== JSON.stringify(g.exposed_models ?? [])
    || JSON.stringify(nextRetry) !== JSON.stringify(curRetry);
});

const sValid = computed(() => !sNameError.value && !sExposedError.value && sBudget.value.ok);

/** 写回配置（只进内存 + markDirty，落库交给顶栏「保存」）。 */
function saveSettings() {
  const cfg = state.cfg;
  const g = props.group;
  if (!cfg || !g || !sValid.value) return;
  const clean: Group = {
    id: g.id,
    name: sName.value.trim(),
    exposed_models: [...sExposedList.value],
    // 层级归「模型配置」子页管，这里原样深拷贝带回去，免得把那边改好的路由表抹掉。
    tiers: JSON.parse(JSON.stringify(g.tiers ?? [])),
    // ★ overrides 必须显式写：不带这个字段等于把该分组自己的重试配置静默抹掉，
    //   服务端会把它当成「跟随全局」（None 序列化成 "retry": null）。
    overrides: { retry: sRetry.value ? normRetry(sRetry.value) : null },
  };
  const i = cfg.groups.findIndex(x => x.id === g.id);
  if (i >= 0) cfg.groups.splice(i, 1, clean);
  else cfg.groups.push(clean);
  markDirty();
  toast.ok(`已更新分组「${clean.name || clean.id}」的设置`);
}

/** 放弃本卡的改动：从当前分组重新灌一遍草稿。 */
function resetSettings() {
  syncSettings();
  toast.ok("已放弃未保存的分组设置改动");
}

/** 跳到「模型配置」子页（路由层级 / 成员 / 权重）。 */
function openModels() {
  setGroupView("models");
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

    <!-- 两个不同口径，别混：左边是**此刻**的并发（真实 in-flight），右边是**上一次**成功落点（last-success） -->
    <div class="flex flex-wrap items-center gap-2 text-[12px]" data-test="gd-active-model">
      <span class="text-ink-3">当前并发</span>
      <span
        v-if="inflightTotal > 0"
        data-test="gd-inflight-total"
        class="inline-flex items-center gap-1 rounded-md bg-teal-100 px-1.5 py-0.5 text-[11.5px] font-medium text-teal-700"
        title="本分组此刻正在处理中的请求数（真实 in-flight 并发）"
      >
        <span class="h-1.5 w-1.5 rounded-full bg-teal-500"></span>{{ inflightTotal }} 个请求
      </span>
      <span v-else class="text-ink-3" data-test="gd-inflight-total" title="此刻没有正在处理中的请求">空闲</span>
      <span v-if="activeMember" class="ml-auto font-mono text-[11.5px] text-ink-3">
        最近成功 {{ activeMember.last_used_ts ? fmtTs(activeMember.last_used_ts) : "—" }}
        · {{ activeMember.channel_name }} · {{ activeMember.upstream_model }}
      </span>
      <span v-else class="ml-auto font-mono text-[11.5px] text-ink-3">最近成功 —（本分组还没有成功请求）</span>
    </div>
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
        <!-- ------------------------ 分组设置（内联编辑） ------------------------ -->
        <Card :pad="false" class="overflow-hidden" data-test="group-settings-card">
          <div class="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
            <Pencil :size="15" class="text-ink-3" />
            <h2 class="text-[13.5px] font-semibold text-ink">分组设置</h2>
            <Badge v-if="sDirty" tone="warn" data-test="group-settings-dirty">有未保存的改动</Badge>
            <span class="ml-auto flex shrink-0 items-center gap-2">
              <Btn size="sm" variant="ghost" data-test="group-settings-reset" :disabled="!sDirty" @click="resetSettings">
                <RotateCcw :size="13" />放弃修改
              </Btn>
              <Btn size="sm" variant="primary" data-test="group-settings-save" :disabled="!sValid || !sDirty" @click="saveSettings">
                <Save :size="13" />保存设置
              </Btn>
            </span>
          </div>

          <div class="space-y-4 px-4 py-3.5">
            <div class="grid grid-cols-1 gap-4 sm:grid-cols-2">
              <Field label="分组 ID" hint="已保存的分组不建议再改 ID（密钥与日志都按它归属）">
                <TInput :model-value="group.id" mono disabled />
              </Field>
              <Field label="分组名" required hint="例如：主力池 / 备用池">
                <TInput v-model="sName" data-test="group-name-input" :invalid="!!sNameError" placeholder="主力池" />
              </Field>
            </div>

            <Field
              label="暴露模型"
              required
              hint="客户端用这些模型名请求，会按本分组的路由表转发；逗号或换行分隔"
            >
              <TTextarea v-model="sExposed" data-test="group-exposed-input" :rows="2" placeholder="gpt-4o, claude-3-5-sonnet" />
            </Field>

            <Alert v-if="sNameError || sExposedError" tone="err" data-test="group-settings-error">
              {{ sNameError || sExposedError }}
            </Alert>

        <!-- ------------------------ 重试与超时（本分组自己的策略配置） ------------------------ -->
        <!-- 这块原先在设置页，按需求挪进「分组自己的配置」里：改哪个分组就在哪个分组的弹窗里改。 -->
        <div class="border-t border-line-soft pt-4" data-test="group-retry-card">
          <div class="mb-2.5 flex flex-wrap items-start justify-between gap-x-4 gap-y-2">
            <div class="min-w-0">
              <div class="flex flex-wrap items-center gap-2">
                <span class="text-[12.5px] font-medium text-ink-2">重试与超时</span>
                <Badge :tone="sRetry ? 'brand' : 'gray'">
                  {{ sRetry ? "自己的配置" : "跟随全局" }}
                </Badge>
              </div>
              <p class="mt-1 text-[11.5px] leading-4 text-ink-3">
                这个分组可以跟随全局配置，也可以拥有自己的一份；有自己的配置时以它为准，全局改动不再影响它。
              </p>
            </div>

            <div class="flex shrink-0 flex-wrap items-center gap-2" data-test="group-retry-source">
              <Btn
                size="sm" :variant="sRetry ? 'ghost' : 'primary'"
                data-test="group-retry-follow"
                @click="followGlobal"
              >跟随全局配置</Btn>
              <Btn
                size="sm" :variant="sRetry ? 'primary' : 'ghost'"
                data-test="group-retry-own"
                @click="useOwn"
              >使用自己的配置</Btn>
            </div>
          </div>

          <p
            v-if="!sRetry"
            class="rounded-lg border border-line-soft bg-app/40 px-3 py-2 text-[12px] leading-5 text-ink-2"
            data-test="group-retry-following"
          >
            {{ followLine }}
          </p>

          <div v-else>
            <div class="mb-2.5 flex flex-wrap items-center justify-between gap-x-4 gap-y-2">
              <span class="text-[11.5px] font-medium uppercase tracking-wide text-ink-3">独立配置</span>
              <Btn
                size="sm" variant="subtle"
                data-test="group-retry-copy-global"
                @click="copyGlobal"
              >
                <Copy :size="12" />
                复制当前全局配置
              </Btn>
            </div>

            <div data-test="group-retry-fields" class="grid grid-cols-1 gap-x-5 gap-y-4 sm:grid-cols-2">
                <Field label="最大尝试次数" hint="单个请求最多尝试多少次（含降级后的重试）">
                  <div data-test="group-retry-max-attempts">
                    <TInput v-model.number="sRetry.max_attempts" type="number" data-test="group-retry-max-attempts" />
                  </div>
                </Field>
                <Field label="最大循环轮数" hint="走完全部层级算 1 轮，最多循环几轮">
                  <div data-test="group-retry-max-cycles">
                    <TInput v-model.number="sRetry.max_cycles" type="number" data-test="group-retry-max-cycles" />
                  </div>
                </Field>
                <Field label="总预算 (ms)" hint="整个请求的总时间上限，超过即直接失败">
                  <div data-test="group-retry-total-deadline-ms">
                    <TInput v-model.number="sRetry.total_deadline_ms" type="number" data-test="group-retry-total-deadline-ms" />
                  </div>
                </Field>
                <Field label="单次尝试上限 (ms)" hint="一次尝试的最长耗时，超时即判定失败并降级">
                  <div data-test="group-retry-attempt-deadline-ms">
                    <TInput v-model.number="sRetry.attempt_deadline_ms" type="number" data-test="group-retry-attempt-deadline-ms" />
                  </div>
                </Field>
                <Field label="连接超时 (ms)" hint="与上游建立 TCP / TLS 连接的超时">
                  <div data-test="group-retry-connect-timeout-ms">
                    <TInput v-model.number="sRetry.connect_timeout_ms" type="number" data-test="group-retry-connect-timeout-ms" />
                  </div>
                </Field>
                <Field label="首字节超时 (ms)" hint="发出请求到收到响应首字节的超时">
                  <div data-test="group-retry-ttfb-timeout-ms">
                    <TInput v-model.number="sRetry.ttfb_timeout_ms" type="number" data-test="group-retry-ttfb-timeout-ms" />
                  </div>
                </Field>
                <Field label="首个事件超时 (ms)" hint="流式响应中第一个事件到达的超时">
                  <div data-test="group-retry-first-event-timeout-ms">
                    <TInput v-model.number="sRetry.first_event_timeout_ms" type="number" data-test="group-retry-first-event-timeout-ms" />
                  </div>
                </Field>
                <Field label="流空闲超时 (ms)" hint="流式响应里两个数据块之间的最大空闲时间">
                  <div data-test="group-retry-chunk-idle-timeout-ms">
                    <TInput v-model.number="sRetry.chunk_idle_timeout_ms" type="number" data-test="group-retry-chunk-idle-timeout-ms" />
                  </div>
                </Field>
                <Field label="预留余量 (ms)" hint="给收尾/补帧留的时间">
                  <div data-test="group-retry-reserve-ms">
                    <TInput v-model.number="sRetry.reserve_ms" type="number" data-test="group-retry-reserve-ms" />
                  </div>
                </Field>
            </div>

            <div class="mt-3 border-t border-line-soft pt-3">
              <div data-test="group-retry-cooldown-wait">
                <TToggle
                  v-model="sRetry.cooldown_wait"
                  label="全部冷却时等待而非直接失败"
                  data-test="group-retry-cooldown-wait"
                />
              </div>
              <p class="mt-1 text-[11.5px] leading-4 text-ink-3">
                开启后，若该分组所有候选渠道都在冷却中，请求会等待最早解除冷却的渠道，而不是立刻返回失败。
              </p>
            </div>

            <div class="mt-3 border-t border-line-soft pt-3">
              <div data-test="group-retry-loop-guard-enabled">
                <TToggle
                  v-model="sRetry.loop_guard_enabled"
                  label="拦截退化输出（死循环拦截）"
                  data-test="group-retry-loop-guard-enabled"
                />
              </div>
              <div class="mt-3 grid grid-cols-1 gap-x-5 gap-y-4 sm:grid-cols-2">
                <Field label="检测起点 (字符)" hint="提交前先攒够这么多字符再开始判断：太小容易误判，太大则首字节推迟更久">
                  <div data-test="group-retry-loop-guard-min-chars">
                    <TInput v-model.number="sRetry.loop_guard_min_chars" type="number" min="1" data-test="group-retry-loop-guard-min-chars" />
                  </div>
                </Field>
                <Field label="重复单元下限 (字符)" hint="被重复的那一段至少多长才算「无意义重复」，太短会把正常的中文/标点重复算进去">
                  <div data-test="group-retry-loop-guard-repeat-min-chars">
                    <TInput v-model.number="sRetry.loop_guard_repeat_min_chars" type="number" min="1" data-test="group-retry-loop-guard-repeat-min-chars" />
                  </div>
                </Field>
                <Field label="重复占比阈值" hint="0~1 的小数，例如 0.75 表示重复部分要占到检测窗口的 75% 才判定为退化">
                  <div data-test="group-retry-loop-guard-repeat-ratio">
                    <TInput v-model.number="sRetry.loop_guard_repeat_ratio" type="number" min="0" max="1" step="0.05" data-test="group-retry-loop-guard-repeat-ratio" />
                  </div>
                </Field>
                <Field label="同一段重复次数" hint="同一段内容重复到第几次判定为退化，越小越敏感">
                  <div data-test="group-retry-loop-guard-max-repeats">
                    <TInput v-model.number="sRetry.loop_guard_max_repeats" type="number" min="2" data-test="group-retry-loop-guard-max-repeats" />
                  </div>
                </Field>
                <Field label="检测窗口 (字符)" hint="最多回看多少字符，超出只保留最近的；越大越吃内存，越小越容易漏判">
                  <div data-test="group-retry-loop-guard-window-chars">
                    <TInput v-model.number="sRetry.loop_guard_window_chars" type="number" min="1" data-test="group-retry-loop-guard-window-chars" />
                  </div>
                </Field>
              </div>
            </div>

            <Alert v-if="!sBudget.ok" tone="err" class="mt-3" data-test="group-retry-invalid">
              <Info :size="13" class="mt-0.5 shrink-0" />
              <span>
                预算不成立：{{ sBudget.line }} &gt; 总预算 {{ sRetry.total_deadline_ms }} ms，
                服务端会拒绝保存，请调小尝试次数或单次上限。
              </span>
            </Alert>
          </div>
        </div>
          </div>
        </Card>

        <!-- ------------------------ 分组概览（只读摘要） ------------------------ -->
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
            <span class="ml-auto text-[12px] text-ink-3">
              <template v-if="autoWeight">按 一级 → 二级 → 三级 顺序尝试，同级内按权重降序</template>
              <template v-else>按 一级 → 二级 → 三级 顺序尝试，同级内按列表顺序</template>
            </span>
            <Btn size="sm" variant="ghost" data-test="gd-open-models" @click="openModels">
              <Pencil :size="12" />模型配置
            </Btn>
          </div>
          <div v-if="sortedTiers.length" class="space-y-2.5 px-4 py-3.5">
            <div v-for="t in sortedTiers" :key="t.level" class="flex items-start gap-3">
              <div class="flex w-12 shrink-0 items-center pt-1">
                <Badge :tone="tierInfo(t.level).tone">{{ tierInfo(t.level).label }}</Badge>
              </div>
              <div class="flex min-w-0 flex-1 flex-wrap items-center gap-1.5">
                <template v-if="t.members?.length">
                  <div
                    v-for="(m, mi) in t.members" :key="t.level + '-' + m.model_id"
                    data-test="gd-member-chip"
                    class="flex cursor-pointer items-center gap-2 rounded-[10px] border border-line bg-app px-2.5 py-1 transition-colors hover:border-teal-300"
                    :title="'查看 ' + bindingLabel(m.model_id) + ' 的详情'"
                    @click="openMember(t.level, m.model_id)"
                  >
                    <span class="font-mono text-[12px] text-ink">{{ bindingLabel(m.model_id) }}</span>
                    <Badge tone="gray">{{ autoWeight ? `权重 ${m.weight}` : `顺序 ${mi + 1} · 权重 ${m.weight}` }}</Badge>
                    <Badge
                      tone="gray"
                      data-test="gd-member-requests"
                      :title="memberRequestsTitle(t.level, m.model_id)"
                    >{{ memberRequests(t.level, m.model_id) }} 次</Badge>
                    <Badge v-if="memberFailed(t.level, m.model_id) > 0" tone="err" data-test="gd-member-failed">
                      失败 {{ memberFailed(t.level, m.model_id) }}
                    </Badge>
                    <span
                      v-if="isMemberInUse(t.level, m.model_id)"
                      data-test="gd-member-active"
                      class="inline-flex shrink-0 items-center gap-1 rounded-md bg-teal-100 px-1.5 py-0.5 text-[11.5px] font-medium text-teal-700"
                      title="此刻正在使用该 (渠道, 上游模型) 的并发请求数"
                    ><span class="h-1.5 w-1.5 rounded-full bg-teal-500"></span>使用中 ×{{ memberInflight(t.level, m.model_id) }}</span>
                  </div>
                </template>
                <span v-else class="py-1 text-[12px] text-ink-3">该级无成员</span>
              </div>
            </div>
          </div>
          <p v-else class="px-4 py-3.5 text-[12px] text-ink-3">未配置层级。</p>
          <p
            v-if="sortedTiers.length"
            class="border-t border-line-soft px-4 py-2.5 text-[11.5px] leading-4 text-ink-3"
            data-test="gd-tier-note"
          >↑ 全部失败 → 回到一级（最多 {{ maxCycles }} 轮）</p>
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
            <span class="ml-auto text-[12px] text-ink-3">{{ byModel.length }} 个模型 · 按成本降序 · 按 暴露模型 × 上游 × 渠道 聚合</span>
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
