<script setup lang="ts">
// 分组详情 →「模型配置」子页：这个分组怎么在渠道/模型之间按 一级 → 二级 → 三级 降级。
//
// 2026-10-13 从「编辑分组」弹窗搬过来的：层级 / 成员 / 权重 / 拖动排序单独成一页，
// 与分组详情页上的「分组设置」（名称、暴露模型、重试与超时）分开 —— 那边是分组本身的属性，
// 这边是它的路由表，改的东西不一样，混在一个弹窗里两边都不好找。
//
// 编辑的是一份草稿（进入时深拷贝 props.group），只有点「保存」才写回 state.cfg 并 markDirty()；
// 「放弃修改」把草稿重新克隆一遍。落库仍然由顶栏「保存」统一负责。
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { ArrowDown, ArrowUp, GripVertical, Plus, RefreshCw, Trash2, Waypoints } from "@lucide/vue";
import { Alert, Badge, Btn, Card, IconBtn, PageHeader, TInput, TSelect } from "../components/ui";
import { bindingLabel, channelName, markDirty, models, state } from "../store";
import { toast } from "../toast";
import { setGroupView } from "../nav";
import type { Group, Tier, TierMember } from "../api";

const props = defineProps<{ group: Group }>();

/** 同级候选的分配方式：false（默认）= 按列表顺序依次尝试；true = 按权重降序。 */
const autoWeight = computed(() => state.cfg?.auto_weight ?? false);

/* ------------------------------- 展示辅助 ------------------------------- */
// 层级名与配色：与分组路由页、分组详情页保持同一套文案（一级/二级/三级…）。
const TIER_NAMES = ["一级", "二级", "三级", "四级", "五级", "六级"];

function tierName(level: number): string {
  return TIER_NAMES[level - 1] ?? `${level} 级`;
}
function tierTone(level: number): "brand" | "gray" {
  return level === 1 ? "brand" : "gray";
}

/* ------------------------------- 编辑草稿 ------------------------------- */

function clone(g: Group): Group {
  const d = JSON.parse(JSON.stringify(g)) as Group;
  if (!d.tiers || !d.tiers.length) d.tiers = [{ level: 1, members: [] }];
  return d;
}

/**
 * 草稿：进入本页时把分组深拷贝一份，之后所有编辑都发生在这份副本上。
 * 之所以不直接改 props.group：用户改了半天层级又不想保存时，「放弃修改」必须能一键还原。
 */
const draft = ref<Group>(clone(props.group));

/** 分组被换掉（顶栏「重新加载配置」、保存成功）时重新克隆，别拿着旧草稿。 */
watch(() => props.group, (g) => { draft.value = clone(g); });

/** 层级与成员的有序签名：只比较「改了什么」，用来判断有没有未保存的改动。 */
function tierSig(g: Group): string {
  return JSON.stringify((g.tiers ?? []).map(t => (t.members ?? []).map(m => [m.model_id, Number(m.weight) || 0])));
}
const dirty = computed(() => tierSig(draft.value) !== tierSig(props.group));

function resetDraft() {
  draft.value = clone(props.group);
  toast.ok("已放弃未保存的模型配置改动");
}

/* 选模型下拉：每一行都是「渠道-模型」（bindingLabel），
   hint 放上游名 —— 两个渠道暴露同名模型时（例如都叫 gpt-4o），
   行内仍然能看出渠道与真实上游，搜索框按 label/hint/group 过滤。 */
const modelOptions = computed(() =>
  models.value.map(m => ({
    value: m.id,
    label: bindingLabel(m.id),
    hint: m.upstream,
    group: channelName(m.channel_id),
  })),
);

function newMember(modelId: string, weight = 1): TierMember {
  return { model_id: modelId, weight };
}
/** 优先挑该层还没用过的模型，避免一上来就撞重复。 */
function firstFreeModelId(tier: Tier): string {
  const used = new Set(tier.members.map(m => m.model_id));
  return models.value.find(m => !used.has(m.id))?.id ?? models.value[0]?.id ?? "";
}

