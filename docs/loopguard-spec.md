# 死循环输出内部检测（loop guard）实现规范

> 本文档在实现冻结后按代码重建（原文件曾被一次错误的编码转写毁坏）。**代码是唯一权威**：
> `server/src/loopguard.rs` / `server/src/router.rs` / `server/src/proxy.rs`。

**需求原话**（用户 m01327 / m01343）：
> 「我希望加一个功能,就是监测 对话中 是否当前输出在死循环 输出无意义的信息,我希望可以在内部自动中断,然后再重复调用」
> 「外部的调用对话不能中断,这一切都发生在内部,我希望是这样子」

即：检测模型流式输出中的**退化重复**（死循环 / 无意义重复）→ **在网关内部**中断该次尝试 → 内部换候选重试 →
**客户端只看到一次正常回答**，外部对话绝不中断、绝不出现重复内容。

---

## 0. 不变量（不可破坏）

| 编号 | 内容 |
|---|---|
| I1 | 提交前客户端 socket 零字节：退化尝试必须整段丢弃，绝不外漏 |
| I2 | 终态互斥二值：一次客户端请求最终只有「成功」或「失败」，不存在半成品 |
| I3 | 每轮循环每个 (channel, model) 至多尝试一次 |
| I4 | `max_attempts × attempt_deadline_ms + reserve_ms ≤ total_deadline_ms`（全局与组级都校验） |
| I5 | 全冷却时必须等待，绝不零尝试失败 |

---

## 1. 判定语义（唯一权威定义）

对**滚动字符窗口**（尾部最多 `window_chars` 个字符）的尾串判定。设窗口长度 `n`，候选单元长度 `unit_len ∈ [1, n/2]`，
`span` = 尾串里与「末尾 `unit_len` 个字符」逐字符相同的最大长度，`repeats = span / unit_len`，`covered = repeats × unit_len`。
命中需同时满足：

| 规则 | 条件 | 含义 |
|---|---|---|
| R1 | `n ≥ min_chars` | 缓冲不足时不判定（`min_chars` 同时就是提交前 holdback） |
| R2 | `repeats ≥ max_repeats` | 重复次数够多（`max_repeats` 至少 2） |
| R3 | `covered ≥ repeat_min_chars` | 重复覆盖的字符数够长 |
| R4 | `covered ≥ repeat_ratio × n` | 重复占窗口的比例够高 |
| R5 | 单元不能整个落在窗口尾部的空白串里 | 排除缩进/填充空白造成的误判；扫描从 `trailing_whitespace + 1` 起 |

多解时取**覆盖字符最多**者，并列取**单元最短**者（`detect()` 按 `unit_len` 升序扫描，仅在严格更大时替换）。
`LoopHit { unit, repeats, chars }`：`unit` 最多截取 `UNIT_SNIPPET_CHARS = 120` 个字符（诊断用，按字符边界安全截断）。

---

## 2. 提交点与回退缓冲（holdback）

- 网关的**提交点**：流式响应在收到第一个合法内容帧之后才向客户端写首字节（I1）。
- loop guard 只在 **PRE-COMMIT 窗口**生效：尚未提交前，文本增量先攒在 `pending` 里，不写客户端。
- `hold = LoopCfg::min_chars`（默认 64 字符）：`pending` 达到 `hold` 且期间没有命中退化 → 一次性提交，客户端才开始收字节。
- 命中退化 → 丢弃整个 `pending` + 该次尝试 → 记 `error_class = "degenerate_output"` → 按路由规则换下一个候选。客户端此时仍是零字节。
- ★`LoopCfg::from_retry` 在 `enabled == false` 时**强制 `min_chars = 0`**：关闭功能 = 不缓冲 = 第一帧立即提交（等于本功能不存在时的行为）。
  保持缓冲却不检测等于「白白推迟首字节」，是欺骗。
- 代价（必须如实写进 README）：开启时首字节推迟约 `min_chars` 个字符（默认 64）。

---

## 3. `server/src/loopguard.rs`

```rust
pub struct LoopCfg { pub enabled: bool, pub min_chars: usize, pub repeat_min_chars: usize,
                     pub repeat_ratio: f64, pub max_repeats: u32, pub window_chars: usize }
impl LoopCfg { pub fn from_retry(rc: &RetryConfig) -> Self }
pub struct LoopHit { pub unit: String, pub repeats: u32, pub chars: usize }
pub struct LoopGuard { /* cfg + VecDeque<char> 窗口 */ }
impl LoopGuard {
    pub fn new(cfg: &LoopCfg) -> Self;
    pub fn window_len(&self) -> usize;
    pub fn push(&mut self, delta: &str) -> Option<LoopHit>;
    fn detect(&self) -> Option<LoopHit>;
    fn trailing_whitespace(&self) -> usize;
}
```

