<script setup lang="ts">
// 渠道 + 模型合并页：每个渠道一张卡片，展开后是该渠道下挂的可用模型。
// 模型绑定（暴露名 / 上游名 / 4 档价格 / 启用）的增删改都在本页完成，
// 原「模型」独立页的能力已并入这里。
import { computed, onBeforeUnmount, onMounted, reactive, ref, watch } from "vue";
import { Check, CircleAlert, Download, Eye, EyeOff, Layers, ListChecks, LoaderCircle, Plug, Plus, Square, Trash2 } from "@lucide/vue";
import {
  Alert, Btn, Card, Empty, Field, IconBtn, Modal,
  TInput, TSelect, TTextarea, TToggle,
} from "../components/ui";
import ChannelCard from "../components/channels/ChannelCard.vue";
import { api, type Channel, type InflightSnapshot, type ModelBinding } from "../api";
import { channels, groupsUsingModel, markDirty, models, modelsOfChannel, state, uid } from "../store";
import { toast } from "../toast";

/* ------------------------------- 列表渲染 ------------------------------- */

const enabledCount = computed(() => channels.value.filter(c => c.enabled).length);

const protocolOptions = [
  { value: "openai", label: "OpenAI 兼容" },
  { value: "anthropic", label: "Anthropic" },
  { value: "gemini", label: "Gemini" },
];

/**
 * 「使用中」标注口径（2026-10-13 改）：**真实 in-flight 并发**，唯一数据来源是
 * GET /admin/api/inflight。只有此刻真的在用这个账号（并发数 > 0）才算「使用中」——
 * 旧的「列表里第一个 enabled 渠道 / 被一级层级引用」与真实流量无关，是假状态，已废弃。
 * 该接口同时给出 channels（渠道并发之和）与 members（"<渠道>::<上游模型>" → 并发数）。
 */
const inflight = ref<InflightSnapshot | null>(null);
/** 空映射常量：引用稳定，避免轮询时给子组件塞新对象。 */
const EMPTY_MEMBERS: Record<string, number> = {};

async function refreshInflight() {
  try {
    inflight.value = await api.inflight();
  } catch {
    // 静默保留上一次快照：服务端重启 / 瞬时抖动不该把徽章抖没，也不该弹错。
  }
}
let inflightTimer: ReturnType<typeof setInterval> | undefined;
onMounted(() => {
  void refreshInflight();
  inflightTimer = setInterval(() => void refreshInflight(), 5000);
});
onBeforeUnmount(() => {
  if (inflightTimer !== undefined) clearInterval(inflightTimer);
});

/** 该渠道此刻的并发请求数；缺键即 0。 */
function inflightOfChannel(id: string): number {
  return inflight.value?.channels?.[id] ?? 0;
}
/** 全站此刻处理中的请求数，仅用于页头提示。 */
const inflightTotal = computed(() => inflight.value?.total ?? 0);
/** 传给卡片/模型行的并发映射（"<渠道>::<上游模型>" → N）。 */
const inflightMembers = computed<Record<string, number>>(() => inflight.value?.members ?? EMPTY_MEMBERS);

/**
 * 「一级引用」标注口径（静态声明，**不再是「使用中」**）：被任意分组一级（level === 1）引用；
 * 若没有任何分组定义，则退化为「enabled 的绑定」。仅用于高亮，不参与调度决策。
 */
const activeIds = computed<Set<string>>(() => {
  const set = new Set<string>();
  const gs = state.cfg?.groups ?? [];
  const firstTier = gs.filter(g => (g.tiers ?? []).some(t => (t.level ?? 0) === 1));
  if (firstTier.length) {
    for (const g of firstTier) {
      for (const t of g.tiers ?? []) {
        if ((t.level ?? 0) !== 1) continue;
        for (const mem of t.members ?? []) set.add(mem.model_id);
      }
    }
    return set;
  }
  for (const m of models.value) if (m.enabled) set.add(m.id);
  return set;
});

/* ------------------------------- 展开折叠 ------------------------------- */

const expanded = reactive<Record<string, boolean>>({});
function toggleExpand(id: string) {
  expanded[id] = !expanded[id];
}
const anyExpanded = computed(() => channels.value.some(c => expanded[c.id]));
function toggleAll() {
  const target = !anyExpanded.value;
  for (const c of channels.value) expanded[c.id] = target;
}

/* -------------------------------- 编辑表单 ------------------------------- */

const modalOpen = ref(false);
const editingId = ref("");
const showKey = ref(false);
const error = ref("");
const form = reactive({
  id: "",
  name: "",
  base_url: "",
  protocol: "openai",
  api_key: "",
  enabled: true,
  headersText: "",
});

