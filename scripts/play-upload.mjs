#!/usr/bin/env node
// Sends an Android App Bundle to a Google Play testing track with the Google
// Play Developer API, signed in as a service account. Only Node's own modules:
// no third-party code sees the account's key.
//
//   node scripts/play-upload.mjs --package com.submarine.app --check
//   node scripts/play-upload.mjs --package com.submarine.app --aab app.aab \
//     --track internal|alpha|beta [--status completed|draft] \
//     [--name "0.5.0-beta.3"] [--notes notes.txt] [--not-for-review]
//
// The key is the service account's JSON, in PLAY_SERVICE_ACCOUNT_JSON or a
// file named by --key-file. Nothing secret is printed.
//
// --check signs in, lists the tracks and their releases, and changes nothing:
// it shows that the account and its rights work.
//
// Tracks: internal (internal testing), alpha (closed testing), beta (open
// testing). Production is left to Play Console on purpose. --status draft is
// what Play takes while the app has never been published; a draft is then
// rolled out in Play Console. --not-for-review commits with
// changesNotSentForReview, for when Play asks for the changes to be sent for
// review by hand.
import { createHash, createSign } from "node:crypto";
import { readFileSync } from "node:fs";

const API = "https://androidpublisher.googleapis.com";
const SCOPE = "https://www.googleapis.com/auth/androidpublisher";
const TRACKS = new Set(["internal", "alpha", "beta"]);

function args(argv) {
  const out = { status: "completed" };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const next = () => {
      const v = argv[++i];
      if (v === undefined) fail(`${a} needs a value`);
      return v;
    };
    switch (a) {
      case "--package": out.pkg = next(); break;
      case "--aab": out.aab = next(); break;
      case "--track": out.track = next(); break;
      case "--status": out.status = next(); break;
      case "--name": out.name = next(); break;
      case "--notes": out.notes = next(); break;
      case "--key-file": out.keyFile = next(); break;
      case "--check": out.check = true; break;
      case "--not-for-review": out.notForReview = true; break;
      default: fail(`unknown argument: ${a}`);
    }
  }
  if (!out.pkg || !/^[A-Za-z][\w]*(\.[A-Za-z][\w]*)+$/.test(out.pkg)) fail("--package <application id> is needed");
  if (!out.check) {
    if (!out.aab) fail("--aab <file> is needed (or --check)");
    if (!TRACKS.has(out.track)) fail("--track must be internal, alpha or beta (production is done in Play Console)");
    if (!["completed", "draft"].includes(out.status)) fail("--status must be completed or draft");
  }
  return out;
}

function fail(message) {
  console.error(`play-upload: ${message}`);
  process.exit(1);
}

function credentials(keyFile) {
  const raw = keyFile ? readFileSync(keyFile, "utf8") : process.env.PLAY_SERVICE_ACCOUNT_JSON;
  if (!raw || !raw.trim()) fail("no service account key: set PLAY_SERVICE_ACCOUNT_JSON or pass --key-file");
  let key;
  try {
    key = JSON.parse(raw);
  } catch {
    fail("the service account key isn't JSON");
  }
  if (key.type !== "service_account" || !key.client_email || !key.private_key) fail("the key isn't a service account's JSON key");
  return key;
}

const b64url = (data) => Buffer.from(data).toString("base64url");

async function accessToken(key) {
  const now = Math.floor(Date.now() / 1000);
  const tokenUri = key.token_uri || "https://oauth2.googleapis.com/token";
  const head = b64url(JSON.stringify({ alg: "RS256", typ: "JWT" }));
  const claims = b64url(JSON.stringify({ iss: key.client_email, scope: SCOPE, aud: tokenUri, iat: now, exp: now + 3600 }));
  const signer = createSign("RSA-SHA256");
  signer.update(`${head}.${claims}`);
  const jwt = `${head}.${claims}.${signer.sign(key.private_key, "base64url")}`;
  const res = await fetch(tokenUri, {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({ grant_type: "urn:ietf:params:oauth:grant-type:jwt-bearer", assertion: jwt }),
  });
  const body = await res.json().catch(() => ({}));
  if (!res.ok || !body.access_token) fail(`sign-in refused (${res.status}): ${body.error_description || body.error || "no token"}`);
  return body.access_token;
}

function client(token, pkg) {
  const base = `/androidpublisher/v3/applications/${encodeURIComponent(pkg)}`;
  return async function call(method, path, { json, bytes, upload = false, query = "" } = {}) {
    const url = `${API}${upload ? "/upload" : ""}${base}${path}${query}`;
    const headers = { authorization: `Bearer ${token}` };
    let body;
    if (json !== undefined) {
      headers["content-type"] = "application/json";
      body = JSON.stringify(json);
    } else if (bytes !== undefined) {
      headers["content-type"] = "application/octet-stream";
      body = bytes;
    }
    const res = await fetch(url, { method, headers, body });
    const text = await res.text();
    let data = {};
    try { data = text ? JSON.parse(text) : {}; } catch { data = { raw: text.slice(0, 300) }; }
    if (!res.ok) {
      const msg = data.error?.message || data.raw || res.statusText;
      throw new Error(`${method} ${path || "/"}: ${res.status} ${msg}`);
    }
    return data;
  };
}

async function main() {
  const opt = args(process.argv.slice(2));
  const key = credentials(opt.keyFile);
  const call = client(await accessToken(key), opt.pkg);
  console.log(`Signed in as the service account; app ${opt.pkg}.`);

  const edit = await call("POST", "/edits", { json: {} });
  const e = `/edits/${encodeURIComponent(edit.id)}`;
  let committed = false;
  try {
    if (opt.check) {
      const { tracks = [] } = await call("GET", `${e}/tracks`);
      for (const t of tracks) {
        const rel = (t.releases || []).map((r) => `${r.name || "(no name)"} [${r.status}] codes ${(r.versionCodes || []).join(",")}`);
        console.log(`  ${t.track}: ${rel.length ? rel.join("; ") : "no release"}`);
      }
      if (tracks.length === 0) console.log("  no tracks yet");
      return;
    }

    const bytes = readFileSync(opt.aab);
    const sha256 = createHash("sha256").update(bytes).digest("hex");
    console.log(`Uploading ${opt.aab} (${(bytes.length / 1048576).toFixed(1)} MB, sha256 ${sha256})…`);
    const bundle = await call("POST", `${e}/bundles`, { bytes, upload: true, query: "?uploadType=media" });
    if (bundle.sha256 && bundle.sha256.toLowerCase() !== sha256) throw new Error(`Play got a different file: sha256 ${bundle.sha256}`);
    console.log(`Uploaded: version code ${bundle.versionCode}.`);

    const release = { versionCodes: [String(bundle.versionCode)], status: opt.status };
    if (opt.name) release.name = opt.name.slice(0, 50);
    if (opt.notes) {
      const text = readFileSync(opt.notes, "utf8").trim();
      // Play takes at most 500 characters of notes per language.
      if (Array.from(text).length > 500) throw new Error("release notes over 500 characters");
      if (text) release.releaseNotes = [{ language: "en-US", text }];
    }
    await call("PUT", `${e}/tracks/${opt.track}`, { json: { track: opt.track, releases: [release] } });
    await call("POST", `${e}:commit`, { query: opt.notForReview ? "?changesNotSentForReview=true" : "" });
    committed = true;
    console.log(`Done: version code ${bundle.versionCode} on the ${opt.track} track, ${opt.status}.`);
  } finally {
    // An edit left open blocks the next one until it expires.
    if (!committed) await call("DELETE", e).catch(() => {});
  }
}

main().catch((err) => fail(err.message));
