<script setup lang="ts">
defineProps<{ title: string; hint?: string }>();
</script>

<template>
  <div class="flex flex-col items-center justify-center gap-2 py-16 text-center">
    <div class="text-ink-3"><slot name="icon" /></div>
    <p class="text-[13.5px] font-medium text-ink-2">{{ title }}</p>
    <p v-if="hint" class="max-w-sm text-[12.5px] leading-5 text-ink-3">{{ hint }}</p>
    <div class="mt-2"><slot /></div>
  </div>
</template>
