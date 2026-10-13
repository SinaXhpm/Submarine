import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Trash2, X } from "lucide-react";
import { useFocusTrap } from "../ui/confirm";

// Deletions a shared profile's sync would have made here, held until this
// device's user decides (src-tauri/src/approvals.rs, hold_deletions): one
// sync deleting much of the profile at once, or an SSH key. Opened from the
// notice above the server list.

export type HeldDeletion = { uuid: string; entity_type: string; name: string };

const KINDS: [string, string][] = [
  ["servers", "Servers"],
  ["folders", "Folders"],
  ["credentials", "Logins"],
  ["ssh_keys", "SSH keys"],
  ["commands", "Commands"],
  ["notes", "Notes"],
  ["monitor_configs", "Monitoring settings"],
];

export function HeldDeletions({ items, onResolve, onClose }: {
  items: HeldDeletion[];
  onResolve: (deleteThem: boolean) => Promise<void>;
  onClose: () => void;
}) {
  const panelRef = useRef<HTMLDivElement | null>(null);
  useFocusTrap(1, panelRef);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") { e.preventDefault(); onClose(); }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  // Decided: nothing left to review.
  useEffect(() => {
    if (items.length === 0) onClose();
  }, [items.length, onClose]);

  const decide = async (deleteThem: boolean) => {
    setBusy(true);
    try { await onResolve(deleteThem); } finally { setBusy(false); }
  };
  const groups = KINDS.map(([type, label]) => [label, items.filter((i) => i.entity_type === type)] as const)
    .filter(([, list]) => list.length > 0);

  return createPortal(
    <div className="fixed inset-0 z-[9998] bg-black/60 backdrop-blur-sm flex items-center justify-center p-4" onClick={onClose}>
      <div
        ref={panelRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby="held-deletions-title"
        onClick={(e) => e.stopPropagation()}
        className="w-full max-w-[520px] max-h-[80vh] flex flex-col bg-[#121214] border border-white/10 rounded-xl shadow-2xl font-mono text-[12px] animate-in zoom-in-95 fade-in duration-150"
      >
        <div className="flex items-start justify-between gap-3 p-4 pb-2">
          <div className="flex items-center gap-2">
            <Trash2 size={14} className="text-amber-400" />
            <span id="held-deletions-title" className="text-[11px] font-black uppercase tracking-widest text-zinc-200">
              Deleted by someone else
            </span>
          </div>
          <button aria-label="Close" onClick={onClose} className="text-zinc-500 hover:text-white shrink-0">
            <X size={12} />
          </button>
        </div>
        <p className="px-4 pb-3 text-[12px] text-zinc-400 leading-relaxed">
          Someone you share this profile with deleted these in one go. They're still here: delete them here too,
          or keep them. Kept items come back for everyone, or, if you can only view this profile, stay on this
          device.
        </p>
        <div className="flex-1 overflow-y-auto custom-scrollbar px-4 pb-2 space-y-3">
          {groups.map(([label, list]) => (
            <section key={label} aria-label={label} className="rounded-lg border border-white/10 bg-white/[0.02] p-3">
              <div className="text-[10.5px] font-bold uppercase tracking-wider text-zinc-500 mb-1.5">{label}</div>
              <ul className="space-y-1">
                {list.map((i) => (
                  <li key={i.uuid} className="text-[12px] text-zinc-200 break-words">{i.name || "(no name)"}</li>
                ))}
              </ul>
            </section>
          ))}
        </div>
        <div className="flex justify-end gap-2 p-4 pt-2">
          <button
            disabled={busy}
            onClick={() => decide(false)}
            className="px-3 h-8 rounded text-[11px] font-bold uppercase tracking-wider bg-primary/15 border border-primary/40 text-primary hover:bg-primary/25 disabled:opacity-50"
          >
            Keep them
          </button>
          <button
            disabled={busy}
            onClick={() => decide(true)}
            className="px-3 h-8 rounded text-[11px] font-bold uppercase tracking-wider bg-rose-500/10 border border-rose-500/40 text-rose-300 hover:bg-rose-500/20 disabled:opacity-50"
          >
            Delete here too
          </button>
        </div>
      </div>
    </div>,
    document.body,
  );
}
