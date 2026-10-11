// 分组详情看板验收探针：打开看板、逐页签取证、验证「打开即自动刷新」与 5s 定时刷新。
// 用法：node tools/cdp-dashboard.mjs [PORT]
import fs from "node:fs";

const PORT = Number(process.argv[2] || process.env.CDP_PORT || 9223);
async function connect(u) {
  return new Promise((res, rej) => {
    const ws = new WebSocket(u); let id = 0; const p = new Map();
    ws.onopen = () => res({ send(m, a = {}) { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: a })); return new Promise((s, j) => p.set(i, { s, j })); }, close: () => ws.close() });
    ws.onerror = () => rej(new Error("ws error"));
    ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && p.has(m.id)) { const { s, j } = p.get(m.id); p.delete(m.id); m.error ? j(new Error(JSON.stringify(m.error))) : s(m.result); } };
  });
}
const list = await (await fetch("http://127.0.0.1:" + PORT + "/json/list")).json();
const c = await connect(list.find(t => t.type === "page").webSocketDebuggerUrl);
const sleep = ms => new Promise(r => setTimeout(r, ms));
const ev = async (x) => { const r = await c.send("Runtime.evaluate", { expression: x, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error(r.exceptionDetails.text + " " + (r.exceptionDetails.exception && r.exceptionDetails.exception.description)); return r.result.value; };
const pt = async (t, x, y) => c.send("Input.dispatchMouseEvent", { type: t, x, y, button: t === "mouseMoved" ? "none" : "left", clickCount: t === "mouseMoved" ? 0 : 1 });
const boxOf = (expr) => ev("(() => { const el = " + expr + "; if (!el) return null; const r = el.getBoundingClientRect(); if (!r.width || !r.height) return null; return { x: r.x + r.width / 2, y: r.y + r.height / 2 }; })()");
const click = async (expr, wait = 600) => { const b = await boxOf(expr); if (!b) return "NOT_FOUND"; await pt("mouseMoved", b.x, b.y); await sleep(60); await pt("mousePressed", b.x, b.y); await sleep(60); await pt("mouseReleased", b.x, b.y); await sleep(wait); return "ok"; };
const shot = async (name) => { const r = await c.send("Page.captureScreenshot", { format: "png" }); fs.writeFileSync(name, Buffer.from(r.data, "base64")); return name; };
const DLG = "document.querySelectorAll('[role=\"dialog\"]')";
const LASTDLG = "[...document.querySelectorAll('[role=\"dialog\"]')].pop()";
const out = {};

// 0) 确保在分组路由页
await click("[...document.querySelectorAll('aside button')].find(x => x.textContent.trim() === '分组路由')", 900);
out.groupDetailBtns = await ev("document.querySelectorAll('[data-test=\"group-detail-btn\"]').length");

// 1) 打开第一个分组的详情看板
out.openClick = await click("document.querySelector('[data-test=\"group-detail-btn\"]')", 1600);
out.dialog = await ev("(() => { const d = " + LASTDLG + "; if (!d) return 'NO_DIALOG'; const r = d.getBoundingClientRect(); const txt = (d.textContent || '').trim(); return JSON.stringify({ x: Math.round(r.x), y: Math.round(r.y), w: Math.round(r.width), h: Math.round(r.height), title: txt.slice(0, 40), buttons: [...d.querySelectorAll('button')].map(b => b.textContent.trim()).filter(s => s.length && s.length < 10).slice(0, 14) }); })()");
out.dashboardMounted = await ev("!!document.querySelector('[data-test=\"group-dashboard\"]')");
out.refreshBtn = await ev("!!document.querySelector('[data-test=\"group-dashboard-refresh\"]')");

// 2) 逐页签点击 + 内容取证
const tabNames = ["概览", "用量", "请求日志", "切换日志", "冷却状态"];
out.tabs = {};
for (const name of tabNames) {
  const r = await click("(() => { const d = " + LASTDLG + "; return d && [...d.querySelectorAll('button')].find(b => b.textContent.trim() === '" + name + "'); })()", 800);
  const info = await ev("(() => { const d = " + LASTDLG + "; if (!d) return 'NO_DIALOG'; const body = d.querySelector('table') ? 'table' : (d.textContent.includes('暂无') || d.textContent.includes('没有') ? 'empty' : 'text'); const rows = d.querySelectorAll('tbody tr').length; const txt = (d.textContent || '').trim(); return JSON.stringify({ click: '" + r + "', body, rows, chars: txt.length, head: txt.slice(0, 160) }); })()");
  out.tabs[name] = info;
}
await shot("w14-dashboard-usage.png");

// 3) 自动刷新：读「最后更新」时间，等 6.5s 再读，必须变化
await click("(() => { const d = " + LASTDLG + "; return d && [...d.querySelectorAll('button')].find(b => b.textContent.trim() === '概览'); })()", 500);
const clockOf = "(() => { const d = " + LASTDLG + "; const t = d ? (d.textContent || '') : ''; const i = t.indexOf('最后更新'); if (i < 0) return 'NO_CLOCK'; const seg = t.slice(i, i + 24); const m = seg.match(/[0-9][0-9]:[0-9][0-9]:[0-9][0-9]/); return m ? m[0] : 'NO_CLOCK'; })()";
out.clock1 = await ev(clockOf);
await sleep(6500);
out.clock2 = await ev(clockOf);
out.autoRefreshWorks = out.clock1 !== "NO_CLOCK" && out.clock2 !== "NO_CLOCK" && out.clock1 !== out.clock2;
out.overviewKeys = await ev("(() => { const d = " + LASTDLG + "; if (!d) return 'NO_DIALOG'; const t = (d.textContent || '').trim(); const i = t.indexOf('密钥'); return i < 0 ? 'NO_KEYS_SECTION' : t.slice(i, i + 90); })()");

// 4) 关闭
await c.send("Input.dispatchKeyEvent", { type: "keyDown", key: "Escape", code: "Escape", windowsVirtualKeyCode: 27 });
await c.send("Input.dispatchKeyEvent", { type: "keyUp", key: "Escape", code: "Escape", windowsVirtualKeyCode: 27 });
await sleep(700);
out.closedDialogs = await ev(DLG + ".length");
out.jsErrors = await ev("JSON.stringify(window.__errs || [])");

console.log(JSON.stringify(out, null, 1));
c.close(); process.exit(0);