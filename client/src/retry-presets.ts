/* ---------------------------------------------------------------------------
   重试档位（快捷预设）：全局设置页与分组详情页共用**同一份**定义。

   两处都能套用，所以定义只留一份 —— 否则两边各自漂移，同一个「快速失败」在
   两个页面上给出不同的数字，是最难查的那种不一致。

   只写数字字段，且 loop_guard_enabled 故意不包含在内：它不参与预算公式
   （max_attempts × attempt_deadline_ms + reserve_ms ≤ total_deadline_ms），
   也不该被「档位」这种纯超时/重试预设顺手改掉。
--------------------------------------------------------------------------- */

import type { RetryConfig } from "./api";

/** 档位能改的那些数字字段（开关类字段刻意不在内，见文件头）。 */
export type RetryNumbers = Pick<
  RetryConfig,
  | "max_attempts" | "max_cycles" | "total_deadline_ms" | "attempt_deadline_ms"
  | "connect_timeout_ms" | "ttfb_timeout_ms" | "first_event_timeout_ms"
  | "chunk_idle_timeout_ms" | "reserve_ms" | "cooldown_wait"
  | "loop_guard_min_chars" | "loop_guard_repeat_min_chars" | "loop_guard_repeat_ratio"
  | "loop_guard_max_repeats" | "loop_guard_window_chars" | "loop_guard_tail_chars"
  | "loop_guard_hold_max_ms"
>;

export interface RetryPreset {
  value: string;
  label: string;
  hint: string;
  patch: RetryNumbers;
}

export const RETRY_PRESETS: RetryPreset[] = [
  {
    value: "default", label: "服务端默认", hint: "6×25s",
    patch: {
      max_attempts: 6, max_cycles: 2, total_deadline_ms: 180000, attempt_deadline_ms: 25000,
      connect_timeout_ms: 5000, ttfb_timeout_ms: 15000, first_event_timeout_ms: 8000,
      chunk_idle_timeout_ms: 15000, reserve_ms: 3000, cooldown_wait: true,
      loop_guard_min_chars: 64, loop_guard_repeat_min_chars: 24, loop_guard_repeat_ratio: 0.75,
      loop_guard_max_repeats: 4, loop_guard_window_chars: 4000, loop_guard_tail_chars: 128,
      loop_guard_hold_max_ms: 250,
    },
  },
  {
    value: "fast", label: "快速失败", hint: "4×12s",
    patch: {
      max_attempts: 4, max_cycles: 1, total_deadline_ms: 60000, attempt_deadline_ms: 12000,
      connect_timeout_ms: 5000, ttfb_timeout_ms: 8000, first_event_timeout_ms: 8000,
      chunk_idle_timeout_ms: 10000, reserve_ms: 5000, cooldown_wait: false,
      loop_guard_min_chars: 64, loop_guard_repeat_min_chars: 24, loop_guard_repeat_ratio: 0.75,
      loop_guard_max_repeats: 4, loop_guard_window_chars: 4000, loop_guard_tail_chars: 128,
      loop_guard_hold_max_ms: 250,
    },
  },
  {
    value: "stream", label: "长流式输出", hint: "6×120s",
    patch: {
      max_attempts: 6, max_cycles: 2, total_deadline_ms: 900000, attempt_deadline_ms: 120000,
      connect_timeout_ms: 10000, ttfb_timeout_ms: 60000, first_event_timeout_ms: 60000,
      chunk_idle_timeout_ms: 120000, reserve_ms: 10000, cooldown_wait: true,
      loop_guard_min_chars: 64, loop_guard_repeat_min_chars: 24, loop_guard_repeat_ratio: 0.75,
      loop_guard_max_repeats: 4, loop_guard_window_chars: 4000, loop_guard_tail_chars: 128,
      loop_guard_hold_max_ms: 250,
    },
  },
  {
    value: "resilient", label: "多级降级", hint: "16×45s",
    patch: {
      max_attempts: 16, max_cycles: 3, total_deadline_ms: 900000, attempt_deadline_ms: 45000,
      connect_timeout_ms: 10000, ttfb_timeout_ms: 30000, first_event_timeout_ms: 10000,
      chunk_idle_timeout_ms: 20000, reserve_ms: 10000, cooldown_wait: true,
      loop_guard_min_chars: 64, loop_guard_repeat_min_chars: 24, loop_guard_repeat_ratio: 0.75,
      loop_guard_max_repeats: 4, loop_guard_window_chars: 4000, loop_guard_tail_chars: 128,
      loop_guard_hold_max_ms: 250,
    },
  },
];

/**
 * 当前值精确匹配某个档位时返回它的 value，否则是 custom。
 * 传 null（= 跟随全局、没有自己的配置）时也算 custom：没有可匹配的独立配置。
 */
export function matchRetryPreset(r: RetryConfig | null | undefined): string {
  if (!r) return "custom";
  const hit = RETRY_PRESETS.find(p => Object.entries(p.patch).every(([k, v]) => (r as any)[k] === v));
  return hit?.value ?? "custom";
}

/** 下拉选项：各档位 + 末尾一个禁用的「自定义」占位（它只是状态，不能主动选）。 */
export function retryPresetOptions(): { value: string; label: string; hint?: string; disabled?: boolean }[] {
  return [
    ...RETRY_PRESETS.map(p => ({ value: p.value, label: p.label, hint: p.hint })),
    { value: "custom", label: "自定义（已微调）", disabled: true },
  ];
}
