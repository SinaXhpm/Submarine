import { createContext, useCallback, useContext, useEffect, useLayoutEffect, useReducer, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { AlertTriangle, X } from "lucide-react";
import { useT } from "../i18n";

// Focus trap for our modal portals. When a prompt opens (`openId` is its id,
// null while that kind of prompt is closed), remember the element that had
// focus, then move focus into the panel, onto its `data-default-focus`
// control (else the first focusable one). It runs as a layout effect, in the
// same commit that shows the panel, so no keystroke lands behind it. The
// panels must not use `autoFocus`: React applies it earlier in that commit,
// so the trap would remember the panel's own button instead of the opener
// and focus would fall to the page when the prompt closes.
// While open, intercept Tab / Shift+Tab so focus wraps inside the panel
// instead of tabbing into the underlying app (which is inert to sighted
// users but reachable via keyboard/AT without this). On close, restore
// focus to the previously-focused element so keyboard users don't lose
// their place. The held-changes review dialog uses it too.
//
// Keyed on the prompt's id, not on "is one open": when one prompt follows
// another of the same kind the panel is a new element, and a trap still
// holding the old one would swallow every Tab.
export const useFocusTrap = (openId: number | null, panelRef: React.RefObject<HTMLDivElement | null>) => {
  useLayoutEffect(() => {
    if (openId === null) return;
    const previouslyFocused = document.activeElement as HTMLElement | null;
    const panel = panelRef.current;
    if (!panel) return;
    const getFocusable = () =>
      Array.from(
        panel.querySelectorAll<HTMLElement>(
          'button, input, textarea, [tabindex]:not([tabindex="-1"])'
        )
      ).filter((el) => !el.hasAttribute("disabled"));
    // For a prompt that follows another of the same kind, the previous
    // trap's cleanup has just handed focus back to the opener, which this
    // trap then remembers in turn.
    if (!panel.contains(document.activeElement)) {
      (panel.querySelector<HTMLElement>("[data-default-focus]") ?? getFocusable()[0])?.focus();
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
  }, [openId, panelRef]);
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
  /** Focus starts on Cancel: an Enter meant for something else (the
   *  terminal the prompt appeared over) can't answer yes. */
  safeDefault?: boolean;
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
  /** Return the input untrimmed (secrets where spaces are significant). */
  keepWhitespace?: boolean;
  /** Return an error string to block OK; return null to allow. */
  validate?: (v: string) => string | null;
}

// A form being closed with changes in it. Returned from useUnsavedPrompt():
// save them, throw them away, or go back to the form ("cancel": Escape, a
// click beside the dialog, its X).
export type UnsavedChoice = "save" | "discard" | "cancel";

export interface UnsavedPromptOptions {
  title?: string;
  message: string;
  saveLabel?: string;
  discardLabel?: string;
  cancelLabel?: string;
}

const ConfirmContext = createContext<((opts: ConfirmOptions | string) => Promise<boolean>) | null>(null);
const OverwriteContext = createContext<((opts: OverwritePromptOptions) => Promise<OverwriteChoice>) | null>(null);
const TextPromptContext = createContext<((opts: TextPromptOptions) => Promise<string | null>) | null>(null);
const UnsavedContext = createContext<((opts: UnsavedPromptOptions) => Promise<UnsavedChoice>) | null>(null);

// A request for a prompt, waiting for its turn or on screen.
type Pending =
  | { id: number; kind: "confirm"; opts: ConfirmOptions; resolve: Resolver }
  | { id: number; kind: "overwrite"; opts: OverwritePromptOptions; resolve: (c: OverwriteChoice) => void }
  | { id: number; kind: "text"; opts: TextPromptOptions; resolve: (v: string | null) => void }
  | { id: number; kind: "unsaved"; opts: UnsavedPromptOptions; resolve: (c: UnsavedChoice) => void };

// How long a prompt that took another one's place ignores clicks and keys.
// The second click of a double-click, or a second quick Enter, was aimed at
// the prompt before it and must not answer this one too.
const CHAIN_GRACE_MS = 500;

// How long the unsaved-changes question ignores the pointer once it is up. It
// comes up under the pointer of the click that asked for it (a click beside a
// form), so the second click of a double-click, or a second quick tap, lands
// on it, maybe on Discard. The keyboard answers at once.
const UNSAVED_POINTER_GRACE_MS = 300;

const sameConfirm = (a: ConfirmOptions, b: ConfirmOptions) =>
  a.title === b.title && a.message === b.message && a.okLabel === b.okLabel &&
  a.cancelLabel === b.cancelLabel && !!a.destructive === !!b.destructive;

// A dialog comes up under the pointer of the click that asked for it, so the
// second click of a double-click lands on the dialog: beside it, or on one of
// its buttons. With a server's bin where the dialog's Delete comes up, a
// double-click deleted the server before the question was seen. The later
// clicks of a multi-click (`detail` counts them) answer nothing: they stop
// here, on the way down to the dialog's own handlers. A click made by the
// keyboard or by a script has no count.
const ignoreLaterClicks = (e: React.MouseEvent) => {
  if (e.detail > 1) e.stopPropagation();
};

export const ConfirmProvider = ({ children }: { children: React.ReactNode }) => {
  const t = useT();
  // Every request, of any kind, waits in one line, and only the first in line
  // is on screen. There used to be one slot per kind, which went wrong two
  // ways:
  //  - A second request of the same kind replaced the first, whose promise
  //    then never settled. Two SFTP transfers that each reached an existing
  //    file left the first batch waiting for good.
  //  - Two kinds could be open at once, and one Enter answered both, the one
  //    hidden underneath included.
  //
  // The line is a ref so a request can be checked against it and added in one
  // synchronous step; `repaint` re-renders after each change.
  const queueRef = useRef<Pending[]>([]);
  const nextIdRef = useRef(1);
  // When the prompt on screen took the place of one just answered
  // (-Infinity: it opened on its own). See CHAIN_GRACE_MS.
  const chainedAtRef = useRef(-Infinity);
  const [, repaint] = useReducer((n: number) => n + 1, 0);
  const [tpValue, setTpValue] = useState("");
  const [tpError, setTpError] = useState<string | null>(null);

  const active = queueRef.current[0] ?? null;
  const state = active?.kind === "confirm" ? active : null;
  const owState = active?.kind === "overwrite" ? active : null;
  const tpState = active?.kind === "text" ? active : null;
  const usState = active?.kind === "unsaved" ? active : null;

  // Refs to each modal's panel — the focus trap needs a DOM anchor to
  // determine "inside vs outside" for the Tab-cycle logic.
  const owPanelRef = useRef<HTMLDivElement | null>(null);
  const confirmPanelRef = useRef<HTMLDivElement | null>(null);
  const confirmOkRef = useRef<HTMLButtonElement | null>(null);
  const tpPanelRef = useRef<HTMLDivElement | null>(null);
  const usPanelRef = useRef<HTMLDivElement | null>(null);
  useFocusTrap(owState?.id ?? null, owPanelRef);
  useFocusTrap(state?.id ?? null, confirmPanelRef);
  useFocusTrap(tpState?.id ?? null, tpPanelRef);
  useFocusTrap(usState?.id ?? null, usPanelRef);

  // A text prompt starts from its own initial value, never from what was
  // typed into the one before it (which may have been a password).
  const primeText = useCallback((p: Pending | undefined) => {
    setTpValue(p?.kind === "text" ? p.opts.initialValue ?? "" : "");
    setTpError(null);
  }, []);

  const enqueue = useCallback((p: Pending) => {
    queueRef.current = [...queueRef.current, p];
    if (queueRef.current.length === 1) primeText(p);
    repaint();
  }, [primeText]);

  const confirm = useCallback((opts: ConfirmOptions | string) => {
    return new Promise<boolean>((resolve) => {
      const normalized = typeof opts === "string" ? { message: opts } : opts;
      // The same question already in line (the window asked to close again
      // while "Close Submarine?" is up) is not asked twice: the repeat is
      // answered "no" and the one already waiting stands.
      if (queueRef.current.some((p) => p.kind === "confirm" && sameConfirm(p.opts, normalized))) {
        resolve(false);
        return;
      }
      enqueue({ id: nextIdRef.current++, kind: "confirm", opts: normalized, resolve });
    });
  }, [enqueue]);

  const overwritePrompt = useCallback((opts: OverwritePromptOptions) => {
    return new Promise<OverwriteChoice>((resolve) => {
      enqueue({ id: nextIdRef.current++, kind: "overwrite", opts, resolve });
    });
  }, [enqueue]);

  const textPrompt = useCallback((opts: TextPromptOptions) => {
    return new Promise<string | null>((resolve) => {
      enqueue({ id: nextIdRef.current++, kind: "text", opts, resolve });
    });
  }, [enqueue]);

  const unsavedPrompt = useCallback((opts: UnsavedPromptOptions) => {
    return new Promise<UnsavedChoice>((resolve) => {
      enqueue({ id: nextIdRef.current++, kind: "unsaved", opts, resolve });
    });
  }, [enqueue]);

  // Take the prompt on screen out of the line and bring up the next one.
  // False when `p` is not the one on screen any more (already answered), or
  // only just took another prompt's place (see CHAIN_GRACE_MS): the caller
  // must then leave it unanswered.
  const advance = (p: Pending) => {
    if (queueRef.current[0] !== p) return false;
    if (performance.now() - chainedAtRef.current < CHAIN_GRACE_MS) return false;
    queueRef.current = queueRef.current.slice(1);
    chainedAtRef.current = queueRef.current.length > 0 ? performance.now() : -Infinity;
    primeText(queueRef.current[0]);
    repaint();
    return true;
  };

  const close = (value: boolean) => {
    if (state && advance(state)) state.resolve(value);
  };
  const closeOverwrite = (choice: OverwriteChoice) => {
    if (owState && advance(owState)) owState.resolve(choice);
  };
  const closeTextPrompt = (value: string | null) => {
    if (tpState && advance(tpState)) tpState.resolve(value);
  };
  const closeUnsaved = (choice: UnsavedChoice) => {
    if (usState && advance(usState)) usState.resolve(choice);
  };
  // An answer by the pointer, unless the click came with the one that opened
  // the question (see UNSAVED_POINTER_GRACE_MS): a later click of a
  // double-click (`detail` counts them), or one just after the question came
  // up, where a touch screen counts no clicks. A click made by the keyboard
  // or by a script has no count and answers at once.
  const usShownAtRef = useRef(0);
  useLayoutEffect(() => {
    if (usState) usShownAtRef.current = performance.now();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [usState?.id]);
  const clickUnsaved = (e: React.MouseEvent, choice: UnsavedChoice) => {
    if (e.detail > 1) return;
    if (e.detail === 1 && performance.now() - usShownAtRef.current < UNSAVED_POINTER_GRACE_MS) return;
    closeUnsaved(choice);
  };
  const submitTextPrompt = () => {
    if (!tpState) return;
    // `keepWhitespace` returns the input exactly as typed (e.g. a sudo
    // password, where leading/trailing spaces are part of the secret).
    const v = tpState.opts.keepWhitespace ? tpValue : tpValue.trim();
    const err = tpState.opts.validate?.(v) ?? null;
    if (err) { setTpError(err); return; }
    closeTextPrompt(v);
  };

  // Esc cancels, Enter confirms — matches the native dialog conventions so
  // muscle memory keeps working. A key still held down from before (auto
  // repeat) answers nothing: it was not pressed for this prompt. In a prompt
  // whose default is Cancel (`safeDefault`), Enter does what the focused
  // button does, so an Enter alone never says yes.
  useEffect(() => {
    if (!state) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") { e.preventDefault(); if (!e.repeat) close(false); }
      if (e.key === "Enter")  {
        e.preventDefault();
        if (!e.repeat) close(state.opts.safeDefault ? document.activeElement === confirmOkRef.current : true);
      }
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
      if (e.key === "Escape") { e.preventDefault(); if (!e.repeat) closeOverwrite("cancel"); }
      if (e.key === "Enter")  { e.preventDefault(); if (!e.repeat) closeOverwrite("overwrite"); }
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
      if (e.key === "Escape") { e.preventDefault(); if (!e.repeat) closeTextPrompt(null); }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tpState]);

  // Esc goes back to the form. Enter does what the focused button does, and
  // Save has the focus at first: an Enter alone never throws changes away.
  useEffect(() => {
    if (!usState) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") { e.preventDefault(); if (!e.repeat) closeUnsaved("cancel"); }
      if (e.key === "Enter") {
        e.preventDefault();
        if (e.repeat) return;
        const focused = document.activeElement;
        const choice = focused instanceof HTMLElement && usPanelRef.current?.contains(focused) ? focused.dataset.choice : undefined;
        closeUnsaved(choice === "discard" || choice === "cancel" ? choice : "save");
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [usState]);

  return (
    <ConfirmContext.Provider value={confirm}>
     <OverwriteContext.Provider value={overwritePrompt}>
      <TextPromptContext.Provider value={textPrompt}>
      <UnsavedContext.Provider value={unsavedPrompt}>
      {children}
      {/* Each dialog is keyed by its prompt, so one that follows another of
          the same kind is a new dialog (it animates in and its default
          control takes focus) and not the old one with the text swapped. */}
      {owState && createPortal(
        <div key={owState.id}
             className="fixed inset-0 z-[9999] bg-black/60 backdrop-blur-sm flex items-center justify-center p-4"
             onClickCapture={ignoreLaterClicks}
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
                <span id="overwrite-modal-title" className="text-[12px] font-semibold text-zinc-200">
                  {owState.opts.direction === "download" ? t("Local file exists") : t("Remote file exists")}
                </span>
              </div>
              <button aria-label={t("Close")}onClick={() => closeOverwrite("cancel")} className="p-[14px] -m-[14px] sm:p-0 sm:m-0 text-zinc-500 hover:text-white shrink-0">
                <X size={12} />
              </button>
            </div>
            <p className="text-[12px] text-zinc-300 leading-relaxed mb-1 whitespace-pre-wrap break-words">
              {owState.opts.direction === "download"
                ? t("A file with this name already exists at the destination:")
                : t("A file with this name already exists on the remote:")}
            </p>
            <p className="text-[12px] text-amber-300 leading-relaxed mb-4 break-all">
              {owState.opts.name}
            </p>
            <div className="grid grid-cols-2 gap-2">
              <button onClick={() => closeOverwrite("overwrite")} data-default-focus
                      className="h-10 sm:h-9 rounded text-[12px] font-semibold bg-primary/15 border border-primary/40 text-primary hover:bg-primary/25">
                {t("Overwrite")}
              </button>
              <button onClick={() => closeOverwrite("skip")}
                      className="h-10 sm:h-9 rounded text-[12px] font-semibold bg-white/[0.04] border border-white/10 text-zinc-300 hover:bg-white/[0.08] hover:text-white">
                {t("Skip")}
              </button>
              {owState.opts.batchSize > 1 && (
                <>
                  <button onClick={() => closeOverwrite("overwrite-all")}
                          className="h-10 sm:h-9 rounded text-[12px] font-semibold bg-primary/10 border border-primary/30 text-primary hover:bg-primary/20">
                    {t("Overwrite all")}
                  </button>
                  <button onClick={() => closeOverwrite("skip-all")}
                          className="h-10 sm:h-9 rounded text-[12px] font-semibold bg-white/[0.03] border border-white/10 text-zinc-400 hover:bg-white/[0.06] hover:text-zinc-200">
                    {t("Skip all")}
                  </button>
                </>
              )}
            </div>
            <div className="mt-3 flex justify-end">
              <button onClick={() => closeOverwrite("cancel")}
                      className="px-3 h-7 rounded text-[11px] font-semibold bg-rose-500/10 border border-rose-500/30 text-rose-300 hover:bg-rose-500/20">
                {t("Cancel batch")}
              </button>
            </div>
          </div>
        </div>,
        document.body
      )}
      {state && createPortal(
        <div key={state.id}
             className="fixed inset-0 z-[9999] bg-black/60 backdrop-blur-sm flex items-center justify-center p-4"
             onClickCapture={ignoreLaterClicks}
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
                <span id="confirm-modal-title" className="text-[12px] font-semibold text-zinc-200">
                  {state.opts.title || (state.opts.destructive ? t("Confirm action") : t("Are you sure?"))}
                </span>
              </div>
              <button aria-label={t("Close")}onClick={() => close(false)} className="p-[14px] -m-[14px] sm:p-0 sm:m-0 text-zinc-500 hover:text-white shrink-0">
                <X size={12} />
              </button>
            </div>
            {/* Scrolls: a Library command is shown whole, however long it is,
                and the buttons have to stay on screen. */}
            <p className="text-[12px] text-zinc-300 leading-relaxed mb-4 whitespace-pre-wrap break-words max-h-[60vh] overflow-y-auto custom-scrollbar">
              {state.opts.message}
            </p>
            <div className="flex justify-end gap-2">
              <button onClick={() => close(false)}
                      data-default-focus={state.opts.safeDefault ? true : undefined}
                      className="px-3 h-10 sm:h-8 rounded text-[12px] font-semibold bg-white/[0.04] border border-white/10 text-zinc-300 hover:bg-white/[0.08] hover:text-white">
                {state.opts.cancelLabel || t("Cancel")}
              </button>
              <button onClick={() => close(true)} ref={confirmOkRef}
                      data-default-focus={state.opts.safeDefault ? undefined : true}
                      className={`px-3 h-10 sm:h-8 rounded text-[12px] font-semibold border ${
                        state.opts.destructive
                          ? "bg-rose-500/15 border-rose-500/40 text-rose-300 hover:bg-rose-500/25"
                          : "bg-primary/15 border-primary/40 text-primary hover:bg-primary/25"
                      }`}>
                {state.opts.okLabel || (state.opts.destructive ? t("Delete") : t("OK"))}
              </button>
            </div>
          </div>
        </div>,
        document.body
      )}
      {tpState && createPortal(
        <div key={tpState.id}
             className="fixed inset-0 z-[9999] bg-black/60 backdrop-blur-sm flex items-center justify-center p-4"
             onClickCapture={ignoreLaterClicks}
             onClick={() => closeTextPrompt(null)}>
          <div ref={tpPanelRef}
               role="dialog"
               aria-modal="true"
               aria-labelledby="text-prompt-modal-title"
               onClick={(e) => e.stopPropagation()}
               className="w-full max-w-[400px] bg-[#121214] border border-white/10 rounded-xl shadow-2xl p-4 font-mono text-[12px] animate-in zoom-in-95 fade-in duration-150">
            <div className="flex items-start justify-between gap-3 mb-3">
              <span id="text-prompt-modal-title" className="text-[12px] font-semibold text-zinc-200">
                {tpState.opts.title || t("Enter value")}
              </span>
              <button aria-label={t("Close")}onClick={() => closeTextPrompt(null)} className="p-[14px] -m-[14px] sm:p-0 sm:m-0 text-zinc-500 hover:text-white shrink-0">
                <X size={12} />
              </button>
            </div>
            {tpState.opts.message && (
              <p className="text-[12px] text-zinc-300 leading-relaxed mb-3 whitespace-pre-wrap break-words">
                {tpState.opts.message}
              </p>
            )}
            <input
              data-default-focus
              type={tpState.opts.password ? "password" : "text"}
              value={tpValue}
              onChange={(e) => { setTpValue(e.target.value); setTpError(null); }}
              onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); if (!e.repeat) submitTextPrompt(); } }}
              placeholder={tpState.opts.placeholder}
              className="w-full h-10 px-3 mb-2 bg-black/40 border border-white/10 rounded-lg text-[13px] text-zinc-100 outline-none focus:border-primary/50 placeholder:text-zinc-600"
            />
            {tpError && (
              <p className="text-[11px] text-rose-300 mb-2">{tpError}</p>
            )}
            <div className="flex justify-end gap-2 mt-2">
              <button onClick={() => closeTextPrompt(null)}
                      className="px-3 h-10 sm:h-8 rounded text-[12px] font-semibold bg-white/[0.04] border border-white/10 text-zinc-300 hover:bg-white/[0.08] hover:text-white">
                {tpState.opts.cancelLabel || t("Cancel")}
              </button>
              <button onClick={submitTextPrompt}
                      className="px-3 h-10 sm:h-8 rounded text-[12px] font-semibold bg-primary/15 border border-primary/40 text-primary hover:bg-primary/25">
                {tpState.opts.okLabel || t("OK")}
              </button>
            </div>
          </div>
        </div>,
        document.body
      )}
      {/* Discard sits apart from Save, so a click that misses Save can't
          throw the changes away. Clicks go through clickUnsaved: the one that
          opened the question may have a second click right behind it. */}
      {usState && createPortal(
        <div key={usState.id}
             className="fixed inset-0 z-[9999] bg-black/60 backdrop-blur-sm flex items-center justify-center p-4"
             onClick={(e) => clickUnsaved(e, "cancel")}>
          <div ref={usPanelRef}
               role="dialog"
               aria-modal="true"
               aria-labelledby="unsaved-modal-title"
               aria-describedby="unsaved-modal-message"
               onClick={(e) => e.stopPropagation()}
               className="w-full max-w-[400px] bg-[#121214] border border-white/10 rounded-xl shadow-2xl p-4 font-mono text-[12px] animate-in zoom-in-95 fade-in duration-150">
            <div className="flex items-start justify-between gap-3 mb-3">
              <div className="flex items-center gap-2">
                <AlertTriangle size={14} className="text-amber-400" />
                <span id="unsaved-modal-title" className="text-[12px] font-semibold text-zinc-200">
                  {usState.opts.title || t("Unsaved changes")}
                </span>
              </div>
              <button aria-label={t("Close")}data-choice="cancel" onClick={(e) => clickUnsaved(e, "cancel")} className="p-[14px] -m-[14px] sm:p-0 sm:m-0 text-zinc-500 hover:text-white shrink-0">
                <X size={12} />
              </button>
            </div>
            <p id="unsaved-modal-message" className="text-[12px] text-zinc-300 leading-relaxed mb-4 whitespace-pre-wrap break-words">
              {usState.opts.message}
            </p>
            <div className="flex flex-wrap items-center justify-between gap-2">
              <button data-choice="discard" onClick={(e) => clickUnsaved(e, "discard")}
                      className="px-3 h-10 sm:h-8 rounded text-[12px] font-semibold bg-rose-500/10 border border-rose-500/30 text-rose-300 hover:bg-rose-500/20">
                {usState.opts.discardLabel || t("Discard")}
              </button>
              <div className="ml-auto flex gap-2">
                <button data-choice="cancel" onClick={(e) => clickUnsaved(e, "cancel")}
                        className="px-3 h-10 sm:h-8 rounded text-[12px] font-semibold bg-white/[0.04] border border-white/10 text-zinc-300 hover:bg-white/[0.08] hover:text-white">
                  {usState.opts.cancelLabel || t("Keep editing")}
                </button>
                <button data-choice="save" data-default-focus onClick={(e) => clickUnsaved(e, "save")}
                        className="px-3 h-10 sm:h-8 rounded text-[12px] font-semibold bg-primary/15 border border-primary/40 text-primary hover:bg-primary/25">
                  {usState.opts.saveLabel || t("Save")}
                </button>
              </div>
            </div>
          </div>
        </div>,
        document.body
      )}
      </UnsavedContext.Provider>
      </TextPromptContext.Provider>
     </OverwriteContext.Provider>
    </ConfirmContext.Provider>
  );
};

export const useUnsavedPrompt = () => {
  const ctx = useContext(UnsavedContext);
  if (!ctx) throw new Error("useUnsavedPrompt must be used within <ConfirmProvider>");
  return ctx;
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
