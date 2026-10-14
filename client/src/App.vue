<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from "vue";
import {
  Zap, Plug, Waypoints, Settings as SettingsIcon, ScrollText, ChartColumn,
  PanelLeftClose, PanelLeftOpen, RefreshCw, Save, CircleAlert, Wifi, WifiOff, LoaderCircle,
  ArrowLeft,
} from "@lucide/vue";
import { groups, loadConfig, saveConfig, state, revert } from "./store";
import { closeGroupDetail, goto, nav, type PageKey } from "./nav";
import { api } from "./api";
import Toaster from "./components/Toaster.vue";
import Btn from "./components/ui/Btn.vue";
import IconBtn from "./components/ui/IconBtn.vue";

import Channels from "./pages/Channels.vue";
import Groups from "./pages/Groups.vue";
import GroupDetail from "./pages/GroupDetail.vue";
import SettingsPage from "./pages/Settings.vue";
import Logs from "./pages/Logs.vue";
import Stats from "./pages/Stats.vue";
import ModelDetail from "./pages/ModelDetail.vue";

const navItems: { key: PageKey; label: string; icon: any; hint: string }[] = [
  { key: "channels", label: "渠道与模型", icon: Plug, hint: "上游供应商、密钥，以及渠道下挂的可用模型" },
  { key: "groups", label: "分组路由", icon: Waypoints, hint: "一级 / 二级 / 三级 故障转移" },
  { key: "settings", label: "设置", icon: SettingsIcon, hint: "重试 / 监听 / 管理面 / 自启" },
  { key: "logs", label: "日志", icon: ScrollText, hint: "切换记录与请求记录" },
  { key: "stats", label: "统计", icon: ChartColumn, hint: "成本与冷却状态" },
];

const scroller = ref<HTMLElement | null>(null);

// Each view owns its own scroll position; without this the next one opens
// scrolled to wherever the previous was left, which feels broken.
watch([() => nav.page, () => nav.groupId, () => nav.modelKey], async () => {
  await nextTick();
  scroller.value?.scrollTo({ top: 0 });
});
const collapsed = ref(localStorage.getItem("nav_collapsed") === "1");
function toggleCollapse() {
  collapsed.value = !collapsed.value;
  localStorage.setItem("nav_collapsed", collapsed.value ? "1" : "0");
}

const current = computed(() => navItems.find(n => n.key === nav.page)!);

/** The group whose detail page is open (null = no detail page). */
const detailGroup = computed(() =>
  nav.groupId ? groups.value.find(g => g.id === nav.groupId) ?? null : null);

// Model detail wins over the group detail: opening a member from the group page
// keeps nav.groupId set, so the modelKey branch has to be checked first.
const pageComp = computed(() =>
  nav.modelKey ? ModelDetail
    : nav.groupId ? GroupDetail
    : {
      channels: Channels, groups: Groups,
      settings: SettingsPage, logs: Logs, stats: Stats,
    }[nav.page]);

/** Transition key: entering/leaving a detail page (or switching member) is a page change too. */
const viewKey = computed(() => nav.modelKey
  ? "model:" + nav.groupId + ":" + nav.modelKey
  : nav.groupId ? "group:" + nav.groupId : nav.page);

/** A detail view of either kind owns the title bar; the model detail renders its own
 *  in-page back button, so only the group detail gets one up here. */
const detailOpen = computed(() => Boolean(nav.modelKey || detailGroup.value));

/* ---------------------------- health polling ---------------------------- */
let timer: number | undefined;
async function ping() {
  try {
    await api.health();
    state.online = true;
  } catch {
    state.online = false;
  }
}
onMounted(async () => {
  await loadConfig();
  await ping();
  timer = window.setInterval(ping, 5000);
});
onUnmounted(() => { if (timer) window.clearInterval(timer); });

const heading = computed(() => (nav.modelKey ? "模型详情"
  : detailGroup.value ? "分组详情"
  : current.value.label));
const headingHint = computed(() => (detailGroup.value
  ? `${detailGroup.value.name || detailGroup.value.id} · ${detailGroup.value.id}`
  : detailOpen.value ? "" : current.value.hint));
</script>

