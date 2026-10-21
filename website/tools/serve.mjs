// Serves website/dist like the production server does for the site: the same
// security headers and CSP per file type, folder index pages, the 404 page,
// and the API path answered by a stand-in (or passed to API_URL, e.g. a
// staging server), so pages and their CSP can be checked before a deploy.
//   node website/tools/serve.mjs [port]        API_URL=http://127.0.0.1:18100 node ...
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";

const DIST = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "dist");
const PORT = Number(process.argv[2] || 8790);
const API = process.env.API_URL || "";
const TYPES = {
  ".html": "text/html; charset=utf-8", ".css": "text/css; charset=utf-8", ".js": "text/javascript; charset=utf-8",
  ".png": "image/png", ".webp": "image/webp", ".ico": "image/x-icon", ".svg": "image/svg+xml; charset=utf-8",
  ".xml": "application/xml; charset=utf-8", ".txt": "text/plain; charset=utf-8", ".webmanifest": "application/manifest+json; charset=utf-8",
};
const SITE_CSP = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";
const common = {
  "X-Content-Type-Options": "nosniff",
  "X-Frame-Options": "DENY",
  "Referrer-Policy": "no-referrer",
  "Content-Security-Policy": "default-src 'none'; frame-ancestors 'none'",
};

function send(res, code, file) {
  const ext = path.extname(file);
  const headers = { ...common, "Content-Type": TYPES[ext] || "application/octet-stream" };
  if (ext === ".html") Object.assign(headers, {
    "Content-Security-Policy": SITE_CSP,
    "Permissions-Policy": "camera=(), microphone=(), geolocation=(), payment=(), usb=()",
    "Cross-Origin-Opener-Policy": "same-origin",
  });
  res.writeHead(code, headers);
  fs.createReadStream(file).pipe(res);
}

http.createServer(async (req, res) => {
  const url = new URL(req.url, "http://localhost");
  let p = decodeURIComponent(url.pathname);
  if (p.startsWith("/api/") || p === "/api") {
    if (!API) {
      res.writeHead(404, { ...common, "Content-Type": "application/json" });
      res.end(JSON.stringify({ error: "no_route", message: `${req.method} ${p.slice(4)} not handled` }));
      return;
    }
    const body = await new Promise((r) => { const b = []; req.on("data", (c) => b.push(c)); req.on("end", () => r(Buffer.concat(b))); });
    const headers = { "content-type": req.headers["content-type"] || "application/json" };
    if (req.headers["x-auth-token"]) headers["x-auth-token"] = req.headers["x-auth-token"];
    const up = await fetch(API + p.slice(4) + url.search, { method: req.method, headers, body: ["GET", "HEAD"].includes(req.method) ? undefined : body, redirect: "manual" });
    res.writeHead(up.status, { ...common, "Content-Type": up.headers.get("content-type") || "application/json" });
    res.end(Buffer.from(await up.arrayBuffer()));
    return;
  }
  if (p.includes("..")) { res.writeHead(400); res.end(); return; }
  let f = path.join(DIST, p);
  if (fs.existsSync(f) && fs.statSync(f).isDirectory()) {
    if (!p.endsWith("/")) { res.writeHead(301, { Location: p + "/" }); res.end(); return; }
    f = path.join(f, "index.html");
  }
  if (!fs.existsSync(f)) { send(res, 404, path.join(DIST, "404.html")); return; }
  send(res, 200, f);
}).listen(PORT, "127.0.0.1", () => console.log(`serving ${DIST} at http://127.0.0.1:${PORT}/${API ? ` (API -> ${API})` : ""}`));
