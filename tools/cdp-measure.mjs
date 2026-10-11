const PORT = Number(process.argv[2] || 9222);
const list = await (await fetch("http://127.0.0.1:" + PORT + "/json/list")).json();
const page = list.find(t => t.type === "page");
const ws = new WebSocket(page.webSocketDebuggerUrl);
let id = 0; const pending = new Map();
await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
ws.onmessage = ev => { const m = JSON.parse(ev.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result); } };
const send = (method, params = {}) => { const mid = ++id; ws.send(JSON.stringify({ id: mid, method, params })); return new Promise((res, rej) => pending.set(mid, { res, rej })); };
const ev = async expr => { const r = await send("Runtime.evaluate", { expression: expr, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error(r.exceptionDetails.text); return r.result.value; };
const sleep = ms => new Promise(r => setTimeout(r, ms));
const go = async label => { await ev(`(() => { const b=[...document.querySelectorAll('button')].find(x=>x.textContent.trim()==='${label}'); b&&b.click(); return 1; })()`); await sleep(1500); };

await go("模型");
console.log("MODELS:", JSON.stringify(await ev(`(() => {
  const scrollers = [...document.querySelectorAll('div')].filter(d => d.scrollWidth > d.clientWidth + 2 && d.clientWidth > 200);
  const tbl = document.querySelector('table');
  const ths = tbl ? [...tbl.querySelectorAll('thead th')].map(th => ({ t: th.innerText.trim(), w: Math.round(th.getBoundingClientRect().width) })) : [];
  return {
    windowInner: window.innerWidth,
    scrollers: scrollers.slice(0,4).map(d => ({ cls: d.className.slice(0,60), sw: d.scrollWidth, cw: d.clientWidth })),
    table: tbl ? { sw: tbl.scrollWidth, cw: tbl.clientWidth } : null,
    ths
  };
})()`)));

await go("日志");
console.log("LOGS:", JSON.stringify(await ev(`(() => {
  const tbl = document.querySelector('table');
  const ths = tbl ? [...tbl.querySelectorAll('thead th')].map(th => ({ t: th.innerText.trim(), w: Math.round(th.getBoundingClientRect().width) })) : [];
  const rows = tbl ? [...tbl.querySelectorAll('tbody tr')].slice(0,4).map(tr => Math.round(tr.getBoundingClientRect().height)) : [];
  const scrollers = [...document.querySelectorAll('div')].filter(d => d.scrollWidth > d.clientWidth + 2 && d.clientWidth > 200);
  return {
    windowInner: window.innerWidth,
    scrollers: scrollers.slice(0,4).map(d => ({ cls: d.className.slice(0,60), sw: d.scrollWidth, cw: d.clientWidth })),
    table: tbl ? { sw: tbl.scrollWidth, cw: tbl.clientWidth } : null,
    ths, rowHeights: rows
  };
})()`)));
ws.close(); process.exit(0);
