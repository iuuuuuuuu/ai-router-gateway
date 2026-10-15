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
| I6 | 提交后命中退化：客户端已收到的字节绝不撤回、绝不重发，只就地截断 + 正常收尾；截断必须落库可见（`loop_truncated`），不得伪装成完好回答 |

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

### R6：空行分隔的复读（`detect_loose`）

R1–R5 有一个真实世界的漏洞：它们要求**单元连续贴在尾部**。而模型复读时常常这样写：

```
hello hello hello ...（第一串）

hello hello hello ...（第二串，前面空了一行）

（共 20 遍）
```

两串之间有**空行**，末尾还有一句「（共 20 遍）」总结。此时尾部窗口里既没有连续重复的单元
（空行把它切开了），最后一句还破坏了 R5 的尾部对齐——**整段复读完全不被识别**，用户的回答被毁掉。
实测（12 字符帧步进）：该形状在修复前泄漏 **144 个字符**（整段原样到达客户端）。

R6 是这个漏洞的兜底，只在 R1–R5 全部落空后才跑（`detect().or_else(detect_loose(...))`）：

| 步骤 | 条件 | 理由 |
|---|---|---|
| 前提 | span 内必须含**空行**（其后仅空白） | 空行是「排版换行」而非「内容重复」的分界；普通单行散文、markdown 小节标题、表格行都不含空行，因此完全不受影响 |
| 折叠 | 去掉 span 内所有空白后得到 `dense`，长度 `m` | 把空行/缩进造成的断点接起来，才能看出「其实是同一单元」 |
| 单元 | 在 `dense` 尾部找重复单元，`unit_len ≤ LOOSE_UNIT_MAX_CHARS = 128` | 限定扫描宽度，保证长窗口下开销可控 |
| 结尾话 | 允许末尾最多 `LOOSE_TRAILER_CHARS = 32` 个字符不属于重复 | 吸收模型自己的「（共 N 遍）」总结 |
| R2/R3/R4 | **原样复用**（R4 的分母是折叠后的 `m`） | 只放宽「连续」这一个条件，证据门槛一点没降 |
| 附加 | 单元内必须含字母或数字 | 排除「纯符号/空行」凑出来的假重复 |

假阳性防线（34 例语料 + 12 字符帧步进实测）：多段散文、清单、markdown 标题、分隔线、表格、纯换行、纯空格、
同一句话连写 10 遍——R6 全部不命中。注意 R6 仍受 R1 约束：**短于 `min_chars` 的形状不判定**
（实测 59 字符的两段重复不判定，这是刻意的——holdback 都装不满的回答不构成风险）。

**两套判定，同一套规则**（`detect_span(n, min_span)` 是唯一实现；R6 是它之外的兜底）：

| 判定 | 判定的范围 `n` | R1 的阈值 | 命中后的处置 |
|---|---|---|---|
| 提交前 `push()` | 整个窗口 `len` | `min_chars` | 丢弃整段尝试，内部换候选重试（I1：客户端零字节） |
| 提交后 `push_tail()` | 只判末尾 `tail_chars` | 尾部必须已饱和（`len ≥ tail_chars`） | 就地截断 + 正常收尾（I6：字节已发出，无法重试） |

★**为什么提交后必须只判尾部**：R4 要求 `covered ≥ repeat_ratio × n`，即 `covered ≥ 0.75n`。
若提交后仍判整个 4000 字符窗口，则`n = W₀ + covered`（`W₀` = 循环开始前已有的正常文本）代入得
`covered ≥ 3 × W₀`——循环开始前已有 1024 字符时，模型要复读 **3000 字**才被抓到，用户的回答早已毁掉。
（实测：`W=4000, W₀=1024` → 泄漏 3004 字符。）把判定范围收紧到末尾 `tail_chars` 后，
R4 的语义变成「**最近这段输出就是循环**」，泄漏量被 `tail_chars` 直接约束。

