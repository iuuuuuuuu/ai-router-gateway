// 吸顶专项：切到长内容页签（请求日志）后滚动内容区，页签条必须贴住内容区顶端。
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
const ev = async (x) => { const r = await c.send("Runtime.evaluate", { expression: x, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error(r.exceptionDetails.text); return r.result.value; };
const pt = async (t, x, y) => c.send("Input.dispatchMouseEvent", { type: t, x, y, button: t === "mouseMoved" ? "none" : "left", clickCount: t === "mouseMoved" ? 0 : 1 });
const boxOf = (expr) => ev("(() => { const el = " + expr + "; if (!el) return null; const r = el.getBoundingClientRect(); if (!r.width || !r.height) return null; return { x: r.x + r.width / 2, y: r.y + r.height / 2 }; })()");
const click = async (expr, wait = 700) => { const b = await boxOf(expr); if (!b) return "NOT_FOUND"; await pt("mouseMoved", b.x, b.y); await sleep(60); await pt("mousePressed", b.x, b.y); await sleep(60); await pt("mouseReleased", b.x, b.y); await sleep(wait); return "ok"; };
const shot = async (n) => { const r = await c.send("Page.captureScreenshot", { format: "png" }); fs.writeFileSync(n, Buffer.from(r.data, "base64")); return n; };
const railBtn = (label) => "[...document.querySelectorAll('aside button')].find(x => x.textContent.trim() === '" + label + "')";
const root = "document.querySelector('[data-test=\"group-dashboard\"]')";
const tabBar = "document.querySelector('[data-test^=\"group-dashboard-tab-\"]').parentElement";
const out = {};

// 定位真正的滚动容器（列出所有候选及其 scrollHeight/clientHeight）
out.scrollers = await ev("JSON.stringify([...document.querySelectorAll('*')].filter(e => { const cs = getComputedStyle(e); return (cs.overflowY === 'auto' || cs.overflowY === 'scroll') && e.scrollHeight > e.clientHeight + 4; }).map(e => ({ tag: e.tagName, cls: (e.className || '').toString().slice(0, 70), sh: e.scrollHeight, ch: e.clientHeight, st: Math.round(e.scrollTop) })))");

await click(railBtn("分组路由"), 900);
await click("document.querySelector('[data-test=\"group-detail-btn\"]')", 1600);
// 切到长内容页签
await click("(() => { const el = " + root + "; return el && [...el.querySelectorAll('button')].find(b => b.textContent.trim() === '请求日志'); })()", 900);
out.rowsOnTab = await ev("document.querySelectorAll('[data-test=\"group-dashboard\"] tbody tr').length");

const sc = "document.querySelector('section[class*=\"overflow-y-auto\"]')";
const tabTopOf = "(() => { const t = " + tabBar + ".getBoundingClientRect().top; const s = " + sc + ".getBoundingClientRect().top; return JSON.stringify({ tabTop: Math.round(t), secTop: Math.round(s), delta: Math.round(t - s), scrollTop: Math.round(" + sc + ".scrollTop), secScrollH: " + sc + ".scrollHeight, secClientH: " + sc + ".clientHeight }); })()";
out.before = await ev(tabTopOf);
await ev(sc + ".scrollTo({ top: 700 })");
await sleep(600);
out.after = await ev(tabTopOf);
out.stickyWorks = await ev("(() => { const t = " + tabBar + ".getBoundingClientRect().top; const s = " + sc + ".getBoundingClientRect().top; return Math.abs(t - s) <= 2; })()");
out.tabBarStickyClass = await ev("(() => { const cs = getComputedStyle(" + tabBar + "); return JSON.stringify({ position: cs.position, top: cs.top, zIndex: cs.zIndex, bg: cs.backgroundColor }); })()");
await shot("w16-groupdetail-sticky.png");
out.jsErrors = await ev("JSON.stringify(window.__errs || [])");
console.log(JSON.stringify(out, null, 1));
c.close(); process.exit(0);
