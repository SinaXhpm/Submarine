import { useEffect, useState, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  X, AlertTriangle, CheckCircle2, Cloud, ArrowRight, ArrowLeft, Upload, Download,
  Trash2, RefreshCw, LogOut, Mail, KeyRound, UserPlus, LogIn, Wand2,
} from "lucide-react";
import { useConfirm } from "../ui/confirm";

// Cloud sync panel. Owns its own auth + sync state — the parent just
// mounts/unmounts. The signed-out experience is split into two clearly
// separate flows so the user picks intent first instead of seeing two
// half-forms at once:
//   chooser     → two big buttons: Sign in / Sign up
//   sign-in     → email + password
//   sign-up     → email → verify token → set password
//   signed-in   → unified profile list with per-row actions and Sync All

interface CloudStatus { signed_in: boolean; email: string | null; }

interface RemoteProfile {
  id: number; name: string; version: number;
  size_bytes: number; last_modified: string;
}

type UploadOutcome =
  | { kind: "uploaded"; id: number; version: number }
  | { kind: "conflict"; server_version: number };

type SyncStatusKind = "both" | "local_only" | "remote_only";

interface SyncEntry {
  name: string;
  status: SyncStatusKind;
  remote: RemoteProfile | null;
  local_size_bytes: number | null;
}

interface SyncAllReport {
  uploaded: string[];
  downloaded: string[];
  skipped_conflicts: string[];
  failed: string[];
}

interface Props {
  isOpen: boolean;
  onClose: () => void;
  localProfiles: string[];
  onLocalProfilesChanged: () => void;
}

type Stage =
  | "chooser"
  | "sign-in"
  | "sign-up-email"
  | "sign-up-verify"
  | "sign-up-password"
  | "forgot-email"
  | "forgot-reset"
  | "link-email"
  | "link-verify"
  | "signed-in";

const fmtBytes = (n: number | null | undefined): string => {
  if (n == null) return "—";
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
};