/* --------------------------- 层级编辑器交互 --------------------------- */
function addMember(tier: Tier) {
  tier.members.push(newMember(firstFreeModelId(tier)));
}
function addTier() {
  const d = draft.value;
  if (!d) return;
  const t: Tier = { level: d.tiers.length + 1, members: [] };
  if (models.value.length) t.members.push(newMember(firstFreeModelId(t)));
  d.tiers.push(t);
}
function removeTier(index: number) {
  const d = draft.value;
  if (!d || d.tiers.length <= 1) return;
  d.tiers.splice(index, 1);
  d.tiers.forEach((t, i) => { t.level = i + 1; });
}
/** 同级内上移 / 下移一位：直接调换数组顺序，界面顺序即保存顺序。 */
function moveMember(tier: Tier, index: number, delta: number) {
  const to = index + delta;
  if (to < 0 || to >= tier.members.length) return;
  const [m] = tier.members.splice(index, 1);
  if (!m) return;
  tier.members.splice(to, 0, m);
}

/**
 * 拖动排序：用 Pointer Events 自己实现，不引第三方拖拽库（拖拽库动辄几十 KB，
 * 而这里只需要「同级内换顺序」「层级换顺序」两件事）。关键取舍：
 *   - setPointerCapture 之后 move / up 都会回到抓手元素上，鼠标划出弹窗也不会丢事件；
 *   - 拖动期间不改数组顺序（DOM 顺序冻结），开始时量一次行几何，之后只更新「落点槽位」
 *     和跟手位移 —— 被拖行的原始位置始终是量到的那份，不会因为自己挪动而抖动；
 *   - 松手才落位：槽位 0 = 插到第 1 行之前，N = 插到最后一行之后；
 *   - 不用 HTML5 draggable：它的拖影、dragend 时机各内核差异大，触控板 / 手指体验也差。
 * 按钮保留：拖拽是鼠标增强，键盘与读屏用户仍然靠上移 / 下移。
 */
type DragKind = "member" | "tier";

const dragKind = ref<DragKind | null>(null);
/** 成员拖动：所属层；层级拖动：被拖的那一层。两种情况都用来高亮整块。 */
const dragTier = ref<Tier | null>(null);
/** 正在被拖动的成员对象本身（按身份跟踪，比 index 更稳）。 */
const dragMember = ref<TierMember | null>(null);
/** 层级拖动时被拖层的下标。 */
const dragTierFrom = ref(-1);
/** 落点槽位：0 = 插到第 1 行之前，长度 = 插到最后一行之后。 */
const dropIndex = ref(-1);
/** 按下时的鼠标 Y、被拖元素的跟手纵向位移。 */
const dragStartY = ref(0);
const dragDelta = ref(0);
/** 拖动开始时量好的几何 —— 拖动期间不改数组，DOM 顺序不变，所以量一次就够。 */
const dragRects = ref<{ top: number; height: number }[]>([]);
/** 定位容器 padding box 的顶边（落点指示线用它换算相对坐标）。 */
const dragPadTop = ref(0);
const dragLineTop = ref(0);

const dragging = computed(() => dragKind.value !== null);

/** 拖动期间给 body 挂一个类：锁住文本选中与光标形状，松手就摘掉。 */
watch(dragging, (on) => {
  document.body.classList.toggle("gw-dragging", on);
});

/** 被拖元素跟着鼠标走：位移 = 鼠标纵向增量（布局冻结，所以不需要自我修正）。 */
const dragStyle = computed(() => (
  dragging.value ? { transform: `translateY(${dragDelta.value}px)` } : undefined
));

/** 鼠标纵向落在哪个槽位：比每一行的中垂线，越过中线就算落到下一格。 */
function slotAt(clientY: number): number {
  const rects = dragRects.value;
  for (let i = 0; i < rects.length; i++) {
    const r = rects[i]!;
    if (clientY < r.top + r.height / 2) return i;
  }
  return rects.length;
}

