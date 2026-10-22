# Agent Note: 数据目录按平台分流，Windows 保持 exe 同目录不动

Status: implemented

## Problem

`gw-client` 最初只有 Windows 一个目标平台，`gateway.db` 的位置因此可以直接推导：放在客户端 exe 所在目录。这个规则在代码里散落成多处 `current_exe().parent()`，没有任何一处写清「为什么是这里」。

多平台之后这条推导不再成立，而且是**静默失效**的那种：

- macOS 的 exe 在 `gw-client.app/Contents/MacOS/`。整个 `.app` 在升级时被整体替换，写进去的数据库会随下一次升级一起消失。用户看到的现象是「更新一次，所有渠道和分组配置没了」，而不是任何报错。
- Linux 的 AppImage 是只读挂载，exe 所在目录在运行时压根不可写，`Store::open` 直接失败，网关起不来。

也就是说，同一行代码在 Windows 上是对的、在另外两个平台上一个丢数据一个起不来。而它的对错取决于 exe 在哪，这件事本身就说明它不该以 exe 为基准。

另有一处同类假设：拉起 sidecar 时硬编码了 `gw-server.exe`。Tauri 在 Unix 上打包 sidecar 时会去掉 `.exe`，所以这个名字在 macOS / Linux 上指不到任何文件。

## Decision

**数据目录改由 `DATA_DIR: OnceLock<PathBuf>` 在 `setup()` 里一次性解析，之后的取用一律走 `gw_data_dir()`。**

解析规则（`resolve_data_dir`）：

| 平台 | 目录 | 理由 |
|---|---|---|
| Windows（含 debug） | exe 同目录 | 既有安装的布局就是这个，升级不得搬动用户已有数据库 |
| macOS / Linux，release | `app.path().app_data_dir()`（`~/Library/Application Support/com.gawaty.client/`、`~/.local/share/com.gawaty.client/`） | 唯一能在升级后存活、且运行时可写的位置 |
| macOS / Linux，debug | `./data` | 开发运行保持自包含、可随手删，与 Windows 开发路径一致 |

`gw_data_dir()` 在 `DATA_DIR` 未设置时回退到 exe 目录而不是 panic：`DATA_DIR.set()` 只在 `setup()` 里调用一次，任何早于它的取用会拿到旧行为，不会让程序崩在启动路径上。

**sidecar 文件名改由 `gateway_exe_name()` 按 `cfg!(windows)` 给**：Windows 返回 `gw-server.exe`，其余返回 `gw-server`，与 Tauri 打包 sidecar 时的命名一致。

**`windows_subsystem = "windows"` 收窄到 `all(not(debug_assertions), target_os = "windows")`**。原来只判 `not(debug_assertions)`，macOS / Linux 的 release 构建也会尝试套这个属性；它是 Windows 专有的，收窄后才是有意义的声明。

**网关的停止路径同样按平台分流**。原来的孤儿进程清理走 `tasklist` + `taskkill`，这两个在 Unix 上不存在。现在非 Windows 侧用 `ps -p <pid> -o comm=` 取可执行名、用 `kill -TERM` 再 `kill -KILL` 收口。两条路径都保留同一个护栏：**先核对 pid 当前的可执行名确实是 `gateway_exe_name()`，再发信号**，否则 OS 可能已把该 pid 回收给别的进程，直接 kill 会误杀无关程序。

图标与打包配置按平台拆成 `tauri.windows.conf.json` / `tauri.macos.conf.json` / `tauri.linux.conf.json`，基座 `tauri.conf.json` 只留三平台共用的部分、`bundle.targets` 放 `"all"`。这样「Windows 出 nsis、macOS 出 dmg、Linux 出 deb+AppImage」各自就近可读，不必在一个文件里用注释解释条件。

## Alternatives considered

### 为什么不在 macOS / Linux 也用「exe 同目录」？

最强理由是它能让三个平台共用一条路径，逻辑最少，Windows 用户的行为一个字都不用改。否掉的原因是它在两个平台上都不可用，而且失败方式都是静默的：macOS 上目录可写，所以 `Store::open` 会成功，数据真的被写进去，直到下次升级随 `.app` 一起被替换；Linux AppImage 上则直接不可写导致启动失败。让「一个本来只对 Windows 成立的事实」继续当三平台通例，等于把丢数据变成默认行为。

