<script setup lang="ts">
// 模型详情（整页版）：从分组概览/详情里点某个成员进来，看这个「渠道-模型」自己的
// 统计、失败分布和最近失败明细。数据来自 /admin/api/group-members（分组成员快照）
// 与 /admin/api/logs/switch（本分组最近若干条切换日志，客户端按渠道 + 上游模型过滤）。
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { ArrowLeft, RefreshCw, Repeat, ScrollText, ShieldAlert, Waypoints } from "@lucide/vue";
import { api, type GroupMember, type GroupMembersPayload, type SwitchLog } from "../api";
import { channelName, bindingLabel } from "../store";
import { closeModelDetail, nav } from "../nav";
import { fmtSec } from "../format";
import Btn from "../components/ui/Btn.vue";
import Badge from "../components/ui/Badge.vue";
import Card from "../components/ui/Card.vue";
import Alert from "../components/ui/Alert.vue";
import Empty from "../components/ui/Empty.vue";
import Spinner from "../components/ui/Spinner.vue";

type Tone = "gray" | "teal" | "ok" | "warn" | "err" | "brand";
// 切换日志行：后端只保证上面这些列，这里按行就地兜底，避免脏数据把表格打崩。
type SwitchRow = Partial<SwitchLog> & { group_id?: string };

// 失败明细一次多拉一些（服务端上限由路由决定，多拿不会报错）；与整页版分组详情同量级。
const LIMIT = 300;
const TIER_NAMES = ["一级", "二级", "三级", "四级", "五级", "六级"];

/* ------------------------------ 格式化兜底 ------------------------------ */
// 与 Logs.vue / Stats.vue / GroupDetail.vue 保持同一份实现（各页各自复制了一份，照此复制）。
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
function tierName(level: unknown): string {
  const t = num(level);
  if (t === null) return "—";
  return TIER_NAMES[t - 1] ?? `${Math.round(t)} 级`;
}
function outcomeInfo(outcome: unknown): { label: string; tone: Tone } {
  // 与 Logs.vue 同一份判定：后端写 "committed"/"failed"，兼容 "success"，两者都算成功。
  const s = String(outcome ?? "").toLowerCase();
  return s === "success" || s === "committed"
    ? { label: "成功", tone: "ok" }
    : { label: "失败", tone: "err" };
}
function errorClass(v: unknown): { text: string; tone: Tone } {
  const s = String(v ?? "").trim();
  const low = s.toLowerCase();
  if (!s || low === "none" || low === "null") return { text: "", tone: "warn" };
  // "committed" 是成功行的占位值；"degenerate_output" 是死循环拦截（提交前判定输出退化、
  // 丢弃这次尝试换下一个候选）—— 两个都走显式映射，避免被当成普通错误或染错色调。
  if (low === "committed") return { text: s, tone: "ok" };
  if (low === "degenerate_output") return { text: "输出退化", tone: "warn" };
  return { text: s, tone: "warn" };
}
function kindInfo(kind: unknown): { label: string; tone: Tone } {
  switch (String(kind ?? "").toLowerCase()) {
    case "auth": return { label: "认证失败", tone: "err" };
    case "model": return { label: "模型不可用", tone: "warn" };
    case "backoff": return { label: "退避中", tone: "gray" };
    default: return { label: fmtText(kind), tone: "gray" };
  }
}

/* --------------------------------- 状态 --------------------------------- */
const loading = ref(false);
const err = ref("");
const payload = ref<GroupMembersPayload | null>(null);
const lastAt = ref(0);
const now = ref(Date.now());
const switchRows = ref<SwitchRow[]>([]);
let tick: number | undefined;

// nav.modelKey / nav.groupId 的快照：closeModelDetail() 会先把 modelKey 清空，
// 组件还要再渲染一帧才被卸载，所以派生信息（标题、空态文案）必须读快照而不是读 nav。
const activeGroupId = ref(nav.groupId ?? "");
const activeKey = ref(nav.modelKey ?? "");

