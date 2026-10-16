// 时间范围快捷选项的唯一真源。放在普通 .ts 而不是 SFC 里：<script setup> 不允许
// 具名导出，写在 SFC 里就没法被 index.ts 转出去，页面只能各自抄一份天数列表。
//
// 为什么是「天」而不是起止时间选择器：这一版要的是「默认按天筛」，且服务端只接受
// 回溯天数（days）。做成日历区间需要服务端支持任意 since/until，超出本轮范围。
export const DAY_RANGES: number[] = [1, 2, 3, 7, 14, 30];

/** 默认 1 天 —— 用户要求「默认按照天筛选」。 */
export const DEFAULT_DAYS = 1;

/** 非法的天数一律回落到默认值，绝不把 NaN 发给服务端。 */
export function normalizeDays(v: unknown): number {
  const n = Number(v);
  return DAY_RANGES.includes(n) ? n : DEFAULT_DAYS;
}
