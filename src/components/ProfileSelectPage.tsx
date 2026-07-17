import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Plus, Trash2, X, AlertTriangle, ArrowRight, Download, Upload, CheckCircle2, Cloud, RefreshCw, Shield } from "lucide-react";
import CloudPanel from "./CloudPanel";
import AboutPanel from "./AboutPanel";
import logoUrl from "../assets/logo.png";
import { IS_ANDROID } from "../util/platform";

interface CloudStatus { signed_in: boolean; email: string | null; }

interface Props {
  onUnlocked: (profileName: string) => void;
}

const ProfileSelectPage = ({ onUnlocked }: Props) => {
  const [profiles, setProfiles] = useState<string[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [info, setInfo] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [selected, setSelected] = useState<string>("");
  const [password, setPassword] = useState("");

  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);

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

  const passwordInputRef = useRef<HTMLInputElement | null>(null);
  const nameInputRef = useRef<HTMLInputElement | null>(null);
  const importNameRef = useRef<HTMLInputElement | null>(null);

  const reload = async () => {
    setLoading(true); setError(null);
    try {
      const list = await invoke<string[]>("list_profiles");
      setProfiles(list);
      setSelected((prev) => (prev && list.includes(prev) ? prev : list[0] || ""));
      // Auto-jump into create mode when there are no profiles yet.
      if (list.length === 0) setCreating(true);
    } catch (e: any) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  };
  useEffect(() => { reload(); /* eslint-disable-next-line */ }, []);

  // Cloud connection status only. Per-profile sync now happens inside the app
  // (each profile's Profile panel), so the picker just shows whether the cloud
  // account is connected + a way in to manage it. Best-effort: a network
  // failure shouldn't block unlocking a local profile, so errors are swallowed.
  const refreshCloud = useCallback(async () => {
    try {
      setCloudStatus(await invoke<CloudStatus>("cloud_status"));
    } catch {
      // swallow — network down, modal can still be opened to retry
    }
  }, []);

  useEffect(() => { refreshCloud(); }, [refreshCloud]);

  useEffect(() => {
    setPassword("");
    setError(null);
    setConfirmDelete(null);
    requestAnimationFrame(() => passwordInputRef.current?.focus());
  }, [selected]);

  useEffect(() => {
    if (creating) requestAnimationFrame(() => nameInputRef.current?.focus());
  }, [creating]);

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
      if (raw.includes("DECRYPT") || raw.toLowerCase().includes("decrypt")) {
        setError("Wrong password.");
      } else {
        setError(raw);
      }
    } finally {
      setBusy(false);
    }
  };

  const deleteProfile = async (name: string) => {
    setBusy(true); setError(null);
    try {
      await invoke("delete_profile", { name });
      setConfirmDelete(null);
      await reload();
    } catch (e: any) {
      setError(String(e));
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

  const exportSelected = async () => {
    if (!selected) return;
    setBusy(true); setError(null); setInfo(null);
    try {
      // Returns the chosen path on success, null if the user cancelled the
      // save dialog. We only surface a toast in the success case.
      const saved = await invoke<string | null>("export_profile", { name: selected });
      if (saved) setInfo(`Exported to ${saved}`);
    } catch (e: any) {
      setError(String(e));
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
      setError(String(e));
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
      await invoke("import_profile_save", {
        sourcePath: importStaged.sourcePath,
        name: trimmed,
      });
      setImportStaged(null);
      setInfo(`Imported as "${trimmed}". Unlock with the original password.`);
      await reload();
      setSelected(trimmed);
    } catch (e: any) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const inputBase =
    "w-full h-11 px-4 bg-zinc-900/40 border border-white/5 rounded-xl text-[14px] text-zinc-50 placeholder:text-zinc-600 outline-none focus:border-primary/50 focus:bg-zinc-900/60 transition-colors";

  return (
    <div className="flex-1 flex items-center justify-center px-6 py-10 bg-background">
      <div className="w-full max-w-[340px] flex flex-col">
        {/* Brand */}
        <div className="flex flex-col items-center mb-8 select-none">
          <img
            src={logoUrl}
            alt=""
            draggable={false}
            // h-28 + object-contain + width:auto so a non-square source
            // (the current app-logo.png is 1822×1770, slight taller-than-
            // wide) fits the brand area without being cropped or stretched
            // — earlier the forced w-28 h-28 squashed it into a square
            // and clipped the edges.
            className="h-28 w-auto max-w-full object-contain mb-4 drop-shadow-[0_0_32px_rgba(var(--primary),0.22)]"
          />
          <h1 className="text-[22px] font-semibold text-white tracking-tight leading-none">Submarine</h1>
          <p className="text-[10px] text-primary/80 mt-1.5 tracking-[0.22em] uppercase font-semibold">
            Run Silent, Run Deep
          </p>
          {/* Security shorthand right under the brand: users landing on
              the unlock screen see immediately what protects their data
              without having to open About. Argon2 for the KDF (so a weak
              password buys real time) + AES-256-GCM for the vault blob
              (so an offline attacker with the file still has to guess). */}
          <div
            title="Password → Argon2 → AES-256-GCM. Servers, keys, notes, commands — nothing decryptable without your password."
            className="mt-3 inline-flex items-center gap-1.5 h-6 px-2 rounded-full bg-emerald-500/8 border border-emerald-500/25 text-[9.5px] font-semibold uppercase tracking-[0.18em] text-emerald-300"
          >
            <Shield size={10} /> Argon2 · AES-256-GCM
          </div>
          <p className="text-[12.5px] text-zinc-500 mt-2">
            {loading
              ? " "
              : profiles.length === 0
                ? "Let's set up your first profile."
                : creating
                  ? "Create a new profile."
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
        ) : !creating ? (
          <div className="space-y-3">
            <div className="relative group">
              <select
                value={selected}
                onChange={(e) => setSelected(e.target.value)}
                className={`${inputBase} pr-16 cursor-pointer appearance-none`}
              >
                {profiles.map((p) => (
                  <option key={p} value={p} className="bg-[#121214] text-zinc-100">
                    {p}
                  </option>
                ))}
              </select>
              {/* Export/Import Profile use rfd's native save/pick dialogs
                  on the desktop; Android has no equivalent and the backend
                  returns an error. Hide the buttons on Android so users
                  don't hit a raw error toast — sync + Cloud Backup is the
                  cross-device transport there. */}
              {!IS_ANDROID && (
                <button
                  onClick={exportSelected}
                  disabled={!selected || busy}
                  title="Export profile (encrypted file)"
                  className="absolute right-9 top-1/2 -translate-y-1/2 w-7 h-7 rounded-md text-zinc-500 hover:text-primary hover:bg-primary/10 disabled:opacity-30 flex items-center justify-center transition-all opacity-100 sm:opacity-0 sm:group-hover:opacity-100 sm:focus:opacity-100"
                >
                  <Download size={13} />
                </button>
              )}
              <button
                onClick={() => setConfirmDelete(selected)}
                disabled={!selected || busy}
                title="Delete profile"
                className="absolute right-2 top-1/2 -translate-y-1/2 w-7 h-7 rounded-md text-zinc-500 hover:text-rose-300 hover:bg-rose-500/10 disabled:opacity-30 flex items-center justify-center transition-all opacity-100 sm:opacity-0 sm:group-hover:opacity-100 sm:focus:opacity-100"
              >
                <Trash2 size={13} />
              </button>
            </div>

            {confirmDelete && (
              <div className="px-3 py-2 bg-rose-500/10 border border-rose-500/20 rounded-lg flex items-center gap-2 animate-in fade-in">
                <span className="flex-1 text-[12.5px] text-rose-100 truncate">
                  Delete <span className="font-semibold">{confirmDelete}</span>?
                </span>
                <button onClick={() => deleteProfile(confirmDelete)} disabled={busy}
                  className="h-7 px-3 rounded-md text-[11.5px] font-semibold bg-rose-500/80 text-white hover:bg-rose-500 disabled:opacity-50">
                  Yes
                </button>
                <button onClick={() => setConfirmDelete(null)}
                  className="h-7 px-3 rounded-md text-[11.5px] font-semibold text-zinc-300 hover:text-white">
                  Cancel
                </button>
              </div>
            )}

            <input
              ref={passwordInputRef}
              type="password"
              placeholder="Password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && unlockSelected()}
              className={inputBase}
            />

            <button
              onClick={unlockSelected}
              disabled={busy || !selected}
              className="w-full h-11 rounded-xl text-[14px] font-semibold bg-primary text-black hover:shadow-[0_0_24px_rgba(var(--primary),0.3)] disabled:opacity-50 flex items-center justify-center gap-2 transition-all"
            >
              {busy ? (<><RefreshCw size={14} className="animate-spin" /> Deriving key…</>) : (<>Open <ArrowRight size={15} /></>)}
            </button>

            <div className="flex gap-2 pt-1">
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
        ) : (
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
              {profiles.length > 0 && (
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
        )}

        {/* Cloud status bar — always visible. Signed-out shows a Connect
            link; signed-in shows the account email + a way into Manage. */}
        <CloudBar
          status={cloudStatus}
          busy={busy}
          onManage={() => setCloudOpen(true)}
        />

        {/* About — deliberately tiny, off to the side. Clicking opens
            a modal with version + update check + project links. */}
        <button
          onClick={() => setAboutOpen(true)}
          className="mt-3 mx-auto text-[10.5px] text-zinc-600 hover:text-primary transition-colors"
        >
          About Submarine
        </button>
      </div>

      <CloudPanel
        isOpen={cloudOpen}
        onClose={() => {
          setCloudOpen(false);
          // The modal may have logged in/out or changed cloud state —
          // re-pull so the bar reflects reality.
          refreshCloud();
        }}
        onLocalProfilesChanged={reload}
      />

      <AboutPanel isOpen={aboutOpen} onClose={() => setAboutOpen(false)} />
    </div>
  );
};

const CloudBar = ({
  status, busy, onManage,
}: {
  status: CloudStatus;
  busy: boolean;
  onManage: () => void;
}) => {
  // Signed-out: a thin, low-weight link rather than a fourth full-width
  // button — the user hasn't asked for cloud yet, so we don't want it
  // competing visually with Open / New / Import.
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
        title="Manage cloud"
        className="text-zinc-500 hover:text-primary disabled:opacity-50 shrink-0 p-0.5"
      >
        <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><circle cx="12" cy="12" r="1"/><circle cx="12" cy="5" r="1"/><circle cx="12" cy="19" r="1"/></svg>
      </button>
    </div>
  );
};

export default ProfileSelectPage;
