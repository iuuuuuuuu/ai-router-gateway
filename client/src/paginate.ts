// tiny useAsync helper for pages
import { ref } from "vue";
export function useAsync<T>(fn: () => Promise<T>) {
  const data = ref<T | null>(null);
  const error = ref("");
  const loading = ref(false);
  async function run() {
    loading.value = true; error.value = "";
    try { data.value = await fn(); } catch (e: any) { error.value = e.message || String(e); }
    finally { loading.value = false; }
  }
  return { data, error, loading, run };
}
export function fmtTime(iso: string) {
  if (!iso) return "-";
  return iso.replace("T", " ").slice(0, 23);
}
export function fmtCost(v: number | null | undefined) {
  if (v === null || v === undefined) return "-";
  return "$" + v.toFixed(6);
}