多解时取**覆盖字符最多**者，并列取**单元最短**者（`detect()` 按 `unit_len` 升序扫描，仅在严格更大时替换）。
`LoopHit { unit, repeats, chars }`：`unit` 最多截取 `UNIT_SNIPPET_CHARS = 120` 个字符（诊断用，按字符边界安全截断）。

---

## 2. 提交点与回退缓冲（holdback）

- 网关的**提交点**：流式响应在收到第一个合法内容帧之后才向客户端写首字节（I1）。
- loop guard 只在 **PRE-COMMIT 窗口**生效：尚未提交前，文本增量先攒在 `pending` 里，不写客户端。
- `hold = LoopCfg::min_chars`（默认 64 字符）：`pending` 达到 `hold` 且期间没有命中退化 → 一次性提交，客户端才开始收字节。
- 命中退化 → **保留复读之前的正文**（`LoopHit::real_span` 指出复读占了尾部多少字符，`LoopGuard::text_before` 取回前面的正文），
  并把复读整段从 `pending` 里切掉。切掉后有两种出路：
  1. **纠正**（默认开）：对**同一个候选**发一次纠正请求——把正文尾部（≤4000 字符）作为 assistant 消息、
     再附一条 user 消息说明原因（`你在复读了，接着上次被打断的输出继续`），续写接回同一条客户端流；
  2. **回退**：复读前没有可交付的正文（`kept < hold`，说明模型从第一个字符就在复读）、纠正次数用尽、
     总超时不够、纠正请求打不开时 → 记 `error_class = "degenerate_output"` → 按路由规则换下一个候选。
  客户端在两种出路下都仍是零字节（I1 不变）。
- ★纠正**不是新的一次尝试**：它发生在 `attempt_once` 内部，所以 `attempts` / 切换日志 / `loop_retries` 的语义都不变，
  纠正次数单独记在 `loop_continues`。
- ★`LoopCfg::from_retry` 在 `enabled == false` 时**强制 `min_chars = 0`**：关闭功能 = 不缓冲 = 第一帧立即提交（等于本功能不存在时的行为）。
  保持缓冲却不检测等于「白白推迟首字节」，是欺骗。
- 代价（必须如实写进 README）：开启时首字节推迟约 `min_chars` 个字符（默认 64）。
- **提交后**（I6）：字节已在客户端手里，**不能重新开始**——重试等于把前缀再发一遍。此时命中退化会：
  1. 先把已装配但未发出的 tool call 冲出去（避免把参数截半）；
  2. **纠正**（默认开）：对同一候选发一次纠正请求，把续写**接回同一条流**——客户端看到的是一条连续的回答，
     且复读前的正文一个字都没丢。这与提交前是同一条纠正路径，只是拼接点不同；
  3. **回退**（纠正关掉 / 次数用尽 / 总超时不够 / 打不开 / 模型复读同一段内容）：发一个 `FinishReason::Length`
     的结束帧（客户端看到的是一条**正常结束**的回答），`feed_close_clean` 正常收尾，请求日志写 `loop_truncated = 1`
     （`status` 仍是 `success`，因为客户端确实收到了一条完整合法的流）。
- 代价：中段复读**无法避免**已发出的那部分（这是物理限制）。每次纠正都是一条新的上游流，新流同样要先攒够证据
  才允许再切，所以「反复复读且不收手」的模型每次纠正会多泄漏一个证据窗口；`can_continue_now` 的**卡住判定**
  （复读单元与上一轮完全相同 → 不再纠正）把这种模型的代价压回一次纠正。这个量级由 l13f 钉住。

---

## 3. `server/src/loopguard.rs`