`from_retry` 的 clamp（**永不 panic、永不返回错误**：一个配置笔误绝不能弄坏客户端请求，越界一律 `tracing::warn!` 后取安全值）：

| 字段 | clamp | 越界时 |
|---|---|---|
| `window_chars` | `[64, 65_536]` | 取边界值 |
| `min_chars` | `min(configured, window_chars)`；`enabled == false` 时强制 0 | 取窗口上限 / 0 |
| `repeat_min_chars` | `[1, window_chars]` | 取边界值 |
| `max_repeats` | `≥ 2` | 取 2 |
| `repeat_ratio` | 必须 `is_finite()` 且 `(0, 1]` | 取 0.75 |

`push()`：`enabled == false` 立即 `None` 且**不累积**任何字符；否则把 `delta` 逐字符入队，超窗口用 `drain(..excess)` 丢弃最老的字符，然后 `detect()`。

### `#[cfg(test)] mod tests` 必须覆盖（15 条）

1. 单字符重复「啊」×100 → 命中，`unit_len == 1`；
2. 短周期短语重复（如「好的，我来处理。」）→ 命中；
3. 重复次数不足 → 不命中（R2）；
4. 覆盖字符数不足 → 不命中（R3）；
5. 重复占比不足（正常长文 + 尾部少量重复）→ 不命中（R4）；
6. 缓冲长度不足 `min_chars` → 不命中（R1）；
7. 纯空白填充不得命中（R5）；
8. 单元恰好等于尾部空白串长度 → 不命中；
9. `enabled == false` → 永不命中且 `window_len() == 0`；
10. 超长输入时窗口长度恒 `≤ window_chars`；
11. 多解时取覆盖最多 / 单元最短；
12. `LoopHit::unit` 截断到 ≤ 120 字符且不破字符边界；
13. `from_retry` 对越界值的 clamp 行为（窗口/最小长度/重复次数/比例）；
14. `from_retry` 在 `enabled == false` 时 `min_chars == 0`；
15. `chars == repeats × unit_len`。

---

## 4. `server/src/config.rs`（`RetryConfig` 新增 6 个字段）

| 字段 | 默认 | `#[serde(default = ...)]` |
|---|---|---|
| `loop_guard_enabled` | `true` | `d_loop_guard_enabled` |
| `loop_guard_min_chars` | `64` | `d_loop_guard_min_chars` |
| `loop_guard_repeat_min_chars` | `24` | `d_loop_guard_repeat_min_chars` |
| `loop_guard_repeat_ratio` | `0.75` | `d_loop_guard_repeat_ratio` |
| `loop_guard_max_repeats` | `4` | `d_loop_guard_max_repeats` |
| `loop_guard_window_chars` | `4000` | `d_loop_guard_window_chars` |

**三处同步**：字段声明 + `fn d_xxx()` + 手写 `impl Default for RetryConfig`。缺任何一处都会让老配置文件与默认配置走岔。

---

## 5. `server/src/router.rs`

- `Budget` 增加 `pub loop_guard: crate::loopguard::LoopCfg`；`build_budget` 用 `LoopCfg::from_retry(rc)` 填充。
- `ProxyError` 增加 `DegenerateOutput { unit: String, repeats: u32 }`。
- `classify_failure` 增加分支（与 `EmptyResponse` 同形）：**scoped** 冷却键 + `Duration::from_secs(15)` + class `"degenerate_output"`。
  即只隔离「该渠道 × 该上游模型」15 秒，绝不整渠道隔离（退化是模型行为，不是渠道故障）。
- **不动 `validate_budget`**（loop guard 参数不是预算的一部分，不参与 I4）。

---

## 6. `server/src/proxy.rs`

- `attempt_stream` 增加 `LoopGuard` / `pending` / `hold`（`hold = budget.loop_guard.min_chars`）。
- PRE-COMMIT 循环四分支：
  1. **文本帧**：先 `guard.push(&t)`，命中 → 返回 `Err((ProxyError::DegenerateOutput { unit, repeats }, usage))`；
     未命中则攒进 `pending`，`pending.chars().count() >= hold` 时才 `break` 出循环并提交；
  2. **Finish 帧**：正常收尾，提交；
  3. **Done（流结束）**：正常收尾，提交；
  4. **流结束返回 None**：按空响应/正常收尾处理（与既有语义一致）。
