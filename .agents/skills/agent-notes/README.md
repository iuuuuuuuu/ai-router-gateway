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

**当前同步版本：`.synced-version` = `0.4.1`（2026-10-18）**。同步方式见文末「升级这份副本」。

## 与其他版本的差异

**与上游**：

| 差异 | 原因 |
|---|---|
| 多了本 `README.md`、`package.json`、`.synced-version` | 说明本地路径 + 声明 ESM + 记录同步版本，见上 |
| 未包含上游 `assets/`（决策看板 HTML 快照） | 那是上游自己的展示物，与本仓库无关；`scripts/build-board.ts` 仍可用来自建 |
| `templates/verify-notes.yml` 保留但**未使用** | 本仓库的 CI 是 `.github/workflows/ci.yml`，范围与模板不同（多了元门禁、少了 tsx），见 [CI 门禁笔记](../../notes/implemented/process/2026-10-17-ci-gates.md) |

**相对 0.4.0 之前（本仓库曾同步的旧版）**，0.4.1 带来的行为变化 —— 旧版会把下列合法写法判红：

| 变化 | 旧版行为 | 新版行为 |
|---|---|---|
| 支持 CommonMark 尖括号链接目标（目标两端包尖括号，路径含空格时必需） | 不剥尖括号，对已存在文件也报死链 | 剥掉后正常解析 |
| `## 后果与边界` | 不认作 `Consequences`，报缺节 | 认作合法别名 |
| `## 其他做法` | 不认作备选方案节，报缺节 | 认作合法别名 |
| `archived/AGENTS.md` | 当成归档笔记校验头部布局，必然报错 | 跳过，不校验 |
| 写完笔记必须跑验证 | 未强制 | `SKILL.md` 明确要求，不得以「没配校验入口」为由跳过 |
| 正文语言 | 未规定 | 一律简体中文（代码、命令、专有名词除外） |

元门禁 `tools/verify-notes-gates.mjs` 已为前四条各配正反用例，并用历史版本脚本实证「旧版红、新版绿」。

## 升级这份副本

上游改了校验规则时：

1. 把上游的 `SKILL.md` / `scripts/` / `references/` / `templates/` 整份覆盖过来；
2. 把上游的 `.synced-version` 一并复制过来（它记录同步版本）；
3. 跑 `node tools/verify-notes-gates.mjs` —— 逐条故障注入，证明新脚本仍然会变红；
4. 若上游新增了校验行为，给元门禁补正反用例，否则新逻辑**未被证明**就被信任；
5. 跑三条笔记门禁确认现有笔记在新规则下仍为绿。

**不要手工改这里的脚本**，改了就和上游分叉，下次升级会冲突。
