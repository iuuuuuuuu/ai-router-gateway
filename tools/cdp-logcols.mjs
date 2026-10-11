// Measure each column of the Logs tables to decide whether the default view can
// fit without horizontal scrolling.
import fs from "node:fs/promises";
const PORT = Number(process.argv[2] || 9223);
async function connect(u) { return new Promise((res, rej) => { const ws = new WebSocket(u); let id = 0; const p = new Map(); ws.onopen = () => res({ send(m, a = {}) { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: a })); return new Promise((s, j) => p.set(i, { s, j })); }, close: () => ws.close() }); ws.onerror = () => rej(new Error("ws")); ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && p.has(m.id)) { const { s, j } = p.get(m.id); p.delete(m.id); m.error ? j(new Error(JSON.stringify(m.error))) : s(m.result); } }; }); }
const list = await (await fetch("http://127.0.0.1:" + PORT + "/json/list")).json();
const c = await connect(list.find(t => t.type === "page").webSocketDebuggerUrl);
const sleep = ms => new Promise(r => setTimeout(r, ms));
const ev = async (x) => { const r = await c.send("Runtime.evaluate", { expression: x, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error(r.exceptionDetails.text + " " + (r.exceptionDetails.exception?.description || "")); return r.result.value; };
const pt = async (t, x, y) => c.send("Input.dispatchMouseEvent", { type: t, x, y, button: t === "mouseMoved" ? "none" : "left", clickCount: t === "mouseMoved" ? 0 : 1 });
const clickExpr = async (expr, wait) => { const b = await ev(`(() => { const el = ${expr}; if (!el) return null; const r = el.getBoundingClientRect(); if (!r.width) return null; return { x: r.x + r.width / 2, y: r.y + r.height / 2 }; })()`); if (!b) return "NOT_FOUND"; await pt("mouseMoved", b.x, b.y); await pt("mousePressed", b.x, b.y); await sleep(50); await pt("mouseReleased", b.x, b.y); await sleep(wait); return "ok"; };
await clickExpr(`[...document.querySelectorAll('aside button')].find(x => x.textContent.trim() === '日志')`, 900);
const cols = `(() => {
  const t = document.querySelector('main table');
  if (!t) return 'NO_TABLE';
  const th = [...t.querySelectorAll('thead th')];
  const widths = th.map(h => ({ name: h.innerText.trim(), w: Math.round(h.getBoundingClientRect().width) }));
  const wrap = t.parentElement;
  const cells = [...t.querySelectorAll('tbody tr:first-child td')].map(td => ({ w: Math.round(td.getBoundingClientRect().width), scroll: td.scrollWidth, text: td.innerText.replace(/\\s+/g,' ').trim().slice(0, 30) }));
  return { total: widths.reduce((a, b) => a + b.w, 0), widths, wrapW: Math.round(wrap.getBoundingClientRect().width), overflow: Math.round(t.getBoundingClientRect().width - wrap.getBoundingClientRect().width), cells };
})()`;
const out = { switchTab: await ev(cols) };
await clickExpr(`[...document.querySelectorAll('main button')].find(x => x.textContent.trim() === '请求日志')`, 900);
out.requestTab = await ev(cols);
console.log(JSON.stringify(out, null, 1));
c.close(); process.exit(0);
