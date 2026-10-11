// tools/verify-window-state.mjs
//
// Real-instance acceptance for "the Tauri client remembers its window size and
// position" (tauri-plugin-window-state).
//
// Two hard-won facts, both learned by failing first:
//   1) Do NOT resize through CDP. Browser.setWindowBounds makes window.outerWidth
//      report the new size while the real HWND never moves (WebView2 only updates
//      its own bookkeeping), so the plugin saves the old size and the harness
//      misreads a working feature as broken. Geometry here is always read/written
//      through Win32 (GetWindowRect/GetClientRect/SetWindowPos).
//   2) gw-client owns TWO top-level HWNDs: the real main window (title
//      "AI gateway console") and an untitled, invisible ~1920x1029 window.
//      Picking by largest area selects the wrong one; prefer a titled window.
//   Also: the 1100x720 in tauri.conf.json is the CLIENT size (outer frame is
//   1116x759) and the plugin persists inner_size, so every assertion uses
//   GetClientRect's cw/ch.
//
// Paths covered:
//   1) clean baseline -> launch -> client area is the tauri.conf.json default
//   2) SetWindowPos to client 1280x860 -> graceful exit via quit.flag (RunEvent::Exit save)
//   3) relaunch -> client area restored to 1280x860
//   4) resize to 1200x800 -> real title-bar close (WM_CLOSE -> CloseRequested:
//      hide to tray + save) -> process alive + window hidden + state file written
//   5) relaunch -> restored to 1200x800
//   6) launch with --from-autostart -> main window stays hidden
//
// Usage: node tools/verify-window-state.mjs [cdpPort] [releaseDir]
// Exit code: 0 = PASS, 1 = FAIL (the JSON on stdout carries every raw number).
// The Win32 probe lives in %TEMP%\gw-winprobe\ (winprobe.ps1 + winprobe.cs, both
// pure ASCII: Windows PowerShell 5.1 reads them in the ANSI code page, so any
// non-ASCII byte in the C# breaks Add-Type). Drive it by hand with:
//   powershell -File <probe> -ProcId <pid> -Op list|geo|settle|resize|close

