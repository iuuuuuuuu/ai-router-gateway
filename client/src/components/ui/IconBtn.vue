<script setup lang="ts">
withDefaults(defineProps<{
  title?: string;
  danger?: boolean;
  disabled?: boolean;
}>(), { danger: false, disabled: false });
</script>

<template>
  <button
    type="button" :title="title" :disabled="disabled"
    class="inline-flex h-8 w-8 shrink-0 items-center justify-center rounded-md text-ink-3 transition-colors hover:bg-app disabled:opacity-40 disabled:pointer-events-none"
    :class="danger ? 'hover:text-err-600 hover:bg-err-50' : 'hover:text-ink'"
  >
    <slot />
  </button>
</template>
