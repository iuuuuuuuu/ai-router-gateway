<script setup lang="ts">
// 分组路由：一级 → 二级 → 三级 的故障转移可视化与编辑。
// 页面只改 state.cfg 并 markDirty()，保存由顶栏统一负责。
import { computed, onMounted, ref } from "vue";
import {
  ArrowDown, ArrowUp, ChevronDown, LayoutDashboard, Pencil, Plus, RefreshCw, Trash2, Waypoints,
} from "@lucide/vue";
import {
  Alert, Badge, Btn, Card, Empty, Field, IconBtn, Modal,
  TInput, TSelect, TTextarea,
} from "../components/ui";
import { openGroupDetail } from "../nav";
import {
  apiKeys, bindingLabel, channelName, groups, groupsUsingModel, markDirty, models, state, uid,
} from "../store";
import { toast } from "../toast";
import {
  api, type Group, type GroupStatRow, type StatsPayload, type Tier, type TierMember,
} from "../api";

/* ------------------------------- 展示辅助 ------------------------------- */

const TIER_NAMES = ["一级", "二级", "三级", "四级", "五级", "六级"];

function tierName(level: number): string {
  return TIER_NAMES[level - 1] ?? `${level} 级`;
}
function tierTone(level: number): "brand" | "gray" {
  return level === 1 ? "brand" : "gray";
}
// 「使用中」高亮口径：一级 = 静态优先级最高层（level===1）；
// 服务端暂无「运行时当前生效层级」数据源（/admin 只有 config / logs / stats / health / test-channel / cooldown/clear，
// health 是渠道级冷却），故不做动态高亮。
function isActiveTier(level: number): boolean {
  return level === 1;
}
function sortedTiers(g: Group): Tier[] {
  return [...(g.tiers ?? [])].sort((a, b) => a.level - b.level);
}
function keysOf(groupId: string) {
  return apiKeys.value.filter(k => k.group_id === groupId);
}
function exposedOf(g: Group): string[] {
  return g.exposed_models ?? [];
}

/** 回环提示里的轮数来自全局重试配置。 */
const maxCycles = computed(() => state.cfg?.retry.max_cycles ?? 2);

/**
 * 自动权重开关（AdminConfig.auto_weight，默认关）：
 * 关闭时同级成员按列表顺序依次尝试；开启时按权重降序（权重并列再退回列表顺序）。
 * 权重输入框在两种模式下都常驻可改 —— 关闭时改的值会照常存盘，等开关打开后立即生效，
 * 这样用户可以先填好权重再一次性开启，不必来回切页面。关闭模式额外显示顺序号，因为那时它才是真实次序。
 * 顺序即数组顺序，保存时直接按 draft 里 members 的数组序写入，不需要额外排序。
 */
const autoWeight = computed(() => state.cfg?.auto_weight ?? false);

/* ------------------------------ 分组统计 ------------------------------ */
// 数据来自 GET /admin/api/stats（不传 group_id = 全量：summary 是全局汇总，
// by_group 恒为跨分组概览，字段见 server/src/store.rs:275）。
// 刻意不轮询：进页面拉一次 + 手动刷新，避免在分组页上持续打管理面。
const stats = ref<StatsPayload | null>(null);
const statsErr = ref("");
const statsLoading = ref(false);
const statsAt = ref(0);

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
  const n = statNum(statOf(groupId)?.avg_duration_ms);
  return n === null ? "—" : `${Math.round(n)}ms`;
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

// summary 是服务端 json_object 拼的（server/src/store.rs:241）：requests / success /
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

// App.vue 切页是 <Transition> + <component :is>，没有 keep-alive，
// 每次切回本页都会重新挂载，onMounted 拉一次就够（因此不额外监听路由）。
onMounted(() => { void loadStats(); });

/* ------------------------------- 编辑草稿 ------------------------------- */

const draft = ref<Group | null>(null);
const isNew = ref(false);
const exposedText = ref("");

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

/** 下拉当前值对应的那一行（用来在触发器下方补一句上下文，例如重复提示）。 */
function optionOf(modelId: string) {
  return modelOptions.value.find(o => o.value === modelId);
}

function newMember(modelId: string, weight = 1): TierMember {
  return { model_id: modelId, weight };
}
/** 优先挑该层还没用过的模型，避免一上来就撞重复。 */
function firstFreeModelId(tier: Tier): string {
  const used = new Set(tier.members.map(m => m.model_id));
  return models.value.find(m => !used.has(m.id))?.id ?? models.value[0]?.id ?? "";
}

function openNew() {
  const t: Tier = { level: 1, members: [] };
  if (models.value.length) t.members.push(newMember(models.value[0].id));
  draft.value = { id: uid("grp"), name: "", exposed_models: [], tiers: [t] };
  exposedText.value = "";
  isNew.value = true;
}