/** nav 指着的那条成员；null = 数据还没回来 / 这个成员已经不在分组里了。 */
const member = computed<GroupMember | null>(() => {
  const list = payload.value?.members;
  if (!Array.isArray(list)) return null;
  return list.find(m => m?.key === activeKey.value) ?? null;
});
const isActive = computed(() => {
  const active = payload.value?.active_key ?? null;
  return active !== null && active === member.value?.key;
});
const groupName = computed(() => payload.value?.group_name || activeGroupId.value || "—");
const title = computed(() => `模型详情 · ${bindingLabel(member.value?.model_id || activeKey.value)}`);
const subtitle = computed(() => {
  const m = member.value;
  const name = m?.channel_name || channelName(m?.channel_id ?? "");
  const parts = [groupName.value, name];
  if (m?.upstream_model) parts.push(m.upstream_model);
  return parts.filter(Boolean).join(" · ");
});
const cooldownRemain = computed<number | null>(() => {
  const m = member.value;
  if (!m || !m.cooling) return null;
  // 与服务端契约一致：cooling_until_ms 是绝对 epoch 毫秒，冷却中恒 > 0；
  // 只有它是 0 / 缺失（例如永久隔离）时才没有可倒计时的剩余时间。
  const abs = num(m.cooling_until_ms);
  return abs !== null && abs > 0 ? Math.max(0, abs - now.value) : null;
});
const failureRows = computed<SwitchRow[]>(() => {
  const m = member.value;
  if (!m) return [];
  return switchRows.value
    .filter(r =>
      String(r?.channel_id ?? "") === String(m.channel_id)
      && String(r?.upstream_model ?? "") === String(m.upstream_model)
      && String(r?.outcome ?? "").toLowerCase() === "failed")
    .slice(0, LIMIT);
});
const errorClassRows = computed(() => {
  const list = member.value?.error_classes;
  if (!Array.isArray(list)) return [];
  return [...list].sort((a, b) => (num(b?.n) ?? 0) - (num(a?.n) ?? 0));
});

/* --------------------------------- 加载 --------------------------------- */
async function load() {
  const groupId = nav.groupId ?? "";
  if (!groupId || !nav.modelKey) return;
  activeGroupId.value = groupId;
  activeKey.value = nav.modelKey;
  loading.value = true;
  err.value = "";
  try {
    // 两个请求各自独立失败：成员快照挂了也要尽力显示失败明细，反之亦然。
    const [members, rows] = await Promise.allSettled([
      api.groupMembers(groupId),
      api.switchLogs(LIMIT, undefined, groupId) as Promise<SwitchRow[]>,
    ]);
    if (members.status === "fulfilled") payload.value = members.value;
    else {
      payload.value = null;
      err.value = members.reason instanceof Error ? members.reason.message : String(members.reason);
    }
    // 明细拉失败不算致命：概览照常显示，表格退化为空态文案。
    switchRows.value = rows.status === "fulfilled" && Array.isArray(rows.value) ? rows.value : [];
    lastAt.value = Date.now();
  } finally {
    loading.value = false;
  }
}

onMounted(() => {
  void load();
  tick = window.setInterval(() => { now.value = Date.now(); }, 1000);
});
onUnmounted(() => { if (tick) window.clearInterval(tick); });
// 换成员（或换分组）就重新拉；同一个 key 不重复请求。
watch(() => [nav.groupId, nav.modelKey].join("|"), () => { void load(); });
</script>

