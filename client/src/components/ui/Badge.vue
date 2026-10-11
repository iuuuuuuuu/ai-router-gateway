<script setup lang="ts">
withDefaults(defineProps<{
  tone?: "gray" | "teal" | "ok" | "warn" | "err" | "brand";
}>(), { tone: "gray" });

const tones: Record<string, string> = {
  gray: "bg-app text-ink-2 border-line",
  teal: "bg-teal-50 text-teal-700 border-teal-200",
  ok: "bg-ok-50 text-ok-700 border-ok-100",
  warn: "bg-warn-50 text-warn-700 border-warn-100",
  err: "bg-err-50 text-err-700 border-err-100",
  brand: "bg-brand-50 text-brand-700 border-brand-200",
};
</script>

<template>
  <span class="inline-flex max-w-full min-w-0 items-center gap-1 overflow-hidden whitespace-nowrap rounded-md border px-1.5 py-px text-[11px] font-medium leading-[18px]" :class="tones[tone]">
    <slot />
  </span>
</template>
