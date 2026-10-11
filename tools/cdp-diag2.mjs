const PORT = Number(process.argv[2] || 9223);
async function connect(u) {
  return new Promise((res, rej) => {
    const ws = new WebSocket(u); let id = 0; const p = new Map();
    ws.onopen = () => res({ send(m, a = {}) { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: a })); return new Promise((s, j) => p.set(i, { s, j })); }, close: () => ws.close() });
    ws.onerror = () => rej(new Error("ws error"));
    ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && p.has(m.id)) { const { s, j } = p.get(m.id); p.delete(m.id); m.error ? j(new Error(JSON.stringify(m.error))) : s(m.result); } };
  });
}
const list = await (await fetch("http://127.0.0.1:" + PORT + "/json/list")).json();
const c = await connect(list.find(t => t.type === "page").webSocketDebuggerUrl);
await c.send("Runtime.enable");
const raw = async (x) => { const r = await c.send("Runtime.evaluate", { expression: x, awaitPromise: true, returnByValue: true }); return r; };
for (const [name, expr] of [
  ["title", "document.title"],
  ["dialogCount", "document.querySelectorAll('[role=\"dialog\"]').length"],
  ["dialogsMap", "JSON.stringify([...document.querySelectorAll('[role=\"dialog\"]')].map(d => ({ t: String(d.innerText).slice(0,40) })))"],
  ["rects", "JSON.stringify([...document.querySelectorAll('[role=\"dialog\"]')].map(d => { const r = d.getBoundingClientRect(); return { w: Math.round(r.width), h: Math.round(r.height), x: Math.round(r.x), y: Math.round(r.y) }; }))"],
  ["innerTextOf", "(() => { const d = document.querySelector('[role=\"dialog\"]'); return d ? String(d.innerText).replace(/[\s\n]+/g, ' ').slice(0, 70) : 'NONE'; })()"],
  ["regexTest", "/[\\s\\n]+/.test('a b')"],
]) {
  const r = await raw(expr);
  if (r.exceptionDetails) console.log(name + " THREW: " + JSON.stringify(r.exceptionDetails).slice(0, 400));
  else console.log(name + " => " + JSON.stringify(r.result.value).slice(0, 300));
}
c.close(); process.exit(0);
