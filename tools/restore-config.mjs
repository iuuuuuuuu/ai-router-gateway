import { readFileSync } from "node:fs";
const raw = readFileSync("run/cfg-backup.json");
console.log("first4=" + [...raw.slice(0, 4)].join(","));
let txt = raw.toString("utf8").replace(/^\uFEFF/, "");
const cfg = JSON.parse(txt);
console.log("models=" + cfg.models.length + " keys=" + Object.keys(cfg).join("|"));
const res = await fetch("http://127.0.0.1:9090/admin/api/config", {
  method: "PUT",
  headers: { "content-type": "application/json; charset=utf-8" },
  body: JSON.stringify(cfg),
});
console.log("PUT status=" + res.status + " body=" + (await res.text()).slice(0, 200));
const after = await (await fetch("http://127.0.0.1:9090/admin/api/config")).json();
console.log("AFTER models=" + after.models.length + " ids=" + after.models.map(m => m.id).join(","));
console.log("AFTER channels=" + after.channels.length + " groups=" + after.groups.length + " keys=" + after.api_keys.length);
