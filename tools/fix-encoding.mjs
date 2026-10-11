const B = "http://127.0.0.1:9090/admin/api";
const cfg = await (await fetch(B + "/config")).json();
const fixed = [];
function walk(o, path) {
  if (typeof o === "string") {
    if (/[^\x00-\x7F]/.test(o)) {
      const f = Buffer.from(o, "latin1").toString("utf8");
      if (!f.includes("\uFFFD") && f !== o) { fixed.push({ path, from: o, to: f }); return f; }
    }
    return o;
  }
  if (Array.isArray(o)) return o.map((v, i) => walk(v, path + "[" + i + "]"));
  if (o && typeof o === "object") { const out = {}; for (const k of Object.keys(o)) out[k] = walk(o[k], path + "." + k); return out; }
  return o;
}
const rep = walk(cfg, "cfg");
console.log("fixed " + fixed.length + " strings:");
for (const f of fixed) console.log("  " + f.path + ": " + JSON.stringify(f.from) + " -> " + JSON.stringify(f.to));
const res = await fetch(B + "/config", { method: "PUT", headers: { "content-type": "application/json; charset=utf-8" }, body: JSON.stringify(rep) });
console.log("PUT status=" + res.status + " body=" + (await res.text()));
const after = await (await fetch(B + "/config")).json();
console.log("VERIFY channels=" + after.channels.map(c => c.name).join(" | "));
console.log("VERIFY groups=" + after.groups.map(g => g.name).join(" | "));
console.log("VERIFY keys=" + after.api_keys.map(k => k.name).join(" | "));
let bad = 0;
(function chk(o) { if (typeof o === "string") { if (/[\u00C0-\u00FF]/.test(o)) bad++; } else if (Array.isArray(o)) o.forEach(chk); else if (o && typeof o === "object") Object.values(o).forEach(chk); })(after);
console.log("remaining non-ascii latin1-range strings=" + bad);
// 重新写一份正确备份
const { writeFileSync } = await import("node:fs");
writeFileSync("run/cfg-backup.json", JSON.stringify(after, null, 2), "utf8");
console.log("backup rewritten, bytes=" + JSON.stringify(after).length);
