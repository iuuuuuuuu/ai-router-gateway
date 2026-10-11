// v2：精确检测页面内删除确认 Modal（排除 toast 干扰），并覆盖渠道/分组页。
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

// Modal 判定：body 下 class 含 z-50 的遮罩，且内部有 data-test 按钮（toast 没有）
const MODAL = `(() => {
  const all=[...document.querySelectorAll('body > div')].filter(d=>d.className.includes('z-50'));
  const m=all.find(d=>d.querySelector('[data-test="confirm"],[data-test="cancel"]'));
  if(!m) return { open:false, z50:all.length };
  return { open:true, z50:all.length,
    head:(m.querySelector('div')?.textContent||'').trim().slice(0,30),
    hasConfirm:!!m.querySelector('[data-test="confirm"]'),
    hasCancel:!!m.querySelector('[data-test="cancel"]'),
    body:m.textContent.replace(/\\s+/g,' ').trim().slice(0,140) };
})()`;

const nav = async (label) => {
  await ev(`(() => { const b=[...document.querySelectorAll('button')].find(x=>x.textContent.trim()==='${label}'); b&&b.click(); return 'ok'; })()`);
  await sleep(1300);
};
const clickTest = (t) => ev(`(() => { const b=document.querySelector('[data-test="${t}"]'); if(!b) return 'NO_BTN'; b.click(); return 'clicked'; })()`);

const report = {};
await send("Page.reload", {});
await sleep(3500);
report.after_reload = await ev("document.readyState");

/* ------------------------------ 1. 渠道页 ------------------------------ */
await nav('渠道');
const chRows = () => ev(`[...document.querySelectorAll('main div')].filter(d=>d.className.includes('rounded-[10px]')&&d.textContent.includes('权重')).length`);
report.ch_rows_before = await chRows();
report.ch_click_trash = await ev(`(() => {
  const rows=[...document.querySelectorAll('main div')].filter(d=>d.className.includes('rounded-[10px]')&&d.textContent.includes('权重'));
  const last=rows[rows.length-1]; if(!last) return 'NO_ROWS';
  const btns=[...last.querySelectorAll('button')];
  window.__ch = last.textContent.replace(/\\s+/g,' ').slice(0,24);
  btns[btns.length-1].click(); return 'clicked:'+window.__ch;
})()`);
await sleep(500);
report.ch_modal = await ev(MODAL);
report.ch_confirm_click = await clickTest("confirm");
await sleep(700);
report.ch_modal_closed = await ev("!document.querySelector('[data-test=\"confirm\"]')");
report.ch_rows_after_confirm = await chRows();
report.ch_alive = await ev("document.readyState");
report.ch_toast = await ev(`(() => { const t=[...document.querySelectorAll('body > div')].map(d=>d.textContent.trim()).filter(s=>s.includes('删除')); return t.slice(0,3); })()`);

/* ------------------------------ 2. 分组页 ------------------------------ */
await nav('分组路由');
report.grp_btn_titles = await ev(`[...document.querySelectorAll('main button')].map(b=>b.getAttribute('title')||b.textContent.trim().slice(0,6)).slice(0,40)`);
report.grp_cards_before = await ev(`[...document.querySelectorAll('main div')].filter(d=>d.className.includes('rounded-xl')&&d.textContent.includes('故障转移层级')).length`);
report.grp_click_trash = await ev(`(() => {
  const cards=[...document.querySelectorAll('main div')].filter(d=>d.className.includes('rounded-xl')&&d.textContent.includes('故障转移层级'));
  const card=cards[0]; if(!card) return 'NO_CARD';
  const btns=[...card.querySelectorAll('button')].filter(b=>b.querySelector('svg')&&!b.textContent.trim());
  if(!btns.length) return 'NO_ICONBTN:'+card.querySelectorAll('button').length;
  btns[btns.length-1].click(); return 'clicked_iconbtns='+btns.length;
})()`);
await sleep(500);
report.grp_modal = await ev(MODAL);
report.grp_cancel_click = await clickTest("cancel");
await sleep(600);
report.grp_modal_closed = await ev("!document.querySelector('[data-test=\"cancel\"]')");
report.grp_alive = await ev("document.readyState");

/* ------------------------------ 3. 模型页（复核） ------------------------------ */
await nav('模型');
report.mdl_rows_before = await ev("document.querySelectorAll('main tbody tr').length");
report.mdl_click_trash = await ev(`(() => {
  const rows=[...document.querySelectorAll('main tbody tr')];
  const last=rows[rows.length-1]; if(!last) return 'NO_ROWS';
  const btns=[...last.querySelectorAll('button')];
  btns[btns.length-1].click(); return 'clicked';
})()`);
await sleep(500);
report.mdl_modal = await ev(MODAL);
report.mdl_cancel_click = await clickTest("cancel");
await sleep(600);
report.mdl_modal_closed = await ev("!document.querySelector('[data-test=\"cancel\"]')");
report.mdl_rows_after = await ev("document.querySelectorAll('main tbody tr').length");
report.mdl_alive = await ev("document.readyState");

console.log(JSON.stringify(report, null, 1));
ws.close(); process.exit(0);