/** 每行一个 "Key: Value"；忽略空行与没有冒号的行。 */
function parseHeaders(text: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (!line) continue;
    const i = line.indexOf(":");
    if (i <= 0) continue;
    const k = line.slice(0, i).trim();
    const v = line.slice(i + 1).trim();
    if (k) out[k] = v;
  }
  return out;
}

function formatHeaders(headers: Record<string, string> | null | undefined): string {
  if (!headers) return "";
  return Object.entries(headers).map(([k, v]) => k + ": " + v).join("\n");
}

function num(v: unknown, fallback = 0): number {
  if (typeof v === "number") return Number.isFinite(v) ? v : fallback;
  const t = String(v ?? "").trim();
  if (!t) return fallback;
  const n = Number(t);
  return Number.isFinite(n) ? n : fallback;
}

function openCreate() {
  editingId.value = "";
  error.value = "";
  showKey.value = false;
  form.id = uid("ch");
  form.name = "";
  form.base_url = "";
  form.protocol = "openai";
  form.api_key = "";
  form.enabled = true;
  form.headersText = "";
  modalOpen.value = true;
}

function openEdit(ch: Channel) {
  editingId.value = ch.id;
  error.value = "";
  showKey.value = false;
  form.id = ch.id;
  form.name = ch.name;
  form.base_url = ch.base_url;
  form.protocol = ch.protocol;
  form.api_key = ch.api_key;
  form.enabled = ch.enabled;
  form.headersText = formatHeaders(ch.headers);
  modalOpen.value = true;
}

function closeModal() {
  modalOpen.value = false;
}

function save() {
  const cfg = state.cfg;
  if (!cfg) return;
  const name = form.name.trim();
  const baseUrl = form.base_url.trim();
  if (!name) { error.value = "渠道名称不能为空。"; return; }
  if (!baseUrl) { error.value = "Base URL 不能为空。"; return; }
  const id = form.id.trim() || uid("ch");
  if (!editingId.value && cfg.channels.some(c => c.id === id)) {
    error.value = "渠道 ID「" + id + "」已存在，请换一个。";
    return;
  }
  // weight 仍是服务端 Channel 的字段，但已不参与路由，界面不再暴露；
  // 编辑时原样保留旧值，新增时用服务端缺省 1，避免写配置时丢字段。
  const prev = cfg.channels.find(c => c.id === editingId.value);
  const payload: Channel = {
    id,
    name,
    base_url: baseUrl,
    protocol: form.protocol as Channel["protocol"],
    api_key: form.api_key,
    enabled: form.enabled,
    weight: prev?.weight ?? 1,
    headers: parseHeaders(form.headersText),
  };
  const i = cfg.channels.findIndex(c => c.id === editingId.value);
  if (editingId.value && i >= 0) cfg.channels[i] = payload;
  else cfg.channels.push(payload);
  markDirty();
  modalOpen.value = false;
  toast.ok(editingId.value ? "渠道「" + name + "」已更新" : "渠道「" + name + "」已添加");
}

/* ------------------------------ 模型绑定表单 ----------------------------- */

/** 父组件每次保存/删除模型后自增，让 ModelRows 丢弃就地编辑草稿。 */
const epoch = ref(0);

const modelModalOpen = ref(false);
const modelEditingId = ref("");
const modelError = ref("");
const modelForm = reactive({
  id: "",
  channel_id: "",
  exposed: "",
  upstream: "",
  input: "0",
  output: "0",
  cacheRead: "0",
  cacheWrite: "0",
  enabled: true,
});

const channelOptions = computed(() =>
  channels.value.map(c => ({ value: c.id, label: c.name || c.id, hint: c.id })),
);

function openCreateModel(channelId = "") {
  modelEditingId.value = "";
  modelError.value = "";
  modelForm.id = uid("mdl");
  modelForm.channel_id = channelId || channels.value[0]?.id || "";
  modelForm.exposed = "";
  modelForm.upstream = "";
  modelForm.input = "0";
  modelForm.output = "0";
  modelForm.cacheRead = "0";
  modelForm.cacheWrite = "0";
  modelForm.enabled = true;
  modelMode.value = "manual";
  resetFetch();
  fetchChannelId.value = modelForm.channel_id;
  modelModalOpen.value = true;
}

function openEditModel(m: ModelBinding) {
  modelEditingId.value = m.id;
  modelError.value = "";
  modelForm.id = m.id;
  modelForm.channel_id = m.channel_id;
  modelForm.exposed = m.exposed;
  modelForm.upstream = m.upstream;
  modelForm.input = String(m.price_per_m[0] ?? 0);
  modelForm.output = String(m.price_per_m[1] ?? 0);
  modelForm.cacheRead = String(m.price_per_m[2] ?? 0);
  modelForm.cacheWrite = String(m.price_per_m[3] ?? 0);
  modelForm.enabled = m.enabled;
  modelMode.value = "manual";
  resetFetch();
  modelModalOpen.value = true;
}

