# 测试与验收（TESTING）

> 四层测试、假上游矩阵、真机验收探针的约定。
> 最后核对：2026-10-17。**已发布最新版 = v0.4.8**（tag 与 GitHub Release 都存在）。

---

## 0. 主门禁：`cargo test --release`

```powershell
cd server
cargo test --release
```

**2026-10-17 实测结果：202 passed / 0 failed / 0 ignored**（本机 `cargo test --release --locked`，退出码 0）。

共 13 个测试目标（`lib.rs` + `main.rs` + 10 个集成测试文件 + doc-tests；其中 `main.rs` 与 doc-tests 各 0 用例）。有用例的分项：

| 目标 | 条数 | 覆盖 |
|---|---|---|
| `unittests src/lib.rs` | 68 | `loopguard` 判定 + `proxy` + `inflight` + `routes` + `store` |
| `channel_models` | 13 | 上游 models 拉取（空列表 vs 不可识别载荷、超时、去重） |
| `exposed_auto` | 7 | 暴露模型按成员自动派生 |
| `group_overrides` | 9 | 组级覆盖与 I4 校验 |
| `group_wildcard` | 8 | 通配路由（含 **EXACT WINS**） |
| `holdback_cap` | 3 | 提交前暂存上限（`loop_guard_tail_chars` 与纠正轮数共同约束） |
| `inflight` | 10 | 实时并发计数 |
| `loopguard` | 27 | R1–R6 判定、钳位、纠正路径 |
| `matrix` | 31 | 500/429/401/404/hang/垃圾/空体/流中断/错误帧/EOF 提前/tier 循环/日志成本精确断言/I4 校验与 clamp/空库统计 |
| `reasoning` | 12 | 思考透传（同协议回放 / 跨协议翻译 / 边界） |
| `roundtrip` | 14 | 3 协议 buffered+stream + 4 跨协议组合 + 4 工具调用参数保真 |

★ 跑之前先确认**工作树状态**：有在途未提交改动时，「在某个提交上跑测试」实际测的是**工作树**。要证明「修复前应当红」的红基线，必须用 `git worktree add` 检出纯净副本再跑。

### 0.1 云端 CI 跑什么、不跑什么

`.github/workflows/ci.yml` 在每次 push 到 `main` 与每个 PR 上跑 `cargo test --release --locked`（`windows-latest`）。它与本机命令的**唯一差别**是 `--locked`：`Cargo.lock` 与 `Cargo.toml` 不一致时直接失败，而不是在云端静默解析出一组新依赖。

**★ CI 绿只代表这张表里的集成测试过了，不证明 I1 在真实实例上成立。** 真机验收（真实 `gw-server.exe` + 真实 Tauri 客户端 + 真实数据面请求）仍只在本机跑，见 §3。CI 的范围取舍见 [DECISIONS.md](DECISIONS.md) D09 与其 2026-10-17 翻案说明。

**★ CI 也顺带守一条本机守不住的东西：主机时区无关性。** `windows-latest` 的默认时区是 UTC，而开发机是 `+08:00`。凡是把本地时间写进断言、或用 `chrono::Local` 渲染却被拿来与固定偏移字符串比较的测试，本机永远绿、CI 红。首次上线 CI 时就抓到了一例（`since_from_days_is_an_exact_rfc3339_boundary`）。**在断言里钉偏移（`FixedOffset` / `Utc`），不要让测试继承 `Local`。**

---

## 1. 四层测试（设计与现状）

| 层 | 内容 | 现状 |
|---|---|---|
| **L1** 表驱动状态机单测 | 毫秒级、无 IO；输入「一次 attempt 的字节事件序列」→ 输出 FAIL / COMMITTED / POST_COMMIT_* | `loopguard.rs` 与 `proxy` 的 `#[cfg(test)] mod tests` |
| **L2** 假上游集成测试（**真 socket、真 fetch**） | 起 N 个假上游，每种一种失败模式；网关配置指向它们，真实跑降级链 | `server/tests/` + `server/tests/support/mod.rs` |
| **L3** 端到端契约测试 | 真实客户端形态打网关 | 由真机验收探针覆盖（见 §3） |
| **L4** 故障注入 / 混沌 | 随机 kill 上游、延迟抖动、WAL BUSY、填满磁盘 | **未做**（诚实标注） |

