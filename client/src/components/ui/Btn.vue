<script setup lang="ts">
withDefaults(defineProps<{
  variant?: "primary" | "ghost" | "subtle" | "danger";
  size?: "sm" | "md";
  disabled?: boolean;
  type?: "button" | "submit";
  title?: string;
}>(), { variant: "ghost", size: "md", disabled: false, type: "button" });

const base = "inline-flex items-center justify-center gap-1.5 rounded-lg font-medium transition-colors select-none whitespace-nowrap disabled:opacity-50 disabled:pointer-events-none";
const sizes: Record<string, string> = {
  sm: "h-7 px-2.5 text-[12px]",
  md: "h-9 px-3.5 text-[13px]",
};
const variants: Record<string, string> = {
  primary: "bg-brand-500 text-white hover:bg-brand-600 shadow-sm",
  ghost: "border border-line bg-surface text-ink-2 hover:bg-app hover:text-ink",
  subtle: "text-ink-2 hover:bg-app hover:text-ink",
  danger: "border border-err-100 bg-surface text-err-600 hover:bg-err-50",
};
</script>

<template>
  <button :type="type" :title="title" :disabled="disabled" :class="[base, sizes[size], variants[variant]]" v-bind="$attrs">
    <slot />
  </button>
</template>
