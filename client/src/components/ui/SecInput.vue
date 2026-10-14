<script setup lang="ts">
// 毫秒 -> 中文「秒」的输入框。
//
// 服务端契约是毫秒，用户看不懂 ms，所以这一层只做双向换算：
//   显示 = 毫秒 / 1000，输入 = 秒 × 1000 四舍五入回毫秒。
// 落到 v-model 上的永远是毫秒，父组件的校验/预算公式/预置档位比对全部不用改。
//
// inheritAttrs:false + v-bind="$attrs" 是为了让父组件写在 <SecInput> 上的
// data-test / @input（markDirty）继续落到真正承载交互的 input 上，而不是飘到 wrapper div。
import { computed } from "vue";
import TInput from "./TInput.vue";

const model = defineModel<number>();
const props = withDefaults(defineProps<{ min?: number; max?: number; step?: number }>(), {
  min: 0.1,
  step: 0.1,
});
defineOptions({ inheritAttrs: false });

/** 毫秒 -> 秒。给到 4 位有效小数就够（0.005 秒 = 5ms），避免 0.30000000000000004 这种浮点尾巴。 */
const display = computed<string | number>({
  get() {
    const ms = Number(model.value);
    if (!Number.isFinite(ms)) return "";
    const sec = ms / 1000;
    return String(Math.round(sec * 10000) / 10000);
  },
  set(v: string | number) {
    const sec = Number(v);
    if (v === "" || !Number.isFinite(sec)) {
      model.value = undefined;
      return;
    }
    model.value = Math.max(0, Math.round(sec * 1000));
  },
});
</script>

<template>
  <div class="relative">
    <TInput v-model="display" v-bind="$attrs" type="number" :min="props.min" :step="props.step" class="pr-9" />
    <span class="pointer-events-none absolute right-3 top-1/2 -translate-y-1/2 text-[11.5px] text-ink-3">秒</span>
  </div>
</template>
