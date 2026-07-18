import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  Plus, Trash2, X, AlertTriangle, ArrowRight, Download, Upload,
  CheckCircle2, Cloud, CloudOff, HardDrive, ChevronDown, RefreshCw, ArrowUpCircle, Heart,
} from "lucide-react";
import CloudPanel from "./CloudPanel";
import AboutPanel from "./AboutPanel";
import logoUrl from "../assets/logo.png";
import { IS_ANDROID } from "../util/platform";
import { useTextPrompt, useConfirm } from "../ui/confirm";

interface CloudStatus { signed_in: boolean; email: string | null; }
// One personal profile as reported by the cloud (GET /sync/profiles). Names +
// counts only — the server never sees the encrypted contents. `profile` is the
// partition key (a name for legacy profiles, a UUID for new ones); `name` is the
// human-readable label to show and to restore under.
interface CloudProfile { profile: string; name: string; records: number; live_records: number; last_updated: string; }
// Result of the GitHub release check (backend: about::check_for_updates).
// `has_update` is true only when `latest` is a strictly newer semver than the
// running build. Mirrors the shape AboutPanel already consumes.
interface UpdateInfo { current: string; latest: string | null; has_update: boolean; release_url: string | null; }

// The picker's unit of display: one profile, wherever it lives. `local` = an
// on-disk vault exists here; `cloud` = the matching cloud partition (or null).
// Merging local + cloud into a single row is what lets the login screen be the
// one place to SEE and MANAGE every profile — open it, export it, remove it
// here, get it from the cloud, or delete it from the cloud.
interface Row { name: string; local: boolean; cloud: CloudProfile | null; }

interface Props {
  onUnlocked: (profileName: string) => void;
}

