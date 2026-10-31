import { Fragment, forwardRef, useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@tauri-apps/api/core";
import {
  AlertTriangle, ArrowBigUp, ArrowRight, ArrowUpCircle, Check, CheckCircle2, Cloud, CloudOff, Download,
  Eye, EyeOff, Fingerprint, Github, HardDrive, Loader2, MoreHorizontal, Plus, RefreshCw, ShieldCheck, Trash2, Upload, X,
} from "lucide-react";
import CloudPanel from "./CloudPanel";
import { CloudConfigView, hostOf } from "./SyncServerSetting";
import logoUrl from "../assets/logo.png";
import { IS_ANDROID, IS_IOS, IS_MOBILE } from "../util/platform";
import { PRIVACY_URL } from "../util/links";
import { useTextPrompt, useConfirm } from "../ui/confirm";
import { forgetProfileBookmarks } from "../util/sftpBookmarks";

// Vault file header (lib.rs VAULT_MAGIC / VAULT_VERSION) — checked up front on
// Android imports so a wrong file fails before the user picks a name.
const VAULT_MAGIC = "OMNV";
const VAULT_VERSION = 1;

// Base64 in 32 KB chunks: spreading a large Uint8Array into one
// String.fromCharCode call would overflow the call stack.
const bytesToBase64 = (bytes: Uint8Array) => {
  let bin = "";
  for (let i = 0; i < bytes.length; i += 0x8000) {
    bin += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(bin);
};

interface CloudStatus { signed_in: boolean; email: string | null; }
// One personal profile as reported by the cloud (GET /sync/profiles). Names +
// counts only — the server never sees the encrypted contents. `profile` is the
// partition key (a name for legacy profiles, a UUID for new ones); `name` is the
// human-readable label to show and to restore under.
interface CloudProfile { profile: string; name: string; records: number; live_records: number; last_updated: string; }
// Result of the GitHub release check (backend: about::check_for_updates).
// `has_update` is true only when `latest` is a strictly newer semver than the
// running build.
interface UpdateInfo { current: string; latest: string | null; has_update: boolean; release_url: string | null; }
// The running version and the project's GitHub page (backend: about::app_info).
// `updates` is false in the App Store's builds, which don't look on GitHub.
interface AppInfo { version: string; updates: boolean; github_repo_url: string; }
// The GitHub link's fallback, should app_info ever fail (about::GITHUB_REPO).
const REPO_URL = "https://github.com/SinaXhpm/Submarine";

// The picker's unit of display: one profile, wherever it lives. `local` = an
// on-disk vault exists here; `cloud` = the matching cloud partition (or null).
// Merging local + cloud into a single row is what lets the login screen be the
// one place to SEE and MANAGE every profile — open it, export it, remove it
// here, get it from the cloud, or delete it from the cloud.
interface Row { name: string; local: boolean; cloud: CloudProfile | null; }

// What the page is busy with. One action at a time.
type Pending = "unlock" | "quick" | "create" | "import" | "get" | "work";

// The profile opened last on this device is the one picked when the page
// opens. Only its name is kept, which this page shows anyway. Storage can be
// unavailable (blocked, private), so every access is guarded; the first
// profile is picked then.
const LAST_PROFILE_KEY = "submarine-last-profile";
const lastProfile = () => {
  try { return localStorage.getItem(LAST_PROFILE_KEY) ?? ""; } catch { return ""; }
};
const rememberProfile = (name: string) => {
  try { localStorage.setItem(LAST_PROFILE_KEY, name); } catch { /* the first profile is picked instead */ }
};
const forgetLastProfile = (name: string) => {
  try { if (localStorage.getItem(LAST_PROFILE_KEY) === name) localStorage.removeItem(LAST_PROFILE_KEY); } catch { /* nothing kept */ }
};

// Clean the [PREFIX] off backend errors for display.
const cleanErr = (e: unknown) => String(e).replace(/^\[[A-Z_]+\]\s*/, "");

// The backend's rules for a profile name (lib.rs validate_profile_name),
// checked while it's typed, plus the names already taken. Case doesn't count:
// the list matches names that way, and so do Windows file names.
const RESERVED_NAME = /^(CON|PRN|AUX|NUL|COM\d|LPT\d)$/i;
const nameProblem = (name: string, local: string[], cloud: string[]): string | null => {
  if (!name) return null;
  if (!/^[A-Za-z0-9_-]+$/.test(name)) return "Use letters, numbers, - and _ only.";
  if (name.length > 32) return "Use 32 characters at most.";
  if (RESERVED_NAME.test(name)) return `"${name}" is a reserved name on Windows.`;
  const key = name.toLowerCase();
  const here = local.find((p) => p.toLowerCase() === key);
  if (here) return `There's already a profile named "${here}".`;
  // A new profile is a different one, even under the same name: it would sit
  // in the list as if it were the cloud one.
  const there = cloud.find((p) => p.toLowerCase() === key);
  if (there) return `"${there}" is in your cloud. Get it from the list, or pick another name.`;
  return null;
};

// A short side-to-side nudge for a refused entry, skipped when the system asks
// for less motion. The Web Animations API leaves no transform behind.
const shake = (el: HTMLElement | null | undefined) => {
  if (!el || typeof el.animate !== "function") return;
  if (window.matchMedia?.("(prefers-reduced-motion: reduce)").matches) return;
  el.animate(
    [0, -7, 6, -4, 3, 0].map((x) => ({ transform: `translateX(${x}px)` })),
    { duration: 380, easing: "ease-out" },
  );
};

// Whether Caps Lock is on. A page only learns it from an event, so every key
// and click reads it.
const useCapsLock = () => {
  const [on, setOn] = useState(false);
  useEffect(() => {
    const read = (e: KeyboardEvent | MouseEvent) => {
      // Some IME and synthetic events come without getModifierState.
      if (typeof e.getModifierState === "function") setOn(e.getModifierState("CapsLock"));
    };
    window.addEventListener("keydown", read, true);
    window.addEventListener("keyup", read, true);
    window.addEventListener("mousedown", read, true);
    return () => {
      window.removeEventListener("keydown", read, true);
      window.removeEventListener("keyup", read, true);
      window.removeEventListener("mousedown", read, true);
    };
  }, []);
  return on;
};

// No height here: the unlock box is h-11, the form's boxes h-10.
const INPUT_BASE =
  "w-full rounded-xl border bg-black/30 text-[14px] text-zinc-50 placeholder:text-zinc-500 outline-none transition-[border-color,background-color,box-shadow] focus:bg-black/40 focus:ring-4 read-only:opacity-70";
// One border and ring colour at a time: two in the same class list would be
// settled by the stylesheet's order, not this one.
const inputTone = (invalid: boolean) =>
  invalid
    ? "border-rose-500/50 focus:border-rose-400/70 focus:ring-rose-500/[0.12]"
    : "border-white/[0.08] focus:border-primary/60 focus:ring-primary/[0.12]";
const PRIMARY_BTN =
  "flex items-center justify-center gap-1.5 rounded-xl bg-primary font-semibold text-black shadow-[0_8px_24px_-12px_rgb(var(--primary)/0.9)] transition hover:brightness-110 active:brightness-95 disabled:pointer-events-none disabled:opacity-40";
const GHOST_BTN =
  "flex h-10 items-center justify-center gap-1.5 rounded-xl border border-white/[0.07] bg-white/[0.02] text-[12.5px] font-medium text-zinc-300 transition-colors hover:border-white/[0.12] hover:bg-white/[0.06] hover:text-white disabled:pointer-events-none disabled:opacity-40";
const LINK_BTN =
  "flex h-8 items-center gap-1.5 rounded-lg px-2.5 text-zinc-400 transition-colors hover:bg-white/[0.05] hover:text-white disabled:pointer-events-none disabled:opacity-40";
const CARD = "rounded-2xl border border-white/[0.07] bg-white/[0.02] shadow-[0_16px_48px_-24px_rgba(0,0,0,0.9)]";

interface Props {
  onUnlocked: (profileName: string) => void;
}

const ProfileSelectPage = ({ onUnlocked }: Props) => {
  const [profiles, setProfiles] = useState<string[]>([]);
  // The first load only: a later reload keeps the list on screen.
  const [loading, setLoading] = useState(true);
  // How the last action on a profile went (export, import, remove, get, delete
  // from the cloud), at the top of the page.
  const [error, setError] = useState<string | null>(null);
  const [info, setInfo] = useState<string | null>(null);
  const [pending, setPending] = useState<Pending | null>(null);
  const busy = pending !== null;

  // The open row (by display name). A local row opens to its password box; a
  // cloud-only row to Get.
  const [selected, setSelected] = useState<string>("");
  const [password, setPassword] = useState("");
  // Why the last unlock failed, under the password box.
  const [unlockError, setUnlockError] = useState<string | null>(null);

  const [creating, setCreating] = useState(false);
  const [newName, setNewName] = useState("");
  const [newPassword, setNewPassword] = useState("");
  const [newConfirmPassword, setNewConfirmPassword] = useState("");
  // Set by a Create click with something missing: the rules not met yet turn
  // red instead of grey.
  const [triedCreate, setTriedCreate] = useState(false);
  // Why the backend refused the new profile.
  const [createError, setCreateError] = useState<string | null>(null);

  // Import is a two-step flow: pick file (backend validates header) → prompt
  // for the profile name to save it under. We keep the picked path here so
  // the second step can pass it back to Rust on commit.
  // Desktop stages a file path (native dialog); Android stages the file's
  // bytes (base64) read from the system file picker via <input type="file">.
  // `file` is the file's name, to show which one is being imported.
  const [importStaged, setImportStaged] = useState<{ sourcePath?: string; bytesB64?: string; name: string; file: string } | null>(null);
  const importFileRef = useRef<HTMLInputElement | null>(null);
  const [cloudOpen, setCloudOpen] = useState(false);

  // Cloud status surfaced directly on this page so the user doesn't have
  // to open the modal just to know if sync is connected or pending.
  const [cloudStatus, setCloudStatus] = useState<CloudStatus>({ signed_in: false, email: null });
  // The sync server's host, shown beside the account when it isn't the default.
  const [cloudHost, setCloudHost] = useState<string | null>(null);
  // Profiles that live in the signed-in cloud account. Merged with the local
  // list below so a fresh device SHOWS the user's profiles instead of making
  // them remember and type a name. Empty until signed in (or on network error).
  const [cloudProfiles, setCloudProfiles] = useState<CloudProfile[]>([]);
  // A click on refresh, while it runs.
  const [refreshing, setRefreshing] = useState(false);
  // The row whose actions menu is open, where to put it, and what to focus
  // when it closes.
  const [menu, setMenu] = useState<{ name: string; place: MenuPlace; from: HTMLElement | null } | null>(null);
  // A newer published release than the running build, if any. Set only when
  // one actually exists, so the notice under the footer appears solely when
  // there's something to announce.
  const [update, setUpdate] = useState<UpdateInfo | null>(null);
  // Footer: the version + GitHub link, and how the last "Check for updates"
  // click went.
  const [appInfo, setAppInfo] = useState<AppInfo | null>(null);
  const [check, setCheck] = useState<"idle" | "checking" | "latest" | "none" | "error">("idle");
  const [checkError, setCheckError] = useState("");

  const textPrompt = useTextPrompt();
  const confirm = useConfirm();
  const capsLock = useCapsLock();

  const passwordInputRef = useRef<HTMLInputElement | null>(null);
  // The password box and its button, nudged on a wrong password.
  const unlockFieldRef = useRef<HTMLDivElement | null>(null);
  const nameInputRef = useRef<HTMLInputElement | null>(null);
  const newPasswordRef = useRef<HTMLInputElement | null>(null);
  const confirmRef = useRef<HTMLInputElement | null>(null);
  const importNameRef = useRef<HTMLInputElement | null>(null);

  // The row to open when none is: the profile opened last here, else the first.
  const pickFrom = (list: string[], prev: string) => {
    if (prev && list.includes(prev)) return prev;
    const last = lastProfile();
    return list.includes(last) ? last : list[0] || "";
  };

  const reload = async () => {
    setError(null);
    try {
      const list = await invoke<string[]>("list_profiles");
      setProfiles(list);
      setSelected((prev) => pickFrom(list, prev));
    } catch (e) {
      setError(cleanErr(e));
    } finally {
      setLoading(false);
    }
  };
  useEffect(() => { reload(); /* eslint-disable-next-line */ }, []);

  // Cloud connection + the account's profile list. Best-effort: a network
  // failure must never block unlocking a local profile, so errors just leave
  // the cloud side empty. A click on refresh (`manual`) says what went wrong.
  const refreshCloud = useCallback(async (manual = false) => {
    if (manual) setRefreshing(true);
    try {
      const s = await invoke<CloudStatus>("cloud_status");
      setCloudStatus(s);
      if (s.signed_in) {
        invoke<CloudConfigView>("cloud_get_config")
          .then((v) => setCloudHost(v.is_custom ? hostOf(v.server) : null))
          .catch(() => setCloudHost(null));
        try {
          const cps = await invoke<CloudProfile[]>("cloud_list_sync_profiles");
          // Hide empty / retired partitions (all-tombstone or escrow-only) —
          // they'd just be noise in the picker.
          setCloudProfiles(cps.filter((c) => c.live_records > 0));
        } catch (e) {
          setCloudProfiles([]);
          if (manual) setError(`Couldn't load your cloud profiles: ${cleanErr(e)}`);
        }
      } else {
        setCloudProfiles([]);
      }
    } catch (e) {
      // The panel can still be opened to retry.
      if (manual) setError(`Couldn't reach the cloud: ${cleanErr(e)}`);
    } finally {
      if (manual) setRefreshing(false);
    }
  }, []);

  useEffect(() => { refreshCloud(); }, [refreshCloud]);

  // Check GitHub for a newer release once per app open (this page mounts on
  // launch and on every return to the picker). Best-effort and silent: the
  // fetch+semver-compare happens in Rust (about::check_for_updates), and any
  // failure — offline, rate-limited, no releases — simply shows nothing.
  useEffect(() => {
    invoke<UpdateInfo>("check_for_updates")
      .then((u) => { if (u.has_update && u.latest) setUpdate(u); })
      .catch(() => {});
  }, []);

  // A link that can't be opened (no browser / URL handler on this device)
  // says so under the footer row instead of doing nothing.
  const [linkError, setLinkError] = useState("");
  const openLink = (url: string) => {
    setLinkError("");
    invoke("open_external_url", { url }).catch((e) => setLinkError(`Couldn't open the link: ${cleanErr(e)}`));
  };

  const openReleaseNotes = () => {
    if (update?.release_url) openLink(update.release_url);
  };

  useEffect(() => {
    invoke<AppInfo>("app_info").then(setAppInfo).catch(() => {});
  }, []);

  const openRepo = () => openLink(appInfo?.github_repo_url ?? REPO_URL);

  // The footer's "Check for updates". Unlike the silent check above it reports
  // every outcome; a newer release lands in `update`, so the same notice
  // announces it.
  const checkForUpdates = async () => {
    if (check === "checking") return;
    setCheck("checking");
    try {
      const u = await invoke<UpdateInfo>("check_for_updates");
      if (u.has_update && u.latest) {
        setUpdate(u);
        setCheck("idle");
      } else {
        setCheck(u.latest ? "latest" : "none");
      }
    } catch (e) {
      setCheckError(cleanErr(e));
      setCheck("error");
    }
  };

  // When the open row changes, reset + focus the password so opening a
  // profile is always "click row, type, Enter". The new row's box also starts
  // with its password hidden.
  useEffect(() => {
    setPassword("");
    setUnlockError(null);
    setError(null);
    requestAnimationFrame(() => passwordInputRef.current?.focus());
  }, [selected]);

  // Quick unlock (#166, quick_unlock.rs): this profile opens with Windows
  // Hello or the fingerprint on this device, if the user turned that on. The
  // app starting on the profile used last asks for it right away, once;
  // closing that prompt leaves the password.
  const [quick, setQuick] = useState<{ name: string; method: string } | null>(null);
  const autoQuickTried = useRef(false);
  useEffect(() => {
    let live = true;
    const name = selected;
    setQuick(null);
    if (!name) return;
    (async () => {
      try {
        const support = await invoke<{ method: string | null; ready: boolean }>("quick_unlock_support");
        if (!support.method || !support.ready) return;
        const on = await invoke<boolean>("quick_unlock_status", { name });
        if (!live || !on) return;
        setQuick({ name, method: support.method });
        if (!autoQuickTried.current && name === lastProfile()) {
          autoQuickTried.current = true;
          void quickUnlock(name);
        }
      } catch { /* the password still works */ }
    })();
    return () => { live = false; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selected]);
  const quickLabel = quick?.method === "android-biometric" ? "Unlock with fingerprint" : "Unlock with Windows Hello";
  const quickUnlock = async (name: string) => {
    if (busy) return;
    autoQuickTried.current = true;
    setPending("quick"); setUnlockError(null);
    try {
      await invoke("quick_unlock", { name });
    } catch (e) {
      const raw = String(e);
      setPending(null);
      if (raw.includes("[QUICK_UNLOCK] CANCELLED")) {
        requestAnimationFrame(() => passwordInputRef.current?.focus());
        return;
      }
      if (raw.includes("[QUICK_UNLOCK] STALE")) {
        setQuick(null);
        setUnlockError("Quick unlock no longer fits this profile here. Unlock with the password, then turn it on again.");
      } else {
        setUnlockError(cleanErr(e));
      }
      requestAnimationFrame(() => passwordInputRef.current?.focus());
      return;
    }
    rememberProfile(name);
    onUnlocked(name);
  };

  // Merge local + cloud into one ordered list, matched case-insensitively by
  // display name (a local file and its cloud partition are the SAME profile
  // even though the partition key may be an opaque UUID). Local-first, then
  // alphabetical, so the profiles already on this device sit up top.
  const buildRows = (): Row[] => {
    const byKey = new Map<string, Row>();
    for (const p of profiles) byKey.set(p.toLowerCase(), { name: p, local: true, cloud: null });
    for (const c of cloudProfiles) {
      const k = c.name.toLowerCase();
      const existing = byKey.get(k);
      if (existing) existing.cloud = c;
      else byKey.set(k, { name: c.name, local: false, cloud: c });
    }
    return Array.from(byKey.values()).sort((a, b) =>
      a.local !== b.local ? (a.local ? -1 : 1) : a.name.localeCompare(b.name)
    );
  };
  const rows = buildRows();

  // First run (nothing anywhere) drops straight into create; a returning user
  // on a fresh device (cloud profiles, no local) still sees the list so they
  // can Get them. `creating` is the explicit "New profile" toggle.
  const firstRun = !loading && rows.length === 0;
  const showCreate = creating || firstRun;

  // Each view focuses its first box: the name in the form, the password in
  // the list.
  useEffect(() => {
    requestAnimationFrame(() => (showCreate ? nameInputRef : passwordInputRef).current?.focus());
  }, [showCreate]);

  // A click on the open row puts the cursor back in its password box.
  const pick = (row: Row) => {
    if (row.name === selected) passwordInputRef.current?.focus();
    else setSelected(row.name);
  };

  const openCreate = () => {
    setCreating(true);
    setPassword(""); setUnlockError(null); setError(null); setInfo(null);
  };

  // Leaving the form forgets what was typed in it, passwords included.
  const closeCreate = () => {
    setCreating(false);
    setNewName(""); setNewPassword(""); setNewConfirmPassword("");
    setTriedCreate(false); setCreateError(null); setError(null);
  };

  const unlockSelected = async () => {
    const name = selected;
    if (!name || !password || busy) return;
    setPending("unlock"); setUnlockError(null);
    try {
      await invoke("select_profile", { name });
      await invoke("setup_master_db", { password });
    } catch (e) {
      const raw = String(e);
      setUnlockError(raw.toLowerCase().includes("decrypt") ? "Wrong password." : cleanErr(e));
      setPending(null);
      shake(unlockFieldRef.current);
      // Selected, so the next try types over it.
      requestAnimationFrame(() => {
        const el = passwordInputRef.current;
        el?.focus();
        el?.select();
      });
      return;
    }
    // No reset after this: the page goes away, and the button stays busy
    // until it does.
    rememberProfile(name);
    onUnlocked(name);
  };

  // The create form's checks, live. Length counts characters the way the
  // backend does (code points, not UTF-16 units).
  const nameIssue = nameProblem(newName.trim(), profiles, cloudProfiles.map((c) => c.name));
  const nameHint = nameIssue ?? (triedCreate && !newName.trim() ? "Give the profile a name." : null);
  const longEnough = [...newPassword].length >= 8;
  const matches = newConfirmPassword.length > 0 && newPassword === newConfirmPassword;
  const createReady = !!newName.trim() && !nameIssue && longEnough && matches;

  // Enter in the name or the first password box moves on to the next box,
  // until everything's filled in; then it creates the profile.
  const advance = (e: React.KeyboardEvent<HTMLInputElement>, next: React.RefObject<HTMLInputElement>) => {
    if (e.key !== "Enter" || e.nativeEvent.isComposing || createReady) return;
    e.preventDefault();
    next.current?.focus();
  };

  const createNew = async () => {
    if (busy) return;
    const name = newName.trim();
    if (!createReady) {
      setTriedCreate(true);
      // To the first thing still missing.
      const el = !name || nameIssue ? nameInputRef.current : !longEnough ? newPasswordRef.current : confirmRef.current;
      el?.focus();
      shake(el === nameInputRef.current ? el : el?.parentElement);
      return;
    }
    setPending("create"); setCreateError(null);
    try {
      await invoke("create_profile", { name, password: newPassword });
    } catch (e) {
      setCreateError(cleanErr(e));
      setPending(null);
      return;
    }
    rememberProfile(name);
    onUnlocked(name);
  };

  const exportProfile = async (name: string) => {
    if (busy) return;
    setPending("work"); setError(null); setInfo(null);
    try {
      // Returns the chosen path on success (on Android, the saved file's
      // name: the system picker doesn't say where it is), null if the user
      // cancelled the save dialog. We only surface a toast in the success case.
      const saved = await invoke<string | null>("export_profile", { name });
      if (saved) setInfo(IS_ANDROID ? `Saved as ${saved}` : IS_IOS ? `Saved as ${saved} in Files › Submarine` : `Exported to ${saved}`);
    } catch (e) {
      setError(cleanErr(e));
    } finally {
      setPending(null);
    }
  };

  const focusImportName = () => requestAnimationFrame(() => {
    const el = importNameRef.current;
    el?.focus();
    el?.select();
  });

  const startImport = async () => {
    if (busy) return;
    setError(null); setInfo(null);
    // Android and iOS: no native dialog — the WebView's file input opens the
    // system picker (Downloads, Drive, Files, messengers…) and hands us the file.
    if (IS_MOBILE) { importFileRef.current?.click(); return; }
    setPending("work");
    try {
      const picked = await invoke<[string, string] | null>("import_profile_pick");
      if (picked) {
        const [sourcePath, suggested] = picked;
        setImportStaged({ sourcePath, name: suggested, file: sourcePath.split(/[\\/]/).pop() || sourcePath });
        focusImportName();
      }
    } catch (e) {
      setError(cleanErr(e));
    } finally {
      setPending(null);
    }
  };

  const onImportFilePicked = async (e: React.ChangeEvent<HTMLInputElement>) => {
    const file = e.target.files?.[0];
    e.target.value = ""; // let the same file be picked again later
    if (!file) return;
    setError(null); setInfo(null);
    try {
      const bytes = new Uint8Array(await file.arrayBuffer());
      // Same quick header check the backend does, for instant feedback.
      if (bytes.length < 5 || String.fromCharCode(...bytes.subarray(0, 4)) !== VAULT_MAGIC || bytes[4] !== VAULT_VERSION) {
        setError("That file isn't a Submarine profile.");
        return;
      }
      const suggested = file.name.replace(/\.[^.]+$/, "").replace(/[^A-Za-z0-9_-]/g, "_").slice(0, 32) || "imported";
      setImportStaged({ bytesB64: bytesToBase64(bytes), name: suggested, file: file.name });
      focusImportName();
    } catch (err) {
      setError(cleanErr(err));
    }
  };

  // An imported file is a profile of its own, so only names on this device
  // are taken; the cloud one of the same name may well be this very profile.
  const importIssue = importStaged ? nameProblem(importStaged.name.trim(), profiles, []) : null;

  const cancelImport = () => { setImportStaged(null); setError(null); };

  const commitImport = async () => {
    if (!importStaged || busy) return;
    const name = importStaged.name.trim();
    if (!name || importIssue) { importNameRef.current?.focus(); shake(importNameRef.current); return; }
    setPending("import"); setError(null);
    try {
      if (importStaged.bytesB64) {
        await invoke("import_profile_bytes", { name, data: importStaged.bytesB64 });
      } else {
        await invoke("import_profile_save", { sourcePath: importStaged.sourcePath, name });
      }
      setImportStaged(null);
      closeCreate();
      setInfo(`Imported as "${name}". Open it with its original password.`);
      await reload();
      setSelected(name);
    } catch (e) {
      setError(cleanErr(e));
    } finally {
      setPending(null);
    }
  };

  // Remove the on-disk copy. If the profile is also in the cloud, it stays
  // there and can be pulled back with its password; if not, it's gone for good
  // — the confirm wording says which.
  const removeLocal = async (name: string) => {
    if (busy) return;
    const inCloud = cloudProfiles.some((c) => c.name.toLowerCase() === name.toLowerCase());
    const ok = await confirm({
      title: "Remove from this device",
      message: inCloud
        ? `Remove "${name}" from this device?\n\nThe encrypted file here is deleted. It stays in your cloud — you can Get it again anytime with its password.`
        : `Remove "${name}" from this device?\n\nThe encrypted file here is deleted. This profile is not in your cloud, so it will be gone for good.`,
      destructive: true,
      okLabel: "Remove",
    });
    if (!ok) return;
    setPending("work"); setError(null); setInfo(null);
    try {
      await invoke("delete_profile", { name });
      forgetProfileBookmarks(name);
      forgetLastProfile(name);
      await reload();
      refreshCloud();
    } catch (e) {
      setError(cleanErr(e));
    } finally {
      setPending(null);
    }
  };

  // Owner-only hard delete of the cloud copy (the /sync store is per-account, so
  // you can only ever delete your own). Local copies are untouched.
  const deleteFromCloud = async (row: Row) => {
    if (!row.cloud || busy) return;
    const ok = await confirm({
      title: "Delete from cloud",
      message: `Delete "${row.name}" from your cloud?\n\nThis erases its synced copy and its restore key from the cloud. Copies already on your devices are NOT touched. This can't be undone.`,
      destructive: true,
      okLabel: "Delete from cloud",
    });
    if (!ok) return;
    setPending("work"); setError(null); setInfo(null);
    try {
      const n = await invoke<number>("cloud_delete_profile", { profile: row.cloud.profile });
      setInfo(`Removed "${row.name}" from the cloud (${n} record${n === 1 ? "" : "s"}).`);
      // A cloud-only row is gone with it: open a local one instead.
      if (!row.local) setSelected((prev) => (prev === row.name ? pickFrom(profiles, "") : prev));
      await refreshCloud();
    } catch (e) {
      setError(cleanErr(e));
    } finally {
      setPending(null);
    }
  };

  // Bring a cloud profile onto this device using nothing but its own password:
  // that password unseals the cloud copy's key AND protects the copy saved
  // here. One field, no separate recovery secret. Taken exactly as typed, as
  // the unlock box takes it: a space at either end is part of the password.
  const bringDown = async (c: CloudProfile) => {
    if (busy) return;
    setError(null); setInfo(null);
    const pw = await textPrompt({
      title: `Get "${c.name}"`,
      message: "Enter THIS profile's password — the same one you open it with. It unlocks the cloud copy and protects the copy saved on this device.",
      placeholder: "Profile password",
      password: true,
      keepWhitespace: true,
      okLabel: "Get it",
      validate: (v: string) => (v.length < 1 ? "Enter the password" : null),
    });
    if (pw === null) return;
    setPending("get");
    try {
      const rep = await invoke<{ pushed: number; pulled: number }>("restore_personal_profile", {
        cloudProfile: c.profile, localName: c.name, vaultPassword: pw,
      });
      setInfo(`"${c.name}" is now on this device — pulled ${rep.pulled} item${rep.pulled === 1 ? "" : "s"}. Open it below with the same password.`);
      await reload();
      setSelected(c.name);
      refreshCloud();
    } catch (e) {
      setError(cleanErr(e));
    } finally {
      setPending(null);
    }
  };

  // A row's actions, in its ⋯ menu. The ones that ask first end in "…".
  const menuItems = (row: Row): MenuItem[] => {
    const items: MenuItem[] = [];
    if (row.local) {
      items.push({ label: "Export…", icon: <Download size={14} />, onSelect: () => exportProfile(row.name) });
      items.push({ label: "Remove from this device…", icon: <Trash2 size={14} />, danger: true, onSelect: () => removeLocal(row.name) });
    } else if (row.cloud) {
      const c = row.cloud;
      items.push({ label: "Get on this device…", icon: <Download size={14} />, onSelect: () => bringDown(c) });
    }
    if (row.cloud) items.push({ label: "Delete from cloud…", icon: <CloudOff size={14} />, danger: true, onSelect: () => deleteFromCloud(row) });
    return items;
  };
  const menuRow = menu ? rows.find((r) => r.name === menu.name) : undefined;

  const closeMenu = (refocus: boolean) => {
    const from = menu?.from;
    setMenu(null);
    if (refocus) from?.focus();
  };

  // What the footer's update item shows for each outcome of the manual check.
  const checkItem = {
    idle: { label: "Check for updates", icon: <RefreshCw size={12} />, tone: undefined },
    checking: { label: "Checking…", icon: <RefreshCw size={12} className="animate-spin" />, tone: undefined },
    latest: { label: "Up to date", icon: <CheckCircle2 size={12} />, tone: "text-emerald-400/90 hover:text-emerald-300" },
    none: { label: "No releases yet", icon: <RefreshCw size={12} />, tone: undefined },
    error: { label: "Couldn't check", icon: <AlertTriangle size={12} />, tone: "text-amber-400/90 hover:text-amber-300" },
  }[check];

  return (
    // Scrolls when the content is taller than the screen (phones with the
    // keyboard open); `m-auto` still centres it when there's room — plain
    // items-center would clip the top out of reach instead.
    <div className="relative flex-1 min-h-0 overflow-y-auto overflow-x-hidden flex flex-col px-4 sm:px-6 py-6 sm:py-10 bg-background">
      {/* A soft glow of the theme colour behind the brand. */}
      <div
        aria-hidden
        className="pointer-events-none absolute inset-x-0 top-0 h-[440px]"
        style={{ background: "radial-gradient(60% 70% at 50% 0%, rgb(var(--primary) / 0.11), transparent 72%)" }}
      />
      {/* On iOS a file input with no type also offers the camera and the
          photo library; asking for plain data opens the Files picker only. */}
      <input
        ref={importFileRef}
        type="file"
        accept={IS_IOS ? ".submarine,application/octet-stream" : undefined}
        className="hidden"
        onChange={onImportFilePicked}
      />
      <div className="relative w-full max-w-[380px] m-auto flex flex-col">
        <header className="mb-5 flex flex-col items-center text-center select-none">
          <img
            src={logoUrl}
            alt=""
            draggable={false}
            className="h-16 w-auto max-w-full object-contain drop-shadow-[0_6px_24px_rgb(var(--primary)/0.3)]"
          />
          <h1 className="mt-3 text-[22px] font-semibold leading-none tracking-tight text-white">Submarine</h1>
          <p className="mt-2 text-[10px] font-semibold uppercase tracking-[0.26em] text-primary/80">
            Run Silent, Run Deep
          </p>
        </header>

        {error && <Banner tone="error" onClose={() => setError(null)}>{error}</Banner>}
        {/* Wraps instead of truncating: an Android export path is long and
            the user needs all of it to find the file. */}
        {info && !error && <Banner tone="info" onClose={() => setInfo(null)}>{info}</Banner>}

        {importStaged && (
          <form
            noValidate
            onSubmit={(e) => { e.preventDefault(); commitImport(); }}
            onKeyDown={(e) => {
              if (e.key === "Escape" && pending !== "import") { e.preventDefault(); e.stopPropagation(); cancelImport(); }
            }}
            className="mb-3 rounded-2xl border border-primary/25 bg-primary/[0.05] p-4 animate-in"
          >
            <div className="flex items-center gap-2 text-[13.5px] font-semibold text-zinc-100">
              <Upload size={15} className="shrink-0 text-primary" /> Import a profile
            </div>
            <p className="mt-1 break-words text-[12px] leading-snug text-zinc-400">
              From {importStaged.file}. Pick a name for it; its password stays the same.
            </p>
            <input
              ref={importNameRef}
              value={importStaged.name}
              onChange={(e) => { const name = e.target.value; setImportStaged((s) => s && { ...s, name }); }}
              placeholder="Profile name"
              aria-label="Name for the imported profile"
              aria-invalid={importIssue ? true : undefined}
              autoComplete="off"
              autoCapitalize="none"
              autoCorrect="off"
              spellCheck={false}
              maxLength={32}
              className={`${INPUT_BASE} mt-3 h-10 px-3.5 ${inputTone(!!importIssue)}`}
            />
            {importIssue && <Hint tone="error">{importIssue}</Hint>}
            <div className="mt-3 flex gap-2">
              <button
                type="submit"
                disabled={busy || !importStaged.name.trim() || !!importIssue}
                className={`${PRIMARY_BTN} h-10 flex-1 text-[13px]`}
              >
                {pending === "import" ? <><Loader2 size={14} className="animate-spin" /> Importing…</> : "Import"}
              </button>
              <button type="button" onClick={cancelImport} disabled={pending === "import"} className={`${GHOST_BTN} px-4`}>
                Cancel
              </button>
            </div>
          </form>
        )}

        {loading ? (
          <div aria-busy="true" aria-label="Loading profiles" className={CARD}>
            <div className="flex items-center gap-3 px-3 py-3 animate-pulse">
              <span className="h-9 w-9 shrink-0 rounded-xl bg-white/[0.06]" />
              <span className="flex-1 space-y-2">
                <span className="block h-3 w-28 rounded bg-white/[0.07]" />
                <span className="block h-2.5 w-20 rounded bg-white/[0.04]" />
              </span>
            </div>
          </div>
        ) : showCreate ? (
          /* ---- Create a profile ---- */
          <form
            noValidate
            onSubmit={(e) => { e.preventDefault(); createNew(); }}
            onKeyDown={(e) => {
              if (e.key === "Escape" && !firstRun && !busy) { e.preventDefault(); e.stopPropagation(); closeCreate(); }
            }}
            className={`${CARD} p-4 animate-in`}
          >
            <h2 className="text-[16px] font-semibold tracking-tight text-zinc-50">
              {firstRun ? "Create your first profile" : "New profile"}
            </h2>
            <p className="mt-1 text-[12px] leading-snug text-zinc-400">
              Encrypted with a password that can't be recovered.
            </p>
            <div className="mt-4 space-y-2.5">
              <div>
                <input
                  ref={nameInputRef}
                  value={newName}
                  onChange={(e) => { setNewName(e.target.value); setCreateError(null); }}
                  onKeyDown={(e) => advance(e, newPasswordRef)}
                  placeholder="Profile name"
                  aria-label="Profile name"
                  aria-invalid={nameHint ? true : undefined}
                  aria-describedby={nameHint ? "new-name-hint" : undefined}
                  autoComplete="off"
                  autoCapitalize="none"
                  autoCorrect="off"
                  spellCheck={false}
                  enterKeyHint="next"
                  maxLength={32}
                  className={`${INPUT_BASE} h-10 px-3.5 ${inputTone(!!nameHint)}`}
                />
                {nameHint && <Hint id="new-name-hint" tone="error">{nameHint}</Hint>}
              </div>
              <PasswordInput
                ref={newPasswordRef}
                value={newPassword}
                onChange={(v) => { setNewPassword(v); setCreateError(null); }}
                onKeyDown={(e) => advance(e, confirmRef)}
                placeholder="Password"
                autoComplete="new-password"
                enterKeyHint="next"
                compact
                invalid={triedCreate && !longEnough}
              />
              <PasswordInput
                ref={confirmRef}
                value={newConfirmPassword}
                onChange={(v) => { setNewConfirmPassword(v); setCreateError(null); }}
                placeholder="Confirm password"
                autoComplete="new-password"
                enterKeyHint="go"
                compact
                invalid={triedCreate && longEnough && !matches}
              />
            </div>
            <ul className="mt-3 flex flex-wrap gap-x-4 gap-y-1.5 text-[12px]">
              <Rule ok={longEnough} strict={triedCreate}>At least 8 characters</Rule>
              <Rule ok={matches} strict={triedCreate}>Both passwords match</Rule>
            </ul>
            <div aria-live="polite">
              {createError ? <Hint tone="error">{createError}</Hint> : capsLock ? <Hint tone="warn">Caps Lock is on</Hint> : null}
            </div>
            <button type="submit" disabled={busy} className={`${PRIMARY_BTN} mt-4 h-11 w-full text-[14px]`}>
              {pending === "create" ? <><Loader2 size={15} className="animate-spin" /> Creating…</> : "Create profile"}
            </button>
            <div className="mt-2 flex items-center justify-center gap-1 text-[12.5px]">
              {!firstRun && (
                <>
                  <button type="button" onClick={closeCreate} disabled={busy} className={LINK_BTN}>Cancel</button>
                  <span aria-hidden className="text-zinc-700">·</span>
                </>
              )}
              <button
                type="button"
                onClick={startImport}
                disabled={busy}
                title="Import an exported .submarine file"
                className={LINK_BTN}
              >
                <Upload size={13} /> Import a file
              </button>
            </div>
          </form>
        ) : (
          /* ---- Your profiles (local + cloud, one list) ---- */
          <section aria-label="Your profiles">
            <div className="mb-2 flex h-7 items-center justify-between pl-1">
              <h2 className="text-[11px] font-semibold uppercase tracking-[0.16em] text-zinc-500">Your profiles</h2>
              {cloudStatus.signed_in && (
                <button
                  type="button"
                  onClick={() => refreshCloud(true)}
                  disabled={busy || refreshing}
                  title="Refresh from cloud"
                  aria-label="Refresh from cloud"
                  className="grid h-7 w-7 place-items-center rounded-lg text-zinc-500 transition-colors hover:bg-white/[0.05] hover:text-primary disabled:opacity-40"
                >
                  <RefreshCw size={13} className={refreshing ? "animate-spin" : ""} />
                </button>
              )}
            </div>

            <ul className="space-y-2">
              {rows.map((row) => {
                const open = selected === row.name;
                return (
                  <li
                    key={row.name}
                    className={`rounded-2xl border transition-colors ${
                      open
                        ? "border-white/[0.1] bg-white/[0.04] shadow-[0_16px_48px_-24px_rgba(0,0,0,0.9)]"
                        : "border-white/[0.06] bg-white/[0.015] hover:border-white/[0.1] hover:bg-white/[0.03]"
                    }`}
                  >
                    <div
                      className="flex items-center"
                      // Right-click: the same actions as ⋯, at the pointer.
                      onContextMenu={(e) => {
                        e.preventDefault();
                        if (busy) return;
                        setMenu({
                          name: row.name,
                          place: { x: e.clientX, top: e.clientY, bottom: e.clientY, end: false },
                          from: e.currentTarget.querySelector("button"),
                        });
                      }}
                    >
                      <button
                        type="button"
                        onClick={() => pick(row)}
                        disabled={pending === "unlock"}
                        aria-expanded={open}
                        className="flex min-w-0 flex-1 items-center gap-3 rounded-2xl py-3 pl-3 pr-1 text-left outline-none focus-visible:ring-2 focus-visible:ring-primary/50"
                      >
                        <Avatar name={row.name} local={row.local} />
                        <span className="min-w-0 flex-1">
                          <span className="block truncate text-[14px] font-medium leading-5 text-zinc-100">{row.name}</span>
                          <RowStatus row={row} />
                        </span>
                      </button>
                      <button
                        type="button"
                        onClick={(e) => {
                          const r = e.currentTarget.getBoundingClientRect();
                          setMenu({ name: row.name, place: { x: r.right, top: r.top, bottom: r.bottom, end: true }, from: e.currentTarget });
                        }}
                        disabled={busy}
                        aria-haspopup="menu"
                        aria-expanded={menu?.name === row.name}
                        aria-label={`Actions for ${row.name}`}
                        title="More actions"
                        className="mr-2 grid h-9 w-9 shrink-0 place-items-center rounded-lg text-zinc-500 outline-none transition-colors hover:bg-white/[0.07] hover:text-zinc-100 focus-visible:ring-2 focus-visible:ring-primary/50 disabled:opacity-40"
                      >
                        <MoreHorizontal size={17} />
                      </button>
                    </div>

                    {open && (row.local ? (
                      <form
                        onSubmit={(e) => { e.preventDefault(); unlockSelected(); }}
                        aria-busy={pending === "unlock"}
                        className="px-3 pb-3"
                      >
                        <div ref={unlockFieldRef} className="flex gap-2">
                          <PasswordInput
                            ref={passwordInputRef}
                            value={password}
                            onChange={(v) => { setPassword(v); setUnlockError(null); }}
                            onKeyDown={(e) => {
                              // Escape clears a typed password.
                              if (e.key === "Escape" && password) {
                                e.preventDefault(); e.stopPropagation();
                                setPassword(""); setUnlockError(null);
                              }
                            }}
                            placeholder="Password"
                            autoComplete="current-password"
                            enterKeyHint="go"
                            invalid={!!unlockError}
                            readOnly={pending === "unlock"}
                            describedBy="unlock-hint"
                          />
                          <button
                            type="submit"
                            disabled={busy || !password}
                            className={`${PRIMARY_BTN} h-11 shrink-0 px-4 text-[13.5px]`}
                          >
                            Unlock
                            {pending === "unlock" ? <Loader2 size={15} className="animate-spin" /> : <ArrowRight size={15} />}
                          </button>
                        </div>
                        <div id="unlock-hint" aria-live="polite">
                          {unlockError ? <Hint tone="error">{unlockError}</Hint> : capsLock ? <Hint tone="warn">Caps Lock is on</Hint> : null}
                        </div>
                        {quick?.name === row.name && (
                          <button
                            type="button"
                            onClick={() => void quickUnlock(row.name)}
                            disabled={busy}
                            className="mt-2 flex h-10 w-full items-center justify-center gap-2 rounded-lg border border-white/10 text-[13px] text-zinc-200 outline-none transition-colors hover:border-primary/40 hover:text-white focus-visible:ring-2 focus-visible:ring-primary/50 disabled:opacity-50"
                          >
                            {pending === "quick" ? <Loader2 size={15} className="animate-spin" /> : <Fingerprint size={15} className="text-primary" />}
                            {quickLabel}
                          </button>
                        )}
                      </form>
                    ) : (
                      <div className="space-y-2.5 px-3 pb-3">
                        <p className="text-[12px] leading-snug text-zinc-400">
                          Not on this device yet. Get it with this profile's password.
                        </p>
                        <button
                          type="button"
                          onClick={() => row.cloud && bringDown(row.cloud)}
                          disabled={busy}
                          className="flex h-10 w-full items-center justify-center gap-1.5 rounded-xl border border-primary/30 bg-primary/10 text-[13px] font-semibold text-primary transition-colors hover:bg-primary hover:text-black disabled:pointer-events-none disabled:opacity-40"
                        >
                          {pending === "get" ? <Loader2 size={14} className="animate-spin" /> : <Download size={14} />} Get on this device
                        </button>
                      </div>
                    ))}
                  </li>
                );
              })}
            </ul>

            <div className="mt-3 grid grid-cols-2 gap-2">
              <button type="button" onClick={openCreate} disabled={busy} className={GHOST_BTN}>
                <Plus size={14} /> New profile
              </button>
              <button
                type="button"
                onClick={startImport}
                disabled={busy}
                title="Import an exported .submarine file"
                className={GHOST_BTN}
              >
                <Upload size={14} /> Import
              </button>
            </div>
          </section>
        )}

        {/* Cloud status — always visible. Signed-out shows a Connect link;
            signed-in shows the account and a way into Manage. */}
        <CloudBar status={cloudStatus} host={cloudHost} busy={busy} firstRun={firstRun} onManage={() => setCloudOpen(true)} />

        {/* GitHub, the update check and the running version side by side,
            with a live "new version" notice underneath when one is available
            (the check steps aside then — the notice says it all). */}
        <footer className="mt-4 flex flex-col items-center gap-2.5">
          <div aria-live="polite" className="flex flex-wrap items-center justify-center gap-1 text-[11.5px]">
            <FooterButton onClick={openRepo} title="Open Submarine on GitHub" icon={<Github size={12} />} label="GitHub" />
            <FooterDot />
            <FooterButton onClick={() => openLink(PRIVACY_URL)} title="What Submarine stores and sends: the privacy policy" icon={<ShieldCheck size={12} />} label="Privacy" />
            {!update && appInfo?.updates !== false && (
              <>
                <FooterDot />
                <FooterButton
                  onClick={checkForUpdates}
                  disabled={check === "checking"}
                  title={check === "error" ? `Couldn't reach GitHub: ${checkError}` : "Look for a newer release on GitHub"}
                  icon={checkItem.icon}
                  label={checkItem.label}
                  tone={checkItem.tone}
                />
              </>
            )}
            {appInfo && (
              <>
                <FooterDot />
                <span className="px-1.5 font-mono text-[11px] text-zinc-600 select-text">v{appInfo.version}</span>
              </>
            )}
          </div>
          {/* Why a check or a link failed — as text, since a phone can't
              hover the tooltip. */}
          {(linkError || (check === "error" && checkError)) && (
            <p role="status" className="max-w-[320px] text-center text-[10.5px] leading-snug text-amber-400/80 break-words">
              {linkError || `Couldn't reach GitHub: ${checkError}`}
            </p>
          )}
          {update?.has_update && update.latest && (
            <button
              type="button"
              onClick={openReleaseNotes}
              title="Open the release notes on GitHub"
              className="group flex items-center gap-1.5 text-[10.5px] text-primary/90 hover:text-primary animate-in fade-in slide-in-from-bottom-1 duration-700"
            >
              {/* pulsing dot — the "something new" signal */}
              <span className="relative flex h-1.5 w-1.5">
                <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-primary/70 opacity-75" />
                <span className="relative inline-flex h-1.5 w-1.5 rounded-full bg-primary" />
              </span>
              <ArrowUpCircle size={11} className="shrink-0" />
              <span>New version <span className="font-mono font-semibold">v{update.latest}</span> available</span>
              <span className="opacity-60 transition-transform group-hover:translate-x-0.5">→</span>
            </button>
          )}
        </footer>
      </div>

      {menu && menuRow && (
        <RowMenu
          label={`Actions for ${menuRow.name}`}
          items={menuItems(menuRow)}
          place={menu.place}
          onClose={closeMenu}
        />
      )}

      <CloudPanel
        isOpen={cloudOpen}
        onClose={() => {
          setCloudOpen(false);
          // The modal may have logged in/out or changed cloud state —
          // re-pull so the bar (and profile list) reflects reality.
          refreshCloud();
        }}
        onLocalProfilesChanged={reload}
      />
    </div>
  );
};

// Each profile gets a steady colour from its name, to tell them apart at a
// glance. Full class names, so Tailwind sees them.
const AVATAR_TONES = [
  "bg-sky-500/15 text-sky-300",
  "bg-violet-500/15 text-violet-300",
  "bg-emerald-500/15 text-emerald-300",
  "bg-amber-500/15 text-amber-300",
  "bg-rose-500/15 text-rose-300",
  "bg-cyan-500/15 text-cyan-300",
  "bg-fuchsia-500/15 text-fuchsia-300",
  "bg-lime-500/15 text-lime-300",
];
const avatarTone = (name: string) => {
  let h = 0;
  for (const ch of Array.from(name.toLowerCase())) h = (Math.imul(h, 31) + (ch.codePointAt(0) ?? 0)) >>> 0;
  return AVATAR_TONES[h % AVATAR_TONES.length];
};

// The profile's initial. A cloud-only profile gets a dashed outline instead:
// it isn't on this device yet.
const Avatar = ({ name, local }: { name: string; local: boolean }) => (
  <span
    aria-hidden
    className={`grid h-9 w-9 shrink-0 place-items-center rounded-xl text-[15px] font-semibold ${
      local ? `${avatarTone(name)} ring-1 ring-inset ring-white/[0.06]` : "border border-dashed border-zinc-600 text-zinc-400"
    }`}
  >
    {(Array.from(name)[0] ?? "?").toUpperCase()}
  </span>
);

// Where a profile lives, under its name.
const RowStatus = ({ row }: { row: Row }) => {
  if (!row.local) {
    const n = row.cloud?.live_records ?? 0;
    return (
      <span className="flex items-center gap-1 text-[11.5px] leading-4 text-primary/80">
        <Cloud size={11} className="shrink-0" />
        <span className="truncate">In your cloud · {n} item{n === 1 ? "" : "s"}</span>
      </span>
    );
  }
  return (
    <span className="flex items-center gap-1 text-[11.5px] leading-4 text-zinc-500">
      <HardDrive size={11} className="shrink-0" />
      <span className="truncate">On this device</span>
      {row.cloud && (
        <>
          <span aria-hidden className="text-zinc-700">·</span>
          <Cloud size={11} className="shrink-0 text-emerald-400/80" />
          <span className="text-emerald-400/80">Synced</span>
        </>
      )}
    </span>
  );
};

interface PasswordInputProps {
  value: string;
  onChange: (value: string) => void;
  placeholder: string;
  autoComplete: "current-password" | "new-password";
  enterKeyHint: "go" | "next";
  /** h-10 instead of h-11, for the form's stack of boxes. */
  compact?: boolean;
  invalid?: boolean;
  readOnly?: boolean;
  describedBy?: string;
  onKeyDown?: (e: React.KeyboardEvent<HTMLInputElement>) => void;
}

// A password box with a show / hide toggle, hidden at first. Shown or not, it
// stays out of spell check, autocorrect and the keyboard's word learning, so a
// visible password isn't kept as a word.
const PasswordInput = forwardRef<HTMLInputElement, PasswordInputProps>(
  ({ value, onChange, placeholder, autoComplete, enterKeyHint, compact, invalid = false, readOnly, describedBy, onKeyDown }, ref) => {
    const [shown, setShown] = useState(false);
    return (
      // min-w-0: an <input> won't shrink below its intrinsic ~20ch width by
      // default, which pushed the button beside it out of the card on phones.
      <div className="relative min-w-0 flex-1">
        <input
          ref={ref}
          type={shown ? "text" : "password"}
          value={value}
          onChange={(e) => onChange(e.target.value)}
          onKeyDown={onKeyDown}
          placeholder={placeholder}
          aria-label={placeholder}
          aria-invalid={invalid || undefined}
          aria-describedby={describedBy}
          autoComplete={autoComplete}
          autoCapitalize="none"
          autoCorrect="off"
          spellCheck={false}
          enterKeyHint={enterKeyHint}
          readOnly={readOnly}
          className={`${INPUT_BASE} ${compact ? "h-10" : "h-11"} pl-3.5 pr-11 ${inputTone(invalid)}`}
        />
        <button
          type="button"
          onClick={() => setShown((s) => !s)}
          // Keeps the focus, and the caret, in the box.
          onMouseDown={(e) => e.preventDefault()}
          aria-label="Show password"
          aria-pressed={shown}
          title={shown ? "Hide password" : "Show password"}
          className="absolute right-1.5 top-1/2 grid h-8 w-8 -translate-y-1/2 place-items-center rounded-lg text-zinc-500 transition-colors hover:bg-white/[0.06] hover:text-zinc-200"
        >
          {shown ? <EyeOff size={15} /> : <Eye size={15} />}
        </button>
      </div>
    );
  },
);
PasswordInput.displayName = "PasswordInput";

// A line under a box: a problem, or a warning such as Caps Lock.
const Hint = ({ id, tone, children }: { id?: string; tone: "error" | "warn"; children: React.ReactNode }) => (
  <p
    id={id}
    className={`mt-2 flex items-start gap-1.5 text-[12px] leading-snug ${tone === "error" ? "text-rose-300" : "text-amber-300/90"}`}
  >
    {tone === "error"
      ? <AlertTriangle size={13} className="mt-px shrink-0" />
      : <ArrowBigUp size={13} className="mt-px shrink-0" />}
    <span className="min-w-0 break-words">{children}</span>
  </p>
);

// A password rule, ticked once it's met. Grey until a Create click, then red
// if still missing.
const Rule = ({ ok, strict, children }: { ok: boolean; strict: boolean; children: React.ReactNode }) => (
  <li className={`flex items-center gap-2 transition-colors ${ok ? "text-emerald-400" : strict ? "text-rose-300" : "text-zinc-500"}`}>
    <span
      aria-hidden
      className={`grid h-4 w-4 shrink-0 place-items-center rounded-full border transition-colors ${
        ok ? "border-emerald-400/60 bg-emerald-400/15" : strict ? "border-rose-400/50" : "border-zinc-600"
      }`}
    >
      {ok && <Check size={10} strokeWidth={3} />}
    </span>
    <span className="sr-only">{ok ? "Done: " : "Needed: "}</span>
    {children}
  </li>
);

// The page's message about the last action, with a close button. Selectable,
// so an export path can be copied.
const Banner = ({ tone, children, onClose }: { tone: "error" | "info"; children: React.ReactNode; onClose: () => void }) => (
  <div
    role={tone === "error" ? "alert" : "status"}
    className={`mb-3 flex items-start gap-2.5 rounded-xl border px-3 py-2.5 text-[12.5px] leading-snug animate-in ${
      tone === "error" ? "border-rose-500/25 bg-rose-500/[0.08] text-rose-100" : "border-emerald-500/25 bg-emerald-500/[0.08] text-emerald-50"
    }`}
  >
    {tone === "error"
      ? <AlertTriangle size={14} className="mt-px shrink-0 text-rose-300" />
      : <CheckCircle2 size={14} className="mt-px shrink-0 text-emerald-300" />}
    <span className="min-w-0 flex-1 break-words select-text">{children}</span>
    <button
      type="button"
      onClick={onClose}
      aria-label="Dismiss"
      className="-mr-1 -mt-0.5 grid h-6 w-6 shrink-0 place-items-center rounded-md opacity-60 transition-opacity hover:bg-white/10 hover:opacity-100"
    >
      <X size={13} />
    </button>
  </div>
);

interface MenuItem { label: string; icon: React.ReactNode; onSelect: () => void; danger?: boolean; }
// Where a menu opens: under (or, short of room, over) the span top..bottom,
// with its right edge at `x` when `end`, else its left edge.
interface MenuPlace { x: number; top: number; bottom: number; end: boolean; }

// A row's actions in a small menu. Arrow keys move, Enter picks, Escape or
// Tab close it, and a click outside closes it without doing anything else.
const RowMenu = ({ label, items, place, onClose }: {
  label: string; items: MenuItem[]; place: MenuPlace; onClose: (refocus: boolean) => void;
}) => {
  const ref = useRef<HTMLDivElement | null>(null);
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null);
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;

  // Measured off screen first, then placed inside the window, all before the
  // first paint.
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const { width: w, height: h } = el.getBoundingClientRect();
    const left = Math.max(8, Math.min(place.end ? place.x - w : place.x, window.innerWidth - w - 8));
    const below = place.bottom + 4;
    const top = below + h <= window.innerHeight - 8 ? below : Math.max(8, place.top - h - 4);
    setPos({ left, top });
    el.querySelector<HTMLElement>("[role=menuitem]")?.focus({ preventScroll: true });
    if (typeof el.animate === "function" && !window.matchMedia?.("(prefers-reduced-motion: reduce)").matches) {
      el.animate([{ opacity: 0, transform: "translateY(-4px)" }, { opacity: 1, transform: "none" }], { duration: 120, easing: "ease-out" });
    }
  }, [place]);

  useEffect(() => {
    const close = () => onCloseRef.current(false);
    window.addEventListener("resize", close);
    window.addEventListener("blur", close);
    return () => {
      window.removeEventListener("resize", close);
      window.removeEventListener("blur", close);
    };
  }, []);

  const onKeyDown = (e: React.KeyboardEvent) => {
    const all = Array.from(ref.current?.querySelectorAll<HTMLElement>("[role=menuitem]") ?? []);
    const i = all.indexOf(document.activeElement as HTMLElement);
    const go = (n: number) => { e.preventDefault(); all[(n + all.length) % all.length]?.focus(); };
    if (e.key === "ArrowDown") go(i + 1);
    else if (e.key === "ArrowUp") go(i < 0 ? -1 : i - 1);
    else if (e.key === "Home") go(0);
    else if (e.key === "End") go(-1);
    else if (e.key === "Escape" || e.key === "Tab") { e.preventDefault(); e.stopPropagation(); onClose(true); }
  };

  return createPortal(
    <>
      <div
        className="fixed inset-0 z-[9998]"
        onClick={() => onClose(false)}
        onContextMenu={(e) => { e.preventDefault(); onClose(false); }}
      />
      <div
        ref={ref}
        role="menu"
        aria-label={label}
        onKeyDown={onKeyDown}
        style={pos ?? { left: -9999, top: -9999 }}
        className="fixed z-[9999] min-w-[220px] rounded-xl border border-white/10 bg-[#141418] p-1 shadow-2xl shadow-black/60"
      >
        {items.map((it, i) => (
          <Fragment key={it.label}>
            {it.danger && i > 0 && !items[i - 1].danger && <div role="separator" className="mx-1 my-1 h-px bg-white/[0.07]" />}
            <button
              type="button"
              role="menuitem"
              tabIndex={-1}
              // Focus goes back to ⋯ first, so a confirm that opens next
              // returns it there.
              onClick={() => { onClose(true); it.onSelect(); }}
              className={`flex h-9 w-full items-center gap-2.5 rounded-lg px-2.5 text-left text-[13px] outline-none transition-colors ${
                it.danger
                  ? "text-rose-300 hover:bg-rose-500/10 focus:bg-rose-500/10"
                  : "text-zinc-200 hover:bg-white/[0.06] focus:bg-white/[0.06]"
              }`}
            >
              <span className="shrink-0 opacity-80">{it.icon}</span>
              {it.label}
            </button>
          </Fragment>
        ))}
      </div>
    </>,
    document.body,
  );
};

