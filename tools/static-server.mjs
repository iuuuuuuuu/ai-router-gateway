// Zero-dep static file server for the built console (client/dist).
// Used to drive the UI in a browser for verification: the admin plane's CORS
// allowlist already includes http://localhost:5173, which is the origin this
// server is normally reached at.
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { extname, join, normalize } from "node:path";

const ROOT = process.argv[2];
const PORT = Number(process.argv[3] || 5173);
const TYPES = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".ico": "image/x-icon",
};

createServer(async (req, res) => {
  try {
    const url = new URL(req.url, "http://localhost");
    let rel = normalize(decodeURIComponent(url.pathname)).replace(/^([/\\])+/, "");
    if (rel === "" || rel.endsWith("/")) rel += "index.html";
    let body;
    try {
      body = await readFile(join(ROOT, rel));
    } catch {
      // SPA fallback: unknown paths serve the shell
      body = await readFile(join(ROOT, "index.html"));
      rel = "index.html";
    }
    res.writeHead(200, { "content-type": TYPES[extname(rel)] || "application/octet-stream" });
    res.end(body);
  } catch (e) {
    res.writeHead(500, { "content-type": "text/plain" });
    res.end(String(e));
  }
}).listen(PORT, "127.0.0.1", () => console.log("static server on http://127.0.0.1:" + PORT + " root=" + ROOT));
