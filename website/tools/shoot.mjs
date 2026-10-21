// Opens pages of the site in headless Edge (or Chrome) over CDP, at desktop and
// phone sizes: a full-page picture of each, plus every console message,
// failed request and CSP violation, which fail the run.
//   node website/tools/shoot.mjs <base url> <out dir> <path> [path...]
//   BROWSER=<path to msedge/chrome>   WIDTHS=1280,390
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const [base, outDir, ...paths] = process.argv.slice(2);
const BROWSER = process.env.BROWSER || "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe";
const WIDTHS = (process.env.WIDTHS || "1280,390").split(",").map(Number);
const PORT = 9400 + Math.floor(Math.random() * 400);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
fs.mkdirSync(outDir, { recursive: true });

const profile = fs.mkdtempSync(path.join(os.tmpdir(), "site-shoot-"));
const browser = spawn(BROWSER, ["--headless=new", "--disable-gpu", "--hide-scrollbars", "--no-first-run", "--no-default-browser-check",
  `--remote-debugging-port=${PORT}`, `--user-data-dir=${profile}`, "about:blank"], { stdio: "ignore" });
let page;
for (let i = 0; i < 50 && !page; i++) {
  await sleep(200);
  try { page = (await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json()).find((t) => t.type === "page"); } catch {}
}
if (!page) throw new Error("no browser page");
const ws = new WebSocket(page.webSocketDebuggerUrl);
await new Promise((r) => (ws.onopen = r));
let id = 0;
const pending = new Map();
const events = [];
ws.onmessage = (m) => {
  const d = JSON.parse(m.data);
  if (d.id && pending.has(d.id)) { pending.get(d.id)(d); pending.delete(d.id); }
  else if (d.method) events.push(d);
};
const send = (method, params = {}) => new Promise((r) => { const i = ++id; pending.set(i, r); ws.send(JSON.stringify({ id: i, method, params })); });
await send("Page.enable");
await send("Runtime.enable");
await send("Log.enable");
await send("Network.enable");

let problems = 0;
for (const p of paths) {
  for (const w of WIDTHS) {
    const mobile = w < 600;
    await send("Emulation.setDeviceMetricsOverride", { width: w, height: mobile ? 844 : 900, deviceScaleFactor: mobile ? 2 : 1, mobile });
    events.length = 0;
    const done = new Promise((r) => { const t = setInterval(() => { if (events.some((e) => e.method === "Page.loadEventFired")) { clearInterval(t); r(); } }, 50); });
    await send("Page.navigate", { url: base + p });
    await Promise.race([done, sleep(15000)]);
    // Down the page and back, so lazily loaded pictures load before the shot.
    await send("Runtime.evaluate", { expression: "(async () => { for (let y = 0; y < document.body.scrollHeight; y += 500) { scrollTo(0, y); await new Promise((r) => setTimeout(r, 40)); } scrollTo(0, 0); await Promise.all([...document.images].map((i) => i.complete ? 0 : new Promise((r) => { i.onload = i.onerror = r; }))); })()", awaitPromise: true });
    await sleep(400);
    const metrics = await send("Page.getLayoutMetrics");
    const h = Math.min(16000, Math.ceil(metrics.result.cssContentSize.height));
    const shot = await send("Page.captureScreenshot", { format: "png", captureBeyondViewport: true, clip: { x: 0, y: 0, width: w, height: h, scale: 1 } });
    const name = `${p.replace(/[^a-z0-9]+/gi, "_").replace(/^_|_$/g, "") || "home"}-${w}.png`;
    fs.writeFileSync(path.join(outDir, name), Buffer.from(shot.result.data, "base64"));
    const msgs = events.filter((e) => (e.method === "Log.entryAdded" && !["info", "verbose"].includes(e.params.entry.level)) || e.method === "Runtime.consoleAPICalled" || e.method === "Runtime.exceptionThrown" || e.method === "Network.loadingFailed")
      .map((e) => e.method === "Log.entryAdded" ? `${e.params.entry.level}: ${e.params.entry.text}` : e.method === "Network.loadingFailed" ? `failed: ${e.params.errorText} ${e.params.blockedReason || ""}` : e.method === "Runtime.exceptionThrown" ? `exception: ${e.params.exceptionDetails.text}` : `console.${e.params.type}: ${e.params.args.map((a) => a.value).join(" ")}`);
    const overflow = await send("Runtime.evaluate", { expression: "document.documentElement.scrollWidth > innerWidth + 1", returnByValue: true });
    if (overflow.result.result.value) msgs.push("layout: wider than the screen (horizontal scroll)");
    problems += msgs.length;
    console.log(`${msgs.length ? "FAIL" : "ok  "} ${p} @${w}: ${name}, ${h}px${msgs.length ? "\n   " + msgs.join("\n   ") : ""}`);
  }
}
ws.close();
browser.kill();
setTimeout(() => { try { fs.rmSync(profile, { recursive: true, force: true }); } catch {} }, 1500);
process.exitCode = problems ? 1 : 0;
