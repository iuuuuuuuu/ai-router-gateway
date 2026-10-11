# AI 网关控制台 — UI 规格（所有页面必须遵守）

> 参考视觉：`docs/ccswitch-ref.png`（CC Switch 桌面控制台）。浅色、大量留白、小字号、无深色主题、无重色块。

## 0. 技术栈（已配置好，不要改）

- Vue 3 `<script setup lang="ts">` + Vite 6 + Tailwind CSS v4（`@tailwindcss/vite` 已接入 `vite.config.ts`）
- 图标库：`@lucide/vue`（**不是** lucide-vue-next）。用法：
  `import { Plug, Plus, Pencil } from "@lucide/vue";` → `<Plug :size="16" />`
- **禁止**写 `<style scoped>` 里的自定义 CSS 类做布局（除非动画）。全部用 Tailwind 原子类。
- **禁止**引入任何新的 npm 依赖。

## 1. 设计令牌（`src/style.css` 的 `@theme`，直接用工具类）

| 用途 | 工具类 |
|---|---|
| 页面底色 | `bg-app` (#f6f6f7) |
| 卡片/输入框底 | `bg-surface` (#fff) |
| 侧栏底 | `bg-rail` (#fbfbfc) |
| 分隔线 | `border-line` (#e7e7ea) / `border-line-soft` (#f1f1f3) |
| 主文字 | `text-ink` (#17171a) |
| 次级文字 | `text-ink-2` (#52525b) |
| 弱化文字/占位 | `text-ink-3` (#8e8e97) |
| 主按钮橙 | `bg-brand-500 hover:bg-brand-600`，浅底 `bg-brand-50 text-brand-700 border-brand-200` |
| 使用中/选中青绿 | `bg-teal-50 text-teal-700 border-teal-200`，实心 `bg-teal-500` |
| 成功/警告/错误 | `ok-*` / `warn-*` / `err-*` 各 50/100/500/600/700 |
| 等宽字体 | `font-mono` |

字号约定：页面标题 19px，卡片标题 13.5–14px，正文 12.5–13px，徽章 11px。
圆角约定：卡片 `rounded-xl`，输入/按钮 `rounded-lg`，徽章 `rounded-md`。

## 2. 通用组件（已写好，直接 import，不要重写）

```ts
import {
  Btn, IconBtn, Badge, Card, PageHeader, Field,
  TInput, TSelect, TTextarea, TToggle, Alert, Empty, Spinner, Modal,
} from "../components/ui";
```

- `<Btn variant="primary|ghost|subtle|danger" size="sm|md" :disabled>` — 默认 ghost。
- `<IconBtn title="...">图标</IconBtn>` — 32×32 图标按钮，列表行右侧操作用它。
- `<Badge tone="gray|teal|ok|warn|err|brand">文字</Badge>`
- `<Card :pad="true" :hover="false">…</Card>`
- `<PageHeader title="渠道" subtitle="上游供应商与密钥"> <template #actions>…</template> </PageHeader>`
- `<Field label="名称" hint="可选说明" required> <TInput v-model="x" /> </Field>`
- `<TInput v-model="str" type="password|text|number" mono size="sm|md" :invalid />`
- `<TSelect v-model="str" :options="[{value,label}]" placeholder />`
- `<TTextarea v-model="str" :rows="4" :mono="true" />`
- `<TToggle v-model="bool" label="启用" />`
- `<Alert tone="err|ok|warn|info">…</Alert>`
- `<Empty title="还没有渠道" hint="点击右上角添加"> <template #icon><Plug :size="28" /></template> …按钮… </Empty>`
- `<Spinner :size="14" />`
- `<Modal :open="bool" title="编辑渠道" width="560px" @close="..."> 表单… <template #footer>按钮</template> </Modal>`
- 全局 toast：`import { toast } from "../toast";` → `toast.ok("已保存")` / `toast.err("失败：" + e.message)` / `toast.warn` / `toast.info`

## 3. 共享状态（`src/store.ts`）—— 页面**不要**自己 fetch 配置、不要自己保存

```ts
import { state, markDirty, channelName, modelLabel, uid, channels, models, groups, apiKeys } from "../store";
```

- `state.cfg` — `AdminConfig`，**已加载完成**才会渲染页面（App.vue 保证）。直接改它的字段（`state.cfg.channels.push(...)`）。
- 任何修改后**必须**调用 `markDirty()`，顶栏会出现「保存」按钮。**页面内不要调用 `saveConfig()`**。
- `channels/models/groups/apiKeys` 是 computed，直接用在模板里。
- `channelName(id)` / `modelLabel(id)` 做 id→名称显示。
- `uid("ch")` 生成新 id。
- `state.online` — 网关是否在线（App.vue 每 5s 轮询）。

## 4. API（`src/api.ts`）—— 只读数据与动作

```ts
import { api } from "../api";
api.testChannel(channelId, upstreamModel?)  // → {ok,status,latency_ms,sample} | {ok:false,error,latency_ms}
api.clearCooldown(channelId)                // → {ok:true}
api.switchLogs(limit=200, requestId?)       // → SwitchLog[]
api.requestLogs(limit=200)                  // → RequestLog[]
api.stats()                                 // → {summary, by_model, health}
api.health()                                // → {cooldowns: CooldownRow[]}
```

类型全部已导出：`Channel, ModelBinding, TierMember, Tier, Group, ApiKey, AdminConfig,
SwitchLog, RequestLog, CooldownRow, StatsPayload, TestChannelResult, RetryConfig`。

★ `RetryConfig` 有 **10** 个字段（含 `reserve_ms`）：
`max_attempts, max_cycles, total_deadline_ms, attempt_deadline_ms, connect_timeout_ms,
ttfb_timeout_ms, first_event_timeout_ms, chunk_idle_timeout_ms, reserve_ms, cooldown_wait`。
服务端 I4 校验用的是 `max_attempts × attempt_deadline_ms + reserve_ms ≤ total_deadline_ms`。

## 5. 交互规范

- 列表为空 → `<Empty>`，不要显示空表格。
- 加载中 → `<Spinner>` + 文案；错误 → `<Alert tone="err">`。
- 删除必须二次确认（用 `window.confirm` 即可，不要自己写确认弹窗）。
- 表单校验失败 → `<TInput :invalid="true">` + `<Alert tone="err">` 说明原因，不要用 alert()。
- 危险操作（删除、清空）用 `variant="danger"` 或 `IconBtn danger`。
- 数字输入用 `type="number"`，绑定到 number 时用 `v-model.number`。
- 表格：`<table class="w-full text-[12.5px]">`，表头 `text-ink-3 font-medium text-[11.5px] uppercase tracking-wide`，
  行 `border-t border-line-soft hover:bg-app`，单元格 `px-3 py-2`。

## 6. 绝对禁止

- 修改 `App.vue`、`style.css`、`api.ts`、`store.ts`、`toast.ts`、`components/**`（这些是 Lead 的写入范围）。
- 运行 `pnpm build` / `vite build` / dev server（Lead 统一构建，避免并发冲突）。
- 引入新依赖、新建全局样式文件。
- 用 `any` 之外的 TS 报错不管；**必须**保证模板里用到的变量都已定义。