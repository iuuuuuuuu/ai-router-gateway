<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { RefreshCw, ScrollText } from "@lucide/vue";
import { api, type RequestLog, type SwitchLog } from "../api";
import { channelName, groups } from "../store";
import { fmtTps, tpsOf } from "../format";
import { toast } from "../toast";
import {
  Alert, Badge, Btn, Card, Empty, Spinner, TInput, TSelect, TToggle,
} from "../components/ui";

type Tone = "gray" | "teal" | "ok" | "warn" | "err" | "brand";
type TabKey = "switch" | "request";

const LIMIT = 300;

/* ------------------------------ 格式化兜底 ------------------------------ */
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
function tierInfo(tier: unknown): { label: string; tone: Tone } {
  const t = num(tier);
  if (t === 1) return { label: "一级", tone: "brand" };
  if (t === 2) return { label: "二级", tone: "gray" };
  if (t === 3) return { label: "三级", tone: "gray" };
  return { label: t === null ? "—" : `${Math.round(t)} 级`, tone: "gray" };
}
function outcomeInfo(outcome: unknown): { label: string; tone: Tone } {
  // 后端 switch_logs.outcome 实际写的是 "committed"/"failed"（server/src/routes.rs:174），
  // 兼容需求里写的 "success"，两者都算成功，避免真实数据把成功显示成失败。
  const s = String(outcome ?? "").toLowerCase();
  return s === "success" || s === "committed"
    ? { label: "成功", tone: "ok" }
    : { label: "失败", tone: "err" };
}
function errorClass(v: unknown): { text: string; tone: Tone } {
  const s = String(v ?? "").trim();
  const low = s.toLowerCase();
  if (!s || low === "none" || low === "null") return { text: "", tone: "warn" };
  // "committed" 是后端成功行的占位值（server/src/proxy.rs:148,169 写 error_class: "committed"），
  // 属成功语义 → 与 outcome 一致用 ok 色调，避免成功行被渲染成 warn 徽章。
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
    // router::Terminal::NoCandidate（server/src/routes.rs:198）：网关返回 404 no_route_for_model 的终态
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
function groupName(id: unknown): string {
  const g = String(id ?? "").trim();
  if (!g) return "—";
  return groups.value.find(x => x.id === g)?.name || g;
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

/* --------------------------------- 状态 --------------------------------- */
const tab = ref<TabKey>("switch");
const q = ref<string>("");
const autoRefresh = ref(false);
const loading = ref(false);
const err = ref("");
const lastAt = ref(0);
const switchRows = ref<SwitchLog[]>([]);
const requestRows = ref<RequestLog[]>([]);

// reka-ui 的 SelectItem 不允许空字符串 value（空串是「清空选择、显示 placeholder」的语义），
// 所以「全部分组」用哨兵值表示，发请求前再转成 undefined，让 api 的 qs() 丢掉这个参数。
const ALL_GROUPS = "__all__";
const groupId = ref<string>(ALL_GROUPS);
const groupOptions = computed(() => [
  { value: ALL_GROUPS, label: "全部分组" },
  ...groups.value.map(g => ({ value: g.id, label: g.name || g.id })),
]);
const scopeId = computed(() => (groupId.value === ALL_GROUPS ? undefined : groupId.value));

const tabs: { key: TabKey; label: string }[] = [
  { key: "switch", label: "切换日志" },
  { key: "request", label: "请求日志" },
];

const count = computed(() => (tab.value === "switch" ? switchRows.value.length : requestRows.value.length));
const filterLabel = computed(() => {
  const parts: string[] = [];
  if (groupId.value !== ALL_GROUPS) parts.push(`分组：${groupName(groupId.value)}`);
  const kw = q.value.trim();
  if (kw) parts.push(`过滤 request_id：${kw}`);
  return parts.join(" · ");
});

let inflight = false;
async function load(silent = false) {
  if (inflight) return;
  inflight = true;
  if (!silent) loading.value = true;
  err.value = "";
  try {
    if (tab.value === "switch") {
      switchRows.value = await api.switchLogs(LIMIT, q.value.trim() || undefined, scopeId.value);
    } else {
      requestRows.value = await api.requestLogs(LIMIT, q.value.trim() || undefined, scopeId.value);
    }
    lastAt.value = Date.now();
  } catch (e: any) {
    err.value = String(e?.message || e);
  } finally {
    inflight = false;
    loading.value = false;
  }
}

/* --------------------------- 交互：过滤 / 复制 --------------------------- */
let debounce: number | undefined;
function loadNow() {
  if (debounce) {
    window.clearTimeout(debounce);
    debounce = undefined;
  }
  void load();
}

watch(q, () => {
  if (debounce) window.clearTimeout(debounce);
  debounce = window.setTimeout(() => {
    debounce = undefined;
    void load();
  }, 350);
});

watch(tab, () => { void load(); });
// 切换分组同样重新拉取（api 侧按 group_id 过滤）
watch(groupId, () => { void load(); });

async function copyId(id: unknown) {
  const s = String(id ?? "");
  if (!s) return;
  try {
    await navigator.clipboard.writeText(s);
    toast.ok("已复制");
    return;
  } catch {
    /* 非安全上下文（Tauri/无权限）时回退 */
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
    if (ok) toast.ok("已复制");
    else toast.err("复制失败");
  } catch {
    toast.err("复制失败");
  }
}

/* ------------------------------- 自动刷新 ------------------------------- */
let timer: number | undefined;
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
watch(autoRefresh, startTimer);

onMounted(() => { void load(); });
onUnmounted(() => {
  stopTimer();
  if (debounce) window.clearTimeout(debounce);
});
</script>

<template>
  <div>
    <!-- 工具条：左侧统计文字 + request_id 过滤，右侧刷新 / 自动刷新 -->
    <div class="mb-3 flex flex-wrap items-center gap-2">
      <div class="text-[12px] text-ink-3">
        共 {{ count }} 条记录<template v-if="filterLabel"> · {{ filterLabel }}</template>
        <span v-if="lastAt" class="ml-1 font-mono text-[11.5px]">· 最后更新 {{ fmtClock(lastAt) }}</span>
      </div>
      <!-- data-test 必须挂在包裹 div 上：reka-ui 的 SelectRoot 是 inheritAttrs:false 且未 v-bind="$attrs"，
           挂在 <TSelect> 上的 attrs 会在 SelectRoot 处被丢掉，不会到达 button[role=combobox]。
           包裹 div 是稳定的真实 DOM 节点，探针用 [data-test=log-group-filter] button[role=combobox] 定位。 -->
      <div class="w-[150px]" data-test="log-group-filter">
        <TSelect v-model="groupId" size="sm" :options="groupOptions" placeholder="全部分组" />
      </div>
      <div class="w-[220px]">
        <TInput
          v-model="q" size="sm" mono
          placeholder="按 request_id 过滤…"
          @keyup.enter="loadNow"
        />
      </div>
      <div class="ml-auto flex flex-wrap items-center gap-2">
        <Btn size="sm" title="立即刷新" @click="loadNow">
          <RefreshCw :size="13" :class="loading ? 'animate-spin' : ''" />
          刷新
        </Btn>
        <TToggle v-model="autoRefresh" label="自动刷新 5s" />
      </div>
    </div>

    <Alert v-if="err" tone="err" class="mb-4">加载失败：{{ err }}</Alert>

    <!-- tabs -->
    <div class="mb-3 flex items-center gap-0.5 border-b border-line">
      <button
        v-for="t in tabs" :key="t.key" type="button"
        class="-mb-px border-b-2 px-3 py-2 text-[13px] transition-colors"
        :class="tab === t.key
          ? 'border-brand-500 font-medium text-ink'
          : 'border-transparent text-ink-3 hover:text-ink-2'"
        @click="tab = t.key"
      >{{ t.label }}</button>
    </div>

    <Card :pad="false" class="overflow-hidden">
      <div v-if="loading && !count" class="flex items-center justify-center gap-2 py-14 text-[13px] text-ink-3">
        <Spinner :size="16" /> 正在加载…
      </div>

      <Empty v-else-if="!count" title="暂无记录" hint="发出请求后这里会显示每一次上游切换与最终结果">
        <template #icon><ScrollText :size="28" /></template>
      </Empty>

      <!-- ---------------------------- 切换日志 ---------------------------- -->
      <div v-else-if="tab === 'switch'" class="overflow-x-auto">
        <table class="w-full text-[12.5px]">
          <thead>
            <tr>
              <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">时间</th>
              <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">request_id</th>
              <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">分组</th>
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
              <td class="whitespace-nowrap px-3 py-2">
                <button
                  v-if="row.request_id" type="button"
                  class="block max-w-[168px] truncate font-mono text-[11.5px] text-ink-2 transition-colors hover:text-brand-600"
                  :title="row.request_id + ' · 点击复制'"
                  @click="copyId(row.request_id)"
                >{{ row.request_id }}</button>
                <span v-else class="text-ink-3">—</span>
              </td>
              <td class="whitespace-nowrap px-3 py-2">{{ groupName(row.group_id) }}</td>
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
                <Badge v-if="errorClass(row.error_class).text" :tone="errorClass(row.error_class).tone" data-test="switch-error-class">
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

      <!-- ---------------------------- 请求日志 ---------------------------- -->
      <div v-else class="overflow-x-auto">
        <table class="w-full text-[12.5px]">
          <thead>
            <tr>
              <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">时间</th>
              <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">request_id</th>
              <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">分组</th>
              <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3 min-w-[200px]">暴露模型</th>
              <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">状态</th>
              <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">最终渠道</th>
              <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3 min-w-[200px]">最终上游模型</th>
              <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">尝试</th>
              <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">tokens 输入 / 输出</th>
              <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">缓存读 / 写</th>
              <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">成本</th>
              <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">总耗时</th>
              <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">首字 TTFT</th>
              <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3" title="输出速度：输出 tokens ÷ (总耗时 − 首字)，上游一次性返回时退化为 输出 tokens ÷ 总耗时">TPS 速度</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="(row, i) in requestRows" :key="rowKey(row, i)" class="border-t border-line-soft hover:bg-app">
              <td class="whitespace-nowrap px-3 py-2 font-mono text-[11.5px] text-ink-2">{{ fmtTs(row.ts) }}</td>
              <td class="whitespace-nowrap px-3 py-2">
                <button
                  v-if="row.request_id" type="button"
                  class="block max-w-[168px] truncate font-mono text-[11.5px] text-ink-2 transition-colors hover:text-brand-600"
                  :title="row.request_id + ' · 点击复制'"
                  @click="copyId(row.request_id)"
                >{{ row.request_id }}</button>
                <span v-else class="text-ink-3">—</span>
              </td>
              <td class="whitespace-nowrap px-3 py-2">{{ groupName(row.group_id) }}</td>
              <td class="whitespace-nowrap px-3 py-2 font-mono text-[11.5px] text-ink-2 min-w-[200px]">{{ fmtText(row.exposed_model) }}</td>
              <td class="whitespace-nowrap px-3 py-2">
                <Badge :tone="statusInfo(row.status).tone">{{ statusInfo(row.status).label }}</Badge>
              </td>
              <td class="whitespace-nowrap px-3 py-2">{{ finalChannel(row.final_channel) }}</td>
              <td class="whitespace-nowrap px-3 py-2 font-mono text-[11.5px] text-ink-2 min-w-[200px]">{{ fmtText(row.final_upstream_model) }}</td>
              <td class="whitespace-nowrap px-3 py-2 text-right font-mono">{{ fmtInt(row.attempts) }}</td>
              <td class="whitespace-nowrap px-3 py-2 text-right font-mono">
                {{ fmtInt(row.input_tokens) }} / {{ fmtInt(row.output_tokens) }}
              </td>
              <td class="whitespace-nowrap px-3 py-2 text-right font-mono text-[11.5px] text-ink-2">
                {{ fmtInt(row.cache_read_tokens) }} / {{ fmtInt(row.cache_creation_tokens) }}
              </td>
              <td class="whitespace-nowrap px-3 py-2 text-right">
                <span class="font-mono">{{ fmtCost(row.cost_usd) }}</span>
                <span v-if="wasted(row.wasted_cost_usd)" class="ml-1 text-[11px] text-warn-600">
                  (浪费 {{ fmtCost(row.wasted_cost_usd) }})
                </span>
              </td>
              <td class="whitespace-nowrap px-3 py-2 text-right font-mono">{{ fmtMs(row.duration_ms) }}</td>
              <td class="whitespace-nowrap px-3 py-2 text-right font-mono">{{ fmtMs(row.ttft_ms) }}</td>
              <td class="whitespace-nowrap px-3 py-2 text-right font-mono tabular-nums text-ink-2">
                {{ fmtTps(tpsOf(row.duration_ms, row.ttft_ms, row.output_tokens)) }}
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </Card>
  </div>
</template>
