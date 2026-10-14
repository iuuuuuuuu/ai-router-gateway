<script setup lang="ts">
// 设置：重试与超时 / 死循环拦截 / 数据面监听 / 管理面 / 开机自启。
// 分组自己的策略配置不在这里 —— 它跟着分组走，在「分组路由」页点进分组详情，在「分组设置」卡里改。
// 本页没有独立保存按钮，所有改动只写 state.cfg 并 markDirty()，由顶栏统一保存。
import { computed, onMounted, ref } from "vue";
import { Check, CircleAlert, Database, Eye, EyeOff, LoaderCircle, Power, Repeat, Rocket, Server, Timer, Trash2 } from "@lucide/vue";
import {
  Alert, Btn, Card, Field, IconBtn, Modal, SecInput, TInput, TSelect, TToggle,
} from "../components/ui";
import { api, getBase, getKey, setBase, setKey, type RetryConfig } from "../api";
import { markDirty, state } from "../store";
import { fmtSec } from "../format";
import { toast } from "../toast";

const cfg = computed(() => state.cfg!);
const retry = computed(() => cfg.value.retry);
const listen = computed(() => cfg.value.listen);
const admin = computed(() => cfg.value.admin);

/* --------------------------- 重试与超时 --------------------------- */

/** 预算公式：max_attempts × attempt_deadline_ms + reserve_ms ≤ total_deadline_ms（算的是毫秒，展示时换成秒）。 */
const worstMs = computed(
  () =>
    (Number(retry.value.max_attempts) || 0) * (Number(retry.value.attempt_deadline_ms) || 0) +
    (Number(retry.value.reserve_ms) || 0),
);
const budgetOk = computed(() => worstMs.value <= (Number(retry.value.total_deadline_ms) || 0));
const budgetFormula = computed(
  () =>
    `${retry.value.max_attempts} × ${fmtSec(retry.value.attempt_deadline_ms)} + ${fmtSec(retry.value.reserve_ms)}`,
);
const budgetLine = computed(
  () => `${budgetFormula.value} = ${fmtSec(worstMs.value)}`,
);

/** 死循环拦截的首字节推迟量 ≈ 检测阈值；老配置还没这个字段时按服务端默认 64 显示。 */
const loopHold = computed(() => Number(retry.value.loop_guard_min_chars) || 64);

/* --------------------------- 重试档位（快捷预设） --------------------------- */
// 9 个数字字段互相约束（预算公式一旦不成立服务端会拒保存），手改很容易打架，
// 所以给几个常用档位一键套用；套用后仍可逐项微调，只要有一项对不上就显示「自定义」。
// 只写这些数字/开关字段，不碰监听与管理面配置；同样只 markDirty()，由顶栏统一保存。
type RetryNumbers = Pick<
  RetryConfig,
  | "max_attempts" | "max_cycles" | "total_deadline_ms" | "attempt_deadline_ms"
  | "connect_timeout_ms" | "ttfb_timeout_ms" | "first_event_timeout_ms"
  | "chunk_idle_timeout_ms" | "reserve_ms" | "cooldown_wait"
  // 死循环拦截的 5 个数字字段：档位一并套用（开关 loop_guard_enabled 故意不在这里，
  // 它不参与预算公式，也不该被「档位」这种纯超时/重试预设顺手改掉）。
  | "loop_guard_min_chars" | "loop_guard_repeat_min_chars" | "loop_guard_repeat_ratio"
  | "loop_guard_max_repeats" | "loop_guard_window_chars"
>;

