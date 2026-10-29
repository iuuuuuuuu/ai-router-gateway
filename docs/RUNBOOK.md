# 运维与发版手册（RUNBOOK）

> 发版、在本机把东西跑起来、按现象排障、回滚的**非显然步骤**。
> 最后核对：2026-10-29。**已发布最新版 = v0.6.0**（tag 与 GitHub Release 都存在）。

---

## 1. 发布一个版本

### 1.1 发版前置（顺序不能颠倒）

1. **完整实机测试**：本轮 diff 影响到的功能逐条真跑（见 [TESTING.md](TESTING.md)）。**编译过 / 单测绿 / SHA256 对得上都不算实测。**
2. **自审代码逐行确认**。
3. **测试或自审发现问题 → 先修好重测，不许带着问题发。**
4. **确认要发版的那个提交上 CI 是绿的**（`gh run list`）。不要从一个 CI 红的提交上打标签。
5. **只有 Windows 能在本机完整验证**：macOS / Linux 的安装包必须在 CI 的原生 runner 上产出，本机跑不出来（Linux 需要 webkit2gtk，macOS 的 dmg 需要 `hdiutil` 与 `lipo`）。所以「本机实测通过」只覆盖 Windows，另外两个平台的保证上限是 CI 的构建 + 冒烟门禁，见 [跨平台发布流水线](../.agents/notes/implemented/process/2026-10-22-cross-platform-release.md) 的诚实边界一节。

### 1.2 版本号要同步改四处（漏一处就出错）

| 文件 | 字段 |
|---|---|
| `server/Cargo.toml` | `version` |
| `client/src-tauri/Cargo.toml` | `version` |
| `client/src-tauri/tauri.conf.json` | `version` |
| `server/Cargo.lock` + `client/src-tauri/Cargo.lock` | 各自包的版本行 |

漏改的后果：产物名（`gw-client_<版本>_x64-setup.exe`）与代码里的版本对不上。这四处由 release 流水线的 `verify-version` job 自动比对，任一不一致直接失败——但**不要依赖它兜底**，本地先改齐。

### 1.3 构建顺序（本机，仅 Windows）

```powershell
# 1) 服务端 release（产物就是 sidecar 的来源）
cd server; cargo build --release

# 2) sidecar：必须用上面那份规范 release 产物覆盖
copy server\target\release\gw-server.exe client\src-tauri\binaries\gw-server-x86_64-pc-windows-msvc.exe

# 3) 客户端 + NSIS 安装包
cd ../client; pnpm tauri build
```

★ **sidecar 必须来自 `cargo build --release`**，不能随手拷一个 exe（历史上提交过非规范产物，见 [DECISIONS.md](DECISIONS.md) D12）。sidecar **入库**，随版本一起提交——入库那份**只服务于本机开发**，CI 打包时一律现编，不复用它。

★ 脚本化构建**直接调 JS 入口**（`node node_modules/vite/bin/vite.js build` / `node node_modules/@tauri-apps/cli/tauri.js build`），绕开 `node_modules/.bin` shim（见 [AGENTS.md](AGENTS.md) §2.1）。本机的 `tools/_build-client.sh` 只是这两条命令的封装，**不入库**。

★ 本机 pnpm 版本注意：`client/package.json` 的 `pnpm.onlyBuiltDependencies` 只有 **pnpm 10** 读，pnpm 11 会忽略它并让 esbuild 的安装脚本被拦（`ERR_PNPM_IGNORED_BUILDS`）。发版相关构建用 pnpm 10。

产物位置：`client/src-tauri/target/release/bundle/nsis/gw-client_<版本>_x64-setup.exe`。

### 1.3.1 三平台发版（正式流程）

正式发布**不靠本机打包**，而是打标签让 CI 在三个原生 runner 上产出：

```bash
git tag -a v0.5.0 -m "v0.5.0 —— <一句话>"
git push origin v0.5.0
gh run watch --workflow=Release
```