**关键设计**：L1 的判定逻辑要能**纯函数化**，这样能穷举几十种字节序列而不用为每种起真 server；**L2 只挑每类代表做真 socket 验证**，防止 L1 的假 adapter 掩盖半包、Nagle、超时等真实网络行为。

---

## 2. 假上游矩阵

`server/tests/support/mod.rs` 的 `Behavior` 枚举就是这份矩阵的落点。已实现的行为：

**失败模式**：`Fail500` / `Fail429`（带 `Retry-After: 1`）/ `Fail401` / `Fail404` / `Hang`（永不发头，走 TTFB 超时）/ `Garbage`（200 但体非 JSON 非 SSE）/ `Empty`（200 空体）/ `StreamBreakMid`（发一帧后 EOF 无 finish）/ `StreamErrorFrame`（SSE 里带 OpenAI error 帧）/ `StreamEofEarly`（EOF 早于任何内容）。

**正常路径**：`GoodJson` / `GoodStream`（OpenAI）、`GoodAnthropicJson` / `GoodAnthropicStream`、`GoodGeminiJson` / `GoodGeminiStream`。

**工具调用保真**：`GoodGeminiToolStream`（整包 functionCall）/ `GoodOpenAiToolStream`（参数分片）/ `GoodOpenAiToolOneFrame`（name 与完整参数同帧）/ `GoodAnthropicToolStream`（`content_block_start` 空 input + `input_json_delta`）。

**退化输出**：`DegenerateShortCycle`（同一短句重复 40 帧）/ `DegenerateSingleChar`（单字符重复 80 帧）/ `NormalLongText`（约 2000 字符不重复，**误判对照**）/ `NormalThenDegenerate` / `NormalThenDegenerateLong`（400 帧，用于证明「截断真的停了流」）/ `NormalOnlyStream`（提交后正常续写，**绝不能被切**）/ `ToolAfterShortText`（短文本后跟工具调用，缓冲的文本必须**随工具调用一起提交**，绝不能丢）。

★ **对照组和失败组一样重要**：误判（把正常回答切掉）比漏判更糟。`NormalLongText` / `NormalOnlyStream` 就是为此存在的。

---

## 3. 真机验收探针（本仓库的主力证据）

### 3.1 为什么要真机

核心承诺 —— I1「提交前客户端 socket 字节数为 0」、失败零外漏、提交后不重发 —— **只有在真实实例上跑真实字节流才能证伪**。纯单元测试证明不了这些。所以验收一律在**真实 `gw-server.exe` + 真实 Tauri 客户端 + 真实数据面请求**上跑。

### 3.2 探针的硬约定（写新探针时照这个来）

| 约定 | 内容 |
|---|---|
| 位置与入库 | `tools/_accept-*.mjs`，**不入库**（逐个登记 `.gitignore`，见 [DECISIONS.md](DECISIONS.md) D10） |
| CDP 端口 | `9223`（默认，可作第一个参数覆盖）；客户端由 `node tools/start-client.mjs <dir> 9223` 拉起 |
| 管理面 / 数据面 | `http://127.0.0.1:9090` / `http://127.0.0.1:8080` |
| 断言与计数 | `ck(name, ok, extra)` 逐条打印 `PASS` / `FAIL`，末尾汇总 `pass` / `fail` |
| **退出码** | **全绿 = 0，有失败 = 非 0**（探针自身退出码就是门禁信号，不许恒 0） |
| 配置基线 | 改动线上配置的探针**必须**先读基线并断言一致（不一致就**中止、绝不注入**），结束时 `PUT` 回基线并 `deepEqual` 校验 |
| 救急口 | 会改配置的探针提供 `--restore` 之类只还原的分支，供探针中途崩掉时救场 |
| 真实数据 | 造数据要**真的走数据面请求**产生（例如打满 503 全冷却路径），不伪造库里的行 |
| 无干扰 | **绝不动鼠标、不抢焦点、不点桌面**；全靠 CDP 与 HTTP 完成 |

