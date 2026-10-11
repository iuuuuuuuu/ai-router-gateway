// Verify the page Transition really runs, by installing a MutationObserver on
// class attributes BEFORE clicking a nav item. A point-in-time probe can miss
// the 100-160ms window entirely.
import fs from "node:fs/promises";
const PORT = Number(process.argv[2] || 9223);
async function connect(wsUrl) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(wsUrl); let id = 0; const pending = new Map();
    ws.onopen = () => resolve({
      send(m, p = {}) { const mid = ++id; ws.send(JSON.stringify({ id: mid, method: m, params: p })); return new Promise((res, rej) => pending.set(mid, { res, rej })); },
      close: () => ws.close(),
    });
    ws.onerror = () => reject(new Error("ws error"));
    ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const { res, rej } = pending.get(m.id); pending.delete(m.id); m.error ? rej(new Error(JSON.stringify(m.error))) : res(m.result); } };
  });
}
const list = await (await fetch("http://127.0.0.1:" + PORT + "/json/list")).json();
const c = await connect(list.find(t => t.type === "page").webSocketDebuggerUrl);
const sleep = ms => new Promise(r => setTimeout(r, ms));
const ev = async (expr) => { const r = await c.send("Runtime.evaluate", { expression: expr, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error(r.exceptionDetails.text + " " + (r.exceptionDetails.exception?.description || "")); return r.result.value; };
const click = async (expr) => {
  const r = await ev(`(() => { const el = ${expr}; if (!el) return null; const b = el.getBoundingClientRect(); if (!b.width) return null; return { x: b.x + b.width / 2, y: b.y + b.height / 2 }; })()`);
  if (!r) return "NOT_FOUND";
  await c.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: r.x, y: r.y, button: "none", clickCount: 0 });
  await c.send("Input.dispatchMouseEvent", { type: "mousePressed", x: r.x, y: r.y, button: "left", clickCount: 1 });
  await sleep(50);
  await c.send("Input.dispatchMouseEvent", { type: "mouseReleased", x: r.x, y: r.y, button: "left", clickCount: 1 });
  return "ok";
};
const out = {};
out.mainExists = await ev("!!document.querySelector('main')");
out.animWrapper = await ev(`(() => { const d = document.querySelector('main > div'); return d ? d.className : 'NONE'; })()`);
out.install = await ev(`(() => {
  window.__anim = [];
  const t0 = performance.now();
  window.__obs && window.__obs.disconnect();
  window.__obs = new MutationObserver(muts => {
    for (const m of muts) {
      const cls = m.target.className;
      if (typeof cls === 'string' && cls.includes('page-')) window.__anim.push({ t: Math.round(performance.now() - t0), cls });
    }
  });
  window.__obs.observe(document.body, { subtree: true, attributes: true, attributeFilter: ['class'] });
  return 'installed';
})()`);
await click(`[...document.querySelectorAll('aside button')].find(x => x.textContent.trim() === '统计')`);
await sleep(700);
out.animLog = await ev("JSON.stringify(window.__anim)");
out.afterNav = await ev("document.querySelector('main > div') ? document.querySelector('main > div').className : 'NONE'");
out.install2 = await ev(`(() => { window.__anim = []; const t0 = performance.now(); window.__t0 = t0; return 'reset'; })()`);
await click(`[...document.querySelectorAll('aside button')].find(x => x.textContent.trim() === '日志')`);
await sleep(700);
out.animLog2 = await ev("JSON.stringify(window.__anim)");
/* also confirm reduced-motion is not silently killing it */
out.reducedMotion = await ev("matchMedia('(prefers-reduced-motion: reduce)').matches");
out.cssHasRules = await ev(`(() => { const out = []; for (const ss of document.styleSheets) { try { for (const r of ss.cssRules) if (r.selectorText && r.selectorText.includes('.page-')) out.push(r.cssText.slice(0, 120)); } catch (e) {} } return JSON.stringify(out); })()`);
console.log(JSON.stringify(out, null, 1));
c.close(); process.exit(0);
