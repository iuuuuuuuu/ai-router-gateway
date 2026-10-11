// 分组详情「独立整页」验收探针：导航高亮 / 无弹窗 / 页签取证 / 吸顶 / 自动刷新 / 返回与 Esc。
// 用法：node tools/cdp-grouppage.mjs [PORT]
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
const click = async (expr, wait = 700) => { const b = await boxOf(expr); if (!b) return "NOT_FOUND"; await pt("mouseMoved", b.x, b.y); await sleep(60); await pt("mousePressed", b.x, b.y); await sleep(60); await pt("mouseReleased", b.x, b.y); await sleep(wait); return "ok"; };
const shot = async (name) => { const r = await c.send("Page.captureScreenshot", { format: "png" }); fs.writeFileSync(name, Buffer.from(r.data, "base64")); return name; };
const railBtn = (label) => "[...document.querySelectorAll('aside button')].find(x => x.textContent.trim() === '" + label + "')";
const root = "document.querySelector('[data-test=\"group-dashboard\"]')";
const tabs = "document.querySelector('[data-test=\"group-dashboard-tab-overview\"]').parentElement";
const out = {};

// 0) 回到分组路由页
await click(railBtn("分组路由"), 900);
out.groupDetailBtns = await ev("document.querySelectorAll('[data-test=\"group-detail-btn\"]').length");

// 1) 打开分组详情
out.openClick = await click("document.querySelector('[data-test=\"group-detail-btn\"]')", 1600);
out.dialogsWhileOpen = await ev("document.querySelectorAll('[role=\"dialog\"]').length");
out.pageMounted = await ev("!!" + root);
out.insideDialog = await ev("(() => { const el = " + root + "; return el ? !!el.closest('[role=\"dialog\"]') : 'NO_ROOT'; })()");
out.heading = await ev("document.querySelector('main > header h1').textContent.trim()");
out.headingHint = await ev("document.querySelector('main > header h1').nextElementSibling.textContent.trim()");
out.backBtn = await ev("!!document.querySelector('[data-test=\"group-detail-back\"]')");
out.railActive = await ev("(() => { const b = " + railBtn("分组路由") + "; if (!b) return 'NO_BTN'; return JSON.stringify({ cls: b.className.includes('bg-surface'), title: b.textContent.trim() }); })()");
out.rootPad = await ev("(() => { const el = " + root + "; if (!el) return 'NO_ROOT'; const cs = getComputedStyle(el); return cs.paddingLeft + ' / ' + cs.paddingTop; })()");
out.pageHeader = await ev("(() => { const el = " + root + "; if (!el) return 'NO_ROOT'; const h = el.querySelector('h1'); return h ? h.textContent.trim() + ' || ' + (h.nextElementSibling ? h.nextElementSibling.textContent.trim() : '') : 'NO_H1'; })()");

// 2) 吸顶：滚动内容区后页签条必须贴住内容区顶端
const tabTopOf = "(() => { const t = " + tabs + "; const s = document.querySelector('section[class*=\"overflow-y-auto\"]'); return JSON.stringify({ tabTop: Math.round(t.getBoundingClientRect().top), secTop: Math.round(s.getBoundingClientRect().top), scrollTop: Math.round(s.scrollTop) }); })()";
out.stickyBefore = await ev(tabTopOf);
await ev("document.querySelector('section[class*=\"overflow-y-auto\"]').scrollTo({ top: 600 })");
await sleep(500);
out.stickyAfter = await ev(tabTopOf);
out.stickyWorks = await ev("(() => { const t = " + tabs + ".getBoundingClientRect().top; const s = document.querySelector('section[class*=\"overflow-y-auto\"]').getBoundingClientRect().top; return Math.abs(t - s) <= 2; })()");
await ev("document.querySelector('section[class*=\"overflow-y-auto\"]').scrollTo({ top: 0 })");
await sleep(400);