```rust
pub struct LoopCfg { pub enabled: bool, pub min_chars: usize, pub repeat_min_chars: usize,
                     pub repeat_ratio: f64, pub max_repeats: u32, pub window_chars: usize,
                     pub tail_chars: usize }
impl LoopCfg { pub fn from_retry(rc: &RetryConfig) -> Self }
pub struct LoopHit { pub unit: String, pub repeats: u32, pub chars: usize }
pub struct LoopGuard { /* cfg + VecDeque<char> 窗口 */ }
impl LoopGuard {
    pub fn new(cfg: &LoopCfg) -> Self;
    pub fn window_len(&self) -> usize;
    pub fn push(&mut self, delta: &str) -> Option<LoopHit>;      // 提交前：判整个窗口
    pub fn push_tail(&mut self, delta: &str) -> Option<LoopHit>; // 提交后：只判末尾 tail_chars
    fn append(&mut self, delta: &str);
    fn detect(&self) -> Option<LoopHit>;                         // = detect_span(len, min_chars)
    pub fn judge_tail(&self) -> Option<LoopHit>;                 // = detect_span(tail_chars, 2)
    fn detect_span(&self, n: usize, min_span: usize) -> Option<LoopHit>;
    fn trailing_whitespace(&self) -> usize;
    fn trailing_whitespace_within(&self, within: usize) -> usize;
    fn match_span(&self, unit_len: usize, n: usize, len: usize) -> usize;
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
| `tail_chars` | `[16, window_chars]` | 取边界值 |

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

### R6 补充覆盖（t22–t25）

| 编号 | 覆盖 |
|---|---|
| t22 | 空行分隔的两串复读 + 结尾一句 → **必须提交前命中**（12 字符帧步进回放），且 `repeats ≥ max_repeats`、`chars ≥ repeat_min_chars`、单元含字母数字 |
| t23 | 59 字符的两段重复 → **不判定**（R1 未满足；刻意的设计边界，见 §8） |
| t24 | 纯排版/结构形状（多段散文、清单、markdown 标题、分隔线、表格、纯换行、纯空格）→ 全部不命中 |
| t25 | 提交后路径同样兜底：空行落在尾部窗口内时**更早**截断，且单元无尾随空格（证明走的是折叠路径） |

---

## 4. `server/src/config.rs`（`RetryConfig` 新增 10 个字段）

| 字段 | 默认 | `#[serde(default = ...)]` |
|---|---|---|
| `loop_guard_enabled` | `true` | `d_loop_guard_enabled` |
| `loop_guard_min_chars` | `64` | `d_loop_guard_min_chars` |
| `loop_guard_repeat_min_chars` | `24` | `d_loop_guard_repeat_min_chars` |
| `loop_guard_repeat_ratio` | `0.75` | `d_loop_guard_repeat_ratio` |
| `loop_guard_max_repeats` | `4` | `d_loop_guard_max_repeats` |
| `loop_guard_window_chars` | `4000` | `d_loop_guard_window_chars` |
| `loop_guard_tail_chars` | `128` | `d_loop_guard_tail_chars` |
| `loop_guard_continue_enabled` | `true` | `d_loop_guard_continue_enabled` |
| `loop_guard_continue_max` | `2`（上限 `LOOP_CONTINUE_MAX_CAP = 5`） | `d_loop_guard_continue_max` |
| `loop_guard_continue_prompt` | 内置中文提示词（见下） | `d_loop_guard_continue_prompt` |

**三处同步**：字段声明 + `fn d_xxx()` + 手写 `impl Default for RetryConfig`。缺任何一处都会让老配置文件与默认配置走岔。

`d_loop_guard_continue_prompt()` 与 `loopguard::d_continue_prompt()` 是**同一段文案的两份拷贝**（同一句文档注释在两侧都写了「保持同步」）：

> 你刚才的输出出现了复读：同一段内容「{unit}」连续重复了 {repeats} 次，已被系统截断。请不要重复已经说过的内容，也不要道歉或解释，直接接着上面被打断的地方继续输出剩余内容。

