# Agent Note: 轻量模式——关窗销毁界面进程，只留托盘与网关

Status: implemented

## Problem

关窗即隐藏的旧行为把整个 WebView2 留在内存里。实测这台机器上该客户端常驻 6 个 `msedgewebview2` 进程、合计 **437MB**，而用户关窗后真正想留的只有网关（转发请求）和托盘（再打开）。桌面控制台大部分时间处于「窗口关了但后台在跑」的状态，这四百多兆纯属白占。

约束有三条：关窗不能丢掉未保存的工作；托盘必须能把界面叫回来；网关进程不能被连带杀掉。

## Decision

新增**轻量模式**，默认开启，开关在「设置」页与托盘菜单里各一份，落盘在数据目录的 `client-settings.json`（不在 `gateway.db` 里——那是服务端配置，与客户端窗口行为无关）。

关窗时按三件事分流：

| 条件 | 行为 | 结果 |
|---|---|---|
| 轻量模式开 且 无未保存工作 | **放它正常关闭**（不 `prevent_close`） | 窗口与 WebView2 一起退出，省下全部界面内存 |
| 轻量模式开 但有未保存工作 | `prevent_close` + `hide()` | 界面内存不省，但输入不丢 |
| 轻量模式关 | `prevent_close` + `hide()` | 老行为，再打开最快 |

进程不跟着退，靠 `RunEvent::ExitRequested` 里的 `prevent_exit()`；托盘「退出」先立 `QUITTING` 旗标再 `app.exit(0)`，否则退出请求会被自己挡回去。

「有未保存的工作」不能只看 `state.dirty`：分组详情里的层级/设置草稿从不 `markDirty()`，它们是页面局部 `ref`。所以前端加了一份本地草稿登记（`store.ts` 的 `setLocalDraft` / `hasUnsavedWork`），由 `App.vue` 把聚合结果通过 `ui_unsaved` 命令上报给 Rust。Rust 侧该标志**默认 true**：webview 还没报到之前一律按「有脏数据」处理，最坏只是少省一次内存，绝不在不确定时销毁界面。

## 为什么是「正常关闭」而不是 `destroy()`

第一版在后台线程里 `window.destroy()`。实测踩到一个隐蔽竞态：Tauri 对已销毁窗口的登记清理是异步的，紧接着用**同一个 label `main`** 重建窗口时，`get_webview_window("main")` 有时仍返回句柄已死的对象（`show()` 全部 `FailedToSendMessage`），有时清理晚到、把刚建好的同名窗口按 label 从登记表里删掉。两种都表现为「托盘点不动」，日志里只有一串 `Err(FailedToSendMessage)`。

改走 Tauri 自己的关闭路径后，OS 窗口销毁与登记清理在同一次事件处理里完成，重建稳定。重建侧另加一层容错：`ensure_main_window` 只有在 `inner_size()` 可读（句柄还活着）时才复用，否则等 120ms 重试最多 25 次再建——这样即使遇到清理延迟也不会把死句柄当成能用。

## Alternatives considered

- **保持 hide，只把窗口最小化**：改动最小、零风险。最强理由是它对「用户随时想看一眼」的体验最好。否掉的原因是它正是内存问题的成因——隐藏的 WebView2 依然全套驻留，437MB 一分不减。
- **不销毁窗口，只销毁 webview**：最贴近需求字面（「销毁 webview」）。最强理由是窗口壳本身开销很小，留着能免掉重建。否掉的原因很硬：Tauri 2 没有只销毁 webview 而保留窗口的稳定 API，`WebviewWindow` 是窗口与 webview 的一体抽象，能做的最小粒度就是整个窗口。
- **把轻量模式做成总是开启、不给开关**：少一个配置项。最强理由是「关窗省内存」几乎没人会想关掉。否掉的原因是它有可感知代价——再打开要重建界面、重新拉配置、丢掉页面位置；而且万一重建路径在某个平台上有问题，用户需要一个能立刻退回老行为的出口。

## Consequences

- 收益：关窗后界面内存归零（实测 437MB → 0），网关与托盘继续工作；数据面 `/healthz` 全程可用。
- 代价：再打开是**冷启动**——重建 webview、重新 `GET /admin/api/config`、导航回到默认页（页面位置不持久化），比 hide 慢约一秒。
- 代价：有未保存改动时这一轮不省内存。这是刻意的取舍：静默丢草稿比多占几百兆更糟。
- 边界：轻量模式只影响**客户端窗口**。服务端该占多少还是多少，网关进程不会因此变小。
- 边界：`client-settings.json` 与 `gateway.db` / `tray-debug.log` 一样落在 exe 同目录（Windows 的数据目录就是这么定的）。装在 `Program Files` 这类需要提权才能写的目录时，写盘会失败——此时开关只在本次进程内生效，不持久化。这是既有布局带来的限制，不是本功能新增的。
- 新增运维面：`show.flag`（无鼠标叫回界面，顺带让探针能在没有托盘交互的情况下验证重建路径）。

## Verification

探针 `tools/_probe-lightweight.mjs` 与 `tools/_probe-lite-ui.mjs`（本机证据，不入库），全部在真实 release 客户端上跑，不移动鼠标、不抢焦点：关窗用 `CloseMainWindow()`（等价点 ×），重开用 `show.flag`。

| 组 | 断言 | 结果 |
|---|---|---|
| A | 轻量开 + 干净的关窗：客户端存活 / 网关存活 / `/healthz`=ok / webview 进程 0 / CDP 关闭 / 窗口不存在 / 内存 437MB→0 | 12/12 |
| B | 重建：webview 回来 / CDP 可用 / `document.title` 正确 / 配置重新加载 / 网关未被重复拉起 / 窗口真的可见 | 6/6 |
| C | 轻量关：关窗后 webview 进程仍在 / CDP 仍可用 / 窗口转为 hidden | 7/7 |
| D | 有页面级未保存草稿（改分组名，不点保存）：关窗后界面**未被销毁** / 草稿值仍在 / 窗口 hidden | 5/5 |
| E | 设置页开关：存在 / 与磁盘一致 / 点击真的翻转 / 写回 `client-settings.json` / 能翻回 | 5/5 |

A 与 C/D 互为反证：若代码「永远销毁」，C/D 会红；若「永不销毁」，A 会红——两组都绿才说明分支真的按条件走，不是恒绿。

**这次验证抓到的真实缺陷**（已在上面记录并修复）：`destroy()` + 同 label 立即重建导致 `FailedToSendMessage`；以及 `Process.MainWindowHandle` 会被托盘窗口顶替，把「窗口没了」误判成「窗口还在」——探针因此改成按 win32 类名 `Tauri Window` 判定，否则这一组断言会假绿。

未覆盖：多显示器/缩放变化下的重建位置；macOS 与 Linux（本机只有 Windows）。