// 3) 逐页签取证
out.tabs = {};
for (const name of ["概览", "用量", "请求日志", "切换日志", "冷却状态"]) {
  const r = await click("(() => { const el = " + root + "; return el && [...el.querySelectorAll('button')].find(b => b.textContent.trim() === '" + name + "'); })()", 900);
  out.tabs[name] = await ev("(() => { const el = " + root + "; if (!el) return 'NO_ROOT'; const body = el.querySelector('table') ? 'table' : (el.textContent.includes('暂无') || el.textContent.includes('没有') ? 'empty' : 'text'); const rows = el.querySelectorAll('tbody tr').length; const txt = (el.textContent || '').trim(); return JSON.stringify({ click: '" + r + "', body, rows, chars: txt.length, head: txt.slice(0, 150) }); })()");
}
await click("(() => { const el = " + root + "; return el && [...el.querySelectorAll('button')].find(b => b.textContent.trim() === '用量'); })()", 700);
await shot("w16-groupdetail-usage.png");

// 4) 自动刷新
await click("(() => { const el = " + root + "; return el && [...el.querySelectorAll('button')].find(b => b.textContent.trim() === '概览'); })()", 700);
const clockOf = "(() => { const el = " + root + "; const t = el ? (el.textContent || '') : ''; const i = t.indexOf('最后更新'); if (i < 0) return 'NO_CLOCK'; const seg = t.slice(i, i + 24); const m = seg.match(/[0-9][0-9]:[0-9][0-9]:[0-9][0-9]/); return m ? m[0] : 'NO_CLOCK'; })()";
out.clock1 = await ev(clockOf);
await sleep(6500);
out.clock2 = await ev(clockOf);
out.autoRefreshWorks = out.clock1 !== "NO_CLOCK" && out.clock2 !== "NO_CLOCK" && out.clock1 !== out.clock2;
await shot("w16-groupdetail-overview.png");

// 5) Esc 返回
await c.send("Input.dispatchKeyEvent", { type: "keyDown", key: "Escape", code: "Escape", windowsVirtualKeyCode: 27 });
await c.send("Input.dispatchKeyEvent", { type: "keyUp", key: "Escape", code: "Escape", windowsVirtualKeyCode: 27 });
await sleep(800);
out.afterEsc = await ev("JSON.stringify({ dashboard: !!document.querySelector('[data-test=\"group-dashboard\"]'), detailBtns: document.querySelectorAll('[data-test=\"group-detail-btn\"]').length, heading: document.querySelector('main > header h1').textContent.trim() })");

// 6) 再进一次，点顶栏「返回」
await click("document.querySelector('[data-test=\"group-detail-btn\"]')", 1500);
out.reopened = await ev("!!" + root);
out.backClick = await click("document.querySelector('[data-test=\"group-detail-back\"]')", 900);
out.afterBack = await ev("JSON.stringify({ dashboard: !!document.querySelector('[data-test=\"group-dashboard\"]'), detailBtns: document.querySelectorAll('[data-test=\"group-detail-btn\"]').length, heading: document.querySelector('main > header h1').textContent.trim() })");

// 7) 详情页开着时点侧栏「统计」→ 必须离开详情页
await click("document.querySelector('[data-test=\"group-detail-btn\"]')", 1500);
out.railNav = await click(railBtn("统计"), 1100);
out.afterRailNav = await ev("JSON.stringify({ dashboard: !!document.querySelector('[data-test=\"group-dashboard\"]'), heading: document.querySelector('main > header h1').textContent.trim(), hasStatsTable: !!document.querySelector('table') })");

// 8) 第二个分组也能打开
await click(railBtn("分组路由"), 900);
out.secondGroup = await ev("(() => { const bs = [...document.querySelectorAll('[data-test=\"group-detail-btn\"]')]; return bs.length < 2 ? 'ONLY_' + bs.length : 'ok'; })()");
out.secondClick = await click("[...document.querySelectorAll('[data-test=\"group-detail-btn\"]')][1]", 1600);
out.secondHint = await ev("document.querySelector('main > header h1').nextElementSibling.textContent.trim()");
await shot("w16-groupdetail-second.png");

out.jsErrors = await ev("JSON.stringify(window.__errs || [])");
console.log(JSON.stringify(out, null, 1));
c.close(); process.exit(0);
