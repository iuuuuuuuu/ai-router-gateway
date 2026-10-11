import { writeFileSync } from "node:fs";
const cfg = await (await fetch("http://127.0.0.1:9090/admin/api/config")).json();
console.log("ch_real_pro.base_url =", cfg.channels.find(c => c.id === "ch_real_pro").base_url);
const sl = await (await fetch("http://127.0.0.1:9090/admin/api/logs/switch?limit=12")).json();
const rows = Array.isArray(sl) ? sl : (sl.rows || sl.logs || []);
for (const r of rows.slice(0, 12)) console.log(JSON.stringify({ t: r.ts, req: (r.request_id || "").slice(0, 8), g: r.group_id, ch: r.channel_id, tier: r.tier, a: r.attempt_no, out: r.outcome, ec: r.error_class, hs: r.http_status, um: r.upstream_model }));
