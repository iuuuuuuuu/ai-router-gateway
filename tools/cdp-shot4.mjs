import { writeFileSync } from "node:fs";
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
const pages = [["模型", "v4-models"], ["渠道", "v4-channels"], ["分组路由", "v4-groups"], ["日志", "v4-logs"], ["统计", "v4-stats"], ["设置", "v4-settings"]];
for (const [label, name] of pages) {
  await ev("(() => { const b=[...document.querySelectorAll('button')].find(x=>x.textContent.trim()==='" + label + "'); b&&b.click(); return 'ok'; })()");
  await sleep(1400);
  const shot = await send("Page.captureScreenshot", { format: "png" });
  writeFileSync(name + ".png", Buffer.from(shot.data, "base64"));
  console.log(name + ".png ok");
}
ws.close(); process.exit(0);