<template>
  <div class="px-5 py-5 space-y-3.5" data-test="model-detail">
    <!-- ------------------------------ 页头 ------------------------------ -->
    <header class="flex items-start justify-between gap-4">
      <div class="flex min-w-0 items-start gap-2">
        <Btn
          variant="ghost" size="sm" class="mt-0.5 shrink-0"
          data-test="model-detail-back" title="返回分组"
          @click="closeModelDetail"
        >
          <ArrowLeft :size="13" />返回
        </Btn>
        <div class="min-w-0">
          <h1 class="truncate text-[19px] font-semibold leading-7 tracking-[-0.01em] text-ink">{{ title }}</h1>
          <p class="mt-0.5 flex flex-wrap items-center gap-1.5 text-[12.5px] text-ink-3">
            <span class="truncate">{{ subtitle }}</span>
            <Badge v-if="isActive" tone="teal">当前使用</Badge>
            <Badge v-if="member && !member.enabled" tone="err">已禁用</Badge>
          </p>
        </div>
      </div>
      <div class="flex shrink-0 flex-wrap items-center justify-end gap-2">
        <Btn size="sm" title="立即刷新" data-test="model-detail-refresh" @click="load()">
          <RefreshCw :size="13" :class="loading ? 'animate-spin' : ''" />
          刷新
        </Btn>
      </div>
    </header>

    <Alert v-if="err" tone="err">
      <div>
        <p class="font-medium">加载模型详情失败</p>
        <p class="mt-0.5 leading-5">{{ err }}</p>
        <p class="mt-0.5 text-[12px] text-err-600/80">
          接口 /admin/api/group-members 由管理面提供；可以点右上角「刷新」重试。
        </p>
      </div>
    </Alert>

    <Card v-else-if="loading && !member">
      <div class="flex items-center justify-center gap-2 py-14 text-[13px] text-ink-3">
        <Spinner :size="16" /> 正在加载模型详情…
      </div>
    </Card>

    <Card v-else-if="!member">
      <Empty
        title="找不到这个模型"
        :hint="`分组 ${activeGroupId || '—'} 里没有 key = ${activeKey || '—'} 的成员；它可能刚被删除，或分组配置刚被改动。`"
      >
        <template #icon><ShieldAlert :size="28" /></template>
        <Btn size="sm" @click="closeModelDetail">返回分组</Btn>
      </Empty>
    </Card>

    <template v-else>
      <!-- ---------------------------- 冷却状态 ---------------------------- -->
      <Alert v-if="member.cooling" tone="warn">
        <div class="flex flex-wrap items-center gap-2">
          <ShieldAlert :size="14" />
          <span class="font-medium">该模型正在冷却</span>
          <Badge :tone="kindInfo(member.cooldown_kind).tone">{{ kindInfo(member.cooldown_kind).label }}</Badge>
          <span class="text-ink-2">·</span>
          <span>{{ cooldownRemain === null ? "剩余时间未知" : `剩余 ${fmtRemain(cooldownRemain)}` }}</span>
          <span v-if="String(member.cooldown_kind ?? '').toLowerCase() === 'auth'" class="text-err-600">
            需要人工解除
          </span>
        </div>
      </Alert>

      <!-- ------------------------------ 概览 ------------------------------ -->
      <Card>
        <div class="mb-3 flex flex-wrap items-center gap-2">
          <Waypoints :size="15" class="text-ink-3" />
          <h2 class="text-[13.5px] font-semibold text-ink">概览</h2>
          <span class="ml-auto text-[12px] text-ink-3">{{ lastAt ? `更新于 ${fmtClock(lastAt)}` : "" }}</span>
        </div>

        <div class="grid grid-cols-2 gap-3 sm:grid-cols-4">
          <div class="rounded-lg border border-line-soft bg-app px-3 py-2.5" data-test="member-requests">
            <p class="text-[11.5px] text-ink-3">请求数</p>
            <p class="mt-0.5 font-mono text-[17px] font-semibold tabular-nums text-ink">{{ fmtInt(member.requests) }}</p>
          </div>
          <div class="rounded-lg border border-line-soft bg-app px-3 py-2.5" data-test="member-failed">
            <p class="text-[11.5px] text-ink-3">失败尝试数</p>
            <p class="mt-0.5 font-mono text-[17px] font-semibold tabular-nums" :class="num(member.failed_attempts) ? 'text-err-600' : 'text-ink'">{{ fmtInt(member.failed_attempts) }}</p>
          </div>
          <div class="rounded-lg border border-line-soft bg-app px-3 py-2.5" data-test="member-loop-retries">
            <p class="flex items-center gap-1 text-[11.5px] text-ink-3"><Repeat :size="11" />死循环拦截</p>
            <p class="mt-0.5 font-mono text-[17px] font-semibold tabular-nums text-ink">{{ fmtInt(member.loop_retries) }}</p>
          </div>
          <div class="rounded-lg border border-line-soft bg-app px-3 py-2.5">
            <p class="text-[11.5px] text-ink-3">平均耗时</p>
            <p class="mt-0.5 font-mono text-[17px] font-semibold tabular-nums text-ink">{{ fmtSec(member.avg_duration_ms) }}</p>
          </div>
        </div>

        <dl class="mt-3 grid grid-cols-1 gap-x-5 gap-y-1.5 text-[12.5px] sm:grid-cols-2">
          <div class="flex items-baseline justify-between gap-2 border-t border-line-soft pt-1.5">
            <dt class="shrink-0 text-ink-3">平均 TTFT</dt>
            <dd class="font-mono tabular-nums text-ink">{{ fmtSec(member.avg_ttft_ms) }}</dd>
          </div>
          <div class="flex items-baseline justify-between gap-2 border-t border-line-soft pt-1.5">
            <dt class="shrink-0 text-ink-3">tokens 输入 / 输出</dt>
            <dd class="font-mono tabular-nums text-ink">{{ fmtInt(member.input_tokens) }} / {{ fmtInt(member.output_tokens) }}</dd>
          </div>
          <div class="flex items-baseline justify-between gap-2 border-t border-line-soft pt-1.5">
            <dt class="shrink-0 text-ink-3">成本</dt>
            <dd class="font-mono tabular-nums text-ink">{{ fmtCost(member.cost_usd) }}</dd>
          </div>
          <div class="flex items-baseline justify-between gap-2 border-t border-line-soft pt-1.5">
            <dt class="shrink-0 text-ink-3">最近使用</dt>
            <dd class="truncate text-ink" :title="fmtText(member.last_used_ts)">{{ fmtTs(member.last_used_ts) }}</dd>
          </div>
          <div class="flex items-baseline justify-between gap-2 border-t border-line-soft pt-1.5">
            <dt class="shrink-0 text-ink-3">层级 / 权重</dt>
            <dd class="text-ink">{{ tierName(member.tier) }} · 第 {{ fmtInt(member.slot) }} 位 · 权重 {{ fmtInt(member.weight) }}</dd>
          </div>
          <div class="flex items-baseline justify-between gap-2 border-t border-line-soft pt-1.5">
            <dt class="shrink-0 text-ink-3">状态</dt>
            <dd class="flex flex-wrap items-center gap-1.5 text-ink">
              <Badge :tone="member.enabled ? 'ok' : 'err'">{{ member.enabled ? "启用" : "禁用" }}</Badge>
              <Badge v-if="isActive" tone="teal">当前使用</Badge>
              <Badge v-if="member.cooling" :tone="kindInfo(member.cooldown_kind).tone">冷却中</Badge>
            </dd>
          </div>
          <div class="flex items-baseline justify-between gap-2 border-t border-line-soft pt-1.5">
            <dt class="shrink-0 text-ink-3">暴露模型</dt>
            <dd class="truncate font-mono text-[11.5px] text-ink" :title="fmtText(member.exposed_model)">{{ fmtText(member.exposed_model) }}</dd>
          </div>
          <div class="flex items-baseline justify-between gap-2 border-t border-line-soft pt-1.5">
            <dt class="shrink-0 text-ink-3">最近结果</dt>
            <dd class="flex min-w-0 items-center gap-1.5 text-ink">
              <Badge :tone="member.last_outcome ? outcomeInfo(member.last_outcome).tone : 'gray'">{{ member.last_outcome ? outcomeInfo(member.last_outcome).label : "—" }}</Badge>
              <span v-if="errorClass(member.last_error_class).text" class="truncate font-mono text-[11.5px] text-ink-2" :title="fmtText(member.last_error_class)">{{ errorClass(member.last_error_class).text }}</span>
            </dd>
          </div>
        </dl>
      </Card>

      <div class="grid gap-3.5 lg:grid-cols-2">
        <!-- --------------------------- 失败统计 --------------------------- -->
        <Card>
          <div class="mb-3 flex flex-wrap items-center gap-2">
            <ShieldAlert :size="15" class="text-ink-3" />
            <h2 class="text-[13.5px] font-semibold text-ink">失败统计</h2>
            <span class="ml-auto text-[12px] text-ink-3">
              {{ errorClassRows.length ? `${errorClassRows.length} 类 · 共 ${fmtInt(member.failed_attempts)} 次` : "暂无失败" }}
            </span>
          </div>

          <Empty v-if="!errorClassRows.length" title="暂无失败" hint="这个模型还没有失败尝试；一旦出现失败，会按错误类在这里汇总" />

          <ul v-else class="space-y-1.5">
            <li
              v-for="(row, i) in errorClassRows" :key="row.error_class + '|' + i"
              class="flex items-center gap-2 rounded-lg border border-line-soft bg-app px-3 py-1.5"
              data-test="member-error-class"
            >
              <Badge :tone="errorClass(row.error_class).tone">{{ errorClass(row.error_class).text }}</Badge>
              <span class="ml-auto font-mono tabular-nums text-[12.5px] text-ink-2">{{ fmtInt(row.n) }} 次</span>
            </li>
          </ul>
        </Card>

        <!-- ------------------------- 最近失败明细 ------------------------- -->
        <Card :pad="false" class="overflow-hidden">
          <div class="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
            <ScrollText :size="15" class="text-ink-3" />
            <h2 class="text-[13.5px] font-semibold text-ink">最近失败明细</h2>
            <span class="ml-auto text-[12px] text-ink-3">{{ failureRows.length ? `${failureRows.length} 条` : "暂无失败" }}</span>
          </div>

          <Empty
            v-if="!failureRows.length"
            title="最近没有失败记录"
            :hint="`本分组最近 ${LIMIT} 条切换日志里，没有这个模型的失败尝试`"
          >
            <template #icon><ScrollText :size="28" /></template>
          </Empty>

          <div v-else class="overflow-x-auto">
            <table class="w-full text-[12.5px]">
              <thead>
                <tr>
                  <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">时间</th>
                  <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">尝试#</th>
                  <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">层级</th>
                  <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">结果</th>
                  <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">错误类</th>
                  <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">HTTP</th>
                  <th class="whitespace-nowrap px-3 py-2 text-right text-[11.5px] font-medium uppercase tracking-wide text-ink-3">耗时</th>
                  <th class="whitespace-nowrap px-3 py-2 text-left text-[11.5px] font-medium uppercase tracking-wide text-ink-3">说明</th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="(row, i) in failureRows" :key="(row.request_id || 'r') + '|' + (row.attempt_no ?? i) + '|' + i" class="border-t border-line-soft hover:bg-app">
                  <td class="whitespace-nowrap px-3 py-2 text-ink-2" :title="fmtText(row.ts)">{{ fmtTs(row.ts) }}</td>
                  <td class="whitespace-nowrap px-3 py-2 text-right font-mono tabular-nums text-ink-2">{{ fmtInt(row.attempt_no) }}</td>
                  <td class="whitespace-nowrap px-3 py-2 text-ink-2">{{ tierName(row.tier) }}</td>
                  <td class="whitespace-nowrap px-3 py-2"><Badge :tone="outcomeInfo(row.outcome).tone">{{ outcomeInfo(row.outcome).label }}</Badge></td>
                  <td class="whitespace-nowrap px-3 py-2"><Badge :tone="errorClass(row.error_class).tone">{{ errorClass(row.error_class).text || "—" }}</Badge></td>
                  <td class="whitespace-nowrap px-3 py-2 text-right font-mono tabular-nums text-ink-2">{{ fmtInt(row.http_status) }}</td>
                  <td class="whitespace-nowrap px-3 py-2 text-right font-mono tabular-nums text-ink-2">{{ fmtSec(row.latency_ms) }}</td>
                  <td class="max-w-[320px] truncate px-3 py-2 text-ink-3" :title="fmtText(row.message)">{{ fmtText(row.message) }}</td>
                </tr>
              </tbody>
            </table>
          </div>
        </Card>
      </div>
    </template>
  </div>
</template>
