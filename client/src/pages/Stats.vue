<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { Eraser, Layers, RefreshCw, ScrollText, ShieldAlert } from "@lucide/vue";
import { api, type CooldownRow as ApiCooldownRow, type GroupStatRow, type StatsPayload } from "../api";
import { channelName, groups } from "../store";
import { toast } from "../toast";
import {
  Alert, Badge, Btn, Card, Empty, Spinner, TSelect, TToggle,
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
// 后端 stats_by_model（server/src/store.rs）现在返回 channel_id —— 成本既能看到
// 落到哪个上游模型，也能看到经由哪个渠道。
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
// 后端 stats_by_group() 永远统计全部分组（server/src/store.rs），不受上面的 scope 影响，
// 所以这里渲染的是「全局总览」，点击某行才把 scope 切到那个分组。
function groupLabel(id: unknown): string {
  const g = String(id ?? "").trim();
  // request_logs.group_id 允许为空串（列级迁移的历史行）→ 统一显示为「未知分组」
  if (!g) return "未知分组";
  return groups.value.find(x => x.id === g)?.name || g;
}
const byGroup = computed<GroupStatRow[]>(() =>
  Array.isArray(data.value?.by_group) ? (data.value!.by_group as GroupStatRow[]) : []);

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
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m${s % 60}s`;
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return `${h}h${m}m`;
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
        <span v-if="lastAt" class="ml-1 font-mono text-[11.5px]">· 最后更新 {{ fmtClock(lastAt) }}</span>
      </div>
      <div class="ml-auto flex flex-wrap items-center gap-2">
        <div class="w-[150px]">
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
            <span class="font-mono text-[20px] font-semibold leading-7 tabular-nums text-ink">{{ fmtMs(avgDuration) }}</span>
            <Badge :tone="rateTone">{{ successRate === null ? "—" : `${successRate.toFixed(1)}%` }}</Badge>
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

        <div v-else class="overflow-x-auto">
          <table class="w-full text-[12.5px]">
            <thead>
              <tr>
                <th class="px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">暴露模型</th>
                <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">渠道</th>
                <th class="px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">上游模型</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">请求数</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">输入 tokens</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">输出 tokens</th>
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">成本</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="(row, i) in byModel" :key="i" class="border-t border-line-soft hover:bg-app">
                <td class="whitespace-nowrap px-3 py-2">{{ modelExposed(row) }}</td>
                <td class="whitespace-nowrap px-3 py-2">{{ modelChannel(row) }}</td>
                <td class="px-3 py-2 font-mono text-[11.5px] text-ink-2">{{ modelUpstream(row) }}</td>
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
      </Card>

      <!-- --------------------------- 分组用量 --------------------------- -->
      <Card :pad="false" class="mt-4 overflow-hidden">
        <div class="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
          <Layers :size="15" class="text-ink-3" />
          <h2 class="text-[13.5px] font-semibold text-ink">分组用量</h2>
          <span class="ml-auto text-[12px] text-ink-3">
            {{ byGroup.length ? `共 ${byGroup.length} 个分组 · 全局总览，点行可切换统计范围` : "全局总览" }}
          </span>
        </div>

        <Empty v-if="!byGroup.length" title="暂无分组用量" hint="带分组的请求产生后，这里会按分组汇总请求数、token 与成本">
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
                <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">模型数</th>
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="(row, i) in byGroup" :key="row.group_id || i"
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
                <td class="whitespace-nowrap px-3 py-2 text-right font-mono tabular-nums">{{ fmtMs(row.avg_duration_ms) }}</td>
                <td class="whitespace-nowrap px-3 py-2 text-right font-mono tabular-nums">{{ fmtInt(groupModels(row)) }}</td>
              </tr>
            </tbody>
          </table>
        </div>
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
