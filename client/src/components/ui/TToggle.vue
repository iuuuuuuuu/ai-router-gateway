<script setup lang="ts">
// reka-ui Switch: real keyboard support (Space/Enter) and aria-checked,
// identical geometry to the previous hand-rolled toggle so layouts and the
// CDP click probes keep working.
import { SwitchRoot, SwitchThumb } from "reka-ui";

const model = defineModel<boolean>({ default: false });
withDefaults(defineProps<{ label?: string; disabled?: boolean }>(), { disabled: false });
</script>

<template>
  <SwitchRoot
    v-model="model" :disabled="disabled"
    class="inline-flex items-center gap-2 disabled:opacity-50"
  >
    <span
      class="relative inline-block h-[18px] w-8 shrink-0 rounded-full transition-colors"
      :class="model ? 'bg-teal-500' : 'bg-[#d4d4d8]'"
    >
      <SwitchThumb
        class="absolute top-[2px] block h-[14px] w-[14px] rounded-full bg-white shadow-sm transition-all"
        :class="model ? 'left-[16px]' : 'left-[2px]'"
      />
    </span>
    <span v-if="label" class="text-[12.5px] text-ink-2">{{ label }}</span>
  </SwitchRoot>
</template>
