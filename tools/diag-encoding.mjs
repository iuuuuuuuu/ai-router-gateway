const B = "http://127.0.0.1:9090/admin/api";
const cfg = await (await fetch(B + "/config")).json();
const bad = [];
function walk(o, path) {
  if (typeof o === "string") { if (/[^\x00-\x7F]/.test(o)) bad.push({ path, val: o, fixed: Buffer.from(o, "latin1").toString("utf8") }); return; }
  if (Array.isArray(o)) { o.forEach((v, i) => walk(v, path + "[" + i + "]")); return; }
  if (o && typeof o === "object") { for (const k of Object.keys(o)) walk(o[k], path + "." + k); return; }
}
walk(cfg, "cfg");
console.log("non-ascii strings in config: " + bad.length);
for (const b of bad) console.log("  " + b.path + "  raw=" + JSON.stringify(b.val) + "  fixed=" + JSON.stringify(b.fixed));
// 日志接口是否也乱码
const sw = await (await fetch(B + "/logs/switch?limit=3")).json();
console.log("switch sample: " + JSON.stringify(sw).slice(0, 500));
const rq = await (await fetch(B + "/logs/requests?limit=2")).json();
console.log("request sample: " + JSON.stringify(rq).slice(0, 400));
