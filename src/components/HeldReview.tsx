import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { ShieldAlert, X } from "lucide-react";
import { useFocusTrap } from "../ui/confirm";

// What this device holds back in a shared profile until its user allows it:
// a change someone else made that would act on its own here (see
// src-tauri/src/approvals.rs). Opened from the notice above the server list.

type HeldServer = {
  id: number;
  name: string;
  run_on_connect?: string;
  edited_by?: string | null;
  held?: string[];
};

/** Commands first (they run in a shell), then autostart, forwards, monitor commands. */
const rank = (item: string) =>
  item === "run_on_connect" ? 0 : item === "autostart" ? 1 : item.startsWith("tunnel:") ? 2 : 3;
const byRank = (a: string, b: string) => rank(a) - rank(b) || a.localeCompare(b);

/** One held item, in words, with the text it would run when there is one. */
function describe(item: string, server: HeldServer): { what: string; detail?: string } {
  if (item === "run_on_connect") {
    return { what: "Types these commands into your terminal when you connect", detail: server.run_on_connect || "" };
  }
  if (item === "autostart") {
    return { what: "Connects by itself when you unlock this profile" };
  }
  if (item.startsWith("tunnel:")) {
    const [kind, local, remote] = item.slice("tunnel:".length).split("|");
    if (kind === "D") return { what: `SOCKS proxy on ${local}`, detail: "Programs set to use it reach the network through the server." };
    if (kind === "L") return { what: `Local forward ${local} → ${remote}`, detail: "Connections to that address on this computer go through the server to the target." };
    if (kind === "R") return { what: `Remote forward: the server listens on ${local} and sends connections to ${remote}`, detail: "Whoever reaches that port on the server reaches this side." };
    return { what: `Forward ${item.slice("tunnel:".length)}` };
  }
  if (item.startsWith("metric:")) {
    return { what: "Monitor command, run on the server at every check", detail: item.slice("metric:".length) };
  }
  return { what: item };
}

export function HeldReview({ servers, onAllow, onClose }: {
  servers: HeldServer[];
  onAllow: (serverId: number, items: string[]) => Promise<void>;
  onClose: () => void;
}) {
  const panelRef = useRef<HTMLDivElement | null>(null);
  useFocusTrap(1, panelRef);
  const [busy, setBusy] = useState(false);
  const held = servers.filter((s) => (s.held || []).length > 0);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") { e.preventDefault(); onClose(); }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  // Everything allowed: nothing left to review.
  useEffect(() => {
    if (held.length === 0) onClose();
  }, [held.length, onClose]);

  const allow = async (serverId: number, items: string[]) => {
    setBusy(true);
    try { await onAllow(serverId, items); } finally { setBusy(false); }
  };

  return createPortal(
    <div className="fixed inset-0 z-[9998] bg-black/60 backdrop-blur-sm flex items-center justify-center p-4" onClick={onClose}>
      <div
        ref={panelRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby="held-review-title"
        onClick={(e) => e.stopPropagation()}
        className="w-full max-w-[560px] max-h-[80vh] flex flex-col bg-[#121214] border border-white/10 rounded-xl shadow-2xl font-mono text-[12px] animate-in zoom-in-95 fade-in duration-150"
      >
        <div className="flex items-start justify-between gap-3 p-4 pb-2">
          <div className="flex items-center gap-2">
            <ShieldAlert size={14} className="text-amber-400" />
            <span id="held-review-title" className="text-[11px] font-black uppercase tracking-widest text-zinc-200">
              Waiting for your OK
            </span>
          </div>
          <button aria-label="Close" onClick={onClose} className="text-zinc-500 hover:text-white shrink-0">
            <X size={12} />
          </button>
        </div>
        <p className="px-4 pb-3 text-[12px] text-zinc-400 leading-relaxed">
          Someone you share this profile with made these changes. Until you allow one, it doesn't run on this
          device: commands aren't typed, forwards stay stopped, autostart doesn't connect and monitor commands
          don't run. Allowing counts for this device only.
        </p>
        <div className="flex-1 overflow-y-auto custom-scrollbar px-4 pb-2 space-y-3">
          {held.map((s) => (
            <section key={s.id} aria-label={s.name} className="rounded-lg border border-white/10 bg-white/[0.02] p-3">
              <div className="flex items-center justify-between gap-2 mb-2">
                <div className="min-w-0">
                  <div className="text-[12.5px] font-bold text-zinc-100 truncate">{s.name}</div>
                  {s.edited_by && <div className="text-[10.5px] text-zinc-500 truncate">last changed by {s.edited_by}</div>}
                </div>
                {(s.held || []).length > 1 && (
                  <button
                    disabled={busy}
                    onClick={() => allow(s.id, [])}
                    className="shrink-0 px-2.5 h-7 rounded text-[10.5px] font-bold uppercase tracking-wider bg-white/[0.04] border border-white/10 text-zinc-300 hover:bg-white/[0.08] hover:text-white disabled:opacity-50"
                  >
                    Allow all
                  </button>
                )}
              </div>
              <ul className="space-y-2">
                {[...(s.held || [])].sort(byRank).map((item) => {
                  const d = describe(item, s);
                  return (
                    <li key={item} className="flex items-start justify-between gap-3">
                      <div className="min-w-0 flex-1">
                        <div className="text-[12px] text-zinc-200 break-words">{d.what}</div>
                        {d.detail && (
                          item === "run_on_connect" || item.startsWith("metric:") ? (
                            <pre className="mt-1 max-h-32 overflow-auto custom-scrollbar rounded bg-black/40 border border-white/5 px-2 py-1 text-[11px] text-amber-200/90 whitespace-pre-wrap break-all">{d.detail}</pre>
                          ) : (
                            <div className="mt-0.5 text-[11px] text-zinc-500">{d.detail}</div>
                          )
                        )}
                      </div>
                      <button
                        disabled={busy}
                        onClick={() => allow(s.id, [item])}
                        aria-label={`Allow: ${d.what}`}
                        className="shrink-0 px-2.5 h-7 rounded text-[10.5px] font-bold uppercase tracking-wider bg-primary/15 border border-primary/40 text-primary hover:bg-primary/25 disabled:opacity-50"
                      >
                        Allow
                      </button>
                    </li>
                  );
                })}
              </ul>
            </section>
          ))}
        </div>
        <div className="flex justify-end p-4 pt-2">
          <button
            onClick={onClose}
            data-default-focus
            className="px-3 h-8 rounded text-[11px] font-bold uppercase tracking-wider bg-white/[0.04] border border-white/10 text-zinc-300 hover:bg-white/[0.08] hover:text-white"
          >
            Close
          </button>
        </div>
      </div>
    </div>,
    document.body
  );
}