const CloudPanel = ({ isOpen, onClose, localProfiles, onLocalProfilesChanged }: Props) => {
  const confirm = useConfirm();
  const [stage, setStage] = useState<Stage>("chooser");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [confirmPw, setConfirmPw] = useState("");
  const [verifyToken, setVerifyToken] = useState("");
  const [claimToken, setClaimToken] = useState<string | null>(null);
  const [resetToken, setResetToken] = useState("");
  const [loginLinkToken, setLoginLinkToken] = useState("");
  const [signedInEmail, setSignedInEmail] = useState<string | null>(null);

  const [entries, setEntries] = useState<SyncEntry[]>([]);
  const [busy, setBusy] = useState(false);
  const [syncing, setSyncing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [info, setInfo] = useState<string | null>(null);

  const [conflict, setConflict] = useState<{ name: string; serverVersion: number } | null>(null);

  const resetForms = () => {
    setEmail(""); setPassword(""); setConfirmPw("");
    setVerifyToken(""); setClaimToken(null);
    setResetToken(""); setLoginLinkToken("");
    setError(null); setInfo(null);
  };

  const refreshStatus = useCallback(async () => {
    try {
      const s = await invoke<CloudStatus>("cloud_status");
      if (s.signed_in) {
        setStage("signed-in");
        setSignedInEmail(s.email);
      } else {
        // Only reset to chooser if we're not mid-flow (verify / set-password
        // shouldn't bounce back to chooser when the panel re-opens).
        setStage((prev) =>
          prev === "sign-up-verify" || prev === "sign-up-password" ? prev : "chooser",
        );
      }
    } catch (e: any) {
      setError(String(e));
    }
  }, []);

  const refreshOverview = useCallback(async () => {
    setError(null);
    try {
      const list = await invoke<SyncEntry[]>("cloud_sync_overview", { localProfiles });
      setEntries(list);
    } catch (e: any) {
      setError(String(e));
    }
  }, [localProfiles]);

  useEffect(() => { if (isOpen) refreshStatus(); }, [isOpen, refreshStatus]);
  useEffect(() => { if (stage === "signed-in") refreshOverview(); }, [stage, refreshOverview]);

  // ---- Auth handlers ---------------------------------------------------

  const handleLogin = async () => {
    setError(null);
    if (!email.trim() || !password) { setError("Email and password required"); return; }
    setBusy(true);
    try {
      const s = await invoke<CloudStatus>("cloud_login", {
        email: email.trim(), password,
      });
      if (s.signed_in) {
        setSignedInEmail(s.email);
        setStage("signed-in");
        resetForms();
      }
    } catch (e: any) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const handleSignupEmail = async () => {
    setError(null); setInfo(null);
    if (!email.trim()) { setError("Enter your email"); return; }
    setBusy(true);
    try {
      await invoke("cloud_signup", { email: email.trim() });
      setInfo("Check your inbox for the verification link. Paste the token below.");
      setStage("sign-up-verify");
    } catch (e: any) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const handleVerify = async () => {
    setError(null);
    if (!verifyToken.trim()) { setError("Paste the verify token from your email"); return; }
    setBusy(true);
    try {
      const resp = await invoke<{ claim_token: string; email: string }>(
        "cloud_consume_verify_link",
        { verifyToken: verifyToken.trim() },
      );
      setClaimToken(resp.claim_token);
      setEmail(resp.email);
      setStage("sign-up-password");
      setInfo("Email verified. Now set a password for your account.");
    } catch (e: any) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const handleSetPassword = async () => {
    setError(null);
    if (!claimToken) { setError("Verify your email first"); return; }
    if (password.length < 8) { setError("Password must be at least 8 characters"); return; }
    if (password !== confirmPw) { setError("Passwords don't match"); return; }
    setBusy(true);
    try {
      const s = await invoke<CloudStatus>("cloud_set_password", { claimToken, password });
      if (s.signed_in) {
        setSignedInEmail(s.email);
        setStage("signed-in");
        setInfo("Account created and signed in.");
        resetForms();
      }
    } catch (e: any) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const handleRequestReset = async () => {
    setError(null); setInfo(null);
    if (!email.trim()) { setError("Enter your email"); return; }
    setBusy(true);
    try {
      await invoke("cloud_request_password_reset", { email: email.trim() });
      setInfo("If that email is registered, a reset link is on its way. Paste the token below.");
      setStage("forgot-reset");
    } catch (e: any) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const handleResetPassword = async () => {
    setError(null);
    if (!resetToken.trim()) { setError("Paste the reset token from your email"); return; }
    if (password.length < 8) { setError("Password must be at least 8 characters"); return; }
    if (password !== confirmPw) { setError("Passwords don't match"); return; }
    setBusy(true);
    try {
      const s = await invoke<CloudStatus>("cloud_reset_password", {
        resetToken: resetToken.trim(), password,
      });
      if (s.signed_in) {
        setSignedInEmail(s.email);
        setStage("signed-in");
        setInfo("Password updated and signed in. Other devices have been signed out.");
        resetForms();
      }
    } catch (e: any) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const handleRequestLoginLink = async () => {
    setError(null); setInfo(null);
    if (!email.trim()) { setError("Enter your email"); return; }
    setBusy(true);
    try {
      await invoke("cloud_request_login_link", { email: email.trim() });
      setInfo("If that email is registered, a sign-in link is on its way. Paste the token below.");
      setStage("link-verify");
    } catch (e: any) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const handleLoginWithLink = async () => {
    setError(null);
    if (!loginLinkToken.trim()) { setError("Paste the sign-in token from your email"); return; }
    setBusy(true);
    try {
      const s = await invoke<CloudStatus>("cloud_login_with_link", {
        loginToken: loginLinkToken.trim(),
      });
      if (s.signed_in) {
        setSignedInEmail(s.email);
        setStage("signed-in");
        resetForms();
      }
    } catch (e: any) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const handleLogout = async () => {
    setBusy(true);
    try {
      await invoke("cloud_logout");
      setStage("chooser");
      setSignedInEmail(null);
      setEntries([]);
      resetForms();
    } catch (e: any) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const goBack = () => {
    setError(null); setInfo(null);
    setStage("chooser");
  };

  // ---- Sync handlers ---------------------------------------------------

  const doUpload = async (name: string) => {
    setBusy(true); setError(null); setInfo(null);
    try {
      const outcome = await invoke<UploadOutcome>("cloud_upload_profile", { name });
      if (outcome.kind === "uploaded") {
        setInfo(`Uploaded "${name}" (v${outcome.version})`);
        await refreshOverview();
      } else {
        setConflict({ name, serverVersion: outcome.server_version });
      }
    } catch (e: any) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const doForceUpload = async () => {
    if (!conflict) return;
    setBusy(true); setError(null);
    try {
      const outcome = await invoke<UploadOutcome>("cloud_force_upload_profile", {
        name: conflict.name,
        serverVersion: conflict.serverVersion,
      });
      if (outcome.kind === "uploaded") {
        setInfo(`Overwrote "${conflict.name}" (v${outcome.version})`);
        setConflict(null);
        await refreshOverview();
      } else {
        setConflict({ name: conflict.name, serverVersion: outcome.server_version });
      }
    } catch (e: any) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const doDownload = async (entry: SyncEntry) => {
    if (!entry.remote) return;
    let saveAs = entry.name;
    let overwrite = false;
    if (entry.status === "both") {
      // The user explicitly clicked download on a profile that already
      // exists locally — they're saying "pull the cloud copy". The Android
      // use case for this is the dominant one: the phone's local vault is
      // months behind the desktop and they want the fresh version. So we
      // default to OVERWRITE rather than the old rename-with-suffix flow,
      // which was confusing on small screens (you'd end up with `node-cloud`
      // as a phantom second profile and have to manually clean it up).
      const ok = await confirm({
        title: "Replace local profile?",
        message:
          `This will replace your local "${entry.name}" with the cloud copy (v${entry.remote.version}). ` +
          `Any changes you made locally since the cloud version was uploaded will be lost.\n\n` +
          `If your local copy is stale and you want the cloud version, this is what you want.`,
        okLabel: "Replace local",
        cancelLabel: "Cancel",
        destructive: true,
      });
      if (!ok) return;
      overwrite = true;
    }
    setBusy(true); setError(null); setInfo(null);
    try {
      await invoke("cloud_download_profile", {
        remoteId: entry.remote.id,
        saveAs,
        overwrite,
      });
      setInfo(overwrite
        ? `Replaced "${entry.name}" with cloud copy.`
        : `Downloaded "${entry.name}" → "${saveAs}"`);
      onLocalProfilesChanged();
      await refreshOverview();
    } catch (e: any) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const doDeleteRemote = async (entry: SyncEntry) => {
    if (!entry.remote) return;
    if (!(await confirm({
      title: "Delete cloud profile?",
      message:
        `Delete "${entry.name}" from cloud? ` +
        (entry.status === "both" ? "Local copy stays untouched." : "This cannot be undone."),
      destructive: true,
    }))) return;
    setBusy(true); setError(null);
    try {
      await invoke("cloud_delete_remote_profile", { remoteId: entry.remote.id });
      setInfo(`Removed "${entry.name}" from cloud.`);
      await refreshOverview();
    } catch (e: any) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const doSyncAll = async () => {
    if (syncing) return;
    setSyncing(true); setError(null); setInfo(null);
    try {
      const report = await invoke<SyncAllReport>("cloud_sync_all", { localProfiles });
      const parts: string[] = [];
      if (report.uploaded.length) parts.push(`uploaded ${report.uploaded.length}`);
      if (report.downloaded.length) parts.push(`downloaded ${report.downloaded.length}`);
      if (report.skipped_conflicts.length) parts.push(`${report.skipped_conflicts.length} need manual pick`);
      if (report.failed.length) parts.push(`${report.failed.length} failed`);
      const summary = parts.length ? `Sync complete: ${parts.join(", ")}.` : "Already in sync.";
      setInfo(summary);
      if (report.failed.length) setError(report.failed.join("\n"));
      onLocalProfilesChanged();
      await refreshOverview();
    } catch (e: any) {
      setError(String(e));
    } finally {
      setSyncing(false);
    }
  };

  // ---- Render ----------------------------------------------------------

  if (!isOpen) return null;

  const inputBase =
    "w-full h-10 px-3 bg-zinc-900/60 border border-white/10 rounded-lg text-[13px] text-zinc-50 placeholder:text-zinc-600 outline-none focus:border-primary/50 transition-colors";

  const sortedEntries = [...entries].sort((a, b) => {
    const order: Record<SyncStatusKind, number> = { local_only: 0, remote_only: 1, both: 2 };
    return order[a.status] - order[b.status] || a.name.localeCompare(b.name);
  });

  const stats = entries.reduce(
    (acc, e) => { acc[e.status]++; return acc; },
    { both: 0, local_only: 0, remote_only: 0 } as Record<SyncStatusKind, number>,
  );
  const needsAction = stats.local_only + stats.remote_only;

  return (
    <div className="fixed inset-0 z-50 bg-black/60 backdrop-blur-sm flex items-center justify-center p-3" onClick={onClose}>
      <div
        className="w-full max-w-3xl max-h-[90vh] flex flex-col bg-[#121214] border border-white/10 rounded-xl shadow-2xl"
        onClick={(e) => e.stopPropagation()}
      >
        {/* Header */}
        <div className="shrink-0 px-4 py-3 border-b border-white/5 flex items-center justify-between">
          <div className="flex items-center gap-2 text-[12px] font-bold uppercase tracking-widest text-white">
            <Cloud size={14} className="text-primary" />
            <span>Cloud Sync</span>
            {signedInEmail && stage === "signed-in" && (
              <span className="text-zinc-500 font-mono normal-case text-[11px] tracking-normal ml-2">
                {signedInEmail}
              </span>
            )}
          </div>
          <button onClick={onClose} className="text-zinc-400 hover:text-white"><X size={16} /></button>
        </div>

        <div className="flex-1 overflow-y-auto custom-scrollbar p-5 space-y-3">
          {error && (
            <div className="px-3 py-2 bg-rose-500/15 border border-rose-500/30 rounded text-rose-200 text-[12px] flex items-start gap-2">
              <AlertTriangle size={13} className="mt-0.5 shrink-0" />
              <span className="flex-1 break-words whitespace-pre-line">{error}</span>
              <button onClick={() => setError(null)} className="text-rose-300/70 hover:text-white mt-0.5"><X size={12} /></button>
            </div>
          )}
          {info && !error && (
            <div className="px-3 py-2 bg-emerald-500/10 border border-emerald-500/20 rounded text-emerald-100 text-[12px] flex items-center gap-2">
              <CheckCircle2 size={13} /> <span className="flex-1 break-words">{info}</span>
              <button onClick={() => setInfo(null)} className="text-emerald-200/70 hover:text-white"><X size={12} /></button>
            </div>
          )}

          {/* ----- Chooser ----- */}
          {stage === "chooser" && (
            <div className="max-w-sm mx-auto py-6 space-y-3">
              <p className="text-center text-[12.5px] text-zinc-400 pb-2">
                Sync encrypted profiles across devices.
              </p>
              <button
                onClick={() => { resetForms(); setStage("sign-in"); }}
                className="w-full h-12 rounded-lg bg-primary text-black text-[13.5px] font-bold flex items-center justify-center gap-2"
              >
                <LogIn size={15} /> Sign in
              </button>
              <button
                onClick={() => { resetForms(); setStage("sign-up-email"); }}
                className="w-full h-12 rounded-lg bg-white/5 border border-white/10 text-zinc-100 hover:bg-white/10 text-[13.5px] font-bold flex items-center justify-center gap-2"
              >
                <UserPlus size={15} /> Create account
              </button>
              <button
                onClick={() => setStage("sign-up-verify")}
                className="w-full h-9 text-[12px] text-zinc-500 hover:text-primary"
              >
                Have a verify token? Continue verifying →
              </button>
            </div>
          )}

          {/* ----- Sign in ----- */}
          {stage === "sign-in" && (
            <div className="max-w-sm mx-auto py-4 space-y-3">
              <div className="flex items-center gap-2 pb-1">
                <button onClick={goBack} className="text-zinc-400 hover:text-white"><ArrowLeft size={14} /></button>
                <span className="text-[12.5px] font-bold uppercase tracking-wider text-zinc-300">Sign in</span>
              </div>
              <input
                value={email}
                onChange={(e) => setEmail(e.target.value)}
                placeholder="you@example.com"
                className={inputBase}
                autoFocus
              />
              <input
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && handleLogin()}
                placeholder="Password"
                className={inputBase}
              />
              <button
                onClick={handleLogin}
                disabled={busy}
                className="w-full h-10 rounded-lg bg-primary text-black text-[13px] font-bold disabled:opacity-50 flex items-center justify-center gap-2"
              >
                {busy ? "…" : (<>Sign in <ArrowRight size={14} /></>)}
              </button>
              <div className="flex items-center gap-2 pt-1">
                <button
                  onClick={() => { setError(null); setInfo(null); setPassword(""); setStage("link-email"); }}
                  className="flex-1 h-9 rounded text-[11.5px] text-zinc-300 hover:text-primary hover:bg-white/5 border border-white/10 flex items-center justify-center gap-1.5"
                >
                  <Wand2 size={12} /> Email me a sign-in link
                </button>
                <button
                  onClick={() => { setError(null); setInfo(null); setPassword(""); setStage("forgot-email"); }}
                  className="flex-1 h-9 rounded text-[11.5px] text-zinc-300 hover:text-primary hover:bg-white/5 border border-white/10"
                >
                  Forgot password?
                </button>
              </div>
            </div>
          )}

          {/* ----- Forgot password: email ----- */}
          {stage === "forgot-email" && (
            <div className="max-w-sm mx-auto py-4 space-y-3">
              <div className="flex items-center gap-2 pb-1">
                <button onClick={goBack} className="text-zinc-400 hover:text-white"><ArrowLeft size={14} /></button>
                <span className="text-[12.5px] font-bold uppercase tracking-wider text-zinc-300">Reset password</span>
              </div>
              <p className="text-[11.5px] text-zinc-500 leading-relaxed">
                Enter the email you signed up with. If it's registered, we'll send a reset token.
                Your local vault and saved servers are not affected — only the cloud-sync password.
              </p>
              <input
                value={email}
                onChange={(e) => setEmail(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && handleRequestReset()}
                placeholder="you@example.com"
                className={inputBase}
                autoFocus
              />
              <button
                onClick={handleRequestReset}
                disabled={busy}
                className="w-full h-10 rounded-lg bg-primary text-black text-[13px] font-bold disabled:opacity-50 flex items-center justify-center gap-2"
              >
                {busy ? "…" : (<><Mail size={13} /> Send reset email</>)}
              </button>
              <button
                onClick={() => { setError(null); setInfo(null); setStage("forgot-reset"); }}
                className="w-full h-9 text-[12px] text-zinc-500 hover:text-primary"
              >
                Already have a reset token? Continue →
              </button>
            </div>
          )}

          {/* ----- Forgot password: paste token + new password ----- */}
          {stage === "forgot-reset" && (
            <div className="max-w-sm mx-auto py-4 space-y-3">
              <div className="flex items-center gap-2 pb-1">
                <button onClick={goBack} className="text-zinc-400 hover:text-white"><ArrowLeft size={14} /></button>
                <span className="text-[12.5px] font-bold uppercase tracking-wider text-zinc-300">New password</span>
              </div>
              <p className="text-[12px] text-zinc-300">
                Paste the reset token from your email, then set a new password.
              </p>
              <input
                value={resetToken}
                onChange={(e) => setResetToken(e.target.value)}
                placeholder="reset token"
                className={inputBase + " font-mono"}
                autoFocus
              />
              <input
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                placeholder="New password (min 8 chars)"
                className={inputBase}
              />
              <input
                type="password"
                value={confirmPw}
                onChange={(e) => setConfirmPw(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && handleResetPassword()}
                placeholder="Confirm new password"
                className={inputBase}
              />
              <button
                onClick={handleResetPassword}
                disabled={busy}
                className="w-full h-10 rounded-lg bg-primary text-black text-[13px] font-bold disabled:opacity-50"
              >
                {busy ? "…" : "Set new password & sign in"}
              </button>
              <p className="text-[11px] text-zinc-500 leading-relaxed pt-1">
                Heads up: changing the password signs out any other devices currently signed in to this account.
              </p>
            </div>
          )}

          {/* ----- Magic link: request ----- */}
          {stage === "link-email" && (
            <div className="max-w-sm mx-auto py-4 space-y-3">
              <div className="flex items-center gap-2 pb-1">
                <button onClick={goBack} className="text-zinc-400 hover:text-white"><ArrowLeft size={14} /></button>
                <span className="text-[12.5px] font-bold uppercase tracking-wider text-zinc-300">Sign in by email</span>
              </div>
              <p className="text-[11.5px] text-zinc-500 leading-relaxed">
                We'll email you a one-time sign-in code. No password needed this time —
                your existing password stays as it is.
              </p>
              <input
                value={email}
                onChange={(e) => setEmail(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && handleRequestLoginLink()}
                placeholder="you@example.com"
                className={inputBase}
                autoFocus
              />
              <button
                onClick={handleRequestLoginLink}
                disabled={busy}
                className="w-full h-10 rounded-lg bg-primary text-black text-[13px] font-bold disabled:opacity-50 flex items-center justify-center gap-2"
              >
                {busy ? "…" : (<><Wand2 size={13} /> Send sign-in link</>)}
              </button>
              <button
                onClick={() => { setError(null); setInfo(null); setStage("link-verify"); }}
                className="w-full h-9 text-[12px] text-zinc-500 hover:text-primary"
              >
                Already have a sign-in token? Continue →
              </button>
            </div>
          )}

          {/* ----- Magic link: consume ----- */}
          {stage === "link-verify" && (
            <div className="max-w-sm mx-auto py-4 space-y-3">
              <div className="flex items-center gap-2 pb-1">
                <button onClick={goBack} className="text-zinc-400 hover:text-white"><ArrowLeft size={14} /></button>
                <span className="text-[12.5px] font-bold uppercase tracking-wider text-zinc-300">Sign in by email</span>
              </div>
              <p className="text-[12px] text-zinc-300">
                Paste the sign-in token from the email.
              </p>
              <input
                value={loginLinkToken}
                onChange={(e) => setLoginLinkToken(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && handleLoginWithLink()}
                placeholder="sign-in token"
                className={inputBase + " font-mono"}
                autoFocus
              />
              <button
                onClick={handleLoginWithLink}
                disabled={busy}
                className="w-full h-10 rounded-lg bg-primary text-black text-[13px] font-bold disabled:opacity-50"
              >
                {busy ? "…" : "Sign in"}
              </button>
            </div>
          )}

          {/* ----- Sign up: email ----- */}
          {stage === "sign-up-email" && (
            <div className="max-w-sm mx-auto py-4 space-y-3">
              <div className="flex items-center gap-2 pb-1">
                <button onClick={goBack} className="text-zinc-400 hover:text-white"><ArrowLeft size={14} /></button>
                <span className="text-[12.5px] font-bold uppercase tracking-wider text-zinc-300">Create account</span>
              </div>
              <p className="text-[11.5px] text-zinc-500 leading-relaxed">
                We'll email you a verification link. You then set a password from inside the app.
              </p>
              <input
                value={email}
                onChange={(e) => setEmail(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && handleSignupEmail()}
                placeholder="you@example.com"
                className={inputBase}
                autoFocus
              />
              <button
                onClick={handleSignupEmail}
                disabled={busy}
                className="w-full h-10 rounded-lg bg-primary text-black text-[13px] font-bold disabled:opacity-50 flex items-center justify-center gap-2"
              >
                {busy ? "…" : (<><Mail size={13} /> Send verification email</>)}
              </button>
            </div>
          )}

          {/* ----- Sign up: verify ----- */}
          {stage === "sign-up-verify" && (
            <div className="max-w-sm mx-auto py-4 space-y-3">
              <div className="flex items-center gap-2 pb-1">
                <button onClick={goBack} className="text-zinc-400 hover:text-white"><ArrowLeft size={14} /></button>
                <span className="text-[12.5px] font-bold uppercase tracking-wider text-zinc-300">Verify email</span>
              </div>
              <p className="text-[12px] text-zinc-300">
                Paste the token from the email{email ? <> sent to <span className="font-mono text-primary">{email}</span></> : null}.
              </p>
              <input
                value={verifyToken}
                onChange={(e) => setVerifyToken(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && handleVerify()}
                placeholder="verify token"
                className={inputBase + " font-mono"}
                autoFocus
              />
              <button
                onClick={handleVerify}
                disabled={busy}
                className="w-full h-10 rounded-lg bg-primary text-black text-[13px] font-bold disabled:opacity-50"
              >
                {busy ? "…" : "Verify"}
              </button>
            </div>
          )}

          {/* ----- Sign up: set password ----- */}
          {stage === "sign-up-password" && (
            <div className="max-w-sm mx-auto py-4 space-y-3">
              <p className="text-[12px] text-zinc-300 flex items-center gap-1.5">
                <KeyRound size={13} className="text-primary" /> Choose a password for{" "}
                <span className="font-mono text-primary">{email}</span>.
              </p>
              <input
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                placeholder="Password (min 8 chars)"
                className={inputBase}
                autoFocus
              />
              <input
                type="password"
                value={confirmPw}
                onChange={(e) => setConfirmPw(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && handleSetPassword()}
                placeholder="Confirm password"
                className={inputBase}
              />
              <button
                onClick={handleSetPassword}
                disabled={busy}
                className="w-full h-10 rounded-lg bg-primary text-black text-[13px] font-bold disabled:opacity-50"
              >
                {busy ? "…" : "Set password & sign in"}
              </button>
            </div>
          )}

          {/* ----- Signed in (unified sync list) ----- */}
          {stage === "signed-in" && (
            <div className="space-y-3">
              {conflict && (
                <div className="px-3 py-2 bg-amber-500/15 border border-amber-500/30 rounded text-amber-200 text-[12px] space-y-2">
                  <div className="flex items-center gap-2">
                    <AlertTriangle size={13} />
                    <span>
                      Cloud has a newer copy (v{conflict.serverVersion}) of{" "}
                      <span className="font-mono">{conflict.name}</span>. Overwrite anyway?
                    </span>
                  </div>
                  <div className="flex gap-2">
                    <button onClick={doForceUpload} disabled={busy}
                      className="h-7 px-3 rounded text-[11px] font-bold bg-amber-500/80 text-black">
                      Overwrite cloud
                    </button>
                    <button onClick={() => setConflict(null)}
                      className="h-7 px-3 rounded text-[11px] font-bold bg-white/5 text-zinc-200 border border-white/10">
                      Keep cloud copy
                    </button>
                  </div>
                </div>
              )}

              <div className="flex items-center gap-2 flex-wrap">
                <button
                  onClick={doSyncAll}
                  disabled={syncing || busy || needsAction === 0}
                  title={needsAction === 0 ? "Nothing to sync" : "Upload local-only, download remote-only"}
                  className="h-9 px-3 rounded-lg bg-primary text-black text-[12.5px] font-bold disabled:opacity-40 flex items-center gap-2"
                >
                  <RefreshCw size={13} className={syncing ? "animate-spin" : ""} />
                  {syncing ? "Syncing…" : `Sync All${needsAction ? ` (${needsAction})` : ""}`}
                </button>
                <button
                  onClick={refreshOverview}
                  disabled={busy || syncing}
                  title="Refresh status from cloud"
                  className="h-9 px-3 rounded-lg bg-white/5 border border-white/10 text-zinc-200 text-[12.5px] font-bold disabled:opacity-40 flex items-center gap-2"
                >
                  <RefreshCw size={12} /> Refresh
                </button>
                <div className="ml-auto flex items-center gap-3 text-[11px] text-zinc-500">
                  <span title="In sync" className="flex items-center gap-1">
                    <CheckCircle2 size={12} className="text-emerald-400" />{stats.both}
                  </span>
                  <span title="Only local" className="flex items-center gap-1">
                    <Upload size={12} className="text-sky-400" />{stats.local_only}
                  </span>
                  <span title="Only on cloud" className="flex items-center gap-1">
                    <Download size={12} className="text-indigo-400" />{stats.remote_only}
                  </span>
                </div>
              </div>

              <div className="bg-zinc-900/40 border border-white/5 rounded-lg overflow-hidden">
                {sortedEntries.length === 0 ? (
                  <div className="p-8 text-center text-zinc-500 text-[12px]">
                    No profiles anywhere. Create one in the app or sign in elsewhere to see it here.
                  </div>
                ) : (
                  <ul className="divide-y divide-white/5">
                    {sortedEntries.map((entry) => (
                      <SyncRow
                        key={entry.name}
                        entry={entry}
                        busy={busy || syncing}
                        onUpload={() => doUpload(entry.name)}
                        onDownload={() => doDownload(entry)}
                        onDelete={() => doDeleteRemote(entry)}
                      />
                    ))}
                  </ul>
                )}
              </div>

              <div className="pt-2 flex justify-end">
                <button
                  onClick={handleLogout}
                  disabled={busy || syncing}
                  className="h-8 px-3 rounded text-[11.5px] font-bold uppercase tracking-wider bg-white/5 border border-white/10 text-zinc-300 hover:text-rose-300 hover:bg-rose-500/10 flex items-center gap-1.5 disabled:opacity-50"
                >
                  <LogOut size={11} /> Sign out
                </button>
              </div>
            </div>
          )}
        </div>
      </div>
    </div>
  );
};

const SyncRow = ({
  entry, busy, onUpload, onDownload, onDelete,
}: {
  entry: SyncEntry; busy: boolean;
  onUpload: () => void; onDownload: () => void; onDelete: () => void;
}) => {
  const badge = (() => {
    switch (entry.status) {
      case "both":
        return (
          <span className="inline-flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] font-bold uppercase tracking-wider bg-emerald-500/15 text-emerald-300">
            <CheckCircle2 size={10} /> Synced
          </span>
        );
      case "local_only":
        return (
          <span className="inline-flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] font-bold uppercase tracking-wider bg-sky-500/15 text-sky-300">
            <Upload size={10} /> Local
          </span>
        );
      case "remote_only":
        return (
          <span className="inline-flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] font-bold uppercase tracking-wider bg-indigo-500/15 text-indigo-300">
            <Download size={10} /> Cloud
          </span>
        );
    }
  })();

  const meta: string[] = [];
  if (entry.remote) meta.push(`cloud v${entry.remote.version}`);
  if (entry.local_size_bytes != null) meta.push(`local ${fmtBytes(entry.local_size_bytes)}`);
  if (entry.remote) meta.push(`cloud ${fmtBytes(entry.remote.size_bytes)}`);

  return (
    <li className="flex items-center gap-2 px-3 py-2">
      <div className="flex-1 min-w-0">
        <div className="flex items-center gap-2">
          <span className="text-[12.5px] font-mono text-zinc-200 truncate">{entry.name}</span>
          {badge}
        </div>
        <div className="text-[10.5px] text-zinc-500 truncate">
          {meta.join(" · ") || "—"}
        </div>
      </div>

      {(entry.status === "local_only" || entry.status === "both") && (
        <button
          onClick={onUpload}
          disabled={busy}
          title={entry.status === "both" ? "Push local → cloud (new version)" : "Upload to cloud"}
          className="p-1.5 rounded hover:bg-primary/15 text-zinc-400 hover:text-primary disabled:opacity-40"
        >
          <Upload size={13} />
        </button>
      )}

      {(entry.status === "remote_only" || entry.status === "both") && (
        <button
          onClick={onDownload}
          disabled={busy}
          title={entry.status === "both" ? "Pull cloud → local (will rename)" : "Download to local"}
          className="p-1.5 rounded hover:bg-primary/15 text-zinc-400 hover:text-primary disabled:opacity-40"
        >
          <Download size={13} />
        </button>
      )}

      {entry.remote && (
        <button
          onClick={onDelete}
          disabled={busy}
          title="Delete cloud copy"
          className="p-1.5 rounded hover:bg-rose-500/15 text-zinc-400 hover:text-rose-400 disabled:opacity-40"
        >
          <Trash2 size={13} />
        </button>
      )}
    </li>
  );
};

export default CloudPanel;
