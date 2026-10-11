<script setup lang="ts">
withDefaults(defineProps<{ tone?: "err" | "ok" | "warn" | "info" }>(), { tone: "info" });
const tones: Record<string, string> = {
  err: "bg-err-50 border-err-100 text-err-700",
  ok: "bg-ok-50 border-ok-100 text-ok-700",
  warn: "bg-warn-50 border-warn-100 text-warn-700",
  info: "bg-app border-line text-ink-2",
};
</script>

<template>
  <div class="flex items-start gap-2 rounded-lg border px-3 py-2 text-[12.5px] leading-5" :class="tones[tone]">
    <slot />
  </div>
</template>