const RETRY_PRESETS: { value: string; label: string; hint: string; patch: RetryNumbers }[] = [
  {
    value: "default", label: "服务端默认", hint: "6×25s",
    patch: {
      max_attempts: 6, max_cycles: 2, total_deadline_ms: 180000, attempt_deadline_ms: 25000,
      connect_timeout_ms: 5000, ttfb_timeout_ms: 15000, first_event_timeout_ms: 8000,
      chunk_idle_timeout_ms: 15000, reserve_ms: 3000, cooldown_wait: true,
      loop_guard_min_chars: 64, loop_guard_repeat_min_chars: 24, loop_guard_repeat_ratio: 0.75,
      loop_guard_max_repeats: 4, loop_guard_window_chars: 4000,
    },
  },
  {
    value: "fast", label: "快速失败", hint: "4×12s",
    patch: {
      max_attempts: 4, max_cycles: 1, total_deadline_ms: 60000, attempt_deadline_ms: 12000,
      connect_timeout_ms: 5000, ttfb_timeout_ms: 8000, first_event_timeout_ms: 8000,
      chunk_idle_timeout_ms: 10000, reserve_ms: 5000, cooldown_wait: false,
      loop_guard_min_chars: 64, loop_guard_repeat_min_chars: 24, loop_guard_repeat_ratio: 0.75,
      loop_guard_max_repeats: 4, loop_guard_window_chars: 4000,
    },
  },
  {
    value: "stream", label: "长流式输出", hint: "6×120s",
    patch: {
      max_attempts: 6, max_cycles: 2, total_deadline_ms: 900000, attempt_deadline_ms: 120000,
      connect_timeout_ms: 10000, ttfb_timeout_ms: 60000, first_event_timeout_ms: 60000,
      chunk_idle_timeout_ms: 120000, reserve_ms: 10000, cooldown_wait: true,
      loop_guard_min_chars: 64, loop_guard_repeat_min_chars: 24, loop_guard_repeat_ratio: 0.75,
      loop_guard_max_repeats: 4, loop_guard_window_chars: 4000,
    },
  },
  {
    value: "resilient", label: "多级降级", hint: "16×45s",
    patch: {
      max_attempts: 16, max_cycles: 3, total_deadline_ms: 900000, attempt_deadline_ms: 45000,
      connect_timeout_ms: 10000, ttfb_timeout_ms: 30000, first_event_timeout_ms: 10000,
      chunk_idle_timeout_ms: 20000, reserve_ms: 10000, cooldown_wait: true,
      loop_guard_min_chars: 64, loop_guard_repeat_min_chars: 24, loop_guard_repeat_ratio: 0.75,
      loop_guard_max_repeats: 4, loop_guard_window_chars: 4000,
    },
  },
];

/** 当前值精确匹配某个档位时返回它的 value，否则是 custom。 */
const activePreset = computed(() => {
  const r = retry.value;
  const hit = RETRY_PRESETS.find(p => Object.entries(p.patch).every(([k, v]) => (r as any)[k] === v));
  return hit?.value ?? "custom";
});

const presetOptions = computed(() => [
  ...RETRY_PRESETS.map(p => ({ value: p.value, label: p.label, hint: p.hint })),
  { value: "custom", label: "自定义（已微调）", disabled: true },
]);

function applyPreset(v: string) {
  const p = RETRY_PRESETS.find(x => x.value === v);
  if (!p) return; // custom 只是状态，不写值
  Object.assign(retry.value, p.patch);
  markDirty();
  toast.ok(`已套用「${p.label}」档位，记得在顶栏保存`);
}
const presetChoice = computed({
  get: () => activePreset.value,
  set: (v: string) => applyPreset(v),
});

/* --------------------------- 管理面密钥显示 --------------------------- */

const showAdminKey = ref(false);
const showAdminConnKey = ref(false);

/* --------------------------- 管理面连接（本机） --------------------------- */

const baseDraft = ref(getBase());
const keyDraft = ref(getKey());

function applyBase() {
  const v = baseDraft.value.trim();
  setBase(v);
  toast.ok("管理面地址已更新，正在重新加载…");
  window.setTimeout(() => window.location.reload(), 300);
}
function applyKey() {
  setKey(keyDraft.value.trim());
  toast.ok("管理面密钥已更新");
}

/* --------------------------- 清空日志与统计 --------------------------- */
// 只删日志 / 冷却记录，绝不碰渠道、模型、分组、密钥这些配置。
// 前端自己发的三个勾选：至少选一项，否则按钮直接不可点。
const purgeOpen = ref(false);
const purgeBusy = ref(false);
const purgeRequestLogs = ref(true);
const purgeSwitchLogs = ref(true);
const purgeCooldowns = ref(true);
const purgeResult = ref<{ request_logs: number; switch_logs: number; cooldowns: number } | null>(null);
const purgeError = ref("");

