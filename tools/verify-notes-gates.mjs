#!/usr/bin/env node
/**
 * 笔记门禁的「元门禁」：对三条校验脚本逐条做故障注入，证明它们真的会变红。
 *
 * 为什么需要它：本仓库禁止假绿门禁（docs/AGENTS.md §5.1）。一条恒 exit 0 的校验
 * 脚本与一条真的校验脚本，在「跑绿了」这件事上长得一模一样。只有先注入故障、
 * 看见它变红，才有资格说它是绿的。
 *
 * 用法（仓库根）：node tools/verify-notes-gates.mjs
 * 退出码：全部注入都按预期变红、对照副本为绿 → 0；任何一条没按预期 → 1。
 *
 * 无本机路径、无网络、无外部依赖；只在系统临时目录里建副本，跑完即删。
 * 注入样本是**自合成的**，不读仓库里的真实笔记 —— 笔记树可以是空的，
 * 元门禁必须照样能证明门禁会红。
 */
import { execFileSync } from "node:child_process";
import { cpSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const scriptsDir = join(repoRoot, ".agents", "skills", "agent-notes", "scripts");

const SAMPLE_NAME = "2026-10-17-sample.md";

/**
 * 合成一篇「格式合法」的 implemented 笔记。
 *
 * 刻意不读仓库里的真实笔记：笔记树允许为空，而元门禁的职责是证明
 * 「三条校验脚本会红」，这件事与仓库当前有没有笔记无关。早期版本从
 * `implemented/process/` 里挑一篇真笔记当样本，清空笔记树后它直接崩了 ——
 * 元门禁随被检查对象一起腐烂，正是它要防的那种假。
 */
function sampleText(title = "注入样本", body = "样本正文。") {
  return [
    `# Agent Note: ${title}`,
    "",
    "Status: implemented",
    "",
    "## Problem",
    "",
    "元门禁用的合成样本。",
    "",
    "## Decision",
    "",
    body,
    "",
    "## Alternatives considered",
    "",
    "- **不合成样本，改用仓库里的真笔记。** 理由是真笔记天然符合格式，样本不会写错；"
      + "但笔记树允许为空，清空后元门禁会因为没有样本而崩，等于让被检查对象决定检查器能不能跑。",
    "",
    "## Consequences",
    "",
    "- **收益**：元门禁与笔记树内容解耦。",
    "- **代价**：样本与真实笔记的格式会漂移；靠 `verify-agent-note-format.ts` 的对照步骤兜住。",
    "",
  ].join("\n");
}

const work = mkdtempSync(join(tmpdir(), "notes-gate-inject-"));
let failures = 0;

/** 在临时目录里跑一条校验脚本；返回 {code, out}。 */
function runGate(script, notesRoot) {
  try {
    const out = execFileSync(process.execPath, [join(scriptsDir, script)], {
      cwd: repoRoot,
      env: { ...process.env, AGENT_NOTE_ROOT: notesRoot },
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    });
    return { code: 0, out };
  } catch (err) {
    return { code: err.status ?? 1, out: `${err.stdout ?? ""}${err.stderr ?? ""}` };
  }
}

/** 重建一份干净笔记副本：合成一篇格式合法的样本笔记。 */
function seed() {
  const root = join(work, "notes");
  rmSync(root, { recursive: true, force: true });
  mkdirSync(join(root, "implemented", "process"), { recursive: true });
  writeFileSync(join(root, "implemented", "process", SAMPLE_NAME), sampleText(), "utf8");
  return root;
}

function check(label, script, notesRoot, expect) {
  const { code, out } = runGate(script, notesRoot);
  const isRed = code !== 0;
  const ok = expect === "red" ? isRed : !isRed;
  const first = out.split("\n").find((l) => l.trim() !== "") ?? "";
  console.log(`${ok ? "PASS" : "FAIL"}  ${expect.padEnd(5)} ${label}`);
  if (!ok) {
    failures += 1;
    console.log(`      exit=${code}  ${first}`);
  } else if (expect === "red") {
    console.log(`      exit=${code}  ${first}`);
  }
}

function append(file, text) {
  writeFileSync(file, readFileSync(file, "utf8") + text, "utf8");
}

try {
  // 0. 对照：未经注入的合成副本必须全绿。若这一步就红，后面所有「红」都不算数。
  //    同时它也是「合成的样本本身格式合法」的证明 —— 样本写错会让每条注入
  //    都「红得对但红错了原因」。
  {
    const root = seed();
    check("control: 未注入的合成样本", "verify-agent-note-tree.ts", root, "green");
    check("control: 未注入的合成样本", "verify-agent-note-format.ts", root, "green");
  }

  // 1. tree —— 未知 class 目录（空目录不构成笔记，必须放一篇进去）
  {
    const root = seed();
    mkdirSync(join(root, "implemented", "refactor"), { recursive: true });
    writeFileSync(join(root, "implemented", "refactor", "2026-10-17-x.md"), "# Agent Note: x\n\nStatus: implemented\n\n## Problem\n\nx\n", "utf8");
    check("未知 class 目录 implemented/refactor/", "verify-agent-note-tree.ts", root, "red");
  }

  // 2. tree —— 文件名不合规
  {
    const root = seed();
    cpSync(join(root, "implemented", "process", SAMPLE_NAME), join(root, "implemented", "process", "notes.md"));
    check("文件名不是 yyyy-mm-dd-topic.md", "verify-agent-note-tree.ts", root, "red");
  }

  // 3. tree —— 笔记间相对链接死链
  {
    const root = seed();
    append(join(root, "implemented", "process", SAMPLE_NAME), "\n见 [不存在的笔记](../../implemented/process/2099-01-01-nope.md)\n");
    check("指向不存在笔记的相对链接", "verify-agent-note-tree.ts", root, "red");
  }

  // 4. format —— 缺 ## Alternatives considered
  {
    const root = seed();
    const f = join(root, "implemented", "process", SAMPLE_NAME);
    writeFileSync(f, readFileSync(f, "utf8").replace(/^## Alternatives considered$/m, "## Something else"), "utf8");
    check("缺 ## Alternatives considered", "verify-agent-note-format.ts", root, "red");
  }

  // 5. format —— implemented 里出现提案式标题
  {
    const root = seed();
    append(join(root, "implemented", "process", SAMPLE_NAME), "\n## Proposal\n\n不该出现。\n");
    check("implemented 里出现 ## Proposal", "verify-agent-note-format.ts", root, "red");
  }

  // 6. format —— Status 与 lifecycle 不符
  {
    const root = seed();
    const f = join(root, "implemented", "process", SAMPLE_NAME);
    writeFileSync(f, readFileSync(f, "utf8").replace("Status: implemented", "Status: proposed"), "utf8");
    check("Status 与 lifecycle 不符", "verify-agent-note-format.ts", root, "red");
  }

  // 7. archived —— 有归档笔记但没有 manifest
  {
    const root = join(work, "notes");
    rmSync(root, { recursive: true, force: true });
    mkdirSync(join(root, "archived", "process"), { recursive: true });
    writeFileSync(join(root, "archived", "process", "2026-10-01-x.md"), "# Agent Note: 假的归档\n\nStatus: implemented\nArchived: 2026-10-17\n\n## Problem\n\nx\n", "utf8");
    check("归档笔记缺 manifest.json", "verify-archived-agent-notes.ts", root, "red");
  }

  // 8. archived —— 封印后篡改正文
  {
    const root = join(work, "notes");
    try {
      execFileSync(process.execPath, [join(scriptsDir, "verify-archived-agent-notes.ts"), "--write"], {
        cwd: repoRoot,
        env: { ...process.env, AGENT_NOTE_ROOT: root },
        stdio: "ignore",
      });
    } catch {
      /* --write 失败本身会在下一步表现为红 */
    }
    append(join(root, "archived", "process", "2026-10-01-x.md"), "\n篡改\n");
    check("已封印归档笔记被篡改", "verify-archived-agent-notes.ts", root, "red");
  }

  // 9. archived —— 头部布局错（Archived 与 Status 之间空行）
  {
    const root = join(work, "notes");
    rmSync(root, { recursive: true, force: true });
    mkdirSync(join(root, "archived", "process"), { recursive: true });
    writeFileSync(join(root, "archived", "process", "2026-10-01-x.md"), "# Agent Note: 假的归档\n\nStatus: implemented\n\nArchived: 2026-10-17\n\n## Problem\n\nx\n", "utf8");
    check("Archived 行没紧邻 Status", "verify-archived-agent-notes.ts", root, "red");
  }

  // 10. 元注入：把校验脚本换成恒 exit 0 的桩，注入器必须报 FAIL。
  //     没有这一步，本脚本自己就可能是假绿（永远打印 PASS）。
  {
    const stubDir = join(work, "stub");
    mkdirSync(stubDir, { recursive: true });
    const stub = join(stubDir, "verify-agent-note-tree.ts");
    writeFileSync(stub, "process.exit(0)\n", "utf8");
    const root = seed();
    mkdirSync(join(root, "implemented", "refactor"), { recursive: true });
    writeFileSync(join(root, "implemented", "refactor", "2026-10-17-x.md"), "# Agent Note: x\n\nStatus: implemented\n\n## Problem\n\nx\n", "utf8");
    let stubExit = 0;
    try {
      execFileSync(process.execPath, [stub], { stdio: "ignore" });
    } catch (err) {
      stubExit = err.status ?? 1;
    }
    console.log(`${stubExit === 0 ? "PASS" : "FAIL"}  meta  恒 exit 0 的桩确认恒绿（否则本注入器无法区分真假）`);
    if (stubExit !== 0) failures += 1;
  }
} finally {
  rmSync(work, { recursive: true, force: true });
}

console.log();
if (failures === 0) {
  console.log("ok: 所有故障注入都按预期变红，对照副本为绿 —— 三条笔记门禁不是假绿");
  process.exit(0);
}
console.log(`FAIL: ${failures} 条注入没有按预期表现 —— 门禁可能是假绿`);
process.exit(1);
