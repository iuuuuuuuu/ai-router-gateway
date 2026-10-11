const PORT = Number(process.argv[2] || 9223);
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
const out = {};
out.viewport = await ev("JSON.stringify({ iw: innerWidth, ih: innerHeight, dpr: devicePixelRatio, docH: document.documentElement.clientHeight })");
await click("[...document.querySelectorAll('aside button')].find(x => x.textContent.trim() === '分组路由')", 800);
await click("[...document.querySelectorAll('main button')].find(x => x.textContent.trim() === '编辑路由')", 900);
out.dialogGeo = await ev("(() => { const d = [...document.querySelectorAll('[role=\"dialog\"]')].pop(); if (!d) return 'NO_DIALOG'; const r = d.getBoundingClientRect(); const btn = [...d.querySelectorAll('button')].find(b => String(b.textContent).trim() === '取消'); const br = btn ? btn.getBoundingClientRect() : null; return JSON.stringify({ dialog: { x: Math.round(r.x), y: Math.round(r.y), w: Math.round(r.width), h: Math.round(r.height) }, scrollH: d.scrollHeight, clientH: d.clientHeight, overflowY: getComputedStyle(d).overflowY, cancelY: br ? Math.round(br.y) : null, cancelVisible: br ? (br.y >= 0 && br.bottom <= innerHeight) : null, innerH: innerHeight }); })()");
/* scroll the modal body to the bottom, then re-measure the footer */
out.afterScroll = await ev("(() => { const d = [...document.querySelectorAll('[role=\"dialog\"]')].pop(); if (!d) return 'NO_DIALOG'; d.scrollTop = d.scrollHeight; const btn = [...d.querySelectorAll('button')].find(b => String(b.textContent).trim() === '取消'); const br = btn.getBoundingClientRect(); return JSON.stringify({ scrollTop: Math.round(d.scrollTop), cancelY: Math.round(br.y), cancelVisible: br.y >= 0 && br.bottom <= innerHeight }); })()");
await c.send("Input.dispatchKeyEvent", { type: "keyDown", key: "Escape", code: "Escape", windowsVirtualKeyCode: 27 });
await c.send("Input.dispatchKeyEvent", { type: "keyUp", key: "Escape", code: "Escape", windowsVirtualKeyCode: 27 });
console.log(JSON.stringify(out, null, 1));
c.close(); process.exit(0);
