// The "delete your account" form: signs in to the sync service with the
// account's email and password, then deletes the account with the password
// again (POST /api/auth/delete-account). Same origin as the API, nothing kept:
// the session token lives only in this function and is ended if the deletion
// doesn't go through. The page's messages come from a JSON block in the page.
(() => {
  const form = document.getElementById("delete-form");
  const textEl = document.getElementById("delete-text");
  if (!form || !textEl) return;
  const t = JSON.parse(textEl.textContent);
  const status = document.getElementById("delete-status");
  const button = form.querySelector("button[type=submit]");
  const email = form.elements.namedItem("email");
  const password = form.elements.namedItem("password");
  const confirmBox = form.elements.namedItem("confirm");

  document.querySelectorAll(".js-only").forEach((el) => { el.hidden = false; });
  document.querySelectorAll(".no-js-note").forEach((el) => { el.hidden = true; });

  const say = (msg, kind) => {
    status.textContent = msg;
    status.className = "form-status" + (kind ? " " + kind : "");
  };
  const post = (path, body, token) =>
    fetch("/api" + path, {
      method: "POST",
      headers: Object.assign({ "Content-Type": "application/json" }, token ? { "X-Auth-Token": token } : {}),
      body: JSON.stringify(body),
      credentials: "omit",
      cache: "no-store",
      redirect: "error",
    });
  const errorCode = async (r) => {
    try { return (await r.json()).error || String(r.status); } catch { return String(r.status); }
  };

  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    if (!confirmBox.checked) { say(t.confirm, "error"); confirmBox.focus(); return; }
    if (!form.reportValidity()) return;
    button.disabled = true;
    const label = button.textContent;
    button.textContent = t.working;
    say("", "");
    let token = null;
    let deleted = false;
    try {
      const login = await post("/auth/login", { email: email.value.trim(), password: password.value });
      if (login.status === 401 || login.status === 400) { say(t.wrong, "error"); return; }
      if (login.status === 429) { say(t.rate, "error"); return; }
      if (!login.ok) { say(t.other.replace("{code}", await errorCode(login)), "error"); return; }
      token = (await login.json()).token;
      const del = await post("/auth/delete-account", { password: password.value }, token);
      if (del.ok) {
        deleted = true;
        form.reset();
        form.hidden = true;
        say(t.done, "ok");
        status.focus();
        return;
      }
      if (del.status === 403) say(t.wrong, "error");
      else if (del.status === 429) say(t.rate, "error");
      else say(t.other.replace("{code}", await errorCode(del)), "error");
    } catch {
      say(t.network, "error");
    } finally {
      password.value = "";
      button.disabled = false;
      button.textContent = label;
      // A session made here that didn't end in a deletion is closed again.
      if (token && !deleted) post("/auth/logout", {}, token).catch(() => {});
    }
  });
})();
