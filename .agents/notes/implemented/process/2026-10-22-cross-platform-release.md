# Agent Note: 发布流水线在三个原生 runner 上各出一份包，产物先过冒烟门禁

Status: implemented

## Problem

在 v0.5.0 之前，「发布」这件事只对 Windows 成立：`docs/DECISIONS.md` 的 D14 规定 sidecar 二进制入库并随版本重建，`README.md` 的下载表只有一行 `gw-client_0.4.8_x64-setup.exe`，`tauri.conf.json` 的 `bundle.targets` 写死 `["nsis"]`。

要支持 macOS（Intel + Apple Silicon）与 Linux，光把配置放开是不够的，有三个具体缺口：

1. **产物没法在本机补齐。** Linux 的包需要 webkit2gtk、macOS 的 dmg 需要 `hdiutil` 与 lipo，两者在 Windows 上都跑不出来。任何「本机构建完上传」的做法都只能覆盖三分之一。
2. **入库的 Windows sidecar 不能复用。** D14 让 `binaries/gw-server-x86_64-pc-windows-msvc.exe` 进仓库，是为了本机开发自包含。若打包时复用它，macOS / Linux 的安装包里会塞进一个 Windows PE 文件，客户端在那些平台拉起 sidecar 时必然失败——而且要等到用户安装后才暴露。
3. **「构建成功」与「产物能用」之间没有门禁。** Tauri 打包成功只说明打包器没报错；一个链接坏掉的二进制、一个空壳 dmg、一个集成了错误架构 sidecar 的 `.app`，在退出码和文件体积上都与正常产物难以区分。

## Decision

新增 `.github/workflows/release.yml`，在 `v*` 标签推送（或手动 `workflow_dispatch` 指定标签）时，于**三个原生 runner 各编一份**：

| runner | target | 产出 |
|---|---|---|
| `windows-latest` | `x86_64-pc-windows-msvc` | `_x64-setup.exe`（nsis）+ `gw-server.exe` |
| `macos-latest` | `universal-apple-darwin` | `_universal.dmg` + `gw-server` |
| `ubuntu-22.04` | `x86_64-unknown-linux-gnu` | `.deb` + `.AppImage` + `gw-server` |

**sidecar 一律现编，不读仓库里那份**。每个 job 先 `cargo build --release --locked` 服务端，再按 Tauri 的命名约定放到 `client/src-tauri/binaries/gw-server-<triple>`（Windows 带 `.exe`）。

**macOS 的 sidecar 必须自己也是 universal 的**：分别编 `aarch64-apple-darwin` 与 `x86_64-apple-darwin`，再用 `lipo -create` 合成 `gw-server-universal-apple-darwin`。只把 app 做成 universal、sidecar 留单架构，会让一半用户装得上但网关起不来（`bad CPU type`）。

**发布前有两道真实门禁**：

1. `verify-version` job 比对标签、`tauri.conf.json`、两份 `Cargo.toml` 的版本号，四者不一致直接失败。错配的标签会在发布页上显示成正确版本，装到用户机器上才发现不对。
2. 每个平台的产物先跑 `node tools/smoke-server.mjs <sidecar>`：**真的启动、真的连 `127.0.0.1:8080/healthz`、断言响应体恰为 `ok`**。它排在打包之前，坏二进制不浪费后面十几分钟的 bundling。

**产物收集后按平台的已知正确数量与最小体积校验**（Windows 2 个、macOS 2 个、Linux 3 个；每个 ≥ 1 MB）。数量不符即失败——少了说明某个打包目标没产出，多了说明收集到了不该发的东西。

**Linux 固定跑 `ubuntu-22.04` 而不是 `ubuntu-latest`**：22.04 的 glibc 较旧，编出来的二进制能在更新的发行版上运行；反过来不行。

**CI 侧新增 `client-check` job（3 个 OS 各跑 `cargo check`）**，见 [CI 只跑等价复现的门禁](2026-10-17-ci-gates.md) 中「服务端真要支持 Linux 时再加 Linux job」这条重访信号的兑现。

## Alternatives considered

### 为什么不在一个 Windows runner 上交叉编译出三个平台的包？

最强理由是只需要一条流水线、一份缓存，运行时间最短，而且本机就能复现 CI 的完整过程。否掉的原因是 macOS 的 dmg 制作依赖 `hdiutil`（只有 macOS 有）、universal 合成依赖 `lipo`、签名依赖 `codesign`，三者都无法在 Windows 上替代；Linux 的 AppImage 打包要在目标环境里收集 `webkit2gtk` 等原生库。交叉编译能产出二进制，产出不了**安装包**，而用户要的是安装包。

