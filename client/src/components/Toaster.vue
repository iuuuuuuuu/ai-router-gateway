<script setup lang="ts">
import { CircleCheck, CircleAlert, TriangleAlert, Info, X } from "@lucide/vue";
import { toasts, toast } from "../toast";

const icons: Record<string, any> = { ok: CircleCheck, err: CircleAlert, warn: TriangleAlert, info: Info };
const tones: Record<string, string> = {
  ok: "border-ok-100 bg-ok-50 text-ok-700",
  err: "border-err-100 bg-err-50 text-err-700",
  warn: "border-warn-100 bg-warn-50 text-warn-700",
  info: "border-line bg-surface text-ink-2",
};
</script>

<template>
  <Teleport to="body">
    <div class="pointer-events-none fixed bottom-4 right-4 z-[100] flex w-[340px] flex-col gap-2">
      <TransitionGroup name="toast">
        <div
          v-for="t in toasts" :key="t.id"
          class="pointer-events-auto flex items-start gap-2 rounded-lg border px-3 py-2.5 text-[12.5px] leading-5 shadow-sm"
          :class="tones[t.tone]"
        >
          <component :is="icons[t.tone]" :size="15" class="mt-0.5 shrink-0" />
          <span class="min-w-0 flex-1 break-words">{{ t.text }}</span>
          <button class="shrink-0 opacity-50 hover:opacity-100" @click="toast.dismiss(t.id)"><X :size="13" /></button>
        </div>
      </TransitionGroup>
    </div>
  </Teleport>
</template>

<style scoped>
.toast-enter-active, .toast-leave-active { transition: all .18s ease; }
.toast-enter-from, .toast-leave-to { opacity: 0; transform: translateX(12px); }
</style>
