# AI 模型路由网关（ai-auto-repety-gawaty）

多渠道多模型 AI 网关：分组多级容灾路由 + 三协议互转 + 成本统计 + 完整日志。

**核心承诺**：模型失败 = 网关的内部事务，客户端永远无感。只有两种互斥结局——**成功**（完整内容 + 完整 usage）或**失败**（一个结构合法的标准错误），中间态即 bug。

## 结构

- `server/` — Rust 网关（axum + reqwest + rusqlite），产出单文件 `gw-server.exe`（约 5MB）
- `client/` — Tauri 2 + Vue 3 桌面控制台，托管网关进程（sidecar）、开机自启（默认关）
- `docs/` — 设计与规格文档，见下「文档索引」
- `tools/` — 开发期探针与构建脚本（绝大多数不入库）

## 快速开始（服务端裸跑）

```powershell
cd server
cargo build --release
$env:GW_DATA_DIR="D:/data"   # 目录（自动接 gateway.db）或 gateway.db 文件路径
./target/release/gw-server.exe
```

配置持久化在 SQLite；首次启动用默认配置，之后通过管理面 `PUT /admin/api/config`（或客户端 UI）修改并热更新（监听地址除外，需重启）。

## 客户端开发

```powershell
cd client
pnpm install
pnpm tauri dev    # 桌面窗口 + 自动拉起网关
pnpm tauri build  # NSIS 安装包 + sidecar 打包
```

带调试口启动（真机验收探针需要）：`node tools/start-client.mjs client/src-tauri/target/release 9223`

## 测试

```powershell
cd server
cargo test --release    # 主门禁
```

用户可见行为（路由语义、分组、报错文案、统计口径、界面约定）见 [docs/FEATURES.md](docs/FEATURES.md)；测试体系与真机验收约定见 [docs/TESTING.md](docs/TESTING.md)。

## 网关端口

| 面 | 端口 | 说明 |
|---|---|---|
| 数据面 HTTP | 8080 | `/v1/chat/completions` `/v1/messages` `/v1beta/models/{m}:generateContent` `/v1/models` `/healthz` |
| 数据面 HTTPS | 8443 | 同上，内置自签证书（localhost/127.0.0.1） |
| 管理面 | 127.0.0.1:9090 | `/admin/api/*`（config/logs/stats/health/test-channel/cooldown） |

## 文档索引

| 我想…… | 翻这份 |
|---|---|
| **开始动手改代码** | [docs/AGENTS.md](docs/AGENTS.md) —— 开发入口（环境命令、硬约束、工作流、验证纪律） |
| 知道「为什么当初这么选」/ 想推翻旧结论 | [docs/DECISIONS.md](docs/DECISIONS.md) |
| 发版、本机跑客户端、按现象排障、回滚 | [docs/RUNBOOK.md](docs/RUNBOOK.md) |
| 写测试 / 跑验收 / 怀疑门禁是不是假的 | [docs/TESTING.md](docs/TESTING.md) |
| 看用户可见行为规格 | [docs/FEATURES.md](docs/FEATURES.md) |
| 理解整体设计与逐版本演进 | [docs/方案.md](docs/方案.md) |
| 碰死循环拦截 | [docs/loopguard-spec.md](docs/loopguard-spec.md) |
| 碰思考（reasoning / thinking）字段 | [docs/reasoning-passthrough-spec.md](docs/reasoning-passthrough-spec.md) |
| 复查历史审查结论 | [docs/review/](docs/review/)（技术选型 / 工程落地 / 事故复盘 三篇） |
| 改任何界面 | [client/UI-SPEC.md](client/UI-SPEC.md) |

完整文档地图（含每份文档的更新触发条件与写文档约定）：[docs/README.md](docs/README.md)。

## 下载与安装

最新版本见 [Releases](https://github.com/iuuuuuuuu/ai-router-gateway/releases)。

| 文件 | 说明 |
|---|---|
| `gw-client_0.4.8_x64-setup.exe` | 桌面控制台 NSIS 安装包（内含 `gw-server` sidecar，安装后托盘常驻、可开机自启） |
| `gw-server-x86_64-pc-windows-msvc.exe` | 仅服务端，裸跑用（见上「快速开始」） |
| `SHA256SUMS.txt` | 上述文件的 SHA256 校验和 |

## 许可证

[MIT](LICENSE) © 2026 杨鹏豪
