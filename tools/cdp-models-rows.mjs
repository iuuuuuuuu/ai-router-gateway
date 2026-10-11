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
await ev(`(() => { const b=[...document.querySelectorAll('button')].find(x=>x.textContent.trim()==='模型'); b&&b.click(); return 'ok'; })()`);
await sleep(1500);
const R = {};
R.rows = await ev(`[...document.querySelectorAll('main tbody tr')].map(r => {
  const tds=[...r.children];
  const tall=tds.map((td,i)=>({i, h:Math.round(td.getBoundingClientRect().height), wrap: td.scrollHeight > Math.ceil(parseFloat(getComputedStyle(td).lineHeight||'0'))+2, txt:td.textContent.replace(/\\s+/g,' ').trim().slice(0,20)}));
  return { rowH: Math.round(r.getBoundingClientRect().height), tall: tall.filter(t=>t.h>34) };
})`);
// 渠道列的 Badge 是否换行
R.badges = await ev(`[...document.querySelectorAll('main tbody tr')].map(r => {
  const b=r.querySelector('span');
  const badges=[...r.querySelectorAll('span')].filter(s=>s.className.includes('rounded')&&s.className.includes('px-'));
  return badges.map(s=>({t:s.textContent.trim(), h:Math.round(s.getBoundingClientRect().height), w:Math.round(s.getBoundingClientRect().width), ws:getComputedStyle(s).whiteSpace}));
})`);
console.log(JSON.stringify(R, null, 1));
ws.close(); process.exit(0);
