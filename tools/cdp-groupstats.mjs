// 分组列表统计条验收：卡片 KPI 数值、工具条全局累计、刷新按钮、几何。
import fs from "node:fs";
const PORT = Number(process.argv[2] || process.env.CDP_PORT || 9223);
async function connect(u) { return new Promise((res, rej) => { const ws = new WebSocket(u); let id = 0; const p = new Map();
  ws.onopen = () => res({ send(m, a = {}) { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: a })); return new Promise((s, j) => p.set(i, { s, j })); }, close: () => ws.close() });
  ws.onerror = () => rej(new Error("ws error"));
  ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && p.has(m.id)) { const { s, j } = p.get(m.id); p.delete(m.id); m.error ? j(new Error(JSON.stringify(m.error))) : s(m.result); } }; }); }
const list = await (await fetch("http://127.0.0.1:" + PORT + "/json/list")).json();
const c = await connect(list.find(t => t.type === "page").webSocketDebuggerUrl);
const sleep = ms => new Promise(r => setTimeout(r, ms));
const ev = async (x) => { const r = await c.send("Runtime.evaluate", { expression: x, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error(r.exceptionDetails.text); return r.result.value; };
const pt = async (t, x, y) => c.send("Input.dispatchMouseEvent", { type: t, x, y, button: t === "mouseMoved" ? "none" : "left", clickCount: t === "mouseMoved" ? 0 : 1 });
const boxOf = (expr) => ev("(() => { const el = " + expr + "; if (!el) return null; const r = el.getBoundingClientRect(); if (!r.width || !r.height) return null; return { x: r.x + r.width / 2, y: r.y + r.height / 2 }; })()");
const click = async (expr, wait = 900) => { const b = await boxOf(expr); if (!b) return "NOT_FOUND"; await pt("mouseMoved", b.x, b.y); await sleep(60); await pt("mousePressed", b.x, b.y); await sleep(60); await pt("mouseReleased", b.x, b.y); await sleep(wait); return "ok"; };
const shot = async (name) => { const r = await c.send("Page.captureScreenshot", { format: "png" }); fs.writeFileSync(name, Buffer.from(r.data, "base64")); return name; };
const out = {};
await click("[...document.querySelectorAll('aside button')].find(x => x.textContent.trim() === '分组路由')", 1800);
const dump = "JSON.stringify([...document.querySelectorAll('[data-test=\"group-stats\"]')].map(bar => { const card = bar.closest('[class*=\"rounded-xl\"]') || bar.parentElement; const m = (card.textContent||'').match(/grp_[a-z_]+/); const g = k => { const n = bar.querySelector('[data-stat=\"' + k + '\"]'); return n ? n.textContent.trim() : null; }; return { gid: m ? m[0] : '?', requests: g('requests'), success: g('success'), cost: g('cost'), avg: g('avg'), tokens: g('tokens'), bars: bar.querySelectorAll('[data-stat]').length }; }))";
out.bars = await ev(dump);
out.toolbar = await ev("(() => { const t = document.querySelector('main').textContent || ''; const i = t.indexOf('共 '); return t.slice(i, i + 120).replace(/\\s+/g, ' '); })()");
out.refreshBtn = await ev("!!document.querySelector('[data-test=\"group-stats-refresh\"]')");
out.barRects = await ev("JSON.stringify([...document.querySelectorAll('[data-test=\"group-stats\"]')].map(b => { const r = b.getBoundingClientRect(); const cols = [...b.querySelectorAll(':scope > div > div')].map(c => Math.round(c.getBoundingClientRect().width)); return { x: Math.round(r.x), y: Math.round(r.y), w: Math.round(r.width), h: Math.round(r.height), cellW: cols }; }))");
out.overflowX = await ev("(() => { const m = document.querySelector('main'); return JSON.stringify({ mainScrollX: m.scrollWidth - m.clientWidth, docScrollX: document.documentElement.scrollWidth - document.documentElement.clientWidth }); })()");
await shot("w15-groups-stats.png");
// 手动刷新按钮：点一下应触发请求并更新「最后更新」时钟
out.clock1 = await ev("(() => { const m = (document.querySelector('main').textContent||'').match(/最后更新\\s*([0-9:]{8})/); return m ? m[1] : 'NO_CLOCK'; })()");
out.clickRefresh = await click("document.querySelector('[data-test=\"group-stats-refresh\"]')", 1400);
out.clock2 = await ev("(() => { const m = (document.querySelector('main').textContent||'').match(/最后更新\\s*([0-9:]{8})/); return m ? m[1] : 'NO_CLOCK'; })()");
out.jsErrors = await ev("JSON.stringify(window.__errs || [])");
console.log(JSON.stringify(out, null, 1));
c.close(); process.exit(0);