import { spawn } from "node:child_process";
import { existsSync, mkdirSync, openSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const PORT = Number(process.argv[2] || process.env.CDP_PORT || 9223);
const DIR = resolve(process.argv[3] || "client/src-tauri/target/release");
const EXE = resolve(DIR, "gw-client.exe");
const APPDATA = process.env.APPDATA || "";
const STATE_FILE = join(APPDATA, "com.gawaty.client", ".window-state.json");
const QUIT_FLAG = resolve(DIR, "quit.flag");
const PS_EXE = join(process.env.SystemRoot || "C:\\Windows", "System32", "WindowsPowerShell", "v1.0", "powershell.exe");
const PROBE_DIR = join(tmpdir(), "gw-winprobe");
const PROBE_PS1 = join(PROBE_DIR, "winprobe.ps1");
const PROBE_CS = join(PROBE_DIR, "winprobe.cs");

const DEFAULT_CLIENT = { w: 1100, h: 720 }; // tauri.conf.json app.windows[0]
const CLIENT_A = { w: 1280, h: 860, x: 120, y: 90 };
const CLIENT_B = { w: 1200, h: 800, x: 200, y: 160 };
const TOL = 4; // client-size tolerance in px

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const j = (o) => JSON.stringify(o);
const log = (...a) => console.log(...a);

/* ------------------------------------------------------------------ *
 * Win32 probe source (pure ASCII, no escapes - see header note)
 * ------------------------------------------------------------------ */
const PROBE_CS_SOURCE = [
  "using System;",
  "using System.Runtime.InteropServices;",
  "using System.Text;",
  "using System.Threading;",
  "",
  "public class WinProbe",
  "{",
  "    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);",
  "",
  "    [DllImport(\"user32.dll\")] public static extern bool EnumWindows(EnumProc cb, IntPtr lParam);",
  "    [DllImport(\"user32.dll\")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);",
  "    [DllImport(\"user32.dll\")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT r);",
  "    [DllImport(\"user32.dll\")] public static extern bool GetClientRect(IntPtr hWnd, out RECT r);",
  "    [DllImport(\"user32.dll\")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int cx, int cy, uint flags);",
  "    [DllImport(\"user32.dll\", CharSet = CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr hWnd, StringBuilder s, int n);",
  "    [DllImport(\"user32.dll\")] public static extern bool IsWindowVisible(IntPtr hWnd);",
  "    [DllImport(\"user32.dll\")] public static extern bool IsIconic(IntPtr hWnd);",
  "    [DllImport(\"user32.dll\")] public static extern bool PostMessageW(IntPtr hWnd, uint msg, IntPtr wp, IntPtr lp);",
  "",
  "    public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }",
  "",
  "    const uint WM_CLOSE = 0x0010;",
  "    const uint SWP_NOZORDER = 0x0004;",
  "    const uint SWP_NOACTIVATE = 0x0010;",
  "",
  "    const char QUOTE = (char)34;",
  "    const char SLASH = (char)92;",
  "",
  "    static string Q(string s)",
  "    {",
  "        if (s == null) return QUOTE.ToString() + QUOTE.ToString();",
  "        StringBuilder b = new StringBuilder();",
  "        b.Append(QUOTE);",
  "        for (int i = 0; i < s.Length; i++)",
  "        {",
  "            char c = s[i];",
  "            if (c == QUOTE) { b.Append(SLASH); b.Append(QUOTE); }",
  "            else if (c == SLASH) { b.Append(SLASH); b.Append(SLASH); }",
  "            else if (c < ' ') { b.Append(' '); }",
  "            else { b.Append(c); }",
  "        }",
  "        b.Append(QUOTE);",
  "        return b.ToString();",
  "    }",
  "",
  "    static string F(string k, long v) { return Q(k) + \":\" + v.ToString(); }",
  "    static string FB(string k, bool v) { return Q(k) + \":\" + (v ? \"true\" : \"false\"); }",
  "    static string FS(string k, string v) { return Q(k) + \":\" + Q(v); }",
  "    static string Obj(params string[] parts) { return \"{\" + string.Join(\",\", parts) + \"}\"; }",
  "",
  "    static string GeometryJson(IntPtr h)",
  "    {",
  "        RECT r; GetWindowRect(h, out r);",
  "        RECT c; GetClientRect(h, out c);",
  "        return string.Join(\",\", new string[] {",
  "            F(\"x\", r.Left), F(\"y\", r.Top),",
  "            F(\"w\", r.Right - r.Left), F(\"h\", r.Bottom - r.Top),",
  "            F(\"cw\", c.Right - c.Left), F(\"ch\", c.Bottom - c.Top)",
  "        });",
  "    }",
  "",
  "    static string WindowJson(IntPtr h)",
  "    {",
  "        StringBuilder t = new StringBuilder(512);",
  "        GetWindowTextW(h, t, 512);",
  "        return Obj(",
  "            F(\"hwnd\", h.ToInt64()),",
  "            FS(\"title\", t.ToString()),",
  "            FB(\"hasTitle\", t.Length > 0),",
  "            FB(\"visible\", IsWindowVisible(h)),",
  "            FB(\"minimized\", IsIconic(h)),",
  "            GeometryJson(h));",
  "    }",
  "",
  "    public static string ListJson(uint pid)",
  "    {",
  "        StringBuilder b = new StringBuilder();",
  "        b.Append(\"[\");",
  "        int[] n = new int[1];",
  "        EnumWindows(delegate(IntPtr h, IntPtr l) {",
  "            uint p; GetWindowThreadProcessId(h, out p);",
  "            if (p != pid) return true;",
  "            if (n[0] > 0) b.Append(\",\");",
  "            b.Append(WindowJson(h));",
  "            n[0]++;",
  "            return true;",
  "        }, IntPtr.Zero);",
  "        b.Append(\"]\");",
  "        return b.ToString();",
  "    }",
  "",
  "    // Main window: prefer titled, then visible, then largest. gw-client owns a",
  "    // second untitled invisible top-level window, so area alone picks wrong.",
  "    static long MainHwnd(uint pid)",
  "    {",
  "        long[] best = new long[] { 0 };",
  "        long[] bestArea = new long[] { -1 };",
  "        int[] bestScore = new int[] { -1 };",
  "        EnumWindows(delegate(IntPtr h, IntPtr l) {",
  "            uint p; GetWindowThreadProcessId(h, out p);",
  "            if (p != pid) return true;",
  "            RECT r; GetWindowRect(h, out r);",
  "            long w = r.Right - r.Left;",
  "            long hh = r.Bottom - r.Top;",
  "            if (w < 100 || hh < 100) return true;",
  "            long area = w * hh;",
  "            StringBuilder t = new StringBuilder(512);",
  "            GetWindowTextW(h, t, 512);",
  "            int score = 0;",
  "            if (t.Length > 0) score += 4;",
  "            if (IsWindowVisible(h)) score += 2;",
  "            if (score > bestScore[0] || (score == bestScore[0] && area > bestArea[0]))",
  "            {",
  "                bestScore[0] = score; bestArea[0] = area; best[0] = h.ToInt64();",
  "            }",
  "            return true;",
  "        }, IntPtr.Zero);",
  "        return best[0];",
  "    }",
  "",
  "    public static string GeometryJsonOfPid(uint pid)",
  "    {",
  "        long target = MainHwnd(pid);",
  "        if (target == 0) return Obj(FS(\"error\", \"no main window for pid\"));",
  "        IntPtr h = new IntPtr(target);",
  "        return Obj(F(\"hwnd\", target), GeometryJson(h),",
  "            FB(\"visible\", IsWindowVisible(h)), FB(\"minimized\", IsIconic(h)));",
  "    }",
  "",
  "    // Wait inside C# until the real geometry stops moving (two equal samples) so",
  "    // one powershell spawn covers a whole wait instead of one per poll.",
  "    public static string SettleJson(uint pid, int timeoutMs)",
  "    {",
  "        int waited = 0;",
  "        int same = 0;",
  "        string lastKey = null;",
  "        string last = null;",
  "        while (waited < timeoutMs)",
  "        {",
  "            long t = MainHwnd(pid);",
  "            if (t != 0)",
  "            {",
  "                IntPtr h = new IntPtr(t);",
  "                RECT r; GetWindowRect(h, out r);",
  "                RECT c; GetClientRect(h, out c);",
  "                string key = (c.Right - c.Left) + \"x\" + (c.Bottom - c.Top) + \"@\" + r.Left + \",\" + r.Top + \":\" + IsWindowVisible(h);",
  "                if (key == lastKey) same++; else { same = 0; lastKey = key; }",
  "                last = WindowJson(h);",
  "                if (same >= 1) return Obj(FB(\"settled\", true), F(\"waitedMs\", waited), Q(\"window\") + \":\" + last);",
  "            }",
  "            Thread.Sleep(400);",
  "            waited += 400;",
  "        }",
  "        if (last == null) return Obj(FB(\"settled\", false), F(\"waitedMs\", waited), FS(\"error\", \"no main window\"));",
  "        return Obj(FB(\"settled\", false), F(\"waitedMs\", waited), Q(\"window\") + \":\" + last);",
  "    }",
  "",
  "    // x,y = outer position; cw,ch = wanted CLIENT size (frame added here).",
  "    public static string MoveJson(uint pid, int x, int y, int cw, int ch)",
  "    {",
  "        long target = MainHwnd(pid);",
  "        if (target == 0) return Obj(FS(\"error\", \"no main window for pid\"));",
  "        IntPtr h = new IntPtr(target);",
  "        string before = Obj(GeometryJson(h));",
  "        RECT b; GetWindowRect(h, out b);",
  "        RECT c; GetClientRect(h, out c);",
  "        int frameW = (b.Right - b.Left) - (c.Right - c.Left);",
  "        int frameH = (b.Bottom - b.Top) - (c.Bottom - c.Top);",
  "        bool ok = SetWindowPos(h, IntPtr.Zero, x, y, cw + frameW, ch + frameH, SWP_NOZORDER | SWP_NOACTIVATE);",
  "        Thread.Sleep(800);",
  "        return Obj(FB(\"moved\", ok), F(\"hwnd\", target), F(\"frameW\", frameW), F(\"frameH\", frameH),",
  "            Q(\"before\") + \":\" + before, Q(\"after\") + \":\" + Obj(GeometryJson(h)), FB(\"visibleAfter\", IsWindowVisible(h)));",
  "    }",
  "",
  "    // Real title-bar close: WM_CLOSE -> Tauri CloseRequested (app hides to tray).",
  "    public static string CloseJson(uint pid)",
  "    {",
  "        long target = MainHwnd(pid);",
  "        if (target == 0) return Obj(FS(\"error\", \"no main window for pid\"));",
  "        IntPtr h = new IntPtr(target);",
  "        string before = Obj(GeometryJson(h));",
  "        bool ok = PostMessageW(h, WM_CLOSE, IntPtr.Zero, IntPtr.Zero);",
  "        Thread.Sleep(1500);",
  "        return Obj(FB(\"posted\", ok), F(\"hwnd\", target), Q(\"before\") + \":\" + before, FB(\"visibleAfter\", IsWindowVisible(h)));",
  "    }",
  "}",
  ""
].join("\n");

const PROBE_PS1_SOURCE = [
  "param(",
  "  [Parameter(Mandatory=$true)][int]$ProcId,",
  "  [string]$Op = \"list\",",
  "  [int]$X = -1,",
  "  [int]$Y = -1,",
  "  [int]$W = 0,",
  "  [int]$H = 0,",
  "  [int]$TimeoutMs = 25000",
  ")",
  "$ErrorActionPreference = \"Stop\"",
  "Add-Type -Path (Join-Path $PSScriptRoot \"winprobe.cs\")",
  "switch ($Op) {",
  "  \"list\"   { [WinProbe]::ListJson([uint32]$ProcId) }",
  "  \"geo\"    { [WinProbe]::GeometryJsonOfPid([uint32]$ProcId) }",
  "  \"settle\" { [WinProbe]::SettleJson([uint32]$ProcId, $TimeoutMs) }",
  "  \"resize\" { [WinProbe]::MoveJson([uint32]$ProcId, $X, $Y, $W, $H) }",
  "  \"close\"  { [WinProbe]::CloseJson([uint32]$ProcId) }",
  "  default  { '{\"error\":\"unknown op\"}' }",
  "}",
  ""
].join("\n");

if (!existsSync(PROBE_DIR)) mkdirSync(PROBE_DIR, { recursive: true });
writeFileSync(PROBE_CS, PROBE_CS_SOURCE, "ascii");
writeFileSync(PROBE_PS1, PROBE_PS1_SOURCE, "ascii");

function ps(args, timeoutMs = 90000) {
  return new Promise((res) => {
    const p = spawn(PS_EXE, ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", PROBE_PS1, ...args],
      { stdio: ["ignore", "pipe", "pipe"], windowsHide: true });
    let out = "", err = "";
    p.stdout.on("data", (d) => (out += d));
    p.stderr.on("data", (d) => (err += d));
    const timer = setTimeout(() => { try { p.kill(); } catch {} }, timeoutMs);
    p.on("close", (code) => {
      clearTimeout(timer);
      const t = out.trim();
      let parsed = null;
      try { parsed = JSON.parse(t); } catch {}
      res({ code, raw: t, parsed, err: err.trim() });
    });
  });
}

const probeList = (pid) => ps(["-ProcId", String(pid), "-Op", "list"]);
const probeSettle = (pid, timeoutMs) => ps(["-ProcId", String(pid), "-Op", "settle", "-TimeoutMs", String(timeoutMs)]);
const probeResize = (pid, b) => ps(["-ProcId", String(pid), "-Op", "resize", "-X", String(b.x ?? -1), "-Y", String(b.y ?? -1), "-W", String(b.w), "-H", String(b.h)]);
const probeClose = (pid) => ps(["-ProcId", String(pid), "-Op", "close"]);

/** Wait for the real window, then for its geometry to stop moving. */
async function settle(pid, timeoutMs = 30000) {
  const r = await probeSettle(pid, timeoutMs);
  if (!r.parsed) return { error: "probe failed", raw: r.raw.slice(0, 400), stderr: r.err.slice(0, 600), exit: r.code };
  return r.parsed;
}

async function windowInventory(pid) {
  const r = await probeList(pid);
  return Array.isArray(r.parsed) ? r.parsed : { error: "list failed", raw: r.raw.slice(0, 300), stderr: r.err.slice(0, 400) };
}

/* ------------------------------------------------------------------ *
 * process / CDP helpers
 * ------------------------------------------------------------------ */
function isAlive(pid) {
  if (!pid) return false;
  try { process.kill(pid, 0); return true; } catch { return false; }
}

function launch(extraArgs = []) {
  const env = {};
  for (const [k, v] of Object.entries(process.env)) {
    if (/^(http_proxy|https_proxy|no_proxy|all_proxy)$/i.test(k)) continue;
    env[k] = v;
  }
  env.WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=" + PORT;
  const out = openSync(resolve(DIR, "client.log"), "a");
  const err = openSync(resolve(DIR, "client.err.log"), "a");
  return spawn(EXE, extraArgs, { cwd: DIR, env, detached: false, stdio: ["ignore", out, err] });
}

async function waitCdp(timeoutMs = 30000) {
  const t0 = Date.now();
  while (Date.now() - t0 < timeoutMs) {
    try {
      const list = await (await fetch("http://127.0.0.1:" + PORT + "/json/list")).json();
      const page = list.find((t) => t.type === "page");
      if (page && page.webSocketDebuggerUrl) return page;
    } catch {}
    await sleep(300);
  }
  return null;
}

async function connect(url) {
  const ws = new WebSocket(url);
  let id = 0;
  const pending = new Map();
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = () => rej(new Error("ws error")); });
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id && pending.has(m.id)) {
      const p = pending.get(m.id); pending.delete(m.id);
      m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result);
    }
  };
  const send = (method, params = {}) => {
    const mid = ++id;
    ws.send(JSON.stringify({ id: mid, method, params }));
    return new Promise((res, rej) => pending.set(mid, { res, rej }));
  };
  const evalJs = async (expr) => {
    const r = await send("Runtime.evaluate", { expression: expr, awaitPromise: true, returnByValue: true });
    if (r.exceptionDetails) throw new Error(r.exceptionDetails.text);
    return r.result.value;
  };
  return { evalJs, close: () => ws.close() };
}