/** 指示线位置：插在某行之前画在它上方，插到末尾画在最后一行下方。 */
function lineTopFor(slot: number, rects: { top: number; height: number }[], padTop: number): number {
  if (!rects.length) return 0;
  if (slot >= rects.length) {
    const last = rects[rects.length - 1]!;
    return Math.max(0, last.top + last.height - padTop + 3);
  }
  return Math.max(0, rects[slot]!.top - padTop - 5);
}

function endDrag() {
  dragKind.value = null;
  dragTier.value = null;
  dragMember.value = null;
  dragTierFrom.value = -1;
  dropIndex.value = -1;
  dragDelta.value = 0;
  dragRects.value = [];
}

/** 量一次几何并进入拖动状态（成员 / 层级两种模式共用）。 */
function startDrag(kind: DragKind, box: HTMLElement, rowSel: string, clientY: number) {
  const padTop = box.getBoundingClientRect().top + box.clientTop;
  const rects = Array.from(box.querySelectorAll<HTMLElement>(rowSel))
    .map((el) => { const r = el.getBoundingClientRect(); return { top: r.top, height: r.height }; });
  dragKind.value = kind;
  dragStartY.value = clientY;
  dragDelta.value = 0;
  dragRects.value = rects;
  dragPadTop.value = padTop;
  dropIndex.value = slotAt(clientY);
  dragLineTop.value = lineTopFor(dropIndex.value, rects, padTop);
}

/** 松手落位：摘下来再插进落点槽位；越过原位置时槽位要左移一位（自己已被摘掉）。 */
function commitDrag() {
  const kind = dragKind.value;
  const slot = dropIndex.value;
  const d = draft.value;
  if (kind === "member") {
    const tier = dragTier.value;
    const m = dragMember.value;
    if (tier && m && slot >= 0) {
      const from = tier.members.indexOf(m);
      if (from >= 0) {
        tier.members.splice(from, 1);
        tier.members.splice(slot > from ? slot - 1 : slot, 0, m);
      }
    }
  } else if (kind === "tier" && d && slot >= 0) {
    const from = dragTierFrom.value;
    if (from >= 0 && from < d.tiers.length) {
      const [t] = d.tiers.splice(from, 1);
      if (t) {
        d.tiers.splice(slot > from ? slot - 1 : slot, 0, t);
        // 层级号跟位置绑定：一级 → 二级 → … 永远等于从上到下的顺序
        d.tiers.forEach((x, i) => { x.level = i + 1; });
      }
    }
  }
  endDrag();
}

function onDragMove(e: PointerEvent) {
  if (!dragging.value) return;
  dragDelta.value = e.clientY - dragStartY.value;
  const slot = slotAt(e.clientY);
  if (slot !== dropIndex.value) {
    dropIndex.value = slot;
    dragLineTop.value = lineTopFor(slot, dragRects.value, dragPadTop.value);
  }
}

/** 拖动中给层容器补样式：成员拖动只要定位，层级拖动还要抬高、描边、压暗。 */
function tierDragClass(t: Tier): string {
  if (dragKind.value === "tier") {
    return dragTier.value === t ? "relative z-20 opacity-70 ring-2 ring-brand-500" : "relative";
  }
  return dragKind.value === "member" && dragTier.value === t ? "relative" : "";
}

/** 抓手：成员行内拖动（同级内换顺序）。 */
function onMemberDragStart(e: PointerEvent, tier: Tier, index: number) {
  if (e.button !== 0 || tier.members.length < 2) return; // 只接左键；只有一个成员时没什么可排的
  e.preventDefault();
  const m = tier.members[index];
  const handle = e.currentTarget as HTMLElement | null;
  const box = handle?.closest("[data-test=member-tier]") as HTMLElement | null;
  if (!m || !box) return;
  dragTier.value = tier;
  dragMember.value = m;
  startDrag("member", box, "[data-test=member-row]", e.clientY);
  // 捕获后 move / up 都会回到抓手，鼠标划出窗口也不丢事件；失败也不影响 window 上的监听
  try { handle?.setPointerCapture(e.pointerId); } catch (err) { /* 忽略 */ }
}

