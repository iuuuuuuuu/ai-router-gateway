#!/usr/bin/env node
// 冒烟测试：启动**真实的** gw-server 二进制，等它真的起来，再向 /healthz 拿一次响应，
// 断言 body 恰好是 `ok`，然后关掉。
//
// 为什么需要它：判一个交付物「能跑」不能只看「文件存在 / 体积不为 0 / 编译退出码 0」。
// 一个链接坏了、架构不对、缺动态库的二进制，在体积和退出码上与好的二进制长得一模一样。
// 唯一能证明它的是真的把它跑起来、真的连上去、真的拿到正确响应。
//
// 用法：node tools/smoke-server.mjs <path-to-gw-server>
// 退出码：0 = 起来并答复正确；1 = 起不来或答复不对；2 = 用法错误（调用方要当成失败）。

import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const BIN = process.argv[2];
if (!BIN) {
  console.error('用法: node tools/smoke-server.mjs <path-to-gw-server>');
  process.exit(2);
}

// 数据目录给临时目录：默认路径是「二进制旁边」，在 CI 里可能就是安装目录，不该写。
const dataDir = mkdtempSync(join(tmpdir(), 'gw-smoke-'));
// 默认打真实的 /healthz；允许覆盖仅为「证明断言会红」的故障注入留入口，
// 流水线一律用默认值，不要拿它把门禁指到一个假地址上蒙绿。
// 端口必须跟 server/src/config.rs 的 d_http_addr 一致：改成 8080 就永远打不中
// 新默认端口，冒烟会在「60000ms 内未拿到健康响应」处变红（v0.6.0 前实际踩过）。
const HEALTH = process.env.GW_SMOKE_HEALTH_URL || 'http://127.0.0.1:38080/healthz';
const DEADLINE_MS = 60_000; // 冷启动 + sqlite 初始化，给足；超时就是失败，不放宽。

let log = '';
let child;
let exited = null;

function append(chunk) {
  log += chunk.toString();
  if (log.length > 64_000) log = log.slice(-64_000); // 只留尾部，避免刷爆日志
}

function finish(code, message) {
  if (message) (code === 0 ? console.log : console.error)(message);
  if (exited !== null) {
    // 子进程已经自己退了，不需要再杀
  } else if (child) {
    child.kill('SIGKILL');
  }
  try { rmSync(dataDir, { recursive: true, force: true }); } catch { /* 清理失败不影响结论 */ }
  process.exit(code);
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

try {
  child = spawn(BIN, [], {
    env: { ...process.env, GW_DATA_DIR: dataDir },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
} catch (e) {
  finish(1, `✗ 无法执行 ${BIN}: ${e.message}`);
}

child.stdout.on('data', append);
child.stderr.on('data', append);
child.on('error', (e) => { exited = 'error'; finish(1, `✗ 启动失败: ${e.message}\n--- 输出 ---\n${log}`); });
child.on('exit', (code, signal) => { exited = { code, signal }; });

const deadlineAt = Date.now() + DEADLINE_MS;
let lastErr = '还没试过';
while (Date.now() < deadlineAt) {
  if (exited !== null) {
    finish(1, `✗ 进程提前退出 (code=${JSON.stringify(exited?.code ?? null)}, signal=${JSON.stringify(exited?.signal ?? null)})\n--- 输出 ---\n${log}`);
  }
  try {
    const res = await fetch(HEALTH, { signal: AbortSignal.timeout(2000) });
    const body = (await res.text()).trim();
    if (res.status !== 200) {
      lastErr = `status=${res.status} body=${JSON.stringify(body.slice(0, 200))}`;
    } else if (body !== 'ok') {
      lastErr = `body=${JSON.stringify(body.slice(0, 200))}（应为 "ok"）`;
    } else {
      finish(0, `✓ ${BIN} 启动并在 ${HEALTH} 答复 "ok"`);
    }
  } catch (e) {
    lastErr = e.message;
  }
  await sleep(300);
}

finish(1, `✗ ${DEADLINE_MS}ms 内未拿到健康响应，最后一次: ${lastErr}\n--- 输出 ---\n${log}`);
