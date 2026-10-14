<script setup lang="ts">
// 分组路由：分组列表 + 每组的概览统计与实时并发。
//
// 编辑去哪了（2026-10-13 重构，同日二次调整）：
//  · 分组本身的设置（名称 / 暴露模型 / 重试与超时）→ 分组详情页里内联改，看板与设置同屏；
//  · 路由层级 / 成员 / 权重（也就是「模型配置」）→ 同样内联在分组详情页里，不再单独开子页；
//  · 本页只剩两件事：把分组列出来、把每组的统计与「使用中」实时并发显示出来。
// 因此本页是只读的（除了删除分组），不再有编辑弹窗，也不再有 draft/校验/保存逻辑。
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { ChevronDown, LayoutDashboard, Plus, RefreshCw, Trash2, Waypoints } from "@lucide/vue";
import { Badge, Btn, Card, Empty, IconBtn } from "../components/ui";
import { openGroupDetail, openModelDetail } from "../nav";
import { apiKeys, bindingLabel, channelName, groups, groupsUsingModel, markDirty, models, state, uid } from "../store";
import { toast } from "../toast";
import { fmtSec } from "../format";
import {
  api, type Group, type GroupMember, type GroupMembersPayload, type GroupStatRow,
  type InflightSnapshot, type StatsPayload, type Tier,
} from "../api";

/* ------------------------------- 展示辅助 ------------------------------- */

const TIER_NAMES = ["一级", "二级", "三级", "四级", "五级", "六级"];

function tierName(level: number): string {
  return TIER_NAMES[level - 1] ?? `${level} 级`;
}
function tierTone(level: number): "brand" | "gray" {
  return level === 1 ? "brand" : "gray";
}
// 两种「在用」口径并存，别混：
//  · 成员 chip 的背景色是**静态**口径 —— level===1 表示声明上优先级最高的一层（模板里内联成 t.level === 1），
//    与此刻有没有流量无关；
//  · 「使用中」徽章是**实时**口径 —— 此刻真的在用（in-flight 并发 > 0），唯一数据来源是
//    GET /admin/api/inflight（groups: group_id → 并发数；members: "<渠道>::<上游模型>" → 并发数；缺键即 0），
//    本页每 5s 拉一次。旧口径 member.active / active_key 是「最近一次成功请求的落点」（last-success），
//    与此刻是否在用无关，属于假状态，已不再用于徽章。
function sortedTiers(g: Group): Tier[] {
  return [...(g.tiers ?? [])].sort((a, b) => a.level - b.level);
}
function keysOf(groupId: string) {
  return apiKeys.value.filter(k => k.group_id === groupId);
}
function exposedOf(g: Group): string[] {
  return g.exposed_models ?? [];
}

/** 全局路由口径：false = 同级按声明顺序（默认）；true = 同级按权重降序。只影响文案，不影响本页只读展示。 */
const autoWeight = computed(() => state.cfg?.auto_weight ?? false);

/* --------------------------- 成员定位与实时并发口径 --------------------------- */
// 同一 model_id 可以同时出现在多个层级里，所以成员一律用 (tier, model_id) 复合定位，
// 不能只用 model_id（否则 chip 会取到别层的数字）。
function memberOf(groupId: string, tierLevel: number, modelId: string): GroupMember | null {
  const p = membersByGroup.value[groupId];
  if (!p) return null;
  return p.members.find(m => m.model_id === modelId && m.tier === tierLevel) ?? null;
}
/**
 * 该成员在实时并发快照里的键："<渠道>::<上游模型>"（与 /admin/api/inflight 的 members 键一致）。
 * 优先用服务端 group-members 给的 key；端点不可用时用本地绑定推一个等价 key（与 openMember 同款）。
 */
