<script setup lang="ts">
// 时间范围快捷选择（1/2/3/7/14/30 天）。日志 / 统计 / 分组详情共用一份，
// 免得各写一套天数列表后互相漂移。
//
// 用 TSelect 而不是一排按钮：天数以后要加时只改 dayRange.ts 一处，
// 工具条宽度也不会随选项数膨胀。
import { computed } from "vue";
import TSelect from "./TSelect.vue";
import { DAY_RANGES, DEFAULT_DAYS, normalizeDays } from "./dayRange";

const model = defineModel<number>({ default: DEFAULT_DAYS });

const options = computed(() => DAY_RANGES.map(d => ({ value: String(d), label: `${d} 天` })));

// reka-ui 的 SelectItem 不接受空字符串 value，所以用字符串承载数字，
// 只在越过组件边界时转回 number。
const asString = computed({
  get: () => String(model.value),
  set: (v: string) => { model.value = normalizeDays(v); },
});
</script>

<template>
  <TSelect v-model="asString" size="sm" :options="options" placeholder="时间范围" />
</template>
