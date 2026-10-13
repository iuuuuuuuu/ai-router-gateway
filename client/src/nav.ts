// Page-level navigation: which rail page is active, and whether a group detail
// page is open on top of it. Kept out of store.ts because that module owns the
// config document, not where the user is looking.
import { reactive } from "vue";

export type PageKey = "channels" | "groups" | "settings" | "logs" | "stats";

/**
 * 分组详情的两个子视图：
 *  · "detail" —— 分组概览（设置 + 用量 + 请求/切换日志 + 冷却）
 *  · "models" —— 该分组的模型配置（层级 / 成员 / 权重 / 降级顺序）
 * 模型配置从概览里拆出来单独一页，是因为它本质上是「另一件事」：概览是看，模型配置是改，
 * 而且层级编辑器带拖动排序，塞在概览里会把看板撑得很长。
 */
export type GroupView = "detail" | "models";

export const nav = reactive({
  page: "channels" as PageKey,
  /** non-null = the group detail page is open for this group id */
  groupId: null as string | null,
  /** 分组详情当前停在哪个子视图（只在 groupId 非空时有意义） */
  groupView: "detail" as GroupView,
  /** non-null = the model detail page is open for this member key ("<channel_id>::<upstream_model>") */
  modelKey: null as string | null,
});

/** Identifies one member row (a model slot inside a group) for the model detail page. */
export type ModelRef = { groupId: string; key: string; modelId: string };

/** Rail click: always leaves the detail page. */
export function goto(page: PageKey) {
  nav.page = page;
  nav.groupId = null;
  nav.groupView = "detail";
  nav.modelKey = null;
}

/** Open the group detail page (keeps the rail highlight on 分组路由). */
export function openGroupDetail(id: string) {
  nav.page = "groups";
  nav.groupId = id;
  nav.groupView = "detail";
  nav.modelKey = null;
}

/** 打开某个分组的「模型配置」子页（从概览里的层级卡跳过来）。 */
export function openGroupModels(id: string) {
  nav.page = "groups";
  nav.groupId = id;
  nav.groupView = "models";
  nav.modelKey = null;
}

/** 分组详情内部切子视图（概览 ↔ 模型配置）。 */
export function setGroupView(v: GroupView) {
  nav.groupView = v;
  nav.modelKey = null;
}

export function closeGroupDetail() {
  nav.groupId = null;
  nav.groupView = "detail";
  nav.modelKey = null;
}

/**
 * Open the model detail page for one member of a group (rail stays on 分组路由).
 * ★ 子视图保持调用点当前的 groupView，不强行改成 "models"：从概览的成员 chip 点进来，
 *   closeModelDetail() 后应当回到概览；从「模型配置」子页点进来，则回到模型配置。
 *   （改死成 "models" 会让概览点 chip → 返回 落在模型配置子页，A65 就是这么红的。）
 */
export function openModelDetail(ref: ModelRef) {
  nav.page = "groups";
  // 换到另一个分组时子视图回到默认的「概览」，别把上一组残留的 "models" 带过来；
  // 同一分组内部点 chip（概览 / 模型配置子页都能点）则保持当前子视图。
  if (nav.groupId !== ref.groupId) nav.groupView = "detail";
  nav.groupId = ref.groupId;
  nav.modelKey = ref.key;
}

/** Close the model detail page, staying on the group it belongs to. */
export function closeModelDetail() {
  nav.modelKey = null;
}