- `{unit}` 在渲染时按 `PROMPT_UNIT_MAX_CHARS = 60` 截断——提示词可配置，不设上限会让病态复读把一次请求撑成兆字节。
- 配置里留空（或全空白）时回落到内置文案，绝不发出没有「为什么」的纠正提示词。

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
- `proxy_err_message` 增加 `DegenerateOutput` 分支，文案是**中文且带原因**：
  `输出复读：同一段内容连续重复了 {repeats} 次（重复单元 {unit_len} 个字符：{unit}）`（用于日志与 `switch_logs.message`）。
  ★`proxy_err_message` 的全部文案都是中文（用户看不懂英文报错），`code` 标识符保持英文供程序判断。
- 纠正引擎（`continuation_ir` / `can_continue_now` / `open_continuation` / `start_post_commit_round`）：
  - `continuation_ir(base, kept, prompt)`：克隆原 IR，追加一条 assistant 消息（`kept` 的尾部 ≤ `CONTINUE_CONTEXT_CHARS = 4000` 字符，**空白时整条跳过**——上游会拒绝空 assistant 消息），
    再追加一条 user 消息（渲染后的提示词）。同候选、同渠道、同上游模型：这是**同一条回答的续写**，换候选等于把两个模型的输出缝在一起。
  - `can_continue_now(cfg, done, deadline, ttfb, stuck)`：四道闸门——`continue_enabled`、`done < continue_max`、
    总超时留得下这次请求（`now < deadline - ttfb`，纠正**绝不延长**本次预算）、以及 `stuck`（复读单元与上一轮完全相同 → 模型卡住了，不再纠正）。
  - `LoopGuard::reseed(&kept)`：切掉复读后**重建窗口**，只保留 `tail_chars` 的尾部。不重建的话下一帧会立刻拿被切掉的复读当证据、报出一次新的复读。
  - `LoopGuard::text_before(real_span)`：从窗口尾部回退 `real_span` 个字符，得到「复读之前的正文」。
  - `pending` 的切法与窗口**不同**：`pending` 是窗口**减去触发帧**（触发帧进了 guard 但没进 `pending`），
    所以丢弃量是 `real_span - trigger_len`，按 `real_span` 直接切会啃掉好文本。
  - 复读前正文不足 `hold`（= 提交阈值）时**不纠正也不交付**，仍走旧回退：说明模型从第一个字符就在复读，此时「续写」没有可续的正文，
    硬发出去等于把复读的碎片当成成功回答交给客户端。
- POST-COMMIT pump：同一个 `guard` 被 move 进 pump 任务（跨提交点保持窗口，因此**横跨提交点的循环**也判得准），
  `FrameClass::Text` 分支改为 `guard.push_tail(&t)`；命中即按 §2 的提交后流程截断收尾并 `return`。
- 非流式路径不改（见 §8.1）。

---

## 7. `server/src/store.rs` + `server/src/routes.rs`（可观测面）

- `request_logs` 增加 `loop_retries INTEGER NOT NULL DEFAULT 0`（含老库 ALTER 迁移兜底）。
- 落库口径：`loop_retries` = 本次客户端请求的 `records` 里 `error_class == "degenerate_output"` 的条数（= 内部被拦下的次数）。
- `switch_logs` 每条退化尝试单独一行：`outcome = "failed"`、`error_class = "degenerate_output"`、`channel_id` 是被丢弃的那个候选。
- `list_request_logs` / `stats_*` / `group_member_stats` 都要带出 `loop_retries`（按 `final_channel::final_upstream_model` 聚合）。
- `request_logs` 另有 `loop_continues INTEGER NOT NULL DEFAULT 0`（同样含老库 ALTER 迁移兜底）：本次回答**实际发起过几次纠正**。
  `0` = 从没复读；`> 0` = 复读过、被纠正过，且客户端拿到的是**完整回答**（与 `loop_truncated` 语义相反，两者互斥：纠正成功就不会截断）。
- `request_logs` 另有 `error_class TEXT NOT NULL DEFAULT ''` 与 `error_message TEXT NOT NULL DEFAULT ''`（同样含老库 ALTER 迁移兜底）：
  **失败原因直接记在请求日志这一行上**。没有这两列时 `exhausted` 行只说「失败了」，真实原因只能去切换日志里逐条翻——这正是要修的缺口。
  成功行 `error_class` 为空；被纠正过的回答写 `loop_continue`，被截断的写 `loop_truncated` 文案。
