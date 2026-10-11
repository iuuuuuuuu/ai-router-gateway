// Minimal toast bus. The shell renders <Toaster /> which subscribes to this.
import { reactive } from "vue";

export type ToastTone = "ok" | "err" | "warn" | "info";
export interface ToastItem { id: number; tone: ToastTone; text: string }

let seq = 1;
export const toasts = reactive<ToastItem[]>([]);

function push(tone: ToastTone, text: string, ttl = 3200) {
  const id = seq++;
  toasts.push({ id, tone, text });
  window.setTimeout(() => {
    const i = toasts.findIndex(t => t.id === id);
    if (i >= 0) toasts.splice(i, 1);
  }, ttl);
}

export const toast = {
  ok: (t: string) => push("ok", t),
  err: (t: string) => push("err", t, 6000),
  warn: (t: string) => push("warn", t, 4500),
  info: (t: string) => push("info", t),
  dismiss: (id: number) => {
    const i = toasts.findIndex(t => t.id === id);
    if (i >= 0) toasts.splice(i, 1);
  },
};
