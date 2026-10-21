// The delete-account form in a real browser (headless Edge or Chrome) against a staging sync API: serve.mjs must run
// with API_URL pointing at it. A throwaway account is made and checked with the commands given in MK and COUNT
// (each prints a number), so this script never touches a database itself.
//   MK="<cmd making the account, prints its id>" COUNT="<cmd printing how many users have the email>"
//   node website/tools/delete-e2e.mjs <base url> <page path> <email> <password>
import { execSync, spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const [base, page, email, password] = process.argv.slice(2);
const BROWSER = process.env.BROWSER || "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe";
const PORT = 9800 + Math.floor(Math.random() * 150);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let fails = 0;
const check = (label, got, expected) => {
  const ok = JSON.stringify(got) === JSON.stringify(expected);
  if (!ok) fails++;
  console.log(`${ok ? "PASS" : "FAIL"} ${label}: ${JSON.stringify(got)}${ok ? "" : ` (expected ${JSON.stringify(expected)})`}`);
};
const users = () => Number(execSync(process.env.COUNT, { env: { ...process.env, MSYS_NO_PATHCONV: "1" } }).toString().trim().split("\n").pop());

const id = Number(execSync(process.env.MK, { env: { ...process.env, MSYS_NO_PATHCONV: "1" } }).toString().trim().split("\n").pop());
check("throwaway account made", id > 0 && users() === 1, true);

const profile = fs.mkdtempSync(path.join(os.tmpdir(), "site-del-"));
const browser = spawn(BROWSER, ["--headless=new", "--disable-gpu", "--no-first-run", `--remote-debugging-port=${PORT}`, `--user-data-dir=${profile}`, "about:blank"], { stdio: "ignore" });
let target;
for (let i = 0; i < 50 && !target; i++) { await sleep(200); try { target = (await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json()).find((t) => t.type === "page"); } catch {} }
const ws = new WebSocket(target.webSocketDebuggerUrl);
await new Promise((r) => (ws.onopen = r));
let n = 0;
const pending = new Map();
const logs = [];
ws.onmessage = (m) => { const d = JSON.parse(m.data); if (d.id && pending.has(d.id)) { pending.get(d.id)(d); pending.delete(d.id); } else if (d.method === "Log.entryAdded" || d.method === "Runtime.exceptionThrown") logs.push(JSON.stringify(d.params).slice(0, 300)); };
const send = (method, params = {}) => new Promise((r) => { const i = ++n; pending.set(i, r); ws.send(JSON.stringify({ id: i, method, params })); });
const ev = async (expr) => (await send("Runtime.evaluate", { expression: expr, awaitPromise: true, returnByValue: true })).result?.result?.value;
await send("Runtime.enable"); await send("Log.enable"); await send("Page.enable");
await send("Page.navigate", { url: base + page });
for (let i = 0; i < 50; i++) { await sleep(200); if (await ev("document.readyState === 'complete' && !!document.getElementById('delete-form')")) break; }
check("the form shows (script ran), the no-script note hides", await ev("[!document.querySelector('.js-only').hidden, document.querySelector('.no-js-note').hidden]"), [true, true]);
const fill = (pw, tick) => ev(`(() => { const f = document.getElementById('delete-form'); f.email.value = ${JSON.stringify(email)}; f.password.value = ${JSON.stringify(pw)}; f.confirm.checked = ${tick}; f.requestSubmit(); return 1; })()`);
const status = async () => { for (let i = 0; i < 240; i++) { await sleep(250); const s = await ev("(() => { const s = document.getElementById('delete-status'); const b = document.querySelector('#delete-form button'); return s.textContent && !b.disabled ? [s.className, s.textContent] : null; })()"); if (s) return s; } return null; };
const t = JSON.parse(await ev("document.getElementById('delete-text').textContent"));

await fill(password, false);
check("without the box ticked: asks for it, nothing sent", await status(), ["form-status error", t.confirm]);
check("the account is still there", users(), 1);
await ev("document.getElementById('delete-status').textContent = ''");
await fill("not the password", true);
check("a wrong password: the wrong-password message", await status(), ["form-status error", t.wrong]);
check("the account is still there", users(), 1);
check("the password field is emptied after a try", await ev("document.getElementById('delete-form').password.value"), "");
await ev("document.getElementById('delete-status').textContent = ''");
await fill(password, true);
let done = null;
for (let i = 0; i < 240 && !done; i++) { await sleep(250); done = await ev("(() => { const s = document.getElementById('delete-status'); return s.className === 'form-status ok' ? s.textContent : null; })()"); }
if (!done) console.log("   status after the right password: " + JSON.stringify(await ev("[document.getElementById('delete-status').className, document.getElementById('delete-status').textContent, document.querySelector('#delete-form button').disabled]")));
check("the right password: deleted, said so", done, t.done);
check("the form is gone", await ev("document.getElementById('delete-form').hidden"), true);
check("the account is gone", users(), 0);
// (The wrong password's 401 is the browser's own note about that answer.)
check("no console errors", logs.filter((l) => !/status of 401/.test(l)), []);
ws.close();
browser.kill();
setTimeout(() => { try { fs.rmSync(profile, { recursive: true, force: true }); } catch {} }, 1500);
console.log(fails ? `${fails} FAILED` : "ALL PASS");
process.exitCode = fails ? 1 : 0;
