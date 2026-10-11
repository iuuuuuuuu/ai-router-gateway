<script setup lang="ts">
// 渠道卡片：头部是渠道本身，展开后是挂在该渠道下的可用模型（ModelRows）。
import { computed, ref } from "vue";
import { ChevronDown, Eye, EyeOff, FlaskConical, Layers, Pencil, Trash2 } from "@lucide/vue";
import { Badge, Btn, IconBtn, Spinner, TToggle } from "../ui";
import ModelRows from "./ModelRows.vue";
import type { Channel, ModelBinding } from "../../api";
import { markDirty } from "../../store";

const props = defineProps<{
  channel: Channel;
  /** 该渠道下挂的模型绑定。 */
  models: ModelBinding[];
  /** 被一级故障转移层级引用的绑定 id，仅用于高亮。 */
  activeIds: Set<string>;
  /** 权重最高的启用渠道 —— 仅用于「使用中」高亮，不参与调度决策。 */
  active: boolean;
  /** 该渠道的连通测试是否进行中。 */
  testing: boolean;
  expanded: boolean;
  epoch: number;
}>();

const emit = defineEmits<{
  toggleExpand: [];
  test: [];
  edit: [];
  remove: [];
  editModel: [m: ModelBinding];
  removeModel: [m: ModelBinding];
  addModel: [];
}>();

const protocolTone: Record<string, string> = {
  openai: "border-brand-200 bg-brand-50 text-brand-700",
  anthropic: "border-warn-100 bg-warn-50 text-warn-700",
  gemini: "border-teal-200 bg-teal-50 text-teal-700",
};

const showKey = ref(false);
const avatarClass = computed(() => protocolTone[props.channel.protocol] ?? "border-line bg-app text-ink-2");
const initial = computed(() => (props.channel.name || props.channel.id || "?").trim().charAt(0).toUpperCase());
const hasKey = computed(() => !!props.channel.api_key);
const enabledModels = computed(() => props.models.filter(m => m.enabled).length);
</script>

<template>
  <div
    class="overflow-hidden rounded-[10px] border transition-all"
    :class="active ? 'border-teal-200 bg-teal-50/60' : (channel.enabled ? 'border-line bg-surface' : 'border-line bg-app opacity-70')"
  >
    <div class="flex items-center gap-3 px-3 py-2.5">
      <button
        type="button"
        class="flex h-9 w-9 shrink-0 cursor-pointer items-center justify-center rounded-lg border text-[14px] font-semibold transition-transform hover:border-ink-3/40"
        :class="[avatarClass, expanded ? 'rotate-90' : '']"
        :title="expanded ? '收起模型' : '展开模型'"
        :aria-expanded="expanded"
        @click="emit('toggleExpand')"
      >
        <ChevronDown v-if="expanded" :size="16" />
        <span v-else>{{ initial }}</span>
      </button>

      <div class="min-w-0 flex-1">
        <div class="flex min-w-0 flex-wrap items-center gap-1.5">
          <span class="truncate text-[13.5px] font-semibold text-ink">{{ channel.name || channel.id }}</span>
          <Badge tone="teal">{{ channel.protocol }}</Badge>
          <Badge tone="gray">权重 {{ channel.weight }}</Badge>
          <Badge v-if="!channel.enabled" tone="err">已停用</Badge>
        </div>
        <div class="mt-1 flex min-w-0 items-center gap-2">
          <span class="min-w-0 truncate font-mono text-[11.5px] text-ink-3">{{ channel.base_url || "—" }}</span>
          <span class="shrink-0 text-[11.5px] text-ink-3" :title="'已启用的模型 / 全部模型'">
            模型 {{ enabledModels }}/{{ models.length }}
          </span>
        </div>
      </div>

      <span
        v-if="active"
        class="ml-1 inline-flex shrink-0 items-center gap-1 text-[11.5px] font-medium text-teal-700"
      >
        <span class="h-1.5 w-1.5 rounded-full bg-teal-500"></span>使用中
      </span>

      <div class="flex shrink-0 items-center gap-0.5">
        <IconBtn title="连通测试" :disabled="testing" @click="emit('test')">
          <Spinner v-if="testing" :size="14" />
          <FlaskConical v-else :size="15" />
        </IconBtn>
        <IconBtn title="编辑渠道" @click="emit('edit')"><Pencil :size="15" /></IconBtn>
        <IconBtn title="删除渠道" danger @click="emit('remove')"><Trash2 :size="15" /></IconBtn>
        <IconBtn :title="expanded ? '收起模型' : '展开模型'" @click="emit('toggleExpand')">
          <ChevronDown :size="16" class="transition-transform" :class="expanded ? 'rotate-180' : ''" />
        </IconBtn>
      </div>
    </div>

    <div class="flex flex-wrap items-center gap-2 border-t border-line-soft px-3 py-1.5">
      <button
        type="button"
        class="inline-flex cursor-pointer items-center gap-1.5 rounded-md px-1 py-0.5 font-mono text-[11.5px] text-ink-3 transition-colors hover:text-ink-2"
        :title="hasKey ? (showKey ? '隐藏密钥' : '显示密钥') : '未配置密钥'"
        @click="showKey = !showKey"
      >
        <EyeOff v-if="showKey" :size="13" />
        <Eye v-else :size="13" />
        <span class="max-w-[260px] truncate">{{ hasKey ? (showKey ? channel.api_key : "密钥已配置 ·••••••") : "未配置密钥" }}</span>
      </button>

      <button
        type="button"
        class="ml-auto inline-flex cursor-pointer items-center gap-1.5 rounded-md px-1 py-0.5 text-[11.5px] text-ink-3 transition-colors hover:text-ink-2"
        @click="emit('toggleExpand')"
      >
        <Layers :size="13" />
        {{ expanded ? '收起' : '可用模型 ' + models.length + ' 个' }}
        <ChevronDown :size="13" class="transition-transform" :class="expanded ? 'rotate-180' : ''" />
      </button>

      <TToggle v-model="channel.enabled" @update:model-value="markDirty()" />
    </div>

    <div v-if="expanded" class="border-t border-line-soft bg-app/40 px-3 py-2.5">
      <ModelRows
        :models="models"
        :active-ids="activeIds"
        :epoch="epoch"
        @edit="(m) => emit('editModel', m)"
        @remove="(m) => emit('removeModel', m)"
        @add="emit('addModel')"
      />
    </div>
  </div>
</template>
