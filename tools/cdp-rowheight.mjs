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
await sleep(1200);
const R = {};
R.cellHeights = await ev(`(() => {
  const tr = document.querySelector('main tbody tr');
  const cs = getComputedStyle(tr);
  return { rowH: Math.round(tr.getBoundingClientRect().height), lineHeight: cs.lineHeight, fontSize: cs.fontSize,
    cells: [...tr.children].map((td, i) => {
      const el = td.firstElementChild;
      const st = getComputedStyle(td);
      return { i, tdH: Math.round(td.getBoundingClientRect().height), padT: st.paddingTop, padB: st.paddingBottom, padL: st.paddingLeft,
        inner: el ? { tag: el.tagName, h: Math.round(el.getBoundingClientRect().height), cls: (el.className||'').slice(0,50) } : null };
    }) };
})()`);
console.log(JSON.stringify(R, null, 1));
ws.close(); process.exit(0);
