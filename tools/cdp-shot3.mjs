import { writeFileSync } from "node:fs";
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
const shot = async (name) => {
  const r = await send("Page.captureScreenshot", { format: "png" });
  writeFileSync(name, Buffer.from(r.data, "base64"));
  console.log("shot " + name);
};
const nav = async (label) => { await ev(`(() => { const b=[...document.querySelectorAll('button')].find(x=>x.textContent.trim()==='${label}'); b&&b.click(); return 'ok'; })()`); await sleep(1300); };

await send("Page.reload", {}); await sleep(3500);

// 渠道删除确认
await nav('渠道');
await ev(`(() => { const rows=[...document.querySelectorAll('main div')].filter(d=>d.className.includes('rounded-[10px]')&&d.textContent.includes('权重')); rows[rows.length-1].querySelectorAll('button').length && [...rows[rows.length-1].querySelectorAll('button')].pop().click(); return 'ok'; })()`);
await sleep(700); await shot("v3-confirm-channels.png");
await ev(`document.querySelector('[data-test="cancel"]').click()`); await sleep(600);

// 分组删除确认
await nav('分组路由');
await ev(`(() => { const cards=[...document.querySelectorAll('main div')].filter(d=>d.className.includes('rounded-xl')&&d.textContent.includes('故障转移层级')); const btns=[...cards[0].querySelectorAll('button')].filter(b=>b.querySelector('svg')&&!b.textContent.trim()); btns[btns.length-1].click(); return 'ok'; })()`);
await sleep(700); await shot("v3-confirm-groups.png");
await ev(`document.querySelector('[data-test="cancel"]').click()`); await sleep(600);

// 模型删除确认
await nav('模型');
await ev(`(() => { const rows=[...document.querySelectorAll('main tbody tr')]; const btns=[...rows[rows.length-1].querySelectorAll('button')]; btns[btns.length-1].click(); return 'ok'; })()`);
await sleep(700); await shot("v3-confirm-models.png");
await ev(`document.querySelector('[data-test="cancel"]').click()`); await sleep(600);

// 修复后的表格页（Models task-7 / Logs task-8）
await nav('模型'); await shot("v3-models-table.png");
await nav('日志'); await sleep(1200); await shot("v3-logs-table.png");

// 表格实测
const m = await ev(`(() => {
  const box=document.querySelector('main .overflow-x-auto');
  const t=document.querySelector('main table');
  const ths=[...document.querySelectorAll('main thead th')].map(t=>Math.round(t.getBoundingClientRect().width));
  return { boxScroll:box?.scrollWidth, boxClient:box?.clientWidth, tableW:Math.round(t?.getBoundingClientRect().width||0), ths };
})()`);
console.log("models_box=" + JSON.stringify(m));
console.log("DONE");
ws.close(); process.exit(0);