- `store::error_class_label(class)` 是失败类的**唯一中文词表**：控制台、切换日志与客户端错误消息共用它，
  三处不可能对同一个类给出不同说法。
- `request_logs` 另有 `loop_truncated INTEGER NOT NULL DEFAULT 0`（同样含老库 ALTER 迁移兜底），口径是**提交后截断**：
  `1` = 这条回答被 loop guard 截断过。★与 `loop_retries` **语义相反**，不可混用：
  `loop_retries` 那些尝试客户端**一个字节都没看到**（提交前丢弃），而 `loop_truncated` 的回答客户端**已经收到了**（只是没给完）。
  两者都进 `/admin/api/stats` 的 `summary`，控制台分别渲染（「死循环拦截 N 次」/「截断收尾 N 次」）。

---

## 8. 诚实边界（必须写进 README，不含糊）

1. **只有流式路径检测退化**。非流式（`stream: false`）一次拿完整 JSON，没有增量帧，本版本不检测。
2. **提交后能检测，也能纠正，但不能重新开始**。已经向客户端写了首字节之后再出现的退化**无法**内部换候选
   （换了就等于往同一条回答里塞第二次回答，违反 I2），处置是**纠正续写**；纠正不可用时才**就地截断 + 正常收尾**（§2）。
   ★无论哪条路，提交后命中**一定**会泄漏一段已发出的复读文本，泄漏量由 `tail_chars` 与纠正轮数共同决定：
   每条新流都要先攒够证据才允许再切，所以「反复复读且不收手」的模型每次纠正多泄漏一个证据窗口，
   卡住判定把它压回一次纠正。l13f 把这条不等式钉在测试里（`leaked <= (1 + loop_continues) * 24`）。
   这是物理限制（字节已在客户端手里），不是实现缺陷；把它说成「客户端看不到」是错的。
   注意这与旧行为不同：旧版本对提交后的复读**完全不检测**（只有把整段尝试丢掉才安全，所以当时干脆不看）。
3. 开启 loop guard 的代价是首字节推迟约 `min_chars` 个字符；关掉它首字节立即返回，但退化不再被拦截。
4. 误判是可能的（长文里恰好出现高比例重复，例如代码里连续空行/重复分隔线）。阈值可配；命中只影响该次尝试，会被内部重试吸收。

---

## 9. 客户端

- 设置页：死循环拦截开关 + **6** 个阈值输入（`loop-guard-enabled` / `-min-chars` / `-repeat-min-chars` / `-repeat-ratio` / `-max-repeats` / `-window-chars` / `-tail-chars`）
  + **3 个纠正项**（`loop-guard-continue-enabled` / `-continue-max` / `-continue-prompt`），并把「提交前 / 提交后 / 纠正」的差别写在页面上。
- 日志页：`error_class == "degenerate_output"` 显示为「输出退化」（warn 色）；`loop_truncated > 0` 的行状态显示为
  **「成功（已截断）」**（warn 色 + tooltip 说明），不得只写「成功」——那是把截断过的回答当成完好回答。
  `loop_continues > 0` 的行显示为**「成功（已纠正复读）」**（warn 色 + tooltip 说明已自动纠正几次），同样不得只写「成功」。
- 日志页新增**「失败原因」列**：直接渲染 `error_message`（失败行为中文原因，成功行为 `—`），鼠标悬停看全文。
  这一列就是「报错要看得懂」的落点：不必再去切换日志里翻是哪条尝试、为什么失败。
- 统计页：死循环拦截次数（`loop_retries`）与截断收尾次数（`loop_truncated`）**分两个徽章**，
  因为前者「客户端看不到」、后者「客户端收到了」；分组详情/模型详情同样展示。

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