### 3.3 已存在的探针（本机 `tools/`，不入库）

按用途分组（文件在 `tools/` 下，多数为 `_accept-*.mjs`）：

| 探针 | 覆盖 |
|---|---|
| `_accept-gdpage.mjs` | 分组详情三页签分页（走真实数据面造 12 条永久冷却 + 全冷却请求） |
| `_accept-gdsettings.mjs` / `_accept-gdcharts.mjs` | 分组详情「自己的配置」草稿不丢 + 档位套用 / 用量页签三张 ECharts 图 |
| `_accept-page.mjs` / `_accept-save.mjs` / `_accept-inuse.mjs` | 列表分页 / 保存失败修复 / 「使用中」高亮 |
| `_accept-v044-sec.mjs` | 中文「秒」+ 安全项回归 |
| `_accept-chart.mjs` / `_accept-v043.mjs` | 统计页图表与派生字段 |
| `_accept-delgroup.mjs` / `_accept-tagscan.mjs` | 删除分组确认框 / 运行时未解析组件扫描 |
| `_accept-keycopy.mjs` | 从看板复制完整密钥（界面文本始终脱敏，完整值只在点击那一刻取出） |
| `_accept-looptail.mjs` / `_accept-midloop.mjs` | 提交后截断 / 中段复读 |
| `_accept-tieredit.mjs` / `_accept-viewport.mjs` | 层级编辑 / 视口适配 |
| `_accept-exposed*.mjs` | 暴露模型自动派生（含新旧构建对比） |
| `verify-window-state.mjs`（**入库**） | 窗口尺寸/位置记忆 |

★ 探针**不入库**意味着别人 clone 后拿不到 —— 这是**有意接受的代价**（它们硬编码本机端点与真实密钥）。需要复用时按 §3.2 的约定自己写。

---

## 4. ★ 禁止假绿门禁

**任何测试/校验/CI 都不得「显示通过但实际没验证到东西」。**

不允许的形态：断言被跳过或吞异常 · mock/桩替掉真实逻辑 · 脚本恒 `exit 0` · 只跑编译或单测就宣称通过 · 失败用例被注释或降级 · 占位 always-pass · 靠 `|| true` 或忽略退出码蒙过。

**门禁必须驱动真实路径**（实机 / 真请求 / 真数据），且**必须能真的变红**：

> **新门禁先做故障注入证明它会失败，才承认它是绿的。**

发现假绿按**缺陷**处理：先修门禁，再谈交付。

**本仓库的真实先例**（都值得记住）：
- 本机的 `tools/_build-client.sh`（不入库）注释里记着：走 `node_modules/.bin` shim 曾产生**假的 exit 0**（末尾 echo 成功、构建其实没跑）。见 [DECISIONS.md](DECISIONS.md) D13。
- v0.4.6 的 `retry-after` 畸形（`"retry-after: 30"` 被当成头部**值**）是探针 C6 **先红后绿**发现的真缺陷 —— 这条断言真的能变红。

---

## 5. 新增测试/探针的检查清单

- [ ] 断言驱动的是**真实路径**（真 socket / 真请求 / 真实例），不是桩。
- [ ] **故障注入**证明过它会失败（临时改坏被测逻辑 → 观察变红 → 改回）。
- [ ] 退出码：全绿 0，有失败非 0。**没有** `|| true`、没有吞异常。
- [ ] 加了**反向/对照**断言（正常输入不该被误判），而不只是失败组。
- [ ] 改线上配置的探针：读基线 → 断言 → 结束时还原 + 校验。
- [ ] 钉住关键数字（如 loop guard 的 `25`、泄漏不等式 `leaked <= (1 + loop_continues) * repeat_min_chars`）—— 改阈值必须让测试变红。
- [ ] 不写本机绝对路径、真实密钥、真实上游地址进**入库**的文件。
- [ ] 运行过程**不干扰**正在使用这台电脑的人（不动鼠标、不抢焦点）。
