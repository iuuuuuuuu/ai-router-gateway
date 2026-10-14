<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, shallowRef, watch } from "vue";
import { init, use, type EChartsCoreOption, type EChartsType } from "echarts/core";
import { BarChart, PieChart } from "echarts/charts";
import { GridComponent, LegendComponent, TooltipComponent } from "echarts/components";
import { CanvasRenderer } from "echarts/renderers";

// 只注册用得到的图表/组件/渲染器，保持打包体积可控（按需引入，不用 echarts 全量入口）。
use([BarChart, PieChart, GridComponent, LegendComponent, TooltipComponent, CanvasRenderer]);

const props = withDefaults(defineProps<{ option: EChartsCoreOption; height?: number }>(), { height: 260 });

const el = ref<HTMLDivElement | null>(null);
// shallowRef：ECharts 实例是个巨大的对象，绝不能让 Vue 深度代理它（代理后会破坏内部引用比较）。
const chart = shallowRef<EChartsType | null>(null);
let ro: ResizeObserver | null = null;

function draw() {
  if (!el.value) return;
  if (!chart.value) chart.value = init(el.value);
  // notMerge：每次都是完整的新 option，避免上一份数据残留的 series 混进图表。
  chart.value.setOption(props.option, { notMerge: true });
}

onMounted(() => {
  draw();
  // 侧栏折叠 / 窗口缩放都会改容器宽度，canvas 不会自己跟着变，必须显式 resize。
  if (typeof ResizeObserver !== "undefined" && el.value) {
    ro = new ResizeObserver(() => chart.value?.resize());
    ro.observe(el.value);
  }
});
watch(() => props.option, draw, { deep: true });
onBeforeUnmount(() => {
  if (ro) { ro.disconnect(); ro = null; }
  if (chart.value) { chart.value.dispose(); chart.value = null; }
});
</script>

<template>
  <div ref="el" class="w-full" :style="{ height: height + 'px' }" />
</template>
