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
R.geom = await ev(`(() => {
  const box = document.querySelector('main .overflow-x-auto');
  const tbl = box.querySelector('table');
  const ths = [...tbl.querySelectorAll('thead th')].map(t => Math.round(t.getBoundingClientRect().width));
  const rows = [...tbl.querySelectorAll('tbody tr')];
  const rowH = rows.map(r => Math.round(r.getBoundingClientRect().height));
  return { boxClient: box.clientWidth, boxScroll: box.scrollWidth, tableW: Math.round(tbl.getBoundingClientRect().width), ths, rowHmin: Math.min(...rowH), rowHmax: Math.max(...rowH), rows: rows.length };
})()`);
// 每个 td 内最外层 inline 元素是否折行（用 getClientRects().length）
R.cells = await ev(`(() => {
  const out = [];
  [...document.querySelectorAll('main tbody tr')].forEach((tr, ri) => {
    [...tr.children].forEach((td, ci) => {
      const el = td.firstElementChild;
      const rects = el ? el.getClientRects().length : 0;
      if (rects > 1) out.push({ ri, ci, rects, txt: (el.textContent||'').replace(/\\s+/g,' ').trim().slice(0,24), cls: (el.className||'').slice(0,60) });
    });
  });
  return out;
})()`);
// 渠道列 Badge 实测
R.channelBadges = await ev(`(() => {
  const rows = [...document.querySelectorAll('main tbody tr')];
  return rows.map(tr => {
    const td = tr.children[2];
    const b = td.querySelector('span');
    if (!b) return null;
    const r = b.getBoundingClientRect();
    return { txt: b.textContent.trim(), w: Math.round(r.width), h: Math.round(r.height), rects: b.getClientRects().length, tdW: Math.round(td.getBoundingClientRect().width), clip: td.scrollWidth > td.clientWidth };
  }).filter(Boolean);
})()`);
// 价格输入框是否被裁剪
R.inputs = await ev(`(() => {
  const ins = [...document.querySelectorAll('main tbody input')];
  return { n: ins.length, minW: Math.min(...ins.map(i => Math.round(i.getBoundingClientRect().width))), clipped: ins.filter(i => i.scrollWidth > i.clientWidth + 1).length, vals: ins.slice(0,4).map(i=>i.value) };
})()`);
// 操作列右边界
R.ops = await ev(`(() => {
  const box = document.querySelector('main .overflow-x-auto');
  const b = box.getBoundingClientRect();
  const btns = [...box.querySelectorAll('tbody tr:first-child td:last-child button')].map(x => Math.round(x.getBoundingClientRect().right));
  return { boxRight: Math.round(b.right), btnRights: btns };
})()`);
console.log(JSON.stringify(R, null, 1));
ws.close(); process.exit(0);
