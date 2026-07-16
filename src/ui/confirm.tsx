import { createContext, useCallback, useContext, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { AlertTriangle, X } from "lucide-react";

// Focus trap for our modal portals. When `isOpen` flips true, remember the
// element that had focus, then move focus into the panel (unless React's
// autoFocus already did — we check panel.contains(activeElement) first).
// While open, intercept Tab / Shift+Tab so focus wraps inside the panel
// instead of tabbing into the underlying app (which is inert to sighted
// users but reachable via keyboard/AT without this). On close, restore
// focus to the previously-focused element so keyboard users don't lose
// their place. Scoped to this file — no callers touch it.
const useFocusTrap = (isOpen: boolean, panelRef: React.RefObject<HTMLDivElement | null>) => {
  useEffect(() => {
    if (!isOpen) return;
    const previouslyFocused = document.activeElement as HTMLElement | null;
    const panel = panelRef.current;
    if (!panel) return;
    const getFocusable = () =>
      Array.from(
        panel.querySelectorAll<HTMLElement>(
          'button, input, textarea, [tabindex]:not([tabindex="-1"])'
        )
      ).filter((el) => !el.hasAttribute("disabled"));
    // Only override focus if React's autoFocus didn't already land inside.
    if (!panel.contains(document.activeElement)) {
      getFocusable()[0]?.focus();
    }
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key !== "Tab") return;
      const items = getFocusable();
      if (items.length === 0) { e.preventDefault(); return; }
      const first = items[0];
      const last = items[items.length - 1];
      const active = document.activeElement as HTMLElement | null;
      if (e.shiftKey) {
        if (active === first || !panel.contains(active)) {
          e.preventDefault();
          last.focus();
        }
      } else {
        if (active === last || !panel.contains(active)) {
          e.preventDefault();
          first.focus();
        }
      }
    };
    document.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("keydown", onKeyDown);
      if (previouslyFocused && previouslyFocused !== document.body) {
        previouslyFocused.focus?.();
      }
    };
  }, [isOpen, panelRef]);
};

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

// Themed replacement for browser-native `prompt()`. Tauri's Android WebView
// silently no-ops native prompt(), which broke callers like "Generate SSH
// key" that need to collect a short string from the user. Resolves to the
// trimmed input (or empty string when the user hits OK with empty input,
// which validators should reject), or null when cancelled.
export interface TextPromptOptions {
  title?: string;
  message?: string;
  placeholder?: string;
  initialValue?: string;
  okLabel?: string;
  cancelLabel?: string;
  /** Mask the input (passwords / passphrases). */
  password?: boolean;
  /** Return an error string to block OK; return null to allow. */
  validate?: (v: string) => string | null;
}

const ConfirmContext = createContext<((opts: ConfirmOptions | string) => Promise<boolean>) | null>(null);
const OverwriteContext = createContext<((opts: OverwritePromptOptions) => Promise<OverwriteChoice>) | null>(null);
const TextPromptContext = createContext<((opts: TextPromptOptions) => Promise<string | null>) | null>(null);

