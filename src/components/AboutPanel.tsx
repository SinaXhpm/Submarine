import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { X, Github, Globe, RefreshCw, CheckCircle2, ArrowUpCircle, AlertTriangle } from "lucide-react";
import logoUrl from "../assets/logo.png";

// Tiny modal that surfaces version + update status + a couple of project
// links. Lives behind a small text-button in ProfileSelectPage's footer
// so it's discoverable but doesn't compete for attention with the unlock
// flow. Update-check hits GitHub from Rust — the response and the URL-
// opener both go through tauri commands so we don't need to widen CSP.

interface AppInfo {
  version: string;
  github_repo_url: string;
  github_releases_url: string;
  website_url: string;
}

interface UpdateInfo {
  current: string;
  latest: string | null;
  has_update: boolean;
  release_url: string | null;
}

interface Props {
  isOpen: boolean;
  onClose: () => void;
}

const AboutPanel = ({ isOpen, onClose }: Props) => {
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [update, setUpdate] = useState<UpdateInfo | null>(null);
  const [checking, setChecking] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Load static info once on open. Cheap — synchronous Rust read of
  // the bundled CARGO_PKG_VERSION + the hard-coded URLs.
  useEffect(() => {
    if (!isOpen) return;
    setError(null);
    invoke<AppInfo>("app_info").then(setInfo).catch((e) => setError(String(e)));
  }, [isOpen]);

  const checkUpdates = async () => {
    if (checking) return;
    setChecking(true); setError(null);
    try {
      const u = await invoke<UpdateInfo>("check_for_updates");
      setUpdate(u);
    } catch (e: any) {
      setError(String(e));
    } finally {
      setChecking(false);
    }
  };

  const openUrl = async (url: string) => {
    try {
      await invoke("open_external_url", { url });
    } catch (e: any) {
      setError(String(e));
    }
  };

  if (!isOpen) return null;

  return (
    <div
      className="fixed inset-0 z-50 bg-black/60 backdrop-blur-sm flex items-center justify-center p-3"
      onClick={onClose}
    >
      <div
        className="w-full max-w-sm bg-[#121214] border border-white/10 rounded-xl shadow-2xl"
        onClick={(e) => e.stopPropagation()}
      >
        {/* Header */}
        <div className="px-4 py-3 border-b border-white/5 flex items-center justify-between">
          <span className="text-[12px] font-bold uppercase tracking-widest text-white">About</span>
          <button onClick={onClose} className="text-zinc-400 hover:text-white"><X size={16} /></button>
        </div>

        {/* Body */}
        <div className="p-6 space-y-5">
          {/* Brand */}
          <div className="flex flex-col items-center select-none">
            <img src={logoUrl} alt="" draggable={false} className="h-14 w-auto max-w-full object-contain mb-3 drop-shadow-[0_0_24px_rgba(var(--primary),0.18)]" />
            <h1 className="text-[20px] font-semibold text-white tracking-tight">Submarine</h1>
            <p className="text-[11.5px] text-zinc-500 mt-1 font-mono">
              {info ? `v${info.version}` : "loading…"}
            </p>
          </div>

          {/* Update status */}
          <div className="rounded-lg border border-white/5 bg-white/[0.02] p-3 space-y-2">
            {update == null ? (
              <button
                onClick={checkUpdates}
                disabled={checking}
                className="w-full h-8 rounded text-[12px] font-bold bg-white/[0.04] border border-white/10 hover:bg-white/10 hover:border-white/20 text-zinc-200 disabled:opacity-50 flex items-center justify-center gap-2"
              >
                <RefreshCw size={12} className={checking ? "animate-spin" : ""} />
                {checking ? "Checking…" : "Check for updates"}
              </button>
            ) : update.has_update && update.latest ? (
              <>
                <div className="text-[12px] text-amber-300 flex items-center gap-1.5">
                  <ArrowUpCircle size={13} /> Update available: <span className="font-mono">v{update.latest}</span>
                </div>
                <div className="flex gap-2">
                  <button
                    onClick={() => update.release_url && openUrl(update.release_url)}
                    className="flex-1 h-7 rounded text-[11.5px] font-bold bg-primary text-black"
                  >
                    Open release notes
                  </button>
                  <button
                    onClick={() => setUpdate(null)}
                    className="h-7 px-2 rounded text-[11.5px] text-zinc-400 hover:text-white"
                  >
                    Later
                  </button>
                </div>
              </>
            ) : update.latest == null ? (
              <div className="text-[11.5px] text-zinc-500 flex items-center gap-1.5">
                <AlertTriangle size={12} /> No releases published yet.
              </div>
            ) : (
              <div className="text-[12px] text-emerald-300 flex items-center gap-1.5">
                <CheckCircle2 size={13} /> You're on the latest version.
              </div>
            )}
          </div>

          {/* Links */}
          {info && (
            <div className="space-y-1.5">
              <button
                onClick={() => openUrl(info.github_repo_url)}
                className="w-full h-9 rounded-lg border border-white/5 bg-white/[0.02] hover:bg-white/5 hover:border-white/10 text-[12px] text-zinc-300 hover:text-white flex items-center justify-center gap-2 transition-colors"
              >
                <Github size={13} /> GitHub
              </button>
              <button
                onClick={() => openUrl(info.website_url)}
                className="w-full h-9 rounded-lg border border-white/5 bg-white/[0.02] hover:bg-white/5 hover:border-white/10 text-[12px] text-zinc-300 hover:text-white flex items-center justify-center gap-2 transition-colors"
              >
                <Globe size={13} /> sinaxhpm.com
              </button>
            </div>
          )}

          {error && (
            <div className="px-3 py-2 bg-rose-500/15 border border-rose-500/30 rounded text-rose-200 text-[11.5px] flex items-start gap-2">
              <AlertTriangle size={12} className="mt-0.5 shrink-0" />
              <span className="flex-1 break-words">{error}</span>
            </div>
          )}
        </div>
      </div>
    </div>
  );
};

export default AboutPanel;
