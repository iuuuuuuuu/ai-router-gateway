// Repeatability test for opening the member TSelect inside the group dialog.
// Answers: does a plain press+release reliably open it, or does it need a retry?
import fs from "node:fs/promises";
const PORT = Number(process.argv[2] || 9223);
async function connect(u) { return new Promise((res, rej) => { const ws = new WebSocket(u); let id = 0; const p = new Map(); ws.onopen = () => res({ send(m, a = {}) { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: a })); return new Promise((s, j) => p.set(i, { s, j })); }, close: () => ws.close() }); ws.onerror = () => rej(new Error("ws")); ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && p.has(m.id)) { const { s, j } = p.get(m.id); p.delete(m.id); m.error ? j(new Error(JSON.stringify(m.error))) : s(m.result); } }; }); }
const list = await (await fetch("http://127.0.0.1:" + PORT + "/json/list")).json();
const c = await connect(list.find(t => t.type === "page").webSocketDebuggerUrl);
const sleep = ms => new Promise(r => setTimeout(r, ms));
const ev = async (x) => { const r = await c.send("Runtime.evaluate", { expression: x, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error(r.exceptionDetails.text + " " + (r.exceptionDetails.exception?.description || "")); return r.result.value; };
const pt = async (type, x, y) => c.send("Input.dispatchMouseEvent", { type, x, y, button: type === "mouseMoved" ? "none" : "left", clickCount: type === "mouseMoved" ? 0 : 1 });
const box = async (expr) => ev(`(() => { const el = ${expr}; if (!el) return null; const b = el.getBoundingClientRect(); return { x: b.x + b.width / 2, y: b.y + b.height / 2 }; })()`);
const nOpts = async () => ev("document.querySelectorAll('[role=\"option\"]').length");
const esc = async () => { await c.send("Input.dispatchKeyEvent", { type: "keyDown", key: "Escape", code: "Escape", windowsVirtualKeyCode: 27 }); await c.send("Input.dispatchKeyEvent", { type: "keyUp", key: "Escape", code: "Escape", windowsVirtualKeyCode: 27 }); await sleep(250); };
const clickExpr = async (expr, wait = 450) => { const b = await box(expr); if (!b) return "NOT_FOUND"; await pt("mouseMoved", b.x, b.y); await pt("mousePressed", b.x, b.y); await sleep(60); await pt("mouseReleased", b.x, b.y); await sleep(wait); return "ok"; };
const nav = async (l) => { await clickExpr(`[...document.querySelectorAll('aside button')].find(x => x.textContent.trim() === '${l}')`, 750); };
const member = `(() => { const d = [...document.querySelectorAll('[role="dialog"]')].pop(); return d && d.querySelectorAll('[role="combobox"]')[0]; })()`;

const out = { rounds: [] };
for (let i = 1; i <= 3; i++) {
  await nav("分组路由");
  await clickExpr(`[...document.querySelectorAll('main button')].find(x => x.textContent.replace(/\\s+/g,' ').trim().startsWith('编辑路由'))`, 600);
  const r = { i };
  r.dialog = await ev("document.querySelectorAll('[role=\"dialog\"]').length");
  /* press-only probe */
  const b = await box(member);
  if (!b) { r.err = "NO_TRIGGER"; out.rounds.push(r); continue; }
  await pt("mouseMoved", b.x, b.y); await pt("mousePressed", b.x, b.y);
  await sleep(300);
  r.afterPress = await nOpts();
  await pt("mouseReleased", b.x, b.y);
  await sleep(400);
  r.afterRelease = await nOpts();
  if (r.afterRelease === 0) { await clickExpr(member, 450); r.afterRetry = await nOpts(); }
  else r.afterRetry = r.afterRelease;
  /* pick the 3rd option and confirm the trigger label changed */
  r.before = await ev(`(() => { const d = [...document.querySelectorAll('[role="dialog"]')].pop(); const t = d && d.querySelectorAll('[role="combobox"]')[0]; return t ? t.innerText.replace(/\\s+/g,' ').trim() : ''; })()`);
  const picked = await clickExpr(`[...document.querySelectorAll('[role="option"]')][2]`, 500);
  r.pick = picked;
  r.after = await ev(`(() => { const d = [...document.querySelectorAll('[role="dialog"]')].pop(); const t = d && d.querySelectorAll('[role="combobox"]')[0]; return t ? t.innerText.replace(/\\s+/g,' ').trim() : ''; })()`);
  r.openAfterPick = await nOpts();
  await esc();
  await clickExpr(`[...document.querySelectorAll('[role="dialog"] button')].find(x => x.textContent.trim() === '取消')`, 450);
  r.dialogAfterCancel = await ev("document.querySelectorAll('[role=\"dialog\"]').length");
  out.rounds.push(r);
}
console.log(JSON.stringify(out, null, 1));
c.close(); process.exit(0);
