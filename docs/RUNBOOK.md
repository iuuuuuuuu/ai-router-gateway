# 运维与发版手册（RUNBOOK）

> 发版、在本机把东西跑起来、按现象排障、回滚的**非显然步骤**。
> 最后核对：2026-10-15。**已发布最新版 = v0.4.6**；HEAD（`0dd8cfa`）上还带着**未发版的 v0.4.7** 改动。

---

## 1. 发布一个版本

### 1.1 发版前置（顺序不能颠倒）

1. **完整实机测试**：本轮 diff 影响到的功能逐条真跑（见 [TESTING.md](TESTING.md)）。**编译过 / 单测绿 / SHA256 对得上都不算实测。**
2. **自审代码逐行确认**。
3. **测试或自审发现问题 → 先修好重测，不许带着问题发。**

### 1.2 版本号要同步改四处（漏一处就出错）

| 文件 | 字段 |
|---|---|
| `server/Cargo.toml` | `version` |
| `client/src-tauri/Cargo.toml` | `version` |
| `client/src-tauri/tauri.conf.json` | `version` |
| `server/Cargo.lock` + `client/src-tauri/Cargo.lock` | 各自包的版本行 |

漏改的后果：产物名（`gw-client_<版本>_x64-setup.exe`）与代码里的版本对不上。

### 1.3 构建顺序

```powershell
# 1) 服务端 release（产物就是 sidecar 的来源）
cd server; cargo build --release

# 2) sidecar：必须用上面那份规范 release 产物覆盖
copy server\target\release\gw-server.exe client\src-tauri\binaries\gw-server-x86_64-pc-windows-msvc.exe

# 3) 客户端 + NSIS 安装包
cd ../client; pnpm tauri build
```

★ **sidecar 必须来自 `cargo build --release`**，不能随手拷一个 exe（历史上提交过非规范产物，见 [DECISIONS.md](DECISIONS.md) D12）。sidecar **入库**，随版本一起提交。

★ 脚本化构建**直接调 JS 入口**（`node node_modules/vite/bin/vite.js build` / `node node_modules/@tauri-apps/cli/tauri.js build`），绕开 `node_modules/.bin` shim（见 [AGENTS.md](AGENTS.md) §2.1）。本机的 `tools/_build-client.sh` 只是这两条命令的封装，**不入库**。

产物位置：`client/src-tauri/target/release/bundle/nsis/gw-client_<版本>_x64-setup.exe`。

### 1.4 发布产物清单

一次发布包含三个文件（与 GitHub Release 一致）：

| 文件 | 说明 |
|---|---|
| `gw-client_<版本>_x64-setup.exe` | 桌面控制台 NSIS 安装包（内含 `gw-server` sidecar） |
| `gw-server-x86_64-pc-windows-msvc.exe` | 仅服务端，裸跑用 |
| `SHA256SUMS.txt` | 上面两个文件的 SHA256 |

`SHA256SUMS.txt` 格式（两空格分隔）：

```
<sha256>  gw-client_0.4.6_x64-setup.exe
<sha256>  gw-server-x86_64-pc-windows-msvc.exe
```

### 1.5 发布后要同步的文档

| 改什么 | 改哪里 |
|---|---|
| 安装包文件名 | `README.md`「下载与安装」表 |
| 新版本能力与版本表 | `docs/方案.md`（版本表 + 新增能力章节） |
| 用户可见行为变化 | `docs/FEATURES.md` |
| 决策变化 | `docs/DECISIONS.md` |
| 发版流程变化 | 本文件 |

---

## 2. 在本机跑起来

### 2.1 只跑服务端（最快）

```powershell
cd server
cargo build --release
$env:GW_DATA_DIR="D:/gw-data"   # 目录（自动接 gateway.db）或直接给 .db 文件路径
.\target\release\gw-server.exe
```

不设 `GW_DATA_DIR` 时，db 落在**可执行文件同级目录**。

### 2.2 完整客户端（带调试口）

```powershell
node tools/start-client.mjs client/src-tauri/target/release 9223
```

★ 这个脚本做了三件**非显然**的事，直接双击 exe 代替不了：

1. **注入 `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=<端口>`** —— 真机验收探针靠它连 CDP。
2. **丢弃 `http_proxy` / `https_proxy` / `no_proxy` / `all_proxy`**（不区分大小写）—— 本机环境块里有大小写重复的代理变量，会拦住本地回环请求。
3. 把 stdout / stderr 重定向到 exe 同级的 `client.log` / `client.err.log`。

客户端会**自动拉起网关**（先探测 9090，已有实例就复用），并把日志写到同目录 `gateway.log`。

### 2.3 冒烟（假上游全链路）

`server/smoke/` 提供假上游（`fake`，监听 **17901**，四个端点各一种行为）与默认配置：

- `server/smoke/config.json` —— 三渠道（死 500 / 好 OpenAI / 好 Anthropic / 好 Gemini）
- `server/smoke/probe.ps1` —— 三个入口 + `/v1/models` 的真实请求
- `server/smoke/put_config.ps1` —— 经管理面灌配置
- `server/smoke/logs.ps1` —— 读切换日志 / 请求日志 / 健康

---

## 3. 按现象排障

### 3.1 先看三个面

