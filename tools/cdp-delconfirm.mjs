// 验证三个页面的删除确认 Modal 在真实 Tauri WebView2 里可用（window.confirm 已被替换）。
// 删除只改本地 state.cfg + markDirty()，不点「保存」就不会写服务端，测试后可安全刷新丢弃。
const PORT = 9222;
const list = await (await fetch("http://127.0.0.1:" + PORT + "/json/list")).json();
const page = list.find(t => t.type === "page");
const ws = new WebSocket(page.webSocketDebuggerUrl);
let id = 0; const pending = new Map();
await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
ws.onmessage = ev => { const m = JSON.parse(ev.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result); } };
const send = (method, params = {}) => { const mid = ++id; ws.send(JSON.stringify({ id: mid, method, params })); return new Promise((res, rej) => pending.set(mid, { res, rej })); };
const ev = async (expr, timeoutMs = 8000) => {
  const t = await Promise.race([send("Runtime.evaluate", { expression: expr, awaitPromise: true, returnByValue: true }), new Promise((_, rj) => setTimeout(() => rj(new Error("EVAL_TIMEOUT")), timeoutMs))]);
  if (t.exceptionDetails) return "EXC:" + t.exceptionDetails.text;
  return t.result.value;
};
const sleep = ms => new Promise(r => setTimeout(r, ms));

const nav = async (label) => {
  await ev(`(() => { const b=[...document.querySelectorAll('button')].find(x=>x.textContent.trim()==='${label}'); b&&b.click(); return 'ok'; })()`);
  await sleep(1200);
};
// 删除确认 Modal 挂在 body 上（Teleport），标题 + 两个 data-test 按钮
const modalState = () => ev(`(() => {
  const m=[...document.querySelectorAll('body > div')].find(d=>d.className.includes('fixed')&&d.textContent.includes('删除'));
  if (!m) return { open:false };
  const btns=[...m.querySelectorAll('button')];
  const confirm=btns.find(b=>b.getAttribute('data-test')==='confirm');
  const cancel=btns.find(b=>b.getAttribute('data-test')==='cancel');
  return { open:true, title:(m.querySelector('div')?.textContent||'').trim().slice(0,40),
           hasConfirm:!!confirm, hasCancel:!!cancel,
           confirmText:(confirm?.textContent||'').trim(),
           body:m.textContent.replace(/\\s+/g,' ').trim().slice(0,120) };
})()`);

const report = {};

/* ------------------------------ 1. 模型页 ------------------------------ */
await nav('模型');
report.models_rows_before = await ev("document.querySelectorAll('main tbody tr').length");
report.models_click_trash = await ev(`(() => {
  const rows=[...document.querySelectorAll('main tbody tr')];
  const last=rows[rows.length-1];
  if(!last) return 'NO_ROWS';
  const btns=[...last.querySelectorAll('button')];
  if(btns.length<2) return 'NO_BTNS:'+btns.length;
  window.__delName = last.querySelectorAll('td')[3]?.textContent.trim() || '?';
  btns[btns.length-1].click();
  return 'clicked:'+window.__delName;
})()`);
await sleep(600);
report.models_modal = await modalState();
report.models_rows_after_click = await ev("document.querySelectorAll('main tbody tr').length");
report.models_click_confirm = await ev(`(() => { const m=[...document.querySelectorAll('body > div')].find(d=>d.className.includes('fixed')&&d.textContent.includes('删除模型绑定')); const b=m&&[...m.querySelectorAll('button')].find(x=>x.getAttribute('data-test')==='confirm'); if(!b) return 'NO_CONFIRM_BTN'; b.click(); return 'clicked'; })()`);
await sleep(800);
report.models_rows_after_confirm = await ev("document.querySelectorAll('main tbody tr').length");
report.models_modal_closed = await ev(`(() => { return ![...document.querySelectorAll('body > div')].some(d=>d.className.includes('fixed')&&d.textContent.includes('删除模型绑定')); })()`);
report.models_alive_after = await ev("document.readyState + '/' + document.querySelectorAll('main tbody tr').length");

/* ------------------------------ 2. 渠道页 ------------------------------ */
await nav('渠道');
const rowSel = "main div.rounded-\\[10px\\]";
report.ch_rows_before = await ev(`document.querySelectorAll('main div').length && [...document.querySelectorAll('main div')].filter(d=>d.className.includes('rounded-[10px]')&&d.textContent.includes('权重')).length`);
report.ch_click_trash = await ev(`(() => {
  const rows=[...document.querySelectorAll('main div')].filter(d=>d.className.includes('rounded-[10px]')&&d.textContent.includes('权重'));
  const last=rows[rows.length-1]; if(!last) return 'NO_ROWS';
  const btns=[...last.querySelectorAll('button')];
  window.__chName = last.textContent.replace(/\\s+/g,' ').slice(0,30);
  btns[btns.length-1].click(); return 'clicked:'+window.__chName;
})()`);
await sleep(600);
report.ch_modal = await modalState();
report.ch_click_cancel = await ev(`(() => { const m=[...document.querySelectorAll('body > div')].find(d=>d.className.includes('fixed')&&d.textContent.includes('删除渠道')); const b=m&&[...m.querySelectorAll('button')].find(x=>x.getAttribute('data-test')==='cancel'); if(!b) return 'NO_CANCEL_BTN'; b.click(); return 'clicked'; })()`);
await sleep(600);
report.ch_cancel_closed = await ev(`!([...document.querySelectorAll('body > div')].some(d=>d.className.includes('fixed')&&d.textContent.includes('删除渠道')))`);
report.ch_rows_after_cancel = await ev(`[...document.querySelectorAll('main div')].filter(d=>d.className.includes('rounded-[10px]')&&d.textContent.includes('权重')).length`);

/* ------------------------------ 3. 分组页 ------------------------------ */
await nav('分组路由');
report.grp_click_trash = await ev(`(() => {
  const btns=[...document.querySelectorAll('main button')].filter(b=>b.querySelector('svg')&&b.className.includes('danger'));
  if(!btns.length) return 'NO_DANGER_BTN:'+[...document.querySelectorAll('main button')].length;
  btns[btns.length-1].click(); return 'clicked:'+btns.length;
})()`);
await sleep(600);
report.grp_modal = await modalState();
report.grp_alive_after = await ev("document.readyState");
report.grp_close = await ev(`(() => { const m=[...document.querySelectorAll('body > div')].find(d=>d.className.includes('fixed')&&d.textContent.includes('删除分组')); const b=m&&[...m.querySelectorAll('button')].find(x=>x.getAttribute('data-test')==='cancel'); if(!b) return 'NO_CANCEL_BTN'; b.click(); return 'clicked'; })()`);

console.log(JSON.stringify(report, null, 1));
ws.close(); process.exit(0);
