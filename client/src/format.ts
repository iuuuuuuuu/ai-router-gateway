/** 请求日志里派生指标的共享计算，Logs.vue 与 GroupDetail.vue 共用，避免两处各写一份。 */

export function num(v: unknown): number | null {
  if (v === null || v === undefined || v === "") return null;
  const n = typeof v === "number" ? v : Number(v);
  return Number.isFinite(n) ? n : null;
}

/** 生成窗口短于这个毫秒数时，"总耗时 - 首字" 只剩抖动，用它算速度会得到天文数字。 */
const MIN_DECODE_WINDOW_MS = 200;

/**
 * 生成速度（tokens/s）。
 *
 * 正常口径：输出 tokens ÷ (总耗时 − 首字)，即第一帧之后每秒吐多少 token。
 * 退化口径：上游一次性把整段回复吐完时（duration 与 ttft 几乎相等），上面的分母只剩
 * 几毫秒，速度会算出几万 —— 那不是测出来的速度，是除零噪声。这种情况退化成端到端
 * 吞吐 输出 tokens ÷ 总耗时，宁可低估也不虚报。
 *
 * 拿不到有效输出量时返回 null，由调用方显示 "—"。
 */
export function tpsOf(durationMs: unknown, ttftMs: unknown, outputTokens: unknown): number | null {
  const dur = num(durationMs);
  const out = num(outputTokens);
  if (dur === null || dur <= 0 || out === null || out <= 0) return null;
  const ttft = num(ttftMs) ?? 0;
  const decodeWindow = dur - ttft;
  const window = decodeWindow >= MIN_DECODE_WINDOW_MS ? decodeWindow : dur;
  if (window <= 0) return null;
  const tps = out / (window / 1000);
  return Number.isFinite(tps) ? tps : null;
}

/** null -> "—"；>=100 时省掉小数，避免表格里数字太长。 */
export function fmtTps(v: number | null): string {
  if (v === null) return "—";
  return v >= 100 ? String(Math.round(v)) : v.toFixed(1);
}

/**
 * 时长按中文「秒」展示。
 *
 * 服务端契约一律是毫秒（retry.*_ms / duration_ms / ttft_ms ...），这里只做展示换算，
 * 不改任何字段名与单位。分档是为了让表格里既能看清毫秒级的差异、又不会被长小数淹没：
 *   < 1 秒   → 2 位小数（0.03秒 / 0.8秒）
 *   < 10 秒  → 1 位小数（1.4秒）；刚好整秒则不带小数（3秒）
 *   < 60 秒  → 整数（12秒）
 *   < 3600 秒→ m分n秒（整分只显 4分）
 *   >= 3600  → h小时n分（整小时只显 2小时）
 * 拿不到数值时返回 "—"，与表格里其它空值一致。
 */
export function fmtSec(v: unknown): string {
  const ms = num(v);
  if (ms === null) return "—";
  if (ms < 0) return "—";
  const sec = ms / 1000;
  if (sec < 1) return sec.toFixed(2) + "秒";
  if (sec < 10) return (Number.isInteger(sec) ? String(sec) : sec.toFixed(1)) + "秒";
  if (sec < 60) return Math.round(sec) + "秒";
  if (sec < 3600) {
    const m = Math.floor(sec / 60);
    const s = Math.round(sec - m * 60);
    if (s === 0 || s === 60) return (s === 60 ? m + 1 : m) + "分";
    return m + "分" + s + "秒";
  }
  const h = Math.floor(sec / 3600);
  const m = Math.round((sec - h * 3600) / 60);
  if (m === 0 || m === 60) return (m === 60 ? h + 1 : h) + "小时";
  return h + "小时" + m + "分";
}
