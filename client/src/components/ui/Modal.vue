<script setup lang="ts">
// reka-ui Dialog wrapper (Radix Vue successor). Same public API as before
// (open / title / width + @close) so every page keeps working, but now with a
// real focus trap, Escape-to-close, scroll lock, and body-scoped portal.
// The content keeps the z-50 + data-test contract the CDP probes rely on.
import { X } from "@lucide/vue";
import { DialogContent, DialogOverlay, DialogRoot, DialogTitle } from "reka-ui";
import IconBtn from "./IconBtn.vue";

withDefaults(defineProps<{ open: boolean; title?: string; width?: string }>(), { width: "560px" });
const emit = defineEmits<{ close: [] }>();
</script>

<template>
  <DialogRoot :open="open" @update:open="(v) => !v && emit('close')">
    <DialogOverlay class="fixed inset-0 z-50 bg-[#17171a]/25" />
    <DialogContent
      class="fixed top-6 left-1/2 z-50 max-h-[calc(100vh-3rem)] w-full -translate-x-1/2 overflow-y-auto rounded-xl border border-line bg-surface shadow-xl focus:outline-none"
      :style="{ maxWidth: width }"
    >
      <!-- The dialog itself is the scroll container, so a long form used to push
           the actions below the fold: reaching 保存 required scrolling first.
           Pinning header and footer keeps them reachable at any scroll offset. -->
      <div class="sticky top-0 z-10 flex items-center justify-between border-b border-line-soft bg-surface px-5 py-3.5">
        <DialogTitle class="text-[14.5px] font-semibold text-ink">{{ title }}</DialogTitle>
        <IconBtn title="关闭" @click="emit('close')"><X :size="16" /></IconBtn>
      </div>
      <div class="px-5 py-4"><slot /></div>
      <div v-if="$slots.footer" class="sticky bottom-0 z-10 flex items-center justify-end gap-2 border-t border-line-soft bg-surface px-5 py-3">
        <slot name="footer" />
      </div>
    </DialogContent>
  </DialogRoot>
</template>