- `proxy_err_message` 增加 `DegenerateOutput` 分支，文案形如
  `degenerate output: repeated {unit_len}-char unit {repeats} times`（用于日志与 `switch_logs.message`）。
- **POST-COMMIT pump 一行不改**；**非流式路径不改**（见 §8）。

---

## 7. `server/src/store.rs` + `server/src/routes.rs`（可观测面）

- `request_logs` 增加 `loop_retries INTEGER NOT NULL DEFAULT 0`（含老库 ALTER 迁移兜底）。
- 落库口径：`loop_retries` = 本次客户端请求的 `records` 里 `error_class == "degenerate_output"` 的条数（= 内部被拦下的次数）。
- `switch_logs` 每条退化尝试单独一行：`outcome = "failed"`、`error_class = "degenerate_output"`、`channel_id` 是被丢弃的那个候选。
- `list_request_logs` / `stats_*` / `group_member_stats` 都要带出 `loop_retries`（按 `final_channel::final_upstream_model` 聚合）。

---

## 8. 诚实边界（必须写进 README，不含糊）

1. **只有流式路径检测退化**。非流式（`stream: false`）一次拿完整 JSON，没有增量帧，本版本不检测。
2. **只有提交前能内部重试**。已经向客户端写了首字节之后再出现的退化**无法**内部换候选（换了就等于往同一条回答里塞第二次回答，违反 I2）。
   这种情况沿用既有 `feed_repair` 语义：补一个错误帧 + 正常终止符，客户端拿到结构完整但带 `finish_reason=error` 的响应。
3. 开启 loop guard 的代价是首字节推迟约 `min_chars` 个字符；关掉它首字节立即返回，但退化不再被拦截。
4. 误判是可能的（长文里恰好出现高比例重复，例如代码里连续空行/重复分隔线）。阈值可配；命中只影响该次尝试，会被内部重试吸收。

---

## 9. 客户端

- 设置页：死循环拦截开关 + 5 个阈值输入（`loop-guard-enabled` / `-min-chars` / `-repeat-min-chars` / `-repeat-ratio` / `-max-repeats` / `-window-chars`）。
- 日志页：`error_class == "degenerate_output"` 显示为「输出退化」（warn 色）。
- 统计页：死循环拦截次数（`loop_retries`）；分组详情/模型详情同样展示。

---

## 10. 端到端验收（Task D，Lead 亲跑）

工具：`tools/fake-loop-upstream.mjs`（真实 HTTP 服务，默认端口 7871；`stream: true` 时每 ~15ms 发一帧
`data: {"choices":[{"delta":{"content":"好的，我来处理。"}}]}`，共 40 帧，然后 `finish_reason: "stop"` + `data: [DONE]`；
模式 loop / normal / tail；`/_stats` 暴露自身帧数——**必须能证明它自己在死循环**）；`tools/verify-loop-guard.mjs`（断言器）。

在真实实例上必须逐条通过：

1. 建临时渠道指 fake + 临时模型 + 临时分组 `grp_loop`（一级成员顺序 = [fake, real]）；
2. 用该分组密钥发**真实** `POST /v1/chat/completions`（stream）→ HTTP 200、`finish_reason == "stop"`、正文是真实上游的正常回答、
   原始 SSE 正文**不含**「好的，我来处理。」（= I1）、收到 `data: [DONE]`；
3. `/admin/api/logs/switch?limit=20` 里存在一行 `error_class == "degenerate_output"`、`channel_id` 是 fake、`outcome == "failed"`；
4. `/admin/api/logs/requests?limit=5` 最新一行：`status == "success"`、`attempts >= 2`、`loop_retries >= 1`、`final_channel` 是真实渠道；
5. OpenAI / Anthropic / Gemini 三个入口各跑一遍，断言同上；
6. **反向对照**：直接打真实渠道（不经 fake）→ `loop_retries == 0`，正常回答；
7. **误判对照**：让 fake 输出一段 2000 字符的正常互不重复文本 → 不被拦截；
8. **代价实测**：记录开启/关闭 loop guard 时的 TTFT（ms），如实报告差值；
9. 收尾：还原基线配置，确认分组数/渠道数/模型数回到基线、无遗留冷却行。

**判定标准**：1~7 全绿才算验收通过；8 的数值如实报告，不做美化。

实际验收结果（2026-10-13）：`node tools/verify-loop-guard.mjs --ui` → **PASS 45 / FAIL 0**。
