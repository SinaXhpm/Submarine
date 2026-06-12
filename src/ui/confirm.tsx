import { createContext, useCallback, useContext, useEffect, useState } from "react";
import { createPortal } from "react-dom";
import { AlertTriangle, X } from "lucide-react";

// Themed replacement for the browser-native `confirm()` dialog. Anywhere in
// the tree, `useConfirm()` returns a function that opens the modal and
// resolves to true / false. Cross-platform (pure React + CSS), no native
// dialog quirks, and lets us style destructive actions consistently.

export interface ConfirmOptions {
  title?: string;
  message: string;
  okLabel?: string;
  cancelLabel?: string;
  destructive?: boolean;
}

type Resolver = (value: boolean) => void;

// File-conflict modal. Returned from useOverwritePrompt(). The caller picks
// one of five outcomes; in a batch operation, the "all" variants are sticky
// — caller stores them in batch state so subsequent files don't re-prompt.
export type OverwriteChoice =
  | "overwrite"
  | "skip"
  | "overwrite-all"
  | "skip-all"
  | "cancel";

export interface OverwritePromptOptions {
  /** Display name (just the basename, not the full path). */
  name: string;
  /** "download" or "upload" — drives the wording. */
  direction: "download" | "upload";
  /** Total number of items in the batch, so we can hide "all" when only one. */
  batchSize: number;
}

const ConfirmContext = createContext<((opts: ConfirmOptions | string) => Promise<boolean>) | null>(null);
const OverwriteContext = createContext<((opts: OverwritePromptOptions) => Promise<OverwriteChoice>) | null>(null);