export const ConfirmProvider = ({ children }: { children: React.ReactNode }) => {
  const [state, setState] = useState<{ opts: ConfirmOptions; resolve: Resolver } | null>(null);
  const [owState, setOwState] = useState<{ opts: OverwritePromptOptions; resolve: (c: OverwriteChoice) => void } | null>(null);
  const [tpState, setTpState] = useState<{ opts: TextPromptOptions; resolve: (v: string | null) => void } | null>(null);
  const [tpValue, setTpValue] = useState("");
  const [tpError, setTpError] = useState<string | null>(null);

  // Refs to each modal's panel — the focus trap needs a DOM anchor to
  // determine "inside vs outside" for the Tab-cycle logic.
  const owPanelRef = useRef<HTMLDivElement | null>(null);
  const confirmPanelRef = useRef<HTMLDivElement | null>(null);
  const tpPanelRef = useRef<HTMLDivElement | null>(null);
  useFocusTrap(owState !== null, owPanelRef);
  useFocusTrap(state !== null, confirmPanelRef);
  useFocusTrap(tpState !== null, tpPanelRef);

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

  const textPrompt = useCallback((opts: TextPromptOptions) => {
    return new Promise<string | null>((resolve) => {
      setTpValue(opts.initialValue ?? "");
      setTpError(null);
      setTpState({ opts, resolve });
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
  const closeTextPrompt = (value: string | null) => {
    tpState?.resolve(value);
    setTpState(null);
  };
  const submitTextPrompt = () => {
    if (!tpState) return;
    const v = tpValue.trim();
    const err = tpState.opts.validate?.(v) ?? null;
    if (err) { setTpError(err); return; }
    closeTextPrompt(v);
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

  // Esc cancels the text prompt. Enter submits (validate → resolve). The
  // input itself handles Enter via onKeyDown so this only catches Esc when
  // the input has focus and when it doesn't.
  useEffect(() => {
    if (!tpState) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") { e.preventDefault(); closeTextPrompt(null); }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tpState]);

  return (
    <ConfirmContext.Provider value={confirm}>
     <OverwriteContext.Provider value={overwritePrompt}>
      <TextPromptContext.Provider value={textPrompt}>
      {children}
      {owState && createPortal(
        <div className="fixed inset-0 z-[9999] bg-black/60 backdrop-blur-sm flex items-center justify-center p-4"
             onClick={() => closeOverwrite("cancel")}>
          <div ref={owPanelRef}
               role="dialog"
               aria-modal="true"
               aria-labelledby="overwrite-modal-title"
               onClick={(e) => e.stopPropagation()}
               className="w-full max-w-[420px] bg-[#121214] border border-white/10 rounded-xl shadow-2xl p-4 font-mono text-[12px] animate-in zoom-in-95 fade-in duration-150">
            <div className="flex items-start justify-between gap-3 mb-3">
              <div className="flex items-center gap-2">
                <AlertTriangle size={14} className="text-amber-400" />
                <span id="overwrite-modal-title" className="text-[11px] font-black uppercase tracking-widest text-zinc-200">
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
          <div ref={confirmPanelRef}
               role="dialog"
               aria-modal="true"
               aria-labelledby="confirm-modal-title"
               onClick={(e) => e.stopPropagation()}
               className="w-full max-w-[380px] bg-[#121214] border border-white/10 rounded-xl shadow-2xl p-4 font-mono text-[12px] animate-in zoom-in-95 fade-in duration-150">
            <div className="flex items-start justify-between gap-3 mb-3">
              <div className="flex items-center gap-2">
                <AlertTriangle size={14} className={state.opts.destructive ? "text-rose-400" : "text-amber-400"} />
                <span id="confirm-modal-title" className="text-[11px] font-black uppercase tracking-widest text-zinc-200">
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
      {tpState && createPortal(
        <div className="fixed inset-0 z-[9999] bg-black/60 backdrop-blur-sm flex items-center justify-center p-4"
             onClick={() => closeTextPrompt(null)}>
          <div ref={tpPanelRef}
               role="dialog"
               aria-modal="true"
               aria-labelledby="text-prompt-modal-title"
               onClick={(e) => e.stopPropagation()}
               className="w-full max-w-[400px] bg-[#121214] border border-white/10 rounded-xl shadow-2xl p-4 font-mono text-[12px] animate-in zoom-in-95 fade-in duration-150">
            <div className="flex items-start justify-between gap-3 mb-3">
              <span id="text-prompt-modal-title" className="text-[11px] font-black uppercase tracking-widest text-zinc-200">
                {tpState.opts.title || "Enter value"}
              </span>
              <button onClick={() => closeTextPrompt(null)} className="text-zinc-500 hover:text-white shrink-0">
                <X size={12} />
              </button>
            </div>
            {tpState.opts.message && (
              <p className="text-[12px] text-zinc-300 leading-relaxed mb-3 whitespace-pre-wrap break-words">
                {tpState.opts.message}
              </p>
            )}
            <input
              autoFocus
              type={tpState.opts.password ? "password" : "text"}
              value={tpValue}
              onChange={(e) => { setTpValue(e.target.value); setTpError(null); }}
              onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); submitTextPrompt(); } }}
              placeholder={tpState.opts.placeholder}
              className="w-full h-10 px-3 mb-2 bg-black/40 border border-white/10 rounded-lg text-[13px] text-zinc-100 outline-none focus:border-primary/50 placeholder:text-zinc-600"
            />
            {tpError && (
              <p className="text-[11px] text-rose-300 mb-2">{tpError}</p>
            )}
            <div className="flex justify-end gap-2 mt-2">
              <button onClick={() => closeTextPrompt(null)}
                      className="px-3 h-8 rounded text-[11px] font-bold uppercase tracking-wider bg-white/[0.04] border border-white/10 text-zinc-300 hover:bg-white/[0.08] hover:text-white">
                {tpState.opts.cancelLabel || "Cancel"}
              </button>
              <button onClick={submitTextPrompt}
                      className="px-3 h-8 rounded text-[11px] font-bold uppercase tracking-wider bg-primary/15 border border-primary/40 text-primary hover:bg-primary/25">
                {tpState.opts.okLabel || "OK"}
              </button>
            </div>
          </div>
        </div>,
        document.body
      )}
      </TextPromptContext.Provider>
     </OverwriteContext.Provider>
    </ConfirmContext.Provider>
  );
};

export const useTextPrompt = () => {
  const ctx = useContext(TextPromptContext);
  if (!ctx) throw new Error("useTextPrompt must be used within <ConfirmProvider>");
  return ctx;
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
