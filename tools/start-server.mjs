// 启动 gw-server：过滤掉会导致 PowerShell 环境块重复键的代理变量，并显式指定 GW_DATA_DIR。
import { spawn } from "node:child_process";
import { openSync } from "node:fs";
import { resolve } from "node:path";

const dir = resolve(process.argv[2] || "client/src-tauri/target/release");
const dbPath = resolve(dir, "gateway.db");
const exe = resolve(dir, "gw-server.exe");

const env = {};
let dropped = 0;
for (const [k, v] of Object.entries(process.env)) {
  if (/^(http_proxy|https_proxy|no_proxy|all_proxy)$/i.test(k)) { dropped++; continue; }
  env[k] = v;
}
env.GW_DATA_DIR = dbPath;

const out = openSync(resolve(dir, "server.log"), "w");
const err = openSync(resolve(dir, "server.err.log"), "w");
const p = spawn(exe, [], { cwd: dir, env, detached: true, stdio: ["ignore", out, err] });
p.unref();
console.log(JSON.stringify({ spawned: p.pid, exe, cwd: dir, GW_DATA_DIR: dbPath, droppedProxyVars: dropped }));
