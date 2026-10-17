# Agent Notes 规范（本仓库的安装副本）

本目录是 [DeepSeek Agent Notes](https://github.com/deepseek-ai/deepseek-harness) 规范 skill 的**逐字副本**，
随仓库分发，目的是让别人 clone 后不必先装 skill 就能跑门禁。上游 `SKILL.md` 与 `references/` 里的命令示例
写的是 `.agents/skills/write-notes-like-deepseek/` —— **那是上游的默认目录名，本仓库不用它**。
本仓库的路径就是本目录，对应命令：

```bash
node .agents/skills/agent-notes/scripts/verify-agent-note-tree.ts
node .agents/skills/agent-notes/scripts/verify-agent-note-format.ts
node .agents/skills/agent-notes/scripts/verify-archived-agent-notes.ts
```

Node 24+ 原生剥离 TS 类型，**不需要 `npm install`、也不需要 `tsx`**（仓库根没有 `package.json`）。
本目录下的 `package.json` 只声明 `"type": "module"`，用来消掉 Node 的 ESM 解析警告，不是依赖清单。

## 与上游的差异

| 差异 | 原因 |
|---|---|
| 多了本 `README.md` 与 `package.json` | 说明本地路径 + 声明 ESM，见上 |
| 未包含上游 `assets/`（决策看板 HTML 快照） | 那是上游自己的展示物，与本仓库无关；`scripts/build-board.ts` 仍可用来自建 |
| `templates/verify-notes.yml` 保留但**未使用** | 本仓库的 CI 是 `.github/workflows/ci.yml`，范围与模板不同（多了元门禁、少了 tsx），见 [CI 门禁笔记](../../notes/implemented/process/2026-10-17-ci-gates.md) |

## 升级这份副本

上游改了校验规则时，把 `SKILL.md` / `scripts/` / `references/` / `templates/` 整份覆盖过来，
然后跑一遍 `node tools/verify-notes-gates.mjs` —— 它会逐条故障注入证明新脚本仍然会变红。
**不要手工改这里的脚本**，改了就和上游分叉，下次升级会冲突。
