const PORT = 9222;
const list = await (await fetch("http://127.0.0.1:" + PORT + "/json/list")).json();
const page = list.find(t => t.type === "page");
const ws = new WebSocket(page.webSocketDebuggerUrl);
let id = 0; const pending = new Map();
await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result); } };
const send = (method, params = {}) => { const mid = ++id; ws.send(JSON.stringify({ id: mid, method, params })); return new Promise((res, rej) => pending.set(mid, { res, rej })); };
const ev = async (expr, timeoutMs = 8000) => {
  const t = await Promise.race([send("Runtime.evaluate", { expression: expr, awaitPromise: true, returnByValue: true }), new Promise((_, rj) => setTimeout(() => rj(new Error("EVAL_TIMEOUT")), timeoutMs))]);
  if (t.exceptionDetails) return "EXC:" + t.exceptionDetails.text;
  return t.result.value;
};
const sleep = ms => new Promise(r => setTimeout(r, ms));
await ev(`(() => { const b=[...document.querySelectorAll('button')].find(x=>x.textContent.trim()==='模型'); b&&b.click(); return 'ok'; })()`);
await sleep(1500);
const m = await ev(`(() => {
  const box=document.querySelector('main .overflow-x-auto');
  const tbl=document.querySelector('main table');
  const ths=[...document.querySelectorAll('main thead th')].map(t=>({w:Math.round(t.getBoundingClientRect().width), txt:t.textContent.trim()}));
  const r0=[...document.querySelectorAll('main tbody tr')][0];
  const cells=[...r0.children].map((td,i)=>({i, w:Math.round(td.getBoundingClientRect().width), sw:td.scrollWidth, cw:td.clientWidth, clip:td.scrollWidth>td.clientWidth+1, txt:td.textContent.replace(/\\s+/g,' ').trim().slice(0,26)}));
  return { winW: window.innerWidth, boxClient:box?.clientWidth, boxScroll:box?.scrollWidth, tableW:Math.round(tbl.getBoundingClientRect().width), ths, cells, rowH:Math.round(r0.getBoundingClientRect().height) };
})()`);
console.log(JSON.stringify(m, null, 1));
ws.close(); process.exit(0);
