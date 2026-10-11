// Mirror the FAST opening sequence used by tools/cdp-dropdown.mjs (nav -> 450ms
// -> 编辑路由 -> 500ms -> click combobox) three times, to decide whether the
// empty-option result was a flaky probe or a real intermittent defect.
import fs from "node:fs/promises";
const PORT = Number(process.argv[2] || 9223);
async function connect(u) { return new Promise((res, rej) => { const ws = new WebSocket(u); let id = 0; const p = new Map(); ws.onopen = () => res({ send(m, a = {}) { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: a })); return new Promise((s, j) => p.set(i, { s, j })); }, close: () => ws.close() }); ws.onerror = () => rej(new Error("ws")); ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && p.has(m.id)) { const { s, j } = p.get(m.id); p.delete(m.id); m.error ? j(new Error(JSON.stringify(m.error))) : s(m.result); } }; }); }
const list = await (await fetch("http://127.0.0.1:" + PORT + "/json/list")).json();
const c = await connect(list.find(t => t.type === "page").webSocketDebuggerUrl);
const sleep = ms => new Promise(r => setTimeout(r, ms));
const ev = async (x) => { const r = await c.send("Runtime.evaluate", { expression: x, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error(r.exceptionDetails.text + " " + (r.exceptionDetails.exception?.description || "")); return r.result.value; };
const pt = async (t, x, y) => c.send("Input.dispatchMouseEvent", { type: t, x, y, button: t === "mouseMoved" ? "none" : "left", clickCount: t === "mouseMoved" ? 0 : 1 });
const box = async (expr) => ev(`(() => { const el = ${expr}; if (!el) return null; const b = el.getBoundingClientRect(); if (!b.width || !b.height) return null; return { x: b.x + b.width / 2, y: b.y + b.height / 2 }; })()`);
const clickExpr = async (expr, wait) => { const b = await box(expr); if (!b) return "NOT_FOUND"; await pt("mouseMoved", b.x, b.y); await pt("mousePressed", b.x, b.y); await sleep(40); await pt("mouseReleased", b.x, b.y); await sleep(wait); return "ok"; };
const snap = async () => ev(`JSON.stringify({ opts: document.querySelectorAll('[role="option"]').length, lb: document.querySelectorAll('[role="listbox"]').length, filter: !!document.querySelector('[data-select-filter]'), dialogs: document.querySelectorAll('[role="dialog"]').length, expanded: [...document.querySelectorAll('[role="combobox"]')].map(t => t.getAttribute('aria-expanded')).join(''), cancelBtn: [...document.querySelectorAll('[role="dialog"] button')].some(b => b.textContent.trim() === '取消') })`);

const out = { fast: [], slow: [] };
const member = `(() => { const d = [...document.querySelectorAll('[role="dialog"]')].pop(); return d && d.querySelectorAll('[role="combobox"]')[0]; })()`;
async function round(bucket, navWait, editWait) {
  await clickExpr(`[...document.querySelectorAll('aside button')].find(x => x.textContent.trim() === '渠道与模型')`, 700);
  const r = {};
  r.nav = await clickExpr(`[...document.querySelectorAll('aside button')].find(x => x.textContent.trim() === '分组路由')`, navWait);
  r.edit = await clickExpr(`[...document.querySelectorAll('main button')].find(x => x.textContent.replace(/\\s+/g,' ').trim().startsWith('编辑路由'))`, editWait);
  r.pre = await snap();
  r.open = await clickExpr(member, 700);
  r.post = await snap();
  if (JSON.parse(r.post).opts === 0) { r.retry = await clickExpr(member, 700); r.post2 = await snap(); }
  await c.send("Input.dispatchKeyEvent", { type: "keyDown", key: "Escape", code: "Escape", windowsVirtualKeyCode: 27 });
  await c.send("Input.dispatchKeyEvent", { type: "keyUp", key: "Escape", code: "Escape", windowsVirtualKeyCode: 27 });
  await sleep(300);
  r.close = await clickExpr(`[...document.querySelectorAll('[role="dialog"] button')].find(x => x.textContent.trim() === '取消')`, 400);
  r.after = await ev("document.querySelectorAll('[role=\"dialog\"]').length");
  bucket.push(r);
}
for (let i = 0; i < 3; i++) await round(out.fast, 700, 450);
for (let i = 0; i < 2; i++) await round(out.slow, 900, 700);
console.log(JSON.stringify(out, null, 1));
c.close(); process.exit(0);
