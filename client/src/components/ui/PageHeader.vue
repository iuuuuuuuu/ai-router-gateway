<script setup lang="ts">
defineProps<{ title: string; subtitle?: string }>();
</script>

<template>
  <header class="mb-5 flex items-start justify-between gap-4">
    <div class="min-w-0">
      <h1 class="truncate text-[19px] font-semibold leading-7 tracking-[-0.01em] text-ink">{{ title }}</h1>
      <p v-if="subtitle" class="mt-0.5 text-[12.5px] text-ink-3">{{ subtitle }}</p>
    </div>
    <div class="flex shrink-0 items-center gap-2">
      <slot name="actions" />
    </div>
  </header>
</template>