function closeModelModal() {
  modelModalOpen.value = false;
}

function saveModel() {
  const cfg = state.cfg;
  if (!cfg) return;
  if (!modelForm.channel_id) { modelError.value = "请先选择所属渠道（没有渠道时请先添加渠道）。"; return; }
  const exposed = modelForm.exposed.trim();
  const upstream = modelForm.upstream.trim();
  if (!exposed) { modelError.value = "暴露模型名不能为空。"; return; }
  if (!upstream) { modelError.value = "上游模型名不能为空。"; return; }
  const id = modelForm.id.trim() || uid("mdl");
  if (!modelEditingId.value && cfg.models.some(m => m.id === id)) {
    modelError.value = "绑定 ID「" + id + "」已存在，请换一个。";
    return;
  }
  const payload: ModelBinding = {
    id,
    channel_id: modelForm.channel_id,
    exposed,
    upstream,
    price_per_m: [
      num(modelForm.input, 0),
      num(modelForm.output, 0),
      num(modelForm.cacheRead, 0),
      num(modelForm.cacheWrite, 0),
    ],
    enabled: modelForm.enabled,
  };
  const i = cfg.models.findIndex(m => m.id === modelEditingId.value);
  if (modelEditingId.value && i >= 0) cfg.models[i] = payload;
  else cfg.models.push(payload);
  markDirty();
  epoch.value++;
  expanded[payload.channel_id] = true;
  modelModalOpen.value = false;
  toast.ok(modelEditingId.value ? "模型绑定「" + exposed + "」已更新" : "模型绑定「" + exposed + "」已添加");
}

/* ------------------------ 添加模型：两种方式 ------------------------ */

/**
 * 添加方式：
 *   manual —— 手动填表（沿用既有表单，逐字段指定暴露名/上游名/价格）；
 *   fetch  —— 从上游的 models 接口拉取真实模型名，勾选后批量导入。
 * 默认 manual：手动方式字段更全（价格、启用），拉取方式适合一次挂很多模型。
 */
const modelMode = ref<"manual" | "fetch">("manual");

/** fetch 方式的数据来源渠道：默认跟手动表单里的渠道保持一致，避免两次选择打架。 */
const fetchChannelId = ref("");
const fetching = ref(false);
const fetchMsg = ref("");
const fetchErr = ref(false);
const fetchFilter = ref("");
const fetchRows = ref<{ id: string; bound: boolean }[]>([]);
/** 已勾选的上游模型名。用响应式 Set，勾选/取消都能正确触发重渲染。 */
const selSet = reactive(new Set<string>());

const filteredFetchRows = computed(() => {
  const q = fetchFilter.value.trim().toLowerCase();
  if (!q) return fetchRows.value;
  return fetchRows.value.filter(r => r.id.toLowerCase().includes(q));
});
const selectedCount = computed(() => selSet.size);
const allFetchedSelected = computed(() => {
  const usable = filteredFetchRows.value.filter(r => !r.bound);
  return usable.length > 0 && usable.every(r => selSet.has(r.id));
});

/** 打开弹窗时把上一次的拉取结果/勾选/错误全部清空，避免旧数据串台。 */
function resetFetch() {
  fetching.value = false;
  fetchMsg.value = "";
  fetchErr.value = false;
  fetchFilter.value = "";
  fetchRows.value = [];
  selSet.clear();
  fetchChannelId.value = channels.value[0]?.id || "";
}

function toggleFetched(id: string) {
  if (selSet.has(id)) selSet.delete(id);
  else selSet.add(id);
}

function toggleAllFetched() {
  const usable = filteredFetchRows.value.filter(r => !r.bound);
  if (allFetchedSelected.value) usable.forEach(r => selSet.delete(r.id));
  else usable.forEach(r => selSet.add(r.id));
}

/**
 * 拉取上游模型列表。
 * 契约：接口永远回 HTTP 200，成败只能看 ok —— 失败时绝不能静默，也绝不能伪造出条目，
 * 必须把 error / status 原样显示在 model-fetch-status 上（验收 G 段就查这个）。
 */