const purgeAny = computed(() => purgeRequestLogs.value || purgeSwitchLogs.value || purgeCooldowns.value);
const purgeTotal = computed(
  () => (purgeResult.value?.request_logs ?? 0) + (purgeResult.value?.switch_logs ?? 0) + (purgeResult.value?.cooldowns ?? 0),
);

function openPurge() {
  purgeResult.value = null;
  purgeError.value = "";
  purgeOpen.value = true;
}
function closePurge() {
  if (purgeBusy.value) return;
  purgeOpen.value = false;
}
function selectAllPurge() {
  purgeRequestLogs.value = true;
  purgeSwitchLogs.value = true;
  purgeCooldowns.value = true;
}

async function confirmPurge() {
  if (!purgeAny.value || purgeBusy.value) return;
  purgeBusy.value = true;
  purgeError.value = "";
  try {
    const r = await api.purgeLogs({
      request_logs: purgeRequestLogs.value,
      switch_logs: purgeSwitchLogs.value,
      cooldowns: purgeCooldowns.value,
    });
    purgeResult.value = r?.deleted ?? null;
    const d = purgeResult.value;
    toast.ok(
      d
        ? `已清空：请求日志 ${d.request_logs} 条 · 切换日志 ${d.switch_logs} 条 · 冷却记录 ${d.cooldowns} 条`
        : "清空完成",
    );
  } catch (e: any) {
    purgeError.value = String(e?.message || e);
    toast.err("清空失败：" + purgeError.value);
  } finally {
    purgeBusy.value = false;
  }
}

/* ------------------------------ 开机自启 ------------------------------ */
// 只影响桌面客户端本身，不写入服务端配置。
const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
const autostart = ref(false);
const autostartBusy = ref(false);

async function loadAutostart() {
  if (!inTauri) return;
  try {
    const mod = await import("@tauri-apps/plugin-autostart");
    autostart.value = await mod.isEnabled();
  } catch (e: any) {
    toast.err("读取开机自启状态失败：" + String(e?.message || e));
  }
}

async function toggleAutostart() {
  if (!inTauri || autostartBusy.value) return;
  const before = autostart.value;
  const next = !before;
  autostartBusy.value = true;
  try {
    const mod = await import("@tauri-apps/plugin-autostart");
    if (next) await mod.enable();
    else await mod.disable();
    autostart.value = next;
    toast.ok(next ? "已开启随系统启动" : "已关闭随系统启动");
  } catch (e: any) {
    autostart.value = before;
    toast.err("设置开机自启失败：" + String(e?.message || e));
  } finally {
    autostartBusy.value = false;
  }
}

onMounted(() => {
  // 老配置可能没有 reserve_ms，补一个默认值，避免预算公式算不出来。
  const r = retry.value;
  if (r && typeof r.reserve_ms !== "number") r.reserve_ms = 3000;
  // 死循环拦截是后加的能力：服务端会回填默认值，但客户端直连老服务端时这些键可能是
  // undefined（数字框会显示空白），这里补上服务端默认值（与 reserve_ms 一样不标脏）。
  if (r) {
    if (typeof r.loop_guard_enabled !== "boolean") r.loop_guard_enabled = true;
    if (typeof r.loop_guard_min_chars !== "number") r.loop_guard_min_chars = 64;
    if (typeof r.loop_guard_repeat_min_chars !== "number") r.loop_guard_repeat_min_chars = 24;
    if (typeof r.loop_guard_repeat_ratio !== "number") r.loop_guard_repeat_ratio = 0.75;
    if (typeof r.loop_guard_max_repeats !== "number") r.loop_guard_max_repeats = 4;
    if (typeof r.loop_guard_window_chars !== "number") r.loop_guard_window_chars = 4000;
  }
  void loadAutostart();
});
</script>

