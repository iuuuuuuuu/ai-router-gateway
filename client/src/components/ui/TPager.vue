<script setup lang="ts">
// 通用分页条：上一页 / 下一页 + 页码 + 跳页 + 每页条数。
// 只做「显示 + 改页码」，不自己发请求：页码由调用方持有（page 是 v-model），
// 变化后由调用方的 watch 去拉数据，这样分页控件不会和业务请求耦合。
import { computed, ref, watch } from "vue";
import { ChevronLeft, ChevronRight } from "@lucide/vue";
import Btn from "./Btn.vue";
import TInput from "./TInput.vue";
import TSelect from "./TSelect.vue";

const props = withDefaults(defineProps<{
  /** 当前页，从 1 开始。 */
  total: number;
  /** 每页条数。 */
  pageSize: number;
  /** 每页条数可选值；传空数组则隐藏「每页条数」下拉。 */
  sizeOptions?: number[];
  /** 数据还在路上时禁用按钮，避免连点产生错位请求。 */
  loading?: boolean;
  /** 单页时的措辞，例如「条记录」「个模型」。 */
  unit?: string;
  /** 是否显示「第 X / Y 页」。 */
  showCounter?: boolean;
}>(), {
  sizeOptions: () => [20, 50, 100, 200, 500],
  loading: false,
  unit: "条记录",
  showCounter: true,
});

const page = defineModel<number>("page", { default: 1 });
const size = defineModel<number>("size", { default: 100 });

const pages = computed(() => Math.max(1, Math.ceil(Math.max(0, props.total) / Math.max(1, props.pageSize))));

// 页码越界（筛选变窄、总页数变少）时收敛回最后一页。写回 model 后由调用方重新拉取。
watch([pages, () => props.total], () => {
  if (page.value > pages.value) page.value = pages.value;
  if (page.value < 1) page.value = 1;
});

const from = computed(() => (props.total <= 0 ? 0 : (page.value - 1) * props.pageSize + 1));
const to = computed(() => Math.min(props.total, page.value * props.pageSize));

const sizeOpts = computed(() =>
  props.sizeOptions.map(n => ({ value: String(n), label: `每页 ${n} 条` })));
const sizeModel = computed({
  get: () => String(props.pageSize),
  set: (v: string) => {
    const n = Number(v);
    if (Number.isFinite(n) && n > 0) {
      size.value = n;
      page.value = 1;
    }
  },
});

const jump = ref<string>("");
function doJump() {
  const n = Number(jump.value);
  jump.value = "";
  if (!Number.isFinite(n)) return;
  const p = Math.min(pages.value, Math.max(1, Math.round(n)));
  if (p !== page.value) page.value = p;
}
function go(p: number) {
  const next = Math.min(pages.value, Math.max(1, p));
  if (next !== page.value) page.value = next;
}
</script>

<template>
  <div class="flex flex-wrap items-center gap-2 border-t border-line bg-app/60 px-4 py-2.5" data-test="pager">
    <div class="text-[12px] text-ink-3" data-test="pager-range">
      <template v-if="total > 0">第 <span class="font-mono text-ink-2">{{ from }}</span>–<span class="font-mono text-ink-2">{{ to }}</span> 条 / 共 <span class="font-mono text-ink-2">{{ total }}</span> {{ unit }}</template>
      <template v-else>共 0 {{ unit }}</template>
    </div>

    <div class="ml-auto flex flex-wrap items-center gap-2">
      <!-- data-test 挂在包裹 div 上：reka-ui 的 SelectRoot 是 inheritAttrs:false 且没有 v-bind="$attrs"，
           直接挂在 <TSelect> 上的 attrs 到不了 button[role=combobox]。 -->
      <div v-if="sizeOpts.length" class="w-[120px]" data-test="pager-size">
        <TSelect v-model="sizeModel" :options="sizeOpts" size="sm" />
      </div>
      <Btn size="sm" :disabled="loading || page <= 1" data-test="pager-prev" @click="go(page - 1)">
        <ChevronLeft :size="14" />上一页
      </Btn>
      <span v-if="showCounter" class="min-w-[74px] text-center text-[12px] text-ink-3" data-test="pager-counter">
        第 <span class="font-mono text-ink-2">{{ page }}</span> / <span class="font-mono text-ink-2">{{ pages }}</span> 页
      </span>
      <Btn size="sm" :disabled="loading || page >= pages" data-test="pager-next" @click="go(page + 1)">
        下一页<ChevronRight :size="14" />
      </Btn>
      <span class="flex items-center gap-1 text-[12px] text-ink-3">
        跳至
        <span class="w-[62px]"><TInput v-model="jump" size="sm" type="number" data-test="pager-jump" @keyup.enter="doJump" /></span>
        页
        <Btn size="sm" :disabled="loading" data-test="pager-go" @click="doJump">跳转</Btn>
      </span>
    </div>
  </div>
</template>