| 面 | 地址 | 看什么 |
|---|---|---|
| 管理面 | `http://127.0.0.1:9090/admin/api/config` | 当前生效配置（**热更新后的**，不是库里的） |
| 健康 | `http://127.0.0.1:9090/admin/api/health` | 冷却快照（全量，无分页） |
| 实时并发 | `http://127.0.0.1:9090/admin/api/inflight` | 此刻在承载请求的 (渠道, 模型)，只含非零项 |

日志两处入口：`/admin/api/logs/switch`（每次尝试）与 `/admin/api/logs/requests`（每个客户端请求）。两者都返回 `{ rows, total, limit, offset }`（`limit` 默认 100、上限 1000）。

### 3.2 常见现象 → 先怀疑什么

| 现象 | 先查 | 说明 |
|---|---|---|
| 客户端拿到 502 / 结构化错误 | `logs/switch` 的 `error_class` | 网关按设计**只有**成功或失败两种结局；先确认是哪一类失败 |
| 「保存失败：`group X tier N references missing model Y`」 | 分组层级成员 vs `models` | 悬空引用被**整体 400 拒收**（零字节落库）。客户端删除模型/渠道时会自动摘除引用；直连管理面 PUT 悬空引用**仍会 400**，这是有意的防线 |
| 分组「使用中」高亮不消失 | `/admin/api/inflight` | 高亮只由实时并发驱动；渠道卡片上的「使用中」是**最近成功过**的活跃标记，两者不是一回事 |
| 某渠道一直冷却 | `/admin/api/health` 的归因 | 401/403 = **整渠道隔离**（密钥问题，要重配，不重试不冷却其他渠道）；404 = 只隔离该模型 6h；429/5xx = 指数退避 15s→300s |
| 全部渠道冷却、请求 503 | 响应头 `retry-after` | 值应为**裸秒数**（如 `30`）。历史上曾出现整行 `retry-after: 30` 被当成值插入的畸形（v0.4.6 修复），客户端按 RFC 解析会失败 |
| 首字节很慢 | 设置页 loop guard 开关 / `loop_guard_hold_max_ms` | 开启后首字节推迟 `min(loop_guard_min_chars 个字符（默认 64）, loop_guard_hold_max_ms（默认 250ms）)`。只按字符数等会让慢模型等很久（64 字符在 2 字符/秒 的模型上要 32 秒），把 `hold_max_ms` 调小可强制早提交 |
| 回答被截断 | `logs/requests` 的 `loop_truncated` | 提交后命中退化且纠正不可用 → 就地截断。日志状态是「成功（已截断）」，**不是**完好回答 |
| 回答里有「已纠正复读」 | `loop_continues` | 正常路径：保留了正文并续写。不是错误 |
| 拉取上游模型列表失败 | 弹窗状态区 | 失败**一定**可见地报出来（如 `HTTP 401`），不会假装成功返回空列表 |
| 统计数字对不上 | 价格快照 | `wasted_cost_usd` 只在**本地观测到 usage** 时计价；上游已计费但没解析到 usage 的尝试记 0（「没有证据就不编数」） |

### 3.3 重启与数据

- **配置热更新**：`PUT /admin/api/config` 立即生效（`config_version` 递增）。**监听地址除外，需重启。**
- **进程启动时的派生**：`exposed_models` 会按分组成员重新派生并落库 —— 若发现暴露模型列表与预期不同，先查分组成员本身。
- **in-flight 计数**：每次进程启动都是全新的（请求不可能跨重启存活），所以**不存在**需要恢复的陈旧计数。
- **数据库文件**：`gateway.db`（+ `-wal` / `-shm`）。密钥在 `secrets` 表里，**仍是明文**（见 [DECISIONS.md](DECISIONS.md) D06）。

---

## 4. 回滚

### 4.1 回滚到上一个已发布版本

1. 从 GitHub Releases 下载目标版本的 `gw-client_<版本>_x64-setup.exe`。
2. **备份数据目录**（`gateway.db` 及其 `-wal` / `-shm`）—— 新版本可能已写入新字段。
3. 安装旧版本。旧版本读库时，**手写/旧库的明文密钥原样保留**（`load_config` 只替换值恰好等于占位符的字段），所以降级不会因为密钥存储分离而丢密钥。

★ **数据库不做向下迁移**：降级前**必须**留备份。若新版本改过 schema，旧版本读到不认识的数据时行为未定义。

### 4.2 回滚一次有问题的配置

管理面 `GET /admin/api/config` 拿到当前配置 → 改回 → `PUT` 回去。注意 `PUT` 会做**引用完整性校验**，悬空引用会被整体 400 拒收（此时配置**一个字节都没变**，属于安全失败）。

### 4.3 停掉网关

- 客户端「退出（停止网关）」：会按 `gateway.pid` 停掉网关，且**在拉起它的进程已消失时仍有效**；杀之前用 `tasklist` 校验镜像名，避免 pid 被 Windows 回收后误杀别的程序。
- 裸跑的服务端：直接结束进程即可。

---

## 5. 卸载与清理

- 安装包自带卸载器（NSIS）。
- 卸载**不会**删除数据目录里的 `gateway.db` / 日志 —— 需要彻底清理时手动删，**删前确认路径**（见 [AGENTS.md](AGENTS.md) §5.4）。
- 本机开发期产物（`tools/_rel0*/`、`tools/_shots/`、`*.db`）已在 `.gitignore` 里，**不要**为了「清理」而批量删 —— 先列清单再执行。
