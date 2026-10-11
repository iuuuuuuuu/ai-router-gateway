const PORT = 9222;
const list = await (await fetch("http://127.0.0.1:" + PORT + "/json/list")).json();
const page = list.find(t => t.type === "page");
const ws = new WebSocket(page.webSocketDebuggerUrl);
let id = 0; const pending = new Map();
await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result); } };
const send = (method, params = {}) => { const mid = ++id; ws.send(JSON.stringify({ id: mid, method, params })); return new Promise((res, rej) => pending.set(mid, { res, rej })); };
const ev = async (expr, timeoutMs = 10000) => {
  const t = await Promise.race([send("Runtime.evaluate", { expression: expr, awaitPromise: true, returnByValue: true }), new Promise((_, rj) => setTimeout(() => rj(new Error("EVAL_TIMEOUT")), timeoutMs))]);
  if (t.exceptionDetails) return "EXC:" + t.exceptionDetails.text;
  return t.result.value;
};
const sleep = ms => new Promise(r => setTimeout(r, ms));
const R = {};
// 顶栏按钮清单（含 disabled 与 text）
R.top_btns = await ev(`[...document.querySelectorAll('button')].filter(b=>!b.closest('main')).map(b=>({t:b.textContent.trim().slice(0,8), title:b.getAttribute('title')||'', dis:b.disabled})).slice(0,12)`);
R.save_click = await ev(`(() => { const b=[...document.querySelectorAll('button')].find(x=>x.textContent.trim()==='保存'&&!x.closest('main')); if(!b) return 'NO_BTN'; if(b.disabled) return 'DISABLED'; b.click(); return 'clicked'; })()`);
await sleep(3000);
R.toasts = await ev(`[...document.querySelectorAll('body > div')].map(d=>d.textContent.replace(/\\s+/g,' ').trim()).filter(s=>s&&s.length<80).slice(0,5)`);
R.rows_after_save = await ev("document.querySelectorAll('main tbody tr').length");
R.save_btn_now = await ev(`(() => { const b=[...document.querySelectorAll('button')].find(x=>x.textContent.trim()==='保存'&&!x.closest('main')); return b? (b.disabled?'disabled':'enabled') : 'NO_BTN'; })()`);
console.log(JSON.stringify(R, null, 1));
ws.close(); process.exit(0);
