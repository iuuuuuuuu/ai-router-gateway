// Probe whether window.confirm() actually works inside the packaged Tauri window.
const PORT = Number(process.argv[2] || 9222);
const list = await (await fetch("http://127.0.0.1:" + PORT + "/json/list")).json();
const page = list.find(t => t.type === "page");
if (!page) { console.log("NO_PAGE"); process.exit(1); }
const ws = new WebSocket(page.webSocketDebuggerUrl);
let id = 0; const pending = new Map();
await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
ws.onmessage = ev => { const m = JSON.parse(ev.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result); } };
const send = (method, params = {}) => { const mid = ++id; ws.send(JSON.stringify({ id: mid, method, params })); return new Promise((res, rej) => pending.set(mid, { res, rej })); };
const ev = async expr => { const r = await send("Runtime.evaluate", { expression: expr, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error(r.exceptionDetails.text); return r.result.value; };
const sleep = ms => new Promise(r => setTimeout(r, ms));

console.log("typeof_confirm=" + await ev("typeof window.confirm"));
console.log("confirm_src=" + String(await ev("String(window.confirm).slice(0,120)")));
// arm a deferred confirm so the evaluate returns before the dialog blocks the renderer
console.log("arm=" + await ev("(() => { window.__probe = 'PENDING'; setTimeout(() => { try { window.__probe = 'RETURNED:' + String(window.confirm('PROBE_CONFIRM')); } catch (e) { window.__probe = 'THREW:' + e.message; } }, 0); return 'armed'; })()"));
await sleep(2500);
let val = "UNREADABLE";
try { val = String(await ev("window.__probe")); } catch (e) { val = "EVAL_BLOCKED:" + e.message.slice(0, 80); }
console.log("probe_after_2500ms=" + val);
ws.close();
process.exit(0);