async function runFetch() {
  const ch = fetchChannelId.value;
  if (!ch) { fetchErr.value = true; fetchMsg.value = "请先选择要拉取模型的渠道。"; return; }
  if (fetching.value) return;
  fetching.value = true;
  fetchErr.value = false;
  fetchMsg.value = "";
  fetchRows.value = [];
  selSet.clear();
  try {
    const r = await api.channelModels(ch);
    if (r.ok) {
      const bound = new Set(models.value.filter(m => m.channel_id === ch).map(m => m.upstream));
      fetchRows.value = (r.models || []).map(id => ({ id, bound: bound.has(id) }));
      const nBound = fetchRows.value.filter(x => x.bound).length;
      fetchMsg.value = "上游返回 " + r.count + " 个模型 · " + fmtSec(r.latency_ms) + (nBound ? " · 其中 " + nBound + " 个已在本渠道绑定" : "");
      fetchErr.value = false;
    } else {
      fetchErr.value = true;
      fetchMsg.value = "获取失败：" + (r.error || ("HTTP " + r.status));
    }
  } catch (e: any) {
    fetchErr.value = true;
    fetchMsg.value = "获取失败：" + String(e?.message || e);
  } finally {
    fetching.value = false;
  }
}

/**
 * 导入勾选的上游模型：每个模型名生成一条绑定（暴露名 = 上游名），默认启用、四档价格 0。
 * 已在本渠道绑定过的上游名直接跳过，不产生重复绑定。整批改完只 markDirty() 一次，由顶栏统一保存。
 */
function importFetched() {
  const cfg = state.cfg;
  if (!cfg) return;
  const ch = fetchChannelId.value;
  if (!ch) { fetchErr.value = true; fetchMsg.value = "请先选择要导入到的渠道。"; return; }
  const bound = new Set(cfg.models.filter(m => m.channel_id === ch).map(m => m.upstream));
  let added = 0, skipped = 0;
  for (const raw of Array.from(selSet)) {
    const up = String(raw).trim();
    if (!up) continue;
    if (bound.has(up)) { skipped++; continue; }
    bound.add(up);
    cfg.models.push({
      id: uid("mdl"),
      channel_id: ch,
      exposed: up,
      upstream: up,
      price_per_m: [0, 0, 0, 0],
      enabled: true,
    });
    added++;
  }
  if (!added) {
    fetchErr.value = true;
    fetchMsg.value = skipped ? "勾选的模型都已在本渠道绑定过了。" : "请先勾选要导入的模型。";
    return;
  }
  markDirty();
  epoch.value++;
  expanded[ch] = true;
  modelModalOpen.value = false;
  toast.ok("已添加 " + added + " 个模型" + (skipped ? "，跳过 " + skipped + " 个已存在" : ""));
}

/**
 * 拉取方式下按 Escape 不关窗。
 * 理由：列表/勾选/错误信息都是花过一次上游请求才拿到的，Escape 误触会把它们一起丢掉，
 * 关窗应该走「取消」或右上角 ×（验收 G7 就是这个语义）。
 * 实现：reka-ui 的 DismissableLayer 在 window 的【冒泡】阶段监听 Escape，
 * 同一节点上 capture 阶段先于冒泡阶段，所以在 window 上用 capture 监听可以拦下它。
 */
function guardEscape(e: KeyboardEvent) {
  if (e.key === "Escape" && modelMode.value === "fetch") e.stopPropagation();
}
watch(modelModalOpen, (open) => {
  if (open) window.addEventListener("keydown", guardEscape, true);
  else window.removeEventListener("keydown", guardEscape, true);
});
onBeforeUnmount(() => window.removeEventListener("keydown", guardEscape, true));

/* ------------------------------- 连通测试 -------------------------------- */

const testingId = ref("");
async function test(ch: Channel) {
  if (testingId.value) return;
  testingId.value = ch.id;
  try {
    const r = await api.testChannel(ch.id);
    if (r.ok) {
      toast.ok("连通正常 · " + fmtSec(r.latency_ms));
      const sample = (r.sample || "").trim();
      if (sample) toast.info(sample.length > 120 ? sample.slice(0, 120) + "…" : sample);
    } else {
      toast.err("连通失败：" + (r.error || "未知错误"));
    }
  } catch (e: any) {
    toast.err("连通测试失败：" + String(e?.message || e));
  } finally {
    testingId.value = "";
  }
}

/* --------------------------------- 删除 --------------------------------- */

const pendingDelete = ref<Channel | null>(null);
const confirming = ref(false);

function remove(ch: Channel) {
  if (!state.cfg) return;
  pendingDelete.value = ch;
  confirming.value = true;
}

/** 删除渠道的影响面：会连带移除的模型数，以及受牵连的分组数。 */
const deleteImpact = computed(() => {
  const ch = pendingDelete.value;
  if (!ch) return { models: 0, groups: 0 };
  const bound = models.value.filter(m => m.channel_id === ch.id);
  const gset = new Set<string>();
  for (const m of bound) for (const g of groupsUsingModel(m.id)) gset.add(g.id);
  return { models: bound.length, groups: gset.size };
});