/** 抓手：层头拖动（层级先后，也就是 一级 → 二级 的尝试顺序）。 */
function onTierDragStart(e: PointerEvent, ti: number) {
  const d = draft.value;
  if (e.button !== 0 || !d || d.tiers.length < 2) return;
  e.preventDefault();
  const t = d.tiers[ti];
  const handle = e.currentTarget as HTMLElement | null;
  const box = handle?.closest("[data-test=tier-list]") as HTMLElement | null;
  if (!t || !box) return;
  dragTier.value = t;
  dragTierFrom.value = ti;
  startDrag("tier", box, "[data-test=member-tier]", e.clientY);
  try { handle?.setPointerCapture(e.pointerId); } catch (err) { /* 忽略 */ }
}

/** Esc 放弃本次拖动（不改顺序）。 */
function onDragKey(e: KeyboardEvent) {
  if (e.key === "Escape" && dragging.value) endDrag();
}

onMounted(() => {
  // 挂在 window 上：鼠标划出弹窗、甚至划出窗口，move / up 一样能收到
  window.addEventListener("pointermove", onDragMove);
  window.addEventListener("pointerup", commitDrag);
  window.addEventListener("pointercancel", endDrag);
  window.addEventListener("keydown", onDragKey);
});
onBeforeUnmount(() => {
  // 拖动途中切页 / 关弹窗时清干净，避免残留 transform 与光标锁定
  document.body.classList.remove("gw-dragging");
  window.removeEventListener("pointermove", onDragMove);
  window.removeEventListener("pointerup", commitDrag);
  window.removeEventListener("pointercancel", endDrag);
  window.removeEventListener("keydown", onDragKey);
});
const emptyTierError = computed(() => {
  const i = draft.value.tiers.findIndex(t => t.members.length === 0);
  return i >= 0 ? `${tierName(i + 1)}还没有成员，每一级至少需要一个模型。` : "";
});
const dupErrors = computed<string[]>(() => {
  const out: string[] = [];
  draft.value.tiers.forEach((t, i) => {
    const seen = new Set<string>();
    for (const m of t.members) {
      if (!m.model_id) {
        out.push(`${tierName(i + 1)}有成员还没有选择模型。`);
        break;
      }
      if (seen.has(m.model_id)) {
        out.push(`${tierName(i + 1)}内重复选择了「${bindingLabel(m.model_id)}」。`);
        break;
      }
      seen.add(m.model_id);
    }
  });
  return out;
});
const valid = computed(() => !emptyTierError.value && dupErrors.value.length === 0);
/** 页脚那句话：把「哪里不对」直接说出来，而不是只把按钮灰掉。 */
const blockReason = computed(() => emptyTierError.value || dupErrors.value[0] || "");

/* -------------------------------- 写回 -------------------------------- */

function save() {
  if (!valid.value) return;
  const cfg = state.cfg;
  const g = props.group;
  if (!cfg) return;
  const clean: Group = {
    id: g.id,
    name: g.name,
    exposed_models: [...(g.exposed_models ?? [])],
    tiers: draft.value.tiers.map((t, i) => ({
      level: i + 1,
      members: t.members.map(m => ({
        model_id: m.model_id,
        weight: Number(m.weight) || 0,
      })),
    })),
    // ★ overrides 必须原样带回去：本页不改组级策略配置，但保存时若丢掉这个字段，
    //   服务端会把它当成「跟随全局」（None 序列化成 "retry": null），
    //   等于把用户在详情页配好的组级配置静默抹掉。
    overrides: g.overrides ? JSON.parse(JSON.stringify(g.overrides)) : { retry: null },
  };
  const index = cfg.groups.findIndex(x => x.id === g.id);
  if (index >= 0) cfg.groups.splice(index, 1, clean);
  else cfg.groups.push(clean);
  markDirty();
  draft.value = clone(clean);
  toast.ok(`已更新「${clean.name || clean.id}」的模型配置`);
}

