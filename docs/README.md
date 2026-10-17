# 文档地图

> 本页说明这个目录里每份文档「为什么存在、什么时候该翻它」。**改文档结构时同步更新本页。**
> 最后核对：2026-10-17。**已发布最新版 = v0.4.8**（tag 与 GitHub Release 都存在）。

## 怎么找文档

| 我想…… | 翻这份 |
|---|---|
| **开始动手改代码** | [AGENTS.md](AGENTS.md) —— 开发入口，开工第一件事 |
| 知道「为什么当初这么选」/ 想推翻旧结论 | [DECISIONS.md](DECISIONS.md) |
| 发版、在本机把客户端跑起来、按现象排障、回滚 | [RUNBOOK.md](RUNBOOK.md) |
| 写测试 / 跑验收 / 怀疑门禁是不是假的 | [TESTING.md](TESTING.md) |
| 改用户能看见的行为之前对齐语义 | [FEATURES.md](FEATURES.md) |
| 理解整体设计取舍与逐版本演进 | [方案.md](方案.md) |
| 碰死循环拦截（loop guard） | [loopguard-spec.md](loopguard-spec.md) |
| 碰思考（reasoning / thinking）字段 | [reasoning-passthrough-spec.md](reasoning-passthrough-spec.md) |
| 复查历史审查结论 | [review/](review/) —— 三篇：技术选型与验收基线 / 工程落地与生产就绪 / 事故复盘 |
| 改任何界面 | [../client/UI-SPEC.md](../client/UI-SPEC.md) —— 设计令牌、通用组件、交互规范 |

## 文档清单

| 文档 | 定位 | 更新触发 |
|---|---|---|
| [AGENTS.md](AGENTS.md) | 开发入口：环境与命令、硬约束、工作流、验证纪律 | 命令/约束/流程变化时 |
| [DECISIONS.md](DECISIONS.md) | 决策记录：为何选它、排除了什么、何时该翻案 | 做出或推翻一项决策时 |
| [RUNBOOK.md](RUNBOOK.md) | 发版、本机运行、按现象排障、回滚 | 发布流程或排障手法变化时 |
| [TESTING.md](TESTING.md) | 四层测试、假上游矩阵、真机验收探针约定 | 测试体系变化时 |
| [FEATURES.md](FEATURES.md) | 用户可见行为规格（路由、分组、报错、统计口径） | 行为变化时 |
| [方案.md](方案.md) | 架构设计与实施进度（含逐版本新增能力） | 每个版本发布时 |
| [loopguard-spec.md](loopguard-spec.md) | 死循环输出拦截规格（判定语义、诚实边界） | 改 loopguard 时 |
| [reasoning-passthrough-spec.md](reasoning-passthrough-spec.md) | 思考透传规格（同协议回放 / 跨协议翻译 / 边界） | 改思考字段时 |
| [review/](review/) | 三篇历史审查（选型、落地、事故复盘） | **只追加不改写**；结论过时在 DECISIONS.md 里翻案 |

## 写文档的约定

- **只写代码里读不出来的**：为什么这样做、约束、陷阱、验收标准、非显然步骤。不复述代码结构。
- **每条结论带日期 + 适用版本或 commit 范围**；过期即删，不留「历史遗留」章节。
- 文档与代码**同一次提交**更新；改结构同步本页索引。
- 判据：**别人 clone 到就成立** → 进仓库文档；**只对本机成立**（本机路径、探针产物、真实密钥）→ 不进仓库（见 AGENTS.md §5.3）。