/** Page-side innerWidth/innerHeight: cross-check against the real client area. */
async function pageInner() {
  const target = await waitCdp(10000);
  if (!target) return null;
  try {
    const c = await connect(target.webSocketDebuggerUrl);
    const v = await c.evalJs("({ innerW: window.innerWidth, innerH: window.innerHeight, outerW: window.outerWidth, outerH: window.outerHeight, dpr: devicePixelRatio, title: document.title })");
    c.close();
    return v;
  } catch (e) { return { error: String((e && e.message) || e) }; }
}

async function quitViaFlag(pid) {
  writeFileSync(QUIT_FLAG, "quit\n");
  const t0 = Date.now();
  while (Date.now() - t0 < 15000) {
    if (!isAlive(pid)) return { quit: true, ms: Date.now() - t0, via: "quit.flag" };
    await sleep(400);
  }
  spawn("taskkill", ["/PID", String(pid), "/F", "/T"], { stdio: "ignore", windowsHide: true });
  const t1 = Date.now();
  while (Date.now() - t1 < 10000) { if (!isAlive(pid)) break; await sleep(400); }
  return { quit: !isAlive(pid), ms: Date.now() - t0, via: "taskkill-fallback", forced: true };
}

function readState() {
  if (!existsSync(STATE_FILE)) return { exists: false, path: STATE_FILE };
  const raw = readFileSync(STATE_FILE, "utf8");
  let parsed = null, parseError = null;
  try { parsed = JSON.parse(raw); } catch (e) { parseError = String(e); }
  return { exists: true, path: STATE_FILE, raw, parsed, parseError, mtimeMs: statSync(STATE_FILE).mtimeMs };
}

