// Submarine Setup's page. The Rust side fills window.__SETUP__ before this
// runs (mode, language, texts, versions, folder) and sends events through
// window.__setupEvent; the page answers with window.ipc.postMessage(JSON).
// Texts only ever go in with textContent.
(() => {
  "use strict";

  const S = window.__SETUP__ || {};
  const STR = S.strings || {};
  const has = (o, k) => Object.prototype.hasOwnProperty.call(o, k);
  const $ = (id) => document.getElementById(id);

  const t = (key, vars) => {
    const s = has(STR, key) ? STR[key] : key;
    return vars ? s.replace(/\{(\w+)\}/g, (m, k) => (has(vars, k) ? String(vars[k]) : m)) : s;
  };
  const post = (msg) => {
    try { window.ipc.postMessage(JSON.stringify(msg)); } catch (_) { /* no host: a preview */ }
  };
  const number = (opts, v, fallback) => {
    try { return new Intl.NumberFormat(S.lang || "en", opts).format(v); } catch (_) { return fallback; }
  };
  const size = (bytes) => {
    const mb = Math.max(1, Math.round(bytes / (1024 * 1024)));
    return number({ style: "unit", unit: "megabyte", maximumFractionDigits: 0 }, mb, mb + " MB");
  };
  const percent = (v) => number({ style: "percent", maximumFractionDigits: 0 }, v, Math.round(v * 100) + "%");

  const uninstalling = S.mode === "uninstall";
  const product = S.product || { name: "Submarine", version: "" };
  const existing = S.existing || { kind: "none", version: "" };
  const opts = { dir: S.dir || "", desktop: !!S.desktop };

  document.documentElement.lang = S.lang || "en";
  document.title = t(uninstalling ? "window.uninstall" : "window.setup");
  for (const el of document.querySelectorAll("[data-t]")) el.textContent = t(el.dataset.t);
  for (const el of document.querySelectorAll("[data-label]")) {
    el.setAttribute("aria-label", t(el.dataset.label));
    el.title = t(el.dataset.label);
  }

  // ---- pages ----
  let current = "";
  let busy = false; // work is running: Escape and the close button wait

  function show(id, focus) {
    for (const p of document.querySelectorAll(".page")) p.classList.toggle("on", p.id === id);
    current = id;
    $("tb-close").disabled = busy;
    const heading = $(id).querySelector("h1, h2");
    if (heading) $("live").textContent = heading.textContent;
    if (focus) requestAnimationFrame(() => $(focus) && $(focus).focus({ preventScroll: true }));
  }

  function setProgress(v) {
    const bar = $("progress-bar");
    if (typeof v !== "number") {
      bar.classList.add("busy");
      bar.removeAttribute("aria-valuenow");
      $("progress-fill").style.width = "";
      $("progress-percent").textContent = "";
      return;
    }
    const pct = Math.max(0, Math.min(100, Math.round(v * 100)));
    bar.classList.remove("busy");
    bar.setAttribute("aria-valuenow", String(pct));
    $("progress-fill").style.width = pct + "%";
    $("progress-percent").textContent = percent(pct / 100);
  }

  function working(title) {
    busy = true;
    setProgress(null);
    $("progress-title").textContent = title;
    show("p-progress");
  }

  // ---- install ----
  const verb = { none: "install", older: "update", same: "reinstall", newer: "install" }[existing.kind] || "install";

  function renderOptions() {
    $("options-path-text").textContent = opts.dir;
    $("options-path").title = opts.dir;
    $("options-desktop").checked = opts.desktop;
    $("options-space").textContent = S.size ? t("options.space", { size: size(S.size) }) : "";
  }

  function startInstall(extra) {
    working(t(existing.kind === "older" ? "progress.update" : "progress.install"));
    post(Object.assign({ cmd: "install", dir: opts.dir, desktop: opts.desktop }, extra));
  }

  if (!uninstalling) {
    $("welcome-name").textContent = product.name;
    $("welcome-go").textContent = t(verb);
    $("options-go").textContent = t(verb);
    const status = $("welcome-status");
    const v = product.version;
    if (existing.kind === "older") status.textContent = t("status.older", { old: existing.version, version: v });
    else if (existing.kind === "same") status.textContent = t("status.same", { version: v });
    else if (existing.kind === "newer") {
      status.textContent = t("status.newer", { old: existing.version, version: v });
      status.classList.add("warn");
    } else status.textContent = t("status.new", { version: v });
    $("welcome-msi").hidden = !S.msi;
    renderOptions();

    $("welcome-go").onclick = () => startInstall();
    $("options-go").onclick = () => startInstall();
    $("welcome-options").onclick = () => show("p-options", "options-change");
    $("options-back").onclick = () => show("p-welcome", "welcome-go");
    $("options-change").onclick = () => post({ cmd: "browse", dir: opts.dir });
    $("options-desktop").onchange = (e) => { opts.desktop = e.target.checked; };
  }

  // ---- uninstall ----
  function startUninstall() {
    working(t("un.progress"));
    post({ cmd: "uninstall", deleteData: $("uninstall-data").checked });
  }
  if (uninstalling) {
    $("uninstall-go").onclick = startUninstall;
    $("uninstall-cancel").onclick = () => post({ cmd: "close" });
    // Tinted while ticked (a class, not :has(), for older WebView2 versions).
    const data = $("uninstall-data");
    data.onchange = () => data.parentElement.classList.toggle("on", data.checked);
  }

  // ---- the app is open ----
  $("running-close").onclick = () => {
    busy = true;
    show("p-progress");
    post({ cmd: "closeApp", ok: true });
  };
  $("running-cancel").onclick = () => post({ cmd: "closeApp", ok: false });

  // ---- finished / failed ----
  $("done-open").onclick = () => post({ cmd: "open" });
  $("done-close").onclick = () => post({ cmd: "close" });
  $("error-close").onclick = () => post({ cmd: "close" });
  $("error-retry").onclick = () => (uninstalling ? startUninstall() : startInstall());
  $("error-back").onclick = () => show("p-options", "options-change");
  $("error-anyway").onclick = () => startInstall({ skipMsi: true });

  const first = uninstalling ? "p-uninstall" : "p-welcome";

  // Events from the installer.
  window.__setupEvent = (e) => {
    switch (e && e.type) {
      case "dir":
        opts.dir = String(e.dir || "");
        renderOptions();
        break;
      case "progress":
        setProgress(e.value);
        break;
      case "running":
        busy = false;
        show("p-running", "running-close");
        break;
      case "cancelled":
        busy = false;
        show(first, uninstalling ? "uninstall-go" : "welcome-go");
        break;
      case "done":
        busy = false;
        setProgress(1);
        if (uninstalling) {
          $("done-title").textContent = t("un.done");
          $("done-body").textContent = "";
          $("done-open").hidden = true;
        } else {
          $("done-title").textContent = t(existing.kind === "older" ? "done.update" : "done.install");
          $("done-body").textContent = t("done.body", { version: product.version });
        }
        show("p-done", uninstalling ? "done-close" : "done-open");
        break;
      case "error":
        busy = false;
        $("error-title").textContent = t(uninstalling ? "un.error" : "error.title");
        $("error-message").textContent = typeof e.size === "number"
          ? String(e.message || "").replace("{size}", size(e.size))
          : String(e.message || "");
        $("error-log").textContent = e.log ? t("error.log", { path: e.log }) : "";
        $("error-retry").hidden = !e.retry;
        $("error-back").hidden = !e.options || uninstalling;
        $("error-anyway").hidden = !e.msi;
        show("p-error", e.retry ? "error-retry" : e.options ? "error-back" : "error-close");
        break;
    }
  };

  // ---- window ----
  $("drag").addEventListener("mousedown", (e) => { if (e.button === 0) post({ cmd: "drag" }); });
  $("tb-min").onclick = () => post({ cmd: "minimize" });
  $("tb-close").onclick = () => post({ cmd: "close" });

  document.addEventListener("keydown", (e) => {
    if (e.key !== "Escape") return;
    e.preventDefault();
    if (current === "p-options") show("p-welcome", "welcome-go");
    else if (current === "p-running") $("running-cancel").click();
    else if (!busy && current !== "p-progress") post({ cmd: "close" });
  });
  document.addEventListener("contextmenu", (e) => e.preventDefault());
  document.addEventListener("dragover", (e) => e.preventDefault());
  document.addEventListener("drop", (e) => e.preventDefault());

  if (S.passive) {
    // /P: no questions, just the progress.
    if (uninstalling) startUninstall();
    else startInstall();
  } else {
    show(first, uninstalling ? "uninstall-go" : "welcome-go");
  }
  post({ cmd: "ready" });
})();