/* ------------------------------ 成员删除影响面 ------------------------------ */

// 删除成员本身只影响当前草稿，但被删掉的模型可能正在被这个分组的其它层使用，
// 删之前把「还有哪些层也在用它」「它挂在哪个渠道」讲清楚，避免误删。
const pendingMember = ref<{ tier: Tier; index: number; modelId: string } | null>(null);
const memberConfirming = ref(false);

function removeMemberAt(tier: Tier, index: number) {
  const m = tier.members[index];
  if (!m) return;
  pendingMember.value = { tier, index, modelId: m.model_id };
  memberConfirming.value = true;
}

/** 同一分组内其它层是否也引用了这个模型（含未保存的改动）。 */
const pendingMemberOtherTiers = computed(() => {
  const pm = pendingMember.value;
  const d = draft.value;
  if (!pm || !d) return 0;
  return d.tiers.filter(t => t !== pm.tier).filter(t => t.members.some(m => m.model_id === pm.modelId)).length;
});

/** 该模型还被哪些已保存的分组引用（用 groupsUsingModel，删完还剩几处）。 */
const pendingMemberGroups = computed(() => {
  const pm = pendingMember.value;
  return pm ? groupsUsingModel(pm.modelId).length : 0;
});

/** 该绑定所属渠道（渠道名-模型名里的前半截，缺绑定/缺渠道时退化）。 */
const pendingMemberChannel = computed(() => {
  const pm = pendingMember.value;
  if (!pm) return "—";
  const m = models.value.find(x => x.id === pm.modelId);
  return m ? channelName(m.channel_id) : "（绑定已删除）";
});

/** 确认删除成员：同样是页面内 Modal，不用 window.confirm。 */
function confirmRemoveMember() {
  const pm = pendingMember.value;
  const d = draft.value;
  if (!pm || !d) return;
  // 位置可能已经变了（比如期间删过别的层），按对象重新定位一次再删。
  const ti = d.tiers.indexOf(pm.tier);
  const tier = ti >= 0 ? d.tiers[ti] : null;
  if (tier) {
    const mi = tier.members.findIndex(m => m.model_id === pm.modelId);
    tier.members.splice(mi >= 0 ? mi : pm.index, 1);
  }
  memberConfirming.value = false;
  pendingMember.value = null;
  toast.ok(`已从${tierName(ti + 1)}移除「${bindingLabel(pm.modelId)}」`);
}
</script>

<!--
  拖动排序的全局兜底：抓手在本页，但指针会跑到 window 上。
  拖住时锁住整页文本选区与光标（否则划过文字会选中一片、光标变成 I 形），
  user-select:none 对 WebView2 里的子元素同样生效，松手立刻恢复。
-->
<style scoped>
:global(body.gw-dragging) {
  cursor: grabbing;
  user-select: none;
}
</style>

