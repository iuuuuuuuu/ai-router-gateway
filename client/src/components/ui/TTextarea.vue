<script setup lang="ts">
const model = defineModel<string>({ default: "" });
withDefaults(defineProps<{ rows?: number; placeholder?: string; mono?: boolean; disabled?: boolean }>(), {
  rows: 4, mono: true, disabled: false,
});
</script>

<template>
  <textarea
    v-model="model" :rows="rows" :placeholder="placeholder" :disabled="disabled"
    class="w-full resize-y rounded-lg border border-line bg-surface px-3 py-2 text-[12.5px] leading-5 text-ink transition-colors placeholder:text-ink-3 disabled:bg-app focus:border-brand-500 focus:ring-2 focus:ring-brand-500/15"
    :class="mono ? 'font-mono' : ''"
  />
</template>
