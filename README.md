# AI 模型路由网关（ai-auto-repety-gawaty）

多渠道多模型 AI 网关：分组多级容灾路由 + 三协议互转 + 成本统计 + 完整日志。

## 结构

- `server/` — Rust 网关（axum + reqwest + rusqlite），产出单文件 `gw-server.exe`（约 5MB）
- `client/` — Tauri 2 + Vue 3 桌面控制台，托管网关进程（sidecar）、开机自启（默认关）
- `方案.md` — 架构设计与实施进度

## 网关端口

| 面 | 端口 | 说明 |
|---|---|---|
| 数据面 HTTP | 8080 | `/v1/chat/completions` `/v1/messages` `/v1beta/models/{m}:generateContent` `/v1/models` `/healthz` |
| 数据面 HTTPS | 8443 | 同上，内置自签证书（localhost/127.0.0.1） |
| 管理面 | 127.0.0.1:9090 | `/admin/api/*`（config/logs/stats/health/test-channel/cooldown） |

## 快速开始（服务端裸跑）

```powershell
cd server
cargo build --release
$env:GW_DATA_DIR="D:/data"   # 目录或 gateway.db 文件路径
./target/release/gw-server.exe
```

配置持久化在 SQLite；首次启动用默认配置，之后通过管理面 PUT `/admin/api/config`（或客户端 UI）修改并热更新。

## 客户端开发

```powershell
cd client
pnpm install
pnpm tauri dev    # 桌面窗口 + 自动拉起网关
pnpm tauri build  # NSIS 安装包 + sidecar 打包
```

客户端 UI 页面：渠道（含透传 headers、连通测试=真实请求）、模型绑定（暴露名→上游名、四类价格）、分组路由（一级/二级/… 成员顺序——拖动或按钮排序、与自动权重开关）、设置（重试/超时/监听/自启开关/自动权重）、日志（切换日志按 request_id 过滤 + 请求日志）、统计（总览/按模型成本/冷却状态）。

窗口会记住上次的大小与位置；开机自启默认关闭，开启后静默驻留托盘。

## 路由语义

- 分组内 tier 1 → 2 → … 升序尝试；同 tier 内**按列表顺序**依次尝试（默认）
- 自动权重（`auto_weight`，默认关闭）：开启后同 tier 内改为按成员权重降序，权重相同则保持列表顺序
- 「分组路由」页里成员的**顺序**和**权重**（数字输入框）在开关的两种状态下都能改：关闭时权重照常存盘但暂不参与调度，开启后立即生效，可以先批量填好权重再一次性开启
- 排序两种方式等价：按住成员行首的抓手**拖动**，或用**上移 / 下移**按钮（键盘与读屏可用）；层头的抓手可以拖动**层级先后**，拖到哪一级，层级号就跟到哪一级
- 候选失败 → 组内下一个；一级全冷却 → 下一级；末级也死 → 回到一级，循环 `max_cycles` 轮
- 全部冷却时 `cooldown_wait=true` 会等到最近一个冷却恢复再试（不白白失败）
- 健康 (channel × model) 粒度：401/403 整渠道隔离，404 只隔离该模型 6h，429/5xx 指数退避 15s→300s
- 提交点：流式请求在收到第一个合法内容帧后才向客户端写首字节（I1：提交前零字节），失败切换对客户端完全透明
- 中途断流：网关补 repair 帧（`gateway_upstream_interrupted`）+ 正常终止符，客户端永远拿到结构完整的响应

## 下载与安装

最新版本见 [Releases](https://github.com/iuuuuuuuu/ai-router-gateway/releases)。

| 文件 | 说明 |
|---|---|
| `gw-client_0.2.2_x64-setup.exe` | 桌面控制台 NSIS 安装包（内含 `gw-server` sidecar，安装后托盘常驻、可开机自启） |
| `gw-server-x86_64-pc-windows-msvc.exe` | 仅服务端，裸跑用（见上「快速开始」） |
| `SHA256SUMS.txt` | 上述文件的 SHA256 校验和 |

## 许可证

[MIT](LICENSE) © 2026 杨鹏豪
