<script setup lang="ts">
const model = defineModel<string | number>({ default: "" });
withDefaults(defineProps<{
  type?: string;
  placeholder?: string;
  mono?: boolean;
  disabled?: boolean;
  invalid?: boolean;
  size?: "sm" | "md";
}>(), { type: "text", mono: false, disabled: false, invalid: false, size: "md" });
</script>

<template>
  <input
    v-model="model" :type="type" :placeholder="placeholder" :disabled="disabled"
    class="w-full rounded-lg border bg-surface px-3 text-ink transition-colors placeholder:text-ink-3 disabled:bg-app disabled:text-ink-3"
    :class="[
      size === 'sm' ? 'h-8 text-[12.5px]' : 'h-9 text-[13px]',
      mono ? 'font-mono' : '',
      invalid ? 'border-err-500 focus:border-err-500' : 'border-line focus:border-brand-500',
      'focus:ring-2 focus:ring-brand-500/15'
    ]"
  />
</template>
