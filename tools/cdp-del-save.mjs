// 端到端：页面内 Modal 删除模型绑定 → 点顶栏「保存」→ 校验服务端已持久化。
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

await send("Page.reload", {}); await sleep(4000);
await ev(`(() => { const b=[...document.querySelectorAll('button')].find(x=>x.textContent.trim()==='模型'); b&&b.click(); return 'ok'; })()`);
await sleep(1500);
R.rows_before = await ev("document.querySelectorAll('main tbody tr').length");
R.dirty_before = await ev(`(() => { const t=[...document.querySelectorAll('main button')].find(b=>b.getAttribute('title')==='保存'); return t? (t.disabled?'disabled':'enabled') : 'NO_SAVE_BTN'; })()`);

// 找 gemini 那一行的删除按钮（第三列渠道含 Gemini 原生）
R.click_trash = await ev(`(() => {
  const rows=[...document.querySelectorAll('main tbody tr')];
  const row=rows.find(r=>r.textContent.includes('gemini-2.5-pro'));
  if(!row) return 'NO_ROW';
  const btns=[...row.querySelectorAll('button')];
  btns[btns.length-1].click(); return 'clicked';
})()`);
await sleep(600);
R.modal_open = await ev("!!document.querySelector('[data-test=\"confirm\"]')");
R.modal_text = await ev(`document.querySelector('[data-test="confirm"]')?.closest('div.fixed')?.textContent.replace(/\\s+/g,' ').trim().slice(0,80)`);
await ev(`document.querySelector('[data-test="confirm"]').click()`);
await sleep(700);
R.rows_after_delete = await ev("document.querySelectorAll('main tbody tr').length");
R.dirty_after_delete = await ev(`(() => { const t=[...document.querySelectorAll('main button')].find(b=>b.getAttribute('title')==='保存'); return t? (t.disabled?'disabled':'enabled') : 'NO_SAVE_BTN'; })()`);

// 点顶栏保存
R.click_save = await ev(`(() => { const t=[...document.querySelectorAll('main button')].find(b=>b.getAttribute('title')==='保存'); if(!t) return 'NO_BTN'; t.click(); return 'clicked'; })()`);
await sleep(2500);
R.toast_after_save = await ev(`[...document.querySelectorAll('body > div')].map(d=>d.textContent.replace(/\\s+/g,' ').trim()).filter(s=>s && s.length<60).slice(0,4)`);
R.alive = await ev("document.readyState");
console.log(JSON.stringify(R, null, 1));
ws.close(); process.exit(0);
