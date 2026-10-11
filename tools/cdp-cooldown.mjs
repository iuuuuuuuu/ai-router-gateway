// 冷却面板真机验收：统计页 + 分组详情页的「永久」隔离显示与失败次数。
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
const click = async (expr, wait = 800) => { const b = await boxOf(expr); if (!b) return "NOT_FOUND"; await pt("mouseMoved", b.x, b.y); await sleep(60); await pt("mousePressed", b.x, b.y); await sleep(60); await pt("mouseReleased", b.x, b.y); await sleep(wait); return "ok"; };
const shot = async (n) => { const r = await c.send("Page.captureScreenshot", { format: "png" }); fs.writeFileSync(n, Buffer.from(r.data, "base64")); return n; };
const railBtn = (label) => "[...document.querySelectorAll('aside button')].find(x => x.textContent.trim() === '" + label + "')";
const out = {};

await ev("window.__errs = []; window.addEventListener('error', e => window.__errs.push(String(e.message)));");
await ev("location.reload()");
await sleep(4500);

// ---------- 统计页 ----------
await click(railBtn("统计"), 2200);
out.statsCooldown = await ev(
  "(() => {"
  + " const h = [...document.querySelectorAll('h2')].find(x => x.textContent.trim() === '冷却状态');"
  + " if (!h) return 'NO_CARD';"
  + " const card = h.closest('div').parentElement.parentElement;"
  + " const rows = [...card.querySelectorAll('tbody tr')].map(tr => [...tr.querySelectorAll('td')].map(td => td.textContent.trim()));"
  + " const cnt = [...card.querySelectorAll('span')].map(s => s.textContent.trim()).find(t => /个条目|全部正常/.test(t));"
  + " return JSON.stringify({ count: cnt, rows });"
  + " })()"
);
await shot("w17-stats-cooldown.png");

// ---------- 分组详情页 ----------
await click(railBtn("分组路由"), 1600);
await click("[...document.querySelectorAll('[data-test=group-detail-btn]')][0]", 2200);
out.detailHeading = await ev("(() => { const h = document.querySelector('h1,h2'); return h ? h.textContent.trim() : null; })()");
await click("(() => { const el = document.querySelector('[data-test=group-dashboard]'); return el && [...el.querySelectorAll('button')].find(b => b.textContent.trim() === '冷却状态'); })()", 1400);
out.detailCooldown = await ev(
  "(() => {"
  + " const el = document.querySelector('[data-test=group-dashboard]');"
  + " if (!el) return 'NO_ROOT';"
  + " const rows = [...el.querySelectorAll('tbody tr')].map(tr => [...tr.querySelectorAll('td')].map(td => td.textContent.trim()));"
  + " const perma = [...el.querySelectorAll('span')].filter(s => s.textContent.trim() === '永久').map(s => getComputedStyle(s).color);"
  + " const empty = [...el.querySelectorAll('*')].some(e => e.textContent.trim() === '本分组相关渠道暂无冷却');"
  + " return JSON.stringify({ rows, permanentCount: perma.length, permanentColors: [...new Set(perma)], emptyState: empty });"
  + " })()"
);
await shot("w18-detail-cooldown.png");
out.jsErrors = await ev("JSON.stringify(window.__errs || [])");
console.log(JSON.stringify(out, null, 1));
c.close(); process.exit(0);
