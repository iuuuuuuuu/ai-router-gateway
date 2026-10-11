const URL = "http://127.0.0.1:9090/admin/api/config";
const cfg = await (await fetch(URL)).json();
console.log("BEFORE name=" + JSON.stringify(cfg.channels[0].name));
console.log("latin1->utf8 = " + JSON.stringify(Buffer.from(cfg.channels[0].name, "latin1").toString("utf8")));
// 探针：直接写入正确 UTF-8 中文
const probe = JSON.parse(JSON.stringify(cfg));
probe.channels[0].name = "官方直连";
const res = await fetch(URL, { method: "PUT", headers: { "content-type": "application/json; charset=utf-8" }, body: JSON.stringify(probe) });
console.log("PUT status=" + res.status);
const after = await (await fetch(URL)).json();
console.log("AFTER  name=" + JSON.stringify(after.channels[0].name));
console.log("match=" + (after.channels[0].name === "官方直连"));
