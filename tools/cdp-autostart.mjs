// Click the autostart toggle inside the real Tauri window, then report the switch state.
const PORT = Number(process.argv[2] || 9222);
const list = await (await fetch("http://127.0.0.1:" + PORT + "/json/list")).json();
const page = list.find(t => t.type === "page");
if (!page) { console.log("NO_PAGE"); process.exit(1); }
const ws = new WebSocket(page.webSocketDebuggerUrl);
let id = 0; const pending = new Map();
await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
ws.onmessage = ev => { const m = JSON.parse(ev.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result); } };
const send = (method, params = {}) => { const mid = ++id; ws.send(JSON.stringify({ id: mid, method, params })); return new Promise((res, rej) => pending.set(mid, { res, rej })); };
const ev = async expr => { const r = await send("Runtime.evaluate", { expression: expr, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error(r.exceptionDetails.text + " " + (r.exceptionDetails.exception?.description || "")); return r.result.value; };
const sleep = ms => new Promise(r => setTimeout(r, ms));

// 1. go to 设置
console.log("nav:", await ev(`(() => { const b = [...document.querySelectorAll('button')].find(x => x.textContent.trim() === '设置'); if (!b) return 'NOT_FOUND'; b.click(); return 'ok'; })()`));
await sleep(1500);

// 2. locate the autostart switch
const before = await ev(`(() => {
  const sw = [...document.querySelectorAll('[role="switch"]')].find(s => (s.textContent || '').includes('随系统启动'));
  if (!sw) return { found: false, switches: [...document.querySelectorAll('[role="switch"]')].map(s => s.textContent.trim()) };
  return { found: true, checked: sw.getAttribute('aria-checked'), disabled: sw.disabled, text: sw.textContent.trim() };
})()`);
console.log("before:", JSON.stringify(before));
if (!before.found || before.disabled) { console.log("CANNOT_TOGGLE"); ws.close(); process.exit(0); }

// 3. click it
console.log("click:", await ev(`(() => { const sw = [...document.querySelectorAll('[role="switch"]')].find(s => (s.textContent || '').includes('随系统启动')); sw.click(); return 'clicked'; })()`));
await sleep(2500);

const after = await ev(`(() => {
  const sw = [...document.querySelectorAll('[role="switch"]')].find(s => (s.textContent || '').includes('随系统启动'));
  const alert = [...document.querySelectorAll('div')].map(d => d.textContent.trim()).filter(t => t.includes('下次登录系统') || t.includes('当前未开启') || t.includes('仅在桌面客户端'));
  return { checked: sw?.getAttribute('aria-checked'), alert: alert.slice(0, 3) };
})()`);
console.log("after:", JSON.stringify(after));
ws.close();
process.exit(0);
