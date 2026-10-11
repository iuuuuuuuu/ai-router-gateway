<script setup lang="ts">
// 设置：重试与超时 / 数据面监听 / 管理面 / 开机自启。
// 本页没有独立保存按钮，所有改动只写 state.cfg 并 markDirty()，由顶栏统一保存。
import { computed, onMounted, ref } from "vue";
import { Eye, EyeOff, Power, Rocket, Server, Timer } from "@lucide/vue";
import {
  Alert, Btn, Card, Field, IconBtn, TInput, TSelect, TToggle,
} from "../components/ui";
import { getBase, getKey, setBase, setKey, type RetryConfig } from "../api";
import { markDirty, state } from "../store";
import { toast } from "../toast";

const cfg = computed(() => state.cfg!);
const retry = computed(() => cfg.value.retry);
const listen = computed(() => cfg.value.listen);
const admin = computed(() => cfg.value.admin);

/* --------------------------- 重试与超时 --------------------------- */

/** 预算公式：max_attempts × attempt_deadline_ms + reserve_ms ≤ total_deadline_ms */
const worstMs = computed(
  () =>
    (Number(retry.value.max_attempts) || 0) * (Number(retry.value.attempt_deadline_ms) || 0) +
    (Number(retry.value.reserve_ms) || 0),
);
const budgetOk = computed(() => worstMs.value <= (Number(retry.value.total_deadline_ms) || 0));
const budgetFormula = computed(
  () =>
    `${retry.value.max_attempts} × ${retry.value.attempt_deadline_ms} ms + ${retry.value.reserve_ms} ms`,
);
const budgetLine = computed(
  () => `${budgetFormula.value} = ${worstMs.value} ms`,
);

/* --------------------------- 重试档位（快捷预设） --------------------------- */
// 9 个数字字段互相约束（预算公式一旦不成立服务端会拒保存），手改很容易打架，
// 所以给几个常用档位一键套用；套用后仍可逐项微调，只要有一项对不上就显示「自定义」。
// 只写这些数字/开关字段，不碰监听与管理面配置；同样只 markDirty()，由顶栏统一保存。
type RetryNumbers = Pick<
  RetryConfig,
  | "max_attempts" | "max_cycles" | "total_deadline_ms" | "attempt_deadline_ms"
  | "connect_timeout_ms" | "ttfb_timeout_ms" | "first_event_timeout_ms"
  | "chunk_idle_timeout_ms" | "reserve_ms" | "cooldown_wait"
>;

const RETRY_PRESETS: { value: string; label: string; hint: string; patch: RetryNumbers }[] = [
  {
    value: "default", label: "服务端默认", hint: "12×45s",
    patch: {
      max_attempts: 12, max_cycles: 2, total_deadline_ms: 600000, attempt_deadline_ms: 45000,
      connect_timeout_ms: 10000, ttfb_timeout_ms: 30000, first_event_timeout_ms: 10000,
      chunk_idle_timeout_ms: 20000, reserve_ms: 5000, cooldown_wait: true,
    },
  },
  {
    value: "fast", label: "快速失败", hint: "4×12s",
    patch: {
      max_attempts: 4, max_cycles: 1, total_deadline_ms: 60000, attempt_deadline_ms: 12000,
      connect_timeout_ms: 5000, ttfb_timeout_ms: 8000, first_event_timeout_ms: 8000,
      chunk_idle_timeout_ms: 10000, reserve_ms: 5000, cooldown_wait: false,
    },
  },
  {
    value: "stream", label: "长流式输出", hint: "6×120s",
    patch: {
      max_attempts: 6, max_cycles: 2, total_deadline_ms: 900000, attempt_deadline_ms: 120000,
      connect_timeout_ms: 10000, ttfb_timeout_ms: 60000, first_event_timeout_ms: 60000,
      chunk_idle_timeout_ms: 120000, reserve_ms: 10000, cooldown_wait: true,
    },
  },
  {
    value: "resilient", label: "多级降级", hint: "16×45s",
    patch: {
      max_attempts: 16, max_cycles: 3, total_deadline_ms: 900000, attempt_deadline_ms: 45000,
      connect_timeout_ms: 10000, ttfb_timeout_ms: 30000, first_event_timeout_ms: 10000,
      chunk_idle_timeout_ms: 20000, reserve_ms: 10000, cooldown_wait: true,
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
  if (r && typeof r.reserve_ms !== "number") r.reserve_ms = 5000;
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
            <h2 class="text-[13.5px] font-semibold text-ink">重试与超时</h2>
            <p class="mt-0.5 text-[12.5px] text-ink-3">
              控制一次请求在层级之间降级重试的次数与各阶段超时时间，单位均为毫秒。
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
          <Field label="总预算 (ms)" hint="整个请求的总时间上限，超过即直接失败">
            <TInput v-model.number="retry.total_deadline_ms" type="number" @input="markDirty()" />
          </Field>
          <Field label="单次尝试上限 (ms)" hint="一次尝试的最长耗时，超时即判定失败并降级">
            <TInput v-model.number="retry.attempt_deadline_ms" type="number" @input="markDirty()" />
          </Field>
          <Field label="连接超时 (ms)" hint="与上游建立 TCP / TLS 连接的超时">
            <TInput v-model.number="retry.connect_timeout_ms" type="number" @input="markDirty()" />
          </Field>
          <Field label="首字节超时 (ms)" hint="发出请求到收到响应首字节的超时">
            <TInput v-model.number="retry.ttfb_timeout_ms" type="number" @input="markDirty()" />
          </Field>
          <Field label="首个事件超时 (ms)" hint="流式响应中第一个事件到达的超时">
            <TInput v-model.number="retry.first_event_timeout_ms" type="number" @input="markDirty()" />
          </Field>
          <Field label="流空闲超时 (ms)" hint="流式响应里两个数据块之间的最大空闲时间">
            <TInput v-model.number="retry.chunk_idle_timeout_ms" type="number" @input="markDirty()" />
          </Field>
          <Field label="预留余量 (ms)" hint="预留余量（ms）：给收尾/补帧留的时间">
            <TInput v-model.number="retry.reserve_ms" type="number" @input="markDirty()" />
          </Field>
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
                {{ budgetOk ? "≤" : ">" }} {{ retry.total_deadline_ms }} ms
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
    </div>
  </div>
</template>
