// Page-level navigation: which rail page is active, and whether a group detail
// page is open on top of it. Kept out of store.ts because that module owns the
// config document, not where the user is looking.
import { reactive } from "vue";

export type PageKey = "channels" | "groups" | "settings" | "logs" | "stats";

export const nav = reactive({
  page: "channels" as PageKey,
  /** non-null = the group detail page is open for this group id */
  groupId: null as string | null,
});

/** Rail click: always leaves the detail page. */
export function goto(page: PageKey) {
  nav.page = page;
  nav.groupId = null;
}

/** Open the group detail page (keeps the rail highlight on 分组路由). */
export function openGroupDetail(id: string) {
  nav.page = "groups";
  nav.groupId = id;
}

export function closeGroupDetail() {
  nav.groupId = null;
}