### 为什么不让用户自己配数据目录（环境变量 / 设置项）？

最强理由是彻底把选择权交出去，连「哪个目录才不会被升级清掉」这种平台知识都不用我们猜。否掉的原因是它把一个**正确性默认**降级成了用户配置项：默认值仍然必须选一个，选错了照样丢数据，只是多了一个没人会去改的开关。`GW_DATA_DIR` 本来就存在，留给需要覆盖的高级场景，不当作默认值的替代。

### 为什么不用 `directories` / `dirs` crate 自己算路径？

最强理由是少一层 Tauri 依赖，路径规则也更容易单测。否掉的原因是 Tauri 已经通过 `app.path().app_data_dir()` 暴露了与 `identifier` 绑定的一致路径，自己再算一遍只会多出一处可能与 Tauri 打包布局脱节的实现。路径正确性没法在 Windows 本机测出 macOS / Linux 的结果，少一处自定义逻辑就少一处只能靠 CI 才能发现的分歧。

### 为什么不把 `gw-server` 改成由 Tauri shell plugin 直接管理，省掉 pid 文件？

最强理由是 shell plugin 自带 child 句柄管理与 `Command` API，能省掉手写的 `tasklist` / `ps` 分支。否掉的原因是它不解决本问题的主因：pid 文件的用途是**跨进程**接管——客户端重启过之后，上一条实例拉起的网关还在跑，此时没有任何 `Child` 句柄可用，只有落盘的 pid 能找回它。换成 shell plugin 仍然需要一个 pid 文件，只是多了一个依赖。

## Consequences

- **收益**：macOS / Linux 上的数据库第一次有了能在升级后存活的位置；Windows 升级行为与旧版完全一致，用户无感。
- **收益**：`gateway_exe_name()` 把「Tauri 在 Unix 上剥掉 `.exe`」这条隐式约定写成了一处可查的代码，而不是散在打包脚本的注释里。
- **代价**：终端用户的数据库位置在 macOS / Linux 上是「应用支持目录」而不是程序旁边，排查问题时不能按 Windows 的习惯去找。README 与发布说明里把三个位置列了出来。
- **代价**：非 Windows 的进程终止路径比 `taskkill /F` 多两次系统调用（`kill -0` 轮询、`kill -TERM` 后最多等 1 秒再 `-KILL`）。这是刻意的：SIGTERM 让网关有机会正常关闭 SQLite 句柄，直接 `-KILL` 有机会留下 WAL。
- **已知上限**：`app_data_dir()` 解析失败时回退到 exe 目录——这个回退在 macOS 的 `.app` 里又回到「升级会丢数据」的老问题。它只在 Tauri 路径解析失败时触发，属于「比 panic 好」的兜底，不是正常路径。
- **重访信号**：若将来要求「用户可自选数据目录」，应在此处引入设置项并把默认值保留为平台数据目录，而不是把默认值改回 exe 同目录。

## Verification

- `cd server && cargo test --release --locked`：**202 passed / 0 failed / 0 ignored**，exit 0（数据目录改动不影响服务端测试，重跑确认没有连带破坏）。
- 侧车二进制冒烟（`node tools/smoke-server.mjs`）：真实启动 `gw-server`、真实连 `127.0.0.1:8080/healthz`、断言 body 恰为 `ok`，通过。
- `cargo check` 的故障注入：临时移走 `binaries/gw-server-x86_64-pc-windows-msvc.exe` 后 `cargo check --release` 直接失败并报 `resource path ... doesn't exist`，还原后通过——证明 `externalBin` 的存在性校验是真实的编译期契约，不是装饰。
- **三平台的编译与打包由 CI 覆盖**（Windows / macOS / Linux 各自原生 runner 上编），本机只能证明 Windows 一支；macOS / Linux 的运行时行为仍以 CI 编译通过 + 本机无法复现的诚实边界记录在案，见 [跨平台发布流水线](../process/2026-10-22-cross-platform-release.md)。
