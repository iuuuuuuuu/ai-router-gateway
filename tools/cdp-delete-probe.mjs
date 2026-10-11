// Does clicking the trash icon on a channel row actually delete it?
const PORT = 9222;
const list = await (await fetch("http://127.0.0.1:" + PORT + "/json/list")).json();
const page = list.find(t => t.type === "page");
const ws = new WebSocket(page.webSocketDebuggerUrl);
let id = 0; const pending = new Map();
await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
ws.onmessage = ev => { const m = JSON.parse(ev.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result); } };
const send = (method, params = {}) => { const mid = ++id; ws.send(JSON.stringify({ id: mid, method, params })); return new Promise((res, rej) => pending.set(mid, { res, rej })); };
const ev = async (expr, timeoutMs = 6000) => {
  const t = await Promise.race([send("Runtime.evaluate", { expression: expr, awaitPromise: true, returnByValue: true }), new Promise((_, rj) => setTimeout(() => rj(new Error("EVAL_TIMEOUT")), timeoutMs))]);
  if (t.exceptionDetails) return "EXC:" + t.exceptionDetails.text;
  return t.result.value;
};
const sleep = ms => new Promise(r => setTimeout(r, ms));

console.log("nav_channels=" + await ev(`(() => { const b=[...document.querySelectorAll('button')].find(x=>x.textContent.trim()==='渠道'); b&&b.click(); return 'ok'; })()`));
await sleep(1500);
const before = await ev(`(() => { const rows=[...document.querySelectorAll('main div')].filter(d=>d.className.includes('rounded-[10px]')&&d.textContent.includes('权重')); return rows.length; })()`);
console.log("rows_before=" + before);
console.log("click_trash=" + await ev(`(() => {
  const rows=[...document.querySelectorAll('main div')].filter(d=>d.className.includes('rounded-[10px]')&&d.textContent.includes('权重'));
  const last = rows[rows.length-1];
  if (!last) return 'NO_ROWS';
  const btns=[...last.querySelectorAll('button')];
  const del = btns[btns.length-1];
  if (!del) return 'NO_BTN';
  window.__delRowText = last.textContent.replace(/\\s+/g,' ').slice(0,60);
  del.click();
  return 'clicked:' + window.__delRowText;
})()`, 6000));
await sleep(3000);
console.log("rows_after=" + await ev(`(() => { const rows=[...document.querySelectorAll('main div')].filter(d=>d.className.includes('rounded-[10px]')&&d.textContent.includes('权重')); return rows.length; })()`, 6000));
console.log("page_still_alive=" + await ev("document.readyState", 6000));
ws.close(); process.exit(0);
