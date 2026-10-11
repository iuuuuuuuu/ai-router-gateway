<script setup lang="ts">
// Headless reka-ui Select (Radix Vue successor) replacing the native <select>.
// Public props stay compatible with the old component so every page keeps
// working; the upgrade buys keyboard navigation, type-ahead, a searchable
// filter, and rich option rows (e.g. "渠道-模型" with a muted hint).
import { computed, nextTick, ref } from "vue";
import { Check, ChevronDown, Search } from "@lucide/vue";
import {
  SelectContent, SelectIcon, SelectItem, SelectItemIndicator, SelectItemText,
  SelectPortal, SelectRoot, SelectTrigger, SelectValue, SelectViewport,
} from "reka-ui";

export interface SelectOption {
  value: string;
  label: string;
  /** Right-aligned muted text inside the row (never shown in the trigger). */
  hint?: string;
  /** Optional non-interactive group header rendered above this row. */
  group?: string;
  disabled?: boolean;
}

const model = defineModel<string>({ default: "" });
const props = withDefaults(defineProps<{
  options: SelectOption[];
  placeholder?: string;
  disabled?: boolean;
  size?: "sm" | "md";
  /** Show a filter box once the list is longer than a few rows. */
  searchable?: boolean;
  /** Rendered when the list is empty (e.g. "请先添加渠道"). */
  emptyText?: string;
}>(), { disabled: false, size: "md", searchable: false, emptyText: "无可选项" });

const query = ref("");
const trigger = ref<{ $el: HTMLElement } | null>(null);

const autoSearch = computed(() => props.searchable || props.options.length > 9);
const filtered = computed(() => {
  const q = query.value.trim().toLowerCase();
  if (!q) return props.options;
  return props.options.filter(o =>
    o.label.toLowerCase().includes(q) ||
    (o.hint ?? "").toLowerCase().includes(q) ||
    (o.group ?? "").toLowerCase().includes(q));
});

// Group headers are derived, so callers only pass a flat list.
const rows = computed(() => {
  let last = "";
  return filtered.value.map(o => {
    const header = o.group && o.group !== last ? o.group : "";
    last = o.group ?? "";
    return { o, header };
  });
});

function onOpenChange(open: boolean) {
  if (open) { query.value = ""; void nextTick(() => { focusFilter(); }); }
}
function focusFilter() {
  const el = (trigger.value?.$el?.parentElement ?? document).querySelector<HTMLInputElement>("[data-select-filter]");
  el?.focus();
}
</script>

<template>
  <SelectRoot :model-value="model" :disabled="disabled" @update:model-value="(v) => (model = v as string)" @update:open="onOpenChange">
    <SelectTrigger
      ref="trigger"
      class="flex w-full cursor-pointer items-center justify-between gap-2 rounded-lg border border-line bg-surface px-3 text-left text-ink transition-colors data-[placeholder]:text-ink-3 disabled:cursor-not-allowed disabled:bg-app disabled:text-ink-3 focus:border-brand-500 focus:ring-2 focus:ring-brand-500/15 focus:outline-none"
      :class="size === 'sm' ? 'h-8 text-[12.5px]' : 'h-9 text-[13px]'"
    >
      <span class="min-w-0 flex-1 truncate"><SelectValue :placeholder="placeholder" /></span>
      <SelectIcon class="shrink-0 text-ink-3"><ChevronDown :size="16" /></SelectIcon>
    </SelectTrigger>
    <SelectPortal>
      <SelectContent
        position="popper" :side-offset="4"
        class="z-[60] max-h-[320px] min-w-[var(--reka-select-trigger-width)] overflow-hidden rounded-lg border border-line bg-surface shadow-xl"
      >
        <div v-if="autoSearch" class="border-b border-line-soft p-1.5">
          <div class="flex items-center gap-1.5 rounded-md bg-app px-2">
            <Search :size="13" class="shrink-0 text-ink-3" />
            <input
              v-model="query" data-select-filter type="text" placeholder="搜索…"
              class="h-7 w-full bg-transparent text-[12.5px] text-ink placeholder:text-ink-3 focus:outline-none"
              @keydown.stop
            />
          </div>
        </div>
        <SelectViewport class="max-h-[268px] overflow-y-auto p-1">
          <template v-for="row in rows" :key="row.o.value">
            <div v-if="row.header" class="px-2 pt-2 pb-1 text-[10.5px] font-semibold tracking-wide text-ink-3 uppercase">{{ row.header }}</div>
            <SelectItem
              :value="row.o.value" :disabled="row.o.disabled"
              class="flex cursor-pointer items-center gap-2 rounded-md px-2 py-1.5 text-[12.5px] text-ink-2 outline-none select-none data-[disabled]:pointer-events-none data-[disabled]:opacity-40 data-[highlighted]:bg-app data-[highlighted]:text-ink data-[state=checked]:text-ink"
            >
              <SelectItemIndicator class="shrink-0 text-brand-500"><Check :size="13" /></SelectItemIndicator>
              <SelectItemText class="min-w-0 flex-1 truncate">{{ row.o.label }}</SelectItemText>
              <span v-if="row.o.hint" class="shrink-0 font-mono text-[11px] text-ink-3">{{ row.o.hint }}</span>
            </SelectItem>
          </template>
          <div v-if="!rows.length" class="px-2 py-3 text-center text-[12px] text-ink-3">{{ emptyText }}</div>
        </SelectViewport>
      </SelectContent>
    </SelectPortal>
  </SelectRoot>
</template>
