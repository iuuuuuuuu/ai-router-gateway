// Geometry audit for the real window: does anything get clipped at the right
// edge of the content area, and is any log/stat table trapped without a
// horizontal scroller?
import fs from "node:fs/promises";
const PORT = Number(process.argv[2] || 9223);
async function connect(u) { return new Promise((res, rej) => { const ws = new WebSocket(u); let id = 0; const p = new Map(); ws.onopen = () => res({ send(m, a = {}) { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: a })); return new Promise((s, j) => p.set(i, { s, j })); }, close: () => ws.close() }); ws.onerror = () => rej(new Error("ws")); ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && p.has(m.id)) { const { s, j } = p.get(m.id); p.delete(m.id); m.error ? j(new Error(JSON.stringify(m.error))) : s(m.result); } }; }); }
const list = await (await fetch("http://127.0.0.1:" + PORT + "/json/list")).json();
const c = await connect(list.find(t => t.type === "page").webSocketDebuggerUrl);
const sleep = ms => new Promise(r => setTimeout(r, ms));
const ev = async (x) => { const r = await c.send("Runtime.evaluate", { expression: x, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error(r.exceptionDetails.text + " " + (r.exceptionDetails.exception?.description || "")); return r.result.value; };
const pt = async (t, x, y) => c.send("Input.dispatchMouseEvent", { type: t, x, y, button: t === "mouseMoved" ? "none" : "left", clickCount: t === "mouseMoved" ? 0 : 1 });
const clickExpr = async (expr, wait) => { const b = await ev(`(() => { const el = ${expr}; if (!el) return null; const r = el.getBoundingClientRect(); if (!r.width) return null; return { x: r.x + r.width / 2, y: r.y + r.height / 2 }; })()`); if (!b) return "NOT_FOUND"; await pt("mouseMoved", b.x, b.y); await pt("mousePressed", b.x, b.y); await sleep(50); await pt("mouseReleased", b.x, b.y); await sleep(wait); return "ok"; };

const probe = `(() => {
  const main = document.querySelector('main');
  const box = main.getBoundingClientRect();
  const bad = [];
  for (const el of main.querySelectorAll('button, input, textarea, [role="switch"], [role="combobox"], table, .card, section > div')) {
    const r = el.getBoundingClientRect();
    if (!r.width || !r.height) continue;
    if (r.right > box.right + 0.6 || r.left < box.left - 0.6) bad.push({ tag: el.tagName, cls: String(el.className).slice(0, 50), left: Math.round(r.left), right: Math.round(r.right) });
  }
  const tables = [...main.querySelectorAll('table')].map(t => {
    const wrap = t.parentElement;
    const cs = wrap ? getComputedStyle(wrap) : null;
    return { w: Math.round(t.getBoundingClientRect().width), wrapW: wrap ? Math.round(wrap.getBoundingClientRect().width) : null, overflowX: cs ? cs.overflowX : null, scrollable: wrap ? wrap.scrollWidth > wrap.clientWidth + 1 : null };
  });
  const sec = main.querySelector('section');
  return { main: { l: Math.round(box.left), r: Math.round(box.right), w: Math.round(box.width) }, clipped: bad.slice(0, 8), clippedCount: bad.length, tables, secScrollX: sec ? sec.scrollWidth - sec.clientWidth : null };
})()`;

const out = {};
for (const label of ["渠道与模型", "分组路由", "设置", "日志", "统计"]) {
  await clickExpr(`[...document.querySelectorAll('aside button')].find(x => x.textContent.trim() === '${label}')`, 800);
  out[label] = await ev(probe);
}
console.log(JSON.stringify(out, null, 1));
c.close(); process.exit(0);
