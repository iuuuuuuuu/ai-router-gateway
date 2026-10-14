import fs from "node:fs";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

// 扫「模板里用了、但 script setup 没导入」的组件。
//
// 原理：<script setup> 里没有 components 选项，模板里的 PascalCase 标签只有在能匹配到
// 顶层绑定（import 进来的组件名 / 变量）时才会被编译成直接引用；匹配不到时 Vue 编译器会
// 生成 _resolveComponent("X")，运行时去组件实例的 components 选项里找 —— 找不到就**退化成
// 原生自定义元素**原样渲染：没有 props、没有插槽出口、没有样式。
// 2026-10-14 的真实事故：client/src/pages/Groups.vue 的 import 行漏了 Modal，
// 结果删除分组的确认框被当成 <modal open="true" title="删除分组"> 原样渲染，
// 文案以纯文本贴在卡片下方，没有遮罩、没有「取消 / 删除」按钮 —— 用户点删除没有任何反应。
//
// ★本扫描器自己出过一次假阴性：正则里的 \w 被写成了 \\w（在正则字面量里等于
//   「反斜杠或字母 w」这个字符类），于是 _resolveComponent("Modal") 永远匹配不上，
//   扫描器一路报 0，把上面那个 bug 放到了线上。所以下面有 assertDetectorWorks() 正对照：
//   每跑一次都先证明「这个探测器确实能发现一个已知的坏样本」，否则直接以非 0 退出。

// ★路径按脚本自身位置推导：仓库里任何 clone、任何目录下都能直接跑。
const R = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const SFC = path.join(R, "client", "node_modules", "@vue", "compiler-sfc", "dist", "compiler-sfc.cjs.js");
const m = await import(pathToFileURL(SFC).href);

// ★单反斜杠：字符类里要的是「单词字符」。别改成 \\w。
const UNRESOLVED_RE = /_resolveComponent\("([A-Za-z_$][\w$]*)"\)/g;

function unresolvedIn(templateSource, bindings, filename) {
  const out = m.compileTemplate({
    source: templateSource,
    filename: filename,
    id: "x",
    compilerOptions: { bindingMetadata: bindings, prefixIdentifiers: true },
  });
  if (out.errors && out.errors.length) return { errors: out.errors, names: [] };
  const names = [...new Set([...out.code.matchAll(UNRESOLVED_RE)].map((x) => x[1]))];
  return { errors: [], names };
}

/* ------------------------- 正对照：探测器自证 ------------------------- */
function assertDetectorWorks() {
  const probe = '<template><Widget :open="x" title="t"><p>body</p></Widget></template>';
  const bad = unresolvedIn(probe, {}, "probe.vue");
  if (!bad.names.includes("Widget")) {
    console.log("SCANNER-BROKEN 正对照失败：探测器没能发现已知未解析组件 Widget（names=" + JSON.stringify(bad.names) + "）");
    process.exit(3);
  }
  const good = unresolvedIn('<template><Widget /></template>', { Widget: "setup-const" }, "probe2.vue");
  if (good.names.length) {
    console.log("SCANNER-BROKEN 负对照失败：已导入的组件被误报为未解析（names=" + JSON.stringify(good.names) + "）");
    process.exit(3);
  }
  console.log("scanner-selfcheck=ok (positive=Widget detected, negative=imported Widget clean)");
}
assertDetectorWorks();

/* ------------------------------- 全量扫描 ------------------------------- */
const root = path.join(R, "client", "src");
let files = [];
(function walk(d) {
  for (const e of fs.readdirSync(d, { withFileTypes: true })) {
    const p = path.join(d, e.name);
    if (e.isDirectory()) walk(p);
    else if (e.name.endsWith(".vue")) files.push(p);
  }
})(root);

let bad = 0;
for (const file of files) {
  const s = fs.readFileSync(file, "utf8");
  const sfc = m.parse(s, { filename: file });
  if (!sfc.descriptor.template) continue;
  let comp = { bindings: {} };
  if (sfc.descriptor.scriptSetup) {
    try {
      comp = m.compileScript(sfc.descriptor, { id: "x" });
    } catch (e) {
      console.log("SCRIPT-ERR " + path.relative(root, file) + " -> " + e.message);
      bad++;
      continue;
    }
  }
  const res = unresolvedIn(sfc.descriptor.template.content, comp.bindings, file);
  if (res.errors.length) {
    console.log("TEMPLATE-ERR " + path.relative(root, file) + " -> " + JSON.stringify(res.errors).slice(0, 300));
    bad++;
    continue;
  }
  if (res.names.length) {
    console.log("UNRESOLVED-COMPONENT " + path.relative(root, file) + " -> " + res.names.join(", "));
    bad++;
  }
}
console.log("files=" + files.length + " filesWithUnresolved=" + bad);
const scanExit = bad === 0 ? 0 : 2; // ★先不退出：后面还有 live-check，合并判定

/* ---------------------- 入库验收：真实事故的两向对照 ---------------------- */
// 每次运行都要真跑一遍「坏样本必须被抓到」，否则这个门禁没有意义。
// 坏样本 = 真实事故那一行去掉 Modal 之后的 import 行（把 Modal 换成 TotallyMissing）。
const LIVE_IMPORT_LINE = 'import { Badge, Btn, Card, Empty, IconBtn, Modal } from "../components/ui";';
const BROKEN_IMPORT_LINE = 'import { Badge, Btn, Card, Empty, IconBtn, TotallyMissing } from "../components/ui";';

function bindingsOfImportLine(src) {
  const names = [...src.matchAll(/import\s*\{([^}]*)\}\s*from/g)].flatMap((x) =>
    x[1].split(",").map((s) => s.trim()).filter(Boolean)
  );
  const b = {};
  for (const n of names) b[n] = "setup-const";
  return b;
}

const groupsPath = path.join(R, "client", "src", "pages", "Groups.vue");
const groupsSrc = fs.readFileSync(groupsPath, "utf8");
const liveHasModal = groupsSrc.includes(LIVE_IMPORT_LINE);
const modalUsed = /<Modal\b/.test(groupsSrc);

// A) 事故样本：同一份 Groups.vue，只把 Modal 从 import 行里拿掉 —— 必须报 Modal。
const brokenBindings = bindingsOfImportLine(BROKEN_IMPORT_LINE);
const brokenRes = unresolvedIn(groupsSrc.slice(groupsSrc.indexOf("<template>")), brokenBindings, "Groups.broken.vue");
const brokenDetected = brokenRes.names.includes("Modal");

// B) 真实文件：Modal 确实在用、且确实被导入 —— 必须干净。
const realBindings = bindingsOfImportLine(groupsSrc);
const realRes = unresolvedIn(groupsSrc.slice(groupsSrc.indexOf("<template>")), realBindings, "Groups.vue");
const realClean = realRes.names.length === 0;

const ok = liveHasModal && modalUsed && brokenDetected && realClean;
console.log(
  "live-check " +
    (ok ? "ok" : "FAILED") +
    " (importLine=" + (liveHasModal ? "has Modal" : "MISSING Modal") +
    ", <Modal> used=" + modalUsed +
    ", remove-import repro=" + (brokenDetected ? "Modal reported" : "NOT reported") +
    ", real file=" + (realClean ? "clean" : "dirty:" + realRes.names.join(",")) + ")"
);
if (!ok) {
  console.log("LIVE-CHECK-FAILED 真事故的两向对照没通过，拒绝以成功退出");
  process.exit(4);
}

process.exit(scanExit);