function memberKey(groupId: string, tierLevel: number, modelId: string): string {
  const fromServer = memberOf(groupId, tierLevel, modelId)?.key;
  if (fromServer) return fromServer;
  const b = models.value.find(x => x.id === modelId);
  return b ? `${b.channel_id}::${b.upstream}` : "";
}
/** 该成员此刻的并发数：优先成员级快照（服务端按 key 聚合），缺失时退回全组口径。 */
/**
 * 该成员此刻的并发数：优先成员级快照（服务端按 key 聚合），缺失时退回全组口径。
 * ★非响应依赖：它读 inflight.value，但函数本身没有 .value 属性（memberInflight.value 恒为 undefined）。
 * 想在模板或计算属性里判断「在用」，一律用下面的 memberInUse()，不要直接用这个原始数字。
 */
function memberInflight(groupId: string, tierLevel: number, modelId: string): number {
  const key = memberKey(groupId, tierLevel, modelId);
  const fromKey = key ? inflight.value?.members?.[key] : undefined;
  if (typeof fromKey === "number") return fromKey;
  return memberOf(groupId, tierLevel, modelId)?.inflight ?? 0;
}
/**
 * 该成员此刻是否真的在用（**真实 in-flight 并发 > 0**，唯一数据来源 GET /admin/api/inflight）。
 * ★必须是普通函数、不可改写成 computed：computed 会让 memberInUse.value 恒为 undefined，
 * 在模板里就是恒假 —— 「使用中」高亮会悄悄永不出现，属于假绿。
 */
function memberInUse(groupId: string, tierLevel: number, modelId: string): boolean {
  return memberInflight(groupId, tierLevel, modelId) > 0;
}
/** 高亮的 title：把「在被谁用、并发多少」说清楚，避免与一级的静态色调混淆。 */
function memberInUseTitle(groupId: string, tierLevel: number, modelId: string): string {
  const g = groups.value.find(x => x.id === groupId);
  const n = memberInflight(groupId, tierLevel, modelId);
  return (g?.name ?? groupId) + "：此刻正在使用该 (渠道, 上游模型) 的并发请求数 " + n;
}
function memberRequests(groupId: string, tierLevel: number, modelId: string): number {
  return memberOf(groupId, tierLevel, modelId)?.requests ?? 0;
}
/** 数字徽章的 title：把「没有数据」和「真的是 0 次」分开说，避免误导成「从来没请求过」。 */
function memberRequestsTitle(groupId: string, tierLevel: number, modelId: string): string {
  const p = membersByGroup.value[groupId];
  if (!p) return "成员数据不可用";
  const n = memberOf(groupId, tierLevel, modelId)?.requests ?? 0;
  return n > 0 ? `本分组内该成员成功请求 ${n} 次` : "本分组内该成员成功请求 0 次";
}
/** 该分组此刻正在处理中的请求数（/admin/api/inflight 的 groups 口径；缺键即 0）。 */
function groupInflight(groupId: string): number {
  return inflight.value?.groups?.[groupId] ?? 0;
}
/** 该层级此刻的并发数 = 该层各成员并发之和。 */
function tierInflight(g: Group, t: Tier): number {
  return (t.members ?? []).reduce((n, m) => n + memberInflight(g.id, t.level, m.model_id), 0);
}
/** 点成员 chip → 模型详情页（返回时仍停在同一个分组详情页）。 */
function openMember(g: Group, tierLevel: number, modelId: string) {
  const key = memberKey(g.id, tierLevel, modelId);
  if (!key) return;
  openModelDetail({ groupId: g.id, key, modelId });
}
/**
 * 「最多 N 轮」里的 N：优先取该分组的**生效** max_cycles（group-members 响应里的 retry
 * 已经是服务端算好的 group ?? global 生效值），端点不可用时退回全局值。
 * 分组有自己 overrides.retry 时全局值不准，所以不能像旧版那样只读 state.cfg.retry。
 */
function maxCyclesOf(g: Group): number {
  return membersByGroup.value[g.id]?.retry?.max_cycles ?? state.cfg?.retry.max_cycles ?? 2;
}

/* ------------------------------ 分组统计 ------------------------------ */
const stats = ref<StatsPayload | null>(null);
const statsErr = ref("");
const statsLoading = ref(false);
const statsAt = ref(0);

