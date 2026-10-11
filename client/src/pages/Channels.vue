<script setup lang="ts">
// 渠道 + 模型合并页：每个渠道一张卡片，展开后是该渠道下挂的可用模型。
// 模型绑定（暴露名 / 上游名 / 4 档价格 / 启用）的增删改都在本页完成，
// 原「模型」独立页的能力已并入这里。
import { computed, reactive, ref } from "vue";
import { Check, Eye, EyeOff, Layers, Plug, Plus, Trash2 } from "@lucide/vue";
import {
  Alert, Btn, Card, Empty, Field, IconBtn, Modal,
  TInput, TSelect, TTextarea, TToggle,
} from "../components/ui";
import ChannelCard from "../components/channels/ChannelCard.vue";
import { api, type Channel, type ModelBinding } from "../api";
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
 * 「使用中」标注口径：列表里第一个 enabled === true 的渠道。
 * 渠道的 weight 在服务端不参与路由（同级候选由分组层级里的顺序/权重决定），
 * 所以这里用「按顺序的第一个可用渠道」来标注，避免权重造成误导。仅用于高亮，不参与调度决策。
 */
const activeId = computed(() => channels.value.find(c => c.enabled)?.id ?? "");

/**
 * 「使用中」标注口径（模型）：被任意分组一级（level === 1）引用；
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

/* ------------------------------- 连通测试 -------------------------------- */

const testingId = ref("");
async function test(ch: Channel) {
  if (testingId.value) return;
  testingId.value = ch.id;
  try {
    const r = await api.testChannel(ch.id);
    if (r.ok) {
      toast.ok("连通正常 · " + r.latency_ms + "ms");
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

/** 确认删除：WebView2 里 window.confirm 不可用，改用页面内 Modal。 */
function confirmRemove() {
  const cfg = state.cfg;
  const ch = pendingDelete.value;
  if (!cfg || !ch) return;
  const bound = cfg.models.filter(m => m.channel_id === ch.id).length;
  cfg.channels = cfg.channels.filter(c => c.id !== ch.id);
  if (bound) cfg.models = cfg.models.filter(m => m.channel_id !== ch.id);
  markDirty();
  epoch.value++;
  confirming.value = false;
  toast.ok(bound ? "已删除渠道，并移除 " + bound + " 个模型绑定" : "渠道已删除");
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
  cfg.models = cfg.models.filter(x => x.id !== m.id);
  markDirty();
  epoch.value++;
  modelConfirming.value = false;
  toast.ok("模型绑定已删除");
}
</script>

<template>
  <div>
    <div class="mb-3 flex flex-wrap items-center gap-2">
      <div v-if="channels.length" class="text-[12px] text-ink-3">
        共 {{ channels.length }} 个渠道 · 已启用 {{ enabledCount }} 个 · 模型绑定 {{ models.length }} 个
      </div>
      <div class="ml-auto flex items-center gap-2">
        <Btn v-if="channels.length" variant="subtle" size="sm" data-test="toggle-all" @click="toggleAll">
          <Layers :size="13" /> {{ anyExpanded ? "收起全部" : "展开全部" }}
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
        :active="ch.id === activeId"
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
        <Field label="所属渠道" required hint="模型挂在这个渠道下，路由标识为「渠道-模型」">
          <TSelect
            v-model="modelForm.channel_id"
            :options="channelOptions"
            placeholder="选择渠道"
            empty-text="还没有渠道"
          />
        </Field>

        <div class="grid grid-cols-1 gap-3 sm:grid-cols-2">
          <Field label="暴露模型名" required hint="客户端请求时使用的名字">
            <TInput
              v-model="modelForm.exposed"
              mono
              :invalid="!!modelError && !modelForm.exposed.trim()"
              placeholder="gpt-4o"
            />
          </Field>
          <Field label="上游模型名" required hint="转发给上游时使用的名字">
            <TInput
              v-model="modelForm.upstream"
              mono
              :invalid="!!modelError && !modelForm.upstream.trim()"
              placeholder="gpt-4o-2024-08-06"
            />
          </Field>
        </div>

        <Field label="绑定 ID" :hint="modelEditingId ? '已有绑定的 ID 不可修改' : '新增时自动生成，可自行调整'">
          <TInput v-model="modelForm.id" mono :disabled="!!modelEditingId" />
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
      </div>

      <template #footer>
        <Btn @click="closeModelModal">取消</Btn>
        <Btn variant="primary" data-test="save-model" @click="saveModel"><Check :size="13" /> 保存</Btn>
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
        会级联删除该渠道下的 {{ deleteImpact.models }} 个模型绑定<template v-if="deleteImpact.groups">，并影响 {{ deleteImpact.groups }} 个分组的故障转移层级（这些候选会消失）</template>。
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
        它正被 {{ modelRefs.length }} 个分组引用：{{ modelRefs.map(g => g.name || g.id).join("、") }}。删除后这些层级会少掉一个候选。
      </p>

      <template #footer>
        <Btn data-test="cancel-model" @click="modelConfirming = false">取消</Btn>
        <Btn data-test="confirm-model" variant="danger" @click="confirmRemoveModel"><Trash2 :size="13" /> 删除</Btn>
      </template>
    </Modal>
  </div>
</template>