<template>
  <div class="px-5 py-5 space-y-3.5" data-test="group-models">
    <!-- ------------------------------ 页头 ------------------------------ -->
    <PageHeader
      :title="'模型配置 · ' + (group.name || group.id)"
      :subtitle="`${group.id} · 按 一级 → 二级 → 三级 顺序尝试，同级内按${autoWeight ? '权重降序' : '列表顺序'}；全部失败回到一级`"
    >
      <template #actions>
        <Btn size="sm" variant="ghost" data-test="gm-back" @click="setGroupView('detail')">返回概览</Btn>
        <Btn size="sm" variant="ghost" data-test="gm-reset" :disabled="!dirty" @click="resetDraft">
          <RefreshCw :size="13" />放弃修改
        </Btn>
        <Btn size="sm" variant="primary" data-test="gm-save" :disabled="!valid || !dirty" @click="save">保存模型配置</Btn>
      </template>
    </PageHeader>

    <Alert v-if="!models.length" tone="warn">
      还没有模型绑定，请先到「渠道与模型」页在某个渠道下添加 exposed → upstream 映射，再回来配置层级。
    </Alert>

    <Card :pad="false" class="overflow-hidden">
      <div class="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
        <Waypoints :size="15" class="text-ink-3" />
        <h2 class="text-[13.5px] font-semibold text-ink">路由层级</h2>
        <span class="ml-auto flex flex-col items-end gap-0.5 text-[11.5px] text-ink-3">
          <span data-test="tier-hint">
            <template v-if="autoWeight">按 一级 → 二级 → 三级 顺序尝试，同级内按权重降序分配</template>
            <template v-else>按 一级 → 二级 → 三级 顺序尝试，同级内按列表顺序依次尝试（自动权重已关闭）</template>
          </span>
          <span data-test="weight-hint">
            <template v-if="autoWeight">权重可直接改；并列时按列表顺序</template>
            <template v-else>权重可先填好，开启「自动权重」后生效</template>
          </span>
          <span data-test="drag-hint">按住行首或层头的抓手可拖动排序</span>
        </span>
      </div>

      <div class="px-4 py-3.5">
      <div class="relative space-y-2.5" data-test="tier-list">
        <!-- 层级拖动的落点指示线：画在层与层之间 -->
        <div
          v-if="dragKind === 'tier' && dropIndex >= 0"
          class="pointer-events-none absolute inset-x-0 z-30 h-[3px] rounded-full bg-brand-500"
          :style="{ top: dragLineTop + 'px' }"
          data-test="drop-line"
        />
        <div
          v-for="(t, ti) in draft.tiers"
          :key="ti"
          class="rounded-[10px] border border-line bg-app/60 p-3"
          :class="tierDragClass(t)"
          :style="dragKind === 'tier' && dragTier === t ? dragStyle : undefined"
          data-test="member-tier"
        >
          <!-- 成员拖动的落点指示线：画在行与行之间 -->
          <div
            v-if="dragKind === 'member' && dragTier === t && dropIndex >= 0"
            class="pointer-events-none absolute inset-x-0 z-30 h-[3px] rounded-full bg-brand-500"
            :style="{ top: dragLineTop + 'px' }"
            data-test="drop-line"
          />
          <div class="mb-2.5 flex items-center justify-between gap-2">
            <div class="flex items-center gap-2">
              <button
                type="button"
                class="flex h-7 w-4 shrink-0 cursor-grab touch-none items-center justify-center rounded-md text-ink-3 transition-colors hover:bg-app hover:text-ink active:cursor-grabbing disabled:pointer-events-none disabled:opacity-40"
                :disabled="draft.tiers.length < 2"
                data-test="tier-drag"
                :title="draft.tiers.length < 2
                  ? '只有一个层级，无需排序'
                  : '按住拖动可调整层级先后（也就是 一级 → 二级 的尝试顺序）'"
                @pointerdown="onTierDragStart($event, ti)"
              >
                <GripVertical :size="14" />
              </button>
              <Badge :tone="tierTone(ti + 1)">{{ tierName(ti + 1) }}</Badge>
              <span class="text-[11.5px] text-ink-3">{{ t.members.length }} 个成员</span>
            </div>
            <div class="flex shrink-0 items-center gap-0.5">
              <Btn size="sm" variant="ghost" :disabled="!models.length" @click="addMember(t)">
                <Plus :size="12" />添加成员
              </Btn>
              <IconBtn
                title="删除该级"
                danger
                :disabled="draft.tiers.length <= 1"
                @click="removeTier(ti)"
              >
                <Trash2 :size="15" />
              </IconBtn>
            </div>
          </div>

          <div v-if="t.members.length" class="space-y-2">
            <div
              v-for="(m, mi) in t.members"
              :key="mi"
              class="flex items-center gap-2 rounded-[10px] border border-line bg-surface px-2.5 py-1.5"
              :class="dragMember === m
                ? 'relative z-20 border-brand-200 bg-brand-50 shadow-lg'
                : 'relative'"
              :style="dragMember === m ? dragStyle : undefined"
              data-test="member-row"
              :data-model="m.model_id"
            >
              <button
                type="button"
                class="flex h-7 w-4 shrink-0 cursor-grab touch-none items-center justify-center rounded-md text-ink-3 transition-colors hover:bg-app hover:text-ink active:cursor-grabbing disabled:pointer-events-none disabled:opacity-40"
                :disabled="t.members.length < 2"
                data-test="member-drag"
                :title="t.members.length < 2
                  ? '同级只有一个成员，无需排序'
                  : '按住拖动可调整同级顺序'"
                @pointerdown="onMemberDragStart($event, t, mi)"
              >
                <GripVertical :size="14" />
              </button>
              <div class="min-w-0 flex-1" data-test="member-row-select">
                <TSelect
                  v-model="m.model_id"
                  size="sm"
                  searchable
                  :options="modelOptions"
                  :placeholder="models.length ? '搜索并选择渠道-模型' : '请先到「渠道与模型」页添加绑定'"
                />
                <!-- 同名模型跨渠道时最容易被误选，这里就地提示 -->
                <p
                  v-if="m.model_id && t.members.filter(x => x.model_id === m.model_id).length > 1"
                  class="mt-1 text-[11px] leading-4 text-warn-700"
                >
                  该模型在本层被选了多次，请换一个。
                </p>
              </div>
              <!-- 顺序号：自动权重关闭时它就是真实尝试次序；开启时仅作权重并列的兜底次序 -->
              <span
                v-if="!autoWeight"
                class="shrink-0 rounded-md border border-line bg-app px-1.5 py-0.5 font-mono text-[11.5px] text-ink-2"
                data-test="member-order"
                :title="`同级第 ${mi + 1} 个尝试`"
              >{{ mi + 1 }}</span>
              <!-- 权重：两种模式都可手动改；关闭时该值不参与调度，开启后立即按它降序 -->
              <span class="shrink-0 text-[11.5px] text-ink-3">权重</span>
              <div class="w-20 shrink-0" data-test="member-weight">
                <TInput
                  v-model.number="m.weight"
                  type="number"
                  min="0"
                  step="1"
                  size="sm"
                  :title="autoWeight
                    ? `同级内按权重降序尝试，权重相同按列表顺序（当前第 ${mi + 1} 位）`
                    : `自动权重已关闭：权重暂不参与调度，开启后立即按它降序；现在的次序由拖动（或上移 / 下移）决定，当前第 ${mi + 1} 位`"
                />
              </div>
              <div class="flex shrink-0 items-center gap-0.5">
                <IconBtn
                  title="上移（也可以直接拖动左侧抓手）"
                  data-test="member-up"
                  :disabled="mi === 0"
                  @click="moveMember(t, mi, -1)"
                >
                  <ArrowUp :size="14" />
                </IconBtn>
                <IconBtn
                  title="下移（也可以直接拖动左侧抓手）"
                  data-test="member-down"
                  :disabled="mi === t.members.length - 1"
                  @click="moveMember(t, mi, 1)"
                >
                  <ArrowDown :size="14" />
                </IconBtn>
              </div>
              <IconBtn title="删除成员" danger @click="removeMemberAt(t, mi)">
                <Trash2 :size="15" />
              </IconBtn>
            </div>
          </div>

          <p v-else class="py-1 text-[12px] text-ink-3">
            该级还没有成员，点右上「添加成员」。
          </p>
        </div>
      </div>

      <div class="mt-2.5">
        <Btn size="sm" variant="ghost" :disabled="!models.length" @click="addTier">
          <Plus :size="12" />添加{{ tierName(draft.tiers.length + 1) }}
        </Btn>
      </div>
      </div>
    </Card>

    <p
      v-if="blockReason" data-test="gm-block-reason"
      class="text-[11.5px] leading-4 text-err-600"
    >还不能保存：{{ blockReason }}</p>
    <p class="text-[11.5px] leading-4 text-ink-3">
      改动只进内存，要落库请点顶栏的「保存」；离开本页不会自动提交。
    </p>
  </div>
</template>