const near = (a, b, tol = TOL) => Math.abs(a - b) <= tol;
const clientIs = (w, want) => !!w && near(w.cw, want.w) && near(w.ch, want.h);
const savedIs = (s, want) => !!s && near(s.width, want.w, 1) && near(s.height, want.h, 1);

/* ------------------------------------------------------------------ */
async function main() {
  const out = { exe: EXE, stateFile: STATE_FILE, cdpPort: PORT, probeDir: PROBE_DIR, defaults: DEFAULT_CLIENT, steps: [] };
  const fail = (why) => { out.verdict = "FAIL"; out.failReason = why; log(j(out)); process.exit(1); };

  if (!existsSync(EXE)) return fail("exe not found: " + EXE);

  // ---- 0. clean baseline ----
  const pre = readState();
  if (pre.exists) rmSync(STATE_FILE, { force: true });
  if (existsSync(QUIT_FLAG)) rmSync(QUIT_FLAG, { force: true });
  out.steps.push({ step: "baseline", stateFileExisted: pre.exists, preRaw: pre.raw || null });

  // ---- 1. first launch: client area is the tauri.conf.json default ----
  const p1 = launch();
  const s1 = await settle(p1.pid, 35000);
  if (!s1 || !s1.window) return fail("launch1: no real window; " + j(s1));
  out.launch1 = { pid: p1.pid, realWindow: s1.window, settleMs: s1.waitedMs, pageInner: await pageInner(), topLevelWindows: await windowInventory(p1.pid) };
  out.launch1.matchesDefault = clientIs(s1.window, DEFAULT_CLIENT);
  out.steps.push({ step: "launch1-default", real: s1.window, expected: DEFAULT_CLIENT, ok: out.launch1.matchesDefault });

  // ---- 2. real resize -> graceful exit (quit.flag -> RunEvent::Exit) ----
  out.resizeA = (await probeResize(p1.pid, CLIENT_A)).parsed;
  const s2 = await settle(p1.pid, 12000);
  out.afterResizeA = { real: s2.window, pageInner: await pageInner() };
  out.resizeApplied = clientIs(s2.window, CLIENT_A);
  out.launch1.quit = await quitViaFlag(p1.pid);
  await sleep(1500);
  const st1 = readState();
  out.stateAfterQuitA = { exists: st1.exists, main: (st1.parsed && st1.parsed.main) || null, raw: st1.raw || null };
  out.stateSavedA = savedIs(st1.parsed && st1.parsed.main, CLIENT_A);

  // ---- 3. relaunch: client area restored to 1280x860 ----
  const p2 = launch();
  const s3 = await settle(p2.pid, 35000);
  if (!s3 || !s3.window) return fail("launch2: no real window; " + j(s3));
  out.launch2 = { pid: p2.pid, realWindow: s3.window, pageInner: await pageInner() };
  out.launch2.restoredA = clientIs(s3.window, CLIENT_A);
  out.launch2.notDefault = !(near(s3.window.cw, DEFAULT_CLIENT.w) && near(s3.window.ch, DEFAULT_CLIENT.h));
  out.steps.push({ step: "launch2-restore", real: s3.window, expected: CLIENT_A, ok: out.launch2.restoredA });

  // ---- 4. title-bar close path (CloseRequested -> hide to tray + save) ----
  out.resizeB = (await probeResize(p2.pid, CLIENT_B)).parsed;
  const s4 = await settle(p2.pid, 12000);
  out.afterResizeB = s4.window;
  const mtimeBeforeClose = readState().mtimeMs || 0;
  out.closeClick = (await probeClose(p2.pid)).parsed;
  await sleep(2000);
  const st2 = readState();
  out.stateAfterClose = { exists: st2.exists, main: (st2.parsed && st2.parsed.main) || null, mtimeChanged: (st2.mtimeMs || 0) > mtimeBeforeClose, raw: st2.raw || null };
  out.closePath = {
    processStillAlive: isAlive(p2.pid),
    windowHidden: out.closeClick ? out.closeClick.visibleAfter === false : null,
    savedB: savedIs(st2.parsed && st2.parsed.main, CLIENT_B),
  };
  out.launch2.quit = await quitViaFlag(p2.pid);
  await sleep(1200);

  // ---- 5. relaunch: restored to 1200x800 ----
  const p3 = launch();
  const s5 = await settle(p3.pid, 35000);
  if (!s5 || !s5.window) return fail("launch3: no real window; " + j(s5));
  out.launch3 = { pid: p3.pid, realWindow: s5.window, restoredB: clientIs(s5.window, CLIENT_B) };
  out.steps.push({ step: "launch3-restore", real: s5.window, expected: CLIENT_B, ok: out.launch3.restoredB });
  out.launch3.quit = await quitViaFlag(p3.pid);
  await sleep(1000);

  // ---- 6. autostart launch: main window stays hidden ----
  const p4 = launch(["--from-autostart"]);
  await sleep(6000);
  const inv4 = await windowInventory(p4.pid);
  const s6 = await settle(p4.pid, 8000);
  out.launch4 = { pid: p4.pid, args: ["--from-autostart"], windows: inv4, realWindow: s6.window || null };
  const wins = Array.isArray(inv4) ? inv4.filter((w) => w.w > 100 && w.h > 100) : [];
  out.launch4.hiddenAsExpected = wins.length > 0 && wins.every((w) => w.visible === false);
  out.launch4.quit = await quitViaFlag(p4.pid);

  out.pass = Boolean(
    out.launch1.matchesDefault &&
    out.resizeApplied &&
    out.stateSavedA &&
    out.launch2.restoredA &&
    out.launch2.notDefault &&
    out.closePath.processStillAlive &&
    out.closePath.windowHidden &&
    out.closePath.savedB &&
    out.launch3.restoredB &&
    out.launch4.hiddenAsExpected
  );
  out.verdict = out.pass ? "PASS" : "FAIL";
  log(j(out));
  process.exit(out.pass ? 0 : 1);
}

main().catch((e) => { log(j({ fatal: String((e && e.stack) || e) })); process.exit(1); });
