// Measure every footer-bearing dialog on the real Tauri window: is the action
// row reachable without scrolling, and does it stay pinned while the body scrolls?
// Usage: node tools/cdp-modalcheck.mjs [port]
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
const click = async (expr, wait = 500) => { const b = await boxOf(expr); if (!b) return "NOT_FOUND"; await pt("mouseMoved", b.x, b.y); await sleep(60); await pt("mousePressed", b.x, b.y); await sleep(60); await pt("mouseReleased", b.x, b.y); await sleep(wait); return "ok"; };
const nav = async (label) => { const r = await click("[...document.querySelectorAll('aside button')].find(x => x.textContent.trim() === '" + label + "')", 850); if (r === "NOT_FOUND") { await esc(); return click("[...document.querySelectorAll('aside button')].find(x => x.textContent.trim() === '" + label + "')", 950); } return r; };
const esc = async () => { await c.send("Input.dispatchKeyEvent", { type: "keyDown", key: "Escape", code: "Escape", windowsVirtualKeyCode: 27 }); await c.send("Input.dispatchKeyEvent", { type: "keyUp", key: "Escape", code: "Escape", windowsVirtualKeyCode: 27 }); await sleep(400); };

/* One dialog measurement. "footer" = the last child div of the scroll container
   that holds buttons, which is exactly the Modal footer slot. */
const MEASURE = `(() => {
  const d = [...document.querySelectorAll('[role="dialog"]')].pop();
  if (!d) return { ok: false, why: 'NO_DIALOG' };
  const kids = [...d.children].filter(e => e.nodeType === 1);
  const foot = [...kids].reverse().find(e => e.querySelector('button'));
  const head = kids[0];
  const R = e => { const r = e.getBoundingClientRect(); return { y: Math.round(r.y), bottom: Math.round(r.bottom), h: Math.round(r.height) }; };
  const dR = R(d), fR = foot ? R(foot) : null, hR = head ? R(head) : null;
  const btn = foot ? [...foot.querySelectorAll('button')].map(b => String(b.textContent).trim() + '@' + Math.round(b.getBoundingClientRect().y)) : [];
  const inView = r => r && r.y >= 0 && r.bottom <= innerHeight + 1;
  return {
    ok: true,
    title: String(d.textContent).trim().split(String.fromCharCode(10))[0].slice(0, 30),
    dialog: dR, scrollH: d.scrollHeight, clientH: d.clientHeight, overflowY: getComputedStyle(d).overflowY,
    footer: fR, footerVisible: inView(fR), footerSticky: foot ? getComputedStyle(foot).position : null,
    header: hR, headerVisible: inView(hR), headerSticky: head ? getComputedStyle(head).position : null,
    buttons: btn, innerH: innerHeight,
  };
})()`;

const measure = async () => JSON.parse(await ev("JSON.stringify(" + MEASURE + ")"));
const scrollBottom = () => ev("(() => { const d = [...document.querySelectorAll('[role=\"dialog\"]')].pop(); if (!d) return 'NO_DIALOG'; d.scrollTop = d.scrollHeight; return Math.round(d.scrollTop); })()");
const cancelDialog = async () => {
  const r = await ev("(() => { const d = [...document.querySelectorAll('[role=\"dialog\"]')].pop(); if (!d) return 'NO_DIALOG'; const b = [...d.querySelectorAll('button')].find(x => /取消/.test(x.textContent)) || d.querySelector('[data-test=\"cancel\"],[data-test=\"member-cancel\"]'); if (!b) return 'NO_CANCEL'; b.click(); return 'clicked'; })()");
  await sleep(450);
  return r;
};

const out = [];
const step = async (name, openExpr, wait) => {
  const opened = await click(openExpr, wait || 800);
  if (opened !== "ok") { out.push({ name, opened, why: "OPEN_NOT_FOUND" }); return; }
  const before = await measure();
  if (!before.ok) { out.push({ name, opened, why: before.why }); await esc(); return; }
  const st = await scrollBottom();
  await sleep(220);
  const after = await measure();
  const cancel = await cancelDialog();
  const left = await ev("document.querySelectorAll('[role=\"dialog\"]').length");
  out.push({ name, opened, scrollTopAfter: st, before, after, cancel, dialogsLeft: left });
  await sleep(250);
};

await nav("分组路由");
await step("groups-edit-route", "[...document.querySelectorAll('main button')].find(x => x.textContent.trim() === '编辑路由')");
await step("groups-delete", "document.querySelector('main [title=\"删除\"], main button svg.lucide-trash2')?.closest('button') || [...document.querySelectorAll('main button')].filter(b => b.querySelector('svg')).find(b => /trash/i.test(b.innerHTML))");

await nav("渠道与模型");
await click("[...document.querySelectorAll('main button')].find(x => x.textContent.trim() === '展开全部')", 900);
await step("channels-add-model", "[...document.querySelectorAll('main button')].find(x => x.textContent.trim() === '添加模型')");
await step("channels-add", "[...document.querySelectorAll('main button')].find(x => x.textContent.trim() === '添加渠道')");
await step("channels-edit", "[...document.querySelectorAll('main button')].find(x => x.textContent.trim() === '添加渠道') && document.querySelector('main button[title=\"编辑\"]')");

console.log(JSON.stringify(out, null, 1));
c.close(); process.exit(0);