<template>
  <div>
    <!-- 工具条：左侧说明，右侧当前管理面地址 -->
    <div class="mb-3 flex flex-wrap items-center gap-2">
      <div class="text-[12px] text-ink-3">
        本页改动只标记为未保存，由顶栏「保存」统一提交
      </div>
      <div class="ml-auto flex flex-wrap items-center gap-2">
        <span class="text-[12px] text-ink-3">当前管理面</span>
        <span class="font-mono text-[11.5px] text-ink-2">{{ getBase() }}</span>
      </div>
    </div>

    <div class="space-y-4">
      <!-- ------------------------------ 重试与超时 ------------------------------ -->
      <Card>
        <div class="mb-3.5 flex flex-wrap items-start justify-between gap-x-4 gap-y-2">
          <div class="min-w-0">
            <h2 class="text-[13.5px] font-semibold text-ink">重试与超时（全局默认）</h2>
            <p class="mt-0.5 text-[12.5px] text-ink-3">
              控制一次请求在层级之间降级重试的次数与各阶段超时时间，时长一律按「秒」填写与显示（内部仍按毫秒存储）。这里是全局默认值，所有分组默认跟随它；某个分组想要不一样的，去「分组路由」页点它的「分组详情」，在「分组设置」卡里的「重试与超时」区单独设置。
            </p>
          </div>
          <!-- 档位快捷套用：省得 9 个数字字段一个个手改 -->
          <div class="flex shrink-0 items-center gap-2">
            <span class="text-[12px] text-ink-3">档位</span>
            <div class="w-[188px]" data-test="retry-preset">
              <TSelect
                v-model="presetChoice"
                size="sm"
                searchable
                placeholder="选择档位"
                :options="presetOptions"
                empty-text="无档位"
              />
            </div>
          </div>
        </div>

        <div class="grid grid-cols-1 gap-x-5 gap-y-4 sm:grid-cols-2 lg:grid-cols-3">
          <Field label="最大尝试次数" hint="单个请求最多尝试多少次（含降级后的重试）">
            <TInput v-model.number="retry.max_attempts" type="number" @input="markDirty()" />
          </Field>
          <Field label="最大循环轮数" hint="走完全部层级算 1 轮，最多循环几轮">
            <TInput v-model.number="retry.max_cycles" type="number" @input="markDirty()" />
          </Field>
          <Field label="总预算 (秒)" hint="整个请求的总时间上限，超过即直接失败">
            <SecInput v-model="retry.total_deadline_ms" data-test="retry-total-deadline-ms" @input="markDirty()" />
          </Field>
          <Field label="单次尝试上限 (秒)" hint="一次尝试的最长耗时，超时即判定失败并降级">
            <SecInput v-model="retry.attempt_deadline_ms" data-test="retry-attempt-deadline-ms" @input="markDirty()" />
          </Field>
          <Field label="连接超时 (秒)" hint="与上游建立 TCP / TLS 连接的超时">
            <SecInput v-model="retry.connect_timeout_ms" data-test="retry-connect-timeout-ms" @input="markDirty()" />
          </Field>
          <Field label="首字节超时 (秒)" hint="发出请求到收到响应首字节的超时">
            <SecInput v-model="retry.ttfb_timeout_ms" data-test="retry-ttfb-timeout-ms" @input="markDirty()" />
          </Field>
          <Field label="首个事件超时 (秒)" hint="流式响应中第一个事件到达的超时">
            <SecInput v-model="retry.first_event_timeout_ms" data-test="retry-first-event-timeout-ms" @input="markDirty()" />
          </Field>
          <Field label="流空闲超时 (秒)" hint="流式响应里两个数据块之间的最大空闲时间">
            <SecInput v-model="retry.chunk_idle_timeout_ms" data-test="retry-chunk-idle-timeout-ms" @input="markDirty()" />
          </Field>
          <Field label="预留余量 (秒)" hint="给收尾/补帧留的时间">
            <SecInput v-model="retry.reserve_ms" data-test="retry-reserve-ms" @input="markDirty()" />
          </Field>
        </div>

        <div class="mt-4 border-t border-line-soft pt-4">
          <TToggle
            v-model="cfg.auto_weight"
            label="自动权重"
            data-test="auto-weight"
            @update:model-value="markDirty()"
          />
          <p class="mt-1 text-[11.5px] leading-4 text-ink-3">
            关闭时同级按列表顺序依次尝试；开启后同级按权重降序。
          </p>
          <p class="mt-1 text-[11.5px] leading-4 text-ink-3">
            顺序在「分组路由」页调整：按住成员行首的抓手拖动，或用上移 / 下移按钮；权重也一直在那儿可改，关闭时先填好、开启后立即生效。
          </p>
        </div>

        <div class="mt-4 border-t border-line-soft pt-4">
          <TToggle
            v-model="retry.cooldown_wait"
            label="全部冷却时等待而非直接失败"
            @update:model-value="markDirty()"
          />
          <p class="mt-1 text-[11.5px] leading-4 text-ink-3">
            开启后，若所有候选渠道都在冷却中，请求会等待最早解除冷却的渠道，而不是立刻返回失败。
          </p>
        </div>

        <div class="mt-4 border-t border-line-soft pt-4">
          <div class="mb-1.5 text-[11.5px] font-medium uppercase tracking-wide text-ink-3">
            预算校验
          </div>
          <Alert :tone="budgetOk ? 'ok' : 'err'">
            <div>
              <p class="font-medium">
                {{ budgetOk ? "预算校验通过" : "预算不足" }}
              </p>
              <p class="mt-0.5 font-mono text-[12px]">
                {{ budgetLine }}
                {{ budgetOk ? "≤" : ">" }} {{ fmtSec(retry.total_deadline_ms) }}
              </p>
              <p class="mt-0.5 text-[11.5px] leading-4">
                <template v-if="budgetOk">
                  公式：最大尝试次数 × 单次尝试上限 + 预留余量 ≤ 总预算，当前配置可用。
                </template>
                <template v-else>
                  服务端会拒绝保存，请调大总预算或调小单次上限。
                </template>
              </p>
              <p class="mt-0.5 text-[11.5px] leading-4 text-ink-3">
                当前档位：{{ activePreset === "custom" ? "自定义" : RETRY_PRESETS.find(p => p.value === activePreset)?.label }}
              </p>
            </div>
          </Alert>
        </div>
      </Card>

      <!-- ------------------------------ 死循环拦截 ------------------------------ -->
      <!-- 服务端在「提交前窗口」里检测退化输出：命中就整次尝试丢弃并换下一个候选，
           客户端只会拿到一次正常回答（提交前零字节，外部对话不会被中断）。 -->
      <Card>
        <div class="mb-3.5">
          <h2 class="flex items-center gap-1.5 text-[13.5px] font-semibold text-ink">
            <Repeat :size="14" class="text-ink-3" />
            死循环拦截
          </h2>
          <p class="mt-0.5 text-[12.5px] text-ink-3">
            检测模型是否在反复输出同一段内容；命中就丢弃这次尝试并自动换下一个候选，客户端只会拿到一次正常回答。
          </p>
        </div>

        <div class="border-t border-line-soft pt-4">
          <TToggle
            v-model="retry.loop_guard_enabled"
            label="开启死循环拦截"
            data-test="loop-guard-enabled"
            @update:model-value="markDirty()"
          />
          <p class="mt-1 text-[11.5px] leading-4 text-ink-3">
            关闭后完全按原样透传上游输出，不做任何检测与缓冲。
          </p>
        </div>

        <div class="mt-4 border-t border-line-soft pt-4">
          <div class="mb-1.5 text-[11.5px] font-medium uppercase tracking-wide text-ink-3">检测参数</div>
          <div class="grid grid-cols-1 gap-x-5 gap-y-4 sm:grid-cols-2 lg:grid-cols-3">
            <Field label="检测起点 (字符)" hint="提交前先攒够这么多字符再开始判断：太小容易误判，太大则首字节推迟更久">
              <div data-test="loop-guard-min-chars">
                <TInput
                  v-model.number="retry.loop_guard_min_chars" type="number" min="1"
                  data-test="loop-guard-min-chars" @input="markDirty()"
                />
              </div>
            </Field>
            <Field label="重复单元下限 (字符)" hint="被重复的那一段至少多长才算「无意义重复」，太短会把正常的中文/标点重复算进去">
              <div data-test="loop-guard-repeat-min-chars">
                <TInput
                  v-model.number="retry.loop_guard_repeat_min_chars" type="number" min="1"
                  data-test="loop-guard-repeat-min-chars" @input="markDirty()"
                />
              </div>
            </Field>
            <Field label="重复占比阈值" hint="0~1 的小数，例如 0.75 表示重复部分要占到检测窗口的 75% 才判定为退化">
              <div data-test="loop-guard-repeat-ratio">
                <TInput
                  v-model.number="retry.loop_guard_repeat_ratio" type="number" min="0" max="1" step="0.05"
                  data-test="loop-guard-repeat-ratio" @input="markDirty()"
                />
              </div>
            </Field>
            <Field label="同一段重复次数" hint="同一段内容重复到第几次判定为退化，越小越敏感">
              <div data-test="loop-guard-max-repeats">
                <TInput
                  v-model.number="retry.loop_guard_max_repeats" type="number" min="2"
                  data-test="loop-guard-max-repeats" @input="markDirty()"
                />
              </div>
            </Field>
            <Field label="检测窗口 (字符)" hint="最多回看多少字符，超出只保留最近的；越大越吃内存，越小越容易漏判">
              <div data-test="loop-guard-window-chars">
                <TInput
                  v-model.number="retry.loop_guard_window_chars" type="number" min="1"
                  data-test="loop-guard-window-chars" @input="markDirty()"
                />
              </div>
            </Field>
          </div>
          <p class="mt-2 text-[11.5px] leading-4 text-ink-3">
            以上 5 项只影响退化判定，<span class="font-medium text-ink-2">不参与上面的预算校验</span>（预算公式只看重试次数与超时）。
          </p>
        </div>

        <div class="mt-4 border-t border-line-soft pt-4">
          <div class="mb-1.5 text-[11.5px] font-medium uppercase tracking-wide text-ink-3">代价与边界</div>
          <ul class="space-y-1.5 text-[11.5px] leading-4 text-ink-3">
            <li>
              · <span class="font-medium text-ink-2">代价</span>：开启后首字节会被推迟约
              {{ loopHold }} 个字符（等于「检测起点」）：上游要先攒够这么多字符，网关才会向客户端写出第一个字节。
            </li>
            <li>
              · <span class="font-medium text-ink-2">只覆盖开头</span>：退化必须出现在回答的前
              {{ loopHold }} 个字符内才能被内部拦截；如果模型先正常输出很长一段、之后才开始重复，则不会内部重试（否则客户端会看到重复内容），这类情况沿用原有语义：补一个错误帧后正常收尾。
            </li>
            <li>
              · <span class="font-medium text-ink-2">只覆盖正文</span>：工具调用的参数不做检测与缓冲。
            </li>
            <li>
              · <span class="font-medium text-ink-2">误判代价</span>：一次误判只会让网关内部多试一个候选，你仍然会拿到正常回答，但会浪费一次上游调用。
            </li>
          </ul>
        </div>
      </Card>

      <!-- ------------------------------ 数据面监听 ------------------------------ -->
      <Card>
        <div class="mb-3.5">
          <h2 class="text-[13.5px] font-semibold text-ink">数据面监听</h2>
          <p class="mt-0.5 text-[12.5px] text-ink-3">
            客户端（Claude Code / OpenAI SDK 等）连接的入口地址。
          </p>
        </div>

        <div class="grid grid-cols-1 gap-x-5 gap-y-4 sm:grid-cols-2">
          <Field label="HTTP 监听" hint="host:port，例如 0.0.0.0:8080">
            <TInput
              v-model="listen.http"
              mono
              placeholder="0.0.0.0:8080"
              @input="markDirty()"
            />
          </Field>
          <Field label="HTTPS 监听" hint="host:port，需同时开启下方开关">
            <TInput
              v-model="listen.https"
              mono
              placeholder="0.0.0.0:8443"
              @input="markDirty()"
            />
          </Field>
        </div>

        <div class="mt-4">
          <TToggle
            v-model="listen.https_enabled"
            label="同时监听 HTTPS（内置自签证书）"
            @update:model-value="markDirty()"
          />
        </div>

        <Alert tone="info" class="mt-4">
          <span>监听地址在服务端启动时绑定，修改后需重启服务端生效。</span>
        </Alert>
      </Card>

      <!-- ------------------------------ 管理面 ------------------------------ -->
      <Card>
        <div class="mb-3.5">
          <h2 class="text-[13.5px] font-semibold text-ink">管理面</h2>
          <p class="mt-0.5 text-[12.5px] text-ink-3">
            控制台自身连接的管理接口，与服务端配置里的 admin 段一致。
          </p>
        </div>

        <div class="grid grid-cols-1 gap-x-5 gap-y-4 sm:grid-cols-2">
          <Field label="监听地址" hint="host:port，默认只监听本机">
            <TInput
              v-model="admin.bind"
              mono
              placeholder="127.0.0.1:9090"
              @input="markDirty()"
            />
          </Field>
          <Field label="访问密钥" hint="留空则仅允许本机访问，不校验密钥">
            <div class="flex items-center gap-1.5">
              <TInput
                v-model="admin.key"
                :type="showAdminKey ? 'text' : 'password'"
                mono
                placeholder="留空表示不校验"
                @input="markDirty()"
              />
              <IconBtn
                :title="showAdminKey ? '隐藏密钥' : '显示密钥'"
                @click="showAdminKey = !showAdminKey"
              >
                <EyeOff v-if="showAdminKey" :size="15" />
                <Eye v-else :size="15" />
              </IconBtn>
            </div>
          </Field>
        </div>

        <!-- 本机连接设置：不写入服务端配置 -->
        <div class="mt-4 rounded-lg border border-line bg-app/60 p-3">
          <div class="mb-2 flex items-center gap-2">
            <Server :size="14" class="text-ink-3" />
            <span class="text-[12.5px] font-medium text-ink-2">管理面连接</span>
          </div>

          <div class="grid grid-cols-1 gap-x-5 gap-y-4 sm:grid-cols-2">
            <Field label="地址">
              <div class="flex items-center gap-1.5">
                <TInput v-model="baseDraft" mono placeholder="http://127.0.0.1:9090" />
                <Btn size="sm" variant="ghost" @click="applyBase">应用</Btn>
              </div>
            </Field>
            <Field label="密钥">
              <div class="flex items-center gap-1.5">
                <TInput
                  v-model="keyDraft"
                  :type="showAdminConnKey ? 'text' : 'password'"
                  mono
                  placeholder="留空表示不带密钥"
                />
                <IconBtn
                  :title="showAdminConnKey ? '隐藏密钥' : '显示密钥'"
                  @click="showAdminConnKey = !showAdminConnKey"
                >
                  <EyeOff v-if="showAdminConnKey" :size="15" />
                  <Eye v-else :size="15" />
                </IconBtn>
                <Btn size="sm" variant="ghost" @click="applyKey">应用</Btn>
              </div>
            </Field>
          </div>

          <p class="mt-2 text-[11.5px] leading-4 text-ink-3">
            这两个值只保存在本机 localStorage，用于本客户端访问管理面，不会写入服务端配置；修改地址后会重新加载页面。
          </p>
        </div>
      </Card>

      <!-- ------------------------------ 开机自启 ------------------------------ -->
      <Card>
        <div class="mb-3.5">
          <h2 class="text-[13.5px] font-semibold text-ink">开机自启</h2>
          <p class="mt-0.5 text-[12.5px] text-ink-3">
            让桌面客户端随系统启动，方便网关长期驻留；不影响服务端自身的启动方式。
          </p>
        </div>

        <div>
          <TToggle
            :model-value="autostart"
            label="随系统启动"
            :disabled="!inTauri || autostartBusy"
            @update:model-value="toggleAutostart"
          />
          <p class="mt-1 text-[11.5px] leading-4 text-ink-3">
            仅桌面客户端可用；默认关闭，需手动开启。
          </p>
        </div>

        <Alert v-if="!inTauri" tone="info" class="mt-4">
          <span class="inline-flex items-center gap-1.5">
            <Power :size="13" />仅在桌面客户端中可用。
          </span>
        </Alert>
        <Alert v-else-if="autostart" tone="ok" class="mt-4">
          <span class="inline-flex items-center gap-1.5">
            <Rocket :size="13" />已开启：下次登录系统时会自动启动本客户端。
          </span>
        </Alert>
        <Alert v-else tone="info" class="mt-4">
          <span class="inline-flex items-center gap-1.5">
            <Timer :size="13" />当前未开启，需要时手动打开上面的开关即可。
          </span>
        </Alert>
      </Card>

      <!-- ------------------------------ 清空日志与统计 ------------------------------ -->
      <Card>
        <div class="mb-3.5">
          <h2 class="text-[13.5px] font-semibold text-ink">清空日志与统计</h2>
          <p class="mt-0.5 text-[12.5px] text-ink-3">
            删除历史请求日志、切换日志与冷却记录，让统计与日志回到干净起点；只动记录，不动配置。
          </p>
        </div>

        <div class="flex flex-wrap items-center gap-2">
          <Btn variant="danger" data-test="purge-logs-open" @click="openPurge()">
            <Trash2 :size="13" /> 清空日志与统计
          </Btn>
          <span class="text-[11.5px] text-ink-3">会先弹确认框，确认后不可撤销。</span>
        </div>

        <p class="mt-2 text-[11.5px] leading-4 text-ink-3">
          只删除请求日志 / 切换日志 / 冷却记录；渠道、模型、分组与密钥等配置一律保留。清空后「统计」与「日志」页重新打开即为空。
        </p>
      </Card>
    </div>

    <!-- 清空日志与统计：二次确认 + 删除结果计数 -->
    <Modal
      :open="purgeOpen"
      title="清空日志与统计"
      width="480px"
      @close="closePurge()"
    >
      <p class="text-[12.5px] leading-5 text-ink-2">
        将<span class="font-medium text-ink">永久删除</span>下面勾选的记录，<span class="font-medium text-ink">不可撤销</span>：
      </p>

      <div class="mt-3 space-y-2.5 rounded-lg border border-line bg-app/60 p-3">
        <div data-test="purge-opt-request-logs">
          <TToggle v-model="purgeRequestLogs" label="请求日志（含成本与 token 统计）" />
        </div>
        <div data-test="purge-opt-switch-logs">
          <TToggle v-model="purgeSwitchLogs" label="切换日志（渠道降级与失败原因）" />
        </div>
        <div data-test="purge-opt-cooldowns">
          <TToggle v-model="purgeCooldowns" label="冷却记录（渠道熔断状态，删除后立即解除冷却）" />
        </div>
        <div class="flex items-center gap-2 border-t border-line-soft pt-2.5">
          <Btn
            size="sm"
            variant="subtle"
            :disabled="purgeAny && purgeRequestLogs && purgeSwitchLogs && purgeCooldowns"
            @click="selectAllPurge()"
          >
            全选（全部清空）
          </Btn>
          <span class="text-[11.5px] text-ink-3">默认三项全选</span>
        </div>
      </div>

      <Alert tone="info" class="mt-3">
        <span class="inline-flex items-start gap-1.5">
          <Database :size="13" class="mt-0.5 shrink-0" />
          <span>渠道、模型、分组、密钥等<span class="font-medium text-ink">配置一律保留</span>；只清日志与冷却记录。</span>
        </span>
      </Alert>

      <Alert v-if="purgeError" tone="err" class="mt-3" data-test="purge-logs-error">
        <span class="inline-flex items-center gap-1.5">
          <CircleAlert :size="13" />清空失败：{{ purgeError }}
        </span>
      </Alert>

      <Alert v-if="purgeResult" tone="ok" class="mt-3" data-test="purge-logs-result">
        <span class="inline-flex items-start gap-1.5">
          <Check :size="13" class="mt-0.5 shrink-0" />
          <span>
            已删除请求日志 <span class="font-medium text-ink">{{ purgeResult.request_logs }}</span> 条 ·
            切换日志 <span class="font-medium text-ink">{{ purgeResult.switch_logs }}</span> 条 ·
            冷却记录 <span class="font-medium text-ink">{{ purgeResult.cooldowns }}</span> 条（共 {{ purgeTotal }} 条）。
          </span>
        </span>
      </Alert>

      <template #footer>
        <Btn data-test="purge-logs-cancel" :disabled="purgeBusy" @click="closePurge()">取消</Btn>
        <Btn
          v-if="purgeResult"
          data-test="purge-logs-done"
          variant="primary"
          @click="closePurge()"
        >
          <Check :size="13" /> 完成
        </Btn>
        <Btn
          v-else
          data-test="purge-logs-confirm"
          variant="danger"
          :disabled="!purgeAny || purgeBusy"
          @click="confirmPurge()"
        >
          <LoaderCircle v-if="purgeBusy" :size="13" class="animate-spin" />
          <Trash2 v-else :size="13" />
          确认清空
        </Btn>
      </template>
    </Modal>
  </div>
</template>
