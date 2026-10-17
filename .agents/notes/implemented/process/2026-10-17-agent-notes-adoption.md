# Agent Note: 引入 Agent Notes 决策笔记树，与 DECISIONS.md 并存分治

Status: implemented

## Problem

这个仓库的「为什么这么选、放弃了什么」原本只有两个去处：`docs/DECISIONS.md` 的 17 条自由格式条目，和散落在 `docs/` 各篇正文里的临时说明。两者都没有机械门禁 —— 写不写、写成什么样，全靠当时的人记得。

具体后果有三条：

1. **漏写无法被发现**。改一个跨文件契约（比如 `stats_by_model` 的聚合口径）却不动 `DECISIONS.md`，没有任何东西会变红。本仓库反复强调「禁止假绿门禁」，但决定记录本身恰恰没有任何门禁。
2. **结构不固定**。`DECISIONS.md` 每条的节选结构靠作者自觉，有的写了「排除了什么」，有的只写结论。被否掉的路线因此经常在后续会话里被重新提出来。
3. **生命周期无处表达**。一个想法从「拟议」到「落地」到「被取代」，`DECISIONS.md` 只有「过期即删」一条规则，删掉的历史就没了，而 `proposed` 阶段根本没有地方存放。

需要一套结构固定、能被脚本检查、且区分「未落地 / 已落地 / 被否 / 已封存」的记录方式。

## Decision

仓库采用 Agent Notes 规范，笔记树落在 `.agents/notes/`：

```
.agents/notes/{proposed,implemented,rejected,archived}/{feature,bug-fix,simplification,architecture,process,testing}/
```

路径即状态与分类，文件名 `yyyy-mm-dd-topic.md`；`archived/` 另有 `manifest.json` 记录每篇的 SHA-256 封印。只建用到的那几个目录，不预建空目录。

**分工（本次定下的边界）**：

- `docs/DECISIONS.md` 的 **D01–D17 冻结为只读历史**：不再新增条目，也不因新决策去改写旧条目正文。它的既有价值是「当年为什么这么选」的一手记录，改动它等于伪造历史。
- **新的决策一律进 `.agents/notes/`**，按生命周期与分类落位；若新决策与某条 `D` 编号相关（推翻、收窄、或引用其理由），在笔记正文里点名回链，并在那条 `D` 的「何时该翻案」处补一行指向新笔记。
- 两者**不重复登记同一个决策**。判据：这条决策的首次提出日在 2026-10-17 之前 → 留在 `DECISIONS.md`；之后 → 进笔记树。

**规范副本入库**：`.agents/skills/agent-notes/` 存放 SKILL.md、`scripts/`、`references/`、`templates/` 的副本，随仓库分发 —— 别人 clone 后能直接跑门禁，不必先装外部 skill。副本带一个 `package.json`（`{"type":"module"}`），让 Node 按 ESM 解析脚本、消掉 `MODULE_TYPELESS_PACKAGE_JSON` 警告。

**门禁三条命令**（从仓库根执行，Node 24+ 原生剥离 TS 类型，无需安装任何依赖）：

```sh
node .agents/skills/agent-notes/scripts/verify-agent-note-tree.ts
node .agents/skills/agent-notes/scripts/verify-agent-note-format.ts
node .agents/skills/agent-notes/scripts/verify-archived-agent-notes.ts
```

三者已接进 CI，见 [CI 门禁的范围](./2026-10-17-ci-gates.md)。

**什么必须写**：改了行为、架构、跨文件契约、流程与工具链、测试策略、落盘 / 网络 / 配置格式，或其他维护者日后可能重访的决定。**什么不写**：纯格式化、错别字、无歧义重命名、不改行为的样式与依赖补丁、版本打标、常规 CRUD、单模块内看 diff 即懂的修复。判不准时从严。

**改写纪律**：已有归属的改动就地更新事实（路径、符号、默认值），不追加变更流水账；决定或理由翻转则新开一篇并互链，禁止把一篇的 `## Decision` 改写成反面。

## Alternatives considered

- **全量迁移：把 D01–D17 改写成 17 篇 `implemented` 笔记，`DECISIONS.md` 只留索引。** 最强理由是单一事实来源 —— 两套记录并存迟早分叉，而分叉后没人知道该信哪一份。否掉的原因是改写成本与失真风险：D01–D17 里有大量实测数字（Rust 3.05MB vs Node 93.42MB、831 次请求里 498 次未路由成功）和「何时该翻案」的条件句，改写成笔记骨架时极易把条件句降级成陈述句，反而丢信息；且这一步会同时改动 6 处外部引用。并存分治用「首次提出日」这条硬判据切断了重复登记，不需要动历史语料就能避免分叉。
- **`DECISIONS.md` 继续主用，笔记树只放门禁逼出来的极少数。** 最强理由是它尊重既有文档习惯：`docs/README.md` 的文档地图、`RUNBOOK.md` 的更新触发条件都指向 `DECISIONS.md`，改习惯有认知成本。否掉的原因是它把两套记录变成「主力 + 例外」，例外没有稳定判据，最终会退化成「哪边顺手写哪边」—— 这正是当前漏写的成因。
- **不引入笔记树，把 `DECISIONS.md` 加上脚本校验。** 最强理由是改动最小：只写一个 Markdown 校验脚本，不引入新目录与新规范。否掉的原因是 `DECISIONS.md` 的单文件形态装不下生命周期 —— `proposed` 的提案不该和已落地的决策混在同一份「现行法律」里，`rejected` 需要冻结原样保留，`archived` 需要哈希封印防止篡改。这三件事都要目录结构承载，单文件加校验做不到。

## Consequences

- **收益**：决定记录第一次有了会变红的门禁 —— 目录、文件名、头块、必备节、备选方案、归档封印都能被机械核对；「必须写」的判据从口头纪律变成清单，判不准时从严有了默认答案。生命周期使 `proposed` 与 `implemented` 分居，被否路线能以 `rejected` 形态冻结保留，不必删掉。
- **代价**：仓库多一份 skill 副本（约 190KB 的 Markdown 与 TS），且**副本会与上游 skill 漂移** —— 上游改了校验规则，本仓库不会自动跟进，需要人工同步。这是有意接受的：门禁必须在 clone 后立刻可跑，不能依赖「先装 skill」。
- **代价**：两套记录并存带来的检索成本。找一条决策要先看日期：2026-10-17 之前的在 `DECISIONS.md`，之后在笔记树。判据简单，但新人不会自然知道，所以 `docs/README.md` 与 `docs/AGENTS.md` 都写了这条分界。
- **已知上限**：脚本只校验结构，不校验语义。「`## Alternatives considered` 里写的是不是真考虑过的方案」「`## Decision` 是不是现在时」这两件事永远是人的判断。不要把门禁全绿当成笔记写得好。

## Verification

- 三条命令在仓库根执行均以 0 退出：`node .agents/skills/agent-notes/scripts/verify-agent-note-{tree,format}.ts` 与 `verify-archived-agent-notes.ts`。
- 每条命令都做过故障注入，确认它会变红（注入方式与观察到的报错记在 [CI 门禁的范围](./2026-10-17-ci-gates.md) 的 Verification 节）。
- `node --version` 实测 v24.11.0 与 v25.9.0 都能原生跑这三个 `.ts` 脚本，无需 `tsx`。
