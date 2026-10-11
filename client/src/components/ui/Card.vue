<script setup lang="ts">
withDefaults(defineProps<{ pad?: boolean; hover?: boolean }>(), { pad: true, hover: false });
</script>

<template>
  <div
    class="rounded-xl border border-line bg-surface transition-shadow"
    :class="[pad ? 'p-4' : '', hover ? 'hover:shadow-sm' : '']"
  >
    <slot />
  </div>
</template>