const ProfileSelectPage = ({ onUnlocked }: Props) => {
  const [profiles, setProfiles] = useState<string[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [info, setInfo] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // The expanded row (by display name). A local row expands to a password +
  // Open + actions; a cloud-only row expands to Get + Delete-from-cloud.
  const [selected, setSelected] = useState<string>("");
  const [password, setPassword] = useState("");

  const [creating, setCreating] = useState(false);
  const [newName, setNewName] = useState("");
  const [newPassword, setNewPassword] = useState("");
  const [newConfirmPassword, setNewConfirmPassword] = useState("");

  // Import is a two-step flow: pick file (backend validates header) → prompt
  // for the profile name to save it under. We keep the picked path here so
  // the second step can pass it back to Rust on commit.
  const [importStaged, setImportStaged] = useState<{ sourcePath: string; name: string } | null>(null);
  const [cloudOpen, setCloudOpen] = useState(false);
  const [aboutOpen, setAboutOpen] = useState(false);

  // Cloud status surfaced directly on this page so the user doesn't have
  // to open the modal just to know if sync is connected or pending.
  const [cloudStatus, setCloudStatus] = useState<CloudStatus>({ signed_in: false, email: null });
  // Profiles that live in the signed-in cloud account. Merged with the local
  // list below so a fresh device SHOWS the user's profiles instead of making
  // them remember and type a name. Empty until signed in (or on network error).
  const [cloudProfiles, setCloudProfiles] = useState<CloudProfile[]>([]);
  // A newer published release than the running build, if any. Set only when
  // one actually exists, so the notice by "About" appears solely when there's
  // something to announce.
  const [update, setUpdate] = useState<UpdateInfo | null>(null);

  const textPrompt = useTextPrompt();
  const confirm = useConfirm();

  const passwordInputRef = useRef<HTMLInputElement | null>(null);
  const nameInputRef = useRef<HTMLInputElement | null>(null);
  const importNameRef = useRef<HTMLInputElement | null>(null);

  // Clean the [PREFIX] off backend errors for display.
  const cleanErr = (e: unknown) => String(e).replace(/^\[[A-Z_]+\]\s*/, "");

  const reload = async () => {
    setLoading(true); setError(null);
    try {
      const list = await invoke<string[]>("list_profiles");
      setProfiles(list);
      setSelected((prev) => (prev && list.includes(prev) ? prev : list[0] || ""));
    } catch (e: any) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  };
  useEffect(() => { reload(); /* eslint-disable-next-line */ }, []);

  // Cloud connection + the account's profile list. Best-effort: a network
  // failure must never block unlocking a local profile, so errors are swallowed
  // and just leave the cloud side empty.
  const refreshCloud = useCallback(async () => {
    try {
      const s = await invoke<CloudStatus>("cloud_status");
      setCloudStatus(s);
      if (s.signed_in) {
        try {
          const cps = await invoke<CloudProfile[]>("cloud_list_sync_profiles");
          // Hide empty / retired partitions (all-tombstone or escrow-only) —
          // they'd just be noise in the picker.
          setCloudProfiles(cps.filter((c) => c.live_records > 0));
        } catch {
          setCloudProfiles([]);
        }
      } else {
        setCloudProfiles([]);
      }
    } catch {
      // swallow — network down, modal can still be opened to retry
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

  const openReleaseNotes = () => {
    if (update?.release_url) invoke("open_external_url", { url: update.release_url }).catch(() => {});
  };
  // Donate → the project's GitHub, anchored at #donate (placeholder section the
  // owner fills in later). Goes through the backend URL-opener like every other
  // external link so the CSP stays tight.
  const openDonate = () => {
    invoke("open_external_url", { url: "https://github.com/SinaXhpm/submarine#donate" }).catch(() => {});
  };

  // When the expanded row changes, reset + focus the password so opening a
  // profile is always "click row, type, Enter".
  useEffect(() => {
    setPassword("");
    setError(null);
    requestAnimationFrame(() => passwordInputRef.current?.focus());
  }, [selected]);

  useEffect(() => {
    if (creating) requestAnimationFrame(() => nameInputRef.current?.focus());
  }, [creating]);

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

  const toggle = (row: Row) => setSelected((prev) => (prev === row.name ? "" : row.name));

  const unlockSelected = async () => {
    if (!selected) { setError("Pick a profile first."); return; }
    if (!password) { setError("Type your password."); return; }
    setBusy(true); setError(null);
    try {
      await invoke("select_profile", { name: selected });
      await invoke("setup_master_db", { password });
      onUnlocked(selected);
    } catch (e: any) {
      const raw = String(e);
      setError(raw.toLowerCase().includes("decrypt") ? "Wrong password." : raw);
    } finally {
      setBusy(false);
    }
  };

  const createNew = async () => {
    setError(null);
    if (!newName.trim()) { setError("Pick a name."); return; }
    if (!newPassword) { setError("Set a password."); return; }
    if (newPassword !== newConfirmPassword) { setError("Passwords don't match."); return; }
    setBusy(true);
    try {
      await invoke("create_profile", { name: newName.trim(), password: newPassword });
      onUnlocked(newName.trim());
    } catch (e: any) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const exportProfile = async (name: string) => {
    setBusy(true); setError(null); setInfo(null);
    try {
      // Returns the chosen path on success, null if the user cancelled the
      // save dialog. We only surface a toast in the success case.
      const saved = await invoke<string | null>("export_profile", { name });
      if (saved) setInfo(`Exported to ${saved}`);
    } catch (e: any) {
      setError(cleanErr(e));
    } finally {
      setBusy(false);
    }
  };

  const startImport = async () => {
    setError(null); setInfo(null);
    setBusy(true);
    try {
      const picked = await invoke<[string, string] | null>("import_profile_pick");
      if (!picked) { setBusy(false); return; }
      const [sourcePath, suggested] = picked;
      setImportStaged({ sourcePath, name: suggested });
      requestAnimationFrame(() => importNameRef.current?.select());
    } catch (e: any) {
      setError(cleanErr(e));
    } finally {
      setBusy(false);
    }
  };

  const commitImport = async () => {
    if (!importStaged) return;
    const trimmed = importStaged.name.trim();
    if (!trimmed) { setError("Pick a name for the imported profile."); return; }
    setBusy(true); setError(null);
    try {
      await invoke("import_profile_save", { sourcePath: importStaged.sourcePath, name: trimmed });
      setImportStaged(null);
      setInfo(`Imported as "${trimmed}". Open it with the original password.`);
      await reload();
      setSelected(trimmed);
    } catch (e: any) {
      setError(cleanErr(e));
    } finally {
      setBusy(false);
    }
  };

  // Remove the on-disk copy. If the profile is also in the cloud, it stays
  // there and can be pulled back with its password; if not, it's gone for good
  // — the confirm wording says which.
  const removeLocal = async (name: string) => {
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
    setBusy(true); setError(null);
    try {
      await invoke("delete_profile", { name });
      await reload();
      refreshCloud();
    } catch (e: any) {
      setError(cleanErr(e));
    } finally {
      setBusy(false);
    }
  };

  // Owner-only hard delete of the cloud copy (the /sync store is per-account, so
  // you can only ever delete your own). Local copies are untouched.
  const deleteFromCloud = async (row: Row) => {
    if (!row.cloud) return;
    const ok = await confirm({
      title: "Delete from cloud",
      message: `Delete "${row.name}" from your cloud?\n\nThis erases its synced copy and its restore key from the cloud. Copies already on your devices are NOT touched. This can't be undone.`,
      destructive: true,
      okLabel: "Delete from cloud",
    });
    if (!ok) return;
    setBusy(true); setError(null); setInfo(null);
    try {
      const n = await invoke<number>("cloud_delete_profile", { profile: row.cloud.profile });
      setInfo(`Removed "${row.name}" from the cloud (${n} record${n === 1 ? "" : "s"}).`);
      if (!row.local) setSelected((prev) => (prev === row.name ? "" : prev));
      await refreshCloud();
    } catch (e: any) {
      setError(cleanErr(e));
    } finally {
      setBusy(false);
    }
  };

  // Bring a cloud profile onto this device using nothing but its own password:
  // that password unseals the cloud copy's key AND protects the copy saved
  // here. One field, no separate recovery secret.
  const bringDown = async (c: CloudProfile) => {
    setError(null); setInfo(null);
    const pw = await textPrompt({
      title: `Get "${c.name}"`,
      message: "Enter THIS profile's password — the same one you open it with. It unlocks the cloud copy and protects the copy saved on this device.",
      placeholder: "Profile password",
      password: true,
      okLabel: "Get it",
      validate: (v: string) => (v.length < 1 ? "Enter the password" : null),
    });
    if (pw === null) return;
    setBusy(true);
    try {
      const rep = await invoke<{ pushed: number; pulled: number }>("restore_personal_profile", {
        cloudProfile: c.profile, localName: c.name, vaultPassword: pw,
      });
      setInfo(`"${c.name}" is now on this device — pulled ${rep.pulled} item${rep.pulled === 1 ? "" : "s"}. Open it below with the same password.`);
      await reload();
      setSelected(c.name);
      refreshCloud();
    } catch (e: any) {
      setError(cleanErr(e));
    } finally {
      setBusy(false);
    }
  };

  const inputBase =
    "w-full h-11 px-4 bg-zinc-900/40 border border-white/5 rounded-xl text-[14px] text-zinc-50 placeholder:text-zinc-600 outline-none focus:border-primary/50 focus:bg-zinc-900/60 transition-colors";

  // First run (nothing anywhere) drops straight into create; a returning user
  // on a fresh device (cloud profiles, no local) still sees the list so they
  // can Get them. `creating` is the explicit "New profile" toggle.
  const showCreate = creating || (!loading && profiles.length === 0 && cloudProfiles.length === 0);

  return (
    <div className="flex-1 flex items-center justify-center px-6 py-10 bg-background">
      <div className="w-full max-w-[340px] flex flex-col">
        {/* Brand */}
        <div className="flex flex-col items-center mb-8 select-none">
          <img
            src={logoUrl}
            alt=""
            draggable={false}
            className="h-28 w-auto max-w-full object-contain mb-4 drop-shadow-[0_0_32px_rgba(var(--primary),0.22)]"
          />
          <h1 className="text-[22px] font-semibold text-white tracking-tight leading-none">Submarine</h1>
          <p className="text-[10px] text-primary/80 mt-1.5 tracking-[0.22em] uppercase font-semibold">
            Run Silent, Run Deep
          </p>
          <p className="text-[12.5px] text-zinc-500 mt-2">
            {loading
              ? " "
              : showCreate
                ? (rows.length === 0 ? "Let's set up your first profile." : "Create a new profile.")
                : null}
          </p>
        </div>

        {error && (
          <div className="mb-3 px-3 py-2 bg-rose-500/10 border border-rose-500/20 rounded-lg text-rose-200 text-[12.5px] flex items-center gap-2">
            <AlertTriangle size={13} className="shrink-0" /> {error}
          </div>
        )}

        {info && !error && (
          <div className="mb-3 px-3 py-2 bg-emerald-500/10 border border-emerald-500/20 rounded-lg text-emerald-100 text-[12.5px] flex items-center gap-2">
            <CheckCircle2 size={13} className="shrink-0" /> <span className="truncate flex-1">{info}</span>
            <button onClick={() => setInfo(null)} className="text-emerald-200/70 hover:text-white shrink-0"><X size={13} /></button>
          </div>
        )}

        {importStaged && (
          <div className="mb-3 px-3 py-3 bg-zinc-900/60 border border-primary/30 rounded-lg space-y-2 animate-in fade-in">
            <div className="text-[11.5px] text-zinc-300 leading-snug">
              Importing a profile. Pick a name (the file's password is unchanged).
            </div>
            <input
              ref={importNameRef}
              value={importStaged.name}
              onChange={(e) => setImportStaged({ ...importStaged, name: e.target.value })}
              onKeyDown={(e) => e.key === "Enter" && commitImport()}
              className={inputBase + " h-9 text-[13px]"}
              placeholder="Profile name"
            />
            <div className="flex gap-2">
              <button
                onClick={commitImport}
                disabled={busy || !importStaged.name.trim()}
                className="flex-1 h-9 rounded-lg text-[12.5px] font-semibold bg-primary text-black disabled:opacity-50"
              >
                {busy ? "Importing…" : "Import"}
              </button>
              <button
                onClick={() => { setImportStaged(null); setError(null); }}
                className="h-9 px-3 rounded-lg text-[12.5px] font-semibold text-zinc-300 hover:text-white bg-white/5 hover:bg-white/10 border border-white/10"
              >
                Cancel
              </button>
            </div>
          </div>
        )}

        {loading ? (
          <div className="text-center text-zinc-500 text-[12.5px] py-6">Loading…</div>
        ) : showCreate ? (
          /* ---- Create a profile ---- */
          <div className="space-y-3 animate-in fade-in">
            <input
              ref={nameInputRef}
              value={newName}
              onChange={(e) => setNewName(e.target.value)}
              placeholder="Profile name"
              className={inputBase}
            />
            <input
              type="password"
              value={newPassword}
              onChange={(e) => setNewPassword(e.target.value)}
              placeholder="Password"
              className={inputBase}
            />
            <input
              type="password"
              value={newConfirmPassword}
              onChange={(e) => setNewConfirmPassword(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && createNew()}
              placeholder="Confirm password"
              className={inputBase}
            />
            <button
              onClick={createNew}
              disabled={busy}
              className="w-full h-11 rounded-xl text-[14px] font-semibold bg-primary text-black hover:shadow-[0_0_24px_rgba(var(--primary),0.3)] disabled:opacity-50 flex items-center justify-center transition-all"
            >
              {busy ? "Creating…" : "Create profile"}
            </button>

            <div className="flex gap-2 pt-1">
              {rows.length > 0 && (
                <button
                  onClick={() => { setCreating(false); setNewName(""); setNewPassword(""); setNewConfirmPassword(""); setError(null); }}
                  className="flex-1 h-9 rounded-lg bg-white/[0.02] border border-white/5 hover:bg-white/5 hover:border-white/10 text-zinc-400 hover:text-zinc-100 text-[12px] font-medium transition-colors flex items-center justify-center gap-1.5"
                >
                  <X size={12} /> Cancel
                </button>
              )}
              {!IS_ANDROID && (
                <button
                  onClick={startImport}
                  disabled={busy}
                  title="Import an exported .submarine file"
                  className="flex-1 h-9 rounded-lg bg-white/[0.02] border border-white/5 hover:bg-white/5 hover:border-white/10 text-zinc-400 hover:text-zinc-100 text-[12px] font-medium transition-colors flex items-center justify-center gap-1.5 disabled:opacity-50"
                >
                  <Upload size={12} /> Import
                </button>
              )}
            </div>

            <p className="text-[11.5px] text-zinc-500 leading-relaxed text-center px-2 pt-1">
              Profiles are encrypted. If you forget the password, the data is gone for good.
            </p>
          </div>
        ) : (
          /* ---- Your profiles (local + cloud, one list) ---- */
          <div className="space-y-3">
            <div className="flex items-center justify-between px-0.5">
              <span className="text-[10px] font-bold uppercase tracking-[0.18em] text-zinc-500">Your profiles</span>
              {cloudStatus.signed_in && (
                <button
                  onClick={refreshCloud}
                  disabled={busy}
                  title="Refresh from cloud"
                  className="text-zinc-600 hover:text-primary disabled:opacity-40 p-0.5"
                >
                  <RefreshCw size={11} className={busy ? "animate-spin" : ""} />
                </button>
              )}
            </div>

            <div className="rounded-xl border border-white/5 bg-white/[0.02] overflow-hidden divide-y divide-white/5">
              {rows.map((row) => {
                const open = selected === row.name;
                return (
                  <div key={row.name}>
                    <button
                      onClick={() => toggle(row)}
                      className={`w-full flex items-center gap-2.5 px-3 h-11 text-left transition-colors ${open ? "bg-white/[0.03]" : "hover:bg-white/[0.02]"}`}
                    >
                      {row.local
                        ? <HardDrive size={13} className="text-zinc-500 shrink-0" />
                        : <Cloud size={13} className="text-primary/70 shrink-0" />}
                      <span className="flex-1 min-w-0 truncate text-[13.5px] text-zinc-100">{row.name}</span>
                      <StatusChip row={row} />
                      <ChevronDown size={13} className={`text-zinc-600 shrink-0 transition-transform ${open ? "rotate-180" : ""}`} />
                    </button>

                    {open && (
                      <div className="px-3 pb-3 pt-1 bg-black/20 space-y-2.5 animate-in fade-in">
                        {row.local ? (
                          <>
                            <div className="flex gap-2">
                              <input
                                ref={passwordInputRef}
                                type="password"
                                placeholder="Password"
                                value={password}
                                onChange={(e) => setPassword(e.target.value)}
                                onKeyDown={(e) => e.key === "Enter" && unlockSelected()}
                                className="flex-1 h-10 px-3.5 bg-zinc-900/50 border border-white/5 rounded-lg text-[13.5px] text-zinc-50 placeholder:text-zinc-600 outline-none focus:border-primary/50 transition-colors"
                              />
                              <button
                                onClick={unlockSelected}
                                disabled={busy || !password}
                                title="Open"
                                className="h-10 px-3.5 rounded-lg text-[13px] font-semibold bg-primary text-black hover:shadow-[0_0_20px_rgba(var(--primary),0.3)] disabled:opacity-40 flex items-center gap-1.5 shrink-0 transition-all"
                              >
                                {busy ? <RefreshCw size={14} className="animate-spin" /> : <>Open <ArrowRight size={14} /></>}
                              </button>
                            </div>
                            <div className="flex flex-wrap items-center gap-1.5">
                              {!IS_ANDROID && (
                                <RowAction onClick={() => exportProfile(row.name)} disabled={busy} icon={<Download size={12} />} label="Export" />
                              )}
                              <RowAction onClick={() => removeLocal(row.name)} disabled={busy} icon={<Trash2 size={12} />} label="Remove local" danger />
                              {row.cloud && (
                                <RowAction onClick={() => deleteFromCloud(row)} disabled={busy} icon={<CloudOff size={12} />} label="Delete from cloud" danger />
                              )}
                            </div>
                          </>
                        ) : (
                          <>
                            <p className="text-[11.5px] text-zinc-400 leading-snug">
                              Not on this device yet · {row.cloud!.live_records} item{row.cloud!.live_records === 1 ? "" : "s"}.
                              Get it with this profile's password.
                            </p>
                            <div className="flex items-center gap-1.5">
                              <button
                                onClick={() => bringDown(row.cloud!)}
                                disabled={busy}
                                className="flex-1 h-9 rounded-lg text-[12.5px] font-semibold bg-primary/10 border border-primary/30 text-primary hover:bg-primary hover:text-black disabled:opacity-40 flex items-center justify-center gap-1.5 transition-colors"
                              >
                                <Download size={13} /> Get on this device
                              </button>
                              <RowAction onClick={() => deleteFromCloud(row)} disabled={busy} icon={<CloudOff size={12} />} label="Delete" danger />
                            </div>
                          </>
                        )}
                      </div>
                    )}
                  </div>
                );
              })}
            </div>

            <div className="flex gap-2 pt-0.5">
              <button
                onClick={() => setCreating(true)}
                className="flex-1 h-9 rounded-lg bg-white/[0.02] border border-white/5 hover:bg-white/5 hover:border-white/10 text-zinc-400 hover:text-zinc-100 text-[12px] font-medium transition-colors flex items-center justify-center gap-1.5"
              >
                <Plus size={12} /> New profile
              </button>
              {!IS_ANDROID && (
                <button
                  onClick={startImport}
                  disabled={busy}
                  title="Import an exported .submarine file"
                  className="flex-1 h-9 rounded-lg bg-white/[0.02] border border-white/5 hover:bg-white/5 hover:border-white/10 text-zinc-400 hover:text-zinc-100 text-[12px] font-medium transition-colors flex items-center justify-center gap-1.5 disabled:opacity-50"
                >
                  <Upload size={12} /> Import
                </button>
              )}
            </div>
          </div>
        )}

        {/* Cloud status bar — always visible. Signed-out shows a Connect
            link; signed-in shows the account email + a way into Manage. */}
        <CloudBar status={cloudStatus} busy={busy} onManage={() => setCloudOpen(true)} />

        {/* About + Donate — a matched pair of pills, with a live "new version"
            notice underneath when one is available. */}
        <div className="mt-4 flex flex-col items-center gap-2.5">
          <div className="flex items-center gap-2">
            <button
              onClick={() => setAboutOpen(true)}
              className="h-8 px-4 rounded-lg text-[12.5px] font-bold text-zinc-200 bg-white/[0.04] border border-white/10 hover:bg-white/[0.09] hover:text-white transition-colors"
            >
              About
            </button>
            <button
              onClick={openDonate}
              title="Support Submarine on GitHub"
              className="h-8 px-4 rounded-lg text-[12.5px] font-bold text-rose-200 bg-rose-500/10 border border-rose-500/25 hover:bg-rose-500/20 hover:text-rose-100 transition-colors flex items-center gap-1.5"
            >
              <Heart size={13} className="fill-rose-400/40" /> Donate
            </button>
          </div>
          {update?.has_update && update.latest && (
            <button
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
        </div>
      </div>

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

      <AboutPanel isOpen={aboutOpen} onClose={() => setAboutOpen(false)} />
    </div>
  );
};

// Small pill telling you where a profile lives at a glance.
const StatusChip = ({ row }: { row: Row }) => {
  const base = "inline-flex items-center gap-1 px-1.5 h-5 rounded-md text-[9.5px] font-semibold uppercase tracking-wide border shrink-0";
  if (row.local && row.cloud)
    return <span className={`${base} text-emerald-300 bg-emerald-500/10 border-emerald-500/20`}><CheckCircle2 size={10} /> Synced</span>;
  if (row.local)
    return <span className={`${base} text-zinc-400 bg-white/[0.04] border-white/10`}><HardDrive size={10} /> This device</span>;
  return <span className={`${base} text-primary bg-primary/10 border-primary/25`}><Cloud size={10} /> In cloud</span>;
};

// A compact secondary action inside an expanded row.
const RowAction = ({
  onClick, disabled, icon, label, danger,
}: {
  onClick: () => void; disabled?: boolean; icon: React.ReactNode; label: string; danger?: boolean;
}) => (
  <button
    onClick={onClick}
    disabled={disabled}
    title={label}
    className={`h-7 px-2 rounded-md text-[11px] font-medium border flex items-center gap-1 disabled:opacity-40 transition-colors ${
      danger
        ? "text-rose-300/90 bg-rose-500/5 border-rose-500/15 hover:bg-rose-500/15 hover:text-rose-200"
        : "text-zinc-400 bg-white/[0.03] border-white/10 hover:bg-white/[0.07] hover:text-zinc-100"
    }`}
  >
    {icon} {label}
  </button>
);

const CloudBar = ({
  status, busy, onManage,
}: {
  status: CloudStatus;
  busy: boolean;
  onManage: () => void;
}) => {
  // Signed-out: a thin, low-weight link rather than a fourth full-width
  // button — the user hasn't asked for cloud yet, so we don't want it
  // competing visually with the profile list.
  if (!status.signed_in) {
    return (
      <button
        onClick={onManage}
        className="mt-4 h-7 mx-auto text-[11.5px] text-zinc-500 hover:text-primary transition-colors flex items-center justify-center gap-1.5"
      >
        <Cloud size={11} /> Connect cloud sync
      </button>
    );
  }
  return (
    <div className="mt-4 h-9 px-2.5 rounded-lg border border-white/5 bg-white/[0.02] flex items-center gap-2 text-[11.5px]">
      <Cloud size={12} className="text-primary shrink-0" />
      <span className="text-zinc-300 truncate font-mono flex-1 min-w-0">{status.email}</span>
      <span className="text-emerald-400 flex items-center gap-1 shrink-0">
        <CheckCircle2 size={11} /> Connected
      </span>
      <button
        onClick={onManage}
        disabled={busy}
        title="Manage cloud account"
        className="text-zinc-500 hover:text-primary disabled:opacity-50 shrink-0 p-0.5"
      >
        <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><circle cx="12" cy="12" r="1"/><circle cx="12" cy="5" r="1"/><circle cx="12" cy="19" r="1"/></svg>
      </button>
    </div>
  );
};

export default ProfileSelectPage;
