// 启动 gw-client（Tauri 客户端），带上 WebView2 的 CDP 调试端口。
// 用 node 启动是为了绕开 PowerShell 环境块里大小写重复的代理变量问题。
import { spawn } from "node:child_process";
import { openSync } from "node:fs";
import { resolve } from "node:path";

const dir = resolve(process.argv[2] || "client/src-tauri/target/release");
const port = process.argv[3] || "9223";
const exe = resolve(dir, "gw-client.exe");

const env = {};
let dropped = 0;
for (const [k, v] of Object.entries(process.env)) {
  if (/^(http_proxy|https_proxy|no_proxy|all_proxy)$/i.test(k)) { dropped++; continue; }
  env[k] = v;
}
env.WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=" + port;

const out = openSync(resolve(dir, "client.log"), "w");
const err = openSync(resolve(dir, "client.err.log"), "w");
const p = spawn(exe, [], { cwd: dir, env, detached: true, stdio: ["ignore", out, err] });
p.unref();
console.log(JSON.stringify({ spawned: p.pid, exe, cwd: dir, cdpPort: port, droppedProxyVars: dropped }));
