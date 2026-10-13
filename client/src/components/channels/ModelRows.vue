<script setup lang="ts">
// 「挂在该渠道下的可用模型」列表 —— 渠道卡片展开后的内容。
//
// 价格是就地编辑：单元格保留用户输入的原始串（draft），避免 "0." 这类中间态
// 被立即收敛成 0 而吞掉小数点（输入 0.15 会变成 15）。只有能解析成有限数字时
// 才写回 price_per_m。父组件保存/删除模型后会 bump epoch，草稿随之丢弃，
// 否则旧草稿会盖住新写入的价格。
import { reactive, watch } from "vue";
import { Layers, Pencil, Plus, Trash2 } from "@lucide/vue";
import { Btn, IconBtn, TInput, TToggle } from "../ui";
import type { ModelBinding } from "../../api";
import { groupsUsingModel, markDirty } from "../../store";

const PRICE_COLS = [
  { label: "输入", hint: "input" },
  { label: "输出", hint: "output" },
  { label: "缓存读", hint: "cache read" },
  { label: "缓存写", hint: "cache write" },
];

const props = defineProps<{
  /** 该渠道下挂的模型绑定（store.modelsOfChannel(channelId) 的结果）。 */
  models: ModelBinding[];
  /** 被一级故障转移层级引用的绑定 id，仅用于高亮。 */
  activeIds: Set<string>;
  /** 父组件每次保存/删除模型后自增，用来丢弃就地编辑草稿。 */
  epoch: number;
}>();

const emit = defineEmits<{
  edit: [m: ModelBinding];
  remove: [m: ModelBinding];
  add: [];
}>();

const drafts = reactive<Record<string, string>>({});

function cellKey(id: string, j: number): string {
  return id + ":" + j;
}
function cellValue(m: ModelBinding, j: number): string | number {
  const k = cellKey(m.id, j);
  return k in drafts ? drafts[k] : (m.price_per_m[j] ?? 0);
}
function setPrice(m: ModelBinding, j: number, v: string | number) {
  const s = String(v ?? "");
  drafts[cellKey(m.id, j)] = s;
  const t = s.trim();
  const n = t === "" ? 0 : Number(t);
  if (!Number.isFinite(n)) return;
  m.price_per_m[j] = n;
  markDirty();
}

watch(() => props.epoch, () => {
  for (const k of Object.keys(drafts)) delete drafts[k];
});
</script>

<template>
  <div>
    <div v-if="models.length" class="overflow-x-auto rounded-lg border border-line bg-surface">
      <table class="w-full min-w-[778px] table-fixed text-[12.5px]">
        <thead>
          <tr>
            <th class="w-[62px] whitespace-nowrap px-1.5 py-1.5 text-left text-[11px] font-medium uppercase tracking-wide text-ink-3">绑定 ID</th>
            <th class="w-[100px] whitespace-nowrap px-1.5 py-1.5 text-left text-[11px] font-medium uppercase tracking-wide text-ink-3">暴露模型名</th>
            <th class="w-[100px] whitespace-nowrap px-1.5 py-1.5 text-left text-[11px] font-medium uppercase tracking-wide text-ink-3">上游模型名</th>
            <th
              v-for="c in PRICE_COLS" :key="c.hint"
              class="w-[84px] whitespace-nowrap px-1.5 py-1.5 text-left text-[11px] font-medium uppercase tracking-wide text-ink-3"
              :title="'USD / 1M tokens · ' + c.hint"
            >{{ c.label }}</th>
            <th class="w-[72px] whitespace-nowrap px-1.5 py-1.5 text-left text-[11px] font-medium uppercase tracking-wide text-ink-3">分组引用</th>
            <th class="w-[44px] whitespace-nowrap px-1.5 py-1.5 text-left text-[11px] font-medium uppercase tracking-wide text-ink-3">启用</th>
            <th class="w-[64px] whitespace-nowrap px-1.5 py-1.5 text-right text-[11px] font-medium uppercase tracking-wide text-ink-3">操作</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="m in models" :key="m.id" class="border-t border-line-soft transition-colors hover:bg-app">
            <td class="truncate px-1.5 py-1.5 font-mono text-[11.5px] text-ink-3" :title="m.id">{{ m.id }}</td>
            <td class="truncate px-1.5 py-1.5 font-mono text-[12px] text-ink" :title="m.exposed">{{ m.exposed }}</td>
            <td class="truncate px-1.5 py-1.5 font-mono text-[12px] text-ink-2" :title="m.upstream">{{ m.upstream }}</td>
            <td v-for="(c, j) in PRICE_COLS" :key="c.hint" class="px-1 py-1">
              <TInput
                :model-value="cellValue(m, j)"
                type="number"
                size="sm"
                mono
                @update:model-value="setPrice(m, j, $event)"
              />
            </td>
            <td class="px-1.5 py-1.5">
              <div class="flex min-w-0 items-center gap-1">
                <span
                  v-if="activeIds.has(m.id)"
                  class="h-1.5 w-1.5 shrink-0 rounded-full bg-teal-500"
                  title="被某个分组的一级层级引用"
                ></span>
                <span
                  v-if="groupsUsingModel(m.id).length"
                  class="truncate text-[11.5px] text-ink-2"
                  :title="groupsUsingModel(m.id).map(g => g.name || g.id).join('、')"
                >{{ groupsUsingModel(m.id).length }} 个分组</span>
                <span v-else class="truncate text-[11.5px] text-ink-3">未引用</span>
              </div>
            </td>
            <td class="whitespace-nowrap px-1.5 py-1.5">
              <TToggle v-model="m.enabled" @update:model-value="markDirty()" />
            </td>
            <td class="whitespace-nowrap px-1.5 py-1">
              <div class="flex items-center justify-end gap-0.5">
                <IconBtn title="编辑模型" @click="emit('edit', m)"><Pencil :size="15" /></IconBtn>
                <IconBtn title="删除模型" danger @click="emit('remove', m)"><Trash2 :size="15" /></IconBtn>
              </div>
            </td>
          </tr>
        </tbody>
      </table>
    </div>

    <div v-else class="flex flex-wrap items-center gap-2 rounded-lg border border-dashed border-line px-3 py-2.5">
      <span class="inline-flex items-center gap-1.5 text-[12.5px] text-ink-3">
        <Layers :size="14" /> 该渠道下还没有可用模型
      </span>
      <Btn variant="subtle" size="sm" class="ml-auto" data-test="add-model-empty" @click="emit('add')">
        <Plus :size="13" /> 添加模型
      </Btn>
    </div>

    <div v-if="models.length" class="mt-2 flex flex-wrap items-center gap-2">
      <p class="text-[11.5px] text-ink-3">
        价格单位 USD / 1M tokens（输入 · 输出 · 缓存读 · 缓存写），可直接在表格中修改。
      </p>
      <Btn variant="subtle" size="sm" class="ml-auto" data-test="add-model-row" @click="emit('add')">
        <Plus :size="13" /> 添加模型
      </Btn>
    </div>
  </div>
</template>
