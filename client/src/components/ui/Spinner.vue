<script setup lang="ts">
import { LoaderCircle } from "@lucide/vue";
withDefaults(defineProps<{ size?: number }>(), { size: 14 });
</script>

<template>
  <LoaderCircle :size="size" class="animate-spin" />
</template>
