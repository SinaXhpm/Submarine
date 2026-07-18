import { useEffect, useState, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  X, AlertTriangle, CheckCircle2, Cloud, ArrowRight, ArrowLeft,
  LogOut, Mail, KeyRound, UserPlus, LogIn, Wand2,
} from "lucide-react";
import InvitesSection from "./InvitesSection";

// Cloud account panel, opened from the profile picker. Owns its own auth state;
// the parent just mounts/unmounts. Signed-out is split into intent-first flows
// (chooser → sign-in / sign-up / forgot / magic-link). Signed-in is now just
// the account identity plus the two things that legitimately live where
// profiles are born: incoming shares to accept, and restoring one of your own
// cloud profiles onto this device. Per-profile sync moved into the app's
// Profile panel; the old whole-vault blob sync is gone entirely.

interface CloudStatus { signed_in: boolean; email: string | null; }

interface Props {
  isOpen: boolean;
  onClose: () => void;
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

const CloudPanel = ({ isOpen, onClose, onLocalProfilesChanged }: Props) => {
  const [stage, setStage] = useState<Stage>("chooser");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [confirmPw, setConfirmPw] = useState("");
  const [verifyToken, setVerifyToken] = useState("");
  const [claimToken, setClaimToken] = useState<string | null>(null);
  const [resetToken, setResetToken] = useState("");
  const [loginLinkToken, setLoginLinkToken] = useState("");
  const [signedInEmail, setSignedInEmail] = useState<string | null>(null);

  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [info, setInfo] = useState<string | null>(null);

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

  useEffect(() => { if (isOpen) refreshStatus(); }, [isOpen, refreshStatus]);

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

  // ---- Render ----------------------------------------------------------

  if (!isOpen) return null;

  const inputBase =
    "w-full h-10 px-3 bg-zinc-900/60 border border-white/10 rounded-lg text-[13px] text-zinc-50 placeholder:text-zinc-600 outline-none focus:border-primary/50 transition-colors";

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

          {/* ----- Signed in (account + shares + restore) ----- */}
          {stage === "signed-in" && (
            <div className="space-y-3">
              <div className="bg-zinc-900/40 border border-white/5 rounded-lg p-3 flex items-center gap-2">
                <Cloud size={14} className="text-primary shrink-0" />
                <span className="text-[12.5px] text-zinc-200 font-mono truncate flex-1">{signedInEmail}</span>
              </div>
              <p className="text-[11.5px] text-zinc-500 leading-relaxed">
                Your profiles sync automatically once open — manage sync and sharing for a profile
                from its <span className="text-zinc-300 font-semibold">Profile</span> panel inside the app.
              </p>

              <InvitesSection onLocalProfilesChanged={onLocalProfilesChanged} />

              <div className="pt-2 flex justify-end">
                <button
                  onClick={handleLogout}
                  disabled={busy}
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

export default CloudPanel;