流水线（`.github/workflows/release.yml`）依次做：`verify-version` 比对四处版本 → 三个 build job（windows-latest / macos-latest / ubuntu-22.04）各自现编 sidecar、**冒烟测试 sidecar**、打包、断言产物数量与体积 → `publish` 生成 `SHA256SUMS.txt` 并建 Release。

**macOS 的 sidecar 必须准备三份**：`gw-server-aarch64-apple-darwin`、`gw-server-x86_64-apple-darwin` 和 lipo 出来的 `gw-server-universal-apple-darwin`。`tauri build --target universal-apple-darwin` 会按两个架构各走一遍构建，每遍的 build script 按自己那个 triple 找 externalBin，缺一个就在 `resource path ... doesn't exist` 处失败。

**标签打错了怎么办**：只要 `publish` job 没跑成功（没有产生 Release），可以删远端标签重打：

```bash
git push origin :refs/tags/v0.5.0 && git tag -d v0.5.0
git tag -a v0.5.0 -m "..." <正确的提交> && git push origin v0.5.0
```

**一旦 Release 已经建出来，不要再移动标签**——已发布的产物和标签必须对得上，改内容就发新版本号。

### 1.4 发布产物清单

一次发布包含 8 个文件（三平台，与 GitHub Release 一致）：

| 文件 | 平台 | 说明 |
|---|---|---|
| `gw-client_<版本>_x64-setup.exe` | Windows 10/11 x64 | NSIS 安装包（内含 `gw-server.exe`） |
| `gw-client_<版本>_universal.dmg` | macOS 11+ | universal 包，Intel 与 Apple Silicon 同一个文件 |
| `gw-client_<版本>_amd64.deb` | Linux x64 | Debian/Ubuntu 安装包 |
| `gw-client_<版本>_amd64.AppImage` | Linux x64 | 免安装可执行 |
| `gw-server-x86_64-pc-windows-msvc.exe` | Windows | 仅服务端，裸跑用 |
| `gw-server-universal-apple-darwin` | macOS | 仅服务端，universal |
| `gw-server-x86_64-unknown-linux-gnu` | Linux | 仅服务端 |
| `SHA256SUMS.txt` | — | 上面七个文件的 SHA256 |

`SHA256SUMS.txt` 格式（两空格分隔）：

```
<sha256>  gw-client_0.5.0_x64-setup.exe
<sha256>  gw-server-x86_64-pc-windows-msvc.exe
```

**三个平台都未做代码签名 / Apple 公证**：Windows 会弹 SmartScreen，macOS 会弹 Gatekeeper。绕过步骤写在 Release 说明里，不要删。

### 1.5 发布后要同步的文档

| 改什么 | 改哪里 |
|---|---|
| 安装包文件名 | `README.md`「下载与安装」表 |
| 新版本能力与版本表 | `docs/方案.md`（版本表 + 新增能力章节） |
| 用户可见行为变化 | `docs/FEATURES.md` |
| 决策变化（2026-10-17 之前） | `docs/DECISIONS.md` |
| 决策变化（2026-10-17 起） | `.agents/notes/`（见 [AGENTS.md](AGENTS.md) §5.5） |
| 发版流程变化 | 本文件 |
| 各文档页首的「已发布最新版」 | 六份 docs 页首 + `docs/方案.md` 的版本记录表 |

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

**无鼠标操作窗口的两个旗标**（探针与环境里没有托盘也能用）：在 exe 同目录放文件即可，客户端每 500ms 轮询一次、处理后自己删掉。

| 文件 | 效果 |
|---|---|
| `show.flag` | 显示主界面。轻量模式下窗口已被销毁，这个动作会把界面**重建**出来（等同托盘「显示主界面」） |
| `quit.flag` | 优雅退出：停掉网关子进程后整个客户端一起退出 |

轻量模式（默认开）下关闭窗口会**销毁**界面进程，只剩托盘与网关；此时 CDP 端口也会一起消失，探针请在放 `show.flag` 之后再连。开关在「设置」页与托盘菜单里都有，落盘在 exe 同级的 `client-settings.json`。

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
