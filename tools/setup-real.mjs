
import { readFileSync, writeFileSync } from "node:fs";
const ADMIN = "http://127.0.0.1:9090";
const cfg = JSON.parse(readFileSync("tools/real-config.json", "utf8"));
const r = await fetch(ADMIN + "/admin/api/config", { method: "PUT", headers: { "content-type": "application/json" }, body: JSON.stringify(cfg) });
console.log("PUT", r.status, (await r.text()).slice(0, 300));
const back = await fetch(ADMIN + "/admin/api/config");
const b = await back.json();
console.log("READBACK groups:", (b.groups||[]).map(g => g.id + "(" + (g.tiers||[]).length + "tiers)").join(", "));
console.log("READBACK channels:", (b.channels||[]).length, "models:", (b.models||[]).length, "keys:", (b.api_keys||[]).length);