// A low-key text button for the footer row under the cloud bar.
const FooterButton = ({
  onClick, disabled, title, icon, label, tone,
}: {
  onClick: () => void; disabled?: boolean; title: string; icon: React.ReactNode; label: string; tone?: string;
}) => (
  <button
    type="button"
    onClick={onClick}
    disabled={disabled}
    title={title}
    className={`h-7 px-2 rounded-md flex items-center gap-1.5 transition-colors hover:bg-white/[0.05] disabled:cursor-default disabled:hover:bg-transparent ${
      tone || "text-zinc-500 hover:text-zinc-200"
    }`}
  >
    {icon} {label}
  </button>
);

const FooterDot = () => <span aria-hidden className="text-zinc-700 select-none">·</span>;

const CloudBar = ({
  status, host, busy, firstRun, onManage,
}: {
  status: CloudStatus;
  host: string | null;
  busy: boolean;
  firstRun: boolean;
  onManage: () => void;
}) => {
  // Signed-out: a light link rather than another full-width button — the
  // user hasn't asked for cloud yet, so it shouldn't compete with the
  // profiles. On a first run it's how a returning user finds theirs.
  if (!status.signed_in) {
    return (
      <div className="mt-4 flex flex-col items-center gap-0.5">
        {firstRun && <span className="text-[12px] text-zinc-500">Already have profiles in the cloud?</span>}
        <button
          type="button"
          onClick={onManage}
          className="flex h-9 items-center gap-2 rounded-full px-4 text-[12.5px] text-zinc-400 transition-colors hover:bg-primary/[0.07] hover:text-primary"
        >
          <Cloud size={14} /> Connect cloud sync
        </button>
      </div>
    );
  }
  return (
    <div className={`${CARD} mt-4 flex items-center gap-3 py-2 pl-2.5 pr-1.5`}>
      <span className="relative grid h-8 w-8 shrink-0 place-items-center rounded-xl bg-primary/10 text-primary">
        <Cloud size={15} />
        <span aria-hidden className="absolute -right-0.5 -top-0.5 h-2.5 w-2.5 rounded-full bg-emerald-400 ring-2 ring-background" />
      </span>
      <span className="min-w-0 flex-1">
        <span className="block truncate text-[12.5px] text-zinc-200">{status.email}</span>
        <span className="block truncate text-[11px] text-zinc-500">{host ? `Signed in · ${host}` : "Signed in"}</span>
      </span>
      <button
        type="button"
        onClick={onManage}
        disabled={busy}
        title="Manage cloud account"
        className="h-8 shrink-0 rounded-lg px-3 text-[12px] font-medium text-zinc-300 transition-colors hover:bg-white/[0.07] hover:text-white disabled:opacity-40"
      >
        Manage
      </button>
    </div>
  );
};

export default ProfileSelectPage;
