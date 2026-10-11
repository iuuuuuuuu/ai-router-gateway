// Console-error + runtime-health check on the real window.
import fs from "node:fs/promises";
const PORT = Number(process.argv[2] || 9223);
async function connect(u) { return new Promise((res, rej) => { const ws = new WebSocket(u); let id = 0; const p = new Map(); const events = []; ws.onopen = () => res({ send(m, a = {}) { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: a })); return new Promise((s, j) => p.set(i, { s, j })); }, close: () => ws.close(), events }); ws.onerror = () => rej(new Error("ws")); ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && p.has(m.id)) { const { s, j } = p.get(m.id); p.delete(m.id); m.error ? j(new Error(JSON.stringify(m.error))) : s(m.result); } else if (m.method) events.push(m); }; }); }
const list = await (await fetch("http://127.0.0.1:" + PORT + "/json/list")).json();
const c = await connect(list.find(t => t.type === "page").webSocketDebuggerUrl);
const sleep = ms => new Promise(r => setTimeout(r, ms));
const ev = async (x) => { const r = await c.send("Runtime.evaluate", { expression: x, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error(r.exceptionDetails.text + " " + (r.exceptionDetails.exception?.description || "")); return r.result.value; };
const pt = async (t, x, y) => c.send("Input.dispatchMouseEvent", { type: t, x, y, button: t === "mouseMoved" ? "none" : "left", clickCount: t === "mouseMoved" ? 0 : 1 });
const clickExpr = async (expr, wait) => { const b = await ev(`(() => { const el = ${expr}; if (!el) return null; const r = el.getBoundingClientRect(); if (!r.width) return null; return { x: r.x + r.width / 2, y: r.y + r.height / 2 }; })()`); if (!b) return "NOT_FOUND"; await pt("mouseMoved", b.x, b.y); await pt("mousePressed", b.x, b.y); await sleep(50); await pt("mouseReleased", b.x, b.y); await sleep(wait); return "ok"; };
await c.send("Runtime.enable"); await c.send("Log.enable"); await c.send("Network.enable");
await ev("window.__errs = []; window.addEventListener('error', e => window.__errs.push(String(e.message))); window.addEventListener('unhandledrejection', e => window.__errs.push('rej:' + String(e.reason))); 'armed'");
const out = { nav: [] };
for (const l of ["渠道与模型", "分组路由", "设置", "日志", "统计", "渠道与模型"]) {
  await clickExpr(`[...document.querySelectorAll('aside button')].find(x => x.textContent.trim() === '${l}')`, 800);
  out.nav.push({ page: l, header: await ev("document.querySelector('main') ? document.querySelector('main').innerText.replace(/\\s+/g,' ').slice(0, 40) : 'NONE'") });
}
out.jsErrors = await ev("JSON.stringify(window.__errs)");
const evs = c.events.filter(e => e.method === "Log.entryAdded" || e.method === "Runtime.exceptionThrown" || e.method === "Runtime.consoleAPICalled");
out.console = evs.slice(0, 10).map(e => JSON.stringify(e.params).slice(0, 200));
out.online = await ev("!!document.querySelector('footer .bg-ok, footer span')");
console.log(JSON.stringify(out, null, 1));
c.close(); process.exit(0);
