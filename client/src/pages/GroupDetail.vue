<script setup lang="ts">
// 分组详情（整页版）：分组设置 + 模型配置（路由层级 / 成员 / 权重，内联编辑）+ 分组概览
// + 该分组的用量统计 + 请求日志 + 切换日志 + 冷却状态。
// 2026-10-14：「模型配置」原本是一个独立子页（nav.groupView === "models"），已并入本页 ——
// 层级 / 成员 / 权重直接在当前页面编辑，不再跳来跳去。本页因此同时管两份草稿（分组设置一份、
// 路由层级一份），各自有独立的「保存 / 放弃」按钮，落库仍由顶栏「保存」统一负责。
// 进入页面时用 Promise.all 一次性并发拉全四份数据，页签切换只切本地视图；
// 三个页签各自分页（请求日志 / 切换日志走服务端 limit+offset，冷却状态是本地分片），
// 翻页只重拉两份日志，stats / health / 成员不动。
// 页面由 App.vue 按 nav.groupId 挂载，group 一定非空；Esc 等价于点顶栏「返回」。
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import {
  Activity, ArrowDown, ArrowRight, ArrowUp, BarChart3, Coins, Copy, GripVertical, Info, KeyRound,
  Layers, Pencil, PieChart, Plus, RefreshCw, RotateCcw, Save, ScrollText, ShieldAlert, Trash2, Waypoints,
} from "@lucide/vue";
import {
  api, type CooldownRow as ApiCooldownRow, type Group, type GroupMember, type GroupMembersPayload,
  type RequestLog, type RetryConfig, type StatsPayload, type SwitchLog, type Tier, type TierMember,
} from "../api";
import { apiKeys, bindingLabel, channelName, markDirty, models, state } from "../store";
import { closeGroupDetail, openModelDetail } from "../nav";
import { toast } from "../toast";
import type { EChartsCoreOption } from "echarts/core";
import { fmtSec, fmtTps, tpsOf } from "../format";
import { RETRY_PRESETS, matchRetryPreset, retryPresetOptions } from "../retry-presets";
import { chartPalette, chartTheme } from "../chart-theme";
import EChart from "../components/EChart.vue";
import {
  Alert, Badge, Btn, Card, Empty, Field, IconBtn, PageHeader, SecInput, Spinner, TInput, TPager,
  TSelect, TToggle, TRange, DEFAULT_DAYS,
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

// 三个页签各自分页：请求日志 / 切换日志走服务端分页（limit + offset），
// 冷却状态的数据源是 /admin/api/health 的全量快照，没有分页接口，只能前端本地分片。
const PAGE_SIZES = [20, 50, 100, 200];
const DEFAULT_SIZE = 20;
const COOLDOWN_PAGE_SIZES = [10, 20, 50];
const COOLDOWN_SIZE = 20;
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
/** 该成员思考打转、被掐断继续转发思考的次数。0 = 没发生过。 */
function memberReasoningLoops(tierLevel: number, modelId: string): number {
  return memberOf(tierLevel, modelId)?.reasoning_truncated ?? 0;
}
/** 徽章 title：说明是「思考」打转而不是「回答」被截断，两者不是一回事。 */
function memberReasoningTitle(tierLevel: number, modelId: string): string {
  const n = memberReasoningLoops(tierLevel, modelId);
  return `该模型在自己的思考里打转 ${n} 次，网关掐断了继续转发的思考；回答本身没有被截断`;
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
function statusInfo(row: { status?: unknown; loop_truncated?: unknown }): { label: string; tone: Tone } {
  // 与 Logs.vue 同一份判定：提交后截断的回答 status 仍是 success（客户端确实收到了完整合法的
  // 流，I2 终态互斥），但内容被截掉了 —— 直接写「成功」等于把退化回答说成好回答。
  if (Number(row?.loop_truncated) > 0) return { label: "成功（已截断）", tone: "warn" };
  switch (String(row?.status ?? "").toLowerCase()) {
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
  if (s < 60) return `${s}秒`;
  if (s < 3600) return `${Math.floor(s / 60)}分${s % 60}秒`;
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return `${h}小时${m}分`;
}
function healthKey(row: CooldownRow, i: number): string {
  return `${row?.channel_id ?? "c"}|${row?.upstream_model ?? ""}|${row?.kind ?? ""}|${i}`;
}
/** 密钥脱敏：表格里只渲染前 8 位，完整值不写进 DOM 文本、也不写进 title。 */
function maskKey(v: unknown): string {
  const s = String(v ?? "");
  if (!s) return "—";
  return s.slice(0, 8) + "…";
}
/**
 * 复制完整密钥（看板里唯一的取回入口）。
 *
 * 完整密钥本来就在前端 state 里（`/admin/api/config` 下发时就带着），脱敏只是这一列的
 * 渲染选择 —— 所以「能复制」并没有让密钥多暴露一分，只是把本来就有的值取出来。
 * 取值只在点击这一刻发生，页面上（含 title/tooltip）始终只有脱敏串。
 *
 * 优先 navigator.clipboard；失败（非安全上下文 / 无权限）时退回临时 textarea ——
 * 那条路径会让完整密钥短暂进一次 DOM，但随即移除。与 Logs.vue 的复制实现同一份语义。
 */
async function copyKey(v: unknown) {
  const s = String(v ?? "");
  if (!s) return;
  try {
    await navigator.clipboard.writeText(s);
    toast.ok("已复制完整密钥");
    return;
  } catch {
    /* 回退见下 */
  }
  try {
    const el = document.createElement("textarea");
    el.value = s;
    el.style.position = "fixed";
    el.style.opacity = "0";
    document.body.appendChild(el);
    el.select();
    const ok = document.execCommand("copy");
    document.body.removeChild(el);
    if (ok) toast.ok("已复制完整密钥");
    else toast.err("复制失败");
  } catch {
    toast.err("复制失败");
  }
}

/* --------------------------------- 状态 --------------------------------- */
const tab = ref<TabKey>("overview");
const autoRefresh = ref(true);
// 时间范围（天）：作用于本页的**用量 / 请求日志 / 切换日志**三个页签。
// 冷却状态不参与 —— 它是「此刻正在冷却」的实时快照，没有历史时间戳可筛。
const days = ref<number>(DEFAULT_DAYS);
const loading = ref(false);
const err = ref("");
const lastAt = ref(0);
const now = ref(Date.now());
const stats = ref<StatsPayload | null>(null);
const switchRows = ref<SwitchLog[]>([]);
const requestRows = ref<RequestLog[]>([]);
const cooldowns = ref<CooldownRow[]>([]);

// 分页状态：页码从 1 开始，三个页签各记一份（切页签不互相影响）。
// total 是服务端给的「过滤命中总数」，忽略分页窗口，所以翻页时它不变。
const requestPage = ref(1);
const requestSize = ref(DEFAULT_SIZE);
const requestTotal = ref(0);
const switchPage = ref(1);
const switchSize = ref(DEFAULT_SIZE);
const switchTotal = ref(0);
// 冷却没有服务端分页，只记页码与每页条数。
const cooldownPage = ref(1);
const cooldownSize = ref(COOLDOWN_SIZE);

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
// 翻页请求撞上 5 秒自动刷新时不能把这一页丢掉：置一个待办标记，等当前这轮结束后补拉一次。
// 否则连点「下一页」会被互斥锁静默吞掉 —— 页码变了，表格还停在上一个页码的数据上。
let loadPending = false;
async function load(silent = false) {
  const g = props.group;
  if (!g) return;
  if (loadInflight) { loadPending = true; return; }
  loadInflight = true;
  try {
    const [s, sw, rq, hl, gm] = await Promise.all([
      api.stats(g.id, days.value),
      // v0.4.4 起这两个接口返回 { rows, total, limit, offset }：total 是忽略分页窗口的
      // 过滤命中总数，页签右侧与分页条都读它，翻页时不会变小。offset 由页签页码算出来。
      api.switchLogsPage(switchSize.value, (switchPage.value - 1) * switchSize.value, undefined, g.id, days.value),
      api.requestLogsPage(requestSize.value, (requestPage.value - 1) * requestSize.value, undefined, g.id, days.value),
      api.health(),
      // 成员数据失败不拖垮整页：单独吞掉错误，退化成「0 次 + 无使用中（并发未知）」。
      api.groupMembers(g.id).catch(() => null),
    ]);
    stats.value = (s ?? null) as StatsPayload | null;
    switchRows.value = Array.isArray(sw?.rows) ? sw.rows : [];
    switchTotal.value = Number(sw?.total ?? 0) || 0;
    requestRows.value = Array.isArray(rq?.rows) ? rq.rows : [];
    requestTotal.value = Number(rq?.total ?? 0) || 0;
    cooldowns.value = Array.isArray(hl?.cooldowns) ? hl.cooldowns : [];
    memberPayload.value = (gm ?? null) as GroupMembersPayload | null;
    // 页码越界（比如这期间该分组的日志被清理了）时收敛回最后一页；
    // 写回 page 会触发分页 watch 再拉一次，拿到的是有效页。
    const lastSw = Math.max(1, Math.ceil(switchTotal.value / switchSize.value));
    if (switchPage.value > lastSw) switchPage.value = lastSw;
    const lastRq = Math.max(1, Math.ceil(requestTotal.value / requestSize.value));
    if (requestPage.value > lastRq) requestPage.value = lastRq;
    const lastCd = Math.max(1, Math.ceil(scopedCooldowns.value.length / cooldownSize.value));
    if (cooldownPage.value > lastCd) cooldownPage.value = lastCd;
  } catch (e: any) {
    err.value = String(e?.message || e);
  } finally {
    loadInflight = false;
    loading.value = false;
    if (loadPending) { loadPending = false; void load(true); }
  }
}

function reset() {
  stats.value = null;
  memberPayload.value = null;
  switchRows.value = [];
  requestRows.value = [];
  cooldowns.value = [];
  switchTotal.value = 0;
  requestTotal.value = 0;
  switchPage.value = 1;
  requestPage.value = 1;
  cooldownPage.value = 1;
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

// 翻页 / 改每页条数：只重拉两份日志，stats / health / 成员都不动。
// 冷却页是本地分片，换页不需要请求，所以不挂在这个 watch 上；
// silent = true 让翻页时不要整页变成「正在加载…」骨架，表格就地替换。
watch([requestPage, requestSize, switchPage, switchSize], () => { void load(true); });
// 换时间范围：行数会变，页码必须回到第 1 页，否则会停在旧范围的第 N 页上。
watch(days, () => { requestPage.value = 1; switchPage.value = 1; void load(true); });

/** Esc = 返回分组路由（保留弹窗版按 Esc 关闭的习惯）。
 *  ★ 模型配置卡里正在拖动排序时不吃这一下：onDragKey 只放弃拖动，页面留在原地。 */
function onKey(e: KeyboardEvent) {
  if (e.key === "Escape" && !dragging.value) closeGroupDetail();
}

onMounted(() => {
  void load();
  startTimer();
  startTick();
  window.addEventListener("keydown", onKey);
  // 拖动排序：挂在 window 上，鼠标划出卡片、甚至划出窗口，move / up 一样能收到
  window.addEventListener("pointermove", onDragMove);
  window.addEventListener("pointerup", commitDrag);
  window.addEventListener("pointercancel", endDrag);
  window.addEventListener("keydown", onDragKey);
});

onUnmounted(() => {
  stopTimer();
  stopTick();
  window.removeEventListener("keydown", onKey);
  // 拖动途中切页时清干净，避免残留 transform 与光标锁定
  document.body.classList.remove("gw-dragging");
  window.removeEventListener("pointermove", onDragMove);
  window.removeEventListener("pointerup", commitDrag);
  window.removeEventListener("pointercancel", endDrag);
  window.removeEventListener("keydown", onDragKey);
});

/* ------------------------------ 概览数据 ------------------------------ */
/**
 * 本分组的暴露模型：**服务端按成员自动派生**，控制台只读（页面上没有可填的输入框）。
 * 口径与 `server/src/config.rs` 的 `derive_exposed_models` 一字不差：层级按 level 升序、
 * 层内按声明顺序，取每个成员绑定的 `exposed`，首次出现去重。
 * 这里再算一遍，只为让界面在「改了层级 / 换了绑定但还没点保存」时也立刻显示新结果；
 * 真正生效的永远是服务端那一份 —— `GET /admin/api/config` 回来的就是派生后的列表。
 */
function deriveExposedTiers(tiers: Tier[] | undefined): string[] {
  const out: string[] = [];
  for (const t of [...(tiers ?? [])].sort((a, b) => a.level - b.level)) {
    for (const mem of t.members ?? []) {
      const b = models.value.find(x => x.id === mem.model_id);
      if (!b || out.includes(b.exposed)) continue;
      out.push(b.exposed);
    }
  }
  return out;
}
function deriveExposed(g: Group | undefined): string[] {
  return deriveExposedTiers(g?.tiers);
}
const exposedModels = computed<string[]>(() => deriveExposed(props.group));
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
// 冷却列表来自 /admin/api/health 的全量快照，没有 limit/offset，所以这一页由前端自己切；
// total 就是本分组相关渠道的条数（与表头「共 N 个条目」同一个数）。
const pagedCooldowns = computed(() => {
  const start = (cooldownPage.value - 1) * cooldownSize.value;
  return scopedCooldowns.value.slice(start, start + cooldownSize.value);
});

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
/** 这一行是否真的路由到了上游。0 = 从未落到任何上游（全渠道失败/耗尽）。 */
function modelRouted(row: Row): boolean {
  return firstNum(row, ["routed"]) !== 0;
}
/** 行种类（后端 stats_by_model 的 kind）：routed=提交成功 / attempted=试过但失败 / unrouted=一次都没试。 */
function modelKind(row: Row): string {
  const k = firstText(row, ["kind"]);
  if (k) return k;
  // 兼容旧后端：没有 kind 时按 routed 推断。
  return modelRouted(row) ? "routed" : "unrouted";
}
/**
 * 没走到任何渠道时那一行的名字。
 * 它没有渠道、也没有模型，所以**不冒充**任何模型名 —— 如实说明「一次都没试」。
 */
const UNROUTED_LABEL = "未尝试任何渠道（全冷却 / 无候选）";
/**
 * 本行的模型身份 = **真实路由到的上游模型**。
 * 后端按 final_channel × final_upstream_model 聚合（server/src/store.rs 的 stats_by_model）。
 * 这里曾经用 exposed_model，但分组名/分组 id 本身就是可调用的模型名（router::is_wildcard 的
 * 通配路径），于是「grp_main」「主力分组」这种别名会各自成行，把分组名当成模型画进图里
 * （实测 grp_main 的 68 次请求里有 43 次落在这种别名行上）。
 */
function modelLabel(row: Row): string {
  if (modelKind(row) === "unrouted") return UNROUTED_LABEL;
  const up = modelUpstream(row);
  const name = up === "—" ? "未知模型" : up;
  // 试过但失败的行：模型名后标注「未成功」，否则图上会与成功行同名、无法分辨。
  return modelKind(row) === "attempted" ? name + "（未成功）" : name;
}
/**
 * 状态列文案。三态必须一眼可分：
 *   成功 = 请求提交到了这个 渠道×模型；
 *   未成功 = 试过这个 渠道×模型 但失败了（请求最终失败）；
 *   未尝试 = 一次候选都没试（全冷却 / 无候选），这一行没有渠道也没有模型。
 */
function modelStatusText(row: Row): string {
  const k = modelKind(row);
  if (k === "routed") return "成功";
  if (k === "attempted") return "未成功";
  return "未尝试";
}
function modelStatusClass(row: Row): string {
  // 只有 -700 档是真实存在的主题 token（style.css 的 --color-ok-700 / --color-warn-700）。
  return modelKind(row) === "routed" ? "text-ok-700" : "text-warn-700";
}
/**
 * 图表标签。同一个上游模型可能同时经由多个渠道出现（实测 deepseek-v4.1-flash 落在 7 个
 * 渠道上），此时补上渠道名，否则图上会出现多根同名柱子、无法分辨。
 */
function modelLabels(rows: Row[]): string[] {
  const base = rows.map(modelLabel);
  const dup = new Set<string>();
  const seen = new Set<string>();
  for (const b of base) { if (seen.has(b)) dup.add(b); else seen.add(b); }
  return base.map((b, i) => dup.has(b) ? chartLabel(b, 12) + " · " + chartLabel(modelChannel(rows[i]), 10) : chartLabel(b));
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

/* ------------------------------ 用量图表 ------------------------------ */
// 用 ECharts（../components/EChart.vue，与统计页同一个封装），不自己画 SVG：
// canvas 渲染拿不到 CSS 变量，所以颜色每次现取一遍设计 token，保证跟页面一致。
const chartPaletteRef = ref<string[]>(chartPalette(chartTheme()));
// 数据拉回来后重取一次 —— 侧栏折叠 / 主题切换后 token 可能变，图表得跟着变。
watch(stats, () => { chartPaletteRef.value = chartPalette(chartTheme()); });

/** 横轴/饼图标签太长会把图挤扁，统一截断。 */
function chartLabel(s: string, max = 16): string {
  const t = s && s !== "—" ? s : "未知";
  return t.length > max ? t.slice(0, max - 1) + "…" : t;
}
function modelRequests(row: Row): number {
  return firstNum(row, ["requests", "request_count", "count", "total_requests"]) ?? 0;
}
function modelTokens(row: Row): number {
  return (firstNum(row, ["input_tokens", "prompt_tokens", "input"]) ?? 0)
    + (firstNum(row, ["output_tokens", "completion_tokens", "output"]) ?? 0);
}

/** 每个图表的空态判断：没有任何可用数字就不画，交给卡片里的空态文案。 */
const hasUsageRows = computed(() => byModel.value.length > 0);
const tokenTotal = computed(() => byModel.value.reduce((s, r) => s + modelTokens(r), 0));
const hasTokens = computed(() => tokenTotal.value > 0);

/** 横向条形图：按模型请求数排行（前 8 名），品牌橙。 */
const usageReqOption = computed<EChartsCoreOption>(() => {
  const t = chartTheme();
  const rows = byModel.value.slice().sort((a, b) => modelRequests(b) - modelRequests(a)).slice(0, 8).reverse();
  return {
    animationDuration: 320,
    grid: { left: 4, right: 28, top: 8, bottom: 4, containLabel: true },
    tooltip: {
      trigger: "axis", axisPointer: { type: "shadow" },
      backgroundColor: t.surface, borderColor: t.line, borderWidth: 1,
      textStyle: { color: t.ink, fontSize: 12, fontFamily: t.mono },
      formatter: (p: any) => {
        const it = Array.isArray(p) ? p[0] : p;
        return it.name + "<br/>请求 <b>" + it.value + "</b>";
      },
    },
    xAxis: {
      type: "value", minInterval: 1,
      axisLine: { show: false }, axisTick: { show: false },
      splitLine: { lineStyle: { color: t.line, type: "dashed" } },
      axisLabel: { color: t.ink3, fontSize: 11 },
    },
    yAxis: {
      type: "category",
      data: modelLabels(rows),
      axisLine: { lineStyle: { color: t.line } }, axisTick: { show: false },
      axisLabel: { color: t.ink2, fontSize: 11 },
    },
    series: [{
      type: "bar", barMaxWidth: 18,
      data: rows.map(modelRequests),
      itemStyle: { color: t.brand, borderRadius: [0, 4, 4, 0] },
      label: { show: true, position: "right", color: t.ink2, fontSize: 11, fontFamily: t.mono },
    }],
  };
});

/** 环形图：按模型 token 占比（输入 + 输出），第 6 名以后合并成「其他」。 */
const usageTokenOption = computed<EChartsCoreOption>(() => {
  const t = chartTheme();
  const rows = byModel.value.slice().sort((a, b) => modelTokens(b) - modelTokens(a));
  const labels = modelLabels(rows);
  const items = rows.slice(0, 6).map((r, i) => ({ name: labels[i], value: modelTokens(r) }));
  const rest = rows.slice(6);
  const restSum = rest.reduce((s, r) => s + modelTokens(r), 0);
  if (restSum > 0) items.push({ name: "其他 " + rest.length + " 个", value: restSum });
  return {
    animationDuration: 320,
    color: chartPaletteRef.value,
    tooltip: {
      trigger: "item",
      backgroundColor: t.surface, borderColor: t.line, borderWidth: 1,
      textStyle: { color: t.ink, fontSize: 12, fontFamily: t.mono },
      formatter: (p: any) => p.name + "<br/>" + fmtInt(p.value) + " tokens（" + p.percent + "%）",
    },
    legend: {
      type: "scroll", orient: "vertical", right: 0, top: "center",
      itemWidth: 8, itemHeight: 8, icon: "circle",
      textStyle: { color: t.ink2, fontSize: 11 },
    },
    series: [{
      type: "pie", radius: ["46%", "70%"], center: ["34%", "50%"],
      avoidLabelOverlap: true,
      itemStyle: { borderColor: t.surface, borderWidth: 2 },
      label: { show: false }, labelLine: { show: false },
      data: items,
    }],
  };
});

/** 堆叠柱状图：每个模型的输入/输出 tokens 构成。 */
const usageStackOption = computed<EChartsCoreOption>(() => {
  const t = chartTheme();
  const rows = byModel.value.slice()
    .sort((a, b) => modelTokens(b) - modelTokens(a))
    .slice(0, 8).reverse();
  const inp = rows.map(r => firstNum(r, ["input_tokens", "prompt_tokens", "input"]) ?? 0);
  const out = rows.map(r => firstNum(r, ["output_tokens", "completion_tokens", "output"]) ?? 0);
  return {
    animationDuration: 320,
    color: [t.brand, t.teal],
    grid: { left: 4, right: 28, top: 28, bottom: 4, containLabel: true },
    tooltip: {
      trigger: "axis", axisPointer: { type: "shadow" },
      backgroundColor: t.surface, borderColor: t.line, borderWidth: 1,
      textStyle: { color: t.ink, fontSize: 12, fontFamily: t.mono },
      formatter: (ps: any) => {
        const arr = Array.isArray(ps) ? ps : [ps];
        const total = arr.reduce((s: number, x: any) => s + Number(x.value || 0), 0);
        return arr[0].name + "<br/>" + arr.map((x: any) => x.marker + x.seriesName + " " + fmtInt(x.value)).join("<br/>")
          + "<br/>合计 <b>" + fmtInt(total) + "</b>";
      },
    },
    legend: {
      top: 0, right: 0, itemWidth: 8, itemHeight: 8, icon: "circle",
      textStyle: { color: t.ink2, fontSize: 11 },
    },
    xAxis: {
      type: "value", minInterval: 1,
      axisLine: { show: false }, axisTick: { show: false },
      splitLine: { lineStyle: { color: t.line, type: "dashed" } },
      axisLabel: { color: t.ink3, fontSize: 11 },
    },
    yAxis: {
      type: "category",
      data: modelLabels(rows),
      axisLine: { lineStyle: { color: t.line } }, axisTick: { show: false },
      axisLabel: { color: t.ink2, fontSize: 11 },
    },
    series: [
      { name: "输入", type: "bar", stack: "tok", barMaxWidth: 16, data: inp,
        itemStyle: { color: t.brand, borderRadius: [0, 0, 0, 0] } },
      { name: "输出", type: "bar", stack: "tok", barMaxWidth: 16, data: out,
        itemStyle: { color: t.teal, borderRadius: [0, 4, 4, 0] } },
    ],
  };
});

/* ---------------------------- 分组设置（内联编辑） ---------------------------- */
// 2026-10-13 起，分组自己的设置不再走「编辑分组」弹窗，而是在这一页直接改：
//  · 名称            —— 分组对客户端暴露的身份之一；
//  · 重试与超时      —— 这个分组自己的策略配置（跟随全局 ↔ 自有配置）。
// 2026-10-14「暴露模型」退出本卡：它不再是可填字段，而是由服务端按本分组的成员自动派生
// （`Config::normalize_exposed_models`），本页只读展示，见下方“暴露模型”那块。
// 改动先进草稿（sName / sRetry），点本卡右上「保存设置」才写回 state.cfg
// 并 markDirty()；真正的落库仍然由顶栏「保存」统一负责 —— 与其它页面同一套语义。
// 路由层级 / 成员 / 权重 不在本卡：「模型配置」卡（data-test="gd-models-card"）管那一半 ——
// 改的东西不一样（那边是路由表，还带拖动排序），所以两张卡各自有「保存 / 放弃」按钮。
// ★ 两张卡写回时都必须把对方那半原样带回：saveSettings() 带 tiers，saveTiers() 带 overrides。

const sName = ref("");
/** null = 跟随全局；有对象 = 使用这个分组自己的配置（判据只有 overrides.retry 是否存在）。 */
const sRetry = ref<RetryConfig | null>(null);

/** 从当前分组把两份草稿重新灌一遍（进入页面、外部换配置、放弃修改时都走它）。 */
function syncSettings() {
  const g = props.group;
  sName.value = g?.name ?? "";
  sRetry.value = g?.overrides?.retry ? normRetry(g.overrides.retry) : null;
}
syncSettings();
// 分组对象被换掉（顶栏「重新加载配置」、保存成功）时重新灌草稿，别拿着上一份的残留。
watch(() => props.group, syncSettings);

const sNameError = computed(() => (sName.value.trim() ? "" : "请填写分组名。"));

/**
 * 暴露模型这一块的说明行：把「从哪来、为什么是空的」直接说出来。
 * 之前这里有一句「至少填写一个暴露模型名」，现在没有可填的字段，改成陈述事实。
 */
const exposedNote = computed(() =>
  exposedModels.value.length
    ? "由下面的路由层级自动汇总，不需要手动维护。"
    : "当前没有任何成员提供模型名，客户端用本分组还请求不到东西 —— 到「模型配置」里加成员即可。",
);

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

/**
 * 「切回跟随全局」时把改动过的那份自己的配置存起来的地方。
 *
 * 2026-10-15 用户要求：自己配置 → 改动 → 切回跟随全局 → 再切回自己配置，
 * 之前改的数字必须还在（原来 followGlobal() 直接写 null，那份草稿就没了）。
 * 所以切回跟随全局前先把它抄进这里，下次 useOwn() 优先从这里恢复。
 *
 * 只活在本次组件实例里（分组详情页按 nav.groupId 挂载，换分组即重建），
 * 且只有「放弃修改」（resetSettings）会显式清它 —— 那是用户明说不要这份改动了。
 * syncSettings() 故意**不**清：它也会在本卡自己保存后被触发，若在那里清，
 * 「跟随全局 → 保存 → 再切回自己配置」正好把要保住的改动丢掉。
 * 它**不是**配置的一部分：不写进 state.cfg，不参与 markDirty()，落库的始终只有
 * sRetry 本身（null = 跟随全局）。
 */
const sRetryStash = ref<RetryConfig | null>(null);

/** 切到「使用自己的配置」：优先恢复上次留的存档，没有才从当前生效值克隆一份。 */
function useOwn() {
  if (sRetry.value) return;
  sRetry.value = sRetryStash.value ? normRetry(sRetryStash.value) : normRetry(effRetry());
}
/** 切回「跟随全局配置」：先把这份改动存档，再写 null（绝不去复制全局值）。 */
function followGlobal() {
  if (!sRetry.value) return;
  sRetryStash.value = normRetry(sRetry.value);
  sRetry.value = null;
}
/** 把当前全局配置整份复制到这个分组，之后它就和全局解耦、可以单独改了。 */
function copyGlobal() {
  sRetry.value = normRetry(globalRetry());
}
/** 丢弃存档：只在「放弃修改」调用（别挪进 syncSettings，理由见上面的说明）。 */
function dropRetryStash() {
  sRetryStash.value = null;
}

/* ---------------------- 重试档位（与设置页同一份定义） ---------------------- */
// 定义在 ../retry-presets.ts，全局设置页与本卡共用 —— 同一个档位名在两处必须是同一组数字。
const retryPresetChoice = computed({
  get: () => matchRetryPreset(sRetry.value),
  set: (v: string) => applyRetryPreset(v),
});
const retryPresetOpts = computed(() => retryPresetOptions());

/** 套用一个档位：只覆盖档位包含的那些数字字段，然后照常走本卡的「保存设置」。 */
function applyRetryPreset(v: string) {
  const p = RETRY_PRESETS.find(x => x.value === v);
  if (!p) return; // custom 只是状态，不写值
  if (!sRetry.value) return; // 跟随全局时没有可套用的对象（按钮也只在自己的配置里出现）
  Object.assign(sRetry.value, p.patch);
  toast.ok(`已套用「${p.label}」档位，记得点本卡「保存设置」`);
}

/** 有存档时的提示语：告诉用户上次改的那份还在，切回去就能拿回来。 */
const retryStashNote = computed(() =>
  sRetryStash.value
    ? `上次「使用自己的配置」改过的 ${sRetryStash.value.max_attempts} 次 / 总超时 ${fmtSec(sRetryStash.value.total_deadline_ms)} 还留着，切回去就能接着改。`
    : "",
);

/** 该分组自己的配置里的预算校验 —— 与设置页「重试与超时」同一套公式。 */
const sBudget = computed(() => {
  const r = sRetry.value;
  if (!r) return { ok: true, line: "" };
  const worst = Number(r.max_attempts) * Number(r.attempt_deadline_ms) + Number(r.reserve_ms);
  return {
    ok: worst <= Number(r.total_deadline_ms),
    line: `${r.max_attempts} × ${fmtSec(r.attempt_deadline_ms)} + ${fmtSec(r.reserve_ms)} = ${fmtSec(worst)}`,
  };
});

/** 跟随全局时的一句话摘要：用实际生效值，和生效的模型名一样能直接看。 */
const followLine = computed(() => {
  const r = effRetry();
  return `跟随全局（当前：max_attempts=${r.max_attempts} / 总超时 ${fmtSec(r.total_deadline_ms)} / 死循环拦截 ${r.loop_guard_enabled ? "开" : "关"}）`;
});

/**
 * 有没有未保存的设置改动（顶栏之外的第二处脏标记，只描述本卡）。
 * 2026-10-14：暴露模型不再是本卡的字段（服务端自动派生），所以本卡只剩两项 —— 名称与重试配置；
 * 它也不再出现在脏判定里，否则「改了层级导致派生列表变了」会误报成「分组设置有改动」。
 */
const sDirty = computed(() => {
  const g = props.group;
  if (!g) return false;
  const nextRetry = sRetry.value ? normRetry(sRetry.value) : null;
  const curRetry = g.overrides?.retry ?? null;
  return sName.value.trim() !== (g.name ?? "")
    || JSON.stringify(nextRetry) !== JSON.stringify(curRetry);
});

const sValid = computed(() => !sNameError.value && sBudget.value.ok);

/** 写回配置（只进内存 + markDirty，落库交给顶栏「保存」）。 */
function saveSettings() {
  const cfg = state.cfg;
  const g = props.group;
  if (!cfg || !g || !sValid.value) return;
  const clean: Group = {
    id: g.id,
    name: sName.value.trim(),
    // ★ 暴露模型是服务端按成员派生的，不是本卡能填的东西：原样带回当前值即可。
    //   即使这里带的是旧的、过期的（比如同一份配置里层级刚被改过），
    //   PUT /admin/api/config 也会在入库前用成员重算一遍 —— 服务端才是那个不变量。
    exposed_models: [...(g.exposed_models ?? [])],
    // 层级归下面的「模型配置」卡管，这里原样深拷贝带回去，免得把那张卡改好的路由表抹掉。
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

/**
 * 放弃本卡的改动：从当前分组重新灌一遍草稿。
 * 这里要**显式**丢掉存档 —— 放弃是用户明说「不要这份改动了」，
 * 若还留着，下一次点「使用自己的配置」又把它捞回来，等于没放弃。
 * （注意 syncSettings() 本身不清：它也会在本卡自己保存后被触发，
 *   那种情况存档得留着，否则「跟随全局 → 保存 → 再切回自己配置」会丢改动。）
 */
function resetSettings() {
  syncSettings();
  dropRetryStash();
  toast.ok("已放弃未保存的分组设置改动");
}

/* ==================== 模型配置（路由层级 / 成员 / 权重，内联编辑） ==================== */
// 2026-10-14 从原本的独立「模型配置」页（原 client/src/pages/GroupModels.vue，已删除）搬进来，逻辑一字未改，
// 只是从「整页」变成「分组详情页里的一张卡」：这样层级 / 成员 / 权重不用跳页就能改。
// 编辑的是一份草稿（进入时深拷贝 props.group），只有点本卡的「保存模型配置」才写回 state.cfg
// 并 markDirty()；「放弃修改」把草稿重新克隆一遍。落库仍然由顶栏「保存」统一负责。
//
// ★ 本卡与「分组设置」卡各管一半字段，写回时必须原样带回对方那半：
//   本卡的 saveTiers() 带回 overrides（组级策略配置）；saveSettings() 带回 tiers。
//   任何一侧漏字段，都会把另一侧静默抹成默认值。
// ★ 与「分组设置」草稿重名的东西一律加 tier 前缀（tierDirty / tierValid），避免遮蔽。

function clone(g: Group): Group {
  const d = JSON.parse(JSON.stringify(g)) as Group;
  if (!d.tiers || !d.tiers.length) d.tiers = [{ level: 1, members: [] }];
  return d;
}

/**
 * 草稿：进入本页时把分组深拷贝一份，之后所有编辑都发生在这份副本上。
 * 之所以不直接改 props.group：用户改了半天层级又不想保存时，「放弃修改」必须能一键还原。
 */
const draft = ref<Group>(clone(props.group));

/** 分组被换掉（顶栏「重新加载配置」、保存成功）时重新克隆，别拿着旧草稿。 */
watch(() => props.group, (g) => { draft.value = clone(g); });

/** 层级与成员的有序签名：只比较「改了什么」，用来判断有没有未保存的改动。 */
function tierSig(g: Group): string {
  return JSON.stringify((g.tiers ?? []).map(t => (t.members ?? []).map(m => [m.model_id, Number(m.weight) || 0])));
}
const tierDirty = computed(() => tierSig(draft.value) !== tierSig(props.group));

function resetDraft() {
  draft.value = clone(props.group);
  toast.ok("已放弃未保存的模型配置改动");
}

/* 选模型下拉：每一行都是「渠道-模型」（bindingLabel），
   hint 放上游名 —— 两个渠道暴露同名模型时（例如都叫 gpt-4o），
   行内仍然能看出渠道与真实上游，搜索框按 label/hint/group 过滤。 */
const modelOptions = computed(() =>
  models.value.map(m => ({
    value: m.id,
    label: bindingLabel(m.id),
    hint: m.upstream,
    group: channelName(m.channel_id),
  })),
);

function newMember(modelId: string, weight = 1): TierMember {
  return { model_id: modelId, weight };
}
/** 优先挑该层还没用过的模型，避免一上来就撞重复。 */
function firstFreeModelId(tier: Tier): string {
  const used = new Set(tier.members.map(m => m.model_id));
  return models.value.find(m => !used.has(m.id))?.id ?? models.value[0]?.id ?? "";
}

/* --------------------------- 层级编辑器交互 --------------------------- */
function addMember(tier: Tier) {
  tier.members.push(newMember(firstFreeModelId(tier)));
}
function addTier() {
  const d = draft.value;
  if (!d) return;
  const t: Tier = { level: d.tiers.length + 1, members: [] };
  if (models.value.length) t.members.push(newMember(firstFreeModelId(t)));
  d.tiers.push(t);
}
function removeTier(index: number) {
  const d = draft.value;
  if (!d || d.tiers.length <= 1) return;
  d.tiers.splice(index, 1);
  d.tiers.forEach((t, i) => { t.level = i + 1; });
}
/** 同级内上移 / 下移一位：直接调换数组顺序，界面顺序即保存顺序。 */
function moveMember(tier: Tier, index: number, delta: number) {
  const to = index + delta;
  if (to < 0 || to >= tier.members.length) return;
  const [m] = tier.members.splice(index, 1);
  if (!m) return;
  tier.members.splice(to, 0, m);
}
/** 删除成员：直接改草稿，改动照样只进内存，点「保存模型配置」才生效。 */
function removeMemberAt(tier: Tier, index: number) {
  const m = tier.members[index];
  if (!m) return;
  tier.members.splice(index, 1);
  toast.ok(`已从${tierName(tier.level)}移除「${bindingLabel(m.model_id)}」（点「保存模型配置」后生效）`);
}

/**
 * 拖动排序：用 Pointer Events 自己实现，不引第三方拖拽库（拖拽库动辄几十 KB，
 * 而这里只需要「同级内换顺序」「层级换顺序」两件事）。关键取舍：
 *   - setPointerCapture 之后 move / up 都会回到抓手元素上，鼠标划出卡片也不会丢事件；
 *   - 拖动期间不改数组顺序（DOM 顺序冻结），开始时量一次行几何，之后只更新「落点槽位」
 *     和跟手位移 —— 被拖行的原始位置始终是量到的那份，不会因为自己挪动而抖动；
 *   - 松手才落位：槽位 0 = 插到第 1 行之前，N = 插到最后一行之后；
 *   - 不用 HTML5 draggable：它的拖影、dragend 时机各内核差异大，触控板 / 手指体验也差。
 * 按钮保留：拖拽是鼠标增强，键盘与读屏用户仍然靠上移 / 下移。
 */
type DragKind = "member" | "tier";

const dragKind = ref<DragKind | null>(null);
/** 成员拖动：所属层；层级拖动：被拖的那一层。两种情况都用来高亮整块。 */
const dragTier = ref<Tier | null>(null);
/** 正在被拖动的成员对象本身（按身份跟踪，比 index 更稳）。 */
const dragMember = ref<TierMember | null>(null);
/** 层级拖动时被拖层的下标。 */
const dragTierFrom = ref(-1);
/** 落点槽位：0 = 插到第 1 行之前，长度 = 插到最后一行之后。 */
const dropIndex = ref(-1);
/** 按下时的鼠标 Y、被拖元素的跟手纵向位移。 */
const dragStartY = ref(0);
const dragDelta = ref(0);
/** 拖动开始时量好的几何 —— 拖动期间不改数组，DOM 顺序不变，所以量一次就够。 */
const dragRects = ref<{ top: number; height: number }[]>([]);
/** 定位容器 padding box 的顶边（落点指示线用它换算相对坐标）。 */
const dragPadTop = ref(0);
const dragLineTop = ref(0);

const dragging = computed(() => dragKind.value !== null);

/** 拖动期间给 body 挂一个类：锁住文本选中与光标形状，松手就摘掉。 */
watch(dragging, (on) => {
  document.body.classList.toggle("gw-dragging", on);
});

/** 被拖元素跟着鼠标走：位移 = 鼠标纵向增量（布局冻结，所以不需要自我修正）。 */
const dragStyle = computed(() => (
  dragging.value ? { transform: `translateY(${dragDelta.value}px)` } : undefined
));

/** 鼠标纵向落在哪个槽位：比每一行的中垂线，越过中线就算落到下一格。 */
function slotAt(clientY: number): number {
  const rects = dragRects.value;
  for (let i = 0; i < rects.length; i++) {
    const r = rects[i]!;
    if (clientY < r.top + r.height / 2) return i;
  }
  return rects.length;
}

/** 指示线位置：插在某行之前画在它上方，插到末尾画在最后一行下方。 */
function lineTopFor(slot: number, rects: { top: number; height: number }[], padTop: number): number {
  if (!rects.length) return 0;
  if (slot >= rects.length) {
    const last = rects[rects.length - 1]!;
    return Math.max(0, last.top + last.height - padTop + 3);
  }
  return Math.max(0, rects[slot]!.top - padTop - 5);
}

function endDrag() {
  dragKind.value = null;
  dragTier.value = null;
  dragMember.value = null;
  dragTierFrom.value = -1;
  dropIndex.value = -1;
  dragDelta.value = 0;
  dragRects.value = [];
}

/** 量一次几何并进入拖动状态（成员 / 层级两种模式共用）。 */
function startDrag(kind: DragKind, box: HTMLElement, rowSel: string, clientY: number) {
  const padTop = box.getBoundingClientRect().top + box.clientTop;
  const rects = Array.from(box.querySelectorAll<HTMLElement>(rowSel))
    .map((el) => { const r = el.getBoundingClientRect(); return { top: r.top, height: r.height }; });
  dragKind.value = kind;
  dragStartY.value = clientY;
  dragDelta.value = 0;
  dragRects.value = rects;
  dragPadTop.value = padTop;
  dropIndex.value = slotAt(clientY);
  dragLineTop.value = lineTopFor(dropIndex.value, rects, padTop);
}

/** 松手落位：摘下来再插进落点槽位；越过原位置时槽位要左移一位（自己已被摘掉）。 */
function commitDrag() {
  const kind = dragKind.value;
  const slot = dropIndex.value;
  const d = draft.value;
  if (kind === "member") {
    const tier = dragTier.value;
    const m = dragMember.value;
    if (tier && m && slot >= 0) {
      const from = tier.members.indexOf(m);
      if (from >= 0) {
        tier.members.splice(from, 1);
        tier.members.splice(slot > from ? slot - 1 : slot, 0, m);
      }
    }
  } else if (kind === "tier" && d && slot >= 0) {
    const from = dragTierFrom.value;
    if (from >= 0 && from < d.tiers.length) {
      const [t] = d.tiers.splice(from, 1);
      if (t) {
        d.tiers.splice(slot > from ? slot - 1 : slot, 0, t);
        // 层级号跟位置绑定：一级 → 二级 → … 永远等于从上到下的顺序
        d.tiers.forEach((x, i) => { x.level = i + 1; });
      }
    }
  }
  endDrag();
}

function onDragMove(e: PointerEvent) {
  if (!dragging.value) return;
  dragDelta.value = e.clientY - dragStartY.value;
  const slot = slotAt(e.clientY);
  if (slot !== dropIndex.value) {
    dropIndex.value = slot;
    dragLineTop.value = lineTopFor(slot, dragRects.value, dragPadTop.value);
  }
}

/** 拖动中给层容器补样式：成员拖动只要定位，层级拖动还要抬高、描边、压暗。 */
function tierDragClass(t: Tier): string {
  if (dragKind.value === "tier") {
    return dragTier.value === t ? "relative z-20 opacity-70 ring-2 ring-brand-500" : "relative";
  }
  return dragKind.value === "member" && dragTier.value === t ? "relative" : "";
}

/** 抓手：成员行内拖动（同级内换顺序）。 */
function onMemberDragStart(e: PointerEvent, tier: Tier, index: number) {
  if (e.button !== 0 || tier.members.length < 2) return; // 只接左键；只有一个成员时没什么可排的
  e.preventDefault();
  const m = tier.members[index];
  const handle = e.currentTarget as HTMLElement | null;
  const box = handle?.closest("[data-test=member-tier]") as HTMLElement | null;
  if (!m || !box) return;
  dragTier.value = tier;
  dragMember.value = m;
  startDrag("member", box, "[data-test=member-row]", e.clientY);
  // 捕获后 move / up 都会回到抓手，鼠标划出窗口也不丢事件；失败也不影响 window 上的监听
  try { handle?.setPointerCapture(e.pointerId); } catch (err) { /* 忽略 */ }
}

/** 抓手：层头拖动（层级先后，也就是 一级 → 二级 的尝试顺序）。 */
function onTierDragStart(e: PointerEvent, ti: number) {
  const d = draft.value;
  if (e.button !== 0 || !d || d.tiers.length < 2) return;
  e.preventDefault();
  const t = d.tiers[ti];
  const handle = e.currentTarget as HTMLElement | null;
  const box = handle?.closest("[data-test=tier-list]") as HTMLElement | null;
  if (!t || !box) return;
  dragTier.value = t;
  dragTierFrom.value = ti;
  startDrag("tier", box, "[data-test=member-tier]", e.clientY);
  try { handle?.setPointerCapture(e.pointerId); } catch (err) { /* 忽略 */ }
}

/** Esc 放弃本次拖动（不改顺序）。★ onKey 里靠 dragging 兜住，避免同一次 Esc 把整页也关掉。 */
function onDragKey(e: KeyboardEvent) {
  if (e.key === "Escape" && dragging.value) endDrag();
}

const emptyTierError = computed(() => {
  const i = draft.value.tiers.findIndex(t => t.members.length === 0);
  return i >= 0 ? `${tierName(i + 1)}还没有成员，每一级至少需要一个模型。` : "";
});
const dupErrors = computed<string[]>(() => {
  const out: string[] = [];
  draft.value.tiers.forEach((t, i) => {
    const seen = new Set<string>();
    for (const m of t.members) {
      if (!m.model_id) {
        out.push(`${tierName(i + 1)}有成员还没有选择模型。`);
        break;
      }
      if (seen.has(m.model_id)) {
        out.push(`${tierName(i + 1)}内重复选择了「${bindingLabel(m.model_id)}」。`);
        break;
      }
      seen.add(m.model_id);
    }
  });
  return out;
});
const tierValid = computed(() => !emptyTierError.value && dupErrors.value.length === 0);
/** 卡脚那句话：把「哪里不对」直接说出来，而不是只把按钮灰掉。 */
const blockReason = computed(() => emptyTierError.value || dupErrors.value[0] || "");

/* -------------------------------- 写回 -------------------------------- */

function saveTiers() {
  if (!tierValid.value) return;
  const cfg = state.cfg;
  const g = props.group;
  if (!cfg) return;
  const clean: Group = {
    id: g.id,
    name: g.name,
    exposed_models: [...(g.exposed_models ?? [])],
    tiers: draft.value.tiers.map((t, i) => ({
      level: i + 1,
      members: t.members.map(m => ({
        model_id: m.model_id,
        weight: Number(m.weight) || 0,
      })),
    })),
    // ★ overrides 必须原样带回去：本卡不改组级策略配置，但保存时若丢掉这个字段，
    //   服务端会把它当成「跟随全局」（None 序列化成 "retry": null），
    //   等于把用户在同一页「分组设置」卡里配好的组级配置静默抹掉。
    overrides: g.overrides ? JSON.parse(JSON.stringify(g.overrides)) : { retry: null },
  };
  const index = cfg.groups.findIndex(x => x.id === g.id);
  if (index >= 0) cfg.groups.splice(index, 1, clean);
  else cfg.groups.push(clean);
  markDirty();
  draft.value = clone(clean);
  toast.ok(`已更新「${clean.name || clean.id}」的模型配置`);
}
</script>

<!--
  拖动排序的全局兜底：抓手在「模型配置」卡里，但指针会跑到 window 上。
  拖住时锁住整页文本选区与光标（否则划过文字会选中一片、光标变成 I 形），
  user-select:none 对 WebView2 里的子元素同样生效，松手立刻恢复。
-->
<style scoped>
:global(body.gw-dragging) {
  cursor: grabbing;
  user-select: none;
}
</style>

<template>
  <div class="flex h-full min-h-0 flex-col gap-3.5 px-5 py-4" data-test="group-dashboard">
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
    <div class="-mx-5 flex shrink-0 flex-wrap items-center gap-2 border-b border-line bg-app px-5">
      <button
        v-for="t in tabs" :key="t.key" type="button"
        :data-test="'group-dashboard-tab-' + t.key"
        class="-mb-px border-b-2 px-3 py-2 text-[13px] transition-colors"
        :class="tab === t.key
          ? 'border-brand-500 font-medium text-ink'
          : 'border-transparent text-ink-3 hover:text-ink-2'"
        @click="tab = t.key"
      >{{ t.label }}</button>
      <!-- 时间范围只影响「用量 / 请求日志 / 切换日志」三个页签，所以只在它们上面出现：
           冷却状态是「此刻正在冷却」的实时快照，没有历史时间戳可筛，摆出来会骗人。
           data-test 挂包裹 div：attrs 在 reka-ui 的 SelectRoot（inheritAttrs:false）处会被丢掉。 -->
      <div
        v-if="tab === 'usage' || tab === 'request' || tab === 'switch'"
        class="ml-auto w-[110px]" data-test="gd-range-filter"
      >
        <TRange v-model="days" />
      </div>
      <div class="text-[12px] text-ink-3" :class="tab === 'overview' || tab === 'cooldown' ? 'ml-auto' : ''" data-test="gd-tab-counts">
        <!-- 这两个数是被时间范围筛过的（服务端 total 就是筛选命中数），而概览/冷却两个页签上
             看不到范围选择器 —— 不注明范围的话「请求日志 0 条」会被读成「这个分组从没请求过」。
             所以范围写在文案里，任何页签上都自解释。 -->
        请求日志 {{ requestTotal }} 条 · 切换日志 {{ switchTotal }} 条（最近 {{ days }} 天）
        <span v-if="lastAt" class="ml-1 font-mono text-[11.5px]">· 最后更新 {{ fmtClock(lastAt) }}</span>
      </div>
    </div>

    <Alert v-if="err" tone="err" class="shrink-0">加载失败：{{ err }}</Alert>

    <div v-if="loading && !stats" class="flex flex-1 items-center justify-center gap-2 py-14 text-[13px] text-ink-3">
      <Spinner :size="16" /> 正在加载…
    </div>

    <template v-else>
      <!-- ------------------------------ 概览 ------------------------------ -->
      <div v-if="tab === 'overview'" class="min-h-0 flex-1 space-y-3.5 overflow-y-auto">
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

            <!-- 暴露模型：只读。服务端按本分组的成员自动派生（Config::normalize_exposed_models），
                 这里只是把它显示出来，没有可填的字段。 -->
            <div data-test="group-exposed-readonly">
              <div class="mb-1.5 flex items-baseline gap-2">
                <span class="text-[12px] font-medium text-ink-2">暴露模型</span>
                <span class="text-[11.5px] text-ink-3">自动维护 · {{ exposedModels.length }} 个</span>
              </div>
              <div v-if="exposedModels.length" class="flex flex-wrap gap-1.5 rounded-lg border border-line-soft bg-app px-2.5 py-2">
                <Badge v-for="m in exposedModels" :key="m" tone="teal">
                  <span class="font-mono">{{ m }}</span>
                </Badge>
              </div>
              <div v-else class="rounded-lg border border-line-soft bg-app px-2.5 py-2 text-[12px] text-ink-3">
                暂无
              </div>
              <p class="mt-1.5 text-[11.5px] leading-4 text-ink-3" data-test="group-exposed-auto-note">
                {{ exposedNote }}
              </p>
            </div>

            <Alert v-if="sNameError" tone="err" data-test="group-settings-error">
              {{ sNameError }}
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
                这个分组可以跟随全局配置，也可以拥有自己的一份；有自己的配置时以它为准，全局改动不再影响它。时长一律按「秒」填写与显示。
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

          <div v-if="!sRetry" class="space-y-2">
            <p
              class="rounded-lg border border-line-soft bg-app/40 px-3 py-2 text-[12px] leading-5 text-ink-2"
              data-test="group-retry-following"
            >
              {{ followLine }}
            </p>
            <!-- 切回跟随全局时，改动过的那份没有被丢掉，这里明说一声，免得以为白改了。 -->
            <p
              v-if="retryStashNote"
              class="flex items-start gap-1.5 rounded-lg border border-brand-200 bg-brand-50/60 px-3 py-2 text-[11.5px] leading-4 text-ink-2"
              data-test="group-retry-stash-note"
            >
              <Info :size="13" class="mt-px shrink-0 text-brand-500" />
              <span>{{ retryStashNote }}</span>
            </p>
          </div>

          <div v-else>
            <div class="mb-2.5 flex flex-wrap items-center gap-x-4 gap-y-2">
              <span class="text-[11.5px] font-medium uppercase tracking-wide text-ink-3">独立配置</span>
              <!-- 档位：与设置页同一份定义（../retry-presets.ts），套用后仍可逐项微调 -->
              <div class="flex items-center gap-2">
                <span class="text-[12px] text-ink-3">档位</span>
                <div class="w-[176px]" data-test="group-retry-preset">
                  <TSelect
                    v-model="retryPresetChoice"
                    size="sm"
                    searchable
                    placeholder="选择档位"
                    :options="retryPresetOpts"
                    empty-text="无档位"
                  />
                </div>
              </div>
              <Btn
                size="sm" variant="subtle" class="ml-auto"
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
                <Field label="总预算 (秒)" hint="整个请求的总时间上限，超过即直接失败">
                  <SecInput v-model="sRetry.total_deadline_ms" data-test="group-retry-total-deadline-ms" @input="markDirty()" />
                </Field>
                <Field label="单次尝试上限 (秒)" hint="一次尝试的最长耗时，超时即判定失败并降级">
                  <SecInput v-model="sRetry.attempt_deadline_ms" data-test="group-retry-attempt-deadline-ms" @input="markDirty()" />
                </Field>
                <Field label="连接超时 (秒)" hint="与上游建立 TCP / TLS 连接的超时">
                  <SecInput v-model="sRetry.connect_timeout_ms" data-test="group-retry-connect-timeout-ms" @input="markDirty()" />
                </Field>
                <Field label="首字节超时 (秒)" hint="发出请求到收到响应首字节的超时">
                  <SecInput v-model="sRetry.ttfb_timeout_ms" data-test="group-retry-ttfb-timeout-ms" @input="markDirty()" />
                </Field>
                <Field label="首个事件超时 (秒)" hint="流式响应中第一个事件到达的超时">
                  <SecInput v-model="sRetry.first_event_timeout_ms" data-test="group-retry-first-event-timeout-ms" @input="markDirty()" />
                </Field>
                <Field label="流空闲超时 (秒)" hint="流式响应里两个数据块之间的最大空闲时间">
                  <SecInput v-model="sRetry.chunk_idle_timeout_ms" data-test="group-retry-chunk-idle-timeout-ms" @input="markDirty()" />
                </Field>
                <Field label="预留余量 (秒)" hint="给收尾/补帧留的时间">
                  <SecInput v-model="sRetry.reserve_ms" data-test="group-retry-reserve-ms" @input="markDirty()" />
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
                <Field label="首字节等待上限 (毫秒)" hint="检测起点是字符数，要等多久取决于模型出字速度。这里给它一个时间上限，到点先提交，后续内容交给已提交后的判定。0 = 不设上限">
                  <div data-test="group-retry-loop-guard-hold-max-ms">
                    <TInput v-model.number="sRetry.loop_guard_hold_max_ms" type="number" min="0" data-test="group-retry-loop-guard-hold-max-ms" />
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
                <Field
                  label="已提交后的判定尾部 (字符)"
                  hint="回答已开始发给客户端之后，只看末尾这么多字符判断是否复读。此时无法重试，只能就地截断收尾，所以它也决定「中段复读最多还能泄漏多少字」"
                >
                  <div data-test="group-retry-loop-guard-tail-chars">
                    <TInput v-model.number="sRetry.loop_guard_tail_chars" type="number" min="16" data-test="group-retry-loop-guard-tail-chars" />
                  </div>
                </Field>
              </div>
            </div>

            <Alert v-if="!sBudget.ok" tone="err" class="mt-3" data-test="group-retry-invalid">
              <Info :size="13" class="mt-0.5 shrink-0" />
              <span>
                预算不成立：{{ sBudget.line }} &gt; 总预算 {{ fmtSec(sRetry.total_deadline_ms) }}，
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

        <!-- ------------------ 模型配置（路由层级 / 成员 / 权重，内联编辑） ------------------ -->
        <!-- 2026-10-14：原「模型配置」子页并进这里，层级 / 成员 / 权重直接在当前页面改。 -->
        <Card :pad="false" class="overflow-hidden" data-test="gd-models-card">
          <div class="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
            <Waypoints :size="15" class="text-ink-3" />
            <h2 class="text-[13.5px] font-semibold text-ink">模型配置</h2>
            <Badge v-if="tierDirty" tone="warn" data-test="gd-models-dirty">有未保存的改动</Badge>
            <span class="ml-auto flex flex-col items-end gap-0.5 text-[11.5px] text-ink-3">
              <span data-test="tier-hint">
                <template v-if="autoWeight">按 一级 → 二级 → 三级 顺序尝试，同级内按权重降序分配</template>
                <template v-else>按 一级 → 二级 → 三级 顺序尝试，同级内按列表顺序依次尝试（自动权重已关闭）</template>
              </span>
              <span data-test="weight-hint">
                <template v-if="autoWeight">权重可直接改；并列时按列表顺序</template>
                <template v-else>权重可先填好，开启「自动权重」后生效</template>
              </span>
              <span data-test="drag-hint">按住行首或层头的抓手可拖动排序</span>
            </span>
            <div class="flex shrink-0 items-center gap-0.5">
              <Btn size="sm" variant="ghost" data-test="gm-reset" :disabled="!tierDirty" @click="resetDraft">
                <RefreshCw :size="13" />放弃修改
              </Btn>
              <Btn
                size="sm" variant="primary" data-test="gm-save"
                :disabled="!tierValid || !tierDirty" @click="saveTiers"
              >保存模型配置</Btn>
            </div>
          </div>

          <Alert v-if="!models.length" tone="warn">
            还没有模型绑定，请先到「渠道与模型」页在某个渠道下添加 exposed → upstream 映射，再回来配置层级。
          </Alert>

          <div class="px-4 py-3.5">
            <div class="relative space-y-2.5" data-test="tier-list">
              <!-- 层级拖动的落点指示线：画在层与层之间 -->
              <div
                v-if="dragKind === 'tier' && dropIndex >= 0"
                class="pointer-events-none absolute inset-x-0 z-30 h-[3px] rounded-full bg-brand-500"
                :style="{ top: dragLineTop + 'px' }"
                data-test="drop-line"
              />
              <div
                v-for="(t, ti) in draft.tiers"
                :key="ti"
                class="rounded-[10px] border border-line bg-app/60 p-3"
                :class="tierDragClass(t)"
                :style="dragKind === 'tier' && dragTier === t ? dragStyle : undefined"
                data-test="member-tier"
              >
                <!-- 成员拖动的落点指示线：画在行与行之间 -->
                <div
                  v-if="dragKind === 'member' && dragTier === t && dropIndex >= 0"
                  class="pointer-events-none absolute inset-x-0 z-30 h-[3px] rounded-full bg-brand-500"
                  :style="{ top: dragLineTop + 'px' }"
                  data-test="drop-line"
                />
                <div class="mb-2.5 flex items-center justify-between gap-2">
                  <div class="flex items-center gap-2">
                    <button
                      type="button"
                      class="flex h-7 w-4 shrink-0 cursor-grab touch-none items-center justify-center rounded-md text-ink-3 transition-colors hover:bg-app hover:text-ink active:cursor-grabbing disabled:pointer-events-none disabled:opacity-40"
                      :disabled="draft.tiers.length < 2"
                      data-test="tier-drag"
                      :title="draft.tiers.length < 2
                        ? '只有一个层级，无需排序'
                        : '按住拖动可调整层级先后（也就是 一级 → 二级 的尝试顺序）'"
                      @pointerdown="onTierDragStart($event, ti)"
                    >
                      <GripVertical :size="14" />
                    </button>
                    <Badge :tone="tierInfo(ti + 1).tone">{{ tierInfo(ti + 1).label }}</Badge>
                    <span class="text-[11.5px] text-ink-3">{{ t.members.length }} 个成员</span>
                  </div>
                  <div class="flex shrink-0 items-center gap-0.5">
                    <Btn size="sm" variant="ghost" :disabled="!models.length" @click="addMember(t)">
                      <Plus :size="12" />添加成员
                    </Btn>
                    <IconBtn
                      title="删除该级"
                      danger
                      :disabled="draft.tiers.length <= 1"
                      @click="removeTier(ti)"
                    >
                      <Trash2 :size="15" />
                    </IconBtn>
                  </div>
                </div>

                <div v-if="t.members.length" class="space-y-2">
                  <div
                    v-for="(m, mi) in t.members"
                    :key="mi"
                    class="flex items-center gap-2 rounded-[10px] border px-2.5 py-1.5"
                    :class="dragMember === m
                      ? 'relative z-20 border-brand-200 bg-brand-50 shadow-lg'
                      : (isMemberInUse(t.level, m.model_id)
                        ? 'relative border-teal-400 bg-teal-100 ring-2 ring-teal-500/35'
                        : 'relative border-line bg-surface')"
                    :style="dragMember === m ? dragStyle : undefined"
                    data-test="member-row"
                    :data-model="m.model_id"
                    :data-in-use="isMemberInUse(t.level, m.model_id) ? '1' : '0'"
                  >
                    <button
                      type="button"
                      class="flex h-7 w-4 shrink-0 cursor-grab touch-none items-center justify-center rounded-md text-ink-3 transition-colors hover:bg-app hover:text-ink active:cursor-grabbing disabled:pointer-events-none disabled:opacity-40"
                      :disabled="t.members.length < 2"
                      data-test="member-drag"
                      :title="t.members.length < 2
                        ? '同级只有一个成员，无需排序'
                        : '按住拖动可调整同级顺序'"
                      @pointerdown="onMemberDragStart($event, t, mi)"
                    >
                      <GripVertical :size="14" />
                    </button>
                    <div class="min-w-0 flex-1" data-test="member-row-select">
                      <TSelect
                        v-model="m.model_id"
                        size="sm"
                        searchable
                        :options="modelOptions"
                        :placeholder="models.length ? '搜索并选择渠道-模型' : '请先到「渠道与模型」页添加绑定'"
                      />
                      <!-- 同名模型跨渠道时最容易被误选，这里就地提示 -->
                      <p
                        v-if="m.model_id && t.members.filter(x => x.model_id === m.model_id).length > 1"
                        class="mt-1 text-[11px] leading-4 text-warn-700"
                      >
                        该模型在本层被选了多次，请换一个。
                      </p>
                    </div>
                    <!-- 顺序号：自动权重关闭时它就是真实尝试次序；开启时仅作权重并列的兜底次序 -->
                    <span
                      v-if="!autoWeight"
                      class="shrink-0 rounded-md border border-line bg-app px-1.5 py-0.5 font-mono text-[11.5px] text-ink-2"
                      data-test="member-order"
                      :title="`同级第 ${mi + 1} 个尝试`"
                    >{{ mi + 1 }}</span>
                    <!-- 权重：两种模式都可手动改；关闭时该值不参与调度，开启后立即按它降序 -->
                    <span class="shrink-0 text-[11.5px] text-ink-3">权重</span>
                    <div class="w-20 shrink-0" data-test="member-weight">
                      <TInput
                        v-model.number="m.weight"
                        type="number"
                        min="0"
                        step="1"
                        size="sm"
                        :title="autoWeight
                          ? `同级内按权重降序尝试，权重相同按列表顺序（当前第 ${mi + 1} 位）`
                          : `自动权重已关闭：权重暂不参与调度，开启后立即按它降序；现在的次序由拖动（或上移 / 下移）决定，当前第 ${mi + 1} 位`"
                      />
                    </div>
                    <!-- 该成员的真实运行数据 + 跳模型详情：请求数 / 失败次数 / 此刻是否在用 -->
                    <div class="flex shrink-0 items-center gap-1">
                      <button
                        type="button"
                        data-test="gd-member-chip"
                        class="rounded-md border border-line bg-app px-1.5 py-0.5 text-[11.5px] text-ink-2 transition-colors hover:border-teal-300 hover:text-ink"
                        :title="'查看 ' + bindingLabel(m.model_id) + ' 的详情'"
                        @click="openMember(t.level, m.model_id)"
                      >详情</button>
                      <Badge
                        tone="gray"
                        data-test="gd-member-requests"
                        :title="memberRequestsTitle(t.level, m.model_id)"
                      >{{ memberRequests(t.level, m.model_id) }} 次</Badge>
                      <Badge v-if="memberFailed(t.level, m.model_id) > 0" tone="err" data-test="gd-member-failed">
                        失败 {{ memberFailed(t.level, m.model_id) }}
                      </Badge>                      <Badge
                        v-if="memberReasoningLoops(t.level, m.model_id) > 0"
                        tone="warn"
                        data-test="gd-member-reasoning-loops"
                        :title="memberReasoningTitle(t.level, m.model_id)"
                      >思考打转 {{ memberReasoningLoops(t.level, m.model_id) }}</Badge>
                      <span
                        v-if="isMemberInUse(t.level, m.model_id)"
                        data-test="gd-member-active"
                        class="inline-flex shrink-0 items-center gap-1 rounded-md bg-teal-100 px-1.5 py-0.5 text-[11.5px] font-medium text-teal-700"
                        title="此刻正在使用该 (渠道, 上游模型) 的并发请求数"
                      ><span class="h-1.5 w-1.5 rounded-full bg-teal-500"></span>使用中 ×{{ memberInflight(t.level, m.model_id) }}</span>
                    </div>
                    <div class="flex shrink-0 items-center gap-0.5">
                      <IconBtn
                        title="上移（也可以直接拖动左侧抓手）"
                        data-test="member-up"
                        :disabled="mi === 0"
                        @click="moveMember(t, mi, -1)"
                      >
                        <ArrowUp :size="14" />
                      </IconBtn>
                      <IconBtn
                        title="下移（也可以直接拖动左侧抓手）"
                        data-test="member-down"
                        :disabled="mi === t.members.length - 1"
                        @click="moveMember(t, mi, 1)"
                      >
                        <ArrowDown :size="14" />
                      </IconBtn>
                    </div>
                    <IconBtn title="删除成员" danger @click="removeMemberAt(t, mi)">
                      <Trash2 :size="15" />
                    </IconBtn>
                  </div>
                </div>

                <p v-else class="py-1 text-[12px] text-ink-3">
                  该级还没有成员，点右上「添加成员」。
                </p>
              </div>
            </div>

            <div class="mt-2.5 flex flex-wrap items-center gap-2">
              <Btn size="sm" variant="ghost" :disabled="!models.length" @click="addTier">
                <Plus :size="12" />添加{{ tierName(draft.tiers.length + 1) }}
              </Btn>
              <span class="text-[11.5px] leading-4 text-ink-3">
                点成员可跳到「模型详情」（返回仍停在本页）。
              </span>
            </div>
          </div>

          <p
            v-if="blockReason" data-test="gm-block-reason"
            class="border-t border-line-soft px-4 py-2 text-[11.5px] leading-4 text-err-600"
          >还不能保存：{{ blockReason }}</p>
          <p
            v-if="draft.tiers.length"
            class="border-t border-line-soft px-4 py-2.5 text-[11.5px] leading-4 text-ink-3"
            data-test="gd-tier-note"
          >↑ 全部失败 → 回到一级（最多 {{ maxCycles }} 轮）；本卡改动只进内存，要落库请点顶栏的「保存」。</p>
        </Card>

        <!-- 密钥列表（脱敏） -->
        <Card :pad="false" class="overflow-hidden">
          <div class="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
            <KeyRound :size="15" class="text-ink-3" />
            <h2 class="text-[13.5px] font-semibold text-ink">密钥</h2>
            <span class="ml-auto text-[12px] text-ink-3">列表只显示前 8 位；点「复制」取完整密钥</span>
          </div>
          <div v-if="groupKeys.length" class="overflow-x-auto">
            <table class="w-full text-[12.5px]">
              <thead>
                <tr>
                  <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">名称</th>
                  <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">密钥（脱敏）</th>
                  <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">状态</th>
                  <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">操作</th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="(k, i) in groupKeys" :key="k.key + i" class="border-t border-line-soft hover:bg-app">
                  <td class="whitespace-nowrap px-3 py-2 text-ink">{{ fmtText(k.name) }}</td>
                  <td class="whitespace-nowrap px-3 py-2 font-mono text-[11.5px] text-ink-2">{{ maskKey(k.key) }}</td>
                  <td class="whitespace-nowrap px-3 py-2">
                    <Badge :tone="k.enabled ? 'ok' : 'gray'">{{ k.enabled ? "已启用" : "已停用" }}</Badge>
                  </td>
                  <td class="whitespace-nowrap px-3 py-2">
                    <!-- 复制按钮是唯一取回完整密钥的入口；:title 里也绝不放完整值。 -->
                    <IconBtn
                      :data-test="'key-copy-' + i"
                      title="复制完整密钥"
                      @click="copyKey(k.key)"
                    ><Copy :size="14" /></IconBtn>
                  </td>
                </tr>
              </tbody>
            </table>
          </div>
          <div v-else class="px-4 py-3.5 text-[12px] text-ink-3">该分组还没有密钥。</div>
        </Card>
      </div>

      <!-- ------------------------------ 用量 ------------------------------ -->
      <div v-else-if="tab === 'usage'" class="min-h-0 flex-1 space-y-3.5 overflow-y-auto">
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
            <div class="mt-1.5 font-mono text-[20px] font-semibold leading-7 tabular-nums text-ink">{{ fmtSec(avgDuration) }}</div>
          </Card>
        </div>

        <!-- 图表：ECharts（与统计页同一个组件），不自己画 SVG -->
        <div class="grid grid-cols-1 gap-3 xl:grid-cols-2">
          <Card :pad="false" class="overflow-hidden">
            <div class="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
              <BarChart3 :size="15" class="text-ink-3" />
              <h2 class="text-[13.5px] font-semibold text-ink">按模型请求数</h2>
              <span class="ml-auto text-[12px] text-ink-3">前 8 名</span>
            </div>
            <div v-if="!hasUsageRows" class="px-4 py-8 text-center text-[12.5px] text-ink-3">暂无用量数据</div>
            <div v-else class="px-3 py-3" data-test="gd-chart-req">
              <EChart :option="usageReqOption" :height="Math.max(150, Math.min(8, byModel.length) * 32 + 46)" />
            </div>
          </Card>

          <Card :pad="false" class="overflow-hidden">
            <div class="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
              <PieChart :size="15" class="text-ink-3" />
              <h2 class="text-[13.5px] font-semibold text-ink">按模型 token 占比</h2>
              <span class="ml-auto text-[12px] text-ink-3">输入 + 输出</span>
            </div>
            <div v-if="!hasTokens" class="px-4 py-8 text-center text-[12.5px] text-ink-3">暂无 token 数据</div>
            <div v-else class="px-3 py-3" data-test="gd-chart-token">
              <EChart :option="usageTokenOption" :height="260" />
            </div>
          </Card>
        </div>

        <Card :pad="false" class="overflow-hidden">
          <div class="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
            <Layers :size="15" class="text-ink-3" />
            <h2 class="text-[13.5px] font-semibold text-ink">按模型 token 构成</h2>
            <span class="ml-auto text-[12px] text-ink-3">输入 / 输出堆叠 · 前 8 名</span>
          </div>
          <div v-if="!hasTokens" class="px-4 py-8 text-center text-[12.5px] text-ink-3">暂无 token 数据</div>
          <div v-else class="px-3 py-3" data-test="gd-chart-stack">
            <EChart :option="usageStackOption" :height="Math.max(150, Math.min(8, byModel.length) * 32 + 66)" />
          </div>
        </Card>

        <Card :pad="false" class="overflow-hidden">
          <div class="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
            <Coins :size="15" class="text-ink-3" />
            <h2 class="text-[13.5px] font-semibold text-ink">按模型用量</h2>
            <span class="ml-auto text-[12px] text-ink-3">{{ byModel.length }} 行 · 按成本降序 · 按 上游模型 × 渠道 聚合（失败请求按「试过的渠道×模型」展开）</span>
          </div>

          <Empty v-if="!byModel.length" title="暂无用量数据" hint="本分组产生请求并成功计费后，这里会按模型列出 token 与花费">
            <template #icon><Coins :size="28" /></template>
          </Empty>

          <div v-else class="overflow-x-auto">
            <table class="w-full text-[12.5px]">
              <thead>
                <tr>
                  <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">状态</th>
                  <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">模型</th>
                  <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">渠道</th>
                  <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">请求数</th>
                  <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">tokens 输入 / 输出</th>
                  <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">成本 / 占比</th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="(row, i) in byModel" :key="i" class="border-t border-line-soft hover:bg-app">
                  <td class="whitespace-nowrap px-3 py-2 text-[11.5px]" :class="modelStatusClass(row)" data-test="gd-model-status">{{ modelStatusText(row) }}</td>
                  <td class="whitespace-nowrap px-3 py-2 font-mono text-[11.5px]" :class="modelRouted(row) ? 'text-ink' : 'text-ink-3'">{{ modelLabel(row) }}</td>
                  <td class="whitespace-nowrap px-3 py-2">{{ modelChannel(row) }}</td>
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
      <Card v-else-if="tab === 'request'" :pad="false" class="flex min-h-0 flex-1 flex-col overflow-hidden">
        <div v-if="!requestTotal" class="flex flex-1 items-center justify-center">
          <Empty title="暂无请求记录" hint="本分组产生请求后，这里会显示每一次请求的最终结果">
            <template #icon><ScrollText :size="28" /></template>
          </Empty>
        </div>

        <div v-else class="min-h-0 flex-1 overflow-auto">
          <table class="w-full text-[12.5px]">
            <thead class="sticky top-0 z-[1] bg-surface">
              <tr>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">时间</th>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3 min-w-[200px]">暴露模型</th>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">状态</th>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">最终渠道</th>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3 min-w-[200px]">最终上游模型</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">尝试</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">tokens 输入 / 输出</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">成本</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">总耗时</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">首字 TTFT</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3" title="输出速度：输出 tokens ÷ (总耗时 − 首字)，上游一次性返回时退化为 输出 tokens ÷ 总耗时">TPS 速度</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="(row, i) in requestRows" :key="rowKey(row, i)" class="border-t border-line-soft hover:bg-app">
                <td class="whitespace-nowrap px-3 py-2 font-mono text-[11.5px] text-ink-2">{{ fmtTs(row.ts) }}</td>
                <td class="whitespace-nowrap px-3 py-2 font-mono text-[11.5px] text-ink-2 min-w-[200px]">{{ fmtText(row.exposed_model) }}</td>
                <td class="whitespace-nowrap px-3 py-2">
                  <Badge
                :tone="statusInfo(row).tone"
                :title="Number(row.loop_truncated) > 0
                  ? '回答已经开始发送后检测到输出退化（死循环），网关就地截断并正常收尾：客户端收到的是截断后的内容'
                  : undefined"
              >{{ statusInfo(row).label }}</Badge>
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
                <td class="whitespace-nowrap px-3 py-2 text-right font-mono">{{ fmtSec(row.duration_ms) }}</td>
                <td class="whitespace-nowrap px-3 py-2 text-right font-mono">{{ fmtSec(row.ttft_ms) }}</td>
                <td class="whitespace-nowrap px-3 py-2 text-right font-mono tabular-nums text-ink-2">
                  {{ fmtTps(tpsOf(row.duration_ms, row.ttft_ms, row.output_tokens)) }}
                </td>
              </tr>
            </tbody>
          </table>
        </div>

        <div v-if="requestTotal > 0" class="shrink-0" data-test="pager-request">
          <TPager
            v-model:page="requestPage" v-model:size="requestSize"
            :total="requestTotal" :page-size="requestSize" :size-options="PAGE_SIZES"
            :loading="loading" unit="条请求日志"
          />
        </div>
      </Card>

      <!-- ---------------------------- 切换日志 ---------------------------- -->
      <Card v-else-if="tab === 'switch'" :pad="false" class="flex min-h-0 flex-1 flex-col overflow-hidden">
        <div v-if="!switchTotal" class="flex flex-1 items-center justify-center">
          <Empty title="暂无切换记录" hint="本分组的请求发生故障转移时，这里会显示每一次上游尝试">
            <template #icon><ScrollText :size="28" /></template>
          </Empty>
        </div>

        <div v-else class="min-h-0 flex-1 overflow-auto">
          <table class="w-full text-[12.5px]">
            <thead class="sticky top-0 z-[1] bg-surface">
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
                <td class="whitespace-nowrap px-3 py-2 text-right font-mono">{{ fmtSec(row.latency_ms) }}</td>
              </tr>
            </tbody>
          </table>
        </div>

        <div v-if="switchTotal > 0" class="shrink-0" data-test="pager-switch">
          <TPager
            v-model:page="switchPage" v-model:size="switchSize"
            :total="switchTotal" :page-size="switchSize" :size-options="PAGE_SIZES"
            :loading="loading" unit="条切换日志"
          />
        </div>
      </Card>

      <!-- ---------------------------- 冷却状态 ---------------------------- -->
      <Card v-else :pad="false" class="flex min-h-0 flex-1 flex-col overflow-hidden">
        <div class="flex shrink-0 flex-wrap items-center gap-2 border-b border-line px-4 py-3">
          <ShieldAlert :size="15" class="text-ink-3" />
          <h2 class="text-[13.5px] font-semibold text-ink">冷却状态</h2>
          <span class="ml-auto text-[12px] text-ink-3" data-test="gd-cooldown-count">
            {{ scopedCooldowns.length ? `共 ${scopedCooldowns.length} 个条目` : "全部正常" }}
          </span>
        </div>

        <div v-if="!scopedCooldowns.length" class="flex flex-1 items-center justify-center">
          <Empty title="本分组相关渠道暂无冷却" hint="冷却中的渠道会在这里显示剩余时间；这里只列出本分组层级里引用到的渠道">
            <template #icon><ShieldAlert :size="28" /></template>
          </Empty>
        </div>

        <div v-else class="min-h-0 flex-1 overflow-auto">
          <table class="w-full text-[12.5px]">
            <thead class="sticky top-0 z-[1] bg-surface">
              <tr>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">渠道</th>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">上游模型</th>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">类型</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">剩余时间</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="(row, i) in pagedCooldowns" :key="healthKey(row, i)" class="border-t border-line-soft hover:bg-app">
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

        <div v-if="scopedCooldowns.length" class="shrink-0" data-test="pager-cooldown">
          <TPager
            v-model:page="cooldownPage" v-model:size="cooldownSize"
            :total="scopedCooldowns.length" :page-size="cooldownSize" :size-options="COOLDOWN_PAGE_SIZES"
            unit="个条目"
          />
        </div>

        <p v-if="scopedCooldowns.length && otherCooldowns > 0" class="shrink-0 border-t border-line-soft px-4 py-2 text-[11.5px] text-ink-3">
          另有 {{ otherCooldowns }} 条其它渠道的冷却未显示（本分组没有引用这些渠道）。
        </p>
      </Card>
    </template>
  </div>
</template>
