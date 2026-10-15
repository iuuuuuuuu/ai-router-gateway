<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { BarChart3, Eraser, Layers, PieChart, RefreshCw, Repeat, Scissors, ScrollText, ShieldAlert } from "@lucide/vue";
import type { EChartsCoreOption } from "echarts/core";
import { api, type CooldownRow as ApiCooldownRow, type GroupStatRow, type StatsPayload } from "../api";
import { channelName, groups } from "../store";
import { toast } from "../toast";
import { fmtSec } from "../format";
import { chartPalette, chartTheme } from "../chart-theme";
import EChart from "../components/EChart.vue";
import {
  Alert, Badge, Btn, Card, Empty, Spinner, TPager, TSelect, TToggle,
} from "../components/ui";

type Tone = "gray" | "teal" | "ok" | "warn" | "err" | "brand";
type Row = Record<string, number | string | null>;
// 冷却行：cooling_until_ms 是绝对 epoch 毫秒（permanent 时为 0），remaining_ms 是服务端算好的
// 快照剩余毫秒，permanent 表示认证失败导致的永久隔离（人工解除前不会自动恢复）。
type CooldownRow = ApiCooldownRow & {
  permanent?: boolean; remaining_ms?: number; consecutive_failures?: number;
};

/* ------------------------------ 数值兜底 ------------------------------ */
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
function fmtClock(ms: number): string {
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

/* --------------------------------- 状态 --------------------------------- */
// reka-ui 的 SelectItem 不允许空字符串 value（空串 = 清空选择、显示 placeholder），
// 所以「全部分组」用哨兵值，请求前再转成 undefined，交给 api 的 qs() 丢掉。
const ALL_GROUPS = "__all__";
const groupId = ref<string>(ALL_GROUPS);
const groupOptions = computed(() => [
  { value: ALL_GROUPS, label: "全部分组" },
  ...groups.value.map(g => ({ value: g.id, label: g.name || g.id })),
]);
// 只在真正选了某个分组时才带参，避免任何一层把参数拼成字面量 "undefined"。
const scopedId = computed(() => (groupId.value === ALL_GROUPS ? "" : groupId.value));

const data = ref<StatsPayload | null>(null);
const loading = ref(false);
const err = ref("");
const autoRefresh = ref(true);
const lastAt = ref(0);
const clearing = ref("");
const now = ref(Date.now());

/* ------------------------------- 表格分页 ------------------------------- */
// 统计接口返回的是全量聚合（by_model 按成本排序、by_group 按分组），条数由「用过几个模型 /
// 几个分组」决定，几十行的量级；所以这里只做前端切片，不再打接口——翻页是零延迟的。
// 两张表共用一份可选值；分页条在有行时就渲染（哪怕只有 1 页），
// 这样「每页条数」这个偏好永远可达——否则行数少于默认页大小时用户根本改不了它。
const SIZE_OPTIONS = [5, 10, 20, 50, 100];
const modelPage = ref(1);
const modelSize = ref(20);
const groupPage = ref(1);
const groupSize = ref(20);
function clampPage(p: number, size: number, rows: number): number {
  const last = Math.max(1, Math.ceil(rows / Math.max(1, size)));
  return Math.min(Math.max(1, p), last);
}

let inflight = false;
async function load(silent = false) {
  if (inflight) return;
  inflight = true;
  if (!silent) loading.value = true;
  err.value = "";
  try {
    // api.stats 自己会拼 "?group_id=..."；传 undefined 即全局统计。
    const payload = (await api.stats(scopedId.value || undefined)) as StatsPayload;
    data.value = payload;
    palette.value = chartPalette(chartTheme());
    lastAt.value = Date.now();
  } catch (e: any) {
    err.value = String(e?.message || e);
  } finally {
    inflight = false;
    loading.value = false;
  }
}

/* --------------------------- summary 容错读取 --------------------------- */
const summary = computed<Row>(() => (data.value?.summary ?? {}) as Row);

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
const costTotal = computed(() => pick(["cost_usd", "total_cost_usd", "cost", "total_cost"]) ?? 0);
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
// 死循环拦截（输出退化）累计次数：老服务端 / 老库还没这一列时返回 null，概览里整块不渲染，
// 避免在没有该能力的实例上显示一个永远为 0 的指标。
const loopRetries = computed(() => pick(["loop_retries", "loop_guard_retries"]));
// 提交后截断：老服务端 / 老库没有这一列时返回 null，整个徽章不渲染。
const loopTruncated = computed(() => pick(["loop_truncated"]));

/* ------------------------------ 按模型成本 ------------------------------ */
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
// 后端 stats_by_model（server/src/store.rs）返回 channel_id —— 成本既能看到
// 落到哪个上游模型，也能看到经由哪个渠道。
/** 这一行是否真的路由到了上游。0 = 从未落到任何上游（全渠道失败/耗尽）。 */
function modelRouted(row: Row): boolean {
  return firstNum(row, ["routed"]) !== 0;
}
/** 未路由（失败）那一行的名字：它不是模型，所以不冒充任何模型名。 */
const UNROUTED_LABEL = "未路由（失败）";
/**
 * 本行的模型身份 = **真实路由到的上游模型**。
 * 后端按 final_channel × final_upstream_model 聚合。
 * 这里曾经用 exposed_model，但分组名/分组 id 本身就是可调用的模型名（router::is_wildcard 的
 * 通配路径），于是「grp_main」「主力分组」这种别名会各自成行，把分组名当成模型画进图里。
 */
function modelLabel(row: Row): string {
  if (!modelRouted(row)) return UNROUTED_LABEL;
  const up = modelUpstream(row);
  return up === "—" ? "未知模型" : up;
}
/**
 * 图表标签。同一个上游模型可能同时经由多个渠道出现，此时补上渠道名，
 * 否则图上会出现多根同名柱子、无法分辨。
 */
function modelLabels(rows: Row[]): string[] {
  const base = rows.map(modelLabel);
  const dup = new Set<string>();
  const seen = new Set<string>();
  for (const b of base) { if (seen.has(b)) dup.add(b); else seen.add(b); }
  return base.map((b, i) => dup.has(b) ? shortLabel(b, 12) + " · " + shortLabel(modelChannel(rows[i]), 10) : shortLabel(b));
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
const byModel = computed<Row[]>(() => {
  const rows = Array.isArray(data.value?.by_model) ? (data.value!.by_model as Row[]) : [];
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

/* ------------------------------ 分组用量 ------------------------------ */
// 后端 stats_by_group() 会跟着上面的 scope 一起过滤（server/src/store.rs 的 WHERE 拼接）：
// 全局时列出所有分组，选了某个分组时只返回那一行。点行可把 scope 切过去 / 再点一次切回全局。
function groupLabel(id: unknown): string {
  const g = String(id ?? "").trim();
  // request_logs.group_id 允许为空串（列级迁移的历史行）→ 统一显示为「未知分组」
  if (!g) return "未知分组";
  return groups.value.find(x => x.id === g)?.name || g;
}
const byGroup = computed<GroupStatRow[]>(() =>
  Array.isArray(data.value?.by_group) ? (data.value!.by_group as GroupStatRow[]) : []);

// 切统计范围 / 刷新后行数会变，页码要跟着收敛到最后一页，否则会停在空白页上。
watch([() => byModel.value.length, modelSize], () => {
  modelPage.value = clampPage(modelPage.value, modelSize.value, byModel.value.length);
});
watch([() => byGroup.value.length, groupSize], () => {
  groupPage.value = clampPage(groupPage.value, groupSize.value, byGroup.value.length);
});
const pagedByModel = computed<Row[]>(() => {
  const start = (modelPage.value - 1) * modelSize.value;
  return byModel.value.slice(start, start + modelSize.value);
});
const pagedByGroup = computed<GroupStatRow[]>(() => {
  const start = (groupPage.value - 1) * groupSize.value;
  return byGroup.value.slice(start, start + groupSize.value);
});

function groupRequests(row: GroupStatRow): number | null {
  return num(row?.requests);
}
function groupSuccess(row: GroupStatRow): number | null {
  return num(row?.success);
}
function groupRate(row: GroupStatRow): number | null {
  const t = groupRequests(row);
  const s = groupSuccess(row);
  if (t === null || s === null || t <= 0) return null;
  return (s / t) * 100;
}
function groupRateTone(row: GroupStatRow): Tone {
  const r = groupRate(row);
  if (r === null) return "gray";
  if (r >= 99) return "ok";
  if (r >= 90) return "warn";
  return "err";
}
function groupRateText(row: GroupStatRow): string {
  const r = groupRate(row);
  return r === null ? "—" : `${r.toFixed(1)}%`;
}
function groupModels(row: GroupStatRow): number | null {
  return num(row?.models);
}
function isCurrentScope(row: GroupStatRow): boolean {
  const id = String(row?.group_id ?? "").trim();
  return id !== "" && id === scopedId.value;
}
function pickGroup(row: GroupStatRow) {
  const id = String(row?.group_id ?? "").trim();
  if (!id) return;
  groupId.value = id === groupId.value ? ALL_GROUPS : id;
}

/* ------------------------------- 冷却状态 ------------------------------- */
const health = computed<CooldownRow[]>(() =>
  Array.isArray(data.value?.health) ? (data.value!.health as CooldownRow[]) : []);

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

async function clearCool(channelId: string) {
  if (!channelId || clearing.value) return;
  clearing.value = channelId;
  try {
    await api.clearCooldown(channelId);
    toast.ok("已解除冷却");
    await load(true);
  } catch (e: any) {
    toast.err("解除失败：" + String(e?.message || e));
  } finally {
    clearing.value = "";
  }
}


/* ------------------------------- 图表 ------------------------------- */
// ECharts 是 canvas 渲染，读不到 CSS 变量；这里在每次拉完数据后现取一遍设计 token，
// 保证图表配色永远跟页面一致（含侧栏折叠、窗口缩放这类布局变化）。
const palette = ref<string[]>(chartPalette(chartTheme()));

// 横轴名字太长会把图表挤扁，统一截断成短标签。
function shortLabel(s: string, max = 14): string {
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

// 横向条形图：按模型请求数排行（前 8 名），颜色统一用品牌橙。
const reqOption = computed<EChartsCoreOption>(() => {
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

// 环形图：按模型 token 占比（输入 + 输出），第 6 名以后合并成「其他」。
const tokenTotal = computed(() => byModel.value.reduce((s, r) => s + modelTokens(r), 0));
const tokenOption = computed<EChartsCoreOption>(() => {
  const t = chartTheme();
  const rows = byModel.value.slice().sort((a, b) => modelTokens(b) - modelTokens(a));
  const labels = modelLabels(rows);
  const items = rows.slice(0, 6).map((r, i) => ({ name: labels[i], value: modelTokens(r) }));
  const rest = rows.slice(6);
  const restSum = rest.reduce((s, r) => s + modelTokens(r), 0);
  if (restSum > 0) items.push({ name: "其他 " + rest.length + " 个", value: restSum });
  return {
    animationDuration: 320,
    color: palette.value,
    tooltip: {
      trigger: "item",
      backgroundColor: t.surface, borderColor: t.line, borderWidth: 1,
      textStyle: { color: t.ink, fontSize: 12, fontFamily: t.mono },
      formatter: (p: any) => p.name + "<br/>" + p.value + " tokens（" + p.percent + "%）",
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

/* ------------------------------ 定时器 ------------------------------ */
let poll: number | undefined;
let tick: number | undefined;
function stopPoll() {
  if (poll) {
    window.clearInterval(poll);
    poll = undefined;
  }
}
function startPoll() {
  stopPoll();
  if (autoRefresh.value) poll = window.setInterval(() => { void load(true); }, 10000);
}
watch(autoRefresh, startPoll);
// 统计范围切换（选择器或点「分组用量」行）都要立刻重新拉取分组作用域的数据。
watch(groupId, () => { void load(); });

onMounted(() => {
  void load();
  startPoll();
  tick = window.setInterval(() => { now.value = Date.now(); }, 1000);
});
onUnmounted(() => {
  stopPoll();
  if (tick) {
    window.clearInterval(tick);
    tick = undefined;
  }
});
</script>

<template>
  <div>
    <!-- 工具条：左侧统计文字，右侧刷新 / 自动刷新 -->
    <div class="mb-3 flex flex-wrap items-center gap-2">
      <div class="text-[12px] text-ink-3">
        统计范围：<span class="font-medium text-ink-2">{{ scopedId ? groupLabel(scopedId) : "全部分组" }}</span>
        · 共 {{ byModel.length }} 个模型 · 合计 {{ fmtCost(costTotal) }}
        <Badge
          v-if="loopRetries !== null && loopRetries > 0"
          tone="warn"
          class="ml-1 align-[1px]"
          data-test="loop-retries"
          title="网关内部因输出退化（死循环）丢弃并重试的次数。这些尝试发生在提交前，客户端零字节可见"
        >
          <Repeat :size="11" />死循环拦截 {{ fmtInt(loopRetries) }} 次
        </Badge>
        <!-- 提交后截断是另一回事：那条回答客户端已经收到了（只是没给完），
             与上面「客户端看不到」的丢弃不能混为一谈，所以分开计数、分开说明。 -->
        <Badge
          v-if="loopTruncated !== null && loopTruncated > 0"
          tone="warn"
          class="ml-1 align-[1px]"
          data-test="loop-truncated"
          title="回答已经开始发送后才判定为输出退化（死循环）并就地截断收尾的次数。客户端收到的是截断后的内容，不是完整回答"
        >
          <Scissors :size="11" />截断收尾 {{ fmtInt(loopTruncated) }} 次
        </Badge>
        <span v-if="lastAt" class="ml-1 font-mono text-[11.5px]">· 最后更新 {{ fmtClock(lastAt) }}</span>
      </div>
      <div class="ml-auto flex flex-wrap items-center gap-2">
        <!-- 同 Logs.vue：attrs 在 SelectRoot（inheritAttrs:false）处被丢弃，钩子必须挂在包裹 div 上。 -->
        <div class="w-[150px]" data-test="stats-group-filter">
          <TSelect v-model="groupId" size="sm" :options="groupOptions" placeholder="全部分组" />
        </div>
        <Btn size="sm" title="立即刷新" @click="load()">
          <RefreshCw :size="13" :class="loading ? 'animate-spin' : ''" />
          刷新
        </Btn>
        <TToggle v-model="autoRefresh" label="自动刷新 10s" />
      </div>
    </div>

    <Alert v-if="err" tone="err" class="mb-4">加载失败：{{ err }}</Alert>

    <div v-if="loading && !data" class="flex items-center justify-center gap-2 py-16 text-[13px] text-ink-3">
      <Spinner :size="16" /> 正在加载统计…
    </div>

    <template v-else>
      <!-- ---------------------------- 指标卡 ---------------------------- -->
      <div class="grid grid-cols-2 gap-3 lg:grid-cols-4">
        <Card>
          <div class="text-[11.5px] text-ink-3">总请求数</div>
          <div class="mt-1.5 font-mono text-[20px] font-semibold leading-7 tabular-nums text-ink">{{ fmtInt(requests) }}</div>
        </Card>
        <Card>
          <div class="text-[11.5px] text-ink-3">成功 / 失败</div>
          <div class="mt-1.5 text-[20px] font-semibold leading-7 tabular-nums">
            <span class="font-mono text-ok-600">{{ fmtInt(success) }}</span>
            <span class="mx-1 text-ink-3">/</span>
            <span class="font-mono text-err-600">{{ fmtInt(failures) }}</span>
          </div>
        </Card>
        <Card>
          <div class="text-[11.5px] text-ink-3">总成本</div>
          <div class="mt-1.5 font-mono text-[20px] font-semibold leading-7 tabular-nums text-ink">{{ fmtCost(costTotal) }}</div>
        </Card>
        <Card>
          <div class="text-[11.5px] text-ink-3">平均耗时 / 成功率</div>
          <div class="mt-1.5 flex items-center gap-2">
            <span class="font-mono text-[20px] font-semibold leading-7 tabular-nums text-ink">{{ fmtSec(avgDuration) }}</span>
            <Badge :tone="rateTone">{{ successRate === null ? "—" : `${successRate.toFixed(1)}%` }}</Badge>
          </div>
        </Card>
      </div>

      <!-- ---------------------------- 图表 ---------------------------- -->
      <div class="mt-5 grid grid-cols-1 gap-3 lg:grid-cols-2">
        <Card :pad="false" class="overflow-hidden">
          <div class="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
            <BarChart3 :size="15" class="text-ink-3" />
            <h2 class="text-[13.5px] font-semibold text-ink">请求数 · 按模型</h2>
            <span class="ml-auto text-[12px] text-ink-3">
              {{ byModel.length ? `Top ${Math.min(8, byModel.length)} / 共 ${byModel.length} 个模型` : "暂无数据" }}
            </span>
          </div>
          <Empty v-if="!byModel.length" title="暂无请求数据" hint="产生请求后这里会按模型展示请求数排行">
            <template #icon><BarChart3 :size="28" /></template>
          </Empty>
          <div v-else class="px-3 py-3" data-test="stats-chart-req">
            <EChart :option="reqOption" :height="Math.max(150, Math.min(8, byModel.length) * 32 + 46)" />
          </div>
        </Card>

        <Card :pad="false" class="overflow-hidden">
          <div class="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
            <PieChart :size="15" class="text-ink-3" />
            <h2 class="text-[13.5px] font-semibold text-ink">用量占比 · 按模型</h2>
            <span class="ml-auto text-[12px] text-ink-3">
              {{ tokenTotal > 0 ? `共 ${fmtInt(tokenTotal)} tokens` : "暂无数据" }}
            </span>
          </div>
          <Empty v-if="!(tokenTotal > 0)" title="暂无 token 数据" hint="上游返回 usage 后，这里会按模型展示输入 + 输出的 token 占比">
            <template #icon><PieChart :size="28" /></template>
          </Empty>
          <div v-else class="px-3 py-3" data-test="stats-chart-token">
            <EChart :option="tokenOption" :height="260" />
          </div>
        </Card>
      </div>
      <!-- -------------------------- 按模型成本 -------------------------- -->
      <Card :pad="false" class="mt-5 overflow-hidden">
        <div class="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
          <ScrollText :size="15" class="text-ink-3" />
          <h2 class="text-[13.5px] font-semibold text-ink">按模型成本</h2>
          <span class="ml-auto text-[12px] text-ink-3">{{ byModel.length }} 个模型</span>
        </div>

        <Empty v-if="!byModel.length" title="暂无成本数据" hint="产生请求并成功计费后，这里会按上游模型列出花费">
          <template #icon><ScrollText :size="28" /></template>
        </Empty>

        <!-- 空态与表格行都按全量 byModel 判断：翻到第 5 页时 byModel 仍然非空，
             用分页切片判断会让表格在翻页瞬间闪成「暂无成本数据」。 -->
        <div v-else class="overflow-x-auto">
          <table class="w-full text-[12.5px]">
            <thead>
              <tr>
                <th class="px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">模型</th>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">渠道</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">请求数</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">输入 tokens</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">输出 tokens</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">成本</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="(row, i) in pagedByModel" :key="modelPage + ':' + i" class="border-t border-line-soft hover:bg-app">
                <td class="whitespace-nowrap px-3 py-2 font-mono text-[11.5px]" :class="modelRouted(row) ? 'text-ink' : 'text-ink-3'">{{ modelLabel(row) }}</td>
                <td class="whitespace-nowrap px-3 py-2">{{ modelChannel(row) }}</td>
                <td class="px-3 py-2 text-right font-mono tabular-nums">
                  {{ fmtInt(firstNum(row, ["requests", "request_count", "count", "total_requests"])) }}
                </td>
                <td class="px-3 py-2 text-right font-mono tabular-nums">
                  {{ fmtInt(firstNum(row, ["input_tokens", "prompt_tokens", "input"])) }}
                </td>
                <td class="px-3 py-2 text-right font-mono tabular-nums">
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

        <!-- 纯前端分页：byModel 已经是全量聚合，切片即可 -->
        <TPager
          v-if="byModel.length"
          v-model:page="modelPage"
          v-model:size="modelSize"
          :total="byModel.length"
          :page-size="modelSize"
          :size-options="SIZE_OPTIONS"
          unit="个模型"
        />
      </Card>

      <!-- --------------------------- 分组用量 --------------------------- -->
      <Card :pad="false" class="mt-4 overflow-hidden">
        <div class="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
          <Layers :size="15" class="text-ink-3" />
          <h2 class="text-[13.5px] font-semibold text-ink">分组用量</h2>
          <span class="ml-auto text-[12px] text-ink-3">
            {{ byGroup.length ? `共 ${byGroup.length} 个分组 · 点行可切换统计范围` : (scopedId ? "该分组暂无用量" : "全局总览") }}
          </span>
        </div>

        <Empty v-if="!byGroup.length" title="暂无分组用量" hint="带分组的请求产生后，这里会按分组汇总请求数、token 与成本；切换统计范围后只显示被选中的分组">
          <template #icon><Layers :size="28" /></template>
        </Empty>

        <div v-else class="overflow-x-auto">
          <table class="w-full text-[12.5px]">
            <thead>
              <tr>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">分组</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">请求数</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">成功率</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">输入 tokens</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">输出 tokens</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">成本</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">平均耗时</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3" title="该分组真实路由到的 上游模型 × 渠道 组合数（不按客户端请求名/分组名去重）">模型数</th>
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="(row, i) in pagedByGroup" :key="row.group_id || i"
                class="cursor-pointer border-t border-line-soft transition-colors hover:bg-app"
                :class="isCurrentScope(row) ? 'bg-app' : ''"
                :title="`点击按「${groupLabel(row.group_id)}」过滤统计`"
                @click="pickGroup(row)"
              >
                <td class="whitespace-nowrap px-3 py-2">
                  <span class="inline-flex items-center gap-1.5">
                    <span class="font-medium" :class="isCurrentScope(row) ? 'text-brand-600' : 'text-ink'">{{ groupLabel(row.group_id) }}</span>
                    <Badge v-if="isCurrentScope(row)" tone="brand">当前</Badge>
                  </span>
                </td>
                <td class="whitespace-nowrap px-3 py-2 text-right font-mono tabular-nums">{{ fmtInt(groupRequests(row)) }}</td>
                <td class="whitespace-nowrap px-3 py-2 text-right">
                  <Badge :tone="groupRateTone(row)">{{ groupRateText(row) }}</Badge>
                </td>
                <td class="whitespace-nowrap px-3 py-2 text-right font-mono tabular-nums text-ink-2">{{ fmtInt(row.input_tokens) }}</td>
                <td class="whitespace-nowrap px-3 py-2 text-right font-mono tabular-nums text-ink-2">{{ fmtInt(row.output_tokens) }}</td>
                <td class="whitespace-nowrap px-3 py-2 text-right font-mono tabular-nums text-ink">{{ fmtCost(row.cost_usd) }}</td>
                <td class="whitespace-nowrap px-3 py-2 text-right font-mono tabular-nums">{{ fmtSec(row.avg_duration_ms) }}</td>
                <td class="whitespace-nowrap px-3 py-2 text-right font-mono tabular-nums">{{ fmtInt(groupModels(row)) }}</td>
              </tr>
            </tbody>
          </table>
        </div>

        <!-- 分组一般只有个位数，但同一个分页条的好处是「范围一换行数就变」时页码自动收敛 -->
        <TPager
          v-if="byGroup.length"
          v-model:page="groupPage"
          v-model:size="groupSize"
          :total="byGroup.length"
          :page-size="groupSize"
          :size-options="SIZE_OPTIONS"
          unit="个分组"
        />
      </Card>

      <!-- --------------------------- 冷却状态 --------------------------- -->
      <Card :pad="false" class="mt-4 overflow-hidden">
        <div class="flex items-center justify-between gap-2 border-b border-line px-4 py-3">
          <div class="flex items-center gap-2">
            <ShieldAlert :size="15" class="text-ink-3" />
            <h2 class="text-[13.5px] font-semibold text-ink">冷却状态</h2>
          </div>
          <span class="text-[12px] text-ink-3">{{ health.length ? `${health.length} 个条目` : "全部正常" }}</span>
        </div>

        <div v-if="!health.length" class="p-4">
          <Alert tone="ok">当前没有冷却中的渠道</Alert>
        </div>

        <div v-else class="overflow-x-auto">
          <table class="w-full text-[12.5px]">
            <thead>
              <tr>
                <th class="px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">渠道</th>
                <th class="px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">上游模型</th>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">类型</th>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">剩余时间</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">操作</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="(row, i) in health" :key="healthKey(row, i)" class="border-t border-line-soft hover:bg-app">
                <td class="whitespace-nowrap px-3 py-2">{{ channelName(row.channel_id) }}</td>
                <td class="px-3 py-2">
                  <Badge v-if="row.upstream_model === '*'" tone="err">整渠道</Badge>
                  <span v-else class="font-mono text-[11.5px] text-ink-2">{{ fmtText(row.upstream_model) }}</span>
                </td>
                <td class="whitespace-nowrap px-3 py-2">
                  <Badge :tone="kindInfo(row.kind).tone">{{ kindInfo(row.kind).label }}</Badge>
                  <span v-if="failCount(row)" class="ml-1.5 font-mono text-[11px] text-ink-3">×{{ failCount(row) }}</span>
                </td>
                <td class="whitespace-nowrap px-3 py-2 font-mono tabular-nums text-ink-2">
                  <span :class="isPermanent(row) ? 'font-sans text-[11.5px] font-medium text-err-600' : ''">{{ fmtCooldownRemain(row) }}</span>
                </td>
                <td class="whitespace-nowrap px-3 py-2 text-right">
                  <Btn
                    variant="danger" size="sm"
                    :disabled="clearing === row.channel_id"
                    @click="clearCool(row.channel_id)"
                  >
                    <Eraser :size="12" />
                    {{ clearing === row.channel_id ? "解除中…" : "解除" }}
                  </Btn>
                </td>
              </tr>
            </tbody>
          </table>
        </div>
      </Card>
    </template>
  </div>
</template>