/**
 * 连带清理：把即将消失的模型绑定从所有分组的故障转移层级里摘掉。
 *
 * ★ 为什么必须做（2026-10-14 修「保存失败」）：
 *   服务端 PUT /admin/api/config 是**整体替换 + 先校验**（server/src/admin.rs:62-70）：
 *   只要有一处 tiers[].members[] 指向不存在的 model_id，整份配置 400 拒收、一个字节都不落库，
 *   用户看到的就是「保存失败：group grp_x tier 1 references missing model mdl_y」。
 *   而删除弹窗本来就写着「这些候选会消失」——所以这里替用户把引用一并摘干净，
 *   否则界面看着正常（分组卡片照样渲染成员），一保存就必然失败。
 *
 * 空层保留不删：服务端接受没有成员的层（admin.rs 只校验引用是否存在），路由时该层没有候选、
 * 等价于不存在；保住用户排好的层号比把界面擦干净更重要。
 *
 * @returns 受影响的分组名（用于 toast 如实告知改了哪些分组）。
 */
function dropGroupRefs(modelIds: Set<string>): string[] {
  const cfg = state.cfg;
  if (!cfg || !modelIds.size) return [];
  const hits: string[] = [];
  for (const g of cfg.groups) {
    let removed = 0;
    for (const t of g.tiers ?? []) {
      const before = (t.members ?? []).length;
      t.members = (t.members ?? []).filter(m => !modelIds.has(m.model_id));
      removed += before - t.members.length;
    }
    if (removed) hits.push(g.name || g.id);
  }
  return hits;
}

/** 确认删除：WebView2 里 window.confirm 不可用，改用页面内 Modal。 */
function confirmRemove() {
  const cfg = state.cfg;
  const ch = pendingDelete.value;
  if (!cfg || !ch) return;
  const bound = cfg.models.filter(m => m.channel_id === ch.id);
  cfg.channels = cfg.channels.filter(c => c.id !== ch.id);
  if (bound.length) cfg.models = cfg.models.filter(m => m.channel_id !== ch.id);
  const hitGroups = dropGroupRefs(new Set(bound.map(m => m.id)));
  markDirty();
  epoch.value++;
  confirming.value = false;
  const tail = [
    bound.length ? `移除 ${bound.length} 个模型绑定` : "",
    hitGroups.length ? `并从 ${hitGroups.length} 个分组的层级里摘掉对应成员（${hitGroups.join("、")}）` : "",
  ].filter(Boolean).join("，");
  toast.ok(tail ? "已删除渠道，" + tail : "渠道已删除");
}

const pendingModel = ref<ModelBinding | null>(null);
const modelConfirming = ref(false);

function removeModel(m: ModelBinding) {
  if (!state.cfg) return;
  pendingModel.value = m;
  modelConfirming.value = true;
}

/** 删除模型前要告知：它被哪些分组引用。 */
const modelRefs = computed(() => (pendingModel.value ? groupsUsingModel(pendingModel.value.id) : []));

function confirmRemoveModel() {
  const cfg = state.cfg;
  const m = pendingModel.value;
  if (!cfg || !m) return;
  const refsBefore = groupsUsingModel(m.id).length;
  cfg.models = cfg.models.filter(x => x.id !== m.id);
  // ★ 与删渠道同理：先摘掉分组层级里的引用，再让用户保存 ——
  //   留着悬空 model_id，顶栏「保存」会被服务端 400 全量拒收。
  const hitGroups = dropGroupRefs(new Set([m.id]));
  markDirty();
  epoch.value++;
  modelConfirming.value = false;
  const tail = hitGroups.length
    ? `，并从 ${hitGroups.length} 个分组的层级里摘掉该成员（${hitGroups.join("、")}）`
    : "";
  toast.ok(
    hitGroups.length || refsBefore
      ? `模型绑定「${m.exposed || m.id}」已删除` + tail
      : "模型绑定已删除",
  );
}
</script>