// ★这一整段统计辅助（statNum/fmt*/statOf/stat*/sum*/STATS_UNAVAILABLE/loadStats）在 2026-10-13 的
// 「编辑弹窗搬去详情页」重构里被切片脚本整段删掉过：vite build 与 tsc 都不校验 <script setup> 里
// 模板引用的标识符是否存在，直到真机运行才以 `TypeError: statOf is not a function` 炸掉整个分组卡片列表。
// 今后重写 SFC 必须逐个人工核对模板里出现的每个名字。
function statNum(v: unknown): number | null {
  if (v === null || v === undefined || v === "") return null;
  const n = typeof v === "number" ? v : Number(v);
  return Number.isFinite(n) ? n : null;
}
function fmtInt(v: unknown): string {
  const n = statNum(v);
  return n === null ? "0" : Math.round(n).toLocaleString("en-US");
}
/** 大数缩写：12.3k / 1.2M（统计条每格只有窄窄一栏，完整千分位留给 title）。 */
function fmtCompact(v: unknown): string {
  const n = statNum(v);
  if (n === null) return "0";
  const a = Math.abs(n);
  if (a >= 1e9) return (n / 1e9).toFixed(1) + "B";
  if (a >= 1e6) return (n / 1e6).toFixed(1) + "M";
  if (a >= 1e4) return (n / 1e3).toFixed(1) + "k";
  return Math.round(n).toLocaleString("en-US");
}
/** 与 GroupDetail 用量页同款：$ + 6 位小数。 */
function fmtCost(v: unknown): string {
  const n = statNum(v);
  return n === null ? "$0.000000" : `$${n.toFixed(6)}`;
}
function fmtClock(ms: number): string {
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

// 服务端按 group_id 精确分组；历史遗留行可能是空串，匹配不到就按「无数据」处理。
const statsByGroup = computed<Record<string, GroupStatRow>>(() => {
  const out: Record<string, GroupStatRow> = {};
  const rows = Array.isArray(stats.value?.by_group) ? stats.value!.by_group : [];
  for (const r of rows) {
    const id = String(r?.group_id ?? "");
    if (id) out[id] = r;
  }
  return out;
});
function statOf(groupId: string): GroupStatRow | null {
  return statsByGroup.value[groupId] ?? null;
}
/** 请求数：没有该分组的行时按 0 显示。 */
function statRequests(groupId: string): number {
  return statNum(statOf(groupId)?.requests) ?? 0;
}
/** 成功率：requests=0 时返回 null，模板里显示「—」。 */
function statRate(groupId: string): number | null {
  const row = statOf(groupId);
  const total = statNum(row?.requests) ?? 0;
  if (total <= 0) return null;
  return ((statNum(row?.success) ?? 0) / total) * 100;
}
function statRateText(groupId: string): string {
  const r = statRate(groupId);
  return r === null ? "—" : `${r.toFixed(1)}%`;
}
function statCost(groupId: string): string {
  return fmtCost(statOf(groupId)?.cost_usd ?? 0);
}
/** 平均耗时：服务端只对成功请求求平均，无成功请求时为 null → 「—」。 */
function statAvg(groupId: string): string {
  return fmtSec(statOf(groupId)?.avg_duration_ms ?? null);
}
function statTokens(groupId: string): { short: string; full: string } {
  const row = statOf(groupId);
  const inTok = statNum(row?.input_tokens) ?? 0;
  const outTok = statNum(row?.output_tokens) ?? 0;
  return {
    short: `${fmtCompact(inTok)} / ${fmtCompact(outTok)}`,
    full: `${fmtInt(inTok)} / ${fmtInt(outTok)}`,
  };
}

/** 接口失败时统计条统一显示这行灰字（不报错、不占位）。 */
const STATS_UNAVAILABLE = "统计不可用";

// summary 是服务端 json_object 拼的（server/src/store.rs）：requests / success /
// cost_usd / input_tokens / output_tokens …；空库时 SUM 系列为 null，一律按 0 兜底。
const sumRequests = computed(() => statNum(stats.value?.summary?.requests) ?? 0);
const sumCost = computed(() => fmtCost(statNum(stats.value?.summary?.cost_usd) ?? 0));

let statsInflight = false;
/** 拉全量统计：并发去重（连点不叠加），失败只写 statsErr，不抛未捕获异常。 */
async function loadStats() {
  if (statsInflight) return;
  statsInflight = true;
  statsLoading.value = true;
  try {
    const payload = (await api.stats()) as StatsPayload;
    stats.value = payload ?? null;
    statsErr.value = "";
    statsAt.value = Date.now();
  } catch (e: any) {
    statsErr.value = String(e?.message || e);
  } finally {
    statsInflight = false;
    statsLoading.value = false;
  }
}

/* --------------------------- 成员级数据（请求数 / 失败） --------------------------- */
// 数据来自 GET /admin/api/group-members（每个分组一份，进页面并发拉一次）。
// 服务端还没这个端点（老版本 / 404）或单个分组失败时整块静默降级：chip 显示「0 次」，不弹错、
// 不写 statsErr，页面其余部分照常。★「使用中」徽章不依赖这份数据（它来自 /admin/api/inflight），
// 端点缺失时仍由实时并发快照决定。
const membersByGroup = ref<Record<string, GroupMembersPayload>>({});
// 防重入 + 「跑完再补一次」：配置在页面停留期间被换掉（顶栏「重新加载配置」）时，
// 键集合会变，必须在当前这轮结束后再拉一次，否则会漏掉一次刷新。
let membersRun: Promise<void> | null = null;
let membersAgain = false;
async function loadGroupMembers() {
  if (!groups.value.length) { membersByGroup.value = {}; return; }
  if (membersRun) { membersAgain = true; return membersRun; }
  membersRun = (async () => {
    const pairs = await Promise.all(groups.value.map(async (g) => {
      try { return [g.id, await api.groupMembers(g.id)] as const; }
      catch { return null; } // 单组失败不拖累其它组，也不冒泡成未捕获异常
    }));
    const out: Record<string, GroupMembersPayload> = {};
    for (const p of pairs) if (p) out[p[0]] = p[1];
    membersByGroup.value = out;
  })();
  try { await membersRun; } finally {
    membersRun = null;
    if (membersAgain) { membersAgain = false; void loadGroupMembers(); }
  }
}
// 分组集合变了（例如点过「重新加载配置」）就重新拉，避免旧 group_id 的陈旧数字。
watch(() => groups.value.map(g => g.id).join(","), () => { void loadGroupMembers(); });

/* ------------------------------ 实时并发（「使用中」的唯一依据） ------------------------------ */
// GET /admin/api/inflight：groups = group_id → 该分组此刻正在处理中的请求数，
// members = "<渠道>::<上游模型>" → 并发数。这是「使用中」的**唯一**数据来源（task-40）：
// 只有并发 > 0 才算在用。统计与成员数据仍然刻意不轮询（进去一次 + 手动刷新），
// 这里单独按 5s 轮询（与分组详情页同频），错了就静默保留上一帧，不要把徽章抖没、也不要弹错。
const inflight = ref<InflightSnapshot | null>(null);
async function refreshInflight() {
  try {
    inflight.value = await api.inflight();
  } catch {
    // 静默保留上一次快照：管理面瞬时抖动不该让「使用中」闪断。
  }
}
let inflightTimer: ReturnType<typeof setInterval> | undefined;
onMounted(() => {
  void refreshInflight();
  inflightTimer = setInterval(() => void refreshInflight(), 5000);
});
onBeforeUnmount(() => {
  if (inflightTimer !== undefined) {
    clearInterval(inflightTimer);
    inflightTimer = undefined;
  }
});

/** 工具条的刷新按钮：统计与成员数据一起重拉（两者都做了防重入）。 */
function refreshAll() { void loadStats(); void loadGroupMembers(); }

/* ------------------------------- 新建分组 ------------------------------- */
// 新增分组不再开弹窗：直接建一个空壳塞进配置、进详情页填内容。
// 只进内存（markDirty），要落库仍然得点顶栏「保存」—— 与其它页面同一套语义。
// 起手给一个一级空层，用户到了详情页的模型配置区就能直接加成员，不用先点「添加一级」。
function createGroup() {
  const cfg = state.cfg;
  if (!cfg) return;
  const g: Group = {
    id: uid("grp"),
    name: "新分组",
    exposed_models: [],
    tiers: [{ level: 1, members: [] }],
    overrides: { retry: null },
  };
  cfg.groups.push(g);
  markDirty();
  toast.ok("已添加分组，接着填名称、暴露模型与模型配置");
  openGroupDetail(g.id);
}

/* ------------------------------- 删除影响面 ------------------------------- */

const pendingDelete = ref<Group | null>(null);
const confirming = ref(false);

function removeGroup(g: Group) {
  const cfg = state.cfg;
  if (!cfg) return;
  pendingDelete.value = g;
  confirming.value = true;
}

/** 该分组下的密钥数量（删除确认里要如实告知会连带移除多少条）。 */
const pendingKeys = computed(() => {
  const g = pendingDelete.value;
  return g ? keysOf(g.id).length : 0;
});
/** 其中已启用的条数：停用的密钥不会让客户端失去访问权限，分开说更准确。 */
const pendingKeysEnabled = computed(() => {
  const g = pendingDelete.value;
  return g ? keysOf(g.id).filter(k => k.enabled).length : 0;
});
/** 删除分组还会让它原本负责的暴露模型名失去落点（这里不做拦截，只如实告知）。 */
const pendingExposed = computed(() => (pendingDelete.value ? exposedOf(pendingDelete.value).length : 0));

/**
 * 删除分组对本分组用到的模型绑定的影响：逐个用 groupsUsingModel 核对，
 * 删完之后还有没有别的分组引用它。没有的话这个绑定就「闲置」了——绑定本身保留，只是没人再用。
 */
const pendingGroupModels = computed(() => {
  const g = pendingDelete.value;
  if (!g) return { used: 0, orphaned: [] as string[] };
  const ids = [...new Set(g.tiers.flatMap(t => t.members.map(m => m.model_id)))];
  const orphaned = ids.filter(id => groupsUsingModel(id).every(x => x.id === g.id));
  return { used: ids.length, orphaned };
});

/** 确认删除：WebView2 里 window.confirm 不可用，改用页面内 Modal。 */
function confirmRemoveGroup() {
  const cfg = state.cfg;
  const g = pendingDelete.value;
  if (!cfg || !g) return;
  cfg.groups = cfg.groups.filter(x => x.id !== g.id);
  const before = cfg.api_keys.length;
  cfg.api_keys = cfg.api_keys.filter(k => k.group_id !== g.id);
  const removed = before - cfg.api_keys.length;
  markDirty();
  confirming.value = false;
  const orphaned = pendingGroupModels.value.orphaned.length;
  const tail = [
    removed ? `并移除 ${removed} 个密钥` : "",
    pendingExposed.value ? `${pendingExposed.value} 个暴露模型名失去落点` : "",
    orphaned ? `${orphaned} 个模型绑定不再被任何分组使用` : "",
  ].filter(Boolean).join("，");
  toast.ok(tail ? `已删除分组「${g.name}」，${tail}` : `已删除分组「${g.name}」`);
}
</script>

<!--
  拖动排序的全局兜底：抓手在分组详情页的模型配置区里，但指针会跑到 window 上。
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
  <div>
    <!-- 工具条：左侧统计，右侧主操作 -->
    <div class="mb-3 flex flex-wrap items-center gap-2">
      <div class="text-[12px] text-ink-3">
        共 {{ groups.length }} 个分组
        <span v-if="groups.length">
          · {{ apiKeys.length }} 个密钥 · {{ models.length }} 个模型绑定
        </span>
        <!-- 全局累计来自 summary；拉取失败时退化成这一行灰字 -->
        <span v-if="statsErr"> · {{ STATS_UNAVAILABLE }}</span>
        <span v-else-if="stats">
          · 累计 {{ fmtInt(sumRequests) }} 次请求 · {{ sumCost }}
        </span>
      </div>
      <div class="ml-auto flex flex-wrap items-center gap-2">
        <span v-if="statsAt" class="font-mono text-[11.5px] text-ink-3">
          最后更新 {{ fmtClock(statsAt) }}
        </span>
        <Btn
          size="sm"
          variant="ghost"
          data-test="group-stats-refresh"
          title="重新拉取统计与成员数据"
          @click="refreshAll"
        >
          <RefreshCw :size="13" :class="statsLoading ? 'animate-spin' : ''" />刷新数据
        </Btn>
        <Btn variant="primary" size="sm" data-test="group-add-btn" @click="createGroup">
          <Plus :size="13" />添加分组
        </Btn>
      </div>
    </div>

    <!-- ------------------------------ 空态 ------------------------------ -->
    <Empty
      v-if="!groups.length"
      title="还没有分组"
      hint="分组决定请求在哪些渠道/模型之间按 一级→二级→三级 故障转移"
    >
      <template #icon><Waypoints :size="28" /></template>
      <Btn variant="primary" size="sm" data-test="group-add-empty-btn" @click="createGroup">
        <Plus :size="13" />添加分组
      </Btn>
    </Empty>

    <!-- ------------------------------ 分组列表 ------------------------------ -->
    <div v-else class="space-y-3.5">
      <Card v-for="g in groups" :key="g.id" :hover="true">
        <!-- 卡片头 -->
        <div class="flex items-start justify-between gap-3">
          <div class="min-w-0 flex-1">
            <div class="flex flex-wrap items-center gap-2">
              <span class="truncate text-[14px] font-semibold text-ink">{{ g.name }}</span>
              <Badge tone="brand">密钥 {{ keysOf(g.id).length }} 个</Badge>
              <Badge tone="gray">暴露模型 {{ exposedOf(g).length }} 个</Badge>
              <span
                v-if="groupInflight(g.id) > 0"
                data-test="group-active"
                class="inline-flex items-center gap-1 rounded-md bg-teal-100 px-1.5 py-0.5 text-[11.5px] font-medium text-teal-700"
                title="此刻正在使用本分组的并发请求数"
              >
                <span class="h-1.5 w-1.5 rounded-full bg-teal-500"></span>使用中 ×{{ groupInflight(g.id) }}
              </span>
            </div>
            <div class="mt-0.5 truncate font-mono text-[11.5px] text-ink-3">{{ g.id }}</div>
          </div>
          <div class="flex shrink-0 items-center gap-0.5">
            <IconBtn title="分组详情（含模型配置）" data-test="group-detail-btn" @click="openGroupDetail(g.id)">
              <LayoutDashboard :size="15" />
            </IconBtn>
            <IconBtn title="删除分组" danger @click="removeGroup(g)"><Trash2 :size="15" /></IconBtn>
          </div>
        </div>

        <!-- 分组统计：插在卡片头与「暴露模型」之间，进页面拉一次全量数据后本地按 group_id 取用 -->
        <div data-test="group-stats" class="mt-3.5 border-t border-line-soft pt-3.5">
          <div v-if="statsErr" class="text-[12px] text-ink-3">{{ STATS_UNAVAILABLE }}</div>
          <div v-else class="grid grid-cols-2 gap-x-4 gap-y-3 sm:grid-cols-5">
            <div>
              <div class="text-[11.5px] uppercase tracking-wide text-ink-3">请求数</div>
              <div
                data-stat="requests"
                class="mt-0.5 text-[15px] font-semibold tabular-nums text-ink"
                :title="fmtInt(statOf(g.id)?.requests) + ' 次请求'"
              >{{ fmtInt(statRequests(g.id)) }}</div>
            </div>
            <div>
              <div class="text-[11.5px] uppercase tracking-wide text-ink-3">成功率</div>
              <div
                data-stat="success"
                class="mt-0.5 text-[15px] font-semibold tabular-nums text-ink"
                :title="'成功 ' + fmtInt(statOf(g.id)?.success) + ' 次'"
              >{{ statRateText(g.id) }}</div>
            </div>
            <div>
              <div class="text-[11.5px] uppercase tracking-wide text-ink-3">成本</div>
              <div data-stat="cost" class="mt-0.5 text-[15px] font-semibold tabular-nums text-ink">
                {{ statCost(g.id) }}
              </div>
            </div>
            <div>
              <div class="text-[11.5px] uppercase tracking-wide text-ink-3">平均耗时</div>
              <div data-stat="avg" class="mt-0.5 text-[15px] font-semibold tabular-nums text-ink">
                {{ statAvg(g.id) }}
              </div>
            </div>
            <div>
              <div class="text-[11.5px] uppercase tracking-wide text-ink-3">Tokens</div>
              <div
                data-stat="tokens"
                class="mt-0.5 text-[15px] font-semibold tabular-nums text-ink"
                :title="statTokens(g.id).full + '（输入 / 输出）'"
              >{{ statTokens(g.id).short }}</div>
            </div>
          </div>
        </div>

        <!-- 暴露模型 -->
        <div class="mt-3.5 border-t border-line-soft pt-3.5">
          <div class="mb-2 flex items-center justify-between gap-2">
            <div class="text-[11.5px] font-medium uppercase tracking-wide text-ink-3">暴露模型</div>
            <span class="text-[11.5px] text-ink-3">{{ exposedOf(g).length }} 个</span>
          </div>
          <div v-if="exposedOf(g).length" class="flex flex-wrap gap-1.5">
            <Badge v-for="m in exposedOf(g)" :key="m" tone="teal">
              <span class="font-mono">{{ m }}</span>
            </Badge>
          </div>
          <p v-else class="text-[12px] text-ink-3">未设置暴露模型</p>
          <p class="mt-2 text-[11.5px] leading-4 text-ink-3" data-test="group-exposed-hint">
            分组名「{{ g.name }}」与 id「{{ g.id }}」也可以直接当模型名用：客户端用它们请求时，组内成员按上面的层级自动轮换。
          </p>
        </div>

        <!-- 故障转移层级 -->
        <div class="mt-3.5 border-t border-line-soft pt-3.5">
          <div class="mb-2.5 flex items-center justify-between gap-2">
            <div class="text-[11.5px] font-medium uppercase tracking-wide text-ink-3">故障转移层级</div>
            <Btn size="sm" variant="ghost" data-test="group-models-inline-btn" @click="openGroupDetail(g.id)">
              <Waypoints :size="12" />模型配置
            </Btn>
          </div>

          <div v-if="sortedTiers(g).length">
            <template v-for="(t, i) in sortedTiers(g)" :key="t.level + '-' + i">
              <div v-if="i > 0" class="flex items-center justify-center py-1 text-ink-3">
                <ChevronDown :size="14" />
              </div>
              <div class="flex items-start gap-3">
                <div class="flex w-12 shrink-0 items-center pt-1.5">
                  <Badge :tone="tierTone(t.level)">{{ tierName(t.level) }}</Badge>
                </div>
                <div class="flex min-w-0 flex-1 flex-wrap items-center gap-1.5">
                  <template v-if="t.members.length">
                    <div
                      v-for="(m, mi) in t.members"
                      :key="t.level + '-' + m.model_id"
                      data-test="member-chip"
                      :data-in-use="memberInUse(g.id, t.level, m.model_id) ? '1' : '0'"
                      class="flex cursor-pointer items-center gap-2 rounded-[10px] border px-2.5 py-1 transition-colors hover:border-teal-300"
                      :class="memberInUse(g.id, t.level, m.model_id)
                        ? 'border-teal-400 bg-teal-100 ring-2 ring-teal-500/35'
                        : (t.level === 1
                          ? 'border-teal-200 bg-teal-50/60'
                          : 'border-line bg-app')"
                      :title="memberInUse(g.id, t.level, m.model_id)
                        ? memberInUseTitle(g.id, t.level, m.model_id)
                        : '查看 ' + bindingLabel(m.model_id) + ' 的详情'"
                      @click="openMember(g.id, t.level, m.model_id)"
                    >
                      <span
                        v-if="memberInUse(g.id, t.level, m.model_id)"
                        class="relative flex h-2 w-2 shrink-0"
                        aria-hidden="true"
                      >
                        <span class="absolute inline-flex h-full w-full animate-ping rounded-full bg-teal-500 opacity-70"></span>
                        <span class="relative inline-flex h-2 w-2 rounded-full bg-teal-600"></span>
                      </span>
                      <span
                        class="font-mono text-[12.5px] text-ink"
                      >{{ bindingLabel(m.model_id) }}</span>
                      <Badge tone="gray" :data-test="autoWeight ? 'badge-weight' : 'badge-order'">
                        {{ autoWeight ? `权重 ${m.weight}` : `顺序 ${mi + 1} · 权重 ${m.weight}` }}
                      </Badge>
                      <Badge
                        tone="gray"
                        :data-test="'member-requests-' + t.level + '-' + m.model_id"
                        :title="memberRequestsTitle(g.id, t.level, m.model_id)"
                      >{{ memberRequests(g.id, t.level, m.model_id) }} 次</Badge>
                    </div>
                  </template>
                  <span v-else class="py-1 text-[12px] text-ink-3">该级无成员</span>
                  <span
                    v-if="tierInflight(g, t) > 0"
                    data-test="member-active"
                    class="ml-1 inline-flex shrink-0 items-center gap-1 rounded-md bg-teal-100 px-1.5 py-0.5 text-[11.5px] font-medium text-teal-700"
                    title="此刻正在使用本层的并发请求数"
                  >
                    <span class="h-1.5 w-1.5 rounded-full bg-teal-500"></span>使用中 ×{{ tierInflight(g, t) }}
                  </span>
                </div>
              </div>
            </template>
            <div class="mt-2.5 pl-[60px] text-[11.5px] text-ink-3">
              ↑ 全部失败 → 回到一级（最多 {{ maxCyclesOf(g) }} 轮）
            </div>
          </div>
          <p v-else class="text-[12px] text-ink-3">未配置层级，进「分组详情」加一层。</p>
        </div>
      </Card>
    </div>

    <!-- 删除确认：WebView2 里 window.confirm 不可用，改用页面内 Modal -->
    <Modal
      :open="confirming"
      title="删除分组"
      width="440px"
      @close="confirming = false"
    >
      <p class="text-[12.5px] leading-5 text-ink-2">
        确定删除分组「<span class="font-medium text-ink">{{ pendingDelete?.name }}</span>」？
      </p>
      <p v-if="pendingKeys" class="mt-1.5 text-[11.5px] leading-4 text-ink-3">
        同时会移除 {{ pendingKeys }} 个指向该分组的密钥<template v-if="pendingKeysEnabled">（其中 {{ pendingKeysEnabled }} 个已启用）</template>，使用这些密钥的客户端会立即失去访问权限。
      </p>
      <p v-if="pendingExposed" class="mt-1 text-[11.5px] leading-4 text-ink-3">
        该分组的 {{ pendingExposed }} 个暴露模型名将不再由本网关提供，客户端用这些名字发起的请求会直接失败。
      </p>
      <p v-if="pendingGroupModels.used" class="mt-1 text-[11.5px] leading-4 text-ink-3">
        本分组用到 {{ pendingGroupModels.used }} 个模型绑定<template v-if="pendingGroupModels.orphaned.length">，其中
        {{ pendingGroupModels.orphaned.length }} 个（{{ pendingGroupModels.orphaned.map(bindingLabel).join("、") }}）删除后不再被任何分组使用</template><template v-else>，都还被其它分组引用</template>。
      </p>
      <p class="mt-2.5 rounded-lg border border-line bg-app/60 px-2.5 py-2 text-[11.5px] leading-4 text-ink-3">
        层级里的模型绑定本身不会被删除，其它分组仍可继续使用。
      </p>

      <template #footer>
        <Btn data-test="cancel" @click="confirming = false">取消</Btn>
        <Btn data-test="confirm" variant="danger" @click="confirmRemoveGroup"><Trash2 :size="13" /> 删除</Btn>
      </template>
    </Modal>

  </div>
</template>