<template>
  <div class="flex h-full min-h-0 bg-app">
    <!-- ------------------------------ rail ------------------------------ -->
    <aside
      class="flex shrink-0 flex-col border-r border-line bg-rail transition-[width] duration-150"
      :style="{ width: collapsed ? '60px' : '186px' }"
    >
      <!-- 折叠时整块只留展开按钮：logo 与标题都收起（logo 只在展开态显示）。 -->
      <div class="flex h-[52px] items-center gap-2 px-3" :class="collapsed ? 'justify-center' : ''">
        <div v-if="!collapsed" class="flex h-7 w-7 shrink-0 items-center justify-center rounded-lg bg-brand-500 text-white shadow-sm">
          <Zap :size="16" />
        </div>
        <span v-if="!collapsed" class="min-w-0 flex-1 truncate text-[13.5px] font-semibold text-ink">AI 网关控制台</span>
        <IconBtn :title="collapsed ? '展开侧栏' : '折叠侧栏'" class="!h-7 !w-7" @click="toggleCollapse">
          <PanelLeftOpen v-if="collapsed" :size="15" />
          <PanelLeftClose v-else :size="15" />
        </IconBtn>
      </div>

      <nav class="min-h-0 flex-1 space-y-0.5 overflow-y-auto px-2 py-1.5">
        <button
          v-for="n in navItems" :key="n.key" :title="collapsed ? n.label : undefined"
          class="group flex w-full items-center gap-2.5 rounded-lg border px-2.5 py-2 text-left transition-all"
          :class="nav.page === n.key
            ? 'border-line bg-surface text-ink shadow-sm'
            : 'border-transparent text-ink-2 hover:bg-surface/70 hover:text-ink'"
          @click="goto(n.key)"
        >
          <component :is="n.icon" :size="16" class="shrink-0" :class="nav.page === n.key ? 'text-brand-500' : 'text-ink-3'" />
          <span v-if="!collapsed" class="min-w-0 flex-1 truncate text-[13px] font-medium">{{ n.label }}</span>
        </button>
      </nav>

      <!-- gateway status -->
      <div class="border-t border-line px-2 py-2.5">
        <div class="flex items-center gap-2 px-1.5" :title="state.online === false ? '管理面不可达' : '管理面在线'">
          <span
            class="h-[7px] w-[7px] shrink-0 rounded-full"
            :class="state.online === null ? 'bg-ink-3' : state.online ? 'bg-ok-500' : 'bg-err-500'"
          />
          <span v-if="!collapsed" class="truncate text-[12px]" :class="state.online === false ? 'text-err-600' : 'text-ink-3'">
            {{ state.online === null ? '检测中…' : state.online ? '网关运行中' : '网关离线' }}
          </span>
        </div>
      </div>
    </aside>

    <!-- ------------------------------ main ------------------------------ -->
    <main class="flex min-w-0 flex-1 flex-col">
      <!-- title bar -->
      <header class="flex h-[52px] shrink-0 items-center justify-between gap-3 border-b border-line bg-surface px-5">
        <div class="flex min-w-0 items-center gap-2">
          <Btn
            v-if="detailGroup && !nav.modelKey" variant="ghost" size="sm"
            data-test="group-detail-back" title="返回分组路由"
            @click="closeGroupDetail"
          >
            <ArrowLeft :size="13" />返回
          </Btn>
          <span v-if="detailGroup" class="h-4 w-px shrink-0 bg-line" />
          <h1 class="shrink-0 truncate text-[14.5px] font-semibold text-ink">{{ heading }}</h1>
          <span class="truncate text-[12px] text-ink-3">{{ headingHint }}</span>
        </div>
        <div class="flex shrink-0 items-center gap-2">
          <span
            v-if="state.dirty"
            class="hidden items-center gap-1 rounded-md border border-warn-100 bg-warn-50 px-2 py-1 text-[11.5px] font-medium text-warn-700 sm:inline-flex"
          >
            <CircleAlert :size="12" /> 有未保存的修改
          </span>
          <Btn v-if="state.dirty" variant="ghost" size="sm" :disabled="state.saving" @click="revert">放弃</Btn>
          <Btn variant="primary" size="sm" :disabled="state.saving || !state.dirty" @click="saveConfig">
            <LoaderCircle v-if="state.saving" :size="13" class="animate-spin" />
            <Save v-else :size="13" />
            保存
          </Btn>
          <IconBtn title="重新加载配置" :disabled="state.loading" @click="revert">
            <RefreshCw :size="15" :class="state.loading ? 'animate-spin' : ''" />
          </IconBtn>
        </div>
      </header>

      <!-- content -->
      <section ref="scroller" class="min-h-0 flex-1 overflow-y-auto">
        <div v-if="state.error" class="m-5 rounded-lg border border-err-100 bg-err-50 px-4 py-3 text-[13px] text-err-700">
          <div class="flex items-center gap-2 font-medium">
            <WifiOff :size="15" /> 无法连接管理面
          </div>
          <p class="mt-1 leading-5">{{ state.error }}</p>
          <p class="mt-1 text-[12px] text-err-600/80">
            默认地址 http://127.0.0.1:9090 —— 可在「设置」页修改管理面地址与密钥。
          </p>
        </div>

        <div v-else-if="!state.loaded" class="flex h-full items-center justify-center gap-2 text-[13px] text-ink-3">
          <LoaderCircle :size="16" class="animate-spin" /> 正在加载配置…
        </div>

        <Transition v-else name="page" mode="out-in" appear>
          <ModelDetail v-if="nav.modelKey" :key="viewKey" />
          <GroupDetail v-else-if="detailGroup" :key="viewKey" :group="detailGroup" />
          <div v-else :key="viewKey" class="px-5 py-5">
            <component :is="pageComp" />
          </div>
        </Transition>
      </section>
    </main>

    <Toaster />
  </div>
</template>