<template>
  <div>
    <div class="mb-3 flex flex-wrap items-center gap-2">
      <div v-if="channels.length" class="text-[12px] text-ink-3">
        共 {{ channels.length }} 个渠道 · 已启用 {{ enabledCount }} 个 · 模型绑定 {{ models.length }} 个
        <span v-if="inflightTotal" class="ml-1 font-medium text-teal-700" title="此刻网关正在处理中的请求总数">· 处理中 {{ inflightTotal }}</span>
      </div>
      <div class="ml-auto flex items-center gap-2">
        <Btn v-if="channels.length" variant="subtle" size="sm" data-test="toggle-all" @click="toggleAll">
          <Layers :size="13" /> {{ anyExpanded ? "收起全部" : "展开全部" }}
        </Btn>
        <Btn
          variant="subtle" size="sm" data-test="add-model"
          title="给渠道添加可用模型：手动填写，或从上游 models 接口拉取后勾选导入"
          @click="openCreateModel()"
        >
          <Plus :size="13" />添加模型
        </Btn>
        <Btn variant="primary" size="sm" data-test="add-channel" @click="openCreate">
          <Plus :size="13" /> 添加渠道
        </Btn>
      </div>
    </div>

    <div v-if="channels.length" class="space-y-2">
      <ChannelCard
        v-for="ch in channels" :key="ch.id"
        :channel="ch"
        :models="modelsOfChannel(ch.id)"
        :active-ids="activeIds"
        :inflight="inflightOfChannel(ch.id)"
        :inflight-members="inflightMembers"
        :testing="testingId === ch.id"
        :expanded="!!expanded[ch.id]"
        :epoch="epoch"
        @toggle-expand="toggleExpand(ch.id)"
        @test="test(ch)"
        @edit="openEdit(ch)"
        @remove="remove(ch)"
        @edit-model="openEditModel"
        @remove-model="removeModel"
        @add-model="openCreateModel(ch.id)"
      />
    </div>

    <Card v-else :pad="true">
      <Empty title="还没有渠道" hint="添加第一个上游供应商（OpenAI / Anthropic / Gemini），再在渠道下挂可用模型">
        <template #icon><Plug :size="28" /></template>
        <Btn variant="primary" size="sm" data-test="add-channel-empty" @click="openCreate">
          <Plus :size="13" /> 添加渠道
        </Btn>
      </Empty>
    </Card>

    <Modal
      :open="modalOpen"
      :title="editingId ? '编辑渠道' : '添加渠道'"
      width="600px"
      @close="closeModal"
    >
      <div class="space-y-3.5">
        <div class="grid grid-cols-1 gap-3 sm:grid-cols-2">
          <Field label="渠道 ID" :hint="editingId ? '已有渠道的 ID 不可修改' : '新增时自动生成，可自行调整'">
            <TInput v-model="form.id" mono :disabled="!!editingId" />
          </Field>
          <Field label="名称" required>
            <TInput v-model="form.name" :invalid="!!error && !form.name.trim()" placeholder="例如 OpenAI 官方" />
          </Field>
        </div>

        <Field label="Base URL" required>
          <TInput
            v-model="form.base_url"
            mono
            :invalid="!!error && !form.base_url.trim()"
            placeholder="https://api.openai.com"
          />
        </Field>

        <Field label="协议" hint="决定请求体与鉴权头的拼装方式">
          <TSelect v-model="form.protocol" :options="protocolOptions" />
        </Field>

        <Field label="API Key" hint="仅写入本地配置文件，不会上传">
          <div class="flex items-center gap-2">
            <div class="min-w-0 flex-1">
              <TInput v-model="form.api_key" :type="showKey ? 'text' : 'password'" mono placeholder="sk-…" />
            </div>
            <IconBtn :title="showKey ? '隐藏密钥' : '显示密钥'" @click="showKey = !showKey">
              <EyeOff v-if="showKey" :size="15" />
              <Eye v-else :size="15" />
            </IconBtn>
          </div>
        </Field>

        <Field label="自定义请求头" hint="每行一个 Key: Value，空行与缺少冒号的行会被忽略">
          <TTextarea v-model="form.headersText" :rows="3" placeholder="X-Org-Id: team-123" />
        </Field>

        <div class="flex items-center justify-between rounded-lg border border-line-soft bg-app px-3 py-2">
          <div class="min-w-0">
            <div class="text-[12.5px] font-medium text-ink-2">启用</div>
            <div class="text-[11.5px] text-ink-3">停用后调度不会选择该渠道</div>
          </div>
          <TToggle v-model="form.enabled" />
        </div>

        <Alert v-if="error" tone="err">{{ error }}</Alert>
      </div>

      <template #footer>
        <Btn @click="closeModal">取消</Btn>
        <Btn variant="primary" data-test="save-channel" @click="save"><Check :size="13" /> 保存</Btn>
      </template>
    </Modal>

    <!-- 模型绑定编辑：字段与原「模型」页一致 -->
    <Modal
      :open="modelModalOpen"
      :title="modelEditingId ? '编辑模型' : '添加模型'"
      width="600px"
      @close="closeModelModal"
    >
      <div class="space-y-3.5">
        <!-- 两种添加方式：手动填表 / 从上游 models 接口拉取后勾选导入（编辑既有绑定时只有手动表单） -->
        <div v-if="!modelEditingId" class="grid grid-cols-2 gap-1 rounded-lg border border-line-soft bg-app p-1">
          <button
            type="button" data-test="model-mode-manual"
            class="h-7 rounded-md text-[12px] font-medium transition-colors"
            :class="modelMode === 'manual' ? 'bg-surface text-ink shadow-sm' : 'text-ink-3 hover:text-ink-2'"
            @click="modelMode = 'manual'"
          >手动添加</button>
          <button
            type="button" data-test="model-mode-fetch"
            class="h-7 rounded-md text-[12px] font-medium transition-colors"
            :class="modelMode === 'fetch' ? 'bg-surface text-ink shadow-sm' : 'text-ink-3 hover:text-ink-2'"
            @click="modelMode = 'fetch'"
          >从 models 接口获取</button>
        </div>

        <template v-if="modelMode === 'manual'">
        <Field label="所属渠道" required hint="模型挂在这个渠道下，路由标识为「渠道-模型」">
          <div data-test="model-manual-channel">
            <TSelect
              v-model="modelForm.channel_id"
              :options="channelOptions"
              placeholder="选择渠道"
              empty-text="还没有渠道"
            />
          </div>
        </Field>

        <div class="grid grid-cols-1 gap-3 sm:grid-cols-2">
          <Field label="暴露模型名" required hint="客户端请求时使用的名字">
            <TInput
              v-model="modelForm.exposed"
              data-test="model-manual-exposed"
              mono
              :invalid="!!modelError && !modelForm.exposed.trim()"
              placeholder="gpt-4o"
            />
          </Field>
          <Field label="上游模型名" required hint="转发给上游时使用的名字">
            <TInput
              v-model="modelForm.upstream"
              data-test="model-manual-upstream"
              mono
              :invalid="!!modelError && !modelForm.upstream.trim()"
              placeholder="gpt-4o-2024-08-06"
            />
          </Field>
        </div>

        <Field label="绑定 ID" :hint="modelEditingId ? '已有绑定的 ID 不可修改' : '新增时自动生成，可自行调整'">
          <TInput v-model="modelForm.id" mono :disabled="!!modelEditingId" data-test="model-manual-id" />
        </Field>

        <div>
          <div class="mb-1.5 text-[12px] font-medium text-ink-2">价格（USD / 1M tokens）</div>
          <div class="grid grid-cols-2 gap-3 sm:grid-cols-4">
            <Field label="输入">
              <TInput v-model="modelForm.input" type="number" size="sm" mono />
            </Field>
            <Field label="输出">
              <TInput v-model="modelForm.output" type="number" size="sm" mono />
            </Field>
            <Field label="缓存读">
              <TInput v-model="modelForm.cacheRead" type="number" size="sm" mono />
            </Field>
            <Field label="缓存写">
              <TInput v-model="modelForm.cacheWrite" type="number" size="sm" mono />
            </Field>
          </div>
        </div>

        <div class="flex items-center justify-between rounded-lg border border-line-soft bg-app px-3 py-2">
          <div class="min-w-0">
            <div class="text-[12.5px] font-medium text-ink-2">启用</div>
            <div class="text-[11.5px] text-ink-3">停用后调度不会选择该模型</div>
          </div>
          <TToggle v-model="modelForm.enabled" />
        </div>

        <Alert v-if="modelError" tone="err">{{ modelError }}</Alert>
        </template>

        <template v-else>
          <Field label="数据来源渠道" required hint="用该渠道的 base_url / 密钥去请求上游的 models 接口">
            <div data-test="model-fetch-channel">
              <TSelect
                v-model="fetchChannelId"
                :options="channelOptions"
                placeholder="选择渠道"
                empty-text="还没有渠道"
              />
            </div>
          </Field>

          <div class="flex flex-wrap items-center gap-2">
            <Btn size="sm" data-test="model-fetch-run" :disabled="fetching" @click="runFetch">
              <LoaderCircle v-if="fetching" :size="13" class="animate-spin" />
              <Download v-else :size="13" />
              {{ fetching ? "获取中…" : "获取模型列表" }}
            </Btn>
            <span class="text-[11.5px] leading-4 text-ink-3">只读取上游的模型名；导入时按「暴露名 = 上游名」生成绑定，可再逐个改价格</span>
          </div>

          <div
            v-if="fetchMsg"
            data-test="model-fetch-status"
            class="flex items-start gap-2 rounded-lg border px-3 py-2 text-[12.5px] leading-5"
            :class="fetchErr ? 'border-err-100 bg-err-50 text-err-700' : 'border-line bg-app text-ink-2'"
          >
            <CircleAlert :size="14" class="mt-0.5 shrink-0" />
            <span class="min-w-0 break-all">{{ fetchMsg }}</span>
          </div>

          <template v-if="fetchRows.length">
            <div class="flex flex-wrap items-center gap-2">
              <TInput v-model="fetchFilter" size="sm" class="max-w-[220px]" data-test="model-fetch-filter" placeholder="过滤模型名…" />
              <span data-test="model-fetch-count" class="text-[12px] text-ink-3">已选 {{ selectedCount }} / 共 {{ fetchRows.length }}</span>
              <Btn size="sm" class="ml-auto" data-test="model-fetch-all" @click="toggleAllFetched">
                <ListChecks :size="13" />{{ allFetchedSelected ? "取消全选" : "全选可导入" }}
              </Btn>
            </div>

            <div data-test="model-fetch-list" class="max-h-[300px] overflow-y-auto rounded-lg border border-line-soft">
              <button
                v-for="row in filteredFetchRows" :key="row.id"
                type="button"
                data-test="model-fetch-item"
                :disabled="row.bound"
                class="flex w-full items-center gap-2 px-3 py-1.5 text-left text-[12.5px] transition-colors"
                :class="row.bound
                  ? 'cursor-not-allowed text-ink-3'
                  : (selSet.has(row.id) ? 'bg-brand-50 text-ink' : 'text-ink-2 hover:bg-app')"
                @click="toggleFetched(row.id)"
              >
                <Check v-if="row.bound" :size="14" class="shrink-0 text-ok-700" />
                <Check v-else-if="selSet.has(row.id)" :size="14" class="shrink-0 text-brand-700" />
                <Square v-else :size="14" class="shrink-0 text-ink-3" />
                <span class="min-w-0 truncate font-mono">{{ row.id }}</span>
                <span v-if="row.bound" class="ml-auto shrink-0 text-[11px] text-ink-3">已存在</span>
              </button>
            </div>
            <div v-if="!filteredFetchRows.length" class="py-1 text-center text-[11.5px] text-ink-3">没有匹配的模型名</div>
          </template>
        </template>
      </div>

      <template #footer>
        <Btn @click="closeModelModal">取消</Btn>
        <Btn
          v-if="modelEditingId || modelMode === 'manual'"
          variant="primary" data-test="save-model" @click="saveModel"
        ><Check :size="13" /> 保存</Btn>
        <Btn
          v-else
          variant="primary" data-test="model-fetch-import"
          :disabled="!selectedCount || fetching" @click="importFetched"
        ><Download :size="13" /> 导入所选（{{ selectedCount }}）</Btn>
      </template>
    </Modal>

    <!-- 删除渠道确认：WebView2 里 window.confirm 不可用，改用页面内 Modal -->
    <Modal
      :open="confirming"
      title="删除渠道"
      width="440px"
      @close="confirming = false"
    >
      <p class="text-[12.5px] leading-5 text-ink-2">
        确定删除渠道「<span class="font-medium text-ink">{{ pendingDelete?.name || pendingDelete?.id }}</span>」？
      </p>
      <p v-if="deleteImpact.models || deleteImpact.groups" class="mt-1.5 text-[11.5px] leading-4 text-ink-3">
        会级联删除该渠道下的 {{ deleteImpact.models }} 个模型绑定<template v-if="deleteImpact.groups">，并从 {{ deleteImpact.groups }} 个分组的故障转移层级里摘掉对应成员（层本身与其余成员保留）</template>。
      </p>

      <template #footer>
        <Btn data-test="cancel" @click="confirming = false">取消</Btn>
        <Btn data-test="confirm" variant="danger" @click="confirmRemove"><Trash2 :size="13" /> 删除</Btn>
      </template>
    </Modal>

    <!-- 删除模型确认 -->
    <Modal
      :open="modelConfirming"
      title="删除模型"
      width="440px"
      @close="modelConfirming = false"
    >
      <p class="text-[12.5px] leading-5 text-ink-2">
        确定删除模型绑定「<span class="font-medium text-ink">{{ pendingModel?.exposed || pendingModel?.id }}</span>」？
      </p>
      <p v-if="modelRefs.length" class="mt-1.5 text-[11.5px] leading-4 text-ink-3">
        它正被 {{ modelRefs.length }} 个分组引用：{{ modelRefs.map(g => g.name || g.id).join("、") }}。删除后会从这些分组的层级里摘掉该成员（层本身与其余成员保留）。
      </p>

      <template #footer>
        <Btn data-test="cancel-model" @click="modelConfirming = false">取消</Btn>
        <Btn data-test="confirm-model" variant="danger" @click="confirmRemoveModel"><Trash2 :size="13" /> 删除</Btn>
      </template>
    </Modal>
  </div>
</template>