### 为什么不复用入库的 Windows sidecar 二进制？

最强理由是省掉每个平台一次 `cargo build --release`，而且能保证 sidecar 与开发者本机跑的是同一个文件——D14 当初让 sidecar 入库就是这个动机。否掉的原因是它把「Windows 产物」和「跨平台产物」绑在一起：macOS / Linux 的包里会出现 PE 文件。入库那份现在只服务于本机开发，CI 一律现编；两者职责分开，也避免了「CI 的产物取决于某个二进制是什么时候被谁提交的」这种不可追溯状态。

### 为什么冒烟门禁只测 `/healthz`，不测数据面转发？

最强理由是数据面转发才是产品本体，只测健康检查等于只测了「进程能起来」。否掉的原因是数据面断言需要真实上游与真实密钥，属于本机真机验收的范畴（见 `docs/TESTING.md`），把含凭据的探针搬进公开仓库的 CI 会泄密。`/healthz` 在这里的职责很窄：证明**这个文件在目标平台上确实能作为进程运行并响应网络请求**，它挡的是架构错、动态库缺、链接坏这三类打包事故，不冒充功能测试。

### 为什么版本校验放在最前面的独立 job，而不是在每个 build job 里各做一次？

最强理由是独立 job 只花几秒，而 build job 要十几分钟——先卡版本能避免三个平台白跑。否掉的是「在每个 build job 里内联校验」：那样版本不一致时三个 build job 会同时开始下载依赖、装系统包，然后才失败，浪费与噪音都更大。代价是 build job 多了一个 `needs`，串行多出几秒。

## Consequences

- **收益**：三个平台的安装包第一次由流水线统一产出，不再依赖「谁的机器上有什么工具链」。发一次版＝推一个标签。
- **收益**：sidecar 与 app 的架构一致性由构造保证（同一 job 内现编），不再是一个需要人工记得的隐性约束。
- **收益**：冒烟门禁把「打包成功但装不上」的一类事故提前到打包之前暴露，且成本只有几秒。
- **代价**：一次发版要吃三个 runner，macOS runner 的分钟数是 Linux 的 10 倍计费，universal 构建还要编两遍 Rust（两个架构各一次）。这是有意的：省下的时间会以「某个平台装不上」的形式还回来。
- **代价**：`release.yml` 与 `ci.yml` 各自维护一份「装系统依赖」的步骤，两处需要同步。没有抽 composite action，是因为两处实际要装的东西不同（CI 只跑 check，release 还要打 AppImage），抽出去会变成一个带大量条件参数的动作。
- **已知上限（诚实边界）**：**产物未做代码签名与 Apple 公证。** macOS 上用户首次打开会遇到 Gatekeeper 拦截，Windows 上会遇到 SmartScreen 提示。这不是遗漏，是没有证书；发布说明里直接写明了绕过步骤，不让用户在「打不开」上猜。
- **已知上限**：CI 只证明产物**构建成功且 sidecar 能跑**，不证明 GUI 在 macOS / Linux 上交互正常。GUI 真机验收仍按 `docs/TESTING.md` 在本机 Windows 上做，macOS / Linux 的界面行为目前没有自动化覆盖。
- **重访信号**：若拿到 Apple Developer ID 与 Windows 代码签名证书，签名步骤应加在 `tauri build` 之前的环境变量注入，而不是事后 `signtool` 补签。

## Verification

- **本地 Windows 全链路实跑**：`cargo test --release --locked` 202 passed / 0 failed；sidecar 冒烟通过；`pnpm tauri build --target x86_64-pc-windows-msvc` 产出 nsis 安装包，体积与清单见发布记录。
- **冒烟门禁的故障注入**（四类，全部变红 exit 1）：路径不存在（`spawn ENOENT`）、文件存在但不可执行（`EFTYPE`）、进程立刻退出、以及**返回 200 但 body 不是 `ok`**（`body="nope"（应为 "ok"）`）。最后一条是关键：它证明断言的是响应内容，不是「端口能连上」。
- **版本校验的判据**：四份版本号（标签 / `tauri.conf.json` / 两份 `Cargo.toml`）由 job 比对，任一不一致即 `::error::` 退出。
- **`--locked` 一致性**：两份 `Cargo.lock` 已同步到 0.5.0，`cargo metadata --locked` 在两个 crate 上均 exit 0。
- **GitHub Actions 实跑**：见 v0.5.0 的 release 工作流运行记录（三个 build job + 版本校验 + 发布 job）。
