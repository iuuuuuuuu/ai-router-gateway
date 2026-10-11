const r = await fetch("http://127.0.0.1:9090/admin/api/config");
console.log("content-type=" + r.headers.get("content-type"));
const cfg = await r.json();
console.log("channels=" + cfg.channels.map(c => c.id + ":" + c.name).join(" | "));
console.log("groups=" + cfg.groups.map(g => g.id + ":" + g.name).join(" | "));
console.log("keys=" + cfg.api_keys.map(k => k.key.slice(0,14) + ":" + k.name).join(" | "));
