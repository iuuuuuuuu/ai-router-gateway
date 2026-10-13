// Page-level navigation: which rail page is active, and whether a group detail
// page is open on top of it. Kept out of store.ts because that module owns the
// config document, not where the user is looking.
import { reactive } from "vue";

export type PageKey = "channels" | "groups" | "settings" | "logs" | "stats";

export const nav = reactive({
  page: "channels" as PageKey,
  /** non-null = the group detail page is open for this group id */
  groupId: null as string | null,
  /** non-null = the model detail page is open for this member key ("<channel_id>::<upstream_model>") */
  modelKey: null as string | null,
});

/** Identifies one member row (a model slot inside a group) for the model detail page. */
export type ModelRef = { groupId: string; key: string; modelId: string };

/** Rail click: always leaves the detail page. */
export function goto(page: PageKey) {
  nav.page = page;
  nav.groupId = null;
  nav.modelKey = null;
}

/** Open the group detail page (keeps the rail highlight on 分组路由). */
export function openGroupDetail(id: string) {
  nav.page = "groups";
  nav.groupId = id;
  nav.modelKey = null;
}

export function closeGroupDetail() {
  nav.groupId = null;
  nav.modelKey = null;
}

/** Open the model detail page for one member of a group (rail stays on 分组路由). */
export function openModelDetail(ref: ModelRef) {
  nav.page = "groups";
  nav.groupId = ref.groupId;
  nav.modelKey = ref.key;
}

/** Close the model detail page, staying on the group it belongs to. */
export function closeModelDetail() {
  nav.modelKey = null;
}