function openEdit(g: Group) {
  draft.value = JSON.parse(JSON.stringify(g)) as Group;
  if (!draft.value.tiers || !draft.value.tiers.length) {
    draft.value.tiers = [{ level: 1, members: [] }];
  }
  exposedText.value = (g.exposed_models ?? []).join("\n");
  isNew.value = false;
}

function closeModal() {
  draft.value = null;
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

/* -------------------------------- 校验 -------------------------------- */

const exposedList = computed(() =>
  exposedText.value
    .split(/[\s,，;；、]+/)
    .map(s => s.trim())
    .filter(Boolean),
);

const nameError = computed(() =>
  draft.value && !draft.value.name.trim() ? "请填写分组名。" : "",
);
const exposedError = computed(() =>
  draft.value && exposedList.value.length === 0
    ? "至少填写一个暴露模型名，客户端用这些名字发起请求。"
    : "",
);
const emptyTierError = computed(() => {
  const d = draft.value;
  if (!d) return "";
  const i = d.tiers.findIndex(t => t.members.length === 0);
  return i >= 0 ? `${tierName(i + 1)}还没有成员，每一级至少需要一个模型。` : "";
});
const dupErrors = computed<string[]>(() => {
  const d = draft.value;
  if (!d) return [];
  const out: string[] = [];
  d.tiers.forEach((t, i) => {
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
const valid = computed(() =>
  !nameError.value && !exposedError.value && !emptyTierError.value && dupErrors.value.length === 0,
);
const totalMembers = computed(() =>
  draft.value ? draft.value.tiers.reduce((n, t) => n + t.members.length, 0) : 0,
);

/* -------------------------------- 写回 -------------------------------- */

function save() {
  const d = draft.value;
  const cfg = state.cfg;
  if (!d || !cfg || !valid.value) return;
  const clean: Group = {
    id: d.id.trim() || uid("grp"),
    name: d.name.trim(),
    exposed_models: exposedList.value,
    tiers: d.tiers.map((t, i) => ({
      level: i + 1,
      members: t.members.map(m => ({
        model_id: m.model_id,
        weight: Number(m.weight) || 0,
      })),
    })),
  };
  const index = cfg.groups.findIndex(g => g.id === clean.id);
  if (index >= 0) cfg.groups.splice(index, 1, clean);
  else cfg.groups.push(clean);
  markDirty();
  toast.ok(isNew.value ? `已添加分组「${clean.name}」` : `已更新分组「${clean.name}」`);
  closeModal();
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

/* ------------------------------ 成员删除影响面 ------------------------------ */

// 删除成员本身只影响当前草稿，但被删掉的模型可能正在被这个分组的其它层使用，
// 删之前把「还有哪些层也在用它」「它挂在哪个渠道」讲清楚，避免误删。
const pendingMember = ref<{ tier: Tier; index: number; modelId: string } | null>(null);
const memberConfirming = ref(false);

function removeMemberAt(tier: Tier, index: number) {
  const m = tier.members[index];
  if (!m) return;
  pendingMember.value = { tier, index, modelId: m.model_id };
  memberConfirming.value = true;
}

/** 同一分组内其它层是否也引用了这个模型（含未保存的改动）。 */
const pendingMemberOtherTiers = computed(() => {
  const pm = pendingMember.value;
  const d = draft.value;
  if (!pm || !d) return 0;
  return d.tiers.filter(t => t !== pm.tier).filter(t => t.members.some(m => m.model_id === pm.modelId)).length;
});

/** 该模型还被哪些已保存的分组引用（用 groupsUsingModel，删完还剩几处）。 */
const pendingMemberGroups = computed(() => {
  const pm = pendingMember.value;
  return pm ? groupsUsingModel(pm.modelId).length : 0;
});

/** 该绑定所属渠道（渠道名-模型名里的前半截，缺绑定/缺渠道时退化）。 */
const pendingMemberChannel = computed(() => {
  const pm = pendingMember.value;
  if (!pm) return "—";
  const m = models.value.find(x => x.id === pm.modelId);
  return m ? channelName(m.channel_id) : "（绑定已删除）";
});

/** 确认删除成员：同样是页面内 Modal，不用 window.confirm。 */
function confirmRemoveMember() {
  const pm = pendingMember.value;
  const d = draft.value;
  if (!pm || !d) return;
  // 位置可能已经变了（比如期间删过别的层），按对象重新定位一次再删。
  const ti = d.tiers.indexOf(pm.tier);
  const tier = ti >= 0 ? d.tiers[ti] : null;
  if (tier) {
    const mi = tier.members.findIndex(m => m.model_id === pm.modelId);
    tier.members.splice(mi >= 0 ? mi : pm.index, 1);
  }
  memberConfirming.value = false;
  pendingMember.value = null;
  toast.ok(`已从${tierName(ti + 1)}移除「${bindingLabel(pm.modelId)}」`);
}
</script>

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
          title="重新拉取全量统计"
          @click="loadStats"
        >
          <RefreshCw :size="13" :class="statsLoading ? 'animate-spin' : ''" />刷新统计
        </Btn>
        <Btn variant="primary" size="sm" @click="openNew">
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
      <Btn variant="primary" size="sm" @click="openNew">
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
            </div>
            <div class="mt-0.5 truncate font-mono text-[11.5px] text-ink-3">{{ g.id }}</div>
          </div>
          <div class="flex shrink-0 items-center gap-0.5">
            <IconBtn title="分组详情" data-test="group-detail-btn" @click="openGroupDetail(g.id)">
              <LayoutDashboard :size="15" />
            </IconBtn>
            <IconBtn title="编辑分组" @click="openEdit(g)"><Pencil :size="15" /></IconBtn>
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
        </div>

        <!-- 故障转移层级 -->
        <div class="mt-3.5 border-t border-line-soft pt-3.5">
          <div class="mb-2.5 flex items-center justify-between gap-2">
            <div class="text-[11.5px] font-medium uppercase tracking-wide text-ink-3">故障转移层级</div>
            <Btn size="sm" variant="ghost" @click="openEdit(g)">
              <Pencil :size="12" />编辑路由
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
                      :key="m.model_id"
                      class="flex items-center gap-2 rounded-[10px] border px-2.5 py-1"
                      :class="isActiveTier(t.level)
                        ? 'border-teal-200 bg-teal-50/60'
                        : 'border-line bg-app'"
                    >
                      <span
                        class="font-mono text-[12.5px] text-ink"
                        :title="optionOf(m.model_id)?.hint ? '上游 ' + optionOf(m.model_id)?.hint : undefined"
                      >{{ bindingLabel(m.model_id) }}</span>
                      <Badge tone="gray" :data-test="autoWeight ? 'badge-weight' : 'badge-order'">
                        {{ autoWeight ? `权重 ${m.weight}` : `顺序 ${mi + 1} · 权重 ${m.weight}` }}
                      </Badge>
                    </div>
                  </template>
                  <span v-else class="py-1 text-[12px] text-ink-3">该级无成员</span>
                  <span
                    v-if="isActiveTier(t.level)"
                    class="ml-1 inline-flex shrink-0 items-center gap-1 text-[11.5px] font-medium text-teal-700"
                  >
                    <span class="h-1.5 w-1.5 rounded-full bg-teal-500"></span>使用中
                  </span>
                </div>
              </div>
            </template>
            <div class="mt-2.5 pl-[60px] text-[11.5px] text-ink-3">
              ↑ 全部失败 → 回到一级（最多 {{ maxCycles }} 轮）
            </div>
          </div>
          <p v-else class="text-[12px] text-ink-3">未配置层级，点击「编辑路由」添加一级。</p>
        </div>
      </Card>
    </div>

    <!-- ------------------------------ 编辑弹窗 ------------------------------ -->
    <Modal
      :open="!!draft"
      :title="isNew ? '添加分组' : '编辑分组'"
      width="720px"
      @close="closeModal"
    >
      <div v-if="draft" class="space-y-4">
        <div class="grid grid-cols-1 gap-4 sm:grid-cols-2">
          <Field label="分组 ID" hint="新增时可修改；已保存的分组不建议再改 ID">
            <TInput v-model="draft.id" mono :disabled="!isNew" />
          </Field>
          <Field label="分组名" required hint="例如：主力池 / 备用池">
            <TInput v-model="draft.name" placeholder="主力池" :invalid="!!nameError" />
          </Field>
        </div>

        <Field
          label="暴露模型"
          required
          hint="客户端用这些模型名请求，会按本分组的路由表转发；逗号或换行分隔"
        >
          <TTextarea v-model="exposedText" :rows="2" placeholder="gpt-4o, claude-3-5-sonnet" />
        </Field>

        <!-- 路由层级编辑器 -->
        <div>
          <div class="mb-2.5 flex items-end justify-between gap-2">
            <span class="text-[12.5px] font-medium text-ink-2">路由层级</span>
            <span class="flex flex-col items-end gap-0.5 text-[11.5px] text-ink-3">
              <span data-test="tier-hint">
                <template v-if="autoWeight">按 一级 → 二级 → 三级 顺序尝试，同级内按权重降序分配</template>
                <template v-else>按 一级 → 二级 → 三级 顺序尝试，同级内按列表顺序依次尝试（自动权重已关闭）</template>
              </span>
              <span data-test="weight-hint">
                <template v-if="autoWeight">权重可直接改；并列时按列表顺序</template>
                <template v-else>权重可先填好，开启「自动权重」后生效</template>
              </span>
            </span>
          </div>

          <Alert v-if="!models.length" tone="warn" class="mb-2.5">
            还没有模型绑定，请先到「渠道与模型」页在某个渠道下添加 exposed → upstream 映射，再回来配置层级。
          </Alert>

          <div class="space-y-2.5">
            <div
              v-for="(t, ti) in draft.tiers"
              :key="ti"
              class="rounded-[10px] border border-line bg-app/60 p-3"
            >
              <div class="mb-2.5 flex items-center justify-between gap-2">
                <div class="flex items-center gap-2">
                  <Badge :tone="tierTone(ti + 1)">{{ tierName(ti + 1) }}</Badge>
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
                  class="flex items-center gap-2 rounded-[10px] border border-line bg-surface px-2.5 py-1.5"
                >
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
                        : '自动权重已关闭：权重暂不参与调度，开启后立即按它降序'"
                    />
                  </div>
                  <div class="flex shrink-0 items-center gap-0.5">
                    <IconBtn title="上移" :disabled="mi === 0" @click="moveMember(t, mi, -1)">
                      <ArrowUp :size="14" />
                    </IconBtn>
                    <IconBtn
                      title="下移"
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

          <div class="mt-2.5">
            <Btn size="sm" variant="ghost" :disabled="!models.length" @click="addTier">
              <Plus :size="12" />添加{{ tierName(draft.tiers.length + 1) }}
            </Btn>
          </div>
        </div>

        <!-- 校验结果 -->
        <Alert v-if="nameError" tone="err">{{ nameError }}</Alert>
        <Alert v-else-if="exposedError" tone="err">{{ exposedError }}</Alert>
        <Alert v-else-if="emptyTierError" tone="err">{{ emptyTierError }}</Alert>
        <Alert v-else-if="dupErrors.length" tone="err">
          <div>
            <p>同一层级内不允许重复选择同一个模型：</p>
            <ul class="mt-0.5 list-disc pl-4">
              <li v-for="(e, i) in dupErrors" :key="i">{{ e }}</li>
            </ul>
          </div>
        </Alert>
        <Alert v-else tone="ok">
          校验通过：{{ draft.tiers.length }} 个层级 / 共 {{ totalMembers }} 个成员。
        </Alert>
      </div>

      <template #footer>
        <Btn variant="ghost" @click="closeModal">取消</Btn>
        <Btn variant="primary" :disabled="!valid" @click="save">
          {{ isNew ? "添加分组" : "保存修改" }}
        </Btn>
      </template>
    </Modal>

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

    <!-- 删除层级成员确认：同样用页面内 Modal（window.confirm 在打包版会冻结） -->
    <Modal
      :open="memberConfirming"
      title="移除层级成员"
      width="460px"
      @close="memberConfirming = false"
    >
      <p class="text-[12.5px] leading-5 text-ink-2">
        确定把
        <span class="font-mono font-medium text-ink">{{ pendingMember ? bindingLabel(pendingMember.modelId) : "" }}</span>
        从当前层级移除？
      </p>
      <dl class="mt-2.5 space-y-1 text-[11.5px] leading-4 text-ink-3">
        <div class="flex gap-1.5">
          <dt class="shrink-0">所属渠道</dt>
          <dd class="text-ink-2">{{ pendingMemberChannel }}</dd>
        </div>
        <div class="flex gap-1.5">
          <dt class="shrink-0">同分组其它层</dt>
          <dd class="text-ink-2">{{ pendingMemberOtherTiers ? `还有 ${pendingMemberOtherTiers} 层也在用它` : "没有其它层使用" }}</dd>
        </div>
        <div class="flex gap-1.5">
          <dt class="shrink-0">其它分组引用</dt>
          <dd class="text-ink-2">{{ pendingMemberGroups ? `${pendingMemberGroups} 个分组引用了这个绑定` : "没有其它分组引用" }}</dd>
        </div>
      </dl>
      <p class="mt-2.5 rounded-lg border border-line bg-app/60 px-2.5 py-2 text-[11.5px] leading-4 text-ink-3">
        只是把这个模型从这个层级的候选里拿掉；模型绑定本身、以及其它分组的使用都不受影响。改动要等顶栏「保存」后才生效。
      </p>

      <template #footer>
        <Btn data-test="member-cancel" @click="memberConfirming = false">取消</Btn>
        <Btn data-test="member-confirm" variant="danger" @click="confirmRemoveMember">
          <Trash2 :size="13" /> 移除
        </Btn>
      </template>
    </Modal>

  </div>
</template>