export const ConfirmProvider = ({ children }: { children: React.ReactNode }) => {
  const [state, setState] = useState<{ opts: ConfirmOptions; resolve: Resolver } | null>(null);
  const [owState, setOwState] = useState<{ opts: OverwritePromptOptions; resolve: (c: OverwriteChoice) => void } | null>(null);

  const confirm = useCallback((opts: ConfirmOptions | string) => {
    return new Promise<boolean>((resolve) => {
      const normalized = typeof opts === "string" ? { message: opts } : opts;
      setState({ opts: normalized, resolve });
    });
  }, []);

  const overwritePrompt = useCallback((opts: OverwritePromptOptions) => {
    return new Promise<OverwriteChoice>((resolve) => {
      setOwState({ opts, resolve });
    });
  }, []);

  const close = (value: boolean) => {
    state?.resolve(value);
    setState(null);
  };
  const closeOverwrite = (choice: OverwriteChoice) => {
    owState?.resolve(choice);
    setOwState(null);
  };

  // Esc cancels, Enter confirms — matches the native dialog conventions so
  // muscle memory keeps working.
  useEffect(() => {
    if (!state) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") { e.preventDefault(); close(false); }
      if (e.key === "Enter")  { e.preventDefault(); close(true); }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [state]);

  // Same Esc/Enter wiring for the overwrite modal. Esc cancels the whole
  // batch; Enter picks "Overwrite" (the most common "yes I meant it" path).
  useEffect(() => {
    if (!owState) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") { e.preventDefault(); closeOverwrite("cancel"); }
      if (e.key === "Enter")  { e.preventDefault(); closeOverwrite("overwrite"); }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [owState]);

  return (
    <ConfirmContext.Provider value={confirm}>
     <OverwriteContext.Provider value={overwritePrompt}>
      {children}
      {owState && createPortal(
        <div className="fixed inset-0 z-[9999] bg-black/60 backdrop-blur-sm flex items-center justify-center p-4"
             onClick={() => closeOverwrite("cancel")}>
          <div onClick={(e) => e.stopPropagation()}
               className="w-full max-w-[420px] bg-[#121214] border border-white/10 rounded-xl shadow-2xl p-4 font-mono text-[12px] animate-in zoom-in-95 fade-in duration-150">
            <div className="flex items-start justify-between gap-3 mb-3">
              <div className="flex items-center gap-2">
                <AlertTriangle size={14} className="text-amber-400" />
                <span className="text-[11px] font-black uppercase tracking-widest text-zinc-200">
                  {owState.opts.direction === "download" ? "Local file exists" : "Remote file exists"}
                </span>
              </div>
              <button onClick={() => closeOverwrite("cancel")} className="text-zinc-500 hover:text-white shrink-0">
                <X size={12} />
              </button>
            </div>
            <p className="text-[12px] text-zinc-300 leading-relaxed mb-1 whitespace-pre-wrap break-words">
              {owState.opts.direction === "download"
                ? "A file with this name already exists at the destination:"
                : "A file with this name already exists on the remote:"}
            </p>
            <p className="text-[12px] text-amber-300 leading-relaxed mb-4 break-all">
              {owState.opts.name}
            </p>
            <div className="grid grid-cols-2 gap-2">
              <button onClick={() => closeOverwrite("overwrite")} autoFocus
                      className="h-9 rounded text-[11px] font-bold uppercase tracking-wider bg-primary/15 border border-primary/40 text-primary hover:bg-primary/25">
                Overwrite
              </button>
              <button onClick={() => closeOverwrite("skip")}
                      className="h-9 rounded text-[11px] font-bold uppercase tracking-wider bg-white/[0.04] border border-white/10 text-zinc-300 hover:bg-white/[0.08] hover:text-white">
                Skip
              </button>
              {owState.opts.batchSize > 1 && (
                <>
                  <button onClick={() => closeOverwrite("overwrite-all")}
                          className="h-9 rounded text-[11px] font-bold uppercase tracking-wider bg-primary/10 border border-primary/30 text-primary hover:bg-primary/20">
                    Overwrite all
                  </button>
                  <button onClick={() => closeOverwrite("skip-all")}
                          className="h-9 rounded text-[11px] font-bold uppercase tracking-wider bg-white/[0.03] border border-white/10 text-zinc-400 hover:bg-white/[0.06] hover:text-zinc-200">
                    Skip all
                  </button>
                </>
              )}
            </div>
            <div className="mt-3 flex justify-end">
              <button onClick={() => closeOverwrite("cancel")}
                      className="px-3 h-7 rounded text-[10px] font-bold uppercase tracking-wider bg-rose-500/10 border border-rose-500/30 text-rose-300 hover:bg-rose-500/20">
                Cancel batch
              </button>
            </div>
          </div>
        </div>,
        document.body
      )}
      {state && createPortal(
        <div className="fixed inset-0 z-[9999] bg-black/60 backdrop-blur-sm flex items-center justify-center p-4"
             onClick={() => close(false)}>
          <div onClick={(e) => e.stopPropagation()}
               className="w-full max-w-[380px] bg-[#121214] border border-white/10 rounded-xl shadow-2xl p-4 font-mono text-[12px] animate-in zoom-in-95 fade-in duration-150">
            <div className="flex items-start justify-between gap-3 mb-3">
              <div className="flex items-center gap-2">
                <AlertTriangle size={14} className={state.opts.destructive ? "text-rose-400" : "text-amber-400"} />
                <span className="text-[11px] font-black uppercase tracking-widest text-zinc-200">
                  {state.opts.title || (state.opts.destructive ? "Confirm action" : "Are you sure?")}
                </span>
              </div>
              <button onClick={() => close(false)} className="text-zinc-500 hover:text-white shrink-0">
                <X size={12} />
              </button>
            </div>
            <p className="text-[12px] text-zinc-300 leading-relaxed mb-4 whitespace-pre-wrap break-words">
              {state.opts.message}
            </p>
            <div className="flex justify-end gap-2">
              <button onClick={() => close(false)}
                      className="px-3 h-8 rounded text-[11px] font-bold uppercase tracking-wider bg-white/[0.04] border border-white/10 text-zinc-300 hover:bg-white/[0.08] hover:text-white">
                {state.opts.cancelLabel || "Cancel"}
              </button>
              <button onClick={() => close(true)} autoFocus
                      className={`px-3 h-8 rounded text-[11px] font-bold uppercase tracking-wider border ${
                        state.opts.destructive
                          ? "bg-rose-500/15 border-rose-500/40 text-rose-300 hover:bg-rose-500/25"
                          : "bg-primary/15 border-primary/40 text-primary hover:bg-primary/25"
                      }`}>
                {state.opts.okLabel || (state.opts.destructive ? "Delete" : "OK")}
              </button>
            </div>
          </div>
        </div>,
        document.body
      )}
     </OverwriteContext.Provider>
    </ConfirmContext.Provider>
  );
};

export const useConfirm = () => {
  const ctx = useContext(ConfirmContext);
  if (!ctx) throw new Error("useConfirm must be used within <ConfirmProvider>");
  return ctx;
};

export const useOverwritePrompt = () => {
  const ctx = useContext(OverwriteContext);
  if (!ctx) throw new Error("useOverwritePrompt must be used within <ConfirmProvider>");
  return ctx;
};